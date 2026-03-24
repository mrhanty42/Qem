use memchr::{memchr2, memchr2_iter};
use ropey::{Rope, RopeBuilder};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, RwLock};
use std::thread;
use std::time::{Duration, Instant};

use super::storage::{FileStorage, StorageOpenError};
use crate::index::DiskLineIndex;
use crate::piece_tree::{editlog_path, Piece, PieceSource, PieceTree, SessionMeta};

mod commands;
mod editing;
mod lifecycle;
mod persistence;
mod positions;
mod reads;
mod search;
mod state;
mod types;

#[cfg(feature = "editor")]
pub(crate) use lifecycle::OpenProgressPhase;
#[cfg(feature = "editor")]
pub(crate) use persistence::{PreparedSave, SaveCompletion};
pub use search::LiteralSearchQuery;
pub use types::{
    ByteProgress, CutResult, DocumentBacking, DocumentError, DocumentStatus, EditCapability,
    EditResult, LineCount, LineSlice, SearchMatch, TextPosition, TextRange, TextSelection,
    TextSlice, Viewport, ViewportRequest, ViewportRow,
};

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
const LINE_LENGTHS_MAX_SYNC_LINES: usize = 4_000_000;
const PARTIAL_PIECE_TABLE_TARGET_LINES: usize = 4_096;
const PARTIAL_PIECE_TABLE_MAX_LINES: usize = LINE_LENGTHS_MAX_SYNC_LINES;
const PARTIAL_PIECE_TABLE_SCAN_BYTES: usize = 16 * 1024 * 1024; // 16 MiB
const APPROX_LINE_BACKTRACK_BYTES: usize = 64 * 1024;
const APPROX_LINE_FORWARD_BYTES: usize = 256 * 1024;
const TAIL_FAST_PATH_MAX_BACKSCAN_BYTES: usize = 1024 * 1024; // 1 MiB
const FALLBACK_NEXT_LINE_SCAN_BYTES: usize = 1024 * 1024; // 1 MiB
const SAVE_STREAM_CHUNK_BYTES: usize = 8 * 1024 * 1024; // 8 MiB
const MAX_ROPE_EDIT_FILE_BYTES: usize = 128 * 1024 * 1024; // 128 MiB safety cap for full materialization
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

fn build_rope_from_bytes(bytes: &[u8]) -> Rope {
    if bytes.is_empty() {
        return Rope::new();
    }

    let mut builder = RopeBuilder::new();
    let mut decoder = encoding_rs::UTF_8.new_decoder();
    let mut input = bytes;
    let mut out = [0u8; 8192];
    let mut prev_was_cr = false;

    loop {
        let last = input.is_empty();
        let (result, read, written, _) = decoder.decode_to_utf8(input, &mut out, last);
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

    builder.finish()
}

fn rope_save_len_bytes(rope: &Rope, line_ending: LineEnding) -> usize {
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

fn line_lengths_from_offsets(offsets: &LineOffsets, file_len: usize) -> Vec<usize> {
    let len = offsets.len().max(1);
    let mut lengths = Vec::with_capacity(len);
    match offsets {
        LineOffsets::U32(v) => {
            for i in 0..v.len() {
                let start = v[i] as usize;
                let end = v
                    .get(i + 1)
                    .copied()
                    .map(|v| v as usize)
                    .unwrap_or(file_len);
                lengths.push(end.saturating_sub(start));
            }
        }
        LineOffsets::U64(v) => {
            for i in 0..v.len() {
                let start = v[i] as usize;
                let end = v
                    .get(i + 1)
                    .copied()
                    .map(|v| v as usize)
                    .unwrap_or(file_len);
                lengths.push(end.saturating_sub(start));
            }
        }
    }
    if lengths.is_empty() {
        lengths.push(file_len);
    }
    lengths
}

fn prefix_line_lengths_from_offsets(offsets: &LineOffsets, max_lines: usize) -> Vec<usize> {
    let complete_lines = offsets.len().saturating_sub(1).min(max_lines);
    let mut lengths = Vec::with_capacity(complete_lines.max(1));
    match offsets {
        LineOffsets::U32(v) => {
            for i in 0..complete_lines {
                let start = v[i] as usize;
                let end = v[i + 1] as usize;
                lengths.push(end.saturating_sub(start));
            }
        }
        LineOffsets::U64(v) => {
            for i in 0..complete_lines {
                let start = v[i] as usize;
                let end = v[i + 1] as usize;
                lengths.push(end.saturating_sub(start));
            }
        }
    }
    lengths
}

fn line_lengths_from_bytes(bytes: &[u8], max_lines: usize) -> Option<Vec<usize>> {
    if bytes.is_empty() {
        return Some(vec![0]);
    }

    let est_lines = (bytes.len() / AVG_LINE_LEN_ESTIMATE).saturating_add(2);
    let mut lengths = Vec::with_capacity(est_lines.min(max_lines.max(1)));
    let mut line_start = 0usize;

    for i in memchr2_iter(b'\n', b'\r', bytes) {
        let b = bytes[i];
        if b == b'\r' && i + 1 < bytes.len() && bytes[i + 1] == b'\n' {
            continue;
        }

        if lengths.len() >= max_lines {
            return None;
        }
        lengths.push((i + 1).saturating_sub(line_start));
        line_start = i + 1;
    }

    if lengths.len() >= max_lines {
        return None;
    }
    lengths.push(bytes.len().saturating_sub(line_start));
    Some(lengths)
}

fn scan_line_lengths_from(
    bytes: &[u8],
    start: usize,
    max_lines: usize,
    max_bytes: usize,
) -> Vec<usize> {
    if max_lines == 0 || start >= bytes.len() {
        return Vec::new();
    }

    let end = start.saturating_add(max_bytes).min(bytes.len());
    let slice = &bytes[start..end];
    let mut lengths = Vec::with_capacity(max_lines.min(256));
    let mut line_start = 0usize;

    for rel in memchr2_iter(b'\n', b'\r', slice) {
        let i = start + rel;
        let b = bytes[i];
        if b == b'\r' && i + 1 < bytes.len() && bytes[i + 1] == b'\n' {
            continue;
        }

        lengths.push((rel + 1).saturating_sub(line_start));
        line_start = rel + 1;
        if lengths.len() >= max_lines {
            return lengths;
        }
    }

    if end == bytes.len() && lengths.len() < max_lines {
        lengths.push(end.saturating_sub(start).saturating_sub(line_start));
    }

    lengths
}

fn count_line_breaks_in_bytes(bytes: &[u8]) -> usize {
    let mut count = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'\n' => {
                count += 1;
                i += 1;
            }
            b'\r' => {
                count += 1;
                if i + 1 < bytes.len() && bytes[i + 1] == b'\n' {
                    i += 2;
                } else {
                    i += 1;
                }
            }
            _ => i += 1,
        }
    }
    count
}

