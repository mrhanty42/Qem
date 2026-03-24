use super::*;
use std::sync::Arc;

/// Cursor position in document coordinates, using 1-based indexing.
///
/// The column uses the same document text-unit semantics as [`TextPosition`]:
/// for UTF-8 text this means Unicode scalar values, not grapheme clusters and
/// not display cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorPosition {
    line: usize,
    column: usize,
}

impl Default for CursorPosition {
    fn default() -> Self {
        Self::new(1, 1)
    }
}

impl CursorPosition {
    /// Creates a cursor position.
    ///
    /// Values smaller than `1` are clamped to `1`.
    pub fn new(line: usize, column: usize) -> Self {
        Self {
            line: line.max(1),
            column: column.max(1),
        }
    }

    /// Returns the 1-based cursor line.
    pub fn line(&self) -> usize {
        self.line
    }

    /// Returns the 1-based cursor column in document text units.
    pub fn column(&self) -> usize {
        self.column
    }

    /// Converts the 1-based cursor into a zero-based document position.
    pub fn to_text_position(self) -> TextPosition {
        TextPosition::new(self.line.saturating_sub(1), self.column.saturating_sub(1))
    }

    /// Converts a zero-based document position into a 1-based cursor.
    pub fn from_text_position(position: TextPosition) -> Self {
        Self::new(
            position.line0().saturating_add(1),
            position.col0().saturating_add(1),
        )
    }
}

/// Current phase of a background document open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadPhase {
    Opening,
    InspectingSource,
    PreparingIndex,
    RecoveringSession,
    Ready,
}

impl LoadPhase {
    pub(super) fn as_raw(self) -> u8 {
        match self {
            Self::Opening => 0,
            Self::InspectingSource => 1,
            Self::PreparingIndex => 2,
            Self::RecoveringSession => 3,
            Self::Ready => 4,
        }
    }

    pub(super) fn from_raw(raw: u8) -> Self {
        match raw {
            0 => Self::Opening,
            1 => Self::InspectingSource,
            2 => Self::PreparingIndex,
            3 => Self::RecoveringSession,
            4 => Self::Ready,
            _ => Self::Opening,
        }
    }
}

/// Typed file-backed progress for background open/save work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileProgress {
    path: Arc<PathBuf>,
    completed_bytes: u64,
    total_bytes: u64,
    load_phase: Option<LoadPhase>,
}

impl FileProgress {
    pub(super) fn new(path: Arc<PathBuf>, completed_bytes: u64, total_bytes: u64) -> Self {
        Self {
            path,
            completed_bytes,
            total_bytes,
            load_phase: None,
        }
    }

    pub(super) fn loading(
        path: Arc<PathBuf>,
        completed_bytes: u64,
        total_bytes: u64,
        load_phase: LoadPhase,
    ) -> Self {
        Self {
            path,
            completed_bytes,
            total_bytes,
            load_phase: Some(load_phase),
        }
    }

    /// Returns the associated file path.
    pub fn path(&self) -> &Path {
        self.path.as_path()
    }

    /// Returns the completed byte count.
    ///
    /// For background loads this tracks source bytes inspected by the open path
    /// before the document becomes ready. Continued line indexing after the open
    /// completes is exposed separately through document-local
    /// `indexing_state()`.
    pub fn completed_bytes(&self) -> u64 {
        self.completed_bytes
    }

    /// Returns the total byte count.
    ///
    /// For background loads this is the source file length. For background saves
    /// this is the destination byte length being written.
    pub fn total_bytes(&self) -> u64 {
        self.total_bytes
    }

    /// Returns the current load phase when this progress value comes from a
    /// background open.
    pub fn load_phase(&self) -> Option<LoadPhase> {
        self.load_phase
    }

    /// Returns completion as a `0.0..=1.0` fraction.
    pub fn fraction(&self) -> f32 {
        if self.total_bytes == 0 {
            0.0
        } else {
            self.completed_bytes as f32 / self.total_bytes as f32
        }
    }
}

