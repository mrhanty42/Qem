use eframe::egui::{
    self, Align, Color32, CornerRadius, DragValue, FontFamily, FontId, Key, Layout, Margin,
    ProgressBar, Rect, RichText, ScrollArea, Sense, Stroke, TextEdit, Vec2,
};
use qem::{DocumentSession, EditCapability, TextPosition, ViewportRequest};
use std::path::PathBuf;
use std::time::Duration;

const BG: Color32 = Color32::from_rgb(15, 17, 21);
const PANEL: Color32 = Color32::from_rgb(20, 23, 28);
const EDITOR_BG: Color32 = Color32::from_rgb(18, 20, 25);
const ACTIVE_LINE: Color32 = Color32::from_rgb(28, 33, 42);
const CARD: Color32 = Color32::from_rgb(24, 28, 35);
const BORDER: Color32 = Color32::from_rgb(42, 47, 57);
const TEXT: Color32 = Color32::from_rgb(215, 220, 230);
const MUTED: Color32 = Color32::from_rgb(126, 135, 151);
const ACCENT: Color32 = Color32::from_rgb(104, 160, 255);
const WARNING: Color32 = Color32::from_rgb(232, 179, 92);
const SAVED: Color32 = Color32::from_rgb(94, 203, 172);
const GUTTER_WIDTH: f32 = 76.0;
const ROW_HEIGHT: f32 = 23.0;

fn main() -> Result<(), eframe::Error> {
    let initial_path = std::env::args_os().nth(1).map(PathBuf::from);
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1320.0, 820.0])
            .with_min_inner_size([980.0, 640.0]),
        ..Default::default()
    };

    eframe::run_native(
        "Qem Large File",
        native_options,
        Box::new(move |cc| {
            install_theme(&cc.egui_ctx);
            Ok(Box::new(LargeFileDemo::new(initial_path.clone())))
        }),
    )
}