#[derive(Debug)]
pub(crate) enum LineOffsets {
    U32(Vec<u32>),
    U64(Vec<u64>),
}

impl Default for LineOffsets {
    fn default() -> Self {
        Self::U32(vec![0])
    }
}

impl LineOffsets {
    pub(crate) fn new_for_file_len(file_len: usize) -> Self {
        if file_len <= u32::MAX as usize {
            let cap = Self::capacity_for::<u32>(file_len);
            let mut v = Vec::with_capacity(cap);
            v.push(0);
            Self::U32(v)
        } else {
            let cap = Self::capacity_for::<u64>(file_len);
            let mut v = Vec::with_capacity(cap);
            v.push(0);
            Self::U64(v)
        }
    }

    pub(crate) fn len(&self) -> usize {
        match self {
            Self::U32(v) => v.len(),
            Self::U64(v) => v.len(),
        }
    }

    pub(crate) fn get_usize(&self, idx: usize) -> Option<usize> {
        match self {
            Self::U32(v) => v.get(idx).copied().map(|v| v as usize),
            Self::U64(v) => v.get(idx).copied().map(|v| v as usize),
        }
    }

    fn capacity_for<T>(file_len: usize) -> usize {
        let max_offsets = (MAX_LINE_OFFSETS_BYTES / std::mem::size_of::<T>()).max(1);
        let est_lines = if file_len == 0 {
            1
        } else {
            (file_len / AVG_LINE_LEN_ESTIMATE).saturating_add(2)
        };
        est_lines.min(max_offsets).max(1)
    }
}

#[derive(Debug)]
struct InlineOpenAnalysis {
    line_offsets: LineOffsets,
    line_ending: LineEnding,
    avg_line_len: usize,
}

fn analyze_inline_open(bytes: &[u8]) -> InlineOpenAnalysis {
    let file_len = bytes.len();
    let avg_line_len = |line_breaks: usize| {
        if file_len == 0 {
            AVG_LINE_LEN_ESTIMATE
        } else {
            file_len.div_ceil(line_breaks.saturating_add(1)).max(1)
        }
    };

    let mut detected_line_ending = None;

    if file_len <= u32::MAX as usize {
        let mut offsets = Vec::with_capacity(LineOffsets::capacity_for::<u32>(file_len));
        offsets.push(0);
        let mut line_breaks = 0usize;
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
            line_breaks += 1;
        }
        return InlineOpenAnalysis {
            line_offsets: LineOffsets::U32(offsets),
            line_ending: detected_line_ending.unwrap_or(LineEnding::Lf),
            avg_line_len: avg_line_len(line_breaks),
        };
    }

    let mut offsets = Vec::with_capacity(LineOffsets::capacity_for::<u64>(file_len));
    offsets.push(0);
    let mut line_breaks = 0usize;
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
        line_breaks += 1;
    }
    InlineOpenAnalysis {
        line_offsets: LineOffsets::U64(offsets),
        line_ending: detected_line_ending.unwrap_or(LineEnding::Lf),
        avg_line_len: avg_line_len(line_breaks),
    }
}

fn estimate_avg_line_len(bytes: &[u8]) -> usize {
    let len = bytes.len();
    if len == 0 {
        return AVG_LINE_LEN_ESTIMATE;
    }

    let sample = AVG_LINE_LEN_SAMPLE_BYTES.min(len);
    let mut total_bytes = 0usize;
    let mut total_lines = 0usize;

    let mut add_sample = |start: usize| {
        let end = (start + sample).min(len);
        if end <= start {
            return;
        }
        let slice = &bytes[start..end];
        let mut newlines = 0usize;
        for rel in memchr2_iter(b'\n', b'\r', slice) {
            let i = start + rel;
            let b = bytes[i];
            if b == b'\r' && i + 1 < len && bytes[i + 1] == b'\n' {
                continue;
            }
            newlines += 1;
        }
        total_bytes = total_bytes.saturating_add(slice.len());
        total_lines = total_lines.saturating_add(newlines + 1);
    };

    let mut starts = vec![0];
    if len > sample {
        starts.push(len.saturating_sub(sample));
    }
    if len > sample * 2 {
        starts.push(len / 4);
        starts.push(len / 2 - sample / 2);
        starts.push((len * 3 / 4).saturating_sub(sample / 2));
    }
    starts.sort_unstable();
    starts.dedup();
    for start in starts {
        add_sample(start.min(len.saturating_sub(sample)));
    }

    if total_lines == 0 {
        AVG_LINE_LEN_ESTIMATE
    } else {
        total_bytes.div_ceil(total_lines).max(1)
    }
}

fn utf8_char_len(first: u8) -> usize {
    if first < 0x80 {
        1
    } else if first < 0xE0 {
        2
    } else if first < 0xF0 {
        3
    } else if first < 0xF8 {
        4
    } else {
        1
    }
}

