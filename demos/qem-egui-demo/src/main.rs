// Release builds run as a plain GUI app: no console window is allocated, so
// an accidental QuickEdit text selection in a console can never block the UI
// thread inside a stderr write.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::egui::{
    self, Align, Color32, CornerRadius, FontFamily, FontId, Id, Key, Layout, Margin, Modal, Rect,
    RichText, ScrollArea, Sense, Stroke, StrokeKind, TextEdit, Vec2,
};
use qem::{
    BackgroundActivity, DocumentEncoding, DocumentSession, LiteralSearchQuery, SearchMatch,
    TextPosition, TextSelection, ViewportRequest,
};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

const BG: Color32 = Color32::from_rgb(15, 17, 21);
const PANEL: Color32 = Color32::from_rgb(20, 23, 28);
const EDITOR_BG: Color32 = Color32::from_rgb(18, 20, 25);
const ACTIVE_LINE: Color32 = Color32::from_rgb(28, 33, 42);
const BORDER: Color32 = Color32::from_rgb(42, 47, 57);
const TEXT: Color32 = Color32::from_rgb(215, 220, 230);
const MUTED: Color32 = Color32::from_rgb(126, 135, 151);
const ACCENT: Color32 = Color32::from_rgb(104, 160, 255);
const WARNING: Color32 = Color32::from_rgb(232, 179, 92);
const ERROR: Color32 = Color32::from_rgb(239, 105, 113);
const SAVED: Color32 = Color32::from_rgb(94, 203, 172);
const GUTTER_WIDTH: f32 = 68.0;
const ROW_HEIGHT: f32 = 23.0;

fn main() -> Result<(), eframe::Error> {
    let initial_path = std::env::args_os().nth(1).map(PathBuf::from);
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1240.0, 780.0])
            .with_min_inner_size([900.0, 580.0]),
        ..Default::default()
    };

    eframe::run_native(
        "Qem Editor",
        options,
        Box::new(move |cc| {
            install_theme(&cc.egui_ctx);
            Ok(Box::new(EditorApp::new(initial_path)))
        }),
    )
}

struct UiTab {
    id: u64,
    session: DocumentSession,
    caret: TextPosition,
    desired_col: usize,
    /// Anchor of the active mouse selection; the head is the caret.
    selection_anchor: Option<TextPosition>,
    search_match: Option<SearchMatch>,
    editor_has_focus: bool,
    reveal_caret: bool,
    /// First logical line shown in the bounded editor window.
    ///
    /// This is deliberately an integer, rather than an egui pixel offset:
    /// `line * row_height` loses line identity once a huge file exceeds f32
    /// precision.
    first_line: usize,
    /// A goto target remains pending until the backend returns that exact row.
    pending_goto: Option<usize>,
    wheel_remainder: f32,
    untitled_name: String,
    notice: Notice,
    close_after_save: bool,
    /// Rows currently visible in the editor viewport; drives PageUp/PageDown.
    visible_rows: usize,
}

impl UiTab {
    fn new(id: u64, untitled_number: usize) -> Self {
        Self {
            id,
            session: DocumentSession::new(),
            caret: TextPosition::new(0, 0),
            desired_col: 0,
            selection_anchor: None,
            search_match: None,
            editor_has_focus: true,
            reveal_caret: false,
            first_line: 0,
            pending_goto: None,
            wheel_remainder: 0.0,
            untitled_name: format!("Untitled {untitled_number}"),
            notice: Notice::info("Ready"),
            close_after_save: false,
            visible_rows: 30,
        }
    }

    fn title(&self) -> String {
        self.session
            .current_path()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            .map(str::to_owned)
            .unwrap_or_else(|| self.untitled_name.clone())
    }

    fn set_caret(&mut self, position: TextPosition) {
        let clamped = self.session.clamp_position(position);
        // Exact viewport rows may lie beyond editable metadata. Never turn
        // a click on such a row into a caret on an unrelated prefix line.
        self.caret = if clamped.line0() == position.line0() {
            clamped
        } else {
            position
        };
        self.desired_col = self.caret.col0();
        // Every programmatic caret move collapses the selection. Only the
        // selection-specific paths (drag, Shift+click, Shift+arrows) manage
        // the anchor explicitly; a stale anchor here would make the next
        // typed character delete everything between it and the new caret.
        self.selection_anchor = None;
        self.editor_has_focus = true;
        self.reveal_caret = true;
    }

    /// Returns the active mouse selection, if it spans more than a caret.
    fn active_selection(&self) -> Option<TextSelection> {
        let anchor = self.selection_anchor?;
        let selection = TextSelection::new(anchor, self.caret);
        (!selection.is_caret()).then_some(selection)
    }

    fn collapse_selection(&mut self) {
        self.selection_anchor = None;
    }

    /// Starts or keeps a selection across a navigation move: without Shift the
    /// selection collapses, with Shift it extends from the previous anchor or
    /// the caret the move started from.
    fn finish_navigation(&mut self, extend: bool, caret_before: TextPosition) {
        if extend {
            self.selection_anchor.get_or_insert(caret_before);
        } else {
            self.selection_anchor = None;
        }
    }

    /// Move the bounded logical window without converting a line number to a
    /// large absolute pixel coordinate.
    fn reveal_line(&mut self, line0: usize) {
        let rows = self.visible_rows.max(1);
        let last = self.first_line.saturating_add(rows.saturating_sub(1));
        if line0 < self.first_line {
            self.first_line = line0;
        } else if line0 > last {
            self.first_line = line0.saturating_sub(rows.saturating_sub(1));
        }
        self.reveal_caret = true;
    }

    fn set_first_line(&mut self, first_line: usize, total: usize) {
        let page = self.visible_rows.max(1);
        let max_first = total.saturating_sub(page);
        self.first_line = first_line.min(max_first);
    }

    fn move_view_by(&mut self, delta: isize, total: usize) {
        let next = if delta.is_negative() {
            self.first_line.saturating_sub(delta.unsigned_abs())
        } else {
            self.first_line.saturating_add(delta as usize)
        };
        self.set_first_line(next, total);
    }
}

#[derive(Clone, Copy)]
enum NoticeKind {
    Info,
    Error,
}

struct Notice {
    text: String,
    kind: NoticeKind,
}

impl Notice {
    fn info(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            kind: NoticeKind::Info,
        }
    }

    fn error(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            kind: NoticeKind::Error,
        }
    }
}

struct RenameDialog {
    tab_id: u64,
    name: String,
    error: Option<String>,
}

#[derive(Clone, Copy)]
enum TabAction {
    Activate(u64),
    Save(u64),
    SaveAs(u64),
    Rename(u64),
    CopyPath(u64),
    Close(u64),
    CloseOthers(u64),
    CloseAll,
    New,
}

#[derive(Clone, Copy)]
enum MenuAction {
    New,
    Open,
    Save,
    SaveAs,
    SaveAll,
    SaveAsEncoding(&'static str),
    Rename,
    CopyPath,
    CloseActive,
    CloseOthers,
    CloseAll,
    Undo,
    Redo,
    CopyMatch,
    CutMatch,
    DeleteMatch,
    Find,
    FindNext,
    FindPrevious,
    NextTab,
    PreviousTab,
    CenterCaret,
    ClearSearchHighlight,
    ToggleSearch,
    GoToLine,
}

struct EditorApp {
    tabs: Vec<UiTab>,
    active_tab_id: u64,
    next_tab_id: u64,
    next_untitled_number: usize,
    search_open: bool,
    search_text: String,
    close_queue: VecDeque<u64>,
    close_prompt: Option<u64>,
    rename_dialog: Option<RenameDialog>,
    goto_dialog: Option<String>,
}

impl EditorApp {
    fn new(initial_path: Option<PathBuf>) -> Self {
        let first = UiTab::new(1, 1);
        let mut app = Self {
            tabs: vec![first],
            active_tab_id: 1,
            next_tab_id: 2,
            next_untitled_number: 2,
            search_open: false,
            search_text: String::new(),
            close_queue: VecDeque::new(),
            close_prompt: None,
            rename_dialog: None,
            goto_dialog: None,
        };
        if let Some(path) = initial_path {
            app.open_path(path);
        }
        app
    }

    fn active_index(&self) -> usize {
        self.tabs
            .iter()
            .position(|tab| tab.id == self.active_tab_id)
            .unwrap_or(0)
    }

    fn tab_index(&self, id: u64) -> Option<usize> {
        self.tabs.iter().position(|tab| tab.id == id)
    }

    fn active_tab(&self) -> &UiTab {
        &self.tabs[self.active_index()]
    }

    fn active_tab_mut(&mut self) -> &mut UiTab {
        let index = self.active_index();
        &mut self.tabs[index]
    }

    fn new_tab(&mut self) -> u64 {
        let id = self.next_tab_id;
        self.next_tab_id = self.next_tab_id.wrapping_add(1);
        let number = self.next_untitled_number;
        self.next_untitled_number = self.next_untitled_number.saturating_add(1);
        self.tabs.push(UiTab::new(id, number));
        self.active_tab_id = id;
        id
    }

    fn comparable_path(path: &Path) -> PathBuf {
        std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
    }

    fn open_dialog(&mut self) {
        if let Some(path) = rfd::FileDialog::new().pick_file() {
            self.open_path(path);
        }
    }