/// Current background activity of a [`DocumentSession`] or [`EditorTab`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackgroundActivity {
    Idle,
    Loading(FileProgress),
    Saving(FileProgress),
}

impl BackgroundActivity {
    /// Returns `true` when no background work is active.
    pub fn is_idle(&self) -> bool {
        matches!(self, Self::Idle)
    }

    /// Returns the current loading progress when a background open is active.
    pub fn loading_state(&self) -> Option<&FileProgress> {
        match self {
            Self::Loading(progress) => Some(progress),
            Self::Idle | Self::Saving(_) => None,
        }
    }

    /// Returns the current loading phase when a background open is active.
    pub fn loading_phase(&self) -> Option<LoadPhase> {
        self.loading_state().and_then(FileProgress::load_phase)
    }

    /// Returns the current save progress when a background save is active.
    pub fn save_state(&self) -> Option<&FileProgress> {
        match self {
            Self::Saving(progress) => Some(progress),
            Self::Idle | Self::Loading(_) => None,
        }
    }

    /// Returns whichever file-backed progress is currently active.
    pub fn progress(&self) -> Option<&FileProgress> {
        match self {
            Self::Idle => None,
            Self::Loading(progress) | Self::Saving(progress) => Some(progress),
        }
    }
}

/// Typed summary of the most recent background open/save problem observed by a
/// session wrapper.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BackgroundIssueKind {
    LoadFailed,
    SaveFailed,
    LoadDiscarded,
    SaveDiscarded,
}

impl BackgroundIssueKind {
    /// Returns `true` when the issue came from a background open.
    pub const fn is_load(self) -> bool {
        matches!(self, Self::LoadFailed | Self::LoadDiscarded)
    }

    /// Returns `true` when the issue came from a background save.
    pub const fn is_save(self) -> bool {
        matches!(self, Self::SaveFailed | Self::SaveDiscarded)
    }

    /// Returns `true` when the worker result was intentionally discarded.
    pub const fn is_discarded(self) -> bool {
        matches!(self, Self::LoadDiscarded | Self::SaveDiscarded)
    }
}

/// Snapshot of the most recent background open/save problem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackgroundIssue {
    kind: BackgroundIssueKind,
    path: Arc<PathBuf>,
    message: Arc<str>,
}

impl BackgroundIssue {
    pub(super) fn new(kind: BackgroundIssueKind, path: PathBuf, message: String) -> Self {
        Self {
            kind,
            path: Arc::new(path),
            message: Arc::from(message),
        }
    }

    /// Returns the issue kind.
    pub fn kind(&self) -> BackgroundIssueKind {
        self.kind
    }

    /// Returns the affected file path.
    pub fn path(&self) -> &Path {
        self.path.as_path()
    }

    /// Returns the short issue message.
    pub fn message(&self) -> &str {
        self.message.as_ref()
    }

    /// Returns `true` when the issue came from a background open.
    pub fn is_load(&self) -> bool {
        self.kind.is_load()
    }

    /// Returns `true` when the issue came from a background save.
    pub fn is_save(&self) -> bool {
        self.kind.is_save()
    }

    /// Returns `true` when the worker result was discarded instead of applied.
    pub fn is_discarded(&self) -> bool {
        self.kind.is_discarded()
    }
}

/// Snapshot of a [`DocumentSession`] state for frontend polling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentSessionStatus {
    generation: u64,
    document: DocumentStatus,
    background_activity: BackgroundActivity,
    background_issue: Option<BackgroundIssue>,
    close_pending: bool,
}

impl DocumentSessionStatus {
    /// Creates a session status snapshot.
    pub(super) fn new(
        generation: u64,
        document: DocumentStatus,
        background_activity: BackgroundActivity,
        background_issue: Option<BackgroundIssue>,
        close_pending: bool,
    ) -> Self {
        Self {
            generation,
            document,
            background_activity,
            background_issue,
            close_pending,
        }
    }

    /// Returns the session generation counter.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Returns the document status snapshot.
    pub fn document(&self) -> &DocumentStatus {
        &self.document
    }

    /// Returns the current document path, if one is set.
    pub fn path(&self) -> Option<&Path> {
        self.document.path()
    }

