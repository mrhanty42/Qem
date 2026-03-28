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
enum SaveRebaseHint {
    PieceTable {
        snapshot_pieces: Vec<Piece>,
        snapshot_add_len: usize,
    },
}

#[derive(Debug, Clone)]
enum SaveSnapshot {
    Empty,
    Bytes(Vec<u8>),
    Mmap(FileStorage),
    Rope { rope: Rope, line_ending: LineEnding },
    PieceTable(PieceTableSnapshot),
}

#[derive(Debug, Clone)]
pub(crate) struct PreparedSave {
    path: PathBuf,
    total_bytes: u64,
    reload_after_save: bool,
    encoding: DocumentEncoding,
    encoding_origin: DocumentEncodingOrigin,
    snapshot: SaveSnapshot,
}

#[derive(Debug)]
pub(crate) struct SaveCompletion {
    pub path: PathBuf,
    pub reload_after_save: bool,
    pub encoding: DocumentEncoding,
    pub encoding_origin: DocumentEncodingOrigin,
    rebase_hint: Option<SaveRebaseHint>,
}

impl PreparedSave {
    #[cfg(feature = "editor")]
    pub(crate) fn total_bytes(&self) -> u64 {
        self.total_bytes
    }

    pub(crate) fn execute(self, written: Arc<AtomicU64>) -> Result<SaveCompletion, DocumentError> {
        let path = self.path.clone();
        let total = self.total_bytes;
        let rebase_hint = match &self.snapshot {
            SaveSnapshot::PieceTable(piece_table) => Some(SaveRebaseHint::PieceTable {
                snapshot_pieces: piece_table.pieces.clone(),
                snapshot_add_len: piece_table.add.len(),
            }),
            _ => None,
        };
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
            encoding: self.encoding,
            encoding_origin: self.encoding_origin,
            rebase_hint,
        })
    }
}

#[derive(Clone, Copy, Debug)]
struct SavedSourceSpan {
    src: PieceSource,
    src_start: usize,
    src_end: usize,
    saved_start: usize,
}

fn saved_source_spans(snapshot_pieces: &[Piece]) -> Vec<SavedSourceSpan> {
    let mut saved_start = 0usize;
    let mut spans = Vec::with_capacity(snapshot_pieces.len());
    for piece in snapshot_pieces {
        spans.push(SavedSourceSpan {
            src: piece.src,
            src_start: piece.start,
            src_end: piece.start.saturating_add(piece.len),
            saved_start,
        });
        saved_start = saved_start.saturating_add(piece.len);
    }
    spans
}

fn next_saved_source_start(
    spans: &[SavedSourceSpan],
    src: PieceSource,
    after: usize,
) -> Option<usize> {
    spans
        .iter()
        .filter(|span| span.src == src && span.src_start > after)
        .map(|span| span.src_start)
        .min()
}

fn remap_discarded_history_piece(
    piece: Piece,
    spans: &[SavedSourceSpan],
    saved_bytes: &[u8],
    old_original: &FileStorage,
    add_bytes: &[u8],
    rebased_add: &mut Vec<u8>,
) -> io::Result<Vec<Piece>> {
    let mut remapped = Vec::new();
    let piece_end = piece.start.saturating_add(piece.len);
    let mut cursor = piece.start;

    while cursor < piece_end {
        if let Some(span) = spans
            .iter()
            .find(|span| span.src == piece.src && span.src_start <= cursor && cursor < span.src_end)
        {
            let overlap_end = piece_end.min(span.src_end);
            let saved_start = span
                .saved_start
                .saturating_add(cursor.saturating_sub(span.src_start));
            let len = overlap_end.saturating_sub(cursor);
            let saved_end = saved_start.saturating_add(len);
            remapped.push(Piece {
                src: PieceSource::Original,
                start: saved_start,
                len,
                line_breaks: count_line_breaks_in_bytes(&saved_bytes[saved_start..saved_end]),
            });
            cursor = overlap_end;
            continue;
        }

        let gap_end = next_saved_source_start(spans, piece.src, cursor)
            .unwrap_or(piece_end)
            .min(piece_end);
        if gap_end <= cursor {
            return Err(io::Error::other(
                "discarded save rebase could not advance history remap cursor",
            ));
        }

        match piece.src {
            PieceSource::Original => {
                let bytes = old_original.read_range(cursor, gap_end);
                if bytes.len() != gap_end.saturating_sub(cursor) {
                    return Err(io::Error::other(
                        "discarded save rebase original slice exceeded source bounds",
                    ));
                }
                let add_start = rebased_add.len();
                rebased_add.extend_from_slice(bytes);
                remapped.push(Piece {
                    src: PieceSource::Add,
                    start: add_start,
                    len: bytes.len(),
                    line_breaks: count_line_breaks_in_bytes(bytes),
                });
            }
            PieceSource::Add => {
                let bytes = add_bytes.get(cursor..gap_end).ok_or_else(|| {
                    io::Error::other("discarded save rebase add slice exceeded buffer bounds")
                })?;
                remapped.push(Piece {
                    src: PieceSource::Add,
                    start: cursor,
                    len: gap_end.saturating_sub(cursor),
                    line_breaks: count_line_breaks_in_bytes(bytes),
                });
            }
        }

        cursor = gap_end;
    }

    Ok(remapped)
}

