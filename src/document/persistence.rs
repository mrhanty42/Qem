use super::*;
use std::io::Write;

#[derive(Debug, Clone)]
struct PieceTableSnapshot {
    original: FileStorage,
    add: Vec<u8>,
    pieces: Vec<Piece>,
}

impl PieceTableSnapshot {
    fn from_piece_table(piece_table: &PieceTable) -> Self {
        Self {
            original: piece_table.original.clone(),
            add: piece_table.add.clone(),
            pieces: piece_table.pieces.to_vec(),
        }
    }

    fn source_bytes(&self, src: PieceSource) -> &[u8] {
        match src {
            PieceSource::Original => self.original.read_range(0, self.original.len()),
            PieceSource::Add => &self.add,
        }
    }

    fn write_to(
        &self,
        out: &mut impl Write,
        written: &Arc<AtomicU64>,
        total: u64,
    ) -> io::Result<()> {
        let mut done = 0u64;
        for piece in &self.pieces {
            let src = self.source_bytes(piece.src);
            let mut start = piece.start;
            let end = piece.start + piece.len;
            while start < end {
                let chunk_end = start.saturating_add(SAVE_STREAM_CHUNK_BYTES).min(end);
                out.write_all(&src[start..chunk_end])?;
                done = done.saturating_add((chunk_end - start) as u64).min(total);
                written.store(done, Ordering::Relaxed);
                start = chunk_end;
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
enum SaveSnapshot {
    Empty,
    Mmap(FileStorage),
    Rope { rope: Rope, line_ending: LineEnding },
    PieceTable(PieceTableSnapshot),
}

#[derive(Debug, Clone)]
pub(crate) struct PreparedSave {
    path: PathBuf,
    total_bytes: u64,
    reload_after_save: bool,
    snapshot: SaveSnapshot,
}

#[derive(Debug)]
pub(crate) struct SaveCompletion {
    pub path: PathBuf,
    pub reload_after_save: bool,
}

impl PreparedSave {
    #[cfg(feature = "editor")]
    pub(crate) fn total_bytes(&self) -> u64 {
        self.total_bytes
    }

    pub(crate) fn execute(self, written: Arc<AtomicU64>) -> Result<SaveCompletion, DocumentError> {
        let path = self.path.clone();
        let total = self.total_bytes;
        let snapshot = self.snapshot;
        let written_for_io = Arc::clone(&written);
        FileStorage::replace_with(&path, move |file| {
            write_snapshot(file, &snapshot, &written_for_io, total)
        })
        .map_err(|source| DocumentError::Write {
            path: path.clone(),
            source,
        })?;

        written.store(total, Ordering::Relaxed);
        Ok(SaveCompletion {
            path,
            reload_after_save: self.reload_after_save,
        })
    }
}

fn write_snapshot(
    out: &mut impl Write,
    snapshot: &SaveSnapshot,
    written: &Arc<AtomicU64>,
    total: u64,
) -> io::Result<()> {
    match snapshot {
        SaveSnapshot::Empty => Ok(()),
        SaveSnapshot::Mmap(storage) => {
            write_bytes_chunked(out, storage.read_range(0, storage.len()), written, total)
        }
        SaveSnapshot::Rope { rope, line_ending } => {
            write_rope_snapshot(out, rope, *line_ending, written, total)
        }
        SaveSnapshot::PieceTable(piece_table) => piece_table.write_to(out, written, total),
    }
}

fn write_rope_snapshot(
    out: &mut impl Write,
    rope: &Rope,
    line_ending: LineEnding,
    written: &Arc<AtomicU64>,
    total: u64,
) -> io::Result<()> {
    if line_ending == LineEnding::Lf {
        let mut done = 0u64;
        for chunk in rope.chunks() {
            let bytes = chunk.as_bytes();
            out.write_all(bytes)?;
            done = done.saturating_add(bytes.len() as u64).min(total);
            written.store(done, Ordering::Relaxed);
        }
        return Ok(());
    }

    let newline = line_ending.as_str().as_bytes();
    let mut done = 0u64;
    for chunk in rope.chunks() {
        let mut start = 0usize;
        for (idx, ch) in chunk.char_indices() {
            if ch != '\n' {
                continue;
            }
            if start < idx {
                let bytes = &chunk.as_bytes()[start..idx];
                out.write_all(bytes)?;
                done = done.saturating_add(bytes.len() as u64).min(total);
                written.store(done, Ordering::Relaxed);
            }
            out.write_all(newline)?;
            done = done.saturating_add(newline.len() as u64).min(total);
            written.store(done, Ordering::Relaxed);
            start = idx + ch.len_utf8();
        }
        if start < chunk.len() {
            let bytes = &chunk.as_bytes()[start..];
            out.write_all(bytes)?;
            done = done.saturating_add(bytes.len() as u64).min(total);
            written.store(done, Ordering::Relaxed);
        }
    }
    Ok(())
}

fn write_bytes_chunked(
    out: &mut impl Write,
    bytes: &[u8],
    written: &Arc<AtomicU64>,
    total: u64,
) -> io::Result<()> {
    let mut done = 0u64;
    for chunk in bytes.chunks(SAVE_STREAM_CHUNK_BYTES.max(1)) {
        out.write_all(chunk)?;
        done = done.saturating_add(chunk.len() as u64).min(total);
        written.store(done, Ordering::Relaxed);
    }
    Ok(())
}

pub(super) fn clear_session_sidecar(path: &Path) {
    let sidecar = editlog_path(path);
    let _ = std::fs::remove_file(sidecar);
}

impl Document {
    /// Forces the current sidecar session state to disk.
    ///
    /// For mmap- or rope-backed documents without a piece-tree session, this is
    /// a no-op.
    ///
    /// # Errors
    /// Returns [`DocumentError`] if `.qem.editlog` cannot be committed.
    pub fn flush_session(&mut self) -> Result<(), DocumentError> {
        let doc_path = self.path.clone();
        let Some(piece_table) = self.piece_table.as_mut() else {
            return Ok(());
        };
        let path = session_sidecar_path(doc_path.as_deref(), piece_table.original.path());
        piece_table
            .flush_session()
            .map_err(|source| DocumentError::Write { path, source })
    }

    /// Restores the document to the previous persisted piece-tree root snapshot.
    pub fn try_undo(&mut self) -> Result<bool, DocumentError> {
        let doc_path = self.path.clone();
        let Some(piece_table) = self.piece_table.as_mut() else {
            return Ok(false);
        };
        let path = session_sidecar_path(doc_path.as_deref(), piece_table.original.path());
        match piece_table.undo() {
            Ok(false) => Ok(false),
            Ok(true) => {
                self.dirty = true;
                Ok(true)
            }
            Err(source) => {
                self.dirty = true;
                Err(DocumentError::Write { path, source })
            }
        }
    }

    /// Rolls the document back to the previous persisted edit snapshot.
    #[doc(hidden)]
    #[deprecated(since = "0.3.0", note = "use try_undo() for explicit error handling")]
    pub fn undo(&mut self) -> bool {
        self.try_undo().unwrap_or(false)
    }

    /// Reapplies the next change from persistent history.
    pub fn try_redo(&mut self) -> Result<bool, DocumentError> {
        let doc_path = self.path.clone();
        let Some(piece_table) = self.piece_table.as_mut() else {
            return Ok(false);
        };
        let path = session_sidecar_path(doc_path.as_deref(), piece_table.original.path());
        match piece_table.redo() {
            Ok(false) => Ok(false),
            Ok(true) => {
                self.dirty = true;
                Ok(true)
            }
            Err(source) => {
                self.dirty = true;
                Err(DocumentError::Write { path, source })
            }
        }
    }

    /// Reapplies the next persisted edit snapshot.
    #[doc(hidden)]
    #[deprecated(since = "0.3.0", note = "use try_redo() for explicit error handling")]
    pub fn redo(&mut self) -> bool {
        self.try_redo().unwrap_or(false)
    }

    pub(crate) fn prepare_save(&self, path: &Path) -> PreparedSave {
        let snapshot = if let Some(piece_table) = self.piece_table.as_ref() {
            SaveSnapshot::PieceTable(PieceTableSnapshot::from_piece_table(piece_table))
        } else if let Some(rope) = self.rope.as_ref() {
            SaveSnapshot::Rope {
                rope: rope.clone(),
                line_ending: self.line_ending,
            }
        } else if let Some(storage) = self.storage.as_ref() {
            SaveSnapshot::Mmap(storage.clone())
        } else {
            SaveSnapshot::Empty
        };

        PreparedSave {
            path: path.to_path_buf(),
            total_bytes: self.file_len() as u64,
            reload_after_save: !self.has_edit_buffer(),
            snapshot,
        }
    }

    pub(crate) fn finish_save(
        &mut self,
        path: PathBuf,
        reload_after_save: bool,
    ) -> Result<(), DocumentError> {
        let previous_path = self.path.clone();
        self.indexing.store(false, Ordering::Relaxed);
        if !reload_after_save {
            if let Some(old_path) = previous_path.as_deref() {
                clear_session_sidecar(old_path);
            }
            clear_session_sidecar(&path);
            self.path = Some(path);
            self.dirty = false;
            return Ok(());
        }

        let fresh_storage = FileStorage::open(&path).map_err(|err| match err {
            StorageOpenError::Open(source) => DocumentError::Open {
                path: path.clone(),
                source,
            },
            StorageOpenError::Map(source) => DocumentError::Map {
                path: path.clone(),
                source,
            },
        })?;
        if let Some(old_path) = previous_path.as_deref() {
            clear_session_sidecar(old_path);
        }
        clear_session_sidecar(&path);
        *self = Self::from_storage(path, fresh_storage);
        Ok(())
    }

    /// Saves the document to the specified path.
    ///
    /// The write is streamed through a temporary file and committed with an
    /// atomic replacement.
    ///
    /// # Errors
    /// Returns [`DocumentError`] if the file cannot be written, renamed, or
    /// reopened after the save completes.
    pub fn save_to(&mut self, path: &Path) -> Result<(), DocumentError> {
        let prepared = self.prepare_save(path);
        let completion = prepared.execute(Arc::new(AtomicU64::new(0)))?;
        self.finish_save(completion.path, completion.reload_after_save)
    }
}
