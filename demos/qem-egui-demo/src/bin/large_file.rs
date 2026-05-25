use eframe::egui::{
    self, Color32, DragValue, Key, ProgressBar, RichText, ScrollArea, Sense, TextEdit, TextStyle,
};
use qem::{DocumentSession, EditCapability, TextPosition, ViewportRequest};
use std::path::PathBuf;
use std::time::Duration;

fn main() -> Result<(), eframe::Error> {
    let initial_path = std::env::args_os().nth(1).map(PathBuf::from);
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1320.0, 820.0])
            .with_min_inner_size([980.0, 640.0]),
        ..Default::default()
    };

    eframe::run_native(
        "Qem large-file egui demo",
        native_options,
        Box::new(move |_cc| Ok(Box::new(LargeFileDemo::new(initial_path.clone())))),
    )
}

struct LargeFileDemo {
    session: DocumentSession,
    open_path: String,
    save_path: String,
    goto_line: String,
    caret: TextPosition,
    desired_col: usize,
    first_line0: usize,
    viewport_rows: usize,
    start_col: usize,
    viewport_cols: usize,
    editor_has_focus: bool,
    notice: String,
    pending_open: Option<PathBuf>,
}

impl LargeFileDemo {
    fn new(initial_path: Option<PathBuf>) -> Self {
        let path_text = initial_path
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_default();

        Self {
            session: DocumentSession::new(),
            open_path: path_text.clone(),
            save_path: path_text,
            goto_line: String::from("1"),
            caret: TextPosition::new(0, 0),
            desired_col: 0,
            first_line0: 0,
            viewport_rows: 40,
            start_col: 0,
            viewport_cols: 180,
            editor_has_focus: false,
            notice: String::from("Large-file viewport demo ready."),
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
                    self.clamp_state();
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

    fn visible_line_count(&self) -> usize {
        self.session.status().display_line_count()
    }

    fn clamp_state(&mut self) {
        self.viewport_rows = self.viewport_rows.clamp(8, 160);
        self.viewport_cols = self.viewport_cols.clamp(40, 512);
        self.caret = self.session.clamp_position(self.caret);
        self.desired_col = self.caret.col0();

        let total_lines = self.visible_line_count();
        let max_first_line0 = total_lines.saturating_sub(1);
        self.first_line0 = self.first_line0.min(max_first_line0);

        self.ensure_caret_visible();
        self.goto_line = (self.caret.line0() + 1).to_string();
    }

    fn ensure_caret_visible(&mut self) {
        if self.caret.line0() < self.first_line0 {
            self.first_line0 = self.caret.line0();
            return;
        }

        let bottom = self
            .first_line0
            .saturating_add(self.viewport_rows.saturating_sub(1));
        if self.caret.line0() > bottom {
            self.first_line0 = self
                .caret
                .line0()
                .saturating_add(1)
                .saturating_sub(self.viewport_rows);
        }
    }

    fn open_document(&mut self, path: PathBuf) {
        match self.session.open_file_async(path.clone()) {
            Ok(()) => {
                self.notice = format!("Opening {}", path.display());
                self.caret = TextPosition::new(0, 0);
                self.desired_col = 0;
                self.first_line0 = 0;
                self.goto_line = String::from("1");
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
        self.first_line0 = 0;
        self.goto_line = String::from("1");
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

    fn set_caret(&mut self, caret: TextPosition) {
        self.caret = self.session.clamp_position(caret);
        self.desired_col = self.caret.col0();
        self.ensure_caret_visible();
        self.goto_line = (self.caret.line0() + 1).to_string();
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

        let previous_line0 = self.caret.line0() - 1;
        let previous_col = self.session.line_len_chars(previous_line0);
        self.set_caret(TextPosition::new(previous_line0, previous_col));
    }

    fn move_right(&mut self) {
        let line_len = self.session.line_len_chars(self.caret.line0());
        if self.caret.col0() < line_len {
            self.set_caret(TextPosition::new(self.caret.line0(), self.caret.col0() + 1));
            return;
        }

        let next_line0 = self.caret.line0() + 1;
        if next_line0 >= self.visible_line_count() {
            return;
        }

        self.set_caret(TextPosition::new(next_line0, 0));
    }

    fn move_vertical(&mut self, delta_lines: isize) {
        let total_lines = self.visible_line_count();
        let current = self.caret.line0();
        let target_line0 = if delta_lines.is_negative() {
            current.saturating_sub(delta_lines.unsigned_abs())
        } else {
            current
                .saturating_add(delta_lines as usize)
                .min(total_lines.saturating_sub(1))
        };
        let target_col = self
            .desired_col
            .min(self.session.line_len_chars(target_line0));
        self.set_caret(TextPosition::new(target_line0, target_col));
    }

    fn jump_to_top(&mut self) {
        self.first_line0 = 0;
        self.set_caret(TextPosition::new(
            0,
            self.desired_col.min(self.session.line_len_chars(0)),
        ));
    }

    fn jump_to_tail(&mut self) {
        let total_lines = self.visible_line_count();
        let last_line0 = total_lines.saturating_sub(1);
        self.first_line0 = total_lines.saturating_sub(self.viewport_rows);
        let last_col = self
            .desired_col
            .min(self.session.line_len_chars(last_line0));
        self.set_caret(TextPosition::new(last_line0, last_col));
    }

    fn jump_to_line_from_field(&mut self) {
        let raw = self.goto_line.trim();
        let Ok(line_number) = raw.parse::<usize>() else {
            self.notice = format!("Invalid line number: {raw}");
            return;
        };

        let total_lines = self.visible_line_count();
        let line0 = line_number
            .saturating_sub(1)
            .min(total_lines.saturating_sub(1));
        let col0 = self.desired_col.min(self.session.line_len_chars(line0));
        self.first_line0 = line0.saturating_sub(self.viewport_rows / 2);
        self.set_caret(TextPosition::new(line0, col0));
    }

    fn page_viewport(&mut self, direction: isize) {
        let page = self.viewport_rows.saturating_sub(1).max(1);
        let total_lines = self.visible_line_count();
        let next_first_line0 = if direction.is_negative() {
            self.first_line0
                .saturating_sub(page.saturating_mul(direction.unsigned_abs()))
        } else {
            self.first_line0
                .saturating_add(page.saturating_mul(direction as usize))
                .min(total_lines.saturating_sub(1))
        };
        self.first_line0 = next_first_line0;
        let target_col = self
            .desired_col
            .min(self.session.line_len_chars(self.first_line0));
        self.set_caret(TextPosition::new(self.first_line0, target_col));
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
                        if (modifiers.ctrl || modifiers.command) && key == Key::Home {
                            self.jump_to_top();
                        }
                        if (modifiers.ctrl || modifiers.command) && key == Key::End {
                            self.jump_to_tail();
                        }
                        continue;
                    }

                    match key {
                        Key::ArrowLeft => self.move_left(),
                        Key::ArrowRight => self.move_right(),
                        Key::ArrowUp => self.move_vertical(-1),
                        Key::ArrowDown => self.move_vertical(1),
                        Key::PageUp => self.page_viewport(-1),
                        Key::PageDown => self.page_viewport(1),
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

    fn render_toolbar(&mut self, ctx: &egui::Context) {
        let busy = self.session.is_busy();
        let has_path = self.session.current_path().is_some();

        egui::TopBottomPanel::top("toolbar").show(ctx, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label("Open");
                let open_response =
                    ui.add(TextEdit::singleline(&mut self.open_path).desired_width(360.0));
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
                    ui.add(TextEdit::singleline(&mut self.save_path).desired_width(360.0));
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
        let total_lines = status.display_line_count();
        let last_line0 = self
            .first_line0
            .saturating_add(self.viewport_rows.saturating_sub(1))
            .min(total_lines.saturating_sub(1));

        egui::SidePanel::left("status")
            .resizable(true)
            .default_width(320.0)
            .show(ctx, |ui| {
                ui.heading("Qem");
                ui.label("Large-file viewport editor on top of DocumentSession.");
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
                ui.monospace(format!("backing: {}", status.backing().as_str()));
                ui.monospace(format!("bytes: {}", status.file_len()));
                ui.monospace(format!(
                    "lines: {} ({})",
                    status.display_line_count(),
                    if status.is_line_count_exact() {
                        "exact"
                    } else {
                        "estimated"
                    }
                ));
                ui.monospace(format!(
                    "line count pending: {}",
                    status.is_line_count_pending()
                ));
                ui.monospace(format!("line ending: {:?}", status.line_ending()));
                ui.monospace(format!("encoding: {}", status.encoding().name()));

                ui.separator();
                ui.label("Viewport");
                ui.monospace(format!(
                    "window: {}..{}",
                    self.first_line0 + 1,
                    last_line0 + 1
                ));
                ui.horizontal(|ui| {
                    if ui.button("Top").clicked() {
                        self.editor_has_focus = true;
                        self.jump_to_top();
                    }
                    if ui.button("-Page").clicked() {
                        self.editor_has_focus = true;
                        self.page_viewport(-1);
                    }
                    if ui.button("+Page").clicked() {
                        self.editor_has_focus = true;
                        self.page_viewport(1);
                    }
                    if ui.button("Tail").clicked() {
                        self.editor_has_focus = true;
                        self.jump_to_tail();
                    }
                });

                ui.horizontal(|ui| {
                    ui.label("Go to line");
                    let goto_response =
                        ui.add(TextEdit::singleline(&mut self.goto_line).desired_width(90.0));
                    if goto_response.has_focus() {
                        self.editor_has_focus = false;
                    }
                    if ui.button("Jump").clicked() {
                        self.editor_has_focus = true;
                        self.jump_to_line_from_field();
                    }
                });

                ui.horizontal(|ui| {
                    ui.label("Rows");
                    let rows_changed = ui
                        .add(
                            DragValue::new(&mut self.viewport_rows)
                                .speed(1)
                                .range(8..=160),
                        )
                        .changed();
                    ui.label("Cols");
                    let cols_changed = ui
                        .add(
                            DragValue::new(&mut self.viewport_cols)
                                .speed(4)
                                .range(40..=512),
                        )
                        .changed();
                    if rows_changed || cols_changed {
                        self.clamp_state();
                    }
                });

                ui.horizontal(|ui| {
                    ui.label("First line");
                    let mut first_line_display = self.first_line0.saturating_add(1);
                    if ui
                        .add(
                            DragValue::new(&mut first_line_display)
                                .speed(8)
                                .range(1..=usize::MAX),
                        )
                        .changed()
                    {
                        self.first_line0 = first_line_display.saturating_sub(1);
                        self.clamp_state();
                    }
                });

                ui.horizontal(|ui| {
                    ui.label("Start col");
                    if ui
                        .add(
                            DragValue::new(&mut self.start_col)
                                .speed(4)
                                .range(0..=usize::MAX),
                        )
                        .changed()
                    {
                        self.clamp_state();
                    }
                });

                ui.separator();
                ui.label("Caret");
                ui.monospace(format!(
                    "line {}, col {}",
                    self.caret.line0() + 1,
                    self.caret.col0() + 1
                ));
                ui.monospace(format!("edit: {}", describe_capability(capability)));

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
        let viewport = self.session.read_viewport(
            ViewportRequest::new(self.first_line0, self.viewport_rows)
                .with_columns(self.start_col, self.viewport_cols),
        );
        let last_visible_line0 = viewport
            .rows()
            .last()
            .map(|row| row.line0())
            .unwrap_or(self.first_line0);

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("Large-file viewport");
            ui.label(
                "This demo keeps viewport state in the application. Use Top/Tail/Page/Jump controls, click a row to move the caret, and type directly into the viewport.",
            );
            ui.monospace(format!(
                "visible lines {}..{}, columns {}..{}",
                self.first_line0 + 1,
                last_visible_line0 + 1,
                self.start_col + 1,
                self.start_col.saturating_add(self.viewport_cols)
            ));
            ui.add_space(8.0);

            egui::Frame::group(ui.style()).show(ui, |ui| {
                let row_height = ui.text_style_height(&TextStyle::Monospace) + 6.0;
                ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    for row in viewport.rows() {
                        let is_caret_row = row.line0() == self.caret.line0();
                        ui.horizontal(|ui| {
                            let exact_marker = if row.is_exact() { "=" } else { "~" };
                            let exact_color = if row.is_exact() {
                                Color32::GRAY
                            } else {
                                Color32::from_rgb(210, 170, 60)
                            };
                            ui.add_sized(
                                [20.0, row_height],
                                egui::Label::new(
                                    RichText::new(exact_marker).monospace().color(exact_color),
                                ),
                            );
                            ui.add_sized(
                                [72.0, row_height],
                                egui::Label::new(
                                    RichText::new(format!("{:>7}", row.line_number()))
                                        .monospace()
                                        .color(exact_color),
                                ),
                            );

                            let row_text = decorate_visible_row(
                                row.text(),
                                is_caret_row && self.editor_has_focus,
                                self.caret.col0(),
                                self.start_col,
                            );
                            let text_color = if is_caret_row {
                                Color32::from_rgb(220, 235, 255)
                            } else {
                                ui.visuals().text_color()
                            };
                            let response = ui.add_sized(
                                [ui.available_width(), row_height],
                                egui::Label::new(
                                    RichText::new(row_text).monospace().color(text_color),
                                )
                                .sense(Sense::click()),
                            );
                            if response.clicked() {
                                self.editor_has_focus = true;
                                let target_col =
                                    self.desired_col.min(self.session.line_len_chars(row.line0()));
                                self.set_caret(TextPosition::new(row.line0(), target_col));
                            }
                        });
                    }

                    if viewport.rows().is_empty() {
                        ui.monospace("<empty viewport>");
                    }
                });
            });

            ui.add_space(8.0);
            ui.horizontal_wrapped(|ui| {
                ui.label(format!("scroll rows: {}", status.display_line_count()));
                ui.separator();
                ui.label(format!("backing: {}", status.backing().as_str()));
                ui.separator();
                ui.label(format!(
                    "line count is {}",
                    if status.is_line_count_exact() {
                        "exact"
                    } else {
                        "estimated"
                    }
                ));
                ui.separator();
                ui.label(format!("dirty: {}", status.is_dirty()));
            });
        });
    }
}

impl eframe::App for LargeFileDemo {
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

fn insert_caret_marker(text: &str, local_col0: usize) -> String {
    if text.is_empty() {
        return String::from("|");
    }

    let total_chars = text.chars().count();
    if local_col0 >= total_chars {
        let mut rendered = text.to_owned();
        rendered.push('|');
        return rendered;
    }

    let mut rendered = String::with_capacity(text.len() + 1);
    for (index, ch) in text.chars().enumerate() {
        if index == local_col0 {
            rendered.push('|');
        }
        rendered.push(ch);
    }
    rendered
}

fn decorate_visible_row(
    text: &str,
    show_caret: bool,
    caret_col0: usize,
    start_col: usize,
) -> String {
    let base = if text.is_empty() {
        String::from(" ")
    } else {
        text.to_owned()
    };

    if !show_caret {
        return base;
    }

    if caret_col0 < start_col {
        return format!("|< {base}");
    }

    let local_col0 = caret_col0.saturating_sub(start_col);
    insert_caret_marker(&base, local_col0)
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