fn write_snapshot(
    out: &mut impl Write,
    snapshot: &SaveSnapshot,
    written: &Arc<AtomicU64>,
    total: u64,
) -> io::Result<()> {
    match snapshot {
        SaveSnapshot::Empty => Ok(()),
        SaveSnapshot::Bytes(bytes) => write_bytes_chunked(out, bytes, written, total),
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

fn save_encoding_error(
    path: &Path,
    operation: &'static str,
    encoding: DocumentEncoding,
    reason: DocumentEncodingErrorKind,
) -> DocumentError {
    DocumentError::Encoding {
        path: path.to_path_buf(),
        operation,
        encoding,
        reason,
    }
}

pub(super) fn clear_session_sidecar(path: &Path) {
    let sidecar = editlog_path(path);
    let _ = std::fs::remove_file(sidecar);
}

impl Document {
    pub(crate) fn handle_discarded_save_completion(&mut self, completion: &SaveCompletion) {
        let Some(path) = self.path.as_deref() else {
            return;
        };
        if path != completion.path.as_path() {
            return;
        }

        let Some(piece_table) = self.piece_table.as_mut() else {
            return;
        };
        let Some(SaveRebaseHint::PieceTable {
            snapshot_pieces,
            snapshot_add_len: _snapshot_add_len,
        }) = completion.rebase_hint.as_ref()
        else {
            return;
        };

        let new_storage = match FileStorage::open(path) {
            Ok(storage) => storage,
            Err(_) => {
                piece_table.pieces.detach_persistence();
                clear_session_sidecar(path);
                return;
            }
        };
        let saved_storage = new_storage.clone();
        let saved_bytes = saved_storage.read_range(0, saved_storage.len());
        let spans = saved_source_spans(snapshot_pieces);
        let old_original = piece_table.original.clone();
        let add_bytes = piece_table.add.clone();
        let mut rebased_add = add_bytes.clone();
        let session_meta = piece_table.session_meta();
        let remap_result = piece_table
            .pieces
            .rebuild_history_roots_disk(path, |piece| {
                remap_discarded_history_piece(
                    piece,
                    &spans,
                    saved_bytes,
                    &old_original,
                    &add_bytes,
                    &mut rebased_add,
                )
            });
        let Ok(mut rebuilt_pieces) = remap_result else {
            piece_table.pieces.detach_persistence();
            clear_session_sidecar(path);
            return;
        };
        if rebuilt_pieces
            .flush_session(&rebased_add, session_meta)
            .is_err()
        {
            piece_table.pieces.detach_persistence();
            clear_session_sidecar(path);
            return;
        }
        piece_table.pieces = rebuilt_pieces;
        piece_table.original = new_storage;
        piece_table.add = rebased_add;
        piece_table.pending_session_flush = false;
        piece_table.pending_session_edits = 0;
        piece_table.last_session_flush = Some(Instant::now());
    }

    fn rendered_text_for_save(&self) -> String {
        if let Some(rope) = &self.rope {
            return rope_text_with_line_endings(rope, self.line_ending);
        }
        if let Some(piece_table) = &self.piece_table {
            return piece_table.to_string_lossy();
        }
        String::from_utf8_lossy(self.mmap_bytes()).to_string()
    }

    fn encoded_save_bytes(
        &self,
        path: &Path,
        encoding: DocumentEncoding,
    ) -> Result<Vec<u8>, DocumentError> {
        let rendered = self.rendered_text_for_save();
        encode_text_with_encoding(&rendered, encoding)
            .map_err(|reason| save_encoding_error(path, "save", encoding, reason))
    }

    /// Forces the current sidecar session state to disk.
    ///
    /// For mmap- or rope-backed documents without a piece-tree session, this is
    /// a no-op.
    ///
    /// The `.qem.editlog` sidecar is an internal durability/recovery format:
    /// Qem writes append-only pages first and then rewrites the fixed header as
    /// the authoritative commit record for the latest session snapshot. Older
    /// pages may remain in the sidecar after newer flushes, but they become
    /// unreachable once the header advances.
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
                self.mark_dirty();
                Ok(true)
            }
            Err(source) => {
                self.mark_dirty();
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
                self.mark_dirty();
                Ok(true)
            }
            Err(source) => {
                self.mark_dirty();
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

    fn maybe_force_compact_before_save_with_policy(
        &mut self,
        policy: CompactionPolicy,
    ) -> Result<bool, DocumentError> {
        let Some(recommendation) = self.compaction_recommendation_with_policy(policy) else {
            return Ok(false);
        };
        if recommendation.urgency() != CompactionUrgency::Forced {
            return Ok(false);
        }

        let doc_path = self.path.clone();
        let sidecar_path = self.piece_table.as_ref().map(|piece_table| {
            session_sidecar_path(doc_path.as_deref(), piece_table.original.path())
        });
        match self.compact_piece_table() {
            Ok(compacted) => Ok(compacted),
            Err(source) => Err(DocumentError::Write {
                path: sidecar_path.unwrap_or_else(|| {
                    doc_path.unwrap_or_else(|| PathBuf::from("<session-sidecar>"))
                }),
                source,
            }),
        }
    }

    pub(crate) fn prepare_save_with_policy(
        &mut self,
        path: &Path,
        compaction_policy: Option<CompactionPolicy>,
    ) -> Result<PreparedSave, DocumentError> {
        self.prepare_save_with_options_and_policy(
            path,
            DocumentSaveOptions::new(),
            compaction_policy,
        )
    }

    pub(crate) fn prepare_save_with_options_and_policy(
        &mut self,
        path: &Path,
        options: DocumentSaveOptions,
        compaction_policy: Option<CompactionPolicy>,
    ) -> Result<PreparedSave, DocumentError> {
        let (encoding, encoding_origin, explicit_conversion) = match options.encoding_policy() {
            SaveEncodingPolicy::Preserve => {
                if !self.encoding.can_roundtrip_save() {
                    return Err(save_encoding_error(
                        path,
                        "save",
                        self.encoding,
                        DocumentEncodingErrorKind::PreserveSaveUnsupported,
                    ));
                }
                if self.preserve_save_materializes_lossy_decoded_text() {
                    return Err(save_encoding_error(
                        path,
                        "save",
                        self.encoding,
                        DocumentEncodingErrorKind::LossyDecodedPreserve,
                    ));
                }
                (self.encoding, self.encoding_origin, false)
            }
            SaveEncodingPolicy::Convert(encoding) => {
                (encoding, DocumentEncodingOrigin::SaveConversion, true)
            }
        };
        self.prepare_save_with_encoding_and_policy(
            path,
            encoding,
            encoding_origin,
            explicit_conversion,
            compaction_policy,
        )
    }

    pub(crate) fn prepare_save_with_encoding_and_policy(
        &mut self,
        path: &Path,
        encoding: DocumentEncoding,
        encoding_origin: DocumentEncodingOrigin,
        explicit_conversion: bool,
        compaction_policy: Option<CompactionPolicy>,
    ) -> Result<PreparedSave, DocumentError> {
        if let Some(policy) = compaction_policy {
            self.maybe_force_compact_before_save_with_policy(policy)?;
        }

        let snapshot = if !explicit_conversion && encoding.is_utf8() && self.encoding.is_utf8() {
            if let Some(piece_table) = self.piece_table.as_ref() {
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
            }
        } else {
            SaveSnapshot::Bytes(self.encoded_save_bytes(path, encoding)?)
        };

        let total_bytes = match &snapshot {
            SaveSnapshot::Empty => 0,
            SaveSnapshot::Bytes(bytes) => bytes.len() as u64,
            _ => self.file_len() as u64,
        };

        let reload_after_save = !self.has_edit_buffer() || self.has_piece_table();
        if reload_after_save && !encoding.is_utf8() && total_bytes > MAX_ROPE_EDIT_FILE_BYTES as u64
        {
            return Err(save_encoding_error(
                path,
                "save",
                encoding,
                DocumentEncodingErrorKind::SaveReopenTooLarge {
                    max_bytes: MAX_ROPE_EDIT_FILE_BYTES,
                },
            ));
        }

        Ok(PreparedSave {
            path: path.to_path_buf(),
            total_bytes,
            reload_after_save,
            encoding,
            encoding_origin,
            snapshot,
        })
    }

    pub(crate) fn prepare_save(&mut self, path: &Path) -> Result<PreparedSave, DocumentError> {
        self.prepare_save_with_policy(path, Some(CompactionPolicy::default()))
    }

    pub(crate) fn finish_save(
        &mut self,
        path: PathBuf,
        reload_after_save: bool,
        encoding: DocumentEncoding,
        encoding_origin: DocumentEncodingOrigin,
    ) -> Result<(), DocumentError> {
        let previous_path = self.path.clone();
        self.indexing.store(false, Ordering::Relaxed);
        if !reload_after_save {
            if let Some(old_path) = previous_path.as_deref() {
                clear_session_sidecar(old_path);
            }
            clear_session_sidecar(&path);
            self.path = Some(path);
            self.encoding = encoding;
            self.encoding_origin = encoding_origin;
            self.decoding_had_errors = false;
            self.mark_clean();
            return Ok(());
        }

        let same_path_sidecar_backup = previous_path
            .as_deref()
            .filter(|old_path| *old_path == path.as_path())
            .and_then(|old_path| std::fs::read(editlog_path(old_path)).ok());
        clear_session_sidecar(&path);
        let reopen_path = path.clone();
        let reopened =
            match Self::reopen_with_encoding_contract(reopen_path, encoding, encoding_origin) {
                Ok(doc) => doc,
                Err(err) => {
                    if let Some(sidecar_bytes) = same_path_sidecar_backup {
                        let _ = std::fs::write(editlog_path(&path), sidecar_bytes);
                    }
                    return Err(err);
                }
            };
        if let Some(old_path) = previous_path.as_deref() {
            if old_path != path.as_path() {
                clear_session_sidecar(old_path);
            }
        }
        *self = reopened;
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
        let prepared = self.prepare_save(path)?;
        let completion = prepared.execute(Arc::new(AtomicU64::new(0)))?;
        self.finish_save(
            completion.path,
            completion.reload_after_save,
            completion.encoding,
            completion.encoding_origin,
        )
    }

    /// Saves the document to the specified path using explicit save options.
    pub fn save_to_with_options(
        &mut self,
        path: &Path,
        options: DocumentSaveOptions,
    ) -> Result<(), DocumentError> {
        let prepared = self.prepare_save_with_options_and_policy(
            path,
            options,
            Some(CompactionPolicy::default()),
        )?;
        let completion = prepared.execute(Arc::new(AtomicU64::new(0)))?;
        self.finish_save(
            completion.path,
            completion.reload_after_save,
            completion.encoding,
            completion.encoding_origin,
        )
    }

    /// Saves the document to the specified path using an explicit target encoding.
    pub fn save_to_with_encoding(
        &mut self,
        path: &Path,
        encoding: DocumentEncoding,
    ) -> Result<(), DocumentError> {
        self.save_to_with_options(path, DocumentSaveOptions::new().with_encoding(encoding))
    }
}
