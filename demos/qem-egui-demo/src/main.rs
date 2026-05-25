use eframe::egui::{
    self, Color32, Key, ProgressBar, RichText, ScrollArea, Sense, TextEdit, TextStyle,
};
use qem::{DocumentSession, EditCapability, TextPosition, ViewportRequest};
use std::path::PathBuf;
use std::time::Duration;

fn main() -> Result<(), eframe::Error> {
    let initial_path = std::env::args_os().nth(1).map(PathBuf::from);
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1180.0, 760.0])
            .with_min_inner_size([900.0, 560.0]),
        ..Default::default()
    };

    eframe::run_native(
        "Qem minimal egui demo",
        native_options,
        Box::new(move |_cc| Ok(Box::new(QemEguiDemo::new(initial_path.clone())))),
    )
}

struct QemEguiDemo {
    session: DocumentSession,
    open_path: String,
    save_path: String,
    caret: TextPosition,
    desired_col: usize,
    viewport_cols: usize,
    editor_has_focus: bool,
    notice: String,
    pending_open: Option<PathBuf>,
}

impl QemEguiDemo {
    fn new(initial_path: Option<PathBuf>) -> Self {
        let path_text = initial_path
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_default();

        Self {
            session: DocumentSession::new(),
            open_path: path_text.clone(),
            save_path: path_text,
            caret: TextPosition::new(0, 0),
            desired_col: 0,
            viewport_cols: 240,
            editor_has_focus: false,
            notice: String::from("Click a viewport row to focus the editor."),
            pending_open: initial_path,
        }
    }

    fn pump_session(&mut self) {
        if let Some(path) = self.pending_open.take() {
            self.open_document(path);
        }

        if let Some(result) = self.session.poll_background_job() {
            match result {
                Ok(()) => {
                    self.sync_paths_from_session();
                    self.clamp_caret();
                    self.notice = String::from("Background operation completed.");
                }
                Err(err) => {
                    self.notice = format!("Background operation failed: {err}");
                }
            }
        }
    }

    fn sync_paths_from_session(&mut self) {
        if let Some(path) = self.session.current_path() {
            let path_text = path.display().to_string();
            self.open_path = path_text.clone();
            self.save_path = path_text;
        }
    }

    fn clamp_caret(&mut self) {
        self.caret = self.session.clamp_position(self.caret);
        self.desired_col = self.caret.col0();
    }

    fn open_document(&mut self, path: PathBuf) {
        match self.session.open_file_async(path.clone()) {
            Ok(()) => {
                self.notice = format!("Opening {}", path.display());
                self.caret = TextPosition::new(0, 0);
                self.desired_col = 0;
                self.editor_has_focus = true;
            }
            Err(err) => {
                self.notice = format!("Open failed: {err}");
            }
        }
    }

    fn open_from_field(&mut self) {
        let path = self.open_path.trim();
        if path.is_empty() {
            self.notice = String::from("Open path is empty.");
            return;
        }
        self.open_document(PathBuf::from(path));
    }

    fn close_document(&mut self) {
        self.session.close_file();
        self.caret = TextPosition::new(0, 0);
        self.desired_col = 0;
        self.editor_has_focus = false;
        self.notice = String::from("Document closed.");
    }

    fn save_current(&mut self) {
        match self.session.save_async() {
            Ok(true) => {
                self.notice = String::from("Save started.");
            }
            Ok(false) => {
                self.notice = String::from("Save skipped: current document is already clean.");
            }
            Err(err) => {
                self.notice = format!("Save failed: {err}");
            }
        }
    }

    fn save_as_from_field(&mut self) {
        let path = self.save_path.trim();
        if path.is_empty() {
            self.notice = String::from("Save path is empty.");
            return;
        }

        match self.session.save_as_async(PathBuf::from(path)) {
            Ok(true) => {
                self.notice = format!("Saving to {path}");
            }
            Ok(false) => {
                self.notice = String::from("Save-as skipped: current document is already clean.");
            }
            Err(err) => {
                self.notice = format!("Save-as failed: {err}");
            }
        }
    }