#[inline]
fn utf8_step(bytes: &[u8], start: usize, end: usize) -> usize {
    let remaining = end.saturating_sub(start);
    if remaining == 0 {
        return 0;
    }

    let width = utf8_char_len(bytes[start]).min(remaining);
    if width <= 1 {
        return 1;
    }
    if utf8_char_is_well_formed(bytes, start, width) {
        width
    } else {
        1
    }
}

#[inline]
fn is_utf8_continuation(b: u8) -> bool {
    (b & 0b1100_0000) == 0b1000_0000
}

#[inline]
fn utf8_char_is_well_formed(bytes: &[u8], start: usize, width: usize) -> bool {
    if start.saturating_add(width) > bytes.len() {
        return false;
    }

    let slice = &bytes[start..start + width];
    match width {
        1 => slice[0] < 0x80,
        2 => is_utf8_continuation(slice[1]),
        3 => match slice[0] {
            0xE0 => matches!(slice[1], 0xA0..=0xBF) && is_utf8_continuation(slice[2]),
            0xE1..=0xEC | 0xEE..=0xEF => {
                is_utf8_continuation(slice[1]) && is_utf8_continuation(slice[2])
            }
            0xED => matches!(slice[1], 0x80..=0x9F) && is_utf8_continuation(slice[2]),
            _ => false,
        },
        4 => match slice[0] {
            0xF0 => {
                matches!(slice[1], 0x90..=0xBF)
                    && is_utf8_continuation(slice[2])
                    && is_utf8_continuation(slice[3])
            }
            0xF1..=0xF3 => {
                is_utf8_continuation(slice[1])
                    && is_utf8_continuation(slice[2])
                    && is_utf8_continuation(slice[3])
            }
            0xF4 => {
                matches!(slice[1], 0x80..=0x8F)
                    && is_utf8_continuation(slice[2])
                    && is_utf8_continuation(slice[3])
            }
            _ => false,
        },
        _ => false,
    }
}

#[inline]
fn count_text_columns(bytes: &[u8], max_cols: usize) -> usize {
    let mut cols = 0usize;
    let mut i = 0usize;
    while i < bytes.len() && cols < max_cols {
        if matches!(bytes[i], b'\n' | b'\r') {
            break;
        }
        i += utf8_step(bytes, i, bytes.len());
        cols += 1;
    }
    cols
}

#[derive(Debug, Clone, Copy)]
struct CursorScanState {
    target: usize,
    seen: usize,
    line0: usize,
    col0: usize,
    prev_was_cr: bool,
}

impl CursorScanState {
    fn new(target: usize) -> Self {
        Self {
            target,
            seen: 0,
            line0: 0,
            col0: 0,
            prev_was_cr: false,
        }
    }

    fn is_done(self) -> bool {
        self.seen >= self.target
    }

    fn position(self) -> (usize, usize) {
        (self.line0, self.col0)
    }
}

fn scan_cursor_position_bytes(bytes: &[u8], state: &mut CursorScanState) {
    let mut i = 0usize;
    while i < bytes.len() && !state.is_done() {
        match bytes[i] {
            b'\n' => {
                state.seen = state.seen.saturating_add(1);
                if !state.prev_was_cr {
                    state.line0 = state.line0.saturating_add(1);
                }
                state.col0 = 0;
                state.prev_was_cr = false;
                i += 1;
            }
            b'\r' => {
                state.seen = state.seen.saturating_add(1);
                state.line0 = state.line0.saturating_add(1);
                state.col0 = 0;
                state.prev_was_cr = true;
                i += 1;
            }
            _ => {
                state.prev_was_cr = false;
                state.seen = state.seen.saturating_add(1);
                state.col0 = state.col0.saturating_add(1);
                i += utf8_step(bytes, i, bytes.len());
            }
        }
    }
}

fn align_utf8_boundary_backward(bytes: &[u8], offset: usize) -> usize {
    let offset = offset.min(bytes.len());
    if offset == 0 || offset == bytes.len() {
        return offset;
    }

    let Ok(text) = std::str::from_utf8(bytes) else {
        return offset;
    };
    let mut aligned = offset;
    while aligned > 0 && !text.is_char_boundary(aligned) {
        aligned -= 1;
    }
    aligned
}

fn align_utf8_boundary_forward(bytes: &[u8], offset: usize) -> usize {
    let offset = offset.min(bytes.len());
    if offset == 0 || offset == bytes.len() {
        return offset;
    }

    let Ok(text) = std::str::from_utf8(bytes) else {
        return offset;
    };
    let mut aligned = offset;
    while aligned < bytes.len() && !text.is_char_boundary(aligned) {
        aligned += 1;
    }
    aligned
}

fn mmap_line_byte_range(
    offsets: Option<&LineOffsets>,
    file_len: usize,
    line0: usize,
    indexing_complete: bool,
) -> Option<(usize, usize)> {
    let offsets = offsets?;
    let start0 = offsets.get_usize(line0)?.min(file_len);
    let end0 = match offsets.get_usize(line0.saturating_add(1)) {
        Some(end0) => end0.min(file_len),
        None if indexing_complete => file_len,
        None => return None,
    };
    Some((start0, end0.max(start0)))
}

fn byte_offset_for_text_col_in_bytes(
    bytes: &[u8],
    line_range: (usize, usize),
    col0: usize,
) -> usize {
    let (start, end) = line_range;
    if col0 == 0 || start >= end {
        return start.min(end);
    }

    let mut col = 0usize;
    let mut offset = start;
    let mut i = start;
    while i < end && col < col0 {
        let b = bytes[i];
        if b == b'\n' || b == b'\r' {
            break;
        }
        let step = utf8_step(bytes, i, end);
        col += 1;
        i += step;
        offset += step;
    }
    offset.min(end)
}

