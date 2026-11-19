use edit_buffer::{EditBuffer, EditBufferBuilder};
use memchr::{memchr2, memchr2_iter};
use std::cell::Cell;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, LazyLock, RwLock};
use std::thread;
use std::time::{Duration, Instant};

use super::storage::{FileStorage, StorageOpenError};
use crate::index::DiskLineIndex;
use crate::piece_tree::{editlog_path, Piece, PieceSource, PieceTree, SessionMeta};

use rope_history::RopeHistory;

mod alignment;
mod commands;
mod compaction;
mod edit_buffer;
mod editing;
mod encoding_engine;
mod fixtures;
mod lifecycle;
mod persistence;
mod piece_table;
mod pieces;
mod positions;
mod reads;
mod regex_search;
mod rope_history;
mod scan;
mod search;
mod state;
mod types;

pub use crate::piece_tree::FragmentationStats;
pub use compaction::{
    CompactionPolicy, CompactionRecommendation, CompactionUrgency, IdleCompactionOutcome,
};
#[cfg(feature = "editor")]
pub(crate) use lifecycle::OpenProgressPhase;
#[cfg(feature = "editor")]
pub(crate) use persistence::{PreparedSave, SaveCompletion};
pub use regex_search::{RegexCompileError, RegexSearchIter, RegexSearchQuery};
pub use search::{LiteralSearchIter, LiteralSearchQuery};
pub use types::{
    ByteProgress, CutResult, DocumentBacking, DocumentEncoding, DocumentEncodingErrorKind,
    DocumentEncodingOrigin, DocumentError, DocumentMaintenanceStatus, DocumentOpenOptions,
    DocumentReadError, DocumentSaveOptions, DocumentStatus, EditCapability, EditResult, LineCount,
    LineSlice, MaintenanceAction, OpenEncodingPolicy, SaveEncodingPolicy, SearchMatch,
    TextPosition, TextRange, TextSelection, TextSlice, UndoResult, Viewport, ViewportRequest,
    ViewportRow,
};

/// Hidden re-exports used by integration tests under `tests/encoding_engine/`.
///
/// The encoding-aware engine lives at `pub(crate)` visibility because it is
/// not part of the stable public API surface; public API stability is
/// reserved for `1.0.0`. Integration tests live in their own crates and
/// would otherwise be unable to reach the engine surface, so we expose a
/// tightly scoped re-export module here. The `#[doc(hidden)]` attribute
/// keeps these symbols out of rustdoc and IDE completion for library
/// consumers; the only legitimate consumers are the property tests under
/// `tests/encoding_engine/`.
#[doc(hidden)]
pub mod __test_support {
    pub use super::encoding_engine::{
        engine_for_encoding, EncodingEngine, SingleByteEngine, Utf8Engine, UTF8_ENGINE,
    };

    /// Bulk-loads LF-normalized UTF-8 `bytes` into a Qem-native edit
    /// buffer without the document lifecycle and returns its line count.
    /// Used by the performance bench as the raw-buffer baseline next to
    /// `Document::open`, the same role the ropey bulk loader played
    /// before the 0.9.0 replacement; the count is the consumable result
    /// that keeps the internal buffer type unexposed.
    pub fn build_edit_buffer_line_count(bytes: &[u8]) -> usize {
        let buffer = super::edit_buffer::EditBuffer::from_str(
            std::str::from_utf8(bytes).expect("bench fixtures are UTF-8"),
        );
        buffer.len_lines()
    }

    /// Aligns `offset` to the largest character-boundary offset of
    /// `doc.encoding()` that is `<= offset` (the `AlignDirection::Floor`
    /// / `Backward` contract used by the encoded edit and regex paths).
    /// Wraps the crate-internal `Document::align_byte_offset` so the
    /// integration property tests under `tests/encoding_engine/` can
    /// validate Property 14 without exposing `AlignDirection` outside
    /// the crate.
    pub fn align_byte_offset_floor(doc: &super::Document, offset: usize) -> usize {
        doc.align_byte_offset(offset, super::alignment::AlignDirection::Backward)
    }

    /// Returns a snapshot of the document's current bytes for the
    /// alignment scan. Mirrors the crate-internal helper used by
    /// `Document::align_byte_offset`; required by Property 14 so the
    /// test can enumerate per-line and per-character boundaries
    /// without re-deriving the backing-resolution logic.
    pub fn bytes_for_alignment(doc: &super::Document) -> Vec<u8> {
        doc.bytes_for_alignment()
    }