    fn handle_editor_input(&mut self, ctx: &egui::Context) {
        if !self.editor_has_focus || self.session.is_busy() {
            return;
        }

        let events = ctx.input(|input| input.events.clone());
        for event in events {
            match event {
                egui::Event::Text(text) => {
                    if text.chars().any(|ch| ch.is_control()) {
                        continue;
                    }
                    self.insert_text(&text);
                }
                egui::Event::Key {
                    key,
                    pressed: true,
                    modifiers,
                    ..
                } => {
                    if modifiers.ctrl || modifiers.command || modifiers.alt {
                        if (modifiers.ctrl || modifiers.command) && key == Key::S {
                            self.save_current();
                        }
                        continue;
                    }

                    match key {
                        Key::ArrowLeft => self.move_left(),
                        Key::ArrowRight => self.move_right(),
                        Key::ArrowUp => self.move_vertical(-1),
                        Key::ArrowDown => self.move_vertical(1),
                        Key::PageUp => self.move_vertical(-20),
                        Key::PageDown => self.move_vertical(20),
                        Key::Home => self.move_home(),
                        Key::End => self.move_end(),
                        Key::Enter => {
                            let newline = self.session.line_ending().as_str().to_owned();
                            self.insert_text(&newline);
                        }
                        Key::Backspace => match self.session.try_backspace(self.caret) {
                            Ok(result) => self.set_caret(result.cursor()),
                            Err(err) => self.notice = format!("Backspace failed: {err}"),
                        },
                        Key::Delete => match self.session.try_delete_forward(self.caret) {
                            Ok(result) => self.set_caret(result.cursor()),
                            Err(err) => self.notice = format!("Delete failed: {err}"),
                        },
                        _ => {}
                    }
                }
                _ => {}
            }
        }
    }