fn advance_offset_by_text_units_in_bytes(
    bytes: &[u8],
    file_len: usize,
    start: usize,
    text_units: usize,
) -> usize {
    let start = start.min(file_len);
    if text_units == 0 || start >= file_len {
        return start;
    }

    let mut remaining = text_units;
    let mut offset = start;
    let mut pending_cr = false;
    while offset < file_len && (remaining > 0 || pending_cr) {
        if pending_cr {
            pending_cr = false;
            if bytes[offset] == b'\n' {
                offset += 1;
                continue;
            }
        }
        if remaining == 0 {
            break;
        }

        match bytes[offset] {
            b'\r' => {
                remaining -= 1;
                offset += 1;
                pending_cr = true;
            }
            b'\n' => {
                remaining -= 1;
                offset += 1;
            }
            _ => {
                let step = utf8_step(bytes, offset, file_len);
                remaining -= 1;
                offset += step;
            }
        }
    }
    offset.min(file_len)
}

#[derive(Debug)]
enum OffsetsChunk {
    U32(Vec<u32>),
    U64(Vec<u64>),
}

/// Text document with mmap-backed reads, background line indexing, and lazy
/// promotion to a mutable editing buffer.
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

    // Mutable text storage. When present, it becomes the source of truth for rendering/editing.
    rope: Option<Rope>,
    piece_table: Option<PieceTable>,
    dirty: bool,
}

#[derive(Debug)]
pub(crate) struct PieceTable {
    original: FileStorage,
    add: Vec<u8>,
    pieces: PieceTree,
    known_line_count: usize,
    known_byte_len: usize,
    total_len: usize,
    full_index: bool,
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

struct PieceTableLineSliceCollector {
    target_lines: usize,
    start_col: usize,
    max_cols: usize,
    slices: Vec<LineSlice>,
    line_buf: Vec<u8>,
    line_col: usize,
    visible_cols: usize,
    pending_cr: bool,
}

impl PieceTableLineSliceCollector {
    fn new(target_lines: usize, start_col: usize, max_cols: usize) -> Self {
        Self {
            target_lines,
            start_col,
            max_cols,
            slices: Vec::with_capacity(target_lines),
            line_buf: Vec::with_capacity(max_cols.min(256).saturating_mul(4)),
            line_col: 0,
            visible_cols: 0,
            pending_cr: false,
        }
    }

    fn is_done(&self) -> bool {
        self.slices.len() >= self.target_lines
    }

    fn push_segment(&mut self, bytes: &[u8]) {
        let mut i = 0usize;
        while i < bytes.len() && !self.is_done() {
            let b = bytes[i];
            if self.pending_cr {
                self.pending_cr = false;
                if b == b'\n' {
                    i += 1;
                    continue;
                }
            }

            match b {
                b'\n' => {
                    self.finish_line();
                    i += 1;
                }
                b'\r' => {
                    self.finish_line();
                    self.pending_cr = true;
                    i += 1;
                }
                _ => {
                    let step = utf8_step(bytes, i, bytes.len());
                    if self.line_col >= self.start_col && self.visible_cols < self.max_cols {
                        self.line_buf.extend_from_slice(&bytes[i..i + step]);
                        self.visible_cols += 1;
                    }
                    self.line_col += 1;
                    i += step;
                }
            }
        }
    }

    fn finish_eof(&mut self) {
        if !self.is_done() {
            self.finish_line();
        }
    }

    fn into_slices(self) -> Vec<LineSlice> {
        self.slices
    }

    fn finish_line(&mut self) {
        if self.is_done() {
            return;
        }
        let text = String::from_utf8(std::mem::take(&mut self.line_buf))
            .unwrap_or_else(|err| String::from_utf8_lossy(&err.into_bytes()).into_owned());
        self.slices.push(LineSlice::new(text, true));
        self.line_col = 0;
        self.visible_cols = 0;
    }
}

impl PieceTable {
    fn new(original: FileStorage, mut line_lengths: Vec<usize>, full_index: bool) -> Self {
        let total_len = original.len();
        if line_lengths.is_empty() {
            line_lengths.push(total_len);
        }
        let known_byte_len = line_lengths.iter().copied().sum::<usize>().min(total_len);
        let known_line_count = line_lengths.len().max(1);
        let pieces =
            Self::build_initial_piece_tree(&original, total_len, &line_lengths, known_byte_len);

        Self {
            original,
            add: Vec::new(),
            pieces,
            known_line_count,
            known_byte_len,
            total_len,
            full_index,
            pending_session_flush: false,
            pending_session_edits: 0,
            last_session_flush: None,
            edit_batch_depth: 0,
            edit_batch_dirty: false,
        }
    }

    fn from_recovered_session(
        original: FileStorage,
        add: Vec<u8>,
        pieces: PieceTree,
        meta: SessionMeta,
    ) -> Self {
        let total_len = pieces.total_len();
        let known_line_count = pieces.total_line_breaks().saturating_add(1).max(1);
        let known_byte_len = meta.known_byte_len.min(total_len);

        Self {
            original,
            add,
            pieces,
            known_line_count,
            known_byte_len,
            total_len,
            full_index: meta.full_index,
            pending_session_flush: false,
            pending_session_edits: 0,
            last_session_flush: None,
            edit_batch_depth: 0,
            edit_batch_dirty: false,
        }
    }

    pub(crate) fn line_count(&self) -> usize {
        self.known_line_count.max(1)
    }

    pub(crate) fn total_len(&self) -> usize {
        self.total_len
    }

    pub(crate) fn full_index(&self) -> bool {
        self.full_index
    }

    fn session_meta(&self) -> SessionMeta {
        SessionMeta {
            known_byte_len: self.known_byte_len,
            full_index: self.full_index,
        }
    }

    fn flush_session(&mut self) -> io::Result<()> {
        self.flush_session_inner(true)
    }