    /// Returns `true` when the document has unsaved changes.
    pub fn is_dirty(&self) -> bool {
        self.document.is_dirty()
    }

    /// Returns the current document line count.
    pub fn line_count(&self) -> LineCount {
        self.document.line_count()
    }

    /// Returns the best-effort line count for viewport sizing and scrolling.
    pub fn display_line_count(&self) -> usize {
        self.document.display_line_count()
    }

    /// Returns the exact document line count when it is known.
    pub fn exact_line_count(&self) -> Option<usize> {
        self.document.exact_line_count()
    }

    /// Returns `true` when the current line count is exact.
    pub fn is_line_count_exact(&self) -> bool {
        self.document.is_line_count_exact()
    }

    /// Returns the current document length in bytes.
    pub fn file_len(&self) -> usize {
        self.document.file_len()
    }

    /// Returns the currently detected line ending style.
    pub fn line_ending(&self) -> LineEnding {
        self.document.line_ending()
    }

    /// Returns the current document backing mode.
    pub fn backing(&self) -> DocumentBacking {
        self.document.backing()
    }

    /// Returns `true` when the document currently has a mutable edit buffer.
    pub fn has_edit_buffer(&self) -> bool {
        self.document.has_edit_buffer()
    }

    /// Returns `true` when the document is currently rope-backed.
    pub fn has_rope(&self) -> bool {
        self.document.has_rope()
    }

    /// Returns `true` when the document is currently piece-table-backed.
    pub fn has_piece_table(&self) -> bool {
        self.document.has_piece_table()
    }

    /// Returns typed indexing progress while document-local indexing is active.
    pub fn indexing_state(&self) -> Option<ByteProgress> {
        self.document.indexing_state()
    }

    /// Returns `true` while document-local indexing is still running.
    pub fn is_indexing(&self) -> bool {
        self.document.is_indexing()
    }

    /// Returns the current background activity.
    pub fn background_activity(&self) -> &BackgroundActivity {
        &self.background_activity
    }

    /// Returns the most recent background open/save problem, if one is being retained.
    pub fn background_issue(&self) -> Option<&BackgroundIssue> {
        self.background_issue.as_ref()
    }

    /// Returns `true` when `close_file()` was requested while a background job
    /// was active and the actual close is deferred until that job finishes.
    pub fn close_pending(&self) -> bool {
        self.close_pending
    }

    /// Returns typed loading progress when a background open is active.
    pub fn loading_state(&self) -> Option<&FileProgress> {
        self.background_activity.loading_state()
    }

    /// Returns the current loading phase when a background open is active.
    pub fn loading_phase(&self) -> Option<LoadPhase> {
        self.background_activity.loading_phase()
    }

    /// Returns typed save progress when a background save is active.
    pub fn save_state(&self) -> Option<&FileProgress> {
        self.background_activity.save_state()
    }

    /// Returns `true` while any background open/save job is active.
    pub fn is_busy(&self) -> bool {
        !matches!(self.background_activity, BackgroundActivity::Idle)
    }

    /// Returns `true` while a background load is in progress.
    pub fn is_loading(&self) -> bool {
        matches!(self.background_activity, BackgroundActivity::Loading(_))
    }

    /// Returns `true` while a background save is in progress.
    pub fn is_saving(&self) -> bool {
        matches!(self.background_activity, BackgroundActivity::Saving(_))
    }
}

/// Snapshot of an [`EditorTab`] state for frontend polling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditorTabStatus {
    id: u64,
    session: DocumentSessionStatus,
    cursor: CursorPosition,
    pinned: bool,
}

impl EditorTabStatus {
    /// Creates an editor-tab status snapshot.
    pub(super) fn new(
        id: u64,
        session: DocumentSessionStatus,
        cursor: CursorPosition,
        pinned: bool,
    ) -> Self {
        Self {
            id,
            session,
            cursor,
            pinned,
        }
    }

    /// Returns the tab identifier.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Returns the underlying session status snapshot.
    pub fn session(&self) -> &DocumentSessionStatus {
        &self.session
    }