    /// Resolves a typed `TextPosition` to the byte offset the public
    /// search APIs use internally.
    ///
    /// Used by the alignment property test: every public API that
    /// surfaces a `TextPosition` (regex / literal search match
    /// endpoints, viewport row starts, line bounds) must land on a
    /// character boundary of `doc.encoding()` once converted to its
    /// byte representation. The conversion here matches the one
    /// `Document::find_next_regex_query`,
    /// `Document::find_next_query`, `find_*_in_range` and friends use
    /// to clamp endpoints onto valid byte ranges, so the property test
    /// is checking the same offsets the search backends compute and
    /// hand back to callers — no parallel reimplementation.
    pub fn byte_offset_for_text_position(
        doc: &super::Document,
        position: super::TextPosition,
    ) -> usize {
        doc.search_byte_offset_for_position(position)
    }

    /// Test-only escape hatch for the reverse-DFA size-limit overflow
    /// property test.
    ///
    /// Drives the same `regex_automata` reverse-DFA build used by
    /// [`super::regex_search::RegexSearchQuery::ensure_reverse`] but
    /// under a caller-chosen `dfa_size_limit` /
    /// `determinize_size_limit`. The integration property test under
    /// `tests/encoding_engine/prop_reverse_dfa_overflow.rs` cannot
    /// reach the `pub(crate)` builder directly, so it goes through
    /// this thin wrapper. Production code paths still go through
    /// `RegexSearchQuery::ensure_reverse` with the project-wide
    /// 32 MiB limit and never touch this helper.
    ///
    /// The DFA itself is dropped before returning so the integration
    /// crate does not need to depend on `regex_automata` types; the
    /// only shape the test cares about is whether the build returns
    /// the typed error and that the diagnostic is non-empty.
    pub fn build_reverse_dfa_with_limit(
        pattern: &str,
        limit_bytes: usize,
    ) -> Result<(), crate::RegexCompileError> {
        super::regex_search::build_reverse_dfa_with_limit(pattern, limit_bytes).map(|_| ())
    }
}

// Hard limits to keep mmap indexing bounded for huge files.
// We still fully index "reasonable" files, but cap the work for truly huge inputs.
const FULL_INDEX_MAX_FILE_BYTES: usize = 2 * 1024 * 1024 * 1024; // 2 GiB
const MAX_INDEXED_BYTES: usize = 1024 * 1024 * 1024; // 1 GiB
const MAX_LINE_OFFSETS_BYTES: usize = 128 * 1024 * 1024; // 128 MiB budget for line start offsets
const INLINE_FULL_INDEX_MAX_FILE_BYTES: usize = 8 * 1024 * 1024; // 8 MiB
const INDEXER_YIELD_EVERY_BYTES: usize = 4 * 1024 * 1024; // 4 MiB
const AVG_LINE_LEN_ESTIMATE: usize = 50;
const AVG_LINE_LEN_SAMPLE_BYTES: usize = 256 * 1024; // 256 KiB windows
const PIECE_TABLE_MIN_BYTES: usize = 1024 * 1024; // 1 MiB
const MAX_LINE_SCAN_CHARS: usize = 16_384;
/// Budget for the materialized line-length table that seeds the piece-table
/// edit buffer. The line count is derived from a byte budget so the 64-bit
/// `usize` table stays bounded regardless of platform.
const LINE_LENGTHS_MAX_SYNC_BYTES: usize = 128 * 1024 * 1024; // 128 MiB
const LINE_LENGTHS_MAX_SYNC_LINES: usize =
    LINE_LENGTHS_MAX_SYNC_BYTES / std::mem::size_of::<usize>();