    fn open_path(&mut self, path: PathBuf) {
        let comparable = Self::comparable_path(&path);
        if let Some(tab) = self.tabs.iter().find(|tab| {
            tab.session
                .current_path()
                .is_some_and(|open| Self::comparable_path(open) == comparable)
        }) {
            self.active_tab_id = tab.id;
            return;
        }

        let use_active_empty = {
            let tab = self.active_tab();
            tab.session.current_path().is_none()
                && !tab.session.is_dirty()
                && tab.session.status().file_len() == 0
                && !tab.session.is_busy()
        };
        let id = if use_active_empty {
            self.active_tab_id
        } else {
            self.new_tab()
        };
        let Some(index) = self.tab_index(id) else {
            return;
        };
        let tab = &mut self.tabs[index];
        match tab.session.open_file_async(path.clone()) {
            Ok(()) => {
                tab.caret = TextPosition::new(0, 0);
                tab.desired_col = 0;
                tab.selection_anchor = None;
                tab.search_match = None;
                tab.first_line = 0;
                tab.pending_goto = None;
                tab.wheel_remainder = 0.0;
                tab.notice = Notice::info(format!("Opening {}", path.display()));
            }
            Err(error) => tab.notice = Notice::error(format!("Open failed: {error}")),
        }
    }

    fn pump(&mut self) {
        let mut close_ids = Vec::new();
        let mut close_save_failed = false;
        for tab in &mut self.tabs {
            let activity = tab.session.background_activity();
            if let Some(result) = tab.session.poll_background_job() {
                match result {
                    Ok(()) => {
                        if !matches!(activity, BackgroundActivity::Saving(_)) {
                            tab.caret = tab.session.clamp_position(tab.caret);
                            tab.desired_col = tab.caret.col0();
                            // The document was replaced (open/reload): a stale
                            // selection anchor must not survive into it.
                            tab.collapse_selection();
                        }
                        match activity {
                            BackgroundActivity::Saving(_) => {
                                // Keep the viewport where the user left it;
                                // a save never moves the text the caret sees.
                                tab.notice = Notice::info("Saved");
                            }
                            _ => {
                                tab.reveal_line(tab.caret.line0());
                                tab.notice = Notice::info("Ready");
                            }
                        }
                        if tab.close_after_save && !tab.session.is_dirty() {
                            close_ids.push(tab.id);
                        }
                    }
                    Err(error) => {
                        close_save_failed |= tab.close_after_save;
                        tab.close_after_save = false;
                        tab.notice = Notice::error(error.to_string());
                    }
                }
            }
        }
        if close_save_failed {
            self.close_queue.clear();
        }
        for id in close_ids {
            self.remove_tab(id);
        }
        self.advance_close_queue();
    }

    fn save_tab(&mut self, id: u64, close_after_save: bool) {
        let Some(index) = self.tab_index(id) else {
            return;
        };
        if self.tabs[index].session.is_busy() {
            self.tabs[index].notice = Notice::info("Document is busy");
            if close_after_save {
                self.close_queue.clear();
            }
            return;
        }
        if self.tabs[index].session.current_path().is_none() {
            self.save_tab_as(id, close_after_save, None);
            return;
        }
        self.tabs[index].close_after_save = close_after_save;
        match self.tabs[index].session.save_async() {
            Ok(true) => self.tabs[index].notice = Notice::info("Saving..."),
            Ok(false) => {
                self.tabs[index].notice = Notice::info("Already saved");
                if close_after_save {
                    self.remove_tab(id);
                }
            }
            Err(error) => {
                self.tabs[index].close_after_save = false;
                if close_after_save {
                    self.close_queue.clear();
                }
                self.tabs[index].notice = Notice::error(format!("Save failed: {error}"));
            }
        }
    }

    fn save_all_tabs(&mut self) {
        let ids: Vec<_> = self.tabs.iter().map(|tab| tab.id).collect();
        for id in ids {
            self.save_tab(id, false);
        }
    }

    fn save_tab_as(
        &mut self,
        id: u64,
        close_after_save: bool,
        encoding_label: Option<&'static str>,
    ) {
        let Some(index) = self.tab_index(id) else {
            return;
        };
        if self.tabs[index].session.is_busy() {
            self.tabs[index].notice = Notice::info("Document is busy");
            if close_after_save {
                self.close_queue.clear();
            }
            return;
        }
        let encoding = encoding_label.and_then(DocumentEncoding::from_label);
        if let Some(encoding) = encoding {
            if let Some(error) = self.tabs[index].session.save_error_for_encoding(encoding) {
                let label = encoding_label.unwrap_or("selected encoding");
                self.tabs[index].notice = Notice::error(format!("Cannot save as {label}: {error}"));
                if close_after_save {
                    self.close_queue.clear();
                }
                return;
            }
        }
        let mut dialog = rfd::FileDialog::new();
        if let Some(path) = self.tabs[index].session.current_path() {
            if let Some(parent) = path.parent() {
                dialog = dialog.set_directory(parent);
            }
            if let Some(name) = path.file_name().and_then(|name| name.to_str()) {
                dialog = dialog.set_file_name(name);
            }
        } else {
            dialog = dialog.set_file_name(&self.tabs[index].untitled_name);
        }
        let Some(path) = dialog.save_file() else {
            self.tabs[index].close_after_save = false;
            if close_after_save {
                self.close_queue.clear();
            }
            return;
        };
        self.tabs[index].close_after_save = close_after_save;
        let result = if let Some(encoding) = encoding {
            self.tabs[index]
                .session
                .save_as_async_with_encoding(path.clone(), encoding)
        } else {
            self.tabs[index].session.save_as_async(path.clone())
        };
        match result {
            Ok(true) => {
                self.tabs[index].notice = Notice::info(format!("Saving {}", path.display()));
            }
            Ok(false) => {
                self.tabs[index].notice = Notice::info("Already saved");
                if close_after_save {
                    self.remove_tab(id);
                }
            }
            Err(error) => {
                self.tabs[index].close_after_save = false;
                if close_after_save {
                    self.close_queue.clear();
                }
                self.tabs[index].notice = Notice::error(format!("Save failed: {error}"));
            }
        }
    }

    fn request_close(&mut self, id: u64) {
        let Some(index) = self.tab_index(id) else {
            return;
        };
        self.active_tab_id = id;
        if self.tabs[index].session.is_busy() {
            self.tabs[index].notice = Notice::info("Wait for the current file operation");
        } else if self.tabs[index].session.is_dirty() {
            self.close_prompt = Some(id);
        } else {
            self.remove_tab(id);
        }
    }

    fn queue_close(&mut self, ids: impl IntoIterator<Item = u64>) {
        self.close_queue.clear();
        self.close_queue.extend(ids);
        self.advance_close_queue();
    }

    fn advance_close_queue(&mut self) {
        if self.close_prompt.is_some() || self.tabs.iter().any(|tab| tab.close_after_save) {
            return;
        }
        while let Some(id) = self.close_queue.pop_front() {
            if self.tab_index(id).is_some() {
                self.request_close(id);
                break;
            }
        }
    }

    fn remove_tab(&mut self, id: u64) {
        let Some(index) = self.tab_index(id) else {
            return;
        };
        self.tabs.remove(index);
        if self.tabs.is_empty() {
            self.new_tab();
        } else if self.active_tab_id == id {
            self.active_tab_id = self.tabs[index.min(self.tabs.len() - 1)].id;
        }
    }

    fn find(&mut self, backwards: bool) {
        let Some(query) = LiteralSearchQuery::new(self.search_text.clone()) else {
            self.active_tab_mut().search_match = None;
            return;
        };
        let tab = self.active_tab_mut();
        let found = if backwards {
            let before = tab
                .search_match
                .map(SearchMatch::start)
                .unwrap_or(tab.caret);
            tab.session.find_prev_query(&query, before).or_else(|| {
                let last_line = tab.session.display_line_count().saturating_sub(1);
                let document_end =
                    TextPosition::new(last_line, tab.session.line_len_chars(last_line));
                tab.session.find_prev_query(&query, document_end)
            })
        } else {
            let from = tab.search_match.map(SearchMatch::end).unwrap_or(tab.caret);
            tab.session
                .find_next_query(&query, from)
                .or_else(|| tab.session.find_next_query(&query, TextPosition::new(0, 0)))
        };

        if let Some(found) = found {
            tab.search_match = Some(found);
            tab.set_caret(found.start());
            tab.reveal_line(found.start().line0());
            tab.notice = Notice::info(format!(
                "Match at {}:{}",
                found.start().line0() + 1,
                found.start().col0() + 1
            ));
        } else {
            tab.search_match = None;
            tab.notice = Notice::info("No matches");
        }
    }

    fn switch_tab(&mut self, delta: isize) {
        if self.tabs.len() < 2 {
            return;
        }
        let current = self.active_index();
        let next = if delta.is_negative() {
            current.checked_sub(1).unwrap_or(self.tabs.len() - 1)
        } else {
            (current + 1) % self.tabs.len()
        };
        self.active_tab_id = self.tabs[next].id;
        self.tabs[next].editor_has_focus = true;
    }