    /// Returns the current tab generation counter.
    pub fn generation(&self) -> u64 {
        self.session.generation()
    }

    /// Returns the current document status snapshot.
    pub fn document(&self) -> &DocumentStatus {
        self.session.document()
    }

    /// Returns the current document path, if one is set.
    pub fn path(&self) -> Option<&Path> {
        self.session.path()
    }

    /// Returns `true` when the document has unsaved changes.
    pub fn is_dirty(&self) -> bool {
        self.session.is_dirty()
    }

    /// Returns the current document line count.
    pub fn line_count(&self) -> LineCount {
        self.session.line_count()
    }

    /// Returns the best-effort line count for viewport sizing and scrolling.
    pub fn display_line_count(&self) -> usize {
        self.session.display_line_count()
    }

    /// Returns the exact document line count when it is known.
    pub fn exact_line_count(&self) -> Option<usize> {
        self.session.exact_line_count()
    }

    /// Returns `true` when the current line count is exact.
    pub fn is_line_count_exact(&self) -> bool {
        self.session.is_line_count_exact()
    }

    /// Returns the current document length in bytes.
    pub fn file_len(&self) -> usize {
        self.session.file_len()
    }

    /// Returns the currently detected line ending style.
    pub fn line_ending(&self) -> LineEnding {
        self.session.line_ending()
    }

    /// Returns the current document backing mode.
    pub fn backing(&self) -> DocumentBacking {
        self.session.backing()
    }

    /// Returns `true` when the document currently has a mutable edit buffer.
    pub fn has_edit_buffer(&self) -> bool {
        self.session.has_edit_buffer()
    }

    /// Returns `true` when the document is currently rope-backed.
    pub fn has_rope(&self) -> bool {
        self.session.has_rope()
    }

    /// Returns `true` when the document is currently piece-table-backed.
    pub fn has_piece_table(&self) -> bool {
        self.session.has_piece_table()
    }

    /// Returns the current background activity.
    pub fn background_activity(&self) -> &BackgroundActivity {
        self.session.background_activity()
    }

    /// Returns the most recent background open/save problem, if one is being retained.
    pub fn background_issue(&self) -> Option<&BackgroundIssue> {
        self.session.background_issue()
    }

    /// Returns `true` when `close_file()` was requested while a background job
    /// was active and the actual close is deferred until that job finishes.
    pub fn close_pending(&self) -> bool {
        self.session.close_pending()
    }

    /// Returns typed loading progress when a background open is active.
    pub fn loading_state(&self) -> Option<&FileProgress> {
        self.session.loading_state()
    }

    /// Returns the current loading phase when a background open is active.
    pub fn loading_phase(&self) -> Option<LoadPhase> {
        self.session.loading_phase()
    }

    /// Returns typed save progress when a background save is active.
    pub fn save_state(&self) -> Option<&FileProgress> {
        self.session.save_state()
    }

    /// Returns `true` while any background open/save job is active.
    pub fn is_busy(&self) -> bool {
        self.session.is_busy()
    }

    /// Returns `true` while a background load is in progress.
    pub fn is_loading(&self) -> bool {
        self.session.is_loading()
    }

    /// Returns `true` while a background save is in progress.
    pub fn is_saving(&self) -> bool {
        self.session.is_saving()
    }

    /// Returns the current cursor position.
    pub fn cursor(&self) -> CursorPosition {
        self.cursor
    }

    /// Returns `true` when the tab is pinned.
    pub fn is_pinned(&self) -> bool {
        self.pinned
    }
}

/// High-level save errors produced by the session wrappers.
#[derive(Debug)]
pub enum SaveError {
    /// No path is associated with the current document.
    NoPath,
    /// A background save is already running.
    InProgress,
    /// The underlying document save operation failed.
    Io(DocumentError),
}

impl std::fmt::Display for SaveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoPath => write!(f, "no path set for current document"),
            Self::InProgress => write!(f, "save already in progress"),
            Self::Io(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for SaveError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            Self::NoPath | Self::InProgress => None,
        }
    }
}
