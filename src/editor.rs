use crate::document::SaveCompletion;
use crate::{
    ByteProgress, CutResult, Document, DocumentBacking, DocumentError, DocumentStatus,
    EditCapability, EditResult, LineCount, LineEnding, TextPosition, TextRange, TextSelection,
    TextSlice, Viewport, ViewportRequest,
};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;

/// Cursor position in document coordinates, using 1-based indexing.
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

    /// Returns the 1-based cursor column.
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

/// Typed file-backed progress for background open/save work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileProgress {
    path: PathBuf,
    completed_bytes: u64,
    total_bytes: u64,
}

impl FileProgress {
    fn new(path: PathBuf, completed_bytes: u64, total_bytes: u64) -> Self {
        Self {
            path,
            completed_bytes,
            total_bytes,
        }
    }

    /// Returns the associated file path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns the completed byte count.
    pub fn completed_bytes(&self) -> u64 {
        self.completed_bytes
    }

    /// Returns the total byte count.
    pub fn total_bytes(&self) -> u64 {
        self.total_bytes
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

/// Snapshot of a [`DocumentSession`] state for frontend polling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentSessionStatus {
    generation: u64,
    document: DocumentStatus,
    background_activity: BackgroundActivity,
}

impl DocumentSessionStatus {
    /// Creates a session status snapshot.
    pub fn new(
        generation: u64,
        document: DocumentStatus,
        background_activity: BackgroundActivity,
    ) -> Self {
        Self {
            generation,
            document,
            background_activity,
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

    /// Returns typed loading progress when a background open is active.
    pub fn loading_state(&self) -> Option<&FileProgress> {
        self.background_activity.loading_state()
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
    pub fn new(
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

    /// Returns typed loading progress when a background open is active.
    pub fn loading_state(&self) -> Option<&FileProgress> {
        self.session.loading_state()
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

#[derive(Debug)]
struct SaveJob {
    path: PathBuf,
    total_bytes: u64,
    written_bytes: Arc<AtomicU64>,
    rx: mpsc::Receiver<Result<SaveCompletion, DocumentError>>,
}

#[derive(Debug)]
struct LoadJob {
    path: PathBuf,
    total_bytes: u64,
    loaded_bytes: Arc<AtomicU64>,
    rx: mpsc::Receiver<Result<Document, DocumentError>>,
}

#[derive(Debug)]
struct SessionCore {
    doc: Document,
    generation: u64,
    load_job: Option<LoadJob>,
    save_job: Option<SaveJob>,
    clear_dirty_after_open: bool,
}

impl SessionCore {
    fn new() -> Self {
        Self {
            doc: Document::new(),
            generation: 0,
            load_job: None,
            save_job: None,
            clear_dirty_after_open: false,
        }
    }

    fn generation(&self) -> u64 {
        self.generation
    }

    fn is_saving(&self) -> bool {
        self.save_job.is_some()
    }

    fn is_loading(&self) -> bool {
        self.load_job.is_some()
    }

    fn is_busy(&self) -> bool {
        self.is_loading() || self.is_saving()
    }

    fn indexing_progress(&self) -> Option<(usize, usize)> {
        self.doc
            .indexing_state()
            .map(|progress| (progress.completed_bytes(), progress.total_bytes()))
    }

    fn indexing_state(&self) -> Option<ByteProgress> {
        self.doc.indexing_state()
    }

    fn loading_state(&self) -> Option<FileProgress> {
        let job = self.load_job.as_ref()?;
        Some(FileProgress::new(
            job.path.clone(),
            job.loaded_bytes
                .load(Ordering::Relaxed)
                .min(job.total_bytes),
            job.total_bytes,
        ))
    }

    fn loading_progress(&self) -> Option<(u64, u64, PathBuf)> {
        self.loading_state().map(|progress| {
            (
                progress.completed_bytes(),
                progress.total_bytes(),
                progress.path().to_path_buf(),
            )
        })
    }

    fn poll_load_job(&mut self) -> Option<Result<(), DocumentError>> {
        let state = match self.load_job.as_ref()?.rx.try_recv() {
            Ok(res) => res,
            Err(mpsc::TryRecvError::Empty) => return None,
            Err(mpsc::TryRecvError::Disconnected) => {
                let job = self.load_job.take().expect("load job must exist");
                return Some(Err(DocumentError::Open {
                    path: job.path,
                    source: io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "load worker disconnected unexpectedly",
                    ),
                }));
            }
        };

        self.load_job = None;
        Some(match state {
            Ok(doc) => {
                self.finish_open(doc);
                Ok(())
            }
            Err(err) => Err(err),
        })
    }

    fn poll_background_job(&mut self) -> Option<Result<(), DocumentError>> {
        self.poll_load_job().or_else(|| self.poll_save_job())
    }

    fn document(&self) -> &Document {
        &self.doc
    }

    fn document_mut(&mut self) -> &mut Document {
        &mut self.doc
    }

    fn current_path(&self) -> Option<&Path> {
        self.doc.path()
    }

    fn is_dirty(&self) -> bool {
        self.doc.is_dirty()
    }

    fn open_file(&mut self, path: PathBuf) -> Result<(), DocumentError> {
        if self.is_saving() {
            return Err(DocumentError::Write {
                path,
                source: io::Error::other("cannot open while save is in progress"),
            });
        }
        if self.is_loading() {
            return Err(DocumentError::Open {
                path,
                source: io::Error::other("cannot open while another load is in progress"),
            });
        }
        let doc = Document::open(path)?;
        self.finish_open(doc);
        Ok(())
    }

    fn open_file_async(&mut self, path: PathBuf) -> Result<(), DocumentError> {
        if self.is_saving() {
            return Err(DocumentError::Write {
                path,
                source: io::Error::other("cannot open while save is in progress"),
            });
        }
        if self.is_loading() {
            return Err(DocumentError::Open {
                path,
                source: io::Error::other("load already in progress"),
            });
        }

        let total_bytes = fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
        let loaded_bytes = Arc::new(AtomicU64::new(0));
        let rx = spawn_load_worker(path.clone(), total_bytes, Arc::clone(&loaded_bytes));
        self.load_job = Some(LoadJob {
            path,
            total_bytes,
            loaded_bytes,
            rx,
        });
        self.clear_dirty_after_open = false;
        Ok(())
    }

    fn close_file(&mut self) {
        self.load_job = None;
        self.save_job = None;
        self.doc = Document::new();
        self.generation = self.generation.wrapping_add(1);
        self.clear_dirty_after_open = false;
    }

    fn after_document_frame(&mut self) {
        if !self.clear_dirty_after_open {
            return;
        }
        self.doc.mark_clean();
        self.clear_dirty_after_open = false;
    }

    fn cancel_clear_dirty_after_open(&mut self) {
        self.clear_dirty_after_open = false;
    }

    fn save(&mut self) -> Result<(), SaveError> {
        let Some(path) = self.current_path().map(|p| p.to_path_buf()) else {
            return Err(SaveError::NoPath);
        };
        if self.is_loading() {
            return Err(SaveError::Io(DocumentError::Write {
                path,
                source: io::Error::other("cannot save while load is in progress"),
            }));
        }
        if !self.doc.is_dirty() {
            return Ok(());
        }
        self.doc.save_to(&path).map_err(SaveError::Io)?;
        self.generation = self.generation.wrapping_add(1);
        Ok(())
    }

    fn save_as(&mut self, path: PathBuf) -> Result<(), DocumentError> {
        if self.is_loading() {
            return Err(DocumentError::Write {
                path,
                source: io::Error::other("cannot save while load is in progress"),
            });
        }
        self.doc.save_to(&path)?;
        self.generation = self.generation.wrapping_add(1);
        Ok(())
    }

    fn set_path(&mut self, path: PathBuf) {
        self.doc.set_path(path);
    }

    fn save_state(&self) -> Option<FileProgress> {
        let job = self.save_job.as_ref()?;
        Some(FileProgress::new(
            job.path.clone(),
            job.written_bytes
                .load(Ordering::Relaxed)
                .min(job.total_bytes),
            job.total_bytes,
        ))
    }

    fn save_progress(&self) -> Option<(u64, u64, PathBuf)> {
        self.save_state().map(|progress| {
            (
                progress.completed_bytes(),
                progress.total_bytes(),
                progress.path().to_path_buf(),
            )
        })
    }

    fn poll_save_job(&mut self) -> Option<Result<(), DocumentError>> {
        let state = match self.save_job.as_ref()?.rx.try_recv() {
            Ok(res) => res,
            Err(mpsc::TryRecvError::Empty) => return None,
            Err(mpsc::TryRecvError::Disconnected) => {
                let job = self.save_job.take().expect("save job must exist");
                return Some(Err(DocumentError::Write {
                    path: job.path,
                    source: io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "save worker disconnected unexpectedly",
                    ),
                }));
            }
        };

        self.save_job = None;
        Some(match state {
            Ok(completion) => match self
                .doc
                .finish_save(completion.path, completion.reload_after_save)
            {
                Ok(()) => {
                    self.generation = self.generation.wrapping_add(1);
                    Ok(())
                }
                Err(err) => Err(err),
            },
            Err(err) => Err(err),
        })
    }

    fn save_async(&mut self) -> Result<bool, SaveError> {
        if self.is_saving() {
            return Err(SaveError::InProgress);
        }
        let Some(path) = self.current_path().map(|p| p.to_path_buf()) else {
            return Err(SaveError::NoPath);
        };
        self.save_to_async(path).map_err(SaveError::Io)
    }

    fn save_as_async(&mut self, path: PathBuf) -> Result<bool, DocumentError> {
        self.save_to_async(path)
    }

    fn save_to_async(&mut self, path: PathBuf) -> Result<bool, DocumentError> {
        if self.is_saving() {
            return Err(DocumentError::Write {
                path,
                source: io::Error::other("save already in progress"),
            });
        }
        if self.is_loading() {
            return Err(DocumentError::Write {
                path,
                source: io::Error::other("cannot save while load is in progress"),
            });
        }

        if !self.doc.is_dirty() && self.current_path() == Some(path.as_path()) {
            return Ok(false);
        }

        let prepared = self.doc.prepare_save(&path);
        let total_bytes = prepared.total_bytes();
        let written_bytes = Arc::new(AtomicU64::new(0));
        let rx = spawn_save_worker(prepared, Arc::clone(&written_bytes));

        self.save_job = Some(SaveJob {
            path,
            total_bytes,
            written_bytes,
            rx,
        });
        Ok(true)
    }

    fn background_activity(&self) -> BackgroundActivity {
        if let Some(progress) = self.loading_state() {
            BackgroundActivity::Loading(progress)
        } else if let Some(progress) = self.save_state() {
            BackgroundActivity::Saving(progress)
        } else {
            BackgroundActivity::Idle
        }
    }

    fn read_viewport(&self, request: ViewportRequest) -> Viewport {
        self.doc.read_viewport(request)
    }

    fn status(&self) -> DocumentSessionStatus {
        DocumentSessionStatus::new(
            self.generation(),
            self.doc.status(),
            self.background_activity(),
        )
    }

    fn finish_open(&mut self, doc: Document) {
        self.clear_dirty_after_open = !doc.is_dirty();
        self.doc = doc;
        self.generation = self.generation.wrapping_add(1);
    }
}

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
    pub fn loading_state(&self) -> Option<FileProgress> {
        self.core.loading_state()
    }

    /// Returns background-load progress.
    ///
    /// Prefer [`DocumentSession::loading_state`] in new code when you want a
    /// typed progress value.
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
    pub fn document_mut(&mut self) -> &mut Document {
        self.core.document_mut()
    }

    /// Returns the full document text as a `String`.
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

    /// Applies a typed insert directly through the session.
    pub fn try_insert(
        &mut self,
        position: TextPosition,
        text: &str,
    ) -> Result<TextPosition, DocumentError> {
        self.core.document_mut().try_insert(position, text)
    }

    /// Applies a typed replacement directly through the session.
    pub fn try_replace(
        &mut self,
        range: TextRange,
        text: &str,
    ) -> Result<TextPosition, DocumentError> {
        self.core.document_mut().try_replace(range, text)
    }

    /// Replaces the current selection and returns the resulting caret position.
    pub fn try_replace_selection(
        &mut self,
        selection: TextSelection,
        text: &str,
    ) -> Result<TextPosition, DocumentError> {
        self.core
            .document_mut()
            .try_replace_selection(selection, text)
    }

    /// Applies a typed backspace directly through the session.
    pub fn try_backspace(&mut self, position: TextPosition) -> Result<EditResult, DocumentError> {
        self.core.document_mut().try_backspace(position)
    }

    /// Applies a backspace command to an anchor/head selection.
    pub fn try_backspace_selection(
        &mut self,
        selection: TextSelection,
    ) -> Result<EditResult, DocumentError> {
        self.core.document_mut().try_backspace_selection(selection)
    }

    /// Deletes the text unit at the cursor and keeps the resulting caret position.
    pub fn try_delete_forward(
        &mut self,
        position: TextPosition,
    ) -> Result<EditResult, DocumentError> {
        self.core.document_mut().try_delete_forward(position)
    }

    /// Applies a forward-delete command to an anchor/head selection.
    pub fn try_delete_forward_selection(
        &mut self,
        selection: TextSelection,
    ) -> Result<EditResult, DocumentError> {
        self.core
            .document_mut()
            .try_delete_forward_selection(selection)
    }

    /// Deletes the current selection and returns the resulting caret position.
    pub fn try_delete_selection(
        &mut self,
        selection: TextSelection,
    ) -> Result<EditResult, DocumentError> {
        self.core.document_mut().try_delete_selection(selection)
    }

    /// Cuts the current selection and returns the removed text together with the edit result.
    pub fn try_cut_selection(
        &mut self,
        selection: TextSelection,
    ) -> Result<CutResult, DocumentError> {
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
    #[deprecated(since = "0.3.0", note = "use save_state() for typed progress instead")]
    pub fn save_progress(&self) -> Option<(u64, u64, PathBuf)> {
        self.core.save_progress()
    }

    /// Starts a background save to the current path.
    pub fn save_async(&mut self) -> Result<bool, SaveError> {
        self.core.save_async()
    }

    /// Starts a background save to a new path.
    pub fn save_as_async(&mut self, path: PathBuf) -> Result<bool, DocumentError> {
        self.core.save_as_async(path)
    }

    /// Returns the current background activity for the session.
    pub fn background_activity(&self) -> BackgroundActivity {
        self.core.background_activity()
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

/// Lightweight editor-tab state with a document, cursor, and async save tracking.
#[derive(Debug)]
pub struct EditorTab {
    id: u64,
    core: SessionCore,
    cursor: CursorPosition,
    pinned: bool,
}

impl EditorTab {
    /// Creates a new empty tab with the provided identifier.
    pub fn new(id: u64) -> Self {
        Self {
            id,
            core: SessionCore::new(),
            cursor: CursorPosition::default(),
            pinned: false,
        }
    }

    /// Returns the tab identifier.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Returns the tab generation counter.
    ///
    /// The counter is incremented after operations that fully replace the document.
    pub fn generation(&self) -> u64 {
        self.core.generation()
    }

    /// Returns `true` while a background save is in progress.
    pub fn is_saving(&self) -> bool {
        self.core.is_saving()
    }

    /// Returns `true` while any background load/save worker is active.
    pub fn is_busy(&self) -> bool {
        self.core.is_busy()
    }

    /// Returns `true` while document-local indexing is still running.
    pub fn is_indexing(&self) -> bool {
        self.core.document().is_indexing()
    }

    /// Returns `true` while a background load is in progress.
    pub fn is_loading(&self) -> bool {
        self.core.is_loading()
    }

    /// Returns document indexing progress as `(indexed_bytes, total_bytes)`.
    ///
    /// Prefer [`EditorTab::indexing_state`] in new code when you want a typed
    /// progress value.
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
    pub fn loading_state(&self) -> Option<FileProgress> {
        self.core.loading_state()
    }

    /// Returns background-load progress.
    ///
    /// Prefer [`EditorTab::loading_state`] in new code when you want a typed
    /// progress value.
    #[deprecated(
        since = "0.3.0",
        note = "use loading_state() for typed progress instead"
    )]
    pub fn loading_progress(&self) -> Option<(u64, u64, PathBuf)> {
        self.core.loading_progress()
    }

    /// Polls the background-load state.
    pub fn poll_load_job(&mut self) -> Option<Result<(), DocumentError>> {
        let result = self.core.poll_load_job();
        if matches!(result, Some(Ok(()))) {
            self.cursor = CursorPosition::default();
        }
        result
    }

    /// Polls whichever background job is active.
    ///
    /// Load jobs are checked first because open/save are mutually exclusive.
    pub fn poll_background_job(&mut self) -> Option<Result<(), DocumentError>> {
        self.poll_load_job().or_else(|| self.poll_save_job())
    }

    /// Returns immutable access to the tab document.
    pub fn document(&self) -> &Document {
        self.core.document()
    }

    /// Returns mutable access to the tab document.
    pub fn document_mut(&mut self) -> &mut Document {
        self.core.document_mut()
    }

    /// Returns the full document text as a `String`.
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

    /// Returns `true` if the tab is pinned.
    pub fn is_pinned(&self) -> bool {
        self.pinned
    }

    /// Toggles the pinned state.
    pub fn toggle_pinned(&mut self) {
        self.pinned = !self.pinned;
    }

    /// Returns the current cursor position.
    pub fn cursor(&self) -> CursorPosition {
        self.cursor
    }

    /// Returns the current cursor as a zero-based text position.
    pub fn cursor_position(&self) -> TextPosition {
        self.cursor.to_text_position()
    }

    /// Sets the cursor position using 1-based coordinates.
    pub fn set_cursor_line_col(&mut self, line: usize, column: usize) {
        self.cursor = CursorPosition::new(line, column);
    }

    /// Sets the cursor using a zero-based text position.
    pub fn set_cursor_position(&mut self, position: TextPosition) {
        self.cursor = CursorPosition::from_text_position(self.clamp_position(position));
    }

    /// Recomputes the cursor position from a character index in the full document text.
    pub fn update_cursor_char_index(&mut self, char_index: usize) {
        self.cursor = CursorPosition::from_text_position(self.position_for_char_index(char_index));
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

    /// Applies a typed insert and updates the tab cursor to the resulting position.
    pub fn try_insert(
        &mut self,
        position: TextPosition,
        text: &str,
    ) -> Result<TextPosition, DocumentError> {
        let cursor = self.core.document_mut().try_insert(position, text)?;
        self.cursor = CursorPosition::from_text_position(cursor);
        Ok(cursor)
    }

    /// Applies a typed replacement and updates the tab cursor to the resulting position.
    pub fn try_replace(
        &mut self,
        range: TextRange,
        text: &str,
    ) -> Result<TextPosition, DocumentError> {
        let cursor = self.core.document_mut().try_replace(range, text)?;
        self.cursor = CursorPosition::from_text_position(cursor);
        Ok(cursor)
    }

    /// Replaces the current selection and updates the tab cursor.
    pub fn try_replace_selection(
        &mut self,
        selection: TextSelection,
        text: &str,
    ) -> Result<TextPosition, DocumentError> {
        let cursor = self
            .core
            .document_mut()
            .try_replace_selection(selection, text)?;
        self.cursor = CursorPosition::from_text_position(cursor);
        Ok(cursor)
    }

    /// Applies a typed backspace and updates the tab cursor to the resulting position.
    pub fn try_backspace(&mut self, position: TextPosition) -> Result<EditResult, DocumentError> {
        let result = self.core.document_mut().try_backspace(position)?;
        self.cursor = CursorPosition::from_text_position(result.cursor());
        Ok(result)
    }

    /// Applies a backspace command to an anchor/head selection and updates the tab cursor.
    pub fn try_backspace_selection(
        &mut self,
        selection: TextSelection,
    ) -> Result<EditResult, DocumentError> {
        let result = self
            .core
            .document_mut()
            .try_backspace_selection(selection)?;
        self.cursor = CursorPosition::from_text_position(result.cursor());
        Ok(result)
    }

    /// Deletes the text unit at the cursor and updates the tab cursor.
    pub fn try_delete_forward(
        &mut self,
        position: TextPosition,
    ) -> Result<EditResult, DocumentError> {
        let result = self.core.document_mut().try_delete_forward(position)?;
        self.cursor = CursorPosition::from_text_position(result.cursor());
        Ok(result)
    }

    /// Applies a forward-delete command to an anchor/head selection and updates the tab cursor.
    pub fn try_delete_forward_selection(
        &mut self,
        selection: TextSelection,
    ) -> Result<EditResult, DocumentError> {
        let result = self
            .core
            .document_mut()
            .try_delete_forward_selection(selection)?;
        self.cursor = CursorPosition::from_text_position(result.cursor());
        Ok(result)
    }

    /// Deletes the current selection and updates the tab cursor.
    pub fn try_delete_selection(
        &mut self,
        selection: TextSelection,
    ) -> Result<EditResult, DocumentError> {
        let result = self.core.document_mut().try_delete_selection(selection)?;
        self.cursor = CursorPosition::from_text_position(result.cursor());
        Ok(result)
    }

    /// Cuts the current selection and updates the tab cursor.
    pub fn try_cut_selection(
        &mut self,
        selection: TextSelection,
    ) -> Result<CutResult, DocumentError> {
        let result = self.core.document_mut().try_cut_selection(selection)?;
        self.cursor = CursorPosition::from_text_position(result.cursor());
        Ok(result)
    }

    /// Opens a file in the tab and resets the cursor to the start of the document.
    ///
    /// # Errors
    /// Returns [`DocumentError`] if the file cannot be opened or if a save is
    /// already in progress for the current document.
    pub fn open_file(&mut self, path: PathBuf) -> Result<(), DocumentError> {
        self.core.open_file(path)?;
        self.cursor = CursorPosition::default();
        Ok(())
    }

    /// Starts opening a file on a background worker.
    ///
    /// # Errors
    /// Returns [`DocumentError`] if another background load or save is already
    /// in progress for the tab.
    pub fn open_file_async(&mut self, path: PathBuf) -> Result<(), DocumentError> {
        self.core.open_file_async(path)
    }

    /// Closes the current document and replaces it with an empty one.
    pub fn close_file(&mut self) {
        self.core.close_file();
        self.cursor = CursorPosition::default();
    }

    /// Clears a deferred dirty flag scheduled after a clean document open.
    pub fn after_text_edit_frame(&mut self) {
        self.core.after_document_frame();
    }

    /// Cancels the deferred dirty-flag clear scheduled after a clean open.
    pub fn cancel_clear_dirty_after_open(&mut self) {
        self.core.cancel_clear_dirty_after_open();
    }

    /// Saves the document synchronously to its current path.
    ///
    /// # Errors
    /// Returns [`SaveError::NoPath`] if no path is set and [`SaveError::Io`] if
    /// the write operation fails.
    pub fn save(&mut self) -> Result<(), SaveError> {
        self.core.save()
    }

    /// Saves the document synchronously to a new path.
    ///
    /// # Errors
    /// Returns [`DocumentError`] if the write operation fails.
    pub fn save_as(&mut self, path: PathBuf) -> Result<(), DocumentError> {
        self.core.save_as(path)
    }

    /// Sets the document path without saving.
    pub fn set_path(&mut self, path: PathBuf) {
        self.core.set_path(path);
    }

    /// Returns typed background-save progress.
    pub fn save_state(&self) -> Option<FileProgress> {
        self.core.save_state()
    }

    /// Returns background-save progress in bytes together with the destination path.
    ///
    /// Prefer [`EditorTab::save_state`] in new code when you want a typed
    /// progress value.
    #[deprecated(since = "0.3.0", note = "use save_state() for typed progress instead")]
    pub fn save_progress(&self) -> Option<(u64, u64, PathBuf)> {
        self.core.save_progress()
    }

    /// Polls the background-save state and applies the completed result.
    ///
    /// Returns `None` if no save has been started or if the job is still running.
    pub fn poll_save_job(&mut self) -> Option<Result<(), DocumentError>> {
        self.core.poll_save_job()
    }

    /// Starts a background save to the current path.
    ///
    /// Returns `Ok(false)` when the document is unchanged and no save is needed.
    ///
    /// # Errors
    /// Returns [`SaveError::NoPath`] if no path is set,
    /// [`SaveError::InProgress`] if a save is already running, and
    /// [`SaveError::Io`] if save preparation fails.
    pub fn save_async(&mut self) -> Result<bool, SaveError> {
        self.core.save_async()
    }

    /// Starts a background save to a new path.
    ///
    /// Returns `Ok(false)` when the document is unchanged and is already bound
    /// to the same path.
    ///
    /// # Errors
    /// Returns [`DocumentError`] if save preparation fails.
    pub fn save_as_async(&mut self, path: PathBuf) -> Result<bool, DocumentError> {
        self.core.save_as_async(path)
    }

    /// Returns the current background activity for the tab.
    pub fn background_activity(&self) -> BackgroundActivity {
        self.core.background_activity()
    }

    /// Returns a frontend-friendly snapshot of the current tab state.
    pub fn status(&self) -> EditorTabStatus {
        EditorTabStatus::new(self.id, self.core.status(), self.cursor, self.pinned)
    }

    /// Reads a visible viewport directly from the current document.
    pub fn read_viewport(&self, request: ViewportRequest) -> Viewport {
        self.core.read_viewport(request)
    }
}

fn spawn_save_worker(
    prepared: crate::document::PreparedSave,
    written_bytes: Arc<AtomicU64>,
) -> mpsc::Receiver<Result<SaveCompletion, DocumentError>> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let result = prepared.execute(written_bytes);
        let _ = tx.send(result);
    });
    rx
}

fn spawn_load_worker(
    path: PathBuf,
    total_bytes: u64,
    loaded_bytes: Arc<AtomicU64>,
) -> mpsc::Receiver<Result<Document, DocumentError>> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let result = Document::open(path);
        if result.is_ok() {
            loaded_bytes.store(total_bytes, Ordering::Relaxed);
        }
        let _ = tx.send(result);
    });
    rx
}