struct LargeFileDemo {
    session: DocumentSession,
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
        Self {
            session: DocumentSession::new(),
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
                    self.clamp_state();
                    self.notice = String::from("Background operation completed.");
                }
                Err(err) => {
                    self.notice = format!("Background operation failed: {err}");
                }
            }
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

    fn open_dialog(&mut self) {
        if let Some(path) = rfd::FileDialog::new().pick_file() {
            self.open_document(path);
        }
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

    fn save_as_dialog(&mut self) {
        let mut dialog = rfd::FileDialog::new();
        if let Some(path) = self.session.current_path() {
            if let Some(parent) = path.parent() {
                dialog = dialog.set_directory(parent);
            }
            if let Some(file_name) = path.file_name() {
                dialog = dialog.set_file_name(file_name.to_string_lossy());
            }
        }

        let Some(path) = dialog.save_file() else {
            return;
        };
        let display_path = path.display().to_string();
        match self.session.save_as_async(path) {
            Ok(true) => {
                self.notice = format!("Saving to {display_path}");
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
        let dirty = self.session.status().is_dirty();
        let title = self
            .session
            .current_path()
            .and_then(|path| path.file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| String::from("No document"));

        egui::TopBottomPanel::top("toolbar")
            .exact_height(52.0)
            .frame(
                egui::Frame::new()
                    .fill(PANEL)
                    .stroke(Stroke::new(1.0_f32, BORDER))
                    .inner_margin(Margin::symmetric(16, 10)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Qem").size(20.0).strong().color(ACCENT));
                    ui.add_space(6.0);
                    ui.label(RichText::new(title).size(15.0).color(TEXT));
                    if dirty {
                        ui.label(RichText::new("Modified").color(WARNING));
                    } else if has_path {
                        ui.label(RichText::new("Saved").color(SAVED));
                    }

                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui
                            .add_enabled(has_path, egui::Button::new("Close"))
                            .clicked()
                        {
                            self.close_document();
                        }
                        if ui
                            .add_enabled(!busy, egui::Button::new("Save As"))
                            .clicked()
                        {
                            self.save_as_dialog();
                        }
                        if ui
                            .add_enabled(has_path && !busy, egui::Button::new("Save"))
                            .clicked()
                        {
                            self.save_current();
                        }
                        if ui.add_enabled(!busy, egui::Button::new("Open")).clicked() {
                            self.open_dialog();
                        }
                    });
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
            .default_width(304.0)
            .min_width(260.0)
            .max_width(380.0)
            .frame(
                egui::Frame::new()
                    .fill(PANEL)
                    .stroke(Stroke::new(1.0_f32, BORDER))
                    .inner_margin(Margin::same(14)),
            )
            .show(ctx, |ui| {
                ui.label(RichText::new("INSPECTOR").small().strong().color(ACCENT));
                ui.label(
                    RichText::new("Large-file controls")
                        .size(18.0)
                        .strong()
                        .color(TEXT),
                );
                ui.label(RichText::new("Bounded viewport and document state").color(MUTED));
                ui.add_space(10.0);
                section_title(ui, "DOCUMENT");

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

                ui.add_space(10.0);
                section_title(ui, "VIEWPORT");
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

                ui.add_space(10.0);
                section_title(ui, "CARET");
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

                ui.add_space(10.0);
                section_title(ui, "ACTIVITY");
                ui.label(RichText::new(&self.notice).color(MUTED));
            });
    }

    fn render_status_bar(&self, ctx: &egui::Context) {
        let status = self.session.status();
        let state = if self.session.is_busy() {
            "Working"
        } else if self.session.is_indexing() {
            "Indexing"
        } else {
            "Ready"
        };
        let state_color = if self.session.is_busy() || self.session.is_indexing() {
            WARNING
        } else {
            SAVED
        };

        egui::TopBottomPanel::bottom("status_bar")
            .exact_height(30.0)
            .frame(
                egui::Frame::new()
                    .fill(PANEL)
                    .stroke(Stroke::new(1.0_f32, BORDER))
                    .inner_margin(Margin::symmetric(12, 5)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(state).color(state_color).strong());
                    ui.separator();
                    ui.label(RichText::new(&self.notice).color(MUTED));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(
                            RichText::new(format!(
                                "Ln {}, Col {}",
                                self.caret.line0() + 1,
                                self.caret.col0() + 1
                            ))
                            .color(TEXT),
                        );
                        ui.separator();
                        ui.label(RichText::new(status.encoding().name()).color(MUTED));
                        ui.separator();
                        ui.label(RichText::new(status.backing().as_str()).color(MUTED));
                        ui.separator();
                        ui.label(
                            RichText::new(if status.is_line_count_exact() {
                                "Exact lines"
                            } else {
                                "Estimated lines"
                            })
                            .color(if status.is_line_count_exact() {
                                SAVED
                            } else {
                                WARNING
                            }),
                        );
                    });
                });
            });
    }

    fn render_editor(&mut self, ctx: &egui::Context) {
        let viewport = self.session.read_viewport(
            ViewportRequest::new(self.first_line0, self.viewport_rows)
                .with_columns(self.start_col, self.viewport_cols),
        );
        let last_visible_line0 = viewport
            .rows()
            .last()
            .map(|row| row.line0())
            .unwrap_or(self.first_line0);
        let mut clicked_caret = None;

        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(BG).inner_margin(Margin::same(14)))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("LARGE-FILE VIEWPORT")
                            .small()
                            .strong()
                            .color(ACCENT),
                    );
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(
                            RichText::new(format!(
                                "Lines {}–{}  ·  Columns {}–{}",
                                self.first_line0 + 1,
                                last_visible_line0 + 1,
                                self.start_col + 1,
                                self.start_col.saturating_add(self.viewport_cols)
                            ))
                            .monospace()
                            .color(MUTED),
                        );
                    });
                });
                ui.add_space(10.0);

                egui::Frame::new()
                    .fill(EDITOR_BG)
                    .stroke(Stroke::new(1.0_f32, BORDER))
                    .corner_radius(CornerRadius::same(6))
                    .show(ui, |ui| {
                        let font = FontId::new(14.0, FontFamily::Monospace);
                        let char_width = 8.4_f32;
                        let content_width =
                            GUTTER_WIDTH + (self.viewport_cols.max(1) as f32 * char_width) + 28.0;

                        ScrollArea::both()
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                ui.set_min_width(content_width.max(ui.available_width()));
                                for row in viewport.rows() {
                                    let is_active = row.line0() == self.caret.line0();
                                    let (rect, response) = ui.allocate_exact_size(
                                        Vec2::new(ui.available_width(), ROW_HEIGHT),
                                        Sense::click(),
                                    );
                                    let painter = ui.painter();

                                    if is_active {
                                        painter.rect_filled(rect, 0.0, ACTIVE_LINE);
                                        painter.rect_filled(
                                            Rect::from_min_size(
                                                rect.min,
                                                Vec2::new(2.0, rect.height()),
                                            ),
                                            0.0,
                                            ACCENT,
                                        );
                                    }

                                    let gutter = Rect::from_min_max(
                                        rect.min,
                                        egui::pos2(rect.min.x + GUTTER_WIDTH, rect.max.y),
                                    );
                                    painter.rect_filled(gutter, 0.0, PANEL);
                                    painter.line_segment(
                                        [gutter.right_top(), gutter.right_bottom()],
                                        Stroke::new(1.0_f32, BORDER),
                                    );

                                    let line_color = if row.is_exact() { MUTED } else { WARNING };
                                    painter.text(
                                        egui::pos2(gutter.right() - 10.0, rect.center().y),
                                        egui::Align2::RIGHT_CENTER,
                                        format!(
                                            "{}{}",
                                            if row.is_exact() { "" } else { "~" },
                                            row.line_number()
                                        ),
                                        font.clone(),
                                        line_color,
                                    );
                                    let text_pos =
                                        egui::pos2(gutter.right() + 12.0, rect.center().y);
                                    painter.text(
                                        text_pos,
                                        egui::Align2::LEFT_CENTER,
                                        if row.text().is_empty() {
                                            " "
                                        } else {
                                            row.text()
                                        },
                                        font.clone(),
                                        TEXT,
                                    );

                                    if is_active && self.editor_has_focus {
                                        let local_col =
                                            self.caret.col0().saturating_sub(self.start_col);
                                        let caret_x = text_pos.x + local_col as f32 * char_width;
                                        painter.line_segment(
                                            [
                                                egui::pos2(caret_x, rect.top() + 3.0),
                                                egui::pos2(caret_x, rect.bottom() - 3.0),
                                            ],
                                            Stroke::new(1.5_f32, ACCENT),
                                        );
                                    }

                                    if response.clicked() {
                                        self.editor_has_focus = true;
                                        if let Some(pointer) = response.interact_pointer_pos() {
                                            let local_col = ((pointer.x - text_pos.x).max(0.0)
                                                / char_width)
                                                .round()
                                                as usize;
                                            let target_col = self
                                                .start_col
                                                .saturating_add(local_col)
                                                .min(self.session.line_len_chars(row.line0()));
                                            clicked_caret =
                                                Some(TextPosition::new(row.line0(), target_col));
                                        }
                                    }
                                }

                                if viewport.rows().is_empty() {
                                    ui.add_space(24.0);
                                    ui.horizontal_centered(|ui| {
                                        ui.label(
                                            RichText::new("No rows in this viewport").color(MUTED),
                                        );
                                    });
                                }
                            });
                    });
            });

        if let Some(caret) = clicked_caret {
            self.set_caret(caret);
        }
    }
}