    fn schedule_session_flush(&mut self) -> io::Result<()> {
        self.pending_session_flush = true;
        self.pending_session_edits = self.pending_session_edits.saturating_add(1);
        if self.edit_batch_depth > 0 {
            self.edit_batch_dirty = true;
            return Ok(());
        }
        self.flush_session_inner(false)
    }

    fn flush_session_inner(&mut self, force: bool) -> io::Result<()> {
        if !force && !self.pending_session_flush {
            return Ok(());
        }
        if !force {
            let debounce_elapsed = self
                .last_session_flush
                .map(|instant| instant.elapsed() >= PIECE_SESSION_FLUSH_DEBOUNCE)
                .unwrap_or(true);
            if !debounce_elapsed && self.pending_session_edits < PIECE_SESSION_FORCE_AFTER_EDITS {
                return Ok(());
            }
        }

        match self.pieces.flush_session(&self.add, self.session_meta()) {
            Ok(()) => {
                self.pending_session_flush = false;
                self.pending_session_edits = 0;
                self.last_session_flush = Some(Instant::now());
                Ok(())
            }
            Err(err) => {
                self.pending_session_flush = false;
                self.pending_session_edits = 0;
                self.last_session_flush = None;
                self.pieces.detach_persistence();
                Err(err)
            }
        }
    }

    fn begin_edit_batch(&mut self) {
        self.edit_batch_depth = self.edit_batch_depth.saturating_add(1);
        self.pieces.begin_batch_edit();
    }

    fn end_edit_batch(&mut self) -> io::Result<()> {
        if self.edit_batch_depth == 0 {
            return Ok(());
        }

        self.edit_batch_depth -= 1;
        self.pieces.end_batch_edit();
        if self.edit_batch_depth == 0 && self.edit_batch_dirty {
            self.edit_batch_dirty = false;
            self.flush_session_inner(false)?;
        }
        Ok(())
    }

    pub(crate) fn has_line(&self, line0: usize) -> bool {
        line0 < self.line_count()
    }

    pub(crate) fn line_len_chars(&self, line0: usize) -> usize {
        let (start, end) = self.line_range(line0);
        if start >= end {
            return 0;
        }
        let mut col = 0usize;
        let mut done = false;
        self.pieces
            .visit_range(start, end, |piece, local_start, local_end| {
                if done || col >= MAX_LINE_SCAN_CHARS {
                    return;
                }
                let seg_start = piece.start + local_start;
                let seg_end = piece.start + local_end;
                let src = self.source_bytes(piece.src);
                let mut i = seg_start;
                while i < seg_end && col < MAX_LINE_SCAN_CHARS {
                    let b = src[i];
                    if b == b'\n' || b == b'\r' {
                        done = true;
                        return;
                    }
                    let step = utf8_step(src, i, seg_end);
                    col += 1;
                    i += step;
                }
            });
        col.min(MAX_LINE_SCAN_CHARS)
    }

    pub(crate) fn line_visible_segment(
        &self,
        line0: usize,
        start_col: usize,
        max_cols: usize,
    ) -> String {
        if max_cols == 0 || line0 >= self.line_count() {
            return String::new();
        }
        let (line_start, line_end) = self.line_range(line0);
        if line_start >= line_end {
            return String::new();
        }
        let start = self.byte_offset_for_col(line0, start_col);
        if start >= line_end {
            return String::new();
        }

        let mut out = Vec::with_capacity(max_cols.min(4096).saturating_mul(4));
        let mut cols = 0usize;
        self.pieces
            .visit_range(start, line_end, |piece, local_start, local_end| {
                if cols >= max_cols {
                    return;
                }
                let seg_start = piece.start + local_start;
                let seg_end = piece.start + local_end;
                let src = self.source_bytes(piece.src);
                let mut i = seg_start;
                while i < seg_end && cols < max_cols {
                    let b = src[i];
                    if b == b'\n' || b == b'\r' {
                        cols = max_cols;
                        break;
                    }
                    let step = utf8_step(src, i, seg_end);
                    out.extend_from_slice(&src[i..i + step]);
                    cols += 1;
                    i += step;
                }
            });

        String::from_utf8(out)
            .unwrap_or_else(|err| String::from_utf8_lossy(&err.into_bytes()).into_owned())
    }

    pub(crate) fn line_slices_exact(
        &self,
        first_line0: usize,
        line_count: usize,
        start_col: usize,
        max_cols: usize,
    ) -> Vec<LineSlice> {
        if line_count == 0 {
            return Vec::new();
        }
        if first_line0 >= self.line_count() {
            return vec![LineSlice::new(String::new(), true); line_count];
        }

        let available = self
            .line_count()
            .saturating_sub(first_line0)
            .min(line_count);
        let Some(start) = self.line_start_byte(first_line0) else {
            return vec![LineSlice::new(String::new(), true); line_count];
        };
        if start >= self.known_byte_len {
            return vec![LineSlice::new(String::new(), true); line_count];
        }

        let mut collector = PieceTableLineSliceCollector::new(available, start_col, max_cols);
        self.pieces.visit_range(
            start,
            self.known_byte_len,
            |piece, local_start, local_end| {
                if collector.is_done() {
                    return;
                }
                let seg_start = piece.start + local_start;
                let seg_end = piece.start + local_end;
                let src = self.source_bytes(piece.src);
                collector.push_segment(&src[seg_start..seg_end]);
            },
        );
        collector.finish_eof();

        let mut slices = collector.into_slices();
        slices.resize(line_count, LineSlice::new(String::new(), true));
        slices
    }