    fn shortcuts(&mut self, ctx: &egui::Context) {
        let command = ctx.input(|input| input.modifiers.command || input.modifiers.ctrl);
        // While the search or rename text field owns the keyboard, the
        // field-native clipboard/undo behavior wins: document-level
        // Copy/Cut/Undo/Redo chords must not fire behind the widget.
        let text_field_focused = ctx.memory(|memory| memory.focused()).is_some_and(|id| {
            id == Id::new("qem-search-field")
                || id == Id::new("qem-rename-field")
                || id == Id::new("qem-goto-field")
        });
        if command && ctx.input(|input| input.key_pressed(Key::N)) {
            self.new_tab();
        }
        if command && ctx.input(|input| input.key_pressed(Key::O)) {
            self.open_dialog();
        }
        if command && ctx.input(|input| input.key_pressed(Key::S)) {
            let id = self.active_tab_id;
            if ctx.input(|input| input.modifiers.shift) {
                self.save_tab_as(id, false, None);
            } else {
                self.save_tab(id, false);
            }
        }
        if command && ctx.input(|input| input.key_pressed(Key::F)) {
            self.search_open = true;
            self.active_tab_mut().editor_has_focus = false;
        }
        if command && ctx.input(|input| input.key_pressed(Key::W)) {
            self.request_close(self.active_tab_id);
        }
        if command && !text_field_focused && ctx.input(|input| input.key_pressed(Key::Z)) {
            self.apply_menu_action(ctx, MenuAction::Undo);
        }
        if command && !text_field_focused && ctx.input(|input| input.key_pressed(Key::Y)) {
            self.apply_menu_action(ctx, MenuAction::Redo);
        }
        if command && !text_field_focused && ctx.input(|input| input.key_pressed(Key::C)) {
            self.apply_menu_action(ctx, MenuAction::CopyMatch);
        }
        if command && !text_field_focused && ctx.input(|input| input.key_pressed(Key::X)) {
            self.apply_menu_action(ctx, MenuAction::CutMatch);
        }
        if command && ctx.input(|input| input.key_pressed(Key::G)) {
            self.apply_menu_action(ctx, MenuAction::GoToLine);
        }
        if ctx.input(|input| input.key_pressed(Key::Tab) && input.modifiers.ctrl) {
            if ctx.input(|input| input.modifiers.shift) {
                self.switch_tab(-1);
            } else {
                self.switch_tab(1);
            }
        }
        if ctx.input(|input| input.key_pressed(Key::Escape)) {
            if !self.search_open && self.active_tab().selection_anchor.is_some() {
                self.active_tab_mut().collapse_selection();
            } else if self.search_open {
                self.search_open = false;
                self.active_tab_mut().search_match = None;
                self.active_tab_mut().editor_has_focus = true;
            }
        }
    }

    fn editor_input(&mut self, ctx: &egui::Context) {
        if self.search_open
            || self.close_prompt.is_some()
            || self.rename_dialog.is_some()
            || self.goto_dialog.is_some()
            || self.active_tab().pending_goto.is_some()
            || self.active_tab().session.is_busy()
        {
            return;
        }
        if !self.active_tab().editor_has_focus {
            return;
        }
        for event in ctx.input(|input| input.events.clone()) {
            // Library edits clamp to their editable prefix. Navigation may
            // legitimately be beyond it, but an edit must never be redirected.
            let mutation = match &event {
                egui::Event::Text(_) | egui::Event::Paste(_) => true,
                egui::Event::Key {
                    key, pressed: true, ..
                } => {
                    matches!(key, Key::Enter | Key::Backspace | Key::Delete)
                }
                _ => false,
            };
            if mutation {
                let tab = self.active_tab_mut();
                if tab.session.clamp_position(tab.caret) != tab.caret {
                    // The typed reason from the backend tells the user whether
                    // the line is temporarily unresolvable or permanently
                    // outside the bounded edit window.
                    let reason = match tab.session.edit_capability_at(tab.caret) {
                        qem::EditCapability::Unsupported { reason, .. } => reason.to_owned(),
                        _ => "This line is visible but not yet available for editing".to_owned(),
                    };
                    tab.notice = Notice::error(reason);
                    continue;
                }
                // Typing, pasting, Backspace and Delete replace or remove the
                // active selection before the per-event handling runs.
                if let Some(selection) = tab.active_selection() {
                    let result = tab.session.try_delete_selection(selection);
                    match result {
                        Ok(result) => {
                            let tab = self.active_tab_mut();
                            tab.set_caret(result.cursor());
                            tab.collapse_selection();
                        }
                        Err(error) => {
                            self.active_tab_mut().notice = Notice::error(error.to_string());
                            continue;
                        }
                    }
                    // A bare Backspace/Delete press has now done its whole job.
                    if matches!(
                        &event,
                        egui::Event::Key {
                            key: Key::Backspace | Key::Delete,
                            ..
                        }
                    ) {
                        continue;
                    }
                }
            }
            match event {
                egui::Event::Text(text) if !text.chars().any(char::is_control) => {
                    self.insert(&text);
                }
                egui::Event::Paste(text) => self.insert(&text),
                egui::Event::Key {
                    key,
                    pressed: true,
                    modifiers,
                    ..
                } if !modifiers.command && !modifiers.ctrl && !modifiers.alt => match key {
                    Key::ArrowLeft => {
                        let before = self.active_tab().caret;
                        self.move_left();
                        self.active_tab_mut()
                            .finish_navigation(modifiers.shift, before);
                    }
                    Key::ArrowRight => {
                        let before = self.active_tab().caret;
                        self.move_right();
                        self.active_tab_mut()
                            .finish_navigation(modifiers.shift, before);
                    }
                    Key::ArrowUp => {
                        let before = self.active_tab().caret;
                        self.move_vertical(-1);
                        self.active_tab_mut()
                            .finish_navigation(modifiers.shift, before);
                    }
                    Key::ArrowDown => {
                        let before = self.active_tab().caret;
                        self.move_vertical(1);
                        self.active_tab_mut()
                            .finish_navigation(modifiers.shift, before);
                    }
                    Key::PageUp => {
                        let before = self.active_tab().caret;
                        let page = self.active_tab().visible_rows.max(1) as isize;
                        self.move_vertical(-page);
                        self.active_tab_mut()
                            .finish_navigation(modifiers.shift, before);
                    }
                    Key::PageDown => {
                        let before = self.active_tab().caret;
                        let page = self.active_tab().visible_rows.max(1) as isize;
                        self.move_vertical(page);
                        self.active_tab_mut()
                            .finish_navigation(modifiers.shift, before);
                    }
                    Key::Home => {
                        let before = self.active_tab().caret;
                        let line = self.active_tab().caret.line0();
                        self.active_tab_mut().set_caret(TextPosition::new(line, 0));
                        self.active_tab_mut()
                            .finish_navigation(modifiers.shift, before);
                    }
                    Key::End => {
                        let before = self.active_tab().caret;
                        let line = self.active_tab().caret.line0();
                        let col = self.active_tab().session.line_len_chars(line);
                        self.active_tab_mut()
                            .set_caret(TextPosition::new(line, col));
                        self.active_tab_mut()
                            .finish_navigation(modifiers.shift, before);
                    }
                    Key::Enter => {
                        let ending = self.active_tab().session.line_ending().as_str().to_owned();
                        self.insert(&ending);
                    }
                    Key::Backspace => {
                        let caret = self.active_tab().caret;
                        let result = self.active_tab_mut().session.try_backspace(caret);
                        match result {
                            Ok(result) => self.active_tab_mut().set_caret(result.cursor()),
                            Err(error) => {
                                self.active_tab_mut().notice = Notice::error(error.to_string());
                            }
                        }
                    }
                    Key::Delete => {
                        let caret = self.active_tab().caret;
                        let result = self.active_tab_mut().session.try_delete_forward(caret);
                        match result {
                            Ok(result) => self.active_tab_mut().set_caret(result.cursor()),
                            Err(error) => {
                                self.active_tab_mut().notice = Notice::error(error.to_string());
                            }
                        }
                    }
                    _ => {}
                },
                _ => {}
            }
        }
    }

    fn insert(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let caret = self.active_tab().caret;
        let result = self.active_tab_mut().session.try_insert(caret, text);
        match result {
            Ok(position) => self.active_tab_mut().set_caret(position),
            Err(error) => {
                self.active_tab_mut().notice = Notice::error(format!("Edit failed: {error}"));
            }
        }
    }

    fn move_left(&mut self) {
        let caret = self.active_tab().caret;
        if caret.col0() > 0 {
            self.active_tab_mut()
                .set_caret(TextPosition::new(caret.line0(), caret.col0() - 1));
        } else if caret.line0() > 0 {
            let line = caret.line0() - 1;
            let col = self.active_tab().session.line_len_chars(line);
            self.active_tab_mut()
                .set_caret(TextPosition::new(line, col));
        }
    }

    fn move_right(&mut self) {
        let caret = self.active_tab().caret;
        let line_len = self.active_tab().session.line_len_chars(caret.line0());
        if caret.col0() < line_len {
            self.active_tab_mut()
                .set_caret(TextPosition::new(caret.line0(), caret.col0() + 1));
        } else if caret.line0() + 1 < self.active_tab().session.display_line_count() {
            self.active_tab_mut()
                .set_caret(TextPosition::new(caret.line0() + 1, 0));
        }
    }

    fn move_vertical(&mut self, delta: isize) {
        let tab = self.active_tab_mut();
        let total = tab.session.display_line_count().max(1);
        let line = if delta.is_negative() {
            tab.caret.line0().saturating_sub(delta.unsigned_abs())
        } else {
            tab.caret
                .line0()
                .saturating_add(delta as usize)
                .min(total - 1)
        };
        let col = tab.desired_col.min(tab.session.line_len_chars(line));
        tab.caret = TextPosition::new(line, col);
        tab.editor_has_focus = true;
        tab.reveal_line(line);
    }