impl eframe::App for LargeFileDemo {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.pump_session();
        self.handle_editor_input(ctx);
        self.render_toolbar(ctx);
        self.render_status_bar(ctx);
        self.render_sidebar(ctx);
        self.render_editor(ctx);

        if self.session.is_busy() || self.session.is_indexing() {
            ctx.request_repaint_after(Duration::from_millis(16));
        }
    }
}

fn install_theme(ctx: &egui::Context) {
    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = PANEL;
    visuals.window_fill = PANEL;
    visuals.extreme_bg_color = BG;
    visuals.faint_bg_color = ACTIVE_LINE;
    visuals.widgets.noninteractive.bg_fill = PANEL;
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, BORDER);
    visuals.widgets.inactive.bg_fill = Color32::from_rgb(28, 32, 39);
    visuals.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, BORDER);
    visuals.widgets.hovered.bg_fill = Color32::from_rgb(38, 44, 54);
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, ACCENT);
    visuals.widgets.active.bg_fill = Color32::from_rgb(46, 57, 75);
    visuals.widgets.active.bg_stroke = Stroke::new(1.0_f32, ACCENT);
    visuals.selection.bg_fill = Color32::from_rgba_unmultiplied(70, 120, 210, 120);
    visuals.selection.stroke = Stroke::new(1.0_f32, ACCENT);
    visuals.window_corner_radius = CornerRadius::same(6);
    ctx.set_visuals(visuals);

    let mut style = (*ctx.style()).clone();
    style.spacing.item_spacing = Vec2::new(8.0, 6.0);
    style.spacing.button_padding = Vec2::new(10.0, 5.0);
    ctx.set_style(style);
}

fn section_title(ui: &mut egui::Ui, title: &str) {
    egui::Frame::new()
        .fill(CARD)
        .stroke(Stroke::new(1.0_f32, BORDER))
        .corner_radius(CornerRadius::same(4))
        .inner_margin(Margin::symmetric(8, 5))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new(title).small().strong().color(MUTED));
        });
    ui.add_space(5.0);
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
