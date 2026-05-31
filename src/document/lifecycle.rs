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

fn open_encoding_error(
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

fn auto_detect_open_encoding(bytes: &[u8]) -> Option<DocumentEncoding> {
    if bytes.starts_with(&[0xFF, 0xFE]) {
        return Some(DocumentEncoding::utf16le());
    }
    if bytes.starts_with(&[0xFE, 0xFF]) {
        return Some(DocumentEncoding::utf16be());
    }
    if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        // UTF-8 BOM. We surface this through the same auto-detected origin
        // as the UTF-16 BOMs so a frontend integrating against the typed
        // origin sees a consistent contract: byte-order-marked sources are
        // detected, not "fast-path UTF-8".
        return Some(DocumentEncoding::utf8());
    }
    None
}

/// Returns `true` when `encoding` is `UTF-16LE` or `UTF-16BE`.
///
/// `DocumentEncoding` does not currently surface a typed predicate for
/// the UTF-16 family, so the open-dispatch routing key falls back to
/// the canonical `encoding_rs` label. Keeping the check here means
/// callers in `from_storage_with_encoding` do not have to know about
/// endianness-specific details from `encoding_engine::utf16`.
fn is_utf16(encoding: DocumentEncoding) -> bool {
    matches!(encoding.name(), "UTF-16LE" | "UTF-16BE")
}

/// Returns `true` when `encoding` is one of the variable-length CJK
/// multibyte encodings driven by `MultiByteEngine`: `Shift_JIS`,
/// `gb18030`, `EUC-KR`.
///
/// These encodings route through the same Class B
/// native open path used by UTF-16: line offsets are indexed via the
/// engine's character-aware `next_line_start`, which honours the
/// false-positive-aware byte walk, and no UTF-8
/// rope is materialised. Like `is_utf16`, we key on the canonical
/// `encoding_rs` label here so the dispatch site stays decoupled from
/// `encoding_engine::multibyte` internals.
fn is_cjk(encoding: DocumentEncoding) -> bool {
    matches!(encoding.name(), "Shift_JIS" | "gb18030" | "EUC-KR")
}