    fn top_bar(&mut self, ctx: &egui::Context) {
        let mut menu_action = None;
        let mut toolbar_action = None;
        let tab = self.active_tab();
        let status = tab.session.status();
        let busy = status.is_busy();
        let title = tab.title();
        let current_encoding = tab.session.encoding();
        // Encoding availability is NOT pre-validated here: the conversion
        // preflight (`save_error_for_encoding`) materializes the whole
        // document for the encode check, which on a 50 GiB file froze the
        // render loop when invoked per frame for every menu entry. The
        // entries stay enabled and the same typed check runs once, on
        // selection, inside `save_tab_as`.
        let encoding_availability: Vec<_> = encoding_choices()
            .into_iter()
            .map(|label| (label, !busy))
            .collect();
        egui::TopBottomPanel::top("titlebar")
            .exact_height(58.0)
            .frame(
                egui::Frame::new()
                    .fill(PANEL)
                    .inner_margin(Margin::symmetric(10, 4)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Q").size(19.0).strong().color(ACCENT));
                    ui.label(RichText::new("Qem Editor").size(15.0).strong());
                    ui.separator();
                    ui.label(RichText::new(title).monospace().color(TEXT));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if toolbar_button(ui, "Save", !busy).clicked() {
                            toolbar_action = Some(MenuAction::Save);
                        }
                        if toolbar_button(ui, "Save As", !busy).clicked() {
                            toolbar_action = Some(MenuAction::SaveAs);
                        }
                        if toolbar_button(ui, "Open", true).clicked() {
                            toolbar_action = Some(MenuAction::Open);
                        }
                        if toolbar_button(ui, "New", true).clicked() {
                            toolbar_action = Some(MenuAction::New);
                        }
                    });
                });
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 2.0;
                    ui.menu_button("File", |ui| {
                        if ui.button("New Tab        Ctrl+N").clicked() {
                            menu_action = Some(MenuAction::New);
                            ui.close();
                        }
                        if ui.button("Open...        Ctrl+O").clicked() {
                            menu_action = Some(MenuAction::Open);
                            ui.close();
                        }
                        ui.separator();
                        if ui
                            .add_enabled(!busy, egui::Button::new("Save        Ctrl+S"))
                            .clicked()
                        {
                            menu_action = Some(MenuAction::Save);
                            ui.close();
                        }
                        if ui
                            .add_enabled(!busy, egui::Button::new("Save As...        Ctrl+Shift+S"))
                            .clicked()
                        {
                            menu_action = Some(MenuAction::SaveAs);
                            ui.close();
                        }
                        if ui.button("Save All").clicked() {
                            menu_action = Some(MenuAction::SaveAll);
                            ui.close();
                        }
                        ui.separator();
                        if ui
                            .add_enabled(!busy, egui::Button::new("Rename File..."))
                            .clicked()
                        {
                            menu_action = Some(MenuAction::Rename);
                            ui.close();
                        }
                        if ui.button("Copy Path").clicked() {
                            menu_action = Some(MenuAction::CopyPath);
                            ui.close();
                        }
                        ui.separator();
                        if ui.button("Close Tab        Ctrl+W").clicked() {
                            menu_action = Some(MenuAction::CloseActive);
                            ui.close();
                        }
                        if ui.button("Close Other Tabs").clicked() {
                            menu_action = Some(MenuAction::CloseOthers);
                            ui.close();
                        }
                        if ui.button("Close All Tabs").clicked() {
                            menu_action = Some(MenuAction::CloseAll);
                            ui.close();
                        }
                    });
                    ui.menu_button("Edit", |ui| {
                        if ui.button("Undo        Ctrl+Z").clicked() {
                            menu_action = Some(MenuAction::Undo);
                            ui.close();
                        }
                        if ui.button("Redo        Ctrl+Y").clicked() {
                            menu_action = Some(MenuAction::Redo);
                            ui.close();
                        }
                        ui.separator();
                        if ui.button("Copy Match        Ctrl+C").clicked() {
                            menu_action = Some(MenuAction::CopyMatch);
                            ui.close();
                        }
                        if ui.button("Cut Match        Ctrl+X").clicked() {
                            menu_action = Some(MenuAction::CutMatch);
                            ui.close();
                        }
                        if ui.button("Delete Match").clicked() {
                            menu_action = Some(MenuAction::DeleteMatch);
                            ui.close();
                        }
                    });
                    ui.menu_button("Search", |ui| {
                        if ui.button("Find        Ctrl+F").clicked() {
                            menu_action = Some(MenuAction::Find);
                            ui.close();
                        }
                        if ui.button("Find Next        Enter").clicked() {
                            menu_action = Some(MenuAction::FindNext);
                            ui.close();
                        }
                        if ui.button("Find Previous        Shift+Enter").clicked() {
                            menu_action = Some(MenuAction::FindPrevious);
                            ui.close();
                        }
                        ui.separator();
                        if ui.button("Go to Line        Ctrl+G").clicked() {
                            menu_action = Some(MenuAction::GoToLine);
                            ui.close();
                        }
                        ui.separator();
                        let mut open = self.search_open;
                        if ui.toggle_value(&mut open, "Search Bar").clicked() {
                            menu_action = Some(MenuAction::ToggleSearch);
                        }
                    });
                    ui.menu_button("Encoding", |ui| {
                        ui.label(
                            RichText::new(format!("Current: {}", current_encoding.name())).small(),
                        );
                        ui.separator();
                        for &(label, enabled) in &encoding_availability {
                            if ui
                                .add_enabled(enabled, egui::Button::new(format!("Save As {label}")))
                                .clicked()
                            {
                                menu_action = Some(MenuAction::SaveAsEncoding(label));
                                ui.close();
                            }
                        }
                    });
                    ui.menu_button("Tabs", |ui| {
                        if ui.button("Next Tab        Ctrl+Tab").clicked() {
                            menu_action = Some(MenuAction::NextTab);
                            ui.close();
                        }
                        if ui.button("Previous Tab        Ctrl+Shift+Tab").clicked() {
                            menu_action = Some(MenuAction::PreviousTab);
                            ui.close();
                        }
                        ui.separator();
                        if ui.button("Close Current").clicked() {
                            menu_action = Some(MenuAction::CloseActive);
                            ui.close();
                        }
                        if ui.button("Close Others").clicked() {
                            menu_action = Some(MenuAction::CloseOthers);
                            ui.close();
                        }
                        if ui.button("Close All").clicked() {
                            menu_action = Some(MenuAction::CloseAll);
                            ui.close();
                        }
                    });
                    ui.menu_button("View", |ui| {
                        if ui.button("Center Caret").clicked() {
                            menu_action = Some(MenuAction::CenterCaret);
                            ui.close();
                        }
                        if ui.button("Clear Search Highlight").clicked() {
                            menu_action = Some(MenuAction::ClearSearchHighlight);
                            ui.close();
                        }
                    });
                });
            });
        if let Some(action) = toolbar_action.or(menu_action) {
            self.apply_menu_action(ctx, action);
        }
    }

    fn tab_bar(&mut self, ctx: &egui::Context) {
        let mut action = None;
        egui::TopBottomPanel::top("tabbar")
            .exact_height(54.0)
            .frame(
                egui::Frame::new()
                    .fill(BG)
                    .inner_margin(Margin::symmetric(10, 7)),
            )
            .show(ctx, |ui| {
                ScrollArea::horizontal()
                    .id_salt("document-tabs")
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            for tab in &self.tabs {
                                let active = tab.id == self.active_tab_id;
                                let status = tab.session.status();
                                let (status_text, status_color) = if status.is_busy() {
                                    ("BUSY", ACCENT)
                                } else if status.is_dirty() {
                                    ("EDITED", WARNING)
                                } else {
                                    ("SAVED", SAVED)
                                };
                                let frame = egui::Frame::new()
                                    .fill(if active {
                                        Color32::from_rgb(34, 40, 51)
                                    } else {
                                        Color32::from_rgb(24, 28, 35)
                                    })
                                    .stroke(Stroke::new(
                                        1.0_f32,
                                        if active { ACCENT } else { BORDER },
                                    ))
                                    .corner_radius(CornerRadius::same(11))
                                    .inner_margin(Margin::symmetric(11, 6));
                                let response = frame
                                    .show(ui, |ui| {
                                        ui.horizontal(|ui| {
                                            let status_response = ui.add(
                                                egui::Label::new(
                                                    RichText::new(status_text)
                                                        .size(9.0)
                                                        .strong()
                                                        .color(status_color),
                                                )
                                                .sense(Sense::click()),
                                            );
                                            let title_response = ui.add(
                                                egui::Label::new(
                                                    RichText::new(tab.title())
                                                        .monospace()
                                                        .color(if active { TEXT } else { MUTED }),
                                                )
                                                .sense(Sense::click()),
                                            );
                                            let close = close_icon_button(ui, active);
                                            if close.clicked() {
                                                action = Some(TabAction::Close(tab.id));
                                            } else if status_response.clicked()
                                                || title_response.clicked()
                                            {
                                                action = Some(TabAction::Activate(tab.id));
                                            }
                                            title_response
                                        })
                                        .inner
                                    })
                                    .inner;
                                response.context_menu(|ui| {
                                    if ui.button("Save").clicked() {
                                        action = Some(TabAction::Save(tab.id));
                                        ui.close();
                                    }
                                    if ui.button("Save As").clicked() {
                                        action = Some(TabAction::SaveAs(tab.id));
                                        ui.close();
                                    }
                                    if ui.button("Rename File").clicked() {
                                        action = Some(TabAction::Rename(tab.id));
                                        ui.close();
                                    }
                                    if ui.button("Copy Path").clicked() {
                                        action = Some(TabAction::CopyPath(tab.id));
                                        ui.close();
                                    }
                                    ui.separator();
                                    if ui.button("Close").clicked() {
                                        action = Some(TabAction::Close(tab.id));
                                        ui.close();
                                    }
                                    if ui.button("Close Others").clicked() {
                                        action = Some(TabAction::CloseOthers(tab.id));
                                        ui.close();
                                    }
                                    if ui.button("Close All").clicked() {
                                        action = Some(TabAction::CloseAll);
                                        ui.close();
                                    }
                                });
                                ui.add_space(5.0);
                            }
                            let add = egui::Frame::new()
                                .fill(Color32::from_rgb(27, 33, 42))
                                .stroke(Stroke::new(1.0_f32, BORDER))
                                .corner_radius(CornerRadius::same(14))
                                .inner_margin(Margin::symmetric(13, 6))
                                .show(ui, |ui| {
                                    ui.label(RichText::new("+").size(18.0).strong().color(ACCENT));
                                })
                                .response
                                .interact(Sense::click())
                                .on_hover_text("New tab (Ctrl+N)");
                            if add.clicked() {
                                action = Some(TabAction::New);
                            }
                        });
                    });
            });
        if let Some(action) = action {
            self.apply_tab_action(ctx, action);
        }
    }

    fn apply_tab_action(&mut self, ctx: &egui::Context, action: TabAction) {
        match action {
            TabAction::Activate(id) => self.active_tab_id = id,
            TabAction::Save(id) => self.save_tab(id, false),
            TabAction::SaveAs(id) => self.save_tab_as(id, false, None),
            TabAction::Rename(id) => self.begin_rename(id),
            TabAction::CopyPath(id) => {
                if let Some(path) = self
                    .tab_index(id)
                    .and_then(|index| self.tabs[index].session.current_path())
                {
                    ctx.copy_text(path.display().to_string());
                } else if let Some(index) = self.tab_index(id) {
                    self.tabs[index].notice = Notice::info("This tab has no file path yet");
                }
            }
            TabAction::Close(id) => self.request_close(id),
            TabAction::CloseOthers(keep) => {
                let ids: Vec<_> = self
                    .tabs
                    .iter()
                    .filter(|tab| tab.id != keep)
                    .map(|tab| tab.id)
                    .collect();
                self.active_tab_id = keep;
                self.queue_close(ids);
            }
            TabAction::CloseAll => {
                let ids: Vec<_> = self.tabs.iter().map(|tab| tab.id).collect();
                self.queue_close(ids);
            }
            TabAction::New => {
                self.new_tab();
            }
        }
    }

    fn apply_menu_action(&mut self, ctx: &egui::Context, action: MenuAction) {
        match action {
            MenuAction::New => {
                self.new_tab();
            }
            MenuAction::Open => self.open_dialog(),
            MenuAction::Save => self.save_tab(self.active_tab_id, false),
            MenuAction::SaveAs => self.save_tab_as(self.active_tab_id, false, None),
            MenuAction::SaveAll => self.save_all_tabs(),
            MenuAction::SaveAsEncoding(label) => {
                self.save_tab_as(self.active_tab_id, false, Some(label));
            }
            MenuAction::Rename => self.begin_rename(self.active_tab_id),
            MenuAction::CopyPath => {
                self.apply_tab_action(ctx, TabAction::CopyPath(self.active_tab_id));
            }
            MenuAction::CloseActive => self.request_close(self.active_tab_id),
            MenuAction::CloseOthers => {
                self.apply_tab_action(ctx, TabAction::CloseOthers(self.active_tab_id));
            }
            MenuAction::CloseAll => self.apply_tab_action(ctx, TabAction::CloseAll),
            MenuAction::Undo => match self.active_tab_mut().session.try_undo() {
                Ok(result) => {
                    let tab = self.active_tab_mut();
                    if result.changed() {
                        if let Some(cursor) = result.cursor() {
                            tab.set_caret(cursor);
                        }
                        tab.notice = Notice::info("Undo");
                    } else {
                        tab.notice = Notice::info("Nothing to undo");
                    }
                }
                Err(error) => {
                    self.active_tab_mut().notice = Notice::error(format!("Undo failed: {error}"));
                }
            },
            MenuAction::Redo => match self.active_tab_mut().session.try_redo() {
                Ok(result) => {
                    let tab = self.active_tab_mut();
                    if result.changed() {
                        if let Some(cursor) = result.cursor() {
                            tab.set_caret(cursor);
                        }
                        tab.notice = Notice::info("Redo");
                    } else {
                        tab.notice = Notice::info("Nothing to redo");
                    }
                }
                Err(error) => {
                    self.active_tab_mut().notice = Notice::error(format!("Redo failed: {error}"));
                }
            },
            MenuAction::CopyMatch => {
                let selection = self
                    .active_tab()
                    .active_selection()
                    .or_else(|| self.match_selection());
                if let Some(selection) = selection {
                    let text = self
                        .active_tab()
                        .session
                        .read_selection(selection)
                        .to_string();
                    ctx.copy_text(text);
                    self.active_tab_mut().notice = Notice::info("Copied selection");
                } else {
                    self.active_tab_mut().notice = Notice::info("No selection or match to copy");
                }
            }
            MenuAction::CutMatch => {
                let selection = self
                    .active_tab()
                    .active_selection()
                    .or_else(|| self.match_selection());
                if let Some(selection) = selection {
                    let result = self.active_tab_mut().session.try_cut_selection(selection);
                    match result {
                        Ok(result) => {
                            let cursor = result.cursor();
                            ctx.copy_text(result.text().to_string());
                            let tab = self.active_tab_mut();
                            tab.set_caret(cursor);
                            tab.collapse_selection();
                            tab.search_match = None;
                        }
                        Err(error) => {
                            self.active_tab_mut().notice =
                                Notice::error(format!("Cut failed: {error}"));
                        }
                    }
                } else {
                    self.active_tab_mut().notice = Notice::info("No selection or match to cut");
                }
            }
            MenuAction::DeleteMatch => {
                let selection = self
                    .active_tab()
                    .active_selection()
                    .or_else(|| self.match_selection());
                if let Some(selection) = selection {
                    let result = self
                        .active_tab_mut()
                        .session
                        .try_delete_selection(selection);
                    match result {
                        Ok(result) => {
                            let cursor = result.cursor();
                            let tab = self.active_tab_mut();
                            tab.set_caret(cursor);
                            tab.collapse_selection();
                            tab.search_match = None;
                        }
                        Err(error) => {
                            self.active_tab_mut().notice =
                                Notice::error(format!("Delete failed: {error}"));
                        }
                    }
                } else {
                    self.active_tab_mut().notice = Notice::info("No selection or match to delete");
                }
            }
            MenuAction::Find => {
                self.search_open = true;
                self.active_tab_mut().editor_has_focus = false;
            }
            MenuAction::FindNext => self.find(false),
            MenuAction::FindPrevious => self.find(true),
            MenuAction::NextTab => self.switch_tab(1),
            MenuAction::PreviousTab => self.switch_tab(-1),
            MenuAction::CenterCaret => {
                let line = self.active_tab().caret.line0();
                self.active_tab_mut().reveal_line(line);
            }
            MenuAction::ClearSearchHighlight => {
                self.active_tab_mut().search_match = None;
            }
            MenuAction::ToggleSearch => {
                self.search_open = !self.search_open;
                self.active_tab_mut().editor_has_focus = !self.search_open;
            }
            MenuAction::GoToLine => self.begin_goto_line(),
        }
    }

    fn match_selection(&self) -> Option<TextSelection> {
        self.active_tab().search_match.map(SearchMatch::selection)
    }

    fn begin_rename(&mut self, id: u64) {
        let Some(index) = self.tab_index(id) else {
            return;
        };
        if self.tabs[index].session.current_path().is_none() {
            self.save_tab_as(id, false, None);
            return;
        }
        if self.tabs[index].session.is_busy() {
            self.tabs[index].notice = Notice::info("Wait for the current file operation");
            return;
        }
        self.active_tab_id = id;
        self.rename_dialog = Some(RenameDialog {
            tab_id: id,
            name: self.tabs[index].title(),
            error: None,
        });
    }

    fn commit_rename(&mut self) {
        let Some(dialog) = self.rename_dialog.as_ref() else {
            return;
        };
        let id = dialog.tab_id;
        let name = dialog.name.trim().to_owned();
        if name.is_empty() || name.contains('/') || name.contains('\\') {
            if let Some(dialog) = &mut self.rename_dialog {
                dialog.error = Some("Enter a file name without path separators".to_owned());
            }
            return;
        }
        let Some(index) = self.tab_index(id) else {
            self.rename_dialog = None;
            return;
        };
        let Some(old_path) = self.tabs[index]
            .session
            .current_path()
            .map(Path::to_path_buf)
        else {
            self.rename_dialog = None;
            self.save_tab_as(id, false, None);
            return;
        };
        let Some(parent) = old_path.parent() else {
            if let Some(dialog) = &mut self.rename_dialog {
                dialog.error = Some("The current file has no parent directory".to_owned());
            }
            return;
        };
        let new_path = parent.join(name);
        if new_path == old_path {
            self.rename_dialog = None;
            return;
        }
        if new_path.exists() {
            if let Some(dialog) = &mut self.rename_dialog {
                dialog.error = Some("A file with that name already exists".to_owned());
            }
            return;
        }
        match std::fs::rename(&old_path, &new_path) {
            Ok(()) => {
                self.tabs[index].session.set_path(new_path.clone());
                self.tabs[index].notice =
                    Notice::info(format!("Renamed to {}", new_path.display()));
                self.rename_dialog = None;
            }
            Err(error) => {
                if let Some(dialog) = &mut self.rename_dialog {
                    dialog.error = Some(format!("Rename failed: {error}"));
                }
            }
        }
    }

    fn search_bar(&mut self, ctx: &egui::Context) {
        if !self.search_open {
            return;
        }
        egui::TopBottomPanel::top("searchbar")
            .exact_height(42.0)
            .frame(
                egui::Frame::new()
                    .fill(EDITOR_BG)
                    .inner_margin(Margin::symmetric(12, 6)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Find").color(MUTED));
                    let response = ui.add(
                        TextEdit::singleline(&mut self.search_text)
                            .id(Id::new("qem-search-field"))
                            .desired_width(360.0)
                            .hint_text("Search in file")
                            .font(FontId::monospace(14.0)),
                    );
                    response.request_focus();
                    if response.changed() {
                        self.active_tab_mut().search_match = None;
                    }
                    if response.has_focus() && ui.input(|input| input.key_pressed(Key::Enter)) {
                        self.find(ui.input(|input| input.modifiers.shift));
                    }
                    if ui.small_button("Previous").clicked() {
                        self.find(true);
                    }
                    if ui.small_button("Next").clicked() {
                        self.find(false);
                    }
                    if ui.small_button("Close").clicked() {
                        self.search_open = false;
                        self.active_tab_mut().search_match = None;
                        self.active_tab_mut().editor_has_focus = true;
                    }
                });
            });
    }

    fn editor(&mut self, ctx: &egui::Context) {
        let index = self.active_index();
        let status = self.tabs[index].session.status();
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(EDITOR_BG))
            .show(ctx, |ui| {
                let tab = &mut self.tabs[index];
                let total = tab
                    .session
                    .exact_line_count()
                    .unwrap_or_else(|| {
                        // Estimates size the overview bar, not the range of an
                        // exact goto. Do not snap a resolved target back into a
                        // stale estimate on the next frame.
                        status.display_line_count().max(
                            tab.first_line
                                .saturating_add(tab.visible_rows)
                                .saturating_add(1),
                        )
                    })
                    .max(1);
                let char_width = ui.fonts_mut(|fonts| {
                    fonts
                        .glyph_width(&FontId::new(14.0, FontFamily::Monospace), 'M')
                        .max(7.0_f32)
                });
                if tab.reveal_caret {
                    if tab.pending_goto.is_none() {
                        tab.reveal_line(tab.caret.line0());
                    }
                    tab.reveal_caret = false;
                }
                // Only horizontal scrolling belongs to egui. Its vertical
                // f32 coordinates cannot represent rows in huge files. The
                // vertical anchor is an integer and all row geometry below
                // is local to this one screen-sized window.
                let area = ScrollArea::horizontal()
                    .id_salt(("editor-scroll", tab.id))
                    .auto_shrink([false, false]);
                // 352-million-line files need 9-digit gutters: scale the
                // gutter to the widest line number instead of letting the
                // numbers paint over the text.
                let number_font_width = char_width * 13.0 / 14.0;
                let gutter_width =
                    (number_font_width * total.to_string().len() as f32 + 24.0).max(GUTTER_WIDTH);
                let bounds = ui.available_rect_before_wrap();
                let track = Rect::from_min_max(
                    egui::pos2(bounds.right() - 16.0, bounds.top()),
                    bounds.right_bottom(),
                );
                let max_first = total.saturating_sub(tab.visible_rows.max(1));
                let track_response = ui.interact(
                    track,
                    Id::new(("editor-line-scroll", tab.id)),
                    Sense::click_and_drag(),
                );
                if track_response.clicked() || track_response.dragged() {
                    if let Some(pointer) = track_response.interact_pointer_pos() {
                        // The overview bar selects a logical line (coarse on
                        // huge files). It never creates an absolute pixel offset.
                        let fraction = ((pointer.y - track.top()) / track.height()).clamp(0.0, 1.0);
                        if tab.pending_goto.take().is_some() {
                            tab.editor_has_focus = true;
                            tab.notice = Notice::info("Go to line cancelled by scrolling");
                        }
                        tab.set_first_line(
                            (f64::from(fraction) * max_first as f64) as usize,
                            total,
                        );
                    }
                }
                let output = area
                    .max_width((bounds.width() - 20.0).max(1.0))
                    .show_viewport(ui, |ui, local_viewport| {
                        // Allocate exactly one screen of local rows. Unlike
                        // show_rows there is no extra item_spacing in the pitch.
                        ui.spacing_mut().item_spacing.y = 0.0;
                        tab.visible_rows =
                            (local_viewport.height() / ROW_HEIGHT).ceil().max(1.0) as usize;
                        if tab.pending_goto.is_none() {
                            tab.set_first_line(tab.first_line, total);
                        }
                        if ui.rect_contains_pointer(ui.clip_rect()) {
                            let wheel = ui.input_mut(|input| {
                                let delta = input.smooth_scroll_delta.y;
                                input.smooth_scroll_delta.y = 0.0;
                                delta
                            });
                            if wheel != 0.0 {
                                if tab.pending_goto.take().is_some() {
                                    tab.editor_has_focus = true;
                                    tab.notice = Notice::info("Go to line cancelled by scrolling");
                                }
                                tab.wheel_remainder -= wheel;
                                let lines = (tab.wheel_remainder / ROW_HEIGHT).trunc();
                                tab.wheel_remainder -= lines * ROW_HEIGHT;
                                tab.move_view_by(lines as isize, total);
                            }
                        }
                        let first_line = tab.first_line;
                        let text_width = (local_viewport.width() - gutter_width - 14.0).max(64.0);
                        let visible_cols = (text_width / char_width) as usize;
                        let viewport = tab.session.read_viewport(
                            ViewportRequest::new(first_line, tab.visible_rows)
                                .with_columns(0, visible_cols),
                        );
                        let rows = viewport.rows();
                        if let Some(target) = tab.pending_goto {
                            if rows
                                .iter()
                                .any(|row| row.line0() == target && row.is_exact())
                            {
                                tab.caret = TextPosition::new(target, 0);
                                tab.desired_col = 0;
                                tab.pending_goto = None;
                                tab.editor_has_focus = true;
                                tab.notice = Notice::info(format!("Line {}", target + 1));
                            } else if tab
                                .session
                                .exact_line_count()
                                .is_some_and(|count| target >= count)
                            {
                                tab.pending_goto = None;
                                tab.editor_has_focus = true;
                                tab.notice = Notice::error(format!(
                                    "Line {} is beyond the end of the file",
                                    target + 1
                                ));
                            }
                        }
                        for offset in 0..tab.visible_rows {
                            let line0 = first_line.saturating_add(offset);
                            if tab
                                .session
                                .exact_line_count()
                                .is_some_and(|count| line0 >= count)
                            {
                                break;
                            }
                            let row = rows
                                .iter()
                                .find(|row| row.line0() == line0 && row.is_exact());
                            let (rect, response) = ui.allocate_exact_size(
                                Vec2::new(ui.available_width().max(1.0), ROW_HEIGHT),
                                Sense::click_and_drag(),
                            );
                            // with_clip_rect already intersects in egui 0.33;
                            // keep the local viewport intersection explicit too.
                            let painter =
                                ui.painter().with_clip_rect(rect.intersect(ui.clip_rect()));
                            let active =
                                row.is_some_and(|rendered| rendered.line0() == tab.caret.line0());
                            if active {
                                painter.rect_filled(rect, 0.0, ACTIVE_LINE);
                                painter.rect_filled(
                                    Rect::from_min_size(rect.min, Vec2::new(2.0, ROW_HEIGHT)),
                                    0.0,
                                    ACCENT,
                                );
                            }
                            let gutter =
                                Rect::from_min_size(rect.min, Vec2::new(gutter_width, ROW_HEIGHT));
                            painter.rect_filled(gutter, 0.0, PANEL);
                            painter.line_segment(
                                [gutter.right_top(), gutter.right_bottom()],
                                Stroke::new(1.0_f32, BORDER),
                            );
                            let text_origin = egui::pos2(gutter.right() + 14.0, rect.center().y);
                            if let Some(selection) = tab.active_selection() {
                                let (anchor, head) = (selection.anchor(), selection.head());
                                let (start, end) = if (anchor.line0(), anchor.col0())
                                    <= (head.line0(), head.col0())
                                {
                                    (anchor, head)
                                } else {
                                    (head, anchor)
                                };
                                if line0 >= start.line0() && line0 <= end.line0() {
                                    let line_len = tab.session.line_len_chars(line0);
                                    let from_col = if line0 == start.line0() {
                                        start.col0()
                                    } else {
                                        0
                                    };
                                    let to_col = if line0 == end.line0() {
                                        end.col0().min(line_len)
                                    } else {
                                        line_len
                                    };
                                    let x0 = text_origin.x + from_col as f32 * char_width;
                                    let x1 = (text_origin.x
                                        + to_col.max(from_col) as f32 * char_width)
                                        .max(x0 + 1.0);
                                    painter.rect_filled(
                                        Rect::from_min_max(
                                            egui::pos2(x0, rect.min.y + 2.0),
                                            egui::pos2(x1, rect.max.y - 2.0),
                                        ),
                                        0.0,
                                        Color32::from_rgba_unmultiplied(70, 120, 210, 110),
                                    );
                                }
                            }
                            let number_color = if active {
                                TEXT
                            } else if row.is_some_and(|rendered| rendered.is_exact()) {
                                MUTED
                            } else {
                                WARNING
                            };
                            painter.text(
                                gutter.right_center() - Vec2::new(12.0, 0.0),
                                egui::Align2::RIGHT_CENTER,
                                row.map(|rendered| rendered.line_number().to_string())
                                    .unwrap_or_else(|| line0.saturating_add(1).to_string()),
                                FontId::monospace(13.0),
                                number_color,
                            );
                            let text = row
                                .map(|rendered| rendered.text())
                                .unwrap_or("Resolving exact line…");
                            painter.text(
                                text_origin,
                                egui::Align2::LEFT_CENTER,
                                if text.is_empty() { " " } else { text },
                                FontId::monospace(14.0),
                                TEXT,
                            );
                            if let Some(found) = tab
                                .search_match
                                .filter(|found| row.is_some() && found.start().line0() == line0)
                            {
                                let start = found.start().col0() as f32 * char_width;
                                let width = found.len_chars().max(1) as f32 * char_width;
                                let highlight = Rect::from_min_size(
                                    egui::pos2(text_origin.x + start, rect.min.y + 3.0),
                                    Vec2::new(width, ROW_HEIGHT - 6.0),
                                );
                                painter.rect_filled(
                                    highlight,
                                    CornerRadius::same(2),
                                    Color32::from_rgba_unmultiplied(104, 160, 255, 56),
                                );
                                painter.rect_stroke(
                                    highlight,
                                    CornerRadius::same(2),
                                    Stroke::new(1.0_f32, ACCENT),
                                    StrokeKind::Inside,
                                );
                            }
                            if active && tab.editor_has_focus {
                                let x = text_origin.x + tab.caret.col0() as f32 * char_width;
                                painter.line_segment(
                                    [
                                        egui::pos2(x, rect.min.y + 3.0),
                                        egui::pos2(x, rect.max.y - 3.0),
                                    ],
                                    Stroke::new(1.5_f32, ACCENT),
                                );
                            }
                            if response.drag_started() && row.is_some() {
                                if let Some(pointer) = response.interact_pointer_pos() {
                                    let col = ((pointer.x - text_origin.x).max(0.0) / char_width)
                                        .round()
                                        as usize;
                                    let col = col.min(tab.session.line_len_chars(line0));
                                    tab.caret = TextPosition::new(line0, col);
                                    tab.desired_col = col;
                                    tab.selection_anchor = Some(tab.caret);
                                    tab.editor_has_focus = true;
                                }
                            } else if response.dragged() {
                                if let Some(pointer) = response.interact_pointer_pos() {
                                    // The pressed row keeps receiving drag events
                                    // after the pointer leaves it, so the head row
                                    // is derived from the pointer offset relative
                                    // to this row — in integer line space.
                                    let delta_rows =
                                        ((pointer.y - rect.min.y) / ROW_HEIGHT).floor() as isize;
                                    let line = if delta_rows >= 0 {
                                        line0.saturating_add(delta_rows as usize)
                                    } else {
                                        line0.saturating_sub((-delta_rows) as usize)
                                    }
                                    .min(total.saturating_sub(1));
                                    let col = ((pointer.x - text_origin.x).max(0.0) / char_width)
                                        .round()
                                        as usize;
                                    let col = col.min(tab.session.line_len_chars(line));
                                    tab.caret = TextPosition::new(line, col);
                                    tab.desired_col = col;
                                }
                            } else if response.clicked() && row.is_some() {
                                if let Some(pointer) = response.interact_pointer_pos() {
                                    let col = ((pointer.x - text_origin.x).max(0.0) / char_width)
                                        .round()
                                        as usize;
                                    let position = TextPosition::new(
                                        line0,
                                        col.min(tab.session.line_len_chars(line0)),
                                    );
                                    if ui.input(|input| input.modifiers.shift) {
                                        let before = tab.caret;
                                        tab.caret = position;
                                        tab.desired_col = position.col0();
                                        tab.selection_anchor.get_or_insert(before);
                                    } else {
                                        tab.selection_anchor = None;
                                        if self.search_open {
                                            // Clicking the editor while the search bar
                                            // is open returns focus to the document:
                                            // close the bar and drop the highlight so
                                            // keystrokes cannot keep landing in the
                                            // search field while the caret visibly
                                            // moved.
                                            self.search_open = false;
                                            tab.search_match = None;
                                        }
                                        tab.set_caret(position);
                                    }
                                }
                            }
                        }
                    });
                let painter = ui.painter().with_clip_rect(bounds);
                painter.rect_filled(track, CornerRadius::same(4), PANEL);
                let thumb_height = (track.height()
                    * (tab.visible_rows as f64 / total as f64) as f32)
                    .clamp(18.0, track.height().max(18.0));
                let fraction = if max_first == 0 {
                    0.0
                } else {
                    tab.first_line as f64 / max_first as f64
                };
                let top = track.top() + fraction as f32 * (track.height() - thumb_height).max(0.0);
                painter.rect_filled(
                    Rect::from_min_size(
                        egui::pos2(track.left(), top),
                        Vec2::new(track.width(), thumb_height),
                    ),
                    CornerRadius::same(4),
                    MUTED,
                );
                // The horizontal scroller may reserve a bar, but its content
                // and clip rectangles must always remain screen-local.
                debug_assert!(output.content_size.y < bounds.height() + ROW_HEIGHT * 2.0);
            });
    }

    fn status_bar(&mut self, ctx: &egui::Context) {
        let tab = self.active_tab();
        let status = tab.session.status();
        egui::TopBottomPanel::bottom("statusbar")
            .exact_height(28.0)
            .frame(
                egui::Frame::new()
                    .fill(PANEL)
                    .inner_margin(Margin::symmetric(10, 5)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    let color = match tab.notice.kind {
                        NoticeKind::Info => MUTED,
                        NoticeKind::Error => ERROR,
                    };
                    if status.is_busy() || status.is_line_count_pending() {
                        ui.spinner();
                    }
                    ui.label(RichText::new(&tab.notice.text).small().color(color));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(
                            RichText::new(format!(
                                "{}:{}",
                                tab.caret.line0() + 1,
                                tab.caret.col0() + 1
                            ))
                            .monospace()
                            .small()
                            .color(MUTED),
                        );
                        ui.separator();
                        ui.label(RichText::new(status.encoding().name()).small().color(MUTED));
                        ui.separator();
                        ui.label(
                            RichText::new(status.line_ending().as_str())
                                .small()
                                .color(MUTED),
                        );
                        ui.separator();
                        ui.label(
                            RichText::new(status.backing().as_str())
                                .small()
                                .color(MUTED),
                        );
                    });
                });
            });
    }

    fn close_modal(&mut self, ctx: &egui::Context) {
        let Some(id) = self.close_prompt else {
            return;
        };
        let Some(index) = self.tab_index(id) else {
            self.close_prompt = None;
            return;
        };
        let title = self.tabs[index].title();
        let mut decision = None;
        let response = Modal::new(Id::new("close-dirty-tab"))
            .frame(
                egui::Frame::popup(&ctx.style())
                    .fill(PANEL)
                    .corner_radius(CornerRadius::same(12))
                    .inner_margin(Margin::same(18)),
            )
            .show(ctx, |ui| {
                ui.set_min_width(390.0);
                ui.heading("Unsaved changes");
                ui.add_space(8.0);
                ui.label(format!("Save changes to {title} before closing?"));
                ui.add_space(16.0);
                ui.horizontal(|ui| {
                    if ui.button("Save").clicked() {
                        decision = Some(1);
                    }
                    if ui.button("Don't Save").clicked() {
                        decision = Some(2);
                    }
                    if ui.button("Cancel").clicked() {
                        decision = Some(3);
                    }
                });
            });
        if decision.is_none() && (response.should_close() || response.backdrop_response.clicked()) {
            // Escape or a backdrop click behaves like Cancel: abort a
            // Close Others/Close All batch and keep the tab open.
            decision = Some(3);
        }
        match decision {
            Some(1) => {
                self.close_prompt = None;
                self.save_tab(id, true);
            }
            Some(2) => {
                self.close_prompt = None;
                self.remove_tab(id);
                self.advance_close_queue();
            }
            Some(3) => {
                self.close_prompt = None;
                self.close_queue.clear();
            }
            _ => {}
        }
    }

    fn rename_modal(&mut self, ctx: &egui::Context) {
        if self.rename_dialog.is_none() {
            return;
        }
        let mut commit = false;
        let mut cancel = false;
        let response = Modal::new(Id::new("rename-file"))
            .frame(
                egui::Frame::popup(&ctx.style())
                    .fill(PANEL)
                    .corner_radius(CornerRadius::same(12))
                    .inner_margin(Margin::same(18)),
            )
            .show(ctx, |ui| {
                ui.set_min_width(420.0);
                ui.heading("Rename file");
                ui.add_space(8.0);
                let dialog = self.rename_dialog.as_mut().expect("dialog exists");
                let field = ui.add(
                    TextEdit::singleline(&mut dialog.name)
                        .id(Id::new("qem-rename-field"))
                        .desired_width(380.0)
                        .font(FontId::monospace(14.0)),
                );
                field.request_focus();
                if let Some(error) = &dialog.error {
                    ui.label(RichText::new(error).color(ERROR));
                }
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    if ui.button("Rename").clicked()
                        || (field.has_focus() && ui.input(|input| input.key_pressed(Key::Enter)))
                    {
                        commit = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
            });
        if !commit && (response.should_close() || response.backdrop_response.clicked()) {
            // Escape or a backdrop click cancels the rename.
            cancel = true;
        }
        if commit {
            self.commit_rename();
        } else if cancel {
            self.rename_dialog = None;
        }
    }

    fn begin_goto_line(&mut self) {
        if self.active_tab().session.is_busy() {
            self.active_tab_mut().notice = Notice::info("Wait for the current file operation");
            return;
        }
        let line = self.active_tab().caret.line0() + 1;
        self.active_tab_mut().editor_has_focus = false;
        self.goto_dialog = Some(line.to_string());
    }

    /// Applies the goto dialog. Returns `false` when the input was invalid
    /// and the dialog must stay open for correction.
    fn commit_goto_line(&mut self) -> bool {
        let Some(raw) = self
            .goto_dialog
            .as_ref()
            .map(|dialog| dialog.trim().to_owned())
        else {
            return true;
        };
        let Ok(line_number) = raw.parse::<usize>() else {
            self.active_tab_mut().notice = Notice::error(format!("Not a line number: {raw}"));
            return false;
        };
        if line_number == 0 {
            self.active_tab_mut().notice = Notice::error("Line numbers start at 1");
            return false;
        }
        let line0 = line_number - 1;
        if self
            .active_tab()
            .session
            .exact_line_count()
            .is_some_and(|count| line0 >= count)
        {
            self.active_tab_mut().notice =
                Notice::error(format!("Line {line_number} is beyond the end of the file"));
            return false;
        }
        let tab = self.active_tab_mut();
        // A line count estimate is not an upper bound. Keep the user's exact
        // request, and do not use edit-caret clamping to navigate the view.
        tab.first_line = line0;
        tab.pending_goto = Some(line0);
        tab.wheel_remainder = 0.0;
        tab.reveal_caret = false;
        tab.collapse_selection();
        tab.editor_has_focus = false;
        tab.notice = Notice::info(format!("Resolving exact line {line_number}…"));
        self.goto_dialog = None;
        true
    }

    fn goto_modal(&mut self, ctx: &egui::Context) {
        if self.goto_dialog.is_none() {
            return;
        }
        let mut commit = false;
        let mut cancel = false;
        let response = Modal::new(Id::new("goto-line"))
            .frame(
                egui::Frame::popup(&ctx.style())
                    .fill(PANEL)
                    .corner_radius(CornerRadius::same(12))
                    .inner_margin(Margin::same(18)),
            )
            .show(ctx, |ui| {
                ui.set_min_width(360.0);
                ui.heading("Go to line");
                ui.add_space(8.0);
                let dialog = self.goto_dialog.as_mut().expect("goto dialog exists");
                let field = ui.add(
                    TextEdit::singleline(dialog)
                        .id(Id::new("qem-goto-field"))
                        .desired_width(220.0)
                        .font(FontId::monospace(14.0)),
                );
                field.request_focus();
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    if ui.button("Go").clicked()
                        || (field.has_focus() && ui.input(|input| input.key_pressed(Key::Enter)))
                    {
                        commit = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
            });
        if !commit && (response.should_close() || response.backdrop_response.clicked()) {
            cancel = true;
        }
        if commit && self.commit_goto_line() {
            return;
        }
        if cancel {
            self.goto_dialog = None;
            self.active_tab_mut().editor_has_focus = true;
        }
    }
}

impl eframe::App for EditorApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // QEM_DEBUG_FRAMES=1 logs per-frame phase timings to stderr; used to
        // diagnose multi-second stalls of the UI thread on huge files.
        let debug_frames = DEBUG_FRAMES
            .get_or_init(|| std::env::var_os("QEM_DEBUG_FRAMES").is_some_and(|v| v != "0"));
        let frame = FRAME_COUNTER.fetch_add(1, Ordering::Relaxed);
        let started = Instant::now();
        let mark = |name: &str| {
            if *debug_frames {
                eprintln!(
                    "[F{frame} {name}] {:.1} ms",
                    started.elapsed().as_secs_f64() * 1e3
                );
            }
        };
        if *debug_frames {
            eprintln!("[F{frame} begin]");
        }
        self.pump();
        mark("pump");
        self.shortcuts(ctx);
        mark("shortcuts");
        self.editor_input(ctx);
        mark("editor_input");
        self.top_bar(ctx);
        mark("top_bar");
        self.tab_bar(ctx);
        mark("tab_bar");
        self.search_bar(ctx);
        mark("search_bar");
        self.status_bar(ctx);
        mark("status_bar");
        self.editor(ctx);
        mark("editor");
        self.close_modal(ctx);
        mark("close_modal");
        self.rename_modal(ctx);
        self.goto_modal(ctx);
        mark("modals");
        if *debug_frames {
            // The segment after update() — egui tessellation plus GPU submit —
            // is outside this log; a gap between "[F n end]" and "[F n+1 begin]"
            // pins a stall to rendering or the event wait.
            eprintln!(
                "[F{frame} end] {:.1} ms",
                started.elapsed().as_secs_f64() * 1e3
            );
        }

        if self
            .tabs
            .iter()
            .any(|tab| tab.session.is_busy() || tab.session.is_indexing())
        {
            ctx.request_repaint_after(Duration::from_millis(16));
        }
    }
}