    fn insert_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }

        match self.session.try_insert(self.caret, text) {
            Ok(cursor) => self.set_caret(cursor),
            Err(err) => self.notice = format!("Insert failed: {err}"),
        }
    }

    fn set_caret(&mut self, caret: TextPosition) {
        self.caret = self.session.clamp_position(caret);
        self.desired_col = self.caret.col0();
    }

    fn move_home(&mut self) {
        self.set_caret(TextPosition::new(self.caret.line0(), 0));
    }

    fn move_end(&mut self) {
        let line_len = self.session.line_len_chars(self.caret.line0());
        self.set_caret(TextPosition::new(self.caret.line0(), line_len));
    }

    fn move_left(&mut self) {
        if self.caret.col0() > 0 {
            self.set_caret(TextPosition::new(self.caret.line0(), self.caret.col0() - 1));
            return;
        }

        if self.caret.line0() == 0 {
            return;
        }

        let previous_line = self.caret.line0() - 1;
        let previous_col = self.session.line_len_chars(previous_line);
        self.set_caret(TextPosition::new(previous_line, previous_col));
    }

    fn move_right(&mut self) {
        let line_len = self.session.line_len_chars(self.caret.line0());
        if self.caret.col0() < line_len {
            self.set_caret(TextPosition::new(self.caret.line0(), self.caret.col0() + 1));
            return;
        }

        let next_line = self.caret.line0() + 1;
        if next_line >= self.session.display_line_count() {
            return;
        }

        self.set_caret(TextPosition::new(next_line, 0));
    }

    fn move_vertical(&mut self, delta_lines: isize) {
        let total_lines = self.session.display_line_count();
        let current = self.caret.line0();
        let target = if delta_lines.is_negative() {
            current.saturating_sub(delta_lines.unsigned_abs())
        } else {
            current
                .saturating_add(delta_lines as usize)
                .min(total_lines.saturating_sub(1))
        };
        let target_col = self.desired_col.min(self.session.line_len_chars(target));
        self.caret = self
            .session
            .clamp_position(TextPosition::new(target, target_col));
    }

    fn row_text(&self, line0: usize, text: &str) -> String {
        let mut rendered = if self.editor_has_focus && self.caret.line0() == line0 {
            insert_caret_marker(text, self.caret.col0())
        } else if text.is_empty() {
            String::from(" ")
        } else {
            text.to_owned()
        };

        if rendered.is_empty() {
            rendered.push(' ');
        }

        rendered
    }

    fn render_toolbar(&mut self, ctx: &egui::Context) {
        let busy = self.session.is_busy();
        let has_path = self.session.current_path().is_some();

        egui::TopBottomPanel::top("toolbar").show(ctx, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label("Open");
                let open_response =
                    ui.add(TextEdit::singleline(&mut self.open_path).desired_width(320.0));
                if open_response.has_focus() {
                    self.editor_has_focus = false;
                }
                if ui
                    .add_enabled(!busy, egui::Button::new("Open async"))
                    .clicked()
                {
                    self.open_from_field();
                }

                if ui
                    .add_enabled(has_path, egui::Button::new("Close"))
                    .clicked()
                {
                    self.close_document();
                }

                ui.separator();
                ui.label("Save as");
                let save_response =
                    ui.add(TextEdit::singleline(&mut self.save_path).desired_width(320.0));
                if save_response.has_focus() {
                    self.editor_has_focus = false;
                }
                if ui
                    .add_enabled(has_path && !busy, egui::Button::new("Save"))
                    .clicked()
                {
                    self.save_current();
                }
                if ui
                    .add_enabled(!busy, egui::Button::new("Save as"))
                    .clicked()
                {
                    self.save_as_from_field();
                }
            });
        });
    }

    fn render_sidebar(&mut self, ctx: &egui::Context) {
        let status = self.session.status();
        let capability = self.session.edit_capability_at(self.caret);

        egui::SidePanel::left("status")
            .resizable(true)
            .default_width(280.0)
            .show(ctx, |ui| {
                ui.heading("Qem");
                ui.label("Minimal viewer/editor on top of DocumentSession.");
                ui.separator();

                ui.monospace(format!(
                    "path: {}",
                    status
                        .path()
                        .map(|path| path.display().to_string())
                        .unwrap_or_else(|| String::from("<none>"))
                ));
                ui.monospace(format!("generation: {}", status.generation()));
                ui.monospace(format!("dirty: {}", status.is_dirty()));
                ui.monospace(format!("bytes: {}", status.file_len()));
                ui.monospace(format!("backing: {}", status.backing().as_str()));
                ui.monospace(format!(
                    "lines: {} ({})",
                    status.display_line_count(),
                    if status.is_line_count_exact() {
                        "exact"
                    } else {
                        "estimated"
                    }
                ));
                ui.monospace(format!("encoding: {}", status.encoding().name()));
                ui.monospace(format!("line ending: {:?}", status.line_ending()));

                ui.separator();
                ui.label("Caret");
                ui.monospace(format!(
                    "line {}, col {}",
                    self.caret.line0() + 1,
                    self.caret.col0() + 1
                ));
                ui.monospace(format!("edit: {}", describe_capability(capability)));

                ui.horizontal(|ui| {
                    if ui.button("Home").clicked() {
                        self.editor_has_focus = true;
                        self.move_home();
                    }
                    if ui.button("End").clicked() {
                        self.editor_has_focus = true;
                        self.move_end();
                    }
                });

                ui.horizontal(|ui| {
                    if ui.button("Up").clicked() {
                        self.editor_has_focus = true;
                        self.move_vertical(-1);
                    }
                    if ui.button("Down").clicked() {
                        self.editor_has_focus = true;
                        self.move_vertical(1);
                    }
                });

                ui.separator();
                ui.add(
                    egui::Slider::new(&mut self.viewport_cols, 40..=512)
                        .text("viewport columns")
                        .clamping(egui::SliderClamping::Always),
                );

                if let Some(progress) = status.loading_state() {
                    ui.separator();
                    ui.label("Loading");
                    ui.add(
                        ProgressBar::new(progress.fraction())
                            .show_percentage()
                            .text(format!(
                                "{:?} {}/{} bytes",
                                progress.load_phase().unwrap_or(qem::LoadPhase::Opening),
                                progress.completed_bytes(),
                                progress.total_bytes()
                            )),
                    );
                }

                if let Some(progress) = status.save_state() {
                    ui.separator();
                    ui.label("Saving");
                    ui.add(
                        ProgressBar::new(progress.fraction())
                            .show_percentage()
                            .text(format!(
                                "{}/{} bytes",
                                progress.completed_bytes(),
                                progress.total_bytes()
                            )),
                    );
                }

                if let Some(progress) = status.indexing_state() {
                    let fraction = progress.fraction();
                    ui.separator();
                    ui.label("Indexing");
                    ui.add(ProgressBar::new(fraction).show_percentage().text(format!(
                        "{}/{} bytes",
                        progress.completed_bytes(),
                        progress.total_bytes()
                    )));
                }

                if let Some(issue) = status.background_issue() {
                    ui.separator();
                    ui.colored_label(
                        Color32::from_rgb(220, 130, 70),
                        format!("{:?}: {}", issue.kind(), issue.message()),
                    );
                }

                ui.separator();
                ui.label("Notice");
                ui.label(&self.notice);
            });
    }

    fn render_editor(&mut self, ctx: &egui::Context) {
        let status = self.session.status();
        let total_lines = status.display_line_count();

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("Viewport");
            ui.label(
                "Click a row to focus the editor. Type text directly. Use arrows, Home/End, PageUp/PageDown, Enter, Backspace, Delete, and Ctrl+S.",
            );
            ui.add_space(8.0);

            egui::Frame::group(ui.style()).show(ui, |ui| {
                let row_height = ui.text_style_height(&TextStyle::Monospace) + 6.0;
                ScrollArea::vertical()
                    .id_salt("qem-egui-demo-scroll")
                    .auto_shrink([false, false])
                    .show_rows(ui, row_height, total_lines, |ui, row_range| {
                        let viewport = self.session.read_viewport(
                            ViewportRequest::new(row_range.start, row_range.len())
                                .with_columns(0, self.viewport_cols),
                        );

                        for row in viewport.rows() {
                            let is_caret_row = row.line0() == self.caret.line0();
                            ui.horizontal(|ui| {
                                let gutter_color = if row.is_exact() {
                                    Color32::GRAY
                                } else {
                                    Color32::from_rgb(210, 170, 60)
                                };
                                ui.add_sized(
                                    [64.0, row_height],
                                    egui::Label::new(
                                        RichText::new(format!("{:>6}", row.line_number()))
                                            .monospace()
                                            .color(gutter_color),
                                    ),
                                );

                                let marker = if is_caret_row && self.editor_has_focus {
                                    ">"
                                } else {
                                    " "
                                };
                                let row_text = format!("{marker} {}", self.row_text(row.line0(), row.text()));
                                let color = if is_caret_row {
                                    Color32::from_rgb(220, 235, 255)
                                } else {
                                    ui.visuals().text_color()
                                };
                                let response = ui.add_sized(
                                    [ui.available_width(), row_height],
                                    egui::Label::new(RichText::new(row_text).monospace().color(color))
                                        .sense(Sense::click()),
                                );

                                if response.clicked() {
                                    self.editor_has_focus = true;
                                    let target_col = self.desired_col.min(self.session.line_len_chars(row.line0()));
                                    self.set_caret(TextPosition::new(row.line0(), target_col));
                                }
                            });
                        }
                    });
            });
        });
    }
}