const PARTIAL_PIECE_TABLE_TARGET_LINES: usize = 4_096;
const PARTIAL_PIECE_TABLE_SCAN_BYTES: usize = 16 * 1024 * 1024; // 16 MiB
const APPROX_LINE_BACKTRACK_BYTES: usize = 64 * 1024;
const APPROX_LINE_FORWARD_BYTES: usize = 256 * 1024;
const TAIL_FAST_PATH_MAX_BACKSCAN_BYTES: usize = 1024 * 1024; // 1 MiB
const FALLBACK_NEXT_LINE_SCAN_BYTES: usize = 1024 * 1024; // 1 MiB
const SAVE_STREAM_CHUNK_BYTES: usize = 8 * 1024 * 1024; // 8 MiB
pub(super) const MAX_ROPE_EDIT_FILE_BYTES: usize = 128 * 1024 * 1024; // 128 MiB safety cap for full materialization
const FULL_SYNC_PIECE_TABLE_MAX_FILE_BYTES: usize = 64 * 1024 * 1024; // 64 MiB
const PIECE_TREE_TARGET_BYTES: usize = 64 * 1024;
const PIECE_TREE_TARGET_LINES: usize = 512;
const PIECE_TREE_DISK_MIN_BYTES: usize = PIECE_TABLE_MIN_BYTES;
const PIECE_SESSION_FLUSH_DEBOUNCE: Duration = Duration::from_millis(250);
const PIECE_SESSION_FORCE_AFTER_EDITS: usize = 32;

/// Detected dominant line ending style for a document.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum LineEnding {
    #[default]
    Lf,
    Crlf,
    Cr,
}

impl LineEnding {
    /// Returns the literal line break sequence for this style.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Lf => "\n",
            Self::Crlf => "\r\n",
            Self::Cr => "\r",
        }
    }
}

fn detect_line_ending(bytes: &[u8]) -> LineEnding {
    let Some(pos) = memchr2(b'\n', b'\r', bytes) else {
        return LineEnding::Lf;
    };

    match bytes[pos] {
        b'\n' if pos > 0 && bytes[pos - 1] == b'\r' => LineEnding::Crlf,
        b'\n' => LineEnding::Lf,
        b'\r' if pos + 1 < bytes.len() && bytes[pos + 1] == b'\n' => LineEnding::Crlf,
        b'\r' => LineEnding::Cr,
        _ => LineEnding::Lf,
    }
}

fn detect_line_ending_text(text: &str) -> LineEnding {
    let bytes = text.as_bytes();
    let Some(pos) = memchr2(b'\n', b'\r', bytes) else {
        return LineEnding::Lf;
    };

    match bytes[pos] {
        b'\n' if pos > 0 && bytes[pos - 1] == b'\r' => LineEnding::Crlf,
        b'\n' => LineEnding::Lf,
        b'\r' if pos + 1 < bytes.len() && bytes[pos + 1] == b'\n' => LineEnding::Crlf,
        b'\r' => LineEnding::Cr,
        _ => LineEnding::Lf,
    }
}

fn normalize_insert_text(
    text: &str,
    virtual_padding_cols: usize,
    line_ending: LineEnding,
) -> (String, usize, usize) {
    let mut normalized = String::with_capacity(
        text.len()
            .saturating_add(virtual_padding_cols)
            .saturating_add(text.matches('\n').count()),
    );
    if virtual_padding_cols > 0 {
        normalized.extend(std::iter::repeat_n(' ', virtual_padding_cols));
    }

    let mut added_lines = 0usize;
    let mut last_col = 0usize;
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    let _ = chars.next();
                }
                normalized.push_str(line_ending.as_str());
                added_lines += 1;
                last_col = 0;
            }
            '\n' => {
                normalized.push_str(line_ending.as_str());
                added_lines += 1;
                last_col = 0;
            }
            _ => {
                normalized.push(ch);
                last_col += 1;
            }
        }
    }

    (normalized, added_lines, last_col)
}

fn build_rope_from_bytes(bytes: &[u8]) -> (EditBuffer, bool) {
    if bytes.is_empty() {
        return (EditBuffer::new(), false);
    }

    let mut builder = EditBufferBuilder::new();
    let mut decoder = encoding_rs::UTF_8.new_decoder();
    let mut input = bytes;
    let mut out = [0u8; 8192];
    let mut prev_was_cr = false;
    let mut had_errors = false;

    loop {
        let last = input.is_empty();
        let (result, read, written, chunk_had_errors) =
            decoder.decode_to_utf8(input, &mut out, last);
        had_errors |= chunk_had_errors;
        if written > 0 {
            if let Ok(s) = std::str::from_utf8(&out[..written]) {
                if !s.is_empty() {
                    let mut normalized = String::with_capacity(s.len());
                    for ch in s.chars() {
                        if prev_was_cr {
                            prev_was_cr = false;
                            if ch == '\n' {
                                continue;
                            }
                        }
                        if ch == '\r' {
                            normalized.push('\n');
                            prev_was_cr = true;
                        } else {
                            normalized.push(ch);
                        }
                    }
                    if !normalized.is_empty() {
                        builder.append(&normalized);
                    }
                }
            }
        }
        input = &input[read..];
        match result {
            encoding_rs::CoderResult::InputEmpty => {
                if last {
                    break;
                }
            }
            encoding_rs::CoderResult::OutputFull => {}
        }
    }

    (builder.finish(), had_errors)
}