/// Classifies the line ending style preceding `next_line_start` for a
/// Class B document, returning `None` if the cell immediately before
/// `next` cannot be inspected (e.g. the file ends with a CR that the
/// engine collapsed but no LF cell follows). The classification is
/// *logical* — `LineEnding::Lf` / `Cr` / `Crlf` — independent of
/// endianness, because the recorded `LineEnding` style is endianness-
/// agnostic; the actual stored bytes vary by endianness (`0x0A 0x00`
/// vs `0x00 0x0A`, etc.).
///
/// Walks backwards from `next` in 2-byte cells when the encoding is
/// UTF-16, falling back to single-byte inspection for other Class B
/// encodings.
fn classify_class_b_line_ending(
    bytes: &[u8],
    encoding: DocumentEncoding,
    next: usize,
) -> Option<LineEnding> {
    if is_utf16(encoding) {
        if next < 2 || next > bytes.len() {
            return None;
        }
        let last = [bytes[next - 2], bytes[next - 1]];
        let (lf, cr) = match encoding.name() {
            "UTF-16LE" => ([0x0A, 0x00], [0x0D, 0x00]),
            "UTF-16BE" => ([0x00, 0x0A], [0x00, 0x0D]),
            _ => return None,
        };
        if last == lf {
            // CRLF if the cell before is CR and is wholly inside the
            // slice. Otherwise lone LF.
            if next >= 4 {
                let prev = [bytes[next - 4], bytes[next - 3]];
                if prev == cr {
                    return Some(LineEnding::Crlf);
                }
            }
            return Some(LineEnding::Lf);
        }
        if last == cr {
            return Some(LineEnding::Cr);
        }
        return None;
    }
    // Class B encodings other than UTF-16 (CJK multibyte) keep the same
    // 0x0A / 0x0D byte values for line endings as ASCII; classify by the
    // byte immediately before `next`.
    if next == 0 || next > bytes.len() {
        return None;
    }
    match bytes[next - 1] {
        b'\n' => {
            if next >= 2 && bytes[next - 2] == b'\r' {
                Some(LineEnding::Crlf)
            } else {
                Some(LineEnding::Lf)
            }
        }
        b'\r' => Some(LineEnding::Cr),
        _ => None,
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
            encoding: DocumentEncoding::utf8(),
            encoding_origin: DocumentEncodingOrigin::NewDocument,
            encoding_engine: encoding_engine::engine_for_encoding(DocumentEncoding::utf8()),
            decoding_had_errors: false,
            preserve_save_error_cache: Cell::new(None),
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
        Self::open_with_options_and_progress(path, DocumentOpenOptions::new(), |_| {})
    }

    /// Opens a file using explicit document-open options.
    pub fn open_with_options(
        path: impl Into<PathBuf>,
        options: DocumentOpenOptions,
    ) -> Result<Self, DocumentError> {
        Self::open_with_options_and_progress(path, options, |_| {})
    }

    /// Opens a file using the lightweight auto-detect path.
    ///
    /// This currently recognizes BOM-backed UTF-16 sources and otherwise
    /// falls back to the default UTF-8/ASCII fast path.
    pub fn open_with_auto_encoding_detection(
        path: impl Into<PathBuf>,
    ) -> Result<Self, DocumentError> {
        Self::open_with_options(
            path,
            DocumentOpenOptions::new().with_auto_encoding_detection(),
        )
    }

    /// Opens a file using an explicit text encoding.
    ///
    /// This explicitly reinterprets the source bytes through `encoding`.
    /// Non-UTF8 opens currently transcode the source into a rope-backed
    /// document instead of using the mmap fast path.
    pub fn open_with_encoding(
        path: impl Into<PathBuf>,
        encoding: DocumentEncoding,
    ) -> Result<Self, DocumentError> {
        Self::open_with_options(path, DocumentOpenOptions::new().with_encoding(encoding))
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn open_with_progress(
        path: impl Into<PathBuf>,
        mut progress: impl FnMut(u64),
    ) -> Result<Self, DocumentError> {
        Self::open_with_options_and_reporting(
            path,
            DocumentOpenOptions::new(),
            &mut progress,
            &mut ignore_open_phase,
        )
    }

    pub(crate) fn open_with_options_and_progress(
        path: impl Into<PathBuf>,
        options: DocumentOpenOptions,
        mut progress: impl FnMut(u64),
    ) -> Result<Self, DocumentError> {
        Self::open_with_options_and_reporting(path, options, &mut progress, &mut ignore_open_phase)
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn open_with_reporting(
        path: impl Into<PathBuf>,
        mut progress: impl FnMut(u64),
        phase: &mut dyn FnMut(OpenProgressPhase),
    ) -> Result<Self, DocumentError> {
        Self::open_with_options_and_reporting(
            path,
            DocumentOpenOptions::new(),
            &mut progress,
            phase,
        )
    }

    pub(crate) fn open_with_options_and_reporting(
        path: impl Into<PathBuf>,
        options: DocumentOpenOptions,
        mut progress: impl FnMut(u64),
        phase: &mut dyn FnMut(OpenProgressPhase),
    ) -> Result<Self, DocumentError> {
        Self::open_with_encoding_policy(path, options.encoding_policy(), &mut progress, phase)
    }

    fn open_with_encoding_policy(
        path: impl Into<PathBuf>,
        encoding_policy: OpenEncodingPolicy,
        progress: &mut dyn FnMut(u64),
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

        let mut tracker = OpenProgressTracker::new(storage.len() as u64, progress);
        let bytes = storage.bytes();
        let (encoding, encoding_origin) = match encoding_policy {
            OpenEncodingPolicy::Utf8FastPath => (None, DocumentEncodingOrigin::Utf8FastPath),
            OpenEncodingPolicy::AutoDetect => {
                let inspected = bytes.len().min(2);
                if inspected > 0 {
                    tracker.report_inspected(inspected);
                }
                match auto_detect_open_encoding(bytes) {
                    Some(encoding) => (Some(encoding), DocumentEncodingOrigin::AutoDetected),
                    None => (None, DocumentEncodingOrigin::AutoDetectFallbackUtf8),
                }
            }
            OpenEncodingPolicy::AutoDetectOrReinterpret(fallback_encoding) => {
                let inspected = bytes.len().min(2);
                if inspected > 0 {
                    tracker.report_inspected(inspected);
                }
                match auto_detect_open_encoding(bytes) {
                    Some(encoding) => (Some(encoding), DocumentEncodingOrigin::AutoDetected),
                    None => (
                        Some(fallback_encoding),
                        DocumentEncodingOrigin::AutoDetectFallbackOverride,
                    ),
                }
            }
            OpenEncodingPolicy::Reinterpret(encoding) => (
                Some(encoding),
                DocumentEncodingOrigin::ExplicitReinterpretation,
            ),
        };
        let doc = if let Some(encoding) = encoding {
            Self::from_storage_with_encoding(
                path,
                storage,
                encoding,
                encoding_origin,
                &mut tracker,
                phase,
            )?
        } else {
            Self::from_storage_with_progress(path, storage, encoding_origin, &mut tracker, phase)
        };
        phase(OpenProgressPhase::Ready);
        tracker.complete();
        Ok(doc)
    }

    pub(super) fn from_storage_with_origin(
        path: PathBuf,
        storage: FileStorage,
        encoding_origin: DocumentEncodingOrigin,
    ) -> Self {
        let total_bytes = storage.len() as u64;
        let mut progress = ignore_open_progress as fn(u64);
        let mut tracker = OpenProgressTracker::new(total_bytes, &mut progress);
        Self::from_storage_with_progress(
            path,
            storage,
            encoding_origin,
            &mut tracker,
            &mut ignore_open_phase,
        )
    }

    pub(super) fn reopen_with_encoding_contract(
        path: PathBuf,
        encoding: DocumentEncoding,
        encoding_origin: DocumentEncodingOrigin,
    ) -> Result<Self, DocumentError> {
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

        if encoding.is_utf8() {
            return Ok(Self::from_storage_with_origin(
                path,
                storage,
                encoding_origin,
            ));
        }

        let total_bytes = storage.len() as u64;
        let mut progress = ignore_open_progress as fn(u64);
        let mut tracker = OpenProgressTracker::new(total_bytes, &mut progress);
        Self::from_storage_with_encoding(
            path,
            storage,
            encoding,
            encoding_origin,
            &mut tracker,
            &mut ignore_open_phase,
        )
    }

    fn from_storage_with_encoding(
        path: PathBuf,
        storage: FileStorage,
        encoding: DocumentEncoding,
        encoding_origin: DocumentEncodingOrigin,
        progress: &mut OpenProgressTracker<'_>,
        phase: &mut dyn FnMut(OpenProgressPhase),
    ) -> Result<Self, DocumentError> {
        // UTF-8 fast path: bytes are already in the canonical encoding so the
        // mmap path applies. The only exception is a UTF-8 BOM ('EF BB BF')
        // at the start of the file: keeping that on the mmap fast path would
        // leave the BOM as visible content at line 0 column 0, which is not
        // what frontends expect from a "UTF-8" document. We strip it through
        // the rope decode pipeline below; very large UTF-8 BOM files fall
        // under the same OpenTranscodeTooLarge guard as Class B encodings
        // (UTF-16, CJK) until their native mmap paths land. Class A is
        // routed away from the guard right after this branch.
        let bytes = storage.bytes();
        let utf8_with_bom = encoding.is_utf8() && bytes.starts_with(&[0xEF, 0xBB, 0xBF]);
        if encoding.is_utf8() && !utf8_with_bom {
            return Ok(Self::from_storage_with_progress(
                path,
                storage,
                encoding_origin,
                progress,
                phase,
            ));
        }
        // Class A native open path.
        //
        // ASCII-superset single-byte encodings (windows-1251, latin1,
        // KOI8-R, IBM866, ...) drive a `SingleByteEngine`, so the mmap
        // bytes are usable as-is for line indexing and viewport reads
        // without materializing a UTF-8 rope. The dedicated constructor
        // installs the encoding contract (and therefore the engine
        // field) atomically through `set_encoding_contract`.
        //
        // Class A is routed *before* the `MAX_ROPE_EDIT_FILE_BYTES`
        // guard below so that the guard never trips for Class A.
        // The guard remains in effect only for the rope-decoding
        // branches that still materialise a full UTF-8 string from
        // the source bytes: the UTF-8 BOM strip path which always
        // transcodes through a rope. Class B (UTF-16 LE/BE and the CJK
        // multibyte encodings Shift_JIS, GB18030, EUC-KR) is routed
        // away from the guard by `from_storage_class_b_native` further
        // down, just like Class A.
        if encoding_engine::SingleByteEngine::supports(encoding) {
            return Ok(Self::from_storage_class_a_native(
                path,
                storage,
                encoding,
                encoding_origin,
                progress,
                phase,
            ));
        }
        // Class B native open path for UTF-16LE / UTF-16BE and the
        // CJK multibyte encodings `Shift_JIS` / `gb18030` / `EUC-KR`.
        // UTF-16 files index line offsets via the surrogate-aware
        // `Utf16Engine::next_line_start`, which walks 2-byte aligned
        // cells so a stray `0x0A` / `0x0D` byte inside a UTF-16 code
        // unit is never mistaken for a line break. The
        // CJK multibyte engines walk the bytes character-by-character
        // through `MultiByteEngine::next_line_start`, which only treats
        // `0x0A` / `0x0D` as line breaks when the engine's leading-byte
        // detector says the cursor is on a single-byte (ASCII) cell —
        // a trailing-byte `0x0A` inside a Shift_JIS / GB18030 / EUC-KR
        // multibyte sequence is therefore never a false-positive line
        // break. The `MAX_ROPE_EDIT_FILE_BYTES`
        // guard does not apply here: there is no UTF-8 rope to
        // materialise.
        if is_utf16(encoding) || is_cjk(encoding) {
            return Ok(Self::from_storage_class_b_native(
                path,
                storage,
                encoding,
                encoding_origin,
                progress,
                phase,
            ));
        }
        if storage.len() > MAX_ROPE_EDIT_FILE_BYTES {
            return Err(open_encoding_error(
                &path,
                "open",
                encoding,
                DocumentEncodingErrorKind::OpenTranscodeTooLarge {
                    max_bytes: MAX_ROPE_EDIT_FILE_BYTES,
                },
            ));
        }

        phase(OpenProgressPhase::InspectingSource);
        let bytes = storage.bytes();
        progress.report_inspected(bytes.len());
        let (decoded, decoding_had_errors) = decode_text_with_encoding(bytes, encoding);
        let line_ending = detect_line_ending_text(&decoded);
        let rope = build_rope_from_decoded_text(&decoded);
        let file_len = storage.len();
        let indexed_bytes = Arc::new(AtomicUsize::new(file_len));
        let avg_line_len = Arc::new(AtomicUsize::new(AVG_LINE_LEN_ESTIMATE));
        let indexing = Arc::new(AtomicBool::new(false));
        let line_offsets = Arc::new(RwLock::new(LineOffsets::new_for_file_len(file_len)));

        Ok(Self {
            path: Some(path),
            storage: Some(storage),
            line_offsets,
            disk_index: None,
            indexing,
            indexing_started: Some(Instant::now()),
            file_len,
            indexed_bytes,
            avg_line_len,
            line_ending,
            encoding,
            encoding_origin,
            encoding_engine: encoding_engine::engine_for_encoding(encoding),
            decoding_had_errors,
            preserve_save_error_cache: Cell::new(None),
            rope: Some(rope),
            piece_table: None,
            dirty: false,
        })
    }

    /// Class A native open path.
    ///
    /// Builds a Document over the mmap bytes for an ASCII-superset
    /// single-byte encoding (windows-1251, latin1, KOI8-R, IBM866, ...)
    /// without materializing a UTF-8 rope. Line offsets are indexed
    /// inline via `memchr2_iter(b'\n', b'\r', bytes)` — the byte
    /// values 0x0A / 0x0D coincide with their UTF-8 / ASCII
    /// counterparts in every Class A encoding, so `memchr2_iter` is
    /// a sound newline scanner directly over the raw mmap bytes.
    /// Window decoding through `encoding_rs` happens later, at
    /// viewport / line-slice / search-result read time,
    /// and only for the requested byte window. The encoding contract
    /// (and therefore `encoding_engine`) is installed atomically
    /// through `set_encoding_contract`.
    fn from_storage_class_a_native(
        path: PathBuf,
        storage: FileStorage,
        encoding: DocumentEncoding,
        encoding_origin: DocumentEncodingOrigin,
        progress: &mut OpenProgressTracker<'_>,
        phase: &mut dyn FnMut(OpenProgressPhase),
    ) -> Self {
        debug_assert!(encoding_engine::SingleByteEngine::supports(encoding));

        phase(OpenProgressPhase::InspectingSource);
        let bytes = storage.bytes();
        let file_len = storage.len();

        // Inline line-offset indexing via memchr2_iter(b'\n', b'\r', bytes).
        // CRLF collapses to a single line break: when we see '\r', we skip
        // it if the next byte is '\n' (the '\n' iteration handles the
        // break); when we see '\n' that follows a '\r' we skip it because
        // that pair was already accounted for. Lone '\n' or lone '\r' both
        // open a new line.
        let mut detected_line_ending: Option<LineEnding> = None;
        let line_offsets = if file_len <= u32::MAX as usize {
            let mut offsets = Vec::with_capacity(LineOffsets::capacity_for::<u32>(file_len));
            offsets.push(0);
            for pos in memchr2_iter(b'\n', b'\r', bytes) {
                match bytes[pos] {
                    b'\r' if pos + 1 < bytes.len() && bytes[pos + 1] == b'\n' => {
                        detected_line_ending.get_or_insert(LineEnding::Crlf);
                        continue;
                    }
                    b'\n' if pos > 0 && bytes[pos - 1] == b'\r' => {}
                    b'\n' => {
                        detected_line_ending.get_or_insert(LineEnding::Lf);
                    }
                    b'\r' => {
                        detected_line_ending.get_or_insert(LineEnding::Cr);
                    }
                    _ => continue,
                }
                offsets.push((pos + 1) as u32);
            }
            LineOffsets::U32(offsets)
        } else {
            let mut offsets = Vec::with_capacity(LineOffsets::capacity_for::<u64>(file_len));
            offsets.push(0);
            for pos in memchr2_iter(b'\n', b'\r', bytes) {
                match bytes[pos] {
                    b'\r' if pos + 1 < bytes.len() && bytes[pos + 1] == b'\n' => {
                        detected_line_ending.get_or_insert(LineEnding::Crlf);
                        continue;
                    }
                    b'\n' if pos > 0 && bytes[pos - 1] == b'\r' => {}
                    b'\n' => {
                        detected_line_ending.get_or_insert(LineEnding::Lf);
                    }
                    b'\r' => {
                        detected_line_ending.get_or_insert(LineEnding::Cr);
                    }
                    _ => continue,
                }
                offsets.push((pos + 1) as u64);
            }
            LineOffsets::U64(offsets)
        };
        progress.report_inspected(file_len);
        let line_ending = detected_line_ending.unwrap_or(LineEnding::Lf);

        let indexing = Arc::new(AtomicBool::new(false));
        let indexed_bytes = Arc::new(AtomicUsize::new(file_len));
        let avg_line_len = Arc::new(AtomicUsize::new(AVG_LINE_LEN_ESTIMATE));

        // Construct the document with a placeholder UTF-8 contract first,
        // then funnel through `set_encoding_contract` so the engine field
        // and the encoding fields are wired together by exactly one path.
        // This keeps the contract installation symmetric with the
        // reinterpret / save-conversion mutators.
        let mut doc = Self {
            path: Some(path),
            storage: Some(storage),
            line_offsets: Arc::new(RwLock::new(line_offsets)),
            disk_index: None,
            indexing,
            indexing_started: Some(Instant::now()),
            file_len,
            indexed_bytes,
            avg_line_len,
            line_ending,
            encoding: DocumentEncoding::utf8(),
            encoding_origin,
            encoding_engine: encoding_engine::engine_for_encoding(DocumentEncoding::utf8()),
            decoding_had_errors: false,
            preserve_save_error_cache: Cell::new(None),
            rope: None,
            piece_table: None,
            dirty: false,
        };
        doc.set_encoding_contract(encoding, encoding_origin);
        doc
    }

    /// Class B native open path for UTF-16 and the CJK multibyte
    /// encodings `Shift_JIS` / `gb18030` / `EUC-KR`.
    ///
    /// Builds a Document over the mmap bytes for `UTF-16LE` /
    /// `UTF-16BE` and the CJK multibyte encodings without materialising
    /// a UTF-8 rope. Line offsets are indexed by walking the bytes
    /// through `engine.next_line_start`, *not* through
    /// `memchr2_iter(b'\n', b'\r', bytes)`: for UTF-16, a stray `0x0A`
    /// or `0x0D` byte at an odd position inside a UTF-16 code unit is
    /// not a line break; for the CJK multibyte engines,
    /// a `0x0A` / `0x0D` byte that lands as the trailing byte of a
    /// multibyte sequence is also not a line break. The engine is the
    /// only component that can correctly
    /// skip such false-positive candidates.
    ///
    /// Window decoding through `encoding_rs` happens later, at
    /// viewport / line-slice / search-result read time,
    /// and only for the requested byte window. The encoding contract
    /// (and therefore `encoding_engine`) is installed atomically
    /// through `set_encoding_contract`.
    fn from_storage_class_b_native(
        path: PathBuf,
        storage: FileStorage,
        encoding: DocumentEncoding,
        encoding_origin: DocumentEncodingOrigin,
        progress: &mut OpenProgressTracker<'_>,
        phase: &mut dyn FnMut(OpenProgressPhase),
    ) -> Self {
        phase(OpenProgressPhase::InspectingSource);
        let bytes = storage.bytes();
        let file_len = storage.len();

        // The line-indexing engine is the engine for the requested
        // encoding, not the document's current contract: we have not
        // wired up `set_encoding_contract` yet, so `self.encoding_engine`
        // would still point at the UTF-8 placeholder used to build the
        // initial `Self` below. Resolve it directly here so the indexing
        // walk uses surrogate-aware UTF-16 stepping (or, for CJK
        // encodings, the multibyte step tables).
        let engine = encoding_engine::engine_for_encoding(encoding);

        // Walk the bytes through `engine.next_line_start` and collect
        // every line-start offset. The first line always starts at 0.
        // The first line break we see also determines the `LineEnding`
        // style: for UTF-16 the recorded style is the *logical* one
        // (`Lf` / `Cr` / `Crlf`); the actual stored bytes vary by
        // endianness (`0x0A 0x00` vs `0x00 0x0A`, etc.) but the
        // engine has already collapsed CRLF into a single boundary,
        // so we can detect the style by inspecting the 2-byte cell
        // immediately before the returned offset.
        let mut detected_line_ending: Option<LineEnding> = None;
        let line_offsets = if file_len <= u32::MAX as usize {
            let mut offsets = Vec::with_capacity(LineOffsets::capacity_for::<u32>(file_len));
            offsets.push(0);
            let mut cursor = 0usize;
            loop {
                let next = engine.next_line_start(bytes, file_len, cursor);
                if next >= file_len || next == cursor {
                    break;
                }
                if detected_line_ending.is_none() {
                    detected_line_ending = classify_class_b_line_ending(bytes, encoding, next);
                }
                offsets.push(next as u32);
                cursor = next;
            }
            LineOffsets::U32(offsets)
        } else {
            let mut offsets = Vec::with_capacity(LineOffsets::capacity_for::<u64>(file_len));
            offsets.push(0);
            let mut cursor = 0usize;
            loop {
                let next = engine.next_line_start(bytes, file_len, cursor);
                if next >= file_len || next == cursor {
                    break;
                }
                if detected_line_ending.is_none() {
                    detected_line_ending = classify_class_b_line_ending(bytes, encoding, next);
                }
                offsets.push(next as u64);
                cursor = next;
            }
            LineOffsets::U64(offsets)
        };
        progress.report_inspected(file_len);
        let line_ending = detected_line_ending.unwrap_or(LineEnding::Lf);

        // CJK multibyte encodings can contain ill-formed sequences
        // (e.g. a stray `0x82` lead byte without a trail) that
        // `encoding_rs` decodes with `U+FFFD` substitution. UTF-16 is
        // streamed byte-for-byte through preserve-save (no rope), so a
        // bad byte does not corrupt the on-disk output and we leave
        // `decoding_had_errors` alone. The CJK multibyte engines also
        // stream byte-for-byte for unedited preserve-save, but the
        // editor frontends rely on the `decoding_had_errors` flag to
        // surface `LossyDecodedPreserve` and to short-circuit silently
        // re-saving a file the user opened as lossy. Preserve that
        // contract here by probe-decoding the bytes once at open time
        // for CJK Class B encodings only — UTF-16 keeps `false` so
        // existing UTF-16 preserve-save tests remain unaffected.
        let decoding_had_errors = if is_cjk(encoding) {
            let (_, had_errors) = encoding.as_encoding().decode_without_bom_handling(bytes);
            had_errors
        } else {
            false
        };

        let indexing = Arc::new(AtomicBool::new(false));
        let indexed_bytes = Arc::new(AtomicUsize::new(file_len));
        let avg_line_len = Arc::new(AtomicUsize::new(AVG_LINE_LEN_ESTIMATE));

        // Construct the document with a placeholder UTF-8 contract first,
        // then funnel through `set_encoding_contract` so the engine field
        // and the encoding fields are wired together by exactly one path.
        // This mirrors the Class A native open path.
        let mut doc = Self {
            path: Some(path),
            storage: Some(storage),
            line_offsets: Arc::new(RwLock::new(line_offsets)),
            disk_index: None,
            indexing,
            indexing_started: Some(Instant::now()),
            file_len,
            indexed_bytes,
            avg_line_len,
            line_ending,
            encoding: DocumentEncoding::utf8(),
            encoding_origin,
            encoding_engine: encoding_engine::engine_for_encoding(DocumentEncoding::utf8()),
            decoding_had_errors,
            preserve_save_error_cache: Cell::new(None),
            rope: None,
            piece_table: None,
            dirty: false,
        };
        doc.set_encoding_contract(encoding, encoding_origin);
        doc
    }

    fn from_storage_with_progress(
        path: PathBuf,
        storage: FileStorage,
        encoding_origin: DocumentEncodingOrigin,
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
                        encoding: DocumentEncoding::utf8(),
                        encoding_origin: meta.encoding_origin.unwrap_or(encoding_origin),
                        encoding_engine: encoding_engine::engine_for_encoding(
                            DocumentEncoding::utf8(),
                        ),
                        decoding_had_errors: meta.decoding_had_errors,
                        preserve_save_error_cache: Cell::new(None),
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
                encoding: DocumentEncoding::utf8(),
                encoding_origin,
                encoding_engine: encoding_engine::engine_for_encoding(DocumentEncoding::utf8()),
                decoding_had_errors: false,
                preserve_save_error_cache: Cell::new(None),
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
                encoding: DocumentEncoding::utf8(),
                encoding_origin,
                encoding_engine: encoding_engine::engine_for_encoding(DocumentEncoding::utf8()),
                decoding_had_errors: inline_analysis.utf8_had_errors,
                preserve_save_error_cache: Cell::new(None),
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
            encoding: DocumentEncoding::utf8(),
            encoding_origin,
            encoding_engine: encoding_engine::engine_for_encoding(DocumentEncoding::utf8()),
            decoding_had_errors: false,
            preserve_save_error_cache: Cell::new(None),
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

    pub(crate) fn can_skip_clean_preserve_save_to_path(&self, path: &Path) -> bool {
        if self.dirty || self.path.as_deref() != Some(path) || !path.exists() {
            return false;
        }

        let Some(backing) = self
            .piece_table
            .as_ref()
            .map(|piece_table| &piece_table.original)
            .or(self.storage.as_ref())
        else {
            return false;
        };

        backing.path() == path && backing.matches_live_file_contents().unwrap_or(false)
    }

    /// Returns `true` if the document has unsaved changes.
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Clears the unsaved-changes flag.
    pub fn mark_clean(&mut self) {
        self.invalidate_preserve_save_error_cache();
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
