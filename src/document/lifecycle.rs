use super::*;

fn ignore_open_progress(_: u64) {}
fn ignore_open_phase(_: OpenProgressPhase) {}

fn should_drop_invalid_session_sidecar(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::InvalidData | io::ErrorKind::UnexpectedEof
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OpenProgressPhase {
    OpeningStorage,
    InspectingSource,
    PreparingIndex,
    RecoveringSession,
    Ready,
}

fn line_ending_probe_bytes(bytes: &[u8]) -> usize {
    let Some(pos) = memchr::memchr2(b'\n', b'\r', bytes) else {
        return bytes.len();
    };
    pos.saturating_add(2).min(bytes.len())
}

struct OpenProgressTracker<'a> {
    total_bytes: u64,
    reported_bytes: u64,
    callback: &'a mut dyn FnMut(u64),
}

impl<'a> OpenProgressTracker<'a> {
    fn new(total_bytes: u64, callback: &'a mut dyn FnMut(u64)) -> Self {
        Self {
            total_bytes,
            reported_bytes: 0,
            callback,
        }
    }

    fn report_inspected(&mut self, inspected_bytes: usize) {
        let completed = (inspected_bytes as u64).min(self.total_bytes);
        if completed <= self.reported_bytes {
            return;
        }
        self.reported_bytes = completed;
        (self.callback)(completed);
    }

    fn complete(&mut self) {
        self.report_inspected(self.total_bytes as usize);
    }
}

impl Default for Document {
    fn default() -> Self {
        Self::new()
    }
}

impl Document {
    /// Creates an empty in-memory document with no backing file.
    ///
    /// This is the lower-level entry point. Most frontends should start with
    /// [`crate::DocumentSession::new`] unless they intentionally manage their
    /// own session and background-job lifecycle.
    pub fn new() -> Self {
        Self {
            path: None,
            storage: None,
            line_offsets: Arc::new(RwLock::new(LineOffsets::default())),
            disk_index: None,
            indexing: Arc::new(AtomicBool::new(false)),
            indexing_started: None,
            file_len: 0,
            indexed_bytes: Arc::new(AtomicUsize::new(0)),
            avg_line_len: Arc::new(AtomicUsize::new(AVG_LINE_LEN_ESTIMATE)),
            line_ending: LineEnding::Lf,
            rope: None,
            piece_table: None,
            dirty: false,
        }
    }