impl eframe::App for QemEguiDemo {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.pump_session();
        self.handle_editor_input(ctx);
        self.render_toolbar(ctx);
        self.render_sidebar(ctx);
        self.render_editor(ctx);

        if self.session.is_busy() || self.session.is_indexing() {
            ctx.request_repaint_after(Duration::from_millis(16));
        }
    }
}

fn insert_caret_marker(text: &str, col0: usize) -> String {
    if text.is_empty() {
        return String::from("|");
    }

    let total_chars = text.chars().count();
    if col0 >= total_chars {
        let mut rendered = text.to_owned();
        rendered.push('|');
        return rendered;
    }

    let mut rendered = String::with_capacity(text.len() + 1);
    for (index, ch) in text.chars().enumerate() {
        if index == col0 {
            rendered.push('|');
        }
        rendered.push(ch);
    }
    rendered
}

fn describe_capability(capability: EditCapability) -> String {
    match capability {
        EditCapability::Editable { backing } => format!("editable on {}", backing.as_str()),
        EditCapability::RequiresPromotion { from, to } => {
            format!("promotes {} -> {}", from.as_str(), to.as_str())
        }
        EditCapability::Unsupported { backing, reason } => {
            format!("unsupported on {}: {reason}", backing.as_str())
        }
    }
}