    pub(crate) fn insert_text_at(
        &mut self,
        line_ending: LineEnding,
        line0: usize,
        col0: usize,
        text: &str,
    ) -> io::Result<EditOutcome> {
        let actual_col0 = self.line_len_chars(line0);
        let insert_col0 = col0.min(actual_col0);
        let virtual_padding_cols = col0.saturating_sub(actual_col0);
        let insert_at = self.byte_offset_for_col(line0, insert_col0);
        let (normalized, added_lines, last_col) =
            normalize_insert_text(text, virtual_padding_cols, line_ending);

        let bytes = normalized.as_bytes();
        if !bytes.is_empty() {
            self.insert_bytes(insert_at, bytes)?;
            if insert_at <= self.known_byte_len {
                self.known_byte_len = self.known_byte_len.saturating_add(bytes.len());
            }
            self.refresh_known_line_count();
        }

        let cursor = if added_lines == 0 {
            (line0, col0.saturating_add(last_col))
        } else {
            (line0.saturating_add(added_lines), last_col)
        };
        Ok(EditOutcome::new(!bytes.is_empty(), cursor))
    }

    pub(crate) fn replace_range_at(
        &mut self,
        line_ending: LineEnding,
        line0: usize,
        col0: usize,
        len_chars: usize,
        text: &str,
    ) -> io::Result<EditOutcome> {
        if len_chars == 0 {
            return self.insert_text_at(line_ending, line0, col0, text);
        }

        let actual_col0 = self.line_len_chars(line0);
        let start_col0 = col0.min(actual_col0);
        let start = self.byte_offset_for_col(line0, start_col0);
        let end = self.advance_offset_by_text_units(start, len_chars);
        let (normalized, added_lines, last_col) = normalize_insert_text(text, 0, line_ending);
        let existing = if end > start {
            self.read_range(start, end)
        } else {
            Vec::new()
        };
        let cursor = if added_lines == 0 {
            (line0, start_col0.saturating_add(last_col))
        } else {
            (line0.saturating_add(added_lines), last_col)
        };
        if existing == normalized.as_bytes() {
            return Ok(EditOutcome::new(false, cursor));
        }

        self.begin_edit_batch();
        let result = (|| -> io::Result<EditOutcome> {
            let mut edited = false;
            if end > start {
                self.delete_range(start, end - start)?;
                edited = true;
            }
            let outcome = self.insert_text_at(line_ending, line0, start_col0, text)?;
            Ok(EditOutcome::new(edited || outcome.edited, outcome.cursor))
        })();
        let end_batch = self.end_edit_batch();
        let outcome = result?;
        end_batch?;
        Ok(EditOutcome::new(true, outcome.cursor))
    }

    pub(crate) fn backspace_at(
        &mut self,
        line0: usize,
        col0: usize,
    ) -> io::Result<(bool, usize, usize)> {
        if self.total_len == 0 {
            return Ok((false, line0, col0));
        }
        if col0 > 0 {
            let actual_col0 = self.line_len_chars(line0);
            if col0 > actual_col0 {
                return Ok((false, line0, col0.saturating_sub(1)));
            }
            let cur_byte = self.byte_offset_for_col(line0, col0);
            let prev_byte = self.byte_offset_for_col(line0, col0.saturating_sub(1));
            let len = cur_byte.saturating_sub(prev_byte);
            if len == 0 {
                return Ok((false, line0, col0));
            }
            self.delete_range(prev_byte, len)?;
            return Ok((true, line0, col0.saturating_sub(1)));
        }

        if line0 == 0 {
            return Ok((false, line0, col0));
        }
        let line_start = self.line_range(line0).0;
        let newline_len = self.newline_len_before(line_start);
        if newline_len == 0 {
            return Ok((false, line0, col0));
        }
        let del_start = line_start.saturating_sub(newline_len);
        self.delete_range(del_start, newline_len)?;
        let new_line0 = line0.saturating_sub(1);
        let new_col0 = self.line_len_chars(new_line0);
        Ok((true, new_line0, new_col0))
    }

    pub(crate) fn position_for_char_index(&self, char_index: usize) -> (usize, usize) {
        let mut state = CursorScanState::new(char_index);
        if self.total_len == 0 || state.is_done() {
            return state.position();
        }

        self.pieces
            .visit_range(0, self.total_len, |piece, local_start, local_end| {
                if state.is_done() {
                    return;
                }
                let seg_start = piece.start + local_start;
                let seg_end = piece.start + local_end;
                let src = self.source_bytes(piece.src);
                scan_cursor_position_bytes(&src[seg_start..seg_end], &mut state);
            });

        state.position()
    }

    pub(crate) fn to_string_lossy(&self) -> String {
        let bytes = self.read_range(0, self.total_len);
        String::from_utf8_lossy(&bytes).to_string()
    }

    fn source_bytes(&self, src: PieceSource) -> &[u8] {
        match src {
            PieceSource::Original => self.original.read_range(0, self.original.len()),
            PieceSource::Add => &self.add,
        }
    }

    fn line_range(&self, line0: usize) -> (usize, usize) {
        if line0 >= self.line_count() {
            return (self.total_len, self.total_len);
        }
        let start = self.line_start_byte(line0).unwrap_or(self.known_byte_len);
        let end = if line0 + 1 < self.line_count() {
            self.line_start_byte(line0 + 1)
                .unwrap_or(self.known_byte_len)
        } else {
            self.known_byte_len
        };
        (
            start.min(self.total_len),
            end.max(start).min(self.total_len),
        )
    }

    fn read_range(&self, start: usize, end: usize) -> Vec<u8> {
        if start >= end || start >= self.total_len {
            return Vec::new();
        }
        let end = end.min(self.total_len);
        let mut out = Vec::with_capacity(end - start);
        self.pieces
            .visit_range(start, end, |piece, local_start, local_end| {
                let seg_start = piece.start + local_start;
                let seg_end = piece.start + local_end;
                let src = self.source_bytes(piece.src);
                out.extend_from_slice(&src[seg_start..seg_end]);
            });
        out
    }

    fn byte_at(&self, offset: usize) -> Option<u8> {
        if offset >= self.total_len {
            return None;
        }
        let mut found = None;
        self.pieces
            .visit_range(offset, offset.saturating_add(1), |piece, local_start, _| {
                if found.is_some() {
                    return;
                }
                let src = self.source_bytes(piece.src);
                found = src.get(piece.start + local_start).copied();
            });
        found
    }