fn normalize_decoded_text(text: &str) -> String {
    let mut normalized = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    let _ = chars.next();
                }
                normalized.push('\n');
            }
            _ => normalized.push(ch),
        }
    }
    normalized
}

fn build_rope_from_decoded_text(text: &str) -> EditBuffer {
    if text.is_empty() {
        return EditBuffer::new();
    }

    let normalized = normalize_decoded_text(text);
    let mut builder = EditBufferBuilder::new();
    builder.append(&normalized);
    builder.finish()
}

fn rope_text_with_line_endings(rope: &EditBuffer, line_ending: LineEnding) -> String {
    if line_ending == LineEnding::Lf {
        return rope.to_string();
    }

    let newline = line_ending.as_str();
    let extra_per_break = newline.len().saturating_sub(1);
    let mut rendered = String::with_capacity(
        rope.len_bytes().saturating_add(
            rope.len_lines()
                .saturating_sub(1)
                .saturating_mul(extra_per_break),
        ),
    );
    for chunk in rope.chunks() {
        for ch in chunk.chars() {
            if ch == '\n' {
                rendered.push_str(newline);
            } else {
                rendered.push(ch);
            }
        }
    }
    rendered
}

fn decode_text_with_encoding(bytes: &[u8], encoding: DocumentEncoding) -> (String, bool) {
    let (decoded, had_errors) = encoding.as_encoding().decode_with_bom_removal(bytes);
    (decoded.into_owned(), had_errors)
}

fn encode_text_with_encoding(
    text: &str,
    encoding: DocumentEncoding,
) -> Result<Vec<u8>, DocumentEncodingErrorKind> {
    if !encoding.can_roundtrip_save() {
        return Err(DocumentEncodingErrorKind::UnsupportedSaveTarget);
    }

    let (encoded, output_encoding, had_errors) = encoding.as_encoding().encode(text);
    if output_encoding != encoding.as_encoding() {
        return Err(DocumentEncodingErrorKind::RedirectedSaveTarget {
            actual: DocumentEncoding::from_encoding_rs(output_encoding),
        });
    }
    if had_errors {
        return Err(DocumentEncodingErrorKind::UnrepresentableText);
    }
    Ok(encoded.into_owned())
}

fn rope_save_len_bytes(rope: &EditBuffer, line_ending: LineEnding) -> usize {
    let extra_per_break = match line_ending {
        LineEnding::Lf => 0,
        LineEnding::Crlf => 1,
        LineEnding::Cr => 0,
    };
    rope.len_bytes().saturating_add(
        rope.len_lines()
            .saturating_sub(1)
            .saturating_mul(extra_per_break),
    )
}

use fixtures::checkpoint_pieces_up_to;
use piece_table::edit_span_unresolvable_error;
use pieces::{split_piece_with_sources, trim_piece_with_sources};
use scan::{
    advance_offset_by_text_units_in_bytes, analyze_inline_open,
    byte_offset_for_text_col_with_engine, count_line_breaks_in_bytes, count_text_columns,
    count_text_columns_exact, estimate_avg_line_len, is_utf8_continuation, line_lengths_from_bytes,
    line_lengths_from_offsets, mmap_line_byte_range, prefix_line_lengths_from_offsets,
    scan_cursor_position_bytes, scan_line_lengths_from, utf8_step, CursorScanState, OffsetsChunk,
};
pub(crate) use scan::{utf16_break_cells, utf16_cells_byte, LineOffsets};