    /// Opens a file and constructs a memory-mapped document.
    ///
    /// This is the synchronous lower-level open path. Most responsive
    /// frontends should prefer [`crate::DocumentSession::open_file_async`] so
    /// open progress and session lifecycle stay explicit.
    ///
    /// # Errors
    /// Returns [`DocumentError`] if the file cannot be opened or mapped.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, DocumentError> {
        Self::open_with_progress(path, |_| {})
    }

    pub(crate) fn open_with_progress(
        path: impl Into<PathBuf>,
        mut progress: impl FnMut(u64),
    ) -> Result<Self, DocumentError> {
        Self::open_with_reporting(path, &mut progress, &mut ignore_open_phase)
    }

    pub(crate) fn open_with_reporting(
        path: impl Into<PathBuf>,
        mut progress: impl FnMut(u64),
        phase: &mut dyn FnMut(OpenProgressPhase),
    ) -> Result<Self, DocumentError> {
        let path = path.into();
        phase(OpenProgressPhase::OpeningStorage);
        let storage = FileStorage::open(&path).map_err(|err| match err {
            StorageOpenError::Open(source) => DocumentError::Open {
                path: path.clone(),
                source,
            },
            StorageOpenError::Map(source) => DocumentError::Map {
                path: path.clone(),
                source,
            },
        })?;

        let mut tracker = OpenProgressTracker::new(storage.len() as u64, &mut progress);
        let doc = Self::from_storage_with_progress(path, storage, &mut tracker, phase);
        phase(OpenProgressPhase::Ready);
        tracker.complete();
        Ok(doc)
    }

    pub(super) fn from_storage(path: PathBuf, storage: FileStorage) -> Self {
        let total_bytes = storage.len() as u64;
        let mut progress = ignore_open_progress as fn(u64);
        let mut tracker = OpenProgressTracker::new(total_bytes, &mut progress);
        Self::from_storage_with_progress(path, storage, &mut tracker, &mut ignore_open_phase)
    }

    fn from_storage_with_progress(
        path: PathBuf,
        storage: FileStorage,
        progress: &mut OpenProgressTracker<'_>,
        phase: &mut dyn FnMut(OpenProgressPhase),
    ) -> Self {
        let file_len = storage.len();
        let mut inspected_source_bytes = 0usize;
        phase(OpenProgressPhase::InspectingSource);
        let inline_analysis =
            (file_len > 0 && file_len <= INLINE_FULL_INDEX_MAX_FILE_BYTES).then(|| {
                let analysis = analyze_inline_open(storage.bytes());
                inspected_source_bytes = file_len;
                progress.report_inspected(inspected_source_bytes);
                analysis
            });
        let line_ending = inline_analysis
            .as_ref()
            .map(|analysis| analysis.line_ending)
            .unwrap_or_else(|| {
                let bytes = storage.bytes();
                inspected_source_bytes = line_ending_probe_bytes(bytes);
                progress.report_inspected(inspected_source_bytes);
                detect_line_ending(bytes)
            });
        phase(OpenProgressPhase::PreparingIndex);
        let disk_index = DiskLineIndex::open_or_build(&path, &storage);
        if disk_index.is_some() {
            inspected_source_bytes = inspected_source_bytes.saturating_add(
                crate::source_identity::sampled_content_fingerprint_budget(file_len),
            );
            progress.report_inspected(inspected_source_bytes);
        }
        let indexing = Arc::new(AtomicBool::new(true));
        let indexing_started = Instant::now();
        let indexed_bytes = Arc::new(AtomicUsize::new(0));
        let avg_line_len = Arc::new(AtomicUsize::new(AVG_LINE_LEN_ESTIMATE));
        let use_u32_offsets = file_len <= u32::MAX as usize;
        let new_line_offsets = || Arc::new(RwLock::new(LineOffsets::new_for_file_len(file_len)));

        // Persistent piece-table sessions are only created for large documents,
        // so skipping this sidecar probe avoids useless I/O on small-file opens.
        if file_len >= PIECE_TREE_DISK_MIN_BYTES {
            phase(OpenProgressPhase::RecoveringSession);
            inspected_source_bytes = inspected_source_bytes.saturating_add(
                crate::source_identity::sampled_content_fingerprint_budget(file_len),
            );
            match PieceTree::try_open_disk_session(&path, &storage) {
                Ok(Some((pieces, add, meta))) => {
                    progress.report_inspected(inspected_source_bytes);
                    indexing.store(false, Ordering::Relaxed);
                    indexed_bytes.store(file_len, Ordering::Relaxed);
                    return Self {
                        path: Some(path),
                        storage: Some(storage.clone()),
                        line_offsets: new_line_offsets(),
                        disk_index,
                        indexing,
                        indexing_started: Some(indexing_started),
                        file_len,
                        indexed_bytes,
                        avg_line_len,
                        line_ending,
                        rope: None,
                        piece_table: Some(PieceTable::from_recovered_session(
                            storage, add, pieces, meta,
                        )),
                        dirty: true,
                    };
                }
                Ok(None) => {}
                Err(err) => {
                    if should_drop_invalid_session_sidecar(&err) {
                        super::persistence::clear_session_sidecar(&path);
                    }
                }
            }
            progress.report_inspected(inspected_source_bytes);
        }

        if file_len == 0 {
            indexing.store(false, Ordering::Relaxed);
            return Self {
                path: Some(path),
                storage: Some(storage),
                line_offsets: new_line_offsets(),
                disk_index,
                indexing,
                indexing_started: Some(indexing_started),
                file_len,
                indexed_bytes,
                avg_line_len,
                line_ending,
                rope: Some(Rope::new()),
                piece_table: None,
                dirty: false,
            };
        }

        if let Some(inline_analysis) = inline_analysis {
            indexing.store(false, Ordering::Relaxed);
            indexed_bytes.store(file_len, Ordering::Relaxed);
            avg_line_len.store(inline_analysis.avg_line_len, Ordering::Relaxed);
            return Self {
                path: Some(path),
                storage: Some(storage),
                line_offsets: Arc::new(RwLock::new(inline_analysis.line_offsets)),
                disk_index,
                indexing,
                indexing_started: Some(indexing_started),
                file_len,
                indexed_bytes,
                avg_line_len,
                line_ending,
                rope: None,
                piece_table: None,
                dirty: false,
            };
        }

        // Scanner thread: finds line break offsets, sends them without touching shared state.
        // Pusher thread: receives chunks and pushes to the shared vector under a write lock.
        let line_offsets = new_line_offsets();
        let (tx, rx) = mpsc::channel::<OffsetsChunk>();
        let storage_scanner = storage.clone();
        let indexed_bytes_scanner = indexed_bytes.clone();
        let avg_line_len_scanner = avg_line_len.clone();
        let indexing_scanner = indexing.clone();
        thread::spawn(move || {
            let bytes = storage_scanner.bytes();
            const SCAN_CHUNK: usize = 4096;
            let scan_limit = if bytes.len() <= FULL_INDEX_MAX_FILE_BYTES {
                bytes.len()
            } else {
                bytes.len().min(MAX_INDEXED_BYTES)
            };

            if !bytes.is_empty() {
                let sampled = estimate_avg_line_len(bytes);
                avg_line_len_scanner.store(sampled.max(1), Ordering::Relaxed);
            }

            let mut scanned = 0usize;
            if use_u32_offsets {
                let mut buf: Vec<u32> = Vec::with_capacity(SCAN_CHUNK);
                let mut newlines_found = 0usize;
                let max_offsets = (MAX_LINE_OFFSETS_BYTES / std::mem::size_of::<u32>()).max(1);
                let max_newlines = max_offsets.saturating_sub(1);
                'scan: while scanned < scan_limit {
                    if !indexing_scanner.load(Ordering::Relaxed) {
                        break 'scan;
                    }
                    let block_end = scanned
                        .saturating_add(INDEXER_YIELD_EVERY_BYTES)
                        .min(scan_limit);
                    let block = &bytes[scanned..block_end];

                    for rel in memchr2_iter(b'\n', b'\r', block) {
                        let i = scanned + rel;
                        let b = bytes[i];

                        if b == b'\r' {
                            // Treat lone '\r' as a newline (old-Mac). Skip CRLF: '\n' will handle it.
                            if i + 1 < scan_limit && bytes[i + 1] == b'\n' {
                                continue;
                            }
                        }

                        if newlines_found >= max_newlines {
                            scanned = i + 1;
                            break 'scan;
                        }
                        newlines_found += 1;
                        buf.push((i + 1) as u32);
                        if buf.len() >= SCAN_CHUNK {
                            let mut to_send: Vec<u32> = Vec::with_capacity(SCAN_CHUNK);
                            std::mem::swap(&mut buf, &mut to_send);
                            let _ = tx.send(OffsetsChunk::U32(to_send));
                        }
                    }

                    scanned = block_end;
                    indexed_bytes_scanner.store(scanned, Ordering::Relaxed);
                    let lines = newlines_found.saturating_add(1).max(1);
                    let new_avg = scanned.div_ceil(lines).max(1);
                    let prev = avg_line_len_scanner.load(Ordering::Relaxed);
                    let blended = if prev == 0 {
                        new_avg
                    } else {
                        (prev * 7 + new_avg) / 8
                    };
                    avg_line_len_scanner.store(blended.max(1), Ordering::Relaxed);
                    thread::yield_now();
                }
                indexed_bytes_scanner.store(scanned, Ordering::Relaxed);
                let lines = newlines_found.saturating_add(1).max(1);
                let final_avg = if scanned == 0 {
                    avg_line_len_scanner.load(Ordering::Relaxed).max(1)
                } else {
                    scanned.div_ceil(lines).max(1)
                };
                avg_line_len_scanner.store(final_avg, Ordering::Relaxed);
                if !buf.is_empty() {
                    let _ = tx.send(OffsetsChunk::U32(buf));
                }
            } else {
                let mut buf: Vec<u64> = Vec::with_capacity(SCAN_CHUNK);
                let mut newlines_found = 0usize;
                let max_offsets = (MAX_LINE_OFFSETS_BYTES / std::mem::size_of::<u64>()).max(1);
                let max_newlines = max_offsets.saturating_sub(1);
                'scan: while scanned < scan_limit {
                    if !indexing_scanner.load(Ordering::Relaxed) {
                        break 'scan;
                    }
                    let block_end = scanned
                        .saturating_add(INDEXER_YIELD_EVERY_BYTES)
                        .min(scan_limit);
                    let block = &bytes[scanned..block_end];

                    for rel in memchr2_iter(b'\n', b'\r', block) {
                        let i = scanned + rel;
                        let b = bytes[i];

                        if b == b'\r' {
                            // Treat lone '\r' as a newline (old-Mac). Skip CRLF: '\n' will handle it.
                            if i + 1 < scan_limit && bytes[i + 1] == b'\n' {
                                continue;
                            }
                        }

                        if newlines_found >= max_newlines {
                            scanned = i + 1;
                            break 'scan;
                        }
                        newlines_found += 1;
                        buf.push((i + 1) as u64);
                        if buf.len() >= SCAN_CHUNK {
                            let mut to_send: Vec<u64> = Vec::with_capacity(SCAN_CHUNK);
                            std::mem::swap(&mut buf, &mut to_send);
                            let _ = tx.send(OffsetsChunk::U64(to_send));
                        }
                    }

                    scanned = block_end;
                    indexed_bytes_scanner.store(scanned, Ordering::Relaxed);
                    let lines = newlines_found.saturating_add(1).max(1);
                    let new_avg = scanned.div_ceil(lines).max(1);
                    let prev = avg_line_len_scanner.load(Ordering::Relaxed);
                    let blended = if prev == 0 {
                        new_avg
                    } else {
                        (prev * 7 + new_avg) / 8
                    };
                    avg_line_len_scanner.store(blended.max(1), Ordering::Relaxed);
                    thread::yield_now();
                }
                indexed_bytes_scanner.store(scanned, Ordering::Relaxed);
                let lines = newlines_found.saturating_add(1).max(1);
                let final_avg = if scanned == 0 {
                    avg_line_len_scanner.load(Ordering::Relaxed).max(1)
                } else {
                    scanned.div_ceil(lines).max(1)
                };
                avg_line_len_scanner.store(final_avg, Ordering::Relaxed);
                if !buf.is_empty() {
                    let _ = tx.send(OffsetsChunk::U64(buf));
                }
            }
            // Drop tx to close channel.
        });

        let offsets_pusher = line_offsets.clone();
        let indexing_pusher = indexing.clone();
        thread::spawn(move || {
            for chunk in rx {
                if let Ok(mut guard) = offsets_pusher.write() {
                    match (&mut *guard, chunk) {
                        (LineOffsets::U32(v), OffsetsChunk::U32(chunk)) => v.extend(chunk),
                        (LineOffsets::U64(v), OffsetsChunk::U64(chunk)) => v.extend(chunk),
                        (LineOffsets::U32(v), OffsetsChunk::U64(chunk)) => {
                            v.extend(chunk.into_iter().filter_map(|v| u32::try_from(v).ok()));
                        }
                        (LineOffsets::U64(v), OffsetsChunk::U32(chunk)) => {
                            v.extend(chunk.into_iter().map(|v| v as u64))
                        }
                    }
                }
            }
            indexing_pusher.store(false, Ordering::Relaxed);
        });

        Self {
            path: Some(path),
            storage: Some(storage),
            line_offsets,
            disk_index,
            indexing,
            indexing_started: Some(indexing_started),
            file_len,
            indexed_bytes,
            avg_line_len,
            line_ending,
            rope: None,
            piece_table: None,
            dirty: false,
        }
    }

    /// Returns the current file path, if the document is file-backed.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Sets the document path without saving its contents.
    pub fn set_path(&mut self, path: PathBuf) {
        self.path = Some(path);
    }

    /// Returns `true` if the document has unsaved changes.
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Clears the unsaved-changes flag.
    pub fn mark_clean(&mut self) {
        self.dirty = false;
    }

    pub(super) fn edit_unsupported(&self, reason: &'static str) -> DocumentError {
        DocumentError::EditUnsupported {
            path: self.path.clone(),
            reason,
        }
    }

    pub(super) fn missing_rope_error(&self) -> DocumentError {
        self.edit_unsupported("internal error: rope buffer is unavailable after materialization")
    }

    pub(super) fn can_materialize_rope(&self, total_len: usize) -> bool {
        total_len <= MAX_ROPE_EDIT_FILE_BYTES
    }
}
