use crate::document::SaveCompletion;
use crate::{Document, DocumentError};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
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
}

/// High-level save errors produced by the editor tab wrapper.
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

/// Lightweight editor-tab state with a document, cursor, and async save tracking.
#[derive(Debug)]
pub struct EditorTab {
    id: u64,
    doc: Document,
    generation: u64,
    cursor: CursorPosition,
    pinned: bool,
    save_job: Option<SaveJob>,
}

impl EditorTab {
    /// Creates a new empty tab with the provided identifier.
    pub fn new(id: u64) -> Self {
        Self {
            id,
            doc: Document::new(),
            generation: 0,
            cursor: CursorPosition::default(),
            pinned: false,
            save_job: None,
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
        self.generation
    }

    /// Returns `true` while a background save is in progress.
    pub fn is_saving(&self) -> bool {
        self.save_job.is_some()
    }

    /// Returns `true` while a background load is in progress.
    ///
    /// The current implementation always returns `false`.
    pub fn is_loading(&self) -> bool {
        false
    }

    /// Returns background-load progress.
    ///
    /// The current implementation always returns `None`.
    pub fn loading_progress(&self) -> Option<(u64, u64, PathBuf)> {
        None
    }

    /// Polls the background-load state.
    ///
    /// The current implementation always returns `None`.
    pub fn poll_load_job(&mut self) -> Option<Result<(), DocumentError>> {
        None
    }

    /// Returns immutable access to the tab document.
    pub fn document(&self) -> &Document {
        &self.doc
    }

    /// Returns mutable access to the tab document.
    pub fn document_mut(&mut self) -> &mut Document {
        &mut self.doc
    }

    /// Returns the full document text as a `String`.
    pub fn text(&self) -> String {
        self.doc.text_lossy()
    }

    /// Returns the current document path, if one is set.
    pub fn current_path(&self) -> Option<&Path> {
        self.doc.path()
    }

    /// Returns `true` if the document has unsaved changes.
    pub fn is_dirty(&self) -> bool {
        self.doc.is_dirty()
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

    /// Sets the cursor position using 1-based coordinates.
    pub fn set_cursor_line_col(&mut self, line: usize, column: usize) {
        self.cursor = CursorPosition::new(line, column);
    }

    /// Recomputes the cursor position from a character index in the full document text.
    pub fn update_cursor_char_index(&mut self, char_index: usize) {
        let (line0, col0) = self.doc.cursor_position_for_char_index(char_index);
        self.cursor = CursorPosition::new(line0 + 1, col0 + 1);
    }

    /// Opens a file in the tab and resets the cursor to the start of the document.
    ///
    /// # Errors
    /// Returns [`DocumentError`] if the file cannot be opened or if a save is
    /// already in progress for the current document.
    pub fn open_file(&mut self, path: PathBuf) -> Result<(), DocumentError> {
        if self.is_saving() {
            return Err(DocumentError::Write {
                path,
                source: io::Error::other("cannot open while save is in progress"),
            });
        }
        self.doc = Document::open(path)?;
        self.generation = self.generation.wrapping_add(1);
        self.cursor = CursorPosition::default();
        Ok(())
    }

    /// Closes the current document and replaces it with an empty one.
    pub fn close_file(&mut self) {
        self.save_job = None;
        self.doc = Document::new();
        self.generation = self.generation.wrapping_add(1);
        self.cursor = CursorPosition::default();
    }

    /// Reserved for post-edit-frame state synchronization.
    pub fn after_text_edit_frame(&mut self) {}

    /// Reserved for cancelling a deferred dirty-flag clear after open.
    pub fn cancel_clear_dirty_after_open(&mut self) {}

    /// Saves the document synchronously to its current path.
    ///
    /// # Errors
    /// Returns [`SaveError::NoPath`] if no path is set and [`SaveError::Io`] if
    /// the write operation fails.
    pub fn save(&mut self) -> Result<(), SaveError> {
        let Some(path) = self.current_path().map(|p| p.to_path_buf()) else {
            return Err(SaveError::NoPath);
        };
        if !self.doc.is_dirty() {
            return Ok(());
        }
        self.doc.save_to(&path).map_err(SaveError::Io)?;
        self.generation = self.generation.wrapping_add(1);
        Ok(())
    }

    /// Saves the document synchronously to a new path.
    ///
    /// # Errors
    /// Returns [`DocumentError`] if the write operation fails.
    pub fn save_as(&mut self, path: PathBuf) -> Result<(), DocumentError> {
        self.doc.save_to(&path)?;
        self.generation = self.generation.wrapping_add(1);
        Ok(())
    }

    /// Sets the document path without saving.
    pub fn set_path(&mut self, path: PathBuf) {
        self.doc.set_path(path);
    }

    /// Returns background-save progress in bytes together with the destination path.
    pub fn save_progress(&self) -> Option<(u64, u64, PathBuf)> {
        let job = self.save_job.as_ref()?;
        Some((
            job.written_bytes
                .load(Ordering::Relaxed)
                .min(job.total_bytes),
            job.total_bytes,
            job.path.clone(),
        ))
    }

    /// Polls the background-save state and applies the completed result.
    ///
    /// Returns `None` if no save has been started or if the job is still running.
    pub fn poll_save_job(&mut self) -> Option<Result<(), DocumentError>> {
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

    /// Starts a background save to the current path.
    ///
    /// Returns `Ok(false)` when the document is unchanged and no save is needed.
    ///
    /// # Errors
    /// Returns [`SaveError::NoPath`] if no path is set,
    /// [`SaveError::InProgress`] if a save is already running, and
    /// [`SaveError::Io`] if save preparation fails.
    pub fn save_async(&mut self) -> Result<bool, SaveError> {
        if self.is_saving() {
            return Err(SaveError::InProgress);
        }
        let Some(path) = self.current_path().map(|p| p.to_path_buf()) else {
            return Err(SaveError::NoPath);
        };
        self.save_to_async(path).map_err(SaveError::Io)
    }

    /// Starts a background save to a new path.
    ///
    /// Returns `Ok(false)` when the document is unchanged and is already bound
    /// to the same path.
    ///
    /// # Errors
    /// Returns [`DocumentError`] if save preparation fails.
    pub fn save_as_async(&mut self, path: PathBuf) -> Result<bool, DocumentError> {
        self.save_to_async(path)
    }

    fn save_to_async(&mut self, path: PathBuf) -> Result<bool, DocumentError> {
        if self.is_saving() {
            return Err(DocumentError::Write {
                path,
                source: io::Error::other("save already in progress"),
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

#[cfg(test)]
mod tests {
    use super::EditorTab;
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

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(result) = tab.poll_save_job() {
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
}