static FRAME_COUNTER: AtomicU64 = AtomicU64::new(0);
static DEBUG_FRAMES: OnceLock<bool> = OnceLock::new();

fn toolbar_button(ui: &mut egui::Ui, label: &str, enabled: bool) -> egui::Response {
    ui.add_enabled(enabled, egui::Button::new(RichText::new(label).size(13.0)))
}

fn close_icon_button(ui: &mut egui::Ui, active: bool) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(20.0), Sense::click());
    if response.hovered() {
        ui.painter()
            .rect_filled(rect, CornerRadius::same(7), Color32::from_rgb(74, 46, 54));
    }
    let color = if response.hovered() {
        ERROR
    } else if active {
        TEXT
    } else {
        MUTED
    };
    let center = rect.center();
    let radius = 4.0;
    ui.painter().line_segment(
        [
            egui::pos2(center.x - radius, center.y - radius),
            egui::pos2(center.x + radius, center.y + radius),
        ],
        Stroke::new(1.5_f32, color),
    );
    ui.painter().line_segment(
        [
            egui::pos2(center.x + radius, center.y - radius),
            egui::pos2(center.x - radius, center.y + radius),
        ],
        Stroke::new(1.5_f32, color),
    );
    response.on_hover_text("Close tab")
}

fn encoding_choices() -> [&'static str; 8] {
    [
        "UTF-8",
        "UTF-16LE",
        "UTF-16BE",
        "windows-1251",
        "windows-1252",
        "Shift_JIS",
        "GB18030",
        "EUC-KR",
    ]
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

#[cfg(test)]
mod tests {
    use super::*;

    fn test_app() -> EditorApp {
        let mut app = EditorApp::new(None);
        let text = (1..=200)
            .map(|line| format!("row-{line:03}\n"))
            .collect::<String>();
        app.active_tab_mut()
            .session
            .try_insert(TextPosition::new(0, 0), &text)
            .unwrap();
        app
    }

    fn frame(
        ctx: &egui::Context,
        app: &mut EditorApp,
        events: Vec<egui::Event>,
    ) -> egui::FullOutput {
        ctx.run(
            egui::RawInput {
                screen_rect: Some(Rect::from_min_size(
                    egui::Pos2::ZERO,
                    Vec2::new(800.0, 400.0),
                )),
                events,
                ..Default::default()
            },
            |ctx| app.editor(ctx),
        )
    }

    fn painted_rows(output: &egui::FullOutput) -> Vec<(String, f32, Rect)> {
        output
            .shapes
            .iter()
            .filter_map(|shape| {
                if let egui::epaint::Shape::Text(text) = &shape.shape {
                    let content = text.galley.job.text.clone();
                    if content.starts_with("row-") {
                        return Some((content, text.pos.y, shape.clip_rect));
                    }
                }
                None
            })
            .collect()
    }

    #[test]
    fn integer_window_preserves_adjacent_lines_in_huge_files() {
        let mut tab = UiTab::new(1, 1);
        tab.visible_rows = 17;
        tab.first_line = 352_000_000;
        tab.move_view_by(1, 500_000_000);
        assert_eq!(tab.first_line, 352_000_001);
        tab.move_view_by(-1, 500_000_000);
        assert_eq!(tab.first_line, 352_000_000);
        tab.reveal_line(352_000_020);
        assert_eq!(tab.first_line, 352_000_004);
        tab.move_view_by(isize::MAX, 500_000_000);
        assert_eq!(tab.first_line, 500_000_000 - 17);
        tab.move_view_by(isize::MIN, 500_000_000);
        assert_eq!(tab.first_line, 0);
    }

    #[test]
    fn headless_goto_resolves_requested_row_without_spacing_drift() {
        let ctx = egui::Context::default();
        install_theme(&ctx);
        let mut app = test_app();
        app.goto_dialog = Some("101".to_owned());
        assert!(app.commit_goto_line());
        assert_eq!(app.active_tab().pending_goto, Some(100));
        assert_eq!(app.active_tab().caret.line0(), 0);
        let output = frame(&ctx, &mut app, vec![]);
        assert_eq!(app.active_tab().caret.line0(), 100);
        assert_eq!(app.active_tab().pending_goto, None);
        assert_eq!(app.active_tab().first_line, 100);
        let rows = painted_rows(&output);
        assert_eq!(rows.first().unwrap().0, "row-101");
        assert!(rows.len() >= 10);
        for pair in rows.windows(2) {
            assert!((pair[1].1 - pair[0].1 - ROW_HEIGHT).abs() < 0.1);
        }
        for (_, _, clip) in &rows {
            assert!(clip.top() >= 0.0 && clip.bottom() <= 400.0);
            assert!(clip.left() >= 0.0 && clip.right() < 800.0);
            assert!(clip.height() <= ROW_HEIGHT + 0.1);
        }
        frame(&ctx, &mut app, vec![]);
        assert_eq!(app.active_tab().first_line, 100);
    }

    #[test]
    fn headless_wheel_keeps_rows_contiguous_and_round_trips() {
        let ctx = egui::Context::default();
        install_theme(&ctx);
        let mut app = test_app();
        app.active_tab_mut().first_line = 50;
        frame(&ctx, &mut app, vec![]);
        let mut events = vec![egui::Event::PointerMoved(egui::pos2(200.0, 100.0))];
        for delta in [-ROW_HEIGHT, -ROW_HEIGHT, ROW_HEIGHT, ROW_HEIGHT] {
            events.push(egui::Event::MouseWheel {
                unit: egui::MouseWheelUnit::Point,
                delta: Vec2::new(0.0, delta),
                modifiers: egui::Modifiers::default(),
            });
            let output = frame(&ctx, &mut app, std::mem::take(&mut events));
            let rows = painted_rows(&output);
            for pair in rows.windows(2) {
                let first = pair[0].0[4..].parse::<usize>().unwrap();
                let second = pair[1].0[4..].parse::<usize>().unwrap();
                assert_eq!(second, first + 1);
                assert!((pair[1].1 - pair[0].1 - ROW_HEIGHT).abs() < 0.1);
            }
        }
        assert_eq!(app.active_tab().first_line, 50);
    }

    #[test]
    fn goto_rejects_invalid_or_out_of_range_instead_of_clamping() {
        let mut app = test_app();
        for text in ["0", "invalid", "999999999", "18446744073709551616"] {
            app.goto_dialog = Some(text.to_owned());
            assert!(!app.commit_goto_line(), "accepted {text}");
            assert!(app.goto_dialog.is_some());
            assert_eq!(app.active_tab().caret.line0(), 0);
            assert_eq!(app.active_tab().first_line, 0);
        }
    }

    #[test]
    fn click_caret_does_not_silently_clamp_to_an_unrelated_line() {
        let mut tab = UiTab::new(1, 1);
        tab.set_caret(TextPosition::new(352_000_000, 0));
        assert_eq!(tab.caret.line0(), 352_000_000);
        assert_ne!(tab.session.clamp_position(tab.caret), tab.caret);
    }
}