/// Text document with mmap-backed reads, background line indexing, and lazy
/// promotion to a mutable editing buffer.
///
/// This is the lower-level engine surface. Use [`crate::DocumentSession`] for
/// most frontend integrations unless your application deliberately owns its own
/// tab/session lifecycle, background jobs, and save orchestration.
#[derive(Debug)]
pub struct Document {
    path: Option<PathBuf>,
    storage: Option<FileStorage>,
    line_offsets: Arc<RwLock<LineOffsets>>,
    disk_index: Option<DiskLineIndex>,
    indexing: Arc<AtomicBool>,
    indexing_started: Option<Instant>,
    file_len: usize,
    indexed_bytes: Arc<AtomicUsize>,
    avg_line_len: Arc<AtomicUsize>,
    line_ending: LineEnding,
    encoding: DocumentEncoding,
    encoding_origin: DocumentEncodingOrigin,
    /// Sticky reference to the encoding-aware byte engine for the current
    /// `encoding`. The accessor [`Document::encoding_engine`] returns this
    /// field directly; the field is always updated atomically with
    /// `encoding` and `encoding_origin` through
    /// [`Document::set_encoding_contract`].
    encoding_engine: &'static dyn encoding_engine::EncodingEngine,
    decoding_had_errors: bool,
    preserve_save_error_cache: Cell<Option<Option<DocumentEncodingErrorKind>>>,
    /// Cached save length for rope-backed non-UTF-8 documents.
    ///
    /// `file_len` for that combination has no O(chunks) formula: the only
    /// exact answer is a full render + re-encode of the rope, which made
    /// every status poll / edit-capability check on a promoted document
    /// allocate the whole encoded copy. The cache holds the last computed
    /// encoded length and is cleared by the same invalidation discipline
    /// as `preserve_save_error_cache` (edits, encoding contract changes,
    /// backing swaps).
    rope_file_len_cache: Cell<Option<usize>>,

    // Mutable text storage. When present, it becomes the source of truth for rendering/editing.
    rope: Option<EditBuffer>,
    piece_table: Option<PieceTable>,
    /// In-memory undo/redo history for the rope edit buffer. The piece-tree
    /// buffer keeps its own snapshot history; this field only covers edits
    /// applied directly to `rope`. See `rope_history::RopeHistory`.
    rope_history: rope_history::RopeHistory,
    dirty: bool,
}

#[derive(Debug)]
pub(crate) struct PieceTable {
    original: FileStorage,
    add: Vec<u8>,
    pieces: PieceTree,
    /// The document's byte-movement engine. Non-UTF-8 piece tables must
    /// drive every column / text-unit / decode walk through this engine:
    /// the `utf8_step`-based variants only stay correct for UTF-8 and
    /// single-byte documents whose text units are bytes.
    engine: &'static dyn encoding_engine::EncodingEngine,
    known_line_count: usize,
    exact_base_line_breaks: Option<usize>,
    exact_base_total_lines: Option<usize>,
    known_byte_len: usize,
    total_len: usize,
    full_index: bool,
    /// The document encoding contract at piece-table build time. Persisted
    /// through the `.qem.editlog` v3 header label so reopened sessions
    /// restore the same encoding instead of forcing the caller to guess it
    /// again.
    encoding: DocumentEncoding,
    encoding_origin: DocumentEncodingOrigin,
    decoding_had_errors: bool,
    pending_session_flush: bool,
    pending_session_edits: usize,
    last_session_flush: Option<Instant>,
    edit_batch_depth: usize,
    edit_batch_dirty: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct EditOutcome {
    edited: bool,
    cursor: (usize, usize),
}

impl EditOutcome {
    const fn new(edited: bool, cursor: (usize, usize)) -> Self {
        Self { edited, cursor }
    }
}

/// Error from a piece-table replace batch
/// ([`PieceTable::replace_encoded_bytes_at`] / [`PieceTable::replace_range_at`]).
///
/// A replace runs as a delete + insert pair inside one edit batch, so a
/// failure can arrive after the delete half already landed - the same
/// situation the rope composite documents in `Document::try_replace_range`:
/// the document did change and the deletion must stay undoable. `mutated`
/// tells the caller whether the content changed despite the error, so the
/// document can be marked dirty before the failure is surfaced.
#[derive(Debug)]
pub(crate) struct ReplaceBatchError {
    pub(crate) source: io::Error,
    pub(crate) mutated: bool,
}

impl From<ReplaceBatchError> for io::Error {
    fn from(err: ReplaceBatchError) -> Self {
        err.source
    }
}

fn session_sidecar_path(path: Option<&Path>, fallback: &Path) -> PathBuf {
    let source = path.unwrap_or(fallback);
    editlog_path(source)
}

#[cfg(test)]
fn clear_session_sidecar(path: &Path) {
    persistence::clear_session_sidecar(path);
}

#[cfg(test)]
mod encoding_recovery_tests;
#[cfg(test)]
mod regex_tests;
#[cfg(test)]
mod tests;