    fn byte_offset_for_col(&self, line0: usize, col0: usize) -> usize {
        let (start, end) = self.line_range(line0);
        if col0 == 0 || start >= end {
            return start;
        }
        let mut col = 0usize;
        let mut offset = start;
        self.pieces
            .visit_range(start, end, |piece, local_start, local_end| {
                if col >= col0 {
                    return;
                }
                let seg_start = piece.start + local_start;
                let seg_end = piece.start + local_end;
                let src = self.source_bytes(piece.src);
                let mut i = seg_start;
                while i < seg_end && col < col0 {
                    let b = src[i];
                    if b == b'\n' || b == b'\r' {
                        col = col0;
                        return;
                    }
                    let step = utf8_step(src, i, seg_end);
                    col += 1;
                    i += step;
                    offset += step;
                }
            });
        offset.min(end)
    }

    fn advance_offset_by_text_units(&self, start: usize, text_units: usize) -> usize {
        let start = start.min(self.total_len);
        if text_units == 0 || start >= self.total_len {
            return start;
        }

        let mut remaining = text_units;
        let mut offset = start;
        let mut pending_cr = false;
        self.pieces
            .visit_range(start, self.total_len, |piece, local_start, local_end| {
                if remaining == 0 && !pending_cr {
                    return;
                }
                let seg_start = piece.start + local_start;
                let seg_end = piece.start + local_end;
                let src = self.source_bytes(piece.src);
                let mut i = seg_start;
                while i < seg_end && (remaining > 0 || pending_cr) {
                    if pending_cr {
                        pending_cr = false;
                        if src[i] == b'\n' {
                            i += 1;
                            offset = offset.saturating_add(1);
                            continue;
                        }
                    }
                    if remaining == 0 {
                        break;
                    }

                    match src[i] {
                        b'\r' => {
                            remaining -= 1;
                            i += 1;
                            offset = offset.saturating_add(1);
                            pending_cr = true;
                        }
                        b'\n' => {
                            remaining -= 1;
                            i += 1;
                            offset = offset.saturating_add(1);
                        }
                        _ => {
                            let step = utf8_step(src, i, seg_end);
                            remaining -= 1;
                            i += step;
                            offset = offset.saturating_add(step);
                        }
                    }
                }
            });
        offset.min(self.total_len)
    }

    fn insert_bytes(&mut self, pos: usize, bytes: &[u8]) -> io::Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        let add_start = self.add.len();
        self.add.extend_from_slice(bytes);
        let new_piece = Piece {
            src: PieceSource::Add,
            start: add_start,
            len: bytes.len(),
            line_breaks: count_line_breaks_in_bytes(bytes),
        };
        let original = &self.original;
        let add = &self.add;
        let mut split_piece = |piece: Piece, left_len: usize| {
            split_piece_with_sources(original, add, piece, left_len)
        };
        self.pieces
            .insert(pos.min(self.total_len), new_piece, &mut split_piece);
        self.total_len = self.pieces.total_len();
        if self.full_index {
            self.known_byte_len = self.total_len;
        }
        self.schedule_session_flush()
    }

    fn delete_range(&mut self, start: usize, len: usize) -> io::Result<()> {
        if len == 0 || start >= self.total_len {
            return Ok(());
        }
        let end = start.saturating_add(len).min(self.total_len);
        let known_overlap = end
            .min(self.known_byte_len)
            .saturating_sub(start.min(self.known_byte_len));
        let original = &self.original;
        let add = &self.add;
        let mut trim_piece = |piece: Piece, local_start: usize, local_end: usize| {
            trim_piece_with_sources(original, add, piece, local_start, local_end)
        };
        self.pieces.delete_range(start, len, &mut trim_piece);
        self.total_len = self.pieces.total_len();
        if self.full_index {
            self.known_byte_len = self.total_len;
        } else if known_overlap > 0 {
            self.known_byte_len = self.known_byte_len.saturating_sub(known_overlap);
        }
        self.refresh_known_line_count();
        self.schedule_session_flush()
    }

    fn newline_len_before(&self, line_start: usize) -> usize {
        if line_start == 0 {
            return 0;
        }
        let b1 = self.byte_at(line_start - 1);
        if b1 == Some(b'\n') {
            if line_start >= 2 && self.byte_at(line_start - 2) == Some(b'\r') {
                return 2;
            }
            return 1;
        }
        if b1 == Some(b'\r') {
            return 1;
        }
        0
    }

    fn build_initial_piece_tree(
        original: &FileStorage,
        total_len: usize,
        line_lengths: &[usize],
        known_byte_len: usize,
    ) -> PieceTree {
        if total_len == 0 {
            return PieceTree::new();
        }

        let mut pieces = Vec::new();
        let mut start = 0usize;
        let mut chunk_len = 0usize;
        let mut chunk_breaks = 0usize;
        let mut chunk_lines = 0usize;
        let known_line_count = line_lengths.len().max(1);

        for (idx, len) in line_lengths.iter().copied().enumerate() {
            chunk_len = chunk_len.saturating_add(len);
            if idx + 1 < known_line_count {
                chunk_breaks = chunk_breaks.saturating_add(1);
            }
            chunk_lines = chunk_lines.saturating_add(1);

            let should_flush =
                chunk_len >= PIECE_TREE_TARGET_BYTES || chunk_lines >= PIECE_TREE_TARGET_LINES;
            if should_flush && chunk_len > 0 {
                pieces.push(Piece {
                    src: PieceSource::Original,
                    start,
                    len: chunk_len,
                    line_breaks: chunk_breaks,
                });
                start = start.saturating_add(chunk_len);
                chunk_len = 0;
                chunk_breaks = 0;
                chunk_lines = 0;
            }
        }

        if chunk_len > 0 {
            pieces.push(Piece {
                src: PieceSource::Original,
                start,
                len: chunk_len,
                line_breaks: chunk_breaks,
            });
        }

        if known_byte_len < total_len {
            pieces.push(Piece {
                src: PieceSource::Original,
                start: known_byte_len,
                len: total_len - known_byte_len,
                line_breaks: 0,
            });
        }

        if pieces.is_empty() {
            pieces.push(Piece {
                src: PieceSource::Original,
                start: 0,
                len: total_len,
                line_breaks: 0,
            });
        }

        if total_len >= PIECE_TREE_DISK_MIN_BYTES {
            if let Ok(tree) = PieceTree::from_pieces_disk(original.path(), pieces.clone()) {
                return tree;
            }
        }

        PieceTree::from_pieces(pieces)
    }

