use super::*;

/// Backend-first document session wrapper with async open/save helpers and no
/// GUI-level cursor or widget assumptions.
#[derive(Debug)]
pub struct DocumentSession {
    core: SessionCore,
}

impl Default for DocumentSession {
    fn default() -> Self {
        Self::new()
    }
}

impl DocumentSession {
    /// Creates a new empty document session.
    pub fn new() -> Self {
        Self {
            core: SessionCore::new(),
        }
    }

    /// Returns the session generation counter.
    pub fn generation(&self) -> u64 {
        self.core.generation()
    }

    /// Returns `true` while a background save is in progress.
    pub fn is_saving(&self) -> bool {
        self.core.is_saving()
    }

    /// Returns `true` while a background load is in progress.
    pub fn is_loading(&self) -> bool {
        self.core.is_loading()
    }

    /// Returns `true` while any background worker is active.
    pub fn is_busy(&self) -> bool {
        self.core.is_busy()
    }

    /// Returns `true` while document-local indexing is still running.
    pub fn is_indexing(&self) -> bool {
        self.core.document().is_indexing()
    }

    /// Returns document indexing progress as `(indexed_bytes, total_bytes)`.
    ///
    /// Prefer [`DocumentSession::indexing_state`] in new code when you want a
    /// typed progress value.
    #[doc(hidden)]
    #[deprecated(
        since = "0.3.0",
        note = "use indexing_state() for typed progress instead"
    )]
    pub fn indexing_progress(&self) -> Option<(usize, usize)> {
        self.core.indexing_progress()
    }

    /// Returns typed indexing progress while background indexing is active.
    pub fn indexing_state(&self) -> Option<ByteProgress> {
        self.core.indexing_state()
    }

    /// Returns typed background-load progress.
    ///
    /// This covers the asynchronous open path itself. Once the document is
    /// ready, follow `indexing_state()` for any continued background line
    /// indexing.
    pub fn loading_state(&self) -> Option<FileProgress> {
        self.core.loading_state()
    }

    /// Returns the current loading phase when a background open is active.
    pub fn loading_phase(&self) -> Option<LoadPhase> {
        self.core.loading_phase()
    }

    /// Returns background-load progress.
    ///
    /// Prefer [`DocumentSession::loading_state`] in new code when you want a
    /// typed progress value.
    #[doc(hidden)]
    #[deprecated(
        since = "0.3.0",
        note = "use loading_state() for typed progress instead"
    )]
    pub fn loading_progress(&self) -> Option<(u64, u64, PathBuf)> {
        self.core.loading_progress()
    }

    /// Polls the background-load state.
    pub fn poll_load_job(&mut self) -> Option<Result<(), DocumentError>> {
        self.core.poll_load_job()
    }

    /// Polls the background-save state.
    pub fn poll_save_job(&mut self) -> Option<Result<(), DocumentError>> {
        self.core.poll_save_job()
    }

    /// Polls whichever background job is active.
    pub fn poll_background_job(&mut self) -> Option<Result<(), DocumentError>> {
        self.core.poll_background_job()
    }

    /// Returns immutable access to the current document.
    pub fn document(&self) -> &Document {
        self.core.document()
    }

    /// Returns mutable access to the current document.
    ///
    /// This is an escape hatch for callers that fully coordinate their own
    /// background job lifecycle. The typed edit helpers on
    /// [`DocumentSession`] reject edits while `is_busy()` is `true`; mutating
    /// the raw [`Document`] through this reference during a background
    /// open/save marks the in-flight worker result as stale, so the next poll
    /// surfaces an error instead of applying an outdated load/save result over
    /// the current document. If a `close_file()` was deferred while that job
    /// was active, this raw mutation also cancels the deferred close because
    /// the session state has changed again.
    pub fn document_mut(&mut self) -> &mut Document {
        self.core.document_mut()
    }

    /// Returns the full document text as a `String`.
    ///
    /// This materializes the entire current document through
    /// [`Document::text_lossy`]. Prefer `read_viewport(...)` or `read_text(...)`
    /// when a frontend only needs a visible window or a bounded selection.
    pub fn text(&self) -> String {
        self.core.document().text_lossy()
    }

    /// Returns the current document path, if one is set.
    pub fn current_path(&self) -> Option<&Path> {
        self.core.current_path()
    }

    /// Returns `true` if the document has unsaved changes.
    pub fn is_dirty(&self) -> bool {
        self.core.is_dirty()
    }

    /// Returns the current document line count with exact/estimated semantics.
    pub fn line_count(&self) -> LineCount {
        self.core.document().line_count()
    }

    /// Returns the exact document line count when it is known.
    pub fn exact_line_count(&self) -> Option<usize> {
        self.core.document().exact_line_count()
    }

    /// Returns the current best-effort line count for viewport sizing and scrolling.
    pub fn display_line_count(&self) -> usize {
        self.core.document().display_line_count()
    }

    /// Returns `true` when the current line count is exact.
    pub fn is_line_count_exact(&self) -> bool {
        self.core.document().is_line_count_exact()
    }

    /// Returns the current document length in bytes.
    pub fn file_len(&self) -> usize {
        self.core.document().file_len()
    }

    /// Returns the currently detected line ending style.
    pub fn line_ending(&self) -> LineEnding {
        self.core.document().line_ending()
    }

    /// Returns the visible line length in text columns, excluding line endings.
    pub fn line_len_chars(&self, line0: usize) -> usize {
        self.core.document().line_len_chars(line0)
    }

    /// Clamps a typed position into the currently known document bounds.
    pub fn clamp_position(&self, position: TextPosition) -> TextPosition {
        self.core.document().clamp_position(position)
    }

    /// Returns the typed document position for a full-text character index.
    pub fn position_for_char_index(&self, char_index: usize) -> TextPosition {
        self.core.document().position_for_char_index(char_index)
    }

    /// Returns the full-text character index for a typed document position.
    pub fn char_index_for_position(&self, position: TextPosition) -> usize {
        self.core.document().char_index_for_position(position)
    }

    /// Returns the ordered pair of two clamped positions.
    pub fn ordered_positions(
        &self,
        first: TextPosition,
        second: TextPosition,
    ) -> (TextPosition, TextPosition) {
        self.core.document().ordered_positions(first, second)
    }

    /// Clamps a selection into the currently known document bounds.
    pub fn clamp_selection(&self, selection: TextSelection) -> TextSelection {
        self.core.document().clamp_selection(selection)
    }

    /// Returns the number of edit text units between two positions.
    pub fn text_units_between(&self, start: TextPosition, end: TextPosition) -> usize {
        self.core.document().text_units_between(start, end)
    }

    /// Builds a typed edit range between two positions.
    pub fn text_range_between(&self, start: TextPosition, end: TextPosition) -> TextRange {
        self.core.document().text_range_between(start, end)
    }

    /// Builds a typed edit range from an anchor/head selection.
    pub fn text_range_for_selection(&self, selection: TextSelection) -> TextRange {
        self.core.document().text_range_for_selection(selection)
    }

    /// Returns whether the requested position is editable and whether it would
    /// require a backend promotion first.
    pub fn edit_capability_at(&self, position: TextPosition) -> EditCapability {
        self.core.document().edit_capability_at(position)
    }

    /// Returns editability for a typed edit range.
    pub fn edit_capability_for_range(&self, range: TextRange) -> EditCapability {
        self.core.document().edit_capability_for_range(range)
    }

    /// Returns editability for an anchor/head selection.
    pub fn edit_capability_for_selection(&self, selection: TextSelection) -> EditCapability {
        self.core
            .document()
            .edit_capability_for_selection(selection)
    }

    /// Reads a typed text range from the current document.
    pub fn read_text(&self, range: TextRange) -> TextSlice {
        self.core.document().read_text(range)
    }

    /// Reads the current selection as a typed text slice.
    pub fn read_selection(&self, selection: TextSelection) -> TextSlice {
        self.core.document().read_selection(selection)
    }

    /// Finds the next literal match starting at `from`.
    pub fn find_next(&self, needle: &str, from: TextPosition) -> Option<SearchMatch> {
        self.core.document().find_next(needle, from)
    }

    /// Finds the next literal match using a reusable compiled query.
    pub fn find_next_query(
        &self,
        query: &LiteralSearchQuery,
        from: TextPosition,
    ) -> Option<SearchMatch> {
        self.core.document().find_next_query(query, from)
    }

    /// Finds the previous literal match whose end is at or before `before`.
    pub fn find_prev(&self, needle: &str, before: TextPosition) -> Option<SearchMatch> {
        self.core.document().find_prev(needle, before)
    }

    /// Finds the previous literal match using a reusable compiled query.
    pub fn find_prev_query(
        &self,
        query: &LiteralSearchQuery,
        before: TextPosition,
    ) -> Option<SearchMatch> {
        self.core.document().find_prev_query(query, before)
    }

    /// Finds the first literal match fully contained within `range`.
    pub fn find_next_in_range(&self, needle: &str, range: TextRange) -> Option<SearchMatch> {
        self.core.document().find_next_in_range(needle, range)
    }

    /// Finds the last literal match fully contained within `range`.
    pub fn find_prev_in_range(&self, needle: &str, range: TextRange) -> Option<SearchMatch> {
        self.core.document().find_prev_in_range(needle, range)
    }

    /// Finds the first query match fully contained within `range`.
    pub fn find_next_query_in_range(
        &self,
        query: &LiteralSearchQuery,
        range: TextRange,
    ) -> Option<SearchMatch> {
        self.core.document().find_next_query_in_range(query, range)
    }

    /// Finds the last query match fully contained within `range`.
    pub fn find_prev_query_in_range(
        &self,
        query: &LiteralSearchQuery,
        range: TextRange,
    ) -> Option<SearchMatch> {
        self.core.document().find_prev_query_in_range(query, range)
    }

    /// Applies a typed insert directly through the session.
    pub fn try_insert(
        &mut self,
        position: TextPosition,
        text: &str,
    ) -> Result<TextPosition, DocumentError> {
        self.core.ensure_idle_for_edit()?;
        self.core.document_mut().try_insert(position, text)
    }

    /// Applies a typed replacement directly through the session.
    pub fn try_replace(
        &mut self,
        range: TextRange,
        text: &str,
    ) -> Result<TextPosition, DocumentError> {
        self.core.ensure_idle_for_edit()?;
        self.core.document_mut().try_replace(range, text)
    }

    /// Replaces the current selection and returns the resulting caret position.
    pub fn try_replace_selection(
        &mut self,
        selection: TextSelection,
        text: &str,
    ) -> Result<TextPosition, DocumentError> {
        self.core.ensure_idle_for_edit()?;
        self.core
            .document_mut()
            .try_replace_selection(selection, text)
    }

    /// Applies a typed backspace directly through the session.
    pub fn try_backspace(&mut self, position: TextPosition) -> Result<EditResult, DocumentError> {
        self.core.ensure_idle_for_edit()?;
        self.core.document_mut().try_backspace(position)
    }

    /// Applies a backspace command to an anchor/head selection.
    pub fn try_backspace_selection(
        &mut self,
        selection: TextSelection,
    ) -> Result<EditResult, DocumentError> {
        self.core.ensure_idle_for_edit()?;
        self.core.document_mut().try_backspace_selection(selection)
    }

    /// Deletes the text unit at the cursor and keeps the resulting caret position.
    pub fn try_delete_forward(
        &mut self,
        position: TextPosition,
    ) -> Result<EditResult, DocumentError> {
        self.core.ensure_idle_for_edit()?;
        self.core.document_mut().try_delete_forward(position)
    }

    /// Applies a forward-delete command to an anchor/head selection.
    pub fn try_delete_forward_selection(
        &mut self,
        selection: TextSelection,
    ) -> Result<EditResult, DocumentError> {
        self.core.ensure_idle_for_edit()?;
        self.core
            .document_mut()
            .try_delete_forward_selection(selection)
    }

    /// Deletes the current selection and returns the resulting caret position.
    pub fn try_delete_selection(
        &mut self,
        selection: TextSelection,
    ) -> Result<EditResult, DocumentError> {
        self.core.ensure_idle_for_edit()?;
        self.core.document_mut().try_delete_selection(selection)
    }

    /// Cuts the current selection and returns the removed text together with the edit result.
    pub fn try_cut_selection(
        &mut self,
        selection: TextSelection,
    ) -> Result<CutResult, DocumentError> {
        self.core.ensure_idle_for_edit()?;
        self.core.document_mut().try_cut_selection(selection)
    }

    /// Opens a file synchronously.
    pub fn open_file(&mut self, path: PathBuf) -> Result<(), DocumentError> {
        self.core.open_file(path)
    }

    /// Starts opening a file on a background worker.
    pub fn open_file_async(&mut self, path: PathBuf) -> Result<(), DocumentError> {
        self.core.open_file_async(path)
    }

    /// Closes the current document and replaces it with an empty one.
    ///
    /// If a background open/save is still running, the close is deferred until
    /// that job finishes so the session keeps tracking the active worker
    /// result. Deferred closes after background saves are only applied when
    /// the save succeeds; failed saves leave the dirty document open.
    pub fn close_file(&mut self) {
        self.core.close_file();
    }

    /// Clears a deferred dirty flag scheduled after a clean document open.
    pub fn after_document_frame(&mut self) {
        self.core.after_document_frame();
    }

    /// Cancels the deferred dirty-flag clear scheduled after a clean open.
    pub fn cancel_clear_dirty_after_open(&mut self) {
        self.core.cancel_clear_dirty_after_open();
    }

    /// Saves the document synchronously to its current path.
    pub fn save(&mut self) -> Result<(), SaveError> {
        self.core.save()
    }

    /// Saves the document synchronously to a new path.
    pub fn save_as(&mut self, path: PathBuf) -> Result<(), DocumentError> {
        self.core.save_as(path)
    }

    /// Sets the document path without saving.
    ///
    /// When used while a background open/save is active, the path change is
    /// applied immediately to the current document and the in-flight worker
    /// result is marked stale so the next poll returns an error instead of
    /// applying outdated state. If a `close_file()` was deferred while that job
    /// was active, changing the path also cancels that deferred close.
    pub fn set_path(&mut self, path: PathBuf) {
        self.core.set_path(path);
    }

    /// Returns typed background-save progress.
    pub fn save_state(&self) -> Option<FileProgress> {
        self.core.save_state()
    }

    /// Returns background-save progress.
    ///
    /// Prefer [`DocumentSession::save_state`] in new code when you want a
    /// typed progress value.
    #[doc(hidden)]
    #[deprecated(since = "0.3.0", note = "use save_state() for typed progress instead")]
    pub fn save_progress(&self) -> Option<(u64, u64, PathBuf)> {
        self.core.save_progress()
    }

    /// Starts a background save to the current path.
    ///
    /// The write itself runs on a worker thread, but save preparation still
    /// snapshots the current document before that worker starts. For large
    /// edited buffers, the call itself can therefore take noticeable time.
    pub fn save_async(&mut self) -> Result<bool, SaveError> {
        self.core.save_async()
    }

    /// Starts a background save to a new path.
    ///
    /// The write itself runs on a worker thread, but save preparation still
    /// snapshots the current document before that worker starts. For large
    /// edited buffers, the call itself can therefore take noticeable time.
    pub fn save_as_async(&mut self, path: PathBuf) -> Result<bool, DocumentError> {
        self.core.save_as_async(path)
    }

    /// Returns the current background activity for the session.
    pub fn background_activity(&self) -> BackgroundActivity {
        self.core.background_activity()
    }

    /// Returns the most recent background open/save problem, if one is being retained.
    pub fn background_issue(&self) -> Option<&BackgroundIssue> {
        self.core.background_issue()
    }

    /// Takes and clears the most recent retained background open/save problem.
    ///
    /// This is useful for frontends that want to acknowledge a background error
    /// once it has been surfaced to the user instead of keeping it visible until
    /// a later successful operation clears it implicitly.
    pub fn take_background_issue(&mut self) -> Option<BackgroundIssue> {
        self.core.take_background_issue()
    }

    /// Returns `true` when `close_file()` has been requested and is waiting for
    /// the active background open/save to finish.
    pub fn close_pending(&self) -> bool {
        self.core.close_pending()
    }

    /// Returns a frontend-friendly snapshot of the current session state.
    pub fn status(&self) -> DocumentSessionStatus {
        self.core.status()
    }

    /// Reads a visible viewport directly from the current document.
    pub fn read_viewport(&self, request: ViewportRequest) -> Viewport {
        self.core.read_viewport(request)
    }
}