#[cfg(test)]
mod tests {
    use super::{BackgroundActivity, CursorPosition, DocumentSession, EditorTab};
    use crate::{DocumentBacking, EditCapability, TextPosition, TextSelection, ViewportRequest};
    use std::fs;
    use std::time::{Duration, Instant};

    #[test]
    fn save_async_completes_and_clears_dirty_flag() {
        let dir = std::env::temp_dir().join(format!("qem-editor-save-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("large.txt");
        let original = b"abc\ndef\n".repeat((1024 * 1024 / 8) + 1);
        fs::write(&path, &original).unwrap();

        let mut tab = EditorTab::new(1);
        tab.open_file(path.clone()).unwrap();
        let _ = tab.document_mut().try_insert_text_at(0, 0, "123").unwrap();

        assert!(tab.is_dirty());
        assert!(tab.save_async().unwrap());
        assert!(tab.is_saving());
        assert!(tab.is_busy());

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(result) = tab.poll_background_job() {
                result.unwrap();
                break;
            }
            assert!(Instant::now() < deadline, "async save timed out");
            std::thread::sleep(Duration::from_millis(10));
        }

        assert!(!tab.is_dirty());
        assert!(!tab.is_saving());
        assert!(fs::read(&path).unwrap().starts_with(b"123abc\ndef\n"));

        let _ = fs::remove_file(&path);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn update_cursor_char_index_treats_crlf_as_single_newline() {
        let dir = std::env::temp_dir().join(format!("qem-editor-crlf-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("crlf.txt");
        fs::write(&path, b"a\r\nb\r\n").unwrap();

        let mut tab = EditorTab::new(1);
        tab.open_file(path.clone()).unwrap();

        tab.update_cursor_char_index(2);
        assert_eq!(tab.cursor().line(), 2);
        assert_eq!(tab.cursor().column(), 1);

        tab.update_cursor_char_index(3);
        assert_eq!(tab.cursor().line(), 2);
        assert_eq!(tab.cursor().column(), 1);

        tab.update_cursor_char_index(4);
        assert_eq!(tab.cursor().line(), 2);
        assert_eq!(tab.cursor().column(), 2);

        let _ = fs::remove_file(&path);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn cursor_position_roundtrips_with_text_position() {
        let cursor = CursorPosition::new(3, 5);
        let position = cursor.to_text_position();

        assert_eq!(position.line0(), 2);
        assert_eq!(position.col0(), 4);
        assert_eq!(CursorPosition::from_text_position(position), cursor);
    }

    #[test]
    fn open_file_async_completes_and_exposes_progress() {
        let dir = std::env::temp_dir().join(format!("qem-editor-open-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("open.txt");
        fs::write(&path, b"alpha\nbeta\n").unwrap();

        let mut tab = EditorTab::new(7);
        tab.set_cursor_line_col(9, 9);
        tab.open_file_async(path.clone()).unwrap();

        let progress = tab
            .loading_state()
            .expect("typed load progress should exist");
        assert_eq!(progress.total_bytes(), fs::metadata(&path).unwrap().len());
        assert_eq!(progress.path(), path.as_path());
        let typed_progress = tab
            .loading_state()
            .expect("typed load progress should exist");
        assert_eq!(typed_progress.completed_bytes(), 0);
        assert_eq!(typed_progress.total_bytes(), progress.total_bytes());
        assert!(matches!(
            tab.background_activity(),
            BackgroundActivity::Loading(_)
        ));
        let loading_status = tab.status();
        assert!(loading_status.is_loading());
        assert_eq!(
            loading_status
                .loading_state()
                .expect("status should expose typed loading progress")
                .path(),
            path.as_path()
        );
        assert!(tab.is_loading());
        assert!(tab.is_busy());

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(result) = tab.poll_background_job() {
                result.unwrap();
                break;
            }
            assert!(Instant::now() < deadline, "async load timed out");
            std::thread::sleep(Duration::from_millis(10));
        }

        assert!(!tab.is_loading());
        assert_eq!(tab.cursor().line(), 1);
        assert_eq!(tab.cursor().column(), 1);
        assert_eq!(tab.cursor_position().line0(), 0);
        assert_eq!(tab.cursor_position().col0(), 0);
        assert_eq!(tab.current_path(), Some(path.as_path()));
        assert_eq!(tab.exact_line_count(), Some(3));
        assert_eq!(tab.display_line_count(), 3);
        assert!(tab.is_line_count_exact());
        assert_eq!(tab.line_len_chars(0), 5);
        assert_eq!(tab.position_for_char_index(6), TextPosition::new(1, 0));
        assert_eq!(tab.char_index_for_position(TextPosition::new(1, 0)), 6);
        assert_eq!(
            tab.text_range_between(TextPosition::new(1, 2), TextPosition::new(0, 4))
                .len_chars(),
            4
        );
        let viewport = tab.read_viewport(ViewportRequest::new(0, 2).with_columns(0, 16));
        assert_eq!(viewport.rows()[0].text(), "alpha");
        assert!(matches!(
            tab.background_activity(),
            BackgroundActivity::Idle
        ));

        let _ = fs::remove_file(&path);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn editor_tab_try_insert_updates_cursor() {
        let mut tab = EditorTab::new(11);

        let cursor = tab
            .try_insert(TextPosition::new(0, 0), "alpha\nbeta")
            .unwrap();
        let status = tab.status();

        assert_eq!(cursor, TextPosition::new(1, 4));
        assert_eq!(tab.cursor_position(), TextPosition::new(1, 4));
        assert_eq!(tab.cursor().line(), 2);
        assert_eq!(tab.cursor().column(), 5);
        assert!(tab.is_dirty());
        assert_eq!(tab.display_line_count(), 2);
        assert_eq!(tab.text(), "alpha\nbeta");
        assert_eq!(status.id(), 11);
        assert_eq!(status.generation(), 0);
        assert_eq!(status.cursor().line(), 2);
        assert_eq!(status.cursor().column(), 5);
        assert!(status.is_dirty());
        assert_eq!(status.file_len(), tab.file_len());
        assert_eq!(status.exact_line_count(), Some(2));
        assert!(status.has_edit_buffer());
        assert!(status.has_rope());
        assert!(!status.has_piece_table());
        assert!(!status.is_busy());
    }

    #[test]
    fn editor_tab_selection_helpers_update_cursor() {
        let mut tab = EditorTab::new(12);
        let _ = tab
            .try_insert(TextPosition::new(0, 0), "alpha\nbeta")
            .unwrap();

        assert_eq!(
            tab.edit_capability_at(TextPosition::new(0, 1)),
            EditCapability::Editable {
                backing: DocumentBacking::Rope,
            }
        );
        let selection = TextSelection::new(TextPosition::new(1, 2), TextPosition::new(0, 4));
        let selected = tab.read_selection(selection);
        assert!(selected.is_exact());
        assert_eq!(selected.text(), "a\nbe");
        let cursor = tab.try_replace_selection(selection, "Z").unwrap();

        assert_eq!(cursor, TextPosition::new(0, 5));
        assert_eq!(tab.cursor_position(), TextPosition::new(0, 5));
        assert_eq!(tab.text(), "alphZta");

        let delete = tab
            .try_delete_selection(TextSelection::caret(TextPosition::new(0, 2)))
            .unwrap();
        assert!(!delete.changed());
        assert_eq!(delete.cursor(), TextPosition::new(0, 2));
        assert_eq!(tab.cursor_position(), TextPosition::new(0, 2));
    }

    #[test]
    fn editor_tab_delete_forward_updates_cursor() {
        let mut tab = EditorTab::new(13);
        let _ = tab
            .try_insert(TextPosition::new(0, 0), "alpha\nbeta")
            .unwrap();

        let result = tab.try_delete_forward(TextPosition::new(0, 5)).unwrap();
        assert!(result.changed());
        assert_eq!(result.cursor(), TextPosition::new(0, 5));
        assert_eq!(tab.cursor_position(), TextPosition::new(0, 5));
        assert_eq!(tab.text(), "alphabeta");
    }

    #[test]
    fn editor_tab_cut_selection_updates_cursor() {
        let mut tab = EditorTab::new(14);
        let _ = tab
            .try_insert(TextPosition::new(0, 0), "alpha\nbeta")
            .unwrap();

        let cut = tab
            .try_cut_selection(TextSelection::new(
                TextPosition::new(0, 3),
                TextPosition::new(1, 2),
            ))
            .unwrap();

        assert_eq!(cut.text(), "ha\nbe");
        assert!(cut.changed());
        assert_eq!(cut.cursor(), TextPosition::new(0, 3));
        assert_eq!(tab.cursor_position(), TextPosition::new(0, 3));
        assert_eq!(tab.text(), "alpta");
    }

    #[test]
    fn editor_tab_selection_delete_commands_update_cursor() {
        let mut tab = EditorTab::new(15);
        let _ = tab
            .try_insert(TextPosition::new(0, 0), "alpha\nbeta")
            .unwrap();

        let deleted = tab
            .try_delete_forward_selection(TextSelection::new(
                TextPosition::new(0, 3),
                TextPosition::new(1, 2),
            ))
            .unwrap();
        assert!(deleted.changed());
        assert_eq!(deleted.cursor(), TextPosition::new(0, 3));
        assert_eq!(tab.cursor_position(), TextPosition::new(0, 3));
        assert_eq!(tab.text(), "alpta");

        let backspace = tab
            .try_backspace_selection(TextSelection::caret(TextPosition::new(0, 2)))
            .unwrap();
        assert!(backspace.changed());
        assert_eq!(backspace.cursor(), TextPosition::new(0, 1));
        assert_eq!(tab.cursor_position(), TextPosition::new(0, 1));
        assert_eq!(tab.text(), "apta");
    }

    #[test]
    fn cancel_clear_dirty_after_open_preserves_real_edit() {
        let dir =
            std::env::temp_dir().join(format!("qem-editor-dirty-open-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("dirty-open.txt");
        fs::write(&path, b"alpha\n").unwrap();

        let mut tab = EditorTab::new(3);
        tab.open_file(path.clone()).unwrap();
        let _ = tab.document_mut().try_insert_text_at(0, 0, "X").unwrap();
        tab.cancel_clear_dirty_after_open();
        tab.after_text_edit_frame();

        assert!(tab.is_dirty());
        assert!(tab.text().starts_with('X'));

        let _ = fs::remove_file(&path);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn document_session_open_save_and_viewport_flow() {
        let dir = std::env::temp_dir().join(format!("qem-session-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let input = dir.join("input.txt");
        let output = dir.join("output.txt");
        fs::write(&input, b"alpha\nbeta\n").unwrap();

        let mut session = DocumentSession::new();
        session.open_file_async(input.clone()).unwrap();

        let loading = session
            .loading_state()
            .expect("session should expose loading progress");
        assert_eq!(loading.path(), input.as_path());
        assert_eq!(loading.total_bytes(), fs::metadata(&input).unwrap().len());
        assert!(matches!(
            session.background_activity(),
            BackgroundActivity::Loading(_)
        ));
        assert!(session.status().is_loading());

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(result) = session.poll_background_job() {
                result.unwrap();
                break;
            }
            assert!(Instant::now() < deadline, "document session load timed out");
            std::thread::sleep(Duration::from_millis(10));
        }

        assert_eq!(session.current_path(), Some(input.as_path()));
        let status = session.status();
        assert_eq!(status.generation(), session.generation());
        assert_eq!(status.path(), Some(input.as_path()));
        assert!(!status.is_dirty());
        assert_eq!(status.display_line_count(), 3);
        assert_eq!(status.exact_line_count(), Some(3));
        assert_eq!(status.file_len(), session.file_len());
        assert_eq!(status.line_ending(), session.line_ending());
        assert!(!status.is_busy());
        assert_eq!(
            session.file_len(),
            fs::metadata(&input).unwrap().len() as usize
        );
        assert_eq!(session.exact_line_count(), Some(3));
        assert_eq!(session.display_line_count(), 3);
        assert!(session.is_line_count_exact());
        assert_eq!(session.line_len_chars(1), 4);
        assert_eq!(session.position_for_char_index(6), TextPosition::new(1, 0));
        assert_eq!(session.char_index_for_position(TextPosition::new(1, 0)), 6);
        assert_eq!(
            session.text_units_between(TextPosition::new(0, 4), TextPosition::new(1, 2)),
            4
        );
        assert_eq!(
            session.edit_capability_at(TextPosition::new(0, 1)),
            EditCapability::RequiresPromotion {
                from: DocumentBacking::Mmap,
                to: DocumentBacking::Rope,
            }
        );
        let selection = TextSelection::new(TextPosition::new(1, 2), TextPosition::new(0, 4));
        let selected = session.read_selection(selection);
        assert!(selected.is_exact());
        assert_eq!(selected.text(), "a\nbe");
        let viewport = session.read_viewport(ViewportRequest::new(0, 2).with_columns(0, 16));
        assert_eq!(viewport.rows()[0].text(), "alpha");
        assert_eq!(viewport.rows()[1].text(), "beta");

        let cursor = session.try_replace_selection(selection, "Z").unwrap();
        assert_eq!(cursor, TextPosition::new(0, 5));
        assert_eq!(session.text(), "alphZta\n");

        let delete = session.try_delete_forward(TextPosition::new(0, 5)).unwrap();
        assert!(delete.changed());
        assert_eq!(delete.cursor(), TextPosition::new(0, 5));
        assert_eq!(session.text(), "alphZa\n");

        let cut = session
            .try_cut_selection(TextSelection::new(
                TextPosition::new(0, 4),
                TextPosition::new(0, 6),
            ))
            .unwrap();
        assert_eq!(cut.text(), "Za");
        assert!(cut.changed());
        assert_eq!(cut.cursor(), TextPosition::new(0, 4));
        assert_eq!(session.text(), "alph\n");

        let deleted = session
            .try_delete_forward_selection(TextSelection::new(
                TextPosition::new(0, 1),
                TextPosition::new(0, 3),
            ))
            .unwrap();
        assert!(deleted.changed());
        assert_eq!(deleted.cursor(), TextPosition::new(0, 1));
        assert_eq!(session.text(), "ah\n");

        let backspace = session
            .try_backspace_selection(TextSelection::caret(TextPosition::new(0, 1)))
            .unwrap();
        assert!(backspace.changed());
        assert_eq!(backspace.cursor(), TextPosition::new(0, 0));
        assert_eq!(session.text(), "h\n");

        let _ = session
            .try_insert(TextPosition::new(0, 0), "// inserted by session\n")
            .unwrap();
        assert!(session.is_dirty());
        assert!(session.save_as_async(output.clone()).unwrap());

        let saving = session
            .save_state()
            .expect("session should expose save progress");
        assert_eq!(saving.path(), output.as_path());
        assert!(matches!(
            session.background_activity(),
            BackgroundActivity::Saving(_)
        ));
        let saving_status = session.status();
        assert!(saving_status.is_saving());
        assert_eq!(
            saving_status
                .save_state()
                .expect("status should expose typed save progress")
                .path(),
            output.as_path()
        );

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(result) = session.poll_background_job() {
                result.unwrap();
                break;
            }
            assert!(Instant::now() < deadline, "document session save timed out");
            std::thread::sleep(Duration::from_millis(10));
        }

        assert!(!session.is_dirty());
        assert!(matches!(
            session.background_activity(),
            BackgroundActivity::Idle
        ));
        assert_eq!(session.current_path(), Some(output.as_path()));
        assert!(fs::read_to_string(&output)
            .unwrap()
            .starts_with("// inserted by session\nh\n"));

        let _ = fs::remove_file(&input);
        let _ = fs::remove_file(&output);
        let _ = fs::remove_dir_all(&dir);
    }
}