    fn undo(&mut self) -> io::Result<bool> {
        if !self.pieces.undo() {
            return Ok(false);
        }
        self.total_len = self.pieces.total_len();
        self.known_byte_len = self.known_byte_len.min(self.total_len);
        self.refresh_known_line_count();
        self.schedule_session_flush()?;
        Ok(true)
    }

    fn redo(&mut self) -> io::Result<bool> {
        if !self.pieces.redo() {
            return Ok(false);
        }
        self.total_len = self.pieces.total_len();
        self.known_byte_len = self.known_byte_len.min(self.total_len);
        self.refresh_known_line_count();
        self.schedule_session_flush()?;
        Ok(true)
    }

    fn refresh_known_line_count(&mut self) {
        self.known_line_count = self.pieces.total_line_breaks().saturating_add(1).max(1);
    }

    fn line_start_byte(&self, line0: usize) -> Option<usize> {
        self.pieces
            .find_line_start(line0, |piece, local_break_idx| {
                self.local_offset_after_break(piece, local_break_idx)
            })
            .filter(|offset| *offset <= self.known_byte_len)
    }

    fn local_offset_after_break(&self, piece: Piece, local_break_idx: usize) -> Option<usize> {
        let bytes = self.source_bytes(piece.src);
        let start = piece.start.min(bytes.len());
        let end = piece.start.saturating_add(piece.len).min(bytes.len());
        let mut seen = 0usize;
        let mut i = start;
        while i < end {
            match bytes[i] {
                b'\n' => {
                    if seen == local_break_idx {
                        return Some(i + 1 - start);
                    }
                    seen += 1;
                    i += 1;
                }
                b'\r' => {
                    if i + 1 < end && bytes[i + 1] == b'\n' {
                        if seen == local_break_idx {
                            return Some(i + 2 - start);
                        }
                        seen += 1;
                        i += 2;
                    } else {
                        if seen == local_break_idx {
                            return Some(i + 1 - start);
                        }
                        seen += 1;
                        i += 1;
                    }
                }
                _ => i += 1,
            }
        }
        None
    }
}

fn piece_source_bytes<'a>(original: &'a FileStorage, add: &'a [u8], src: PieceSource) -> &'a [u8] {
    match src {
        PieceSource::Original => original.read_range(0, original.len()),
        PieceSource::Add => add,
    }
}

fn count_piece_line_breaks_with_sources(
    original: &FileStorage,
    add: &[u8],
    piece: Piece,
    local_start: usize,
    local_end: usize,
) -> usize {
    let bytes = piece_source_bytes(original, add, piece.src);
    let start = piece.start.saturating_add(local_start).min(bytes.len());
    let end = piece
        .start
        .saturating_add(local_end)
        .min(bytes.len())
        .max(start);
    count_line_breaks_in_bytes(&bytes[start..end])
}

fn split_piece_with_sources(
    original: &FileStorage,
    add: &[u8],
    piece: Piece,
    left_len: usize,
) -> (Option<Piece>, Option<Piece>) {
    let bytes = piece_source_bytes(original, add, piece.src);
    let start = piece.start.min(bytes.len());
    let end = piece.start.saturating_add(piece.len).min(bytes.len());
    let left_len = align_utf8_boundary_backward(&bytes[start..end], left_len.min(piece.len));
    let right_len = piece.len.saturating_sub(left_len);
    let left = (left_len > 0).then_some(Piece {
        src: piece.src,
        start: piece.start,
        len: left_len,
        line_breaks: count_piece_line_breaks_with_sources(original, add, piece, 0, left_len),
    });
    let right = (right_len > 0).then_some(Piece {
        src: piece.src,
        start: piece.start + left_len,
        len: right_len,
        line_breaks: count_piece_line_breaks_with_sources(
            original, add, piece, left_len, piece.len,
        ),
    });
    (left, right)
}

fn trim_piece_with_sources(
    original: &FileStorage,
    add: &[u8],
    piece: Piece,
    local_start: usize,
    local_end: usize,
) -> (Option<Piece>, Option<Piece>) {
    let bytes = piece_source_bytes(original, add, piece.src);
    let start = piece.start.min(bytes.len());
    let end = piece.start.saturating_add(piece.len).min(bytes.len());
    let piece_bytes = &bytes[start..end];
    let left_len = align_utf8_boundary_backward(piece_bytes, local_start.min(piece.len));
    let right_start = align_utf8_boundary_forward(piece_bytes, local_end.min(piece.len));
    let right_len = piece.len.saturating_sub(right_start);
    let left = (left_len > 0).then_some(Piece {
        src: piece.src,
        start: piece.start,
        len: left_len,
        line_breaks: count_piece_line_breaks_with_sources(original, add, piece, 0, left_len),
    });
    let right = (right_len > 0).then_some(Piece {
        src: piece.src,
        start: piece.start + right_start,
        len: right_len,
        line_breaks: count_piece_line_breaks_with_sources(
            original,
            add,
            piece,
            right_start,
            piece.len,
        ),
    });
    (left, right)
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
mod tests;
