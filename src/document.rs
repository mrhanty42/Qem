use memchr::{memchr2, memchr2_iter};
use ropey::{Rope, RopeBuilder};
use std::io::{self, Write};
use std::iter::FusedIterator;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, RwLock};
use std::thread;
use std::time::{Duration, Instant};

use super::storage::{FileStorage, StorageOpenError};
use crate::index::DiskLineIndex;
use crate::piece_tree::{editlog_path, Piece, PieceSource, PieceTree, SessionMeta};

// Hard limits to keep mmap indexing bounded for huge files.
// We still fully index "reasonable" files (Notepad++-style), but cap the work for truly huge inputs.
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

fn visible_column_byte_range(bytes: &[u8], start_col: usize, max_cols: usize) -> (usize, usize) {
    if max_cols == 0 || bytes.is_empty() {
        return (0, 0);
    }

    let ascii_end = start_col.saturating_add(max_cols).min(bytes.len());
    if bytes[..ascii_end].is_ascii() {
        let start = start_col.min(bytes.len());
        return (start, ascii_end.max(start));
    }

    let mut i = 0usize;
    let mut col = 0usize;
    let mut start = None;
    while i < bytes.len() {
        if matches!(bytes[i], b'\n' | b'\r') {
            break;
        }
        if start.is_none() && col == start_col {
            start = Some(i);
        }
        if col >= start_col && col.saturating_sub(start_col) >= max_cols {
            break;
        }
        i += utf8_step(bytes, i, bytes.len());
        col += 1;
    }

    (start.unwrap_or(i), i)
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

fn mmap_line_visible_bytes(
    bytes: &[u8],
    line_range: Option<(usize, usize)>,
    start_col: usize,
    max_cols: usize,
) -> &[u8] {
    if bytes.is_empty() || max_cols == 0 {
        return &[];
    }

    let Some((start0, mut end0)) = line_range else {
        return &[];
    };

    if end0 > bytes.len() {
        end0 = bytes.len();
    }
    if start0 >= end0 {
        return &[];
    }

    if bytes[end0 - 1] == b'\n' {
        end0 = end0.saturating_sub(1);
    }
    if end0 > start0 && bytes[end0 - 1] == b'\r' {
        end0 = end0.saturating_sub(1);
    }
    if start0 >= end0 {
        return &[];
    }

    let line_bytes = &bytes[start0..end0];
    let (start, end) = visible_column_byte_range(line_bytes, start_col, max_cols);
    &line_bytes[start..end]
}

fn line_slice_from_bytes(
    bytes: &[u8],
    line_range: Option<(usize, usize)>,
    start_col: usize,
    max_cols: usize,
    exact: bool,
) -> LineSlice {
    let line_bytes = mmap_line_visible_bytes(bytes, line_range, start_col, max_cols);
    let text = match std::str::from_utf8(line_bytes) {
        Ok(text) => text.to_owned(),
        Err(_) => String::from_utf8_lossy(line_bytes).into_owned(),
    };

    LineSlice::new(text, exact && line_range.is_some())
}

#[derive(Clone, Copy, Debug)]
struct MmapLineScan {
    range: (usize, usize),
    complete: bool,
}

fn next_mmap_line_range(
    bytes: &[u8],
    file_len: usize,
    start0: usize,
    max_scan_bytes: usize,
) -> Option<MmapLineScan> {
    let start0 = start0.min(file_len);
    if start0 >= file_len {
        return None;
    }

    let scan_end = start0.saturating_add(max_scan_bytes).min(file_len);
    let slice = &bytes[start0..scan_end];
    let end0 = if let Some(rel) = memchr::memchr2(b'\n', b'\r', slice) {
        let idx = start0 + rel;
        if bytes[idx] == b'\r' && idx + 1 < file_len && bytes[idx + 1] == b'\n' {
            idx + 2
        } else {
            idx + 1
        }
    } else {
        scan_end
    };

    Some(MmapLineScan {
        range: (start0, end0.max(start0)),
        complete: end0 < scan_end || scan_end == file_len,
    })
}

fn trailing_mmap_line_ranges(
    bytes: &[u8],
    file_len: usize,
    line_count: usize,
    max_backscan_bytes: usize,
) -> Option<Vec<(usize, usize)>> {
    if file_len == 0 || line_count == 0 {
        return Some(Vec::new());
    }

    let mut starts = Vec::with_capacity(line_count.saturating_add(2));
    starts.push(file_len);

    let mut pos = file_len;
    let scan_floor = file_len.saturating_sub(max_backscan_bytes);
    while starts.len() < line_count.saturating_add(1) && pos > scan_floor {
        pos -= 1;
        match bytes[pos] {
            b'\n' => starts.push(pos + 1),
            b'\r' => {
                if pos + 1 >= file_len || bytes[pos + 1] != b'\n' {
                    starts.push(pos + 1);
                }
            }
            _ => {}
        }
    }

    if starts.len() < line_count.saturating_add(1) && scan_floor > 0 && pos == scan_floor {
        return None;
    }

    starts.push(0);
    starts.sort_unstable();

    let needed = line_count.min(starts.len().saturating_sub(1));
    let from = starts.len().saturating_sub(needed + 1);
    let mut ranges = Vec::with_capacity(needed);
    for i in from..starts.len().saturating_sub(1) {
        ranges.push((starts[i], starts[i + 1]));
    }
    Some(ranges)
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
    ) -> io::Result<(usize, usize)> {
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

        if added_lines == 0 {
            Ok((line0, col0.saturating_add(last_col)))
        } else {
            Ok((line0.saturating_add(added_lines), last_col))
        }
    }

    pub(crate) fn replace_range_at(
        &mut self,
        line_ending: LineEnding,
        line0: usize,
        col0: usize,
        len_chars: usize,
        text: &str,
    ) -> io::Result<(usize, usize)> {
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
        if existing == normalized.as_bytes() {
            return if added_lines == 0 {
                Ok((line0, start_col0.saturating_add(last_col)))
            } else {
                Ok((line0.saturating_add(added_lines), last_col))
            };
        }

        self.begin_edit_batch();
        let result = (|| {
            if end > start {
                self.delete_range(start, end - start)?;
            }
            self.insert_text_at(line_ending, line0, start_col0, text)
        })();
        let end_batch = self.end_edit_batch();
        let cursor = result?;
        end_batch?;
        Ok(cursor)
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

fn clear_session_sidecar(path: &Path) {
    let sidecar = editlog_path(path);
    let _ = std::fs::remove_file(sidecar);
}

fn session_sidecar_path(path: Option<&Path>, fallback: &Path) -> PathBuf {
    let source = path.unwrap_or(fallback);
    editlog_path(source)
}

/// Line slice returned by the engine for rendering or viewport reads.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LineSlice {
    text: String,
    exact: bool,
}

impl LineSlice {
    /// Creates a new line slice and marks whether it is exact.
    pub fn new(text: String, exact: bool) -> Self {
        Self { text, exact }
    }

    /// Returns the slice text.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Consumes the slice and returns the owned text.
    pub fn into_text(self) -> String {
        self.text
    }

    /// Returns `true` if the slice was produced from exact indexes rather than heuristics.
    pub fn is_exact(&self) -> bool {
        self.exact
    }

    /// Returns `true` if the slice is empty.
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }
}

/// Total document line count, represented either as an exact value or as a
/// scrolling estimate while background indexing is still incomplete.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub enum LineCount {
    Exact(usize),
    Estimated(usize),
}

impl LineCount {
    /// Returns the exact line count when it is known.
    pub fn exact(self) -> Option<usize> {
        match self {
            Self::Exact(lines) => Some(lines),
            Self::Estimated(_) => None,
        }
    }

    /// Returns the value that should be used for viewport sizing and scrolling.
    pub fn display_rows(self) -> usize {
        match self {
            Self::Exact(lines) | Self::Estimated(lines) => lines.max(1),
        }
    }

    /// Returns `true` when the total line count is exact.
    pub fn is_exact(self) -> bool {
        matches!(self, Self::Exact(_))
    }
}

struct Lines<'a> {
    doc: &'a Document,
    next_line: usize,
    total_lines: usize,
}

impl<'a> Iterator for Lines<'a> {
    type Item = LineSlice;

    fn next(&mut self) -> Option<Self::Item> {
        if self.next_line >= self.total_lines {
            return None;
        }
        let slice = self.doc.line_slice(self.next_line, 0, usize::MAX);
        self.next_line += 1;
        Some(slice)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.total_lines.saturating_sub(self.next_line);
        (remaining, Some(remaining))
    }
}

impl ExactSizeIterator for Lines<'_> {}
impl FusedIterator for Lines<'_> {}

/// File-system, mapping, and edit-capability errors produced by [`Document`].
#[derive(Debug)]
pub enum DocumentError {
    /// The source file could not be opened.
    Open { path: PathBuf, source: io::Error },
    /// The source file could not be memory-mapped.
    Map { path: PathBuf, source: io::Error },
    /// A write, rename, or reload step failed.
    Write { path: PathBuf, source: io::Error },
    /// The requested edit operation is unsupported for the current document state.
    EditUnsupported {
        path: Option<PathBuf>,
        reason: &'static str,
    },
}

impl std::fmt::Display for DocumentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Open { path, source } => write!(f, "open `{}`: {source}", path.display()),
            Self::Map { path, source } => write!(f, "mmap `{}`: {source}", path.display()),
            Self::Write { path, source } => write!(f, "write `{}`: {source}", path.display()),
            Self::EditUnsupported { path, reason } => {
                if let Some(path) = path {
                    write!(f, "edit `{}`: {reason}", path.display())
                } else {
                    write!(f, "edit: {reason}")
                }
            }
        }
    }
}

impl std::error::Error for DocumentError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Open { source, .. } | Self::Map { source, .. } | Self::Write { source, .. } => {
                Some(source)
            }
            Self::EditUnsupported { .. } => None,
        }
    }
}

impl Default for Document {
    fn default() -> Self {
        Self::new()
    }
}

impl Document {
    /// Creates an empty in-memory document with no backing file.
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
    /// # Errors
    /// Returns [`DocumentError`] if the file cannot be opened or mapped.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, DocumentError> {
        let path = path.into();
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

        Ok(Self::from_storage(path, storage))
    }

    fn from_storage(path: PathBuf, storage: FileStorage) -> Self {
        let file_len = storage.len();
        let inline_analysis = (file_len > 0 && file_len <= INLINE_FULL_INDEX_MAX_FILE_BYTES)
            .then(|| analyze_inline_open(storage.bytes()));
        let line_ending = inline_analysis
            .as_ref()
            .map(|analysis| analysis.line_ending)
            .unwrap_or_else(|| detect_line_ending(storage.bytes()));
        let disk_index = DiskLineIndex::open_or_build(&path, &storage);
        let indexing = Arc::new(AtomicBool::new(true));
        let indexing_started = Instant::now();
        let indexed_bytes = Arc::new(AtomicUsize::new(0));
        let avg_line_len = Arc::new(AtomicUsize::new(AVG_LINE_LEN_ESTIMATE));
        let use_u32_offsets = file_len <= u32::MAX as usize;
        let new_line_offsets = || Arc::new(RwLock::new(LineOffsets::new_for_file_len(file_len)));

        // Persistent piece-table sessions are only created for large documents,
        // so skipping this sidecar probe avoids useless I/O on small-file opens.
        if file_len >= PIECE_TREE_DISK_MIN_BYTES {
            if let Ok(Some((pieces, add, meta))) = PieceTree::try_open_disk_session(&path) {
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
    pub fn redo(&mut self) -> bool {
        self.try_redo().unwrap_or(false)
    }

    /// Returns `true` if the document has already been materialized as a `Rope`.
    pub fn has_rope(&self) -> bool {
        self.rope.is_some()
    }

    /// Returns `true` if the document has been promoted to a mutable editing buffer.
    pub fn has_edit_buffer(&self) -> bool {
        self.rope.is_some() || self.piece_table.is_some()
    }

    /// Returns `true` if the engine knows the exact length of every line.
    pub fn has_precise_line_lengths(&self) -> bool {
        if self.rope.is_some() {
            return true;
        }
        if let Some(piece_table) = &self.piece_table {
            return piece_table.full_index();
        }
        self.is_fully_indexed()
    }

    /// Returns `true` while background indexing of the mmap-backed file is still running.
    pub fn is_indexing(&self) -> bool {
        if self.has_edit_buffer() {
            return false;
        }
        self.indexing.load(Ordering::Relaxed)
    }

    /// Returns `true` if the file has been indexed completely.
    pub fn is_fully_indexed(&self) -> bool {
        self.indexed_bytes() >= self.file_len
    }

    /// Returns the elapsed time since indexing started.
    pub fn indexing_elapsed(&self) -> Option<Duration> {
        let started = self.indexing_started?;
        Some(started.elapsed())
    }

    /// Returns the number of source-file bytes that have already been indexed.
    pub fn indexed_bytes(&self) -> usize {
        self.indexed_bytes.load(Ordering::Relaxed)
    }

    /// Returns `(indexed_bytes, total_bytes)` while background indexing is active.
    pub fn indexing_progress(&self) -> Option<(usize, usize)> {
        if !self.is_indexing() {
            return None;
        }
        Some((self.indexed_bytes(), self.file_len()))
    }

    /// Returns the current estimate of the average line length in bytes.
    pub fn avg_line_len(&self) -> usize {
        self.avg_line_len.load(Ordering::Relaxed).max(1)
    }

    fn edit_unsupported(&self, reason: &'static str) -> DocumentError {
        DocumentError::EditUnsupported {
            path: self.path.clone(),
            reason,
        }
    }

    fn can_materialize_rope(&self, total_len: usize) -> bool {
        total_len <= MAX_ROPE_EDIT_FILE_BYTES
    }

    fn disk_index_total_lines(&self) -> Option<usize> {
        if self.rope.is_some() || self.piece_table.is_some() {
            return None;
        }
        self.disk_index.as_ref()?.total_lines()
    }

    fn disk_index_checkpoint_for_line(&self, line0: usize) -> Option<(usize, usize)> {
        if self.rope.is_some() || self.piece_table.is_some() {
            return None;
        }
        let checkpoint = self.disk_index.as_ref()?.checkpoint_for_line(line0)?;
        Some((checkpoint.line0, checkpoint.byte0))
    }

    fn estimated_mmap_line_byte_range(&self, line0: usize) -> Option<(usize, usize)> {
        let bytes = self.mmap_bytes();
        let file_len = self.file_len.min(bytes.len());
        if bytes.is_empty() || file_len == 0 {
            return None;
        }
        if let Some(total_lines) = self.disk_index_total_lines() {
            if line0 >= total_lines {
                return None;
            }
        }

        let avg_line_len = self.avg_line_len();
        let offsets = self.line_offsets.read().ok();
        let approx = if let Some(offsets) = offsets.as_deref() {
            if let Some(start0) = offsets.get_usize(line0) {
                start0
            } else if let Some((anchor_line0, anchor_byte0)) =
                self.disk_index_checkpoint_for_line(line0)
            {
                anchor_byte0.saturating_add(
                    line0
                        .saturating_sub(anchor_line0)
                        .saturating_mul(avg_line_len.max(1)),
                )
            } else {
                let anchor_line0 = offsets.len().saturating_sub(1);
                let anchor_byte0 = offsets.get_usize(anchor_line0).unwrap_or(0);
                anchor_byte0.saturating_add(
                    line0
                        .saturating_sub(anchor_line0)
                        .saturating_mul(avg_line_len.max(1)),
                )
            }
        } else if let Some((anchor_line0, anchor_byte0)) =
            self.disk_index_checkpoint_for_line(line0)
        {
            anchor_byte0.saturating_add(
                line0
                    .saturating_sub(anchor_line0)
                    .saturating_mul(avg_line_len.max(1)),
            )
        } else {
            line0.saturating_mul(avg_line_len.max(1))
        }
        .min(file_len.saturating_sub(1));

        let back_limit = approx.saturating_sub(APPROX_LINE_BACKTRACK_BYTES);
        let start0 = if approx == 0 {
            0
        } else {
            let back_slice = &bytes[back_limit..approx];
            if let Some(rel) = back_slice.iter().rposition(|b| matches!(*b, b'\n' | b'\r')) {
                let idx = back_limit + rel;
                if bytes[idx] == b'\r' && idx + 1 < file_len && bytes[idx + 1] == b'\n' {
                    idx + 2
                } else {
                    idx + 1
                }
            } else {
                back_limit
            }
        };

        let forward_limit = approx
            .saturating_add(APPROX_LINE_FORWARD_BYTES)
            .min(file_len);
        let start0 = start0.min(forward_limit);
        let forward_slice = &bytes[start0..forward_limit];
        let end0 = if let Some(rel) = memchr::memchr2(b'\n', b'\r', forward_slice) {
            let idx = start0 + rel;
            if bytes[idx] == b'\r' && idx + 1 < file_len && bytes[idx + 1] == b'\n' {
                idx + 2
            } else {
                idx + 1
            }
        } else {
            forward_limit
        };

        Some((start0.min(end0), end0.max(start0)))
    }

    /// Returns the memory-mapped bytes of the original backing file.
    ///
    /// For edited documents, this may still expose the original file contents
    /// rather than the post-edit text.
    pub fn mmap_bytes(&self) -> &[u8] {
        let Some(storage) = &self.storage else {
            return &[];
        };
        storage.read_range(0, storage.len())
    }

    /// Returns the line count without heuristic extrapolation from average line length.
    fn bounded_line_count(&self) -> usize {
        if let Some(piece_table) = &self.piece_table {
            return piece_table.line_count().max(1);
        }
        if let Some(rope) = &self.rope {
            return rope.len_lines().max(1);
        }
        if let Some(total_lines) = self.disk_index_total_lines() {
            return total_lines.max(1);
        }
        if let Ok(guard) = self.line_offsets.read() {
            guard.len().max(1)
        } else {
            1
        }
    }

    /// Returns an estimated line count that is useful while background indexing is in progress.
    fn estimated_line_count_value(&self) -> usize {
        if self.has_precise_line_lengths() {
            return self.exact_line_count().unwrap_or(1);
        }
        if let Some(total_lines) = self.disk_index_total_lines() {
            return total_lines.max(1);
        }

        let estimate = if self.file_len() == 0 {
            1
        } else {
            self.file_len().div_ceil(self.avg_line_len().max(1)).max(1)
        };
        let offsets_rows = if let Ok(guard) = self.line_offsets.read() {
            guard.len().max(1)
        } else {
            1
        };
        let piece_rows = self
            .piece_table
            .as_ref()
            .map(|piece_table| piece_table.line_count().max(1))
            .unwrap_or(1);

        estimate.max(offsets_rows).max(piece_rows)
    }

    fn exact_line_count_value(&self) -> Option<usize> {
        if let Some(piece_table) = &self.piece_table {
            if piece_table.full_index() {
                return Some(piece_table.line_count().max(1));
            }
        }
        if let Some(rope) = &self.rope {
            return Some(rope.len_lines().max(1));
        }
        if let Some(total_lines) = self.disk_index_total_lines() {
            return Some(total_lines.max(1));
        }
        if self.is_fully_indexed() {
            return Some(self.bounded_line_count().max(1));
        }
        None
    }

    /// Returns the exact document line count when it is known.
    pub fn exact_line_count(&self) -> Option<usize> {
        self.exact_line_count_value()
    }

    /// Returns the current document line count, explicitly distinguishing exact
    /// values from scrolling estimates.
    pub fn line_count(&self) -> LineCount {
        if let Some(lines) = self.exact_line_count_value() {
            LineCount::Exact(lines)
        } else {
            LineCount::Estimated(self.estimated_line_count_value())
        }
    }

    /// Returns the current document length in bytes.
    pub fn file_len(&self) -> usize {
        if let Some(piece_table) = &self.piece_table {
            return piece_table.total_len();
        }
        if let Some(rope) = &self.rope {
            return rope_save_len_bytes(rope, self.line_ending);
        }
        self.file_len
    }

    /// Returns the currently detected line ending style for the document.
    pub fn line_ending(&self) -> LineEnding {
        self.line_ending
    }

    /// Returns the full document text, applying lossy UTF-8 decoding when needed.
    pub fn text_lossy(&self) -> String {
        if let Some(rope) = &self.rope {
            return rope.to_string();
        }
        if let Some(piece_table) = &self.piece_table {
            return piece_table.to_string_lossy();
        }
        String::from_utf8_lossy(self.mmap_bytes()).to_string()
    }

    /// Returns a lazy iterator over the currently known document lines.
    ///
    /// While mmap indexing is still in progress this follows [`Document::line_count`],
    /// which is a safe lower bound until indexing completes.
    pub fn lines(&self) -> impl ExactSizeIterator<Item = LineSlice> + FusedIterator + '_ {
        Lines {
            doc: self,
            next_line: 0,
            total_lines: self.line_count().display_rows(),
        }
    }

    /// Returns the visible segment of a line for the requested line and column range.
    ///
    /// If the exact line is not yet available because indexing is incomplete,
    /// the method may return a heuristic slice and mark it via
    /// [`LineSlice::is_exact`].
    pub fn line_slice(&self, line0: usize, start_col: usize, max_cols: usize) -> LineSlice {
        if max_cols == 0 {
            return LineSlice::default();
        }

        if let Some(rope) = &self.rope {
            if line0 >= rope.len_lines() {
                return LineSlice {
                    text: String::new(),
                    exact: true,
                };
            }

            let line = rope.line(line0);
            let mut len = line.len_chars();
            if len > 0 && line.char(len - 1) == '\n' {
                len = len.saturating_sub(1);
            }
            if start_col >= len {
                return LineSlice {
                    text: String::new(),
                    exact: true,
                };
            }

            let end_col = start_col.saturating_add(max_cols).min(len);
            let slice = line.slice(start_col..end_col);
            return LineSlice::new(
                slice
                    .as_str()
                    .map(ToOwned::to_owned)
                    .unwrap_or_else(|| slice.to_string()),
                true,
            );
        }

        if let Some(piece_table) = &self.piece_table {
            if piece_table.full_index() || line0 < piece_table.line_count() {
                return LineSlice::new(
                    piece_table.line_visible_segment(line0, start_col, max_cols),
                    true,
                );
            }
        }

        let bytes = self.mmap_bytes();
        let file_len = self.file_len.min(bytes.len());
        let indexing_complete = self.is_fully_indexed();
        let offsets_guard = self.line_offsets.try_read().ok();
        let offsets: Option<&LineOffsets> = offsets_guard.as_deref();
        let exact_range = mmap_line_byte_range(offsets, file_len, line0, indexing_complete);
        let line_range = exact_range.or_else(|| self.estimated_mmap_line_byte_range(line0));
        line_slice_from_bytes(
            bytes,
            line_range,
            start_col,
            max_cols,
            exact_range.is_some(),
        )
    }

    /// Returns multiple adjacent lines in a single request.
    ///
    /// This is useful for large-file UI rendering: for mmap-backed documents it
    /// tries to reuse a single byte scan instead of performing many independent
    /// lookups.
    pub fn line_slices(
        &self,
        first_line0: usize,
        line_count: usize,
        start_col: usize,
        max_cols: usize,
    ) -> Vec<LineSlice> {
        if line_count == 0 {
            return Vec::new();
        }
        if max_cols == 0 {
            return vec![LineSlice::default(); line_count];
        }

        if let Some(rope) = &self.rope {
            return (0..line_count)
                .map(|offset| {
                    let line0 = first_line0.saturating_add(offset);
                    if line0 >= rope.len_lines() {
                        return LineSlice::new(String::new(), true);
                    }

                    let line = rope.line(line0);
                    let mut len = line.len_chars();
                    if len > 0 && line.char(len - 1) == '\n' {
                        len = len.saturating_sub(1);
                    }
                    if start_col >= len {
                        return LineSlice::new(String::new(), true);
                    }

                    let end_col = start_col.saturating_add(max_cols).min(len);
                    let slice = line.slice(start_col..end_col);
                    LineSlice::new(
                        slice
                            .as_str()
                            .map(ToOwned::to_owned)
                            .unwrap_or_else(|| slice.to_string()),
                        true,
                    )
                })
                .collect();
        }

        if let Some(piece_table) = &self.piece_table {
            let requested_end = first_line0.saturating_add(line_count);
            if piece_table.full_index() || requested_end <= piece_table.line_count() {
                return piece_table.line_slices_exact(first_line0, line_count, start_col, max_cols);
            }
            return (0..line_count)
                .map(|offset| {
                    self.line_slice(first_line0.saturating_add(offset), start_col, max_cols)
                })
                .collect();
        }

        let bytes = self.mmap_bytes();
        let file_len = self.file_len.min(bytes.len());
        if bytes.is_empty() || file_len == 0 {
            return vec![LineSlice::default(); line_count];
        }

        let indexing_complete = self.is_fully_indexed();
        if !indexing_complete {
            let estimated_total = self.estimated_line_count_value().max(1);
            let requested_end = first_line0.saturating_add(line_count);
            let tail_trigger = estimated_total.saturating_sub(line_count.saturating_mul(2).max(32));
            if requested_end >= tail_trigger {
                if let Some(ranges) = trailing_mmap_line_ranges(
                    bytes,
                    file_len,
                    line_count,
                    TAIL_FAST_PATH_MAX_BACKSCAN_BYTES,
                ) {
                    let mut slices: Vec<LineSlice> = ranges
                        .into_iter()
                        .map(|range| {
                            line_slice_from_bytes(bytes, Some(range), start_col, max_cols, false)
                        })
                        .collect();
                    slices.resize(line_count, LineSlice::default());
                    return slices;
                }
            }
        }

        let offsets_guard = self.line_offsets.try_read().ok();
        let offsets: Option<&LineOffsets> = offsets_guard.as_deref();

        let mut slices = Vec::with_capacity(line_count);
        let mut next_line0 = first_line0;
        let mut scan_start = None;

        while slices.len() < line_count {
            let Some(range) =
                mmap_line_byte_range(offsets, file_len, next_line0, indexing_complete)
            else {
                break;
            };
            scan_start = Some(range.1);
            slices.push(line_slice_from_bytes(
                bytes,
                Some(range),
                start_col,
                max_cols,
                true,
            ));
            next_line0 = next_line0.saturating_add(1);
        }

        let mut scan_start = scan_start.or_else(|| {
            self.estimated_mmap_line_byte_range(next_line0)
                .map(|(start0, _)| start0)
        });

        while slices.len() < line_count {
            let Some(start0) = scan_start else {
                break;
            };
            let Some(scanned) =
                next_mmap_line_range(bytes, file_len, start0, FALLBACK_NEXT_LINE_SCAN_BYTES)
            else {
                break;
            };
            let range = scanned.range;
            slices.push(line_slice_from_bytes(
                bytes,
                Some(range),
                start_col,
                max_cols,
                false,
            ));
            if !scanned.complete {
                break;
            }
            scan_start = (range.1 > start0).then_some(range.1);
        }

        slices.resize(line_count, LineSlice::default());
        slices
    }

    fn precise_piece_table_line_lengths(&self, indexed_complete: bool) -> Option<Vec<usize>> {
        if !indexed_complete {
            return None;
        }

        let Ok(guard) = self.line_offsets.try_read() else {
            return None;
        };
        if guard.len() > LINE_LENGTHS_MAX_SYNC_LINES {
            return None;
        }

        Some(line_lengths_from_offsets(&guard, self.file_len))
    }

    fn piece_table_line_lengths_for_edit(&self, line0: usize) -> Option<(Vec<usize>, bool)> {
        let indexed_complete = self.indexed_bytes() >= self.file_len;
        if let Some(line_lengths) = self.precise_piece_table_line_lengths(indexed_complete) {
            return Some((line_lengths, true));
        }

        let storage = self.storage.as_ref()?;
        if !indexed_complete && self.file_len <= FULL_SYNC_PIECE_TABLE_MAX_FILE_BYTES {
            if let Some(line_lengths) =
                line_lengths_from_bytes(storage.bytes(), LINE_LENGTHS_MAX_SYNC_LINES)
            {
                return Some((line_lengths, true));
            }
        }
        let required_lines = line0
            .saturating_add(1)
            .clamp(
                PARTIAL_PIECE_TABLE_TARGET_LINES,
                PARTIAL_PIECE_TABLE_MAX_LINES,
            )
            .min(LINE_LENGTHS_MAX_SYNC_LINES);
        let guard = self.line_offsets.read().ok()?;

        let mut line_lengths = prefix_line_lengths_from_offsets(&guard, required_lines);
        if line_lengths.len() < required_lines {
            let scan_start = guard.get_usize(line_lengths.len()).unwrap_or(0);
            let scanned = scan_line_lengths_from(
                storage.bytes(),
                scan_start,
                required_lines.saturating_sub(line_lengths.len()),
                PARTIAL_PIECE_TABLE_SCAN_BYTES,
            );
            line_lengths.extend(scanned);
        }

        if line_lengths.len() <= line0 {
            return None;
        }

        Some((line_lengths, false))
    }

    fn ensure_edit_buffer_for_line(&mut self, line0: usize) -> Result<(), DocumentError> {
        if self.rope.is_some() || self.piece_table.is_some() {
            return Ok(());
        }
        // Editing should stay responsive: stop the background indexer once we switch to a mutable buffer.
        self.indexing.store(false, Ordering::Relaxed);
        let use_piece_table = self.storage.is_some() && self.file_len >= PIECE_TABLE_MIN_BYTES;
        if use_piece_table {
            if let Some((line_lengths, full_index)) = self.piece_table_line_lengths_for_edit(line0)
            {
                let storage = self.storage.as_ref().expect("storage required").clone();
                self.piece_table = Some(PieceTable::new(storage, line_lengths, full_index));
                return Ok(());
            }
        }

        // On huge mmap-backed files we must never fall back to a full Rope materialization.
        self.ensure_rope()
    }

    fn ensure_rope(&mut self) -> Result<(), DocumentError> {
        if self.rope.is_some() {
            return Ok(());
        }
        if !self.can_materialize_rope(self.file_len) {
            return Err(self.edit_unsupported(
                "document is too large to materialize into a rope; editing this region is disabled",
            ));
        }
        let bytes = self.mmap_bytes();
        self.rope = Some(build_rope_from_bytes(bytes));
        Ok(())
    }

    fn promote_piece_table_to_rope(&mut self) -> Result<(), DocumentError> {
        if self.rope.is_some() {
            return Ok(());
        }

        let Some(piece_table) = self.piece_table.take() else {
            return self.ensure_rope();
        };

        if !self.can_materialize_rope(piece_table.total_len()) {
            self.piece_table = Some(piece_table);
            return Err(self.edit_unsupported(
                "document is too large to widen partial piece-table editing beyond the indexed prefix",
            ));
        }
        let bytes = piece_table.read_range(0, piece_table.total_len());
        self.rope = Some(build_rope_from_bytes(&bytes));
        Ok(())
    }

    fn rope_mut(&mut self) -> Result<&mut Rope, DocumentError> {
        self.ensure_rope()?;
        self.dirty = true;
        Ok(self
            .rope
            .as_mut()
            .expect("rope must be present after ensure_rope"))
    }

    fn rope_line_len_chars_without_newline(rope: &Rope, line0: usize) -> usize {
        let line = rope.line(line0);
        let mut len = line.len_chars();
        if len > 0 && line.char(len - 1) == '\n' {
            len = len.saturating_sub(1);
        }
        len
    }

    /// Returns the line length in characters, excluding any trailing line ending.
    pub fn line_len_chars(&self, line0: usize) -> usize {
        if let Some(piece_table) = &self.piece_table {
            if piece_table.full_index() || piece_table.has_line(line0) {
                return piece_table.line_len_chars(line0);
            }
        }
        if let Some(rope) = &self.rope {
            return Self::rope_line_len_chars_without_newline(rope, line0);
        }

        let bytes = self.mmap_bytes();
        let exact_range = self
            .line_offsets
            .read()
            .ok()
            .and_then(|offsets| {
                let start0 = offsets.get_usize(line0)?;
                let end0 = offsets
                    .get_usize(line0 + 1)
                    .or_else(|| (self.indexed_bytes() >= self.file_len).then_some(self.file_len))?;
                Some((start0, end0))
            })
            .or_else(|| self.estimated_mmap_line_byte_range(line0));
        let Some((start0, end0)) = exact_range else {
            return 0;
        };
        let start = start0.min(bytes.len());
        let mut end = end0.min(bytes.len());
        while end > start {
            let b = bytes[end - 1];
            if b == b'\n' || b == b'\r' {
                end -= 1;
            } else {
                break;
            }
        }
        count_text_columns(&bytes[start..end], MAX_LINE_SCAN_CHARS)
    }

    pub(crate) fn cursor_position_for_char_index(&self, char_index: usize) -> (usize, usize) {
        if let Some(rope) = &self.rope {
            let char_index = char_index.min(rope.len_chars());
            let line0 = rope.char_to_line(char_index);
            let line_start = rope.line_to_char(line0);
            let line_len = Self::rope_line_len_chars_without_newline(rope, line0);
            let col0 = char_index.saturating_sub(line_start).min(line_len);
            return (line0, col0);
        }

        if let Some(piece_table) = &self.piece_table {
            return piece_table.position_for_char_index(char_index);
        }

        let mut state = CursorScanState::new(char_index);
        scan_cursor_position_bytes(self.mmap_bytes(), &mut state);
        state.position()
    }

    fn line_col_to_char_index(rope: &Rope, line0: usize, col0: usize) -> usize {
        let line0 = line0.min(rope.len_lines().saturating_sub(1));
        let line_start = rope.line_to_char(line0);
        let line_len = Self::rope_line_len_chars_without_newline(rope, line0);
        line_start + col0.min(line_len)
    }

    /// Attempts to insert text at the given position and returns the new cursor coordinates.
    ///
    /// # Errors
    /// Returns [`DocumentError::EditUnsupported`] if editing would require
    /// fully materializing an excessively large file in memory.
    pub fn try_insert_text_at(
        &mut self,
        line0: usize,
        col0: usize,
        text: &str,
    ) -> Result<(usize, usize), DocumentError> {
        self.ensure_edit_buffer_for_line(line0)?;
        let piece_table_supports_line = self
            .piece_table
            .as_ref()
            .map(|piece_table| piece_table.full_index() || piece_table.has_line(line0))
            .unwrap_or(false);
        if self.piece_table.is_some() && !piece_table_supports_line {
            self.promote_piece_table_to_rope()?;
        }
        let doc_path = self.path.clone();
        if let Some(piece_table) = self.piece_table.as_mut() {
            self.dirty = true;
            let path = session_sidecar_path(doc_path.as_deref(), piece_table.original.path());
            return piece_table
                .insert_text_at(self.line_ending, line0, col0, text)
                .map_err(|source| DocumentError::Write { path, source });
        }

        let rope = self.rope_mut()?;

        let actual_col0 = Self::rope_line_len_chars_without_newline(rope, line0);
        let insert_at = Self::line_col_to_char_index(rope, line0, col0.min(actual_col0));
        let virtual_padding_cols = col0.saturating_sub(actual_col0);
        let mut added_lines = 0usize;
        let mut last_col = 0usize;
        let needs_normalization =
            text.contains('\r') || text.contains('\n') || virtual_padding_cols > 0;
        if needs_normalization {
            let (normalized, normalized_lines, normalized_last_col) =
                normalize_insert_text(text, virtual_padding_cols, LineEnding::Lf);
            added_lines = normalized_lines;
            last_col = normalized_last_col;
            rope.insert(insert_at, &normalized);
        } else {
            for ch in text.chars() {
                if ch == '\n' {
                    added_lines += 1;
                    last_col = 0;
                } else {
                    last_col += 1;
                }
            }
            rope.insert(insert_at, text);
        }
        if added_lines == 0 {
            Ok((line0, col0.saturating_add(last_col)))
        } else {
            Ok((line0.saturating_add(added_lines), last_col))
        }
    }

    /// Inserts text at the given position and returns the new cursor coordinates.
    ///
    /// On edit failure, this compatibility helper preserves the previous
    /// behavior and returns the original coordinates unchanged. Use
    /// [`Document::try_insert_text_at`] for explicit error handling.
    pub fn insert_text_at(&mut self, line0: usize, col0: usize, text: &str) -> (usize, usize) {
        self.try_insert_text_at(line0, col0, text)
            .unwrap_or((line0, col0))
    }

    fn try_delete_text_range_at_internal(
        &mut self,
        line0: usize,
        col0: usize,
        len_chars: usize,
    ) -> Result<(usize, usize), DocumentError> {
        if len_chars == 0 {
            return Ok((line0, col0));
        }

        self.ensure_edit_buffer_for_line(line0)?;
        let piece_table_supports_line = self
            .piece_table
            .as_ref()
            .map(|piece_table| piece_table.full_index() || piece_table.has_line(line0))
            .unwrap_or(false);
        if self.piece_table.is_some() && !piece_table_supports_line {
            self.promote_piece_table_to_rope()?;
        }

        let doc_path = self.path.clone();
        if let Some(piece_table) = self.piece_table.as_mut() {
            let actual_col0 = piece_table.line_len_chars(line0);
            let start_col0 = col0.min(actual_col0);
            let start = piece_table.byte_offset_for_col(line0, start_col0);
            let end = piece_table.advance_offset_by_text_units(start, len_chars);
            if end > start {
                self.dirty = true;
                let path = session_sidecar_path(doc_path.as_deref(), piece_table.original.path());
                piece_table
                    .delete_range(start, end - start)
                    .map_err(|source| DocumentError::Write { path, source })?;
            }
            return Ok((line0, start_col0));
        }

        self.ensure_rope()?;
        let rope = self
            .rope
            .as_mut()
            .expect("rope must be present after ensure_rope");
        let actual_col0 = Self::rope_line_len_chars_without_newline(rope, line0);
        let start_col0 = col0.min(actual_col0);
        let start = Self::line_col_to_char_index(rope, line0, start_col0);
        let end = start.saturating_add(len_chars).min(rope.len_chars());
        if end > start {
            rope.remove(start..end);
            self.dirty = true;
        }
        Ok((line0, start_col0))
    }

    /// Replaces `len_chars` text units starting at the given line/column.
    ///
    /// Newline sequences are treated as a single text unit for piece-table backed
    /// documents, so replacing across CRLF text behaves like a normal editor
    /// operation instead of deleting only half of the line break.
    pub fn try_replace_range(
        &mut self,
        line0: usize,
        col0: usize,
        len_chars: usize,
        text: &str,
    ) -> Result<(usize, usize), DocumentError> {
        self.ensure_edit_buffer_for_line(line0)?;
        let piece_table_supports_line = self
            .piece_table
            .as_ref()
            .map(|piece_table| piece_table.full_index() || piece_table.has_line(line0))
            .unwrap_or(false);
        if self.piece_table.is_some() && !piece_table_supports_line {
            self.promote_piece_table_to_rope()?;
        }

        let doc_path = self.path.clone();
        if let Some(piece_table) = self.piece_table.as_mut() {
            self.dirty = true;
            let path = session_sidecar_path(doc_path.as_deref(), piece_table.original.path());
            return piece_table
                .replace_range_at(self.line_ending, line0, col0, len_chars, text)
                .map_err(|source| DocumentError::Write { path, source });
        }

        let (line0, col0) = self.try_delete_text_range_at_internal(line0, col0, len_chars)?;
        self.try_insert_text_at(line0, col0, text)
    }

    /// Compatibility wrapper for [`Document::try_replace_range`].
    ///
    /// On edit failure the original coordinates are returned unchanged.
    pub fn replace_range(
        &mut self,
        line0: usize,
        col0: usize,
        len_chars: usize,
        text: &str,
    ) -> (usize, usize) {
        self.try_replace_range(line0, col0, len_chars, text)
            .unwrap_or((line0, col0))
    }

    /// Attempts to delete the character before the cursor and returns the edit
    /// result together with the new position.
    ///
    /// # Errors
    /// Returns [`DocumentError::EditUnsupported`] if editing would require
    /// fully materializing an excessively large file in memory.
    pub fn try_backspace_at(
        &mut self,
        line0: usize,
        col0: usize,
    ) -> Result<(bool, usize, usize), DocumentError> {
        self.ensure_edit_buffer_for_line(line0)?;
        let piece_table_supports_line = self
            .piece_table
            .as_ref()
            .map(|piece_table| piece_table.full_index() || piece_table.has_line(line0))
            .unwrap_or(false);
        if self.piece_table.is_some() && !piece_table_supports_line {
            self.promote_piece_table_to_rope()?;
        }
        let doc_path = self.path.clone();
        if let Some(piece_table) = self.piece_table.as_mut() {
            let path = session_sidecar_path(doc_path.as_deref(), piece_table.original.path());
            match piece_table.backspace_at(line0, col0) {
                Ok((edited, new_line0, new_col0)) => {
                    if edited {
                        self.dirty = true;
                    }
                    return Ok((edited, new_line0, new_col0));
                }
                Err(source) => {
                    self.dirty = true;
                    return Err(DocumentError::Write { path, source });
                }
            }
        }

        let rope = self.rope_mut()?;
        if rope.len_chars() == 0 {
            return Ok((false, line0, col0));
        }

        let actual_col0 = Self::rope_line_len_chars_without_newline(rope, line0);
        if col0 > actual_col0 {
            return Ok((false, line0, col0.saturating_sub(1)));
        }

        let cur = Self::line_col_to_char_index(rope, line0, col0);
        if cur == 0 {
            return Ok((false, line0, col0));
        }

        let prev_ch = rope.char(cur - 1);
        rope.remove((cur - 1)..cur);

        if prev_ch == '\n' {
            let new_line0 = line0.saturating_sub(1);
            let new_col0 = Self::rope_line_len_chars_without_newline(rope, new_line0);
            Ok((true, new_line0, new_col0))
        } else {
            Ok((true, line0, col0.saturating_sub(1)))
        }
    }

    /// Deletes the character before the cursor and returns the edit result and
    /// new position.
    ///
    /// On edit failure, this compatibility helper preserves the previous
    /// behavior and reports no change. Use [`Document::try_backspace_at`] for
    /// explicit error handling.
    pub fn backspace_at(&mut self, line0: usize, col0: usize) -> (bool, usize, usize) {
        self.try_backspace_at(line0, col0)
            .unwrap_or((false, line0, col0))
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

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::io::Write;
    use tempfile::tempdir;

    #[derive(Clone, Debug)]
    enum EditOp {
        Insert {
            line_hint: usize,
            col0: usize,
            text: String,
        },
        Replace {
            line_hint: usize,
            col0: usize,
            len_chars: usize,
            text: String,
        },
        Backspace {
            line_hint: usize,
            col0: usize,
        },
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct ModelLine {
        start: usize,
        end: usize,
        len_chars: usize,
    }

    fn model_lines(text: &str) -> Vec<ModelLine> {
        let mut lines = Vec::new();
        let mut start = 0usize;
        let mut len_chars = 0usize;

        for (idx, ch) in text.char_indices() {
            if ch == '\n' {
                lines.push(ModelLine {
                    start,
                    end: idx,
                    len_chars,
                });
                start = idx + ch.len_utf8();
                len_chars = 0;
            } else {
                len_chars = len_chars.saturating_add(1);
            }
        }

        lines.push(ModelLine {
            start,
            end: text.len(),
            len_chars,
        });
        lines
    }

    fn clamp_model_line(text: &str, line_hint: usize) -> usize {
        let lines = model_lines(text);
        line_hint.min(lines.len().saturating_sub(1))
    }

    fn byte_offset_for_col(text: &str, line: ModelLine, col0: usize) -> usize {
        let clamped = col0.min(line.len_chars);
        if clamped == line.len_chars {
            return line.end;
        }

        let mut chars_seen = 0usize;
        for (offset, _) in text[line.start..line.end].char_indices() {
            if chars_seen == clamped {
                return line.start + offset;
            }
            chars_seen = chars_seen.saturating_add(1);
        }

        line.end
    }

    fn advance_byte_offset_by_chars(text: &str, start: usize, len_chars: usize) -> usize {
        if len_chars == 0 || start >= text.len() {
            return start.min(text.len());
        }

        let mut chars_seen = 0usize;
        for (offset, _) in text[start..].char_indices() {
            if chars_seen == len_chars {
                return start + offset;
            }
            chars_seen = chars_seen.saturating_add(1);
        }

        text.len()
    }

    fn visible_segment_for_model_line(
        text: &str,
        line0: usize,
        start_col: usize,
        max_cols: usize,
    ) -> String {
        if max_cols == 0 {
            return String::new();
        }

        let lines = model_lines(text);
        let Some(line) = lines.get(line0).copied() else {
            return String::new();
        };
        if start_col >= line.len_chars {
            return String::new();
        }

        let start = byte_offset_for_col(text, line, start_col);
        let end = byte_offset_for_col(text, line, start_col.saturating_add(max_cols));
        text[start..end].to_string()
    }

    fn model_insert(
        text: &mut String,
        line_hint: usize,
        col0: usize,
        insert: &str,
    ) -> (usize, usize) {
        let lines = model_lines(text);
        let line0 = line_hint.min(lines.len().saturating_sub(1));
        let line = lines[line0];
        let insert_at = byte_offset_for_col(text, line, col0);
        let virtual_padding_cols = col0.saturating_sub(line.len_chars);
        let (normalized, added_lines, last_col) =
            normalize_insert_text(insert, virtual_padding_cols, LineEnding::Lf);
        text.insert_str(insert_at, &normalized);

        if added_lines == 0 {
            (line0, col0.saturating_add(last_col))
        } else {
            (line0.saturating_add(added_lines), last_col)
        }
    }

    fn model_delete(
        text: &mut String,
        line_hint: usize,
        col0: usize,
        len_chars: usize,
    ) -> (usize, usize) {
        if len_chars == 0 {
            return (clamp_model_line(text, line_hint), col0);
        }

        let lines = model_lines(text);
        let line0 = line_hint.min(lines.len().saturating_sub(1));
        let line = lines[line0];
        let start_col0 = col0.min(line.len_chars);
        let start = byte_offset_for_col(text, line, start_col0);
        let end = advance_byte_offset_by_chars(text, start, len_chars);
        if end > start {
            text.replace_range(start..end, "");
        }
        (line0, start_col0)
    }

    fn model_replace(
        text: &mut String,
        line_hint: usize,
        col0: usize,
        len_chars: usize,
        replacement: &str,
    ) -> (usize, usize) {
        let (line0, col0) = model_delete(text, line_hint, col0, len_chars);
        model_insert(text, line0, col0, replacement)
    }

    fn model_backspace(text: &mut String, line_hint: usize, col0: usize) -> (bool, usize, usize) {
        if text.is_empty() {
            return (false, 0, col0);
        }

        let lines = model_lines(text);
        let line0 = line_hint.min(lines.len().saturating_sub(1));
        let line = lines[line0];
        if col0 > line.len_chars {
            return (false, line0, col0.saturating_sub(1));
        }

        let cur = byte_offset_for_col(text, line, col0);
        if cur == 0 {
            return (false, line0, col0);
        }

        let (prev_start, prev_ch) = text[..cur]
            .char_indices()
            .last()
            .expect("non-empty prefix must contain a character");
        text.replace_range(prev_start..cur, "");

        if prev_ch == '\n' {
            let new_line0 = line0.saturating_sub(1);
            let new_col0 = model_lines(text)[new_line0].len_chars;
            (true, new_line0, new_col0)
        } else {
            (true, line0, col0.saturating_sub(1))
        }
    }

    fn assert_doc_matches_model(doc: &Document, expected: &str) {
        let expected_lines = model_lines(expected);

        assert_eq!(doc.text_lossy(), expected);
        assert_eq!(doc.exact_line_count(), Some(expected_lines.len()));
        assert_eq!(doc.line_count(), LineCount::Exact(expected_lines.len()));
        assert!(doc.has_precise_line_lengths());

        for (line0, line) in expected_lines.iter().copied().enumerate() {
            assert_eq!(
                doc.line_len_chars(line0),
                line.len_chars,
                "line_len_chars mismatch at line {line0}"
            );
            let visible = doc.line_slice(line0, 0, line.len_chars.saturating_add(4));
            assert!(
                visible.is_exact(),
                "line_slice should be exact at line {line0}"
            );
            assert_eq!(
                visible.text(),
                &expected[line.start..line.end],
                "line_slice text mismatch at line {line0}"
            );

            let offset_col = line.len_chars / 2;
            let partial = doc.line_slice(line0, offset_col, 3);
            assert_eq!(
                partial.text(),
                visible_segment_for_model_line(expected, line0, offset_col, 3),
                "partial line_slice mismatch at line {line0}"
            );
        }
    }

    fn apply_op_to_doc(doc: &mut Document, expected: &mut String, op: &EditOp) {
        match op {
            EditOp::Insert {
                line_hint,
                col0,
                text,
            } => {
                let line0 = clamp_model_line(expected, *line_hint);
                let expected_cursor = model_insert(expected, line0, *col0, text);
                let actual_cursor = doc.try_insert_text_at(line0, *col0, text).unwrap();
                assert_eq!(actual_cursor, expected_cursor);
            }
            EditOp::Replace {
                line_hint,
                col0,
                len_chars,
                text,
            } => {
                let line0 = clamp_model_line(expected, *line_hint);
                let expected_cursor = model_replace(expected, line0, *col0, *len_chars, text);
                let actual_cursor = doc
                    .try_replace_range(line0, *col0, *len_chars, text)
                    .unwrap();
                assert_eq!(actual_cursor, expected_cursor);
            }
            EditOp::Backspace { line_hint, col0 } => {
                let line0 = clamp_model_line(expected, *line_hint);
                let expected_cursor = model_backspace(expected, line0, *col0);
                let actual_cursor = doc.try_backspace_at(line0, *col0).unwrap();
                assert_eq!(actual_cursor, expected_cursor);
            }
        }

        assert_doc_matches_model(doc, expected);
    }

    fn apply_op_to_doc_text_only(doc: &mut Document, expected: &mut String, op: &EditOp) {
        match op {
            EditOp::Insert {
                line_hint,
                col0,
                text,
            } => {
                let line0 = clamp_model_line(expected, *line_hint);
                let expected_cursor = model_insert(expected, line0, *col0, text);
                let actual_cursor = doc.try_insert_text_at(line0, *col0, text).unwrap();
                assert_eq!(actual_cursor, expected_cursor);
            }
            EditOp::Replace {
                line_hint,
                col0,
                len_chars,
                text,
            } => {
                let line0 = clamp_model_line(expected, *line_hint);
                let expected_cursor = model_replace(expected, line0, *col0, *len_chars, text);
                let actual_cursor = doc
                    .try_replace_range(line0, *col0, *len_chars, text)
                    .unwrap();
                assert_eq!(actual_cursor, expected_cursor);
            }
            EditOp::Backspace { line_hint, col0 } => {
                let line0 = clamp_model_line(expected, *line_hint);
                let expected_cursor = model_backspace(expected, line0, *col0);
                let actual_cursor = doc.try_backspace_at(line0, *col0).unwrap();
                assert_eq!(actual_cursor, expected_cursor);
            }
        }

        assert_eq!(doc.text_lossy(), *expected);
    }

    fn op_text_strategy() -> impl Strategy<Value = String> {
        prop::collection::vec(
            prop_oneof![
                Just('a'),
                Just('b'),
                Just('c'),
                Just('x'),
                Just('y'),
                Just('z'),
                Just(' '),
                Just('\n'),
                Just('\r'),
                Just('é'),
                Just('中'),
                Just('🙂'),
            ],
            0..8,
        )
        .prop_map(|chars| chars.into_iter().collect())
    }

    fn non_empty_op_text_strategy() -> impl Strategy<Value = String> {
        prop::collection::vec(
            prop_oneof![
                Just('a'),
                Just('b'),
                Just('c'),
                Just('x'),
                Just('y'),
                Just('z'),
                Just(' '),
                Just('\n'),
                Just('\r'),
                Just('\u{00E9}'),
                Just('\u{4E2D}'),
                Just('\u{1F642}'),
            ],
            1..6,
        )
        .prop_map(|chars| chars.into_iter().collect())
    }

    fn edit_op_strategy() -> impl Strategy<Value = EditOp> {
        prop_oneof![
            (0usize..12, 0usize..24, op_text_strategy()).prop_map(|(line_hint, col0, text)| {
                EditOp::Insert {
                    line_hint,
                    col0,
                    text,
                }
            }),
            (0usize..12, 0usize..24, 0usize..12, op_text_strategy()).prop_map(
                |(line_hint, col0, len_chars, text)| {
                    EditOp::Replace {
                        line_hint,
                        col0,
                        len_chars,
                        text,
                    }
                },
            ),
            (0usize..12, 0usize..24)
                .prop_map(|(line_hint, col0)| EditOp::Backspace { line_hint, col0 }),
        ]
    }

    fn file_backed_edit_op_strategy() -> impl Strategy<Value = EditOp> {
        prop_oneof![
            (0usize..64, 0usize..12, non_empty_op_text_strategy()).prop_map(
                |(line_hint, col0, text)| EditOp::Insert {
                    line_hint,
                    col0,
                    text,
                },
            ),
            (
                0usize..64,
                0usize..12,
                0usize..8,
                non_empty_op_text_strategy(),
            )
                .prop_map(|(line_hint, col0, len_chars, text)| EditOp::Replace {
                    line_hint,
                    col0,
                    len_chars,
                    text,
                }),
            (0usize..64, 0usize..12)
                .prop_map(|(line_hint, col0)| EditOp::Backspace { line_hint, col0 }),
        ]
    }

    fn render_with_line_ending(text: &str, line_ending: LineEnding) -> String {
        match line_ending {
            LineEnding::Lf => text.to_owned(),
            LineEnding::Crlf => text.replace('\n', "\r\n"),
            LineEnding::Cr => text.replace('\n', "\r"),
        }
    }

    fn write_disk_backed_fixture(path: &Path) {
        let mut file = std::fs::File::create(path).unwrap();
        let chunk = b"abc\ndef\n".repeat(1024);
        let target_len = PIECE_TREE_DISK_MIN_BYTES + chunk.len();
        let mut written = 0usize;
        while written < target_len {
            file.write_all(&chunk).unwrap();
            written = written.saturating_add(chunk.len());
        }
        file.flush().unwrap();
    }

    #[test]
    fn line_lengths_from_bytes_preserves_newline_bytes() {
        let lengths = line_lengths_from_bytes(b"a\r\nbb\n", 16).unwrap();
        assert_eq!(lengths, vec![3, 3, 0]);
    }

    #[test]
    fn line_lengths_from_bytes_respects_limit() {
        assert!(line_lengths_from_bytes(b"a\nb\nc\n", 2).is_none());
    }

    #[test]
    fn precise_piece_table_line_lengths_require_complete_index() {
        let doc = Document {
            path: None,
            storage: None,
            line_offsets: Arc::new(RwLock::new(LineOffsets::U32(vec![0, 4, 9]))),
            disk_index: None,
            indexing: Arc::new(AtomicBool::new(true)),
            indexing_started: None,
            file_len: 9,
            indexed_bytes: Arc::new(AtomicUsize::new(4)),
            avg_line_len: Arc::new(AtomicUsize::new(AVG_LINE_LEN_ESTIMATE)),
            line_ending: LineEnding::Lf,
            rope: None,
            piece_table: None,
            dirty: false,
        };

        assert!(doc.precise_piece_table_line_lengths(false).is_none());
    }

    #[test]
    fn precise_piece_table_line_lengths_reject_large_line_arrays() {
        let mut offsets = Vec::with_capacity(LINE_LENGTHS_MAX_SYNC_LINES + 1);
        for i in 0..=LINE_LENGTHS_MAX_SYNC_LINES {
            offsets.push(i as u32);
        }

        let doc = Document {
            path: None,
            storage: None,
            line_offsets: Arc::new(RwLock::new(LineOffsets::U32(offsets))),
            disk_index: None,
            indexing: Arc::new(AtomicBool::new(false)),
            indexing_started: None,
            file_len: LINE_LENGTHS_MAX_SYNC_LINES + 1,
            indexed_bytes: Arc::new(AtomicUsize::new(LINE_LENGTHS_MAX_SYNC_LINES + 1)),
            avg_line_len: Arc::new(AtomicUsize::new(AVG_LINE_LEN_ESTIMATE)),
            line_ending: LineEnding::Lf,
            rope: None,
            piece_table: None,
            dirty: false,
        };

        assert!(doc.precise_piece_table_line_lengths(true).is_none());
    }

    #[test]
    fn piece_table_line_lengths_for_edit_builds_partial_prefix() {
        let dir = std::env::temp_dir().join(format!(
            "standpad-doc-partial-prefix-{}",
            std::process::id()
        ));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("partial.txt");
        std::fs::write(&path, b"alpha\nbeta\ngamma\n").unwrap();
        let storage = FileStorage::open(&path).unwrap();

        let doc = Document {
            path: Some(path.clone()),
            storage: Some(storage),
            line_offsets: Arc::new(RwLock::new(LineOffsets::default())),
            disk_index: None,
            indexing: Arc::new(AtomicBool::new(false)),
            indexing_started: None,
            file_len: 17,
            indexed_bytes: Arc::new(AtomicUsize::new(0)),
            avg_line_len: Arc::new(AtomicUsize::new(AVG_LINE_LEN_ESTIMATE)),
            line_ending: LineEnding::Lf,
            rope: None,
            piece_table: None,
            dirty: false,
        };

        let (line_lengths, full_index) = doc.piece_table_line_lengths_for_edit(0).unwrap();
        assert!(full_index);
        assert_eq!(line_lengths, vec![6, 5, 6, 0]);

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn piece_table_backspace_in_virtual_space_only_moves_cursor() {
        let dir =
            std::env::temp_dir().join(format!("standpad-doc-backspace-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("virtual.txt");
        std::fs::write(&path, b"abc\n").unwrap();
        let storage = FileStorage::open(&path).unwrap();
        let mut piece_table = PieceTable::new(storage, vec![4, 0], true);

        let before = piece_table.read_range(0, piece_table.total_len());
        let (edited, line0, col0) = piece_table.backspace_at(0, 5).unwrap();
        let after = piece_table.read_range(0, piece_table.total_len());

        assert!(!edited);
        assert_eq!((line0, col0), (0, 4));
        assert_eq!(before, after);

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn piece_table_insert_in_virtual_space_materializes_spaces() {
        let dir =
            std::env::temp_dir().join(format!("standpad-doc-insert-spaces-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("spaces.txt");
        std::fs::write(&path, b"abc\n").unwrap();
        let storage = FileStorage::open(&path).unwrap();
        let mut piece_table = PieceTable::new(storage, vec![4, 0], true);

        let (line0, col0) = piece_table
            .insert_text_at(LineEnding::Lf, 0, 6, "Z")
            .unwrap();

        assert_eq!((line0, col0), (0, 7));
        assert_eq!(piece_table.to_string_lossy(), "abc   Z\n");

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn piece_table_line_lengths_for_edit_supports_large_exact_prefix() {
        let dir =
            std::env::temp_dir().join(format!("standpad-doc-large-prefix-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("large-prefix.bin");
        std::fs::write(&path, vec![b'x'; LINE_LENGTHS_MAX_SYNC_LINES + 1]).unwrap();
        let storage = FileStorage::open(&path).unwrap();

        let mut offsets = Vec::with_capacity(LINE_LENGTHS_MAX_SYNC_LINES + 1);
        for i in 0..=LINE_LENGTHS_MAX_SYNC_LINES {
            offsets.push(i as u32);
        }
        let doc = Document {
            path: Some(path.clone()),
            storage: Some(storage),
            line_offsets: Arc::new(RwLock::new(LineOffsets::U32(offsets))),
            disk_index: None,
            indexing: Arc::new(AtomicBool::new(false)),
            indexing_started: None,
            file_len: LINE_LENGTHS_MAX_SYNC_LINES + 1,
            indexed_bytes: Arc::new(AtomicUsize::new(LINE_LENGTHS_MAX_SYNC_LINES + 1)),
            avg_line_len: Arc::new(AtomicUsize::new(AVG_LINE_LEN_ESTIMATE)),
            line_ending: LineEnding::Lf,
            rope: None,
            piece_table: None,
            dirty: false,
        };

        let (line_lengths, full_index) = doc.piece_table_line_lengths_for_edit(150_000).unwrap();

        assert!(!full_index);
        assert_eq!(line_lengths.len(), 150_001);
        assert!(line_lengths.iter().all(|len| *len == 1));

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn insert_uses_partial_piece_table_before_full_index_finishes() {
        let dir = std::env::temp_dir().join(format!(
            "standpad-doc-insert-partial-{}",
            std::process::id()
        ));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("insert.txt");
        let original = b"abc\ndef\n".repeat((PIECE_TABLE_MIN_BYTES / 8) + 1);
        std::fs::write(&path, &original).unwrap();
        let storage = FileStorage::open(&path).unwrap();

        let mut doc = Document {
            path: Some(path.clone()),
            storage: Some(storage),
            line_offsets: Arc::new(RwLock::new(LineOffsets::default())),
            disk_index: None,
            indexing: Arc::new(AtomicBool::new(true)),
            indexing_started: None,
            file_len: original.len(),
            indexed_bytes: Arc::new(AtomicUsize::new(0)),
            avg_line_len: Arc::new(AtomicUsize::new(AVG_LINE_LEN_ESTIMATE)),
            line_ending: LineEnding::Lf,
            rope: None,
            piece_table: None,
            dirty: false,
        };

        let (line0, col0) = doc.try_insert_text_at(0, 0, "X").unwrap();
        assert_eq!((line0, col0), (0, 1));
        assert!(doc.piece_table.is_some());
        assert!(doc.rope.is_none());
        assert!(doc.text_lossy().starts_with("Xabc\ndef\n"));

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn insert_fully_indexes_medium_unindexed_piece_table_documents() {
        let dir = std::env::temp_dir().join(format!(
            "standpad-doc-insert-full-piece-table-{}",
            std::process::id()
        ));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("insert-full.txt");

        let mut original = String::new();
        for i in 0..300_000usize {
            use std::fmt::Write as _;
            let _ = writeln!(&mut original, "L{i:06}");
        }
        std::fs::write(&path, original).unwrap();
        let storage = FileStorage::open(&path).unwrap();

        let mut doc = Document {
            path: Some(path.clone()),
            storage: Some(storage),
            line_offsets: Arc::new(RwLock::new(LineOffsets::default())),
            disk_index: None,
            indexing: Arc::new(AtomicBool::new(true)),
            indexing_started: None,
            file_len: std::fs::metadata(&path).unwrap().len() as usize,
            indexed_bytes: Arc::new(AtomicUsize::new(0)),
            avg_line_len: Arc::new(AtomicUsize::new(AVG_LINE_LEN_ESTIMATE)),
            line_ending: LineEnding::Lf,
            rope: None,
            piece_table: None,
            dirty: false,
        };

        let (line0, col0) = doc.try_insert_text_at(0, 0, "TOP\n").unwrap();
        let slice = doc.line_slice(5_000, 0, 16);

        assert_eq!((line0, col0), (1, 0));
        assert!(doc.piece_table.is_some());
        assert!(doc.has_precise_line_lengths());
        assert_eq!(doc.exact_line_count(), Some(300_002));
        assert!(slice.is_exact());
        assert_eq!(slice.text(), "L004999");

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn line_slice_reads_exact_text_from_edited_rope() {
        let mut doc = Document::new();
        let _ = doc.try_insert_text_at(0, 0, "hello\nworld").unwrap();

        let slice = doc.line_slice(1, 1, 3);

        assert_eq!(slice.text(), "orl");
        assert!(slice.is_exact());
    }

    #[test]
    fn mmap_line_slice_uses_character_columns_for_multibyte_utf8() {
        let dir = std::env::temp_dir().join(format!("qem-mmap-columns-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("utf8.txt");
        std::fs::write(&path, "A\u{1F600}\u{0411}\n").unwrap();

        let doc = Document::open(&path).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while doc.is_indexing() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }

        let slice = doc.line_slice(0, 1, 2);

        assert_eq!(slice.text(), "\u{1F600}\u{0411}");
        assert!(slice.is_exact());

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mmap_line_len_counts_invalid_utf8_bytes() {
        let dir =
            std::env::temp_dir().join(format!("qem-mmap-invalid-utf8-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("bytes.bin");
        std::fs::write(&path, [0xFF, b'a', b'\n']).unwrap();

        let doc = Document::open(&path).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while doc.is_indexing() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }

        assert_eq!(doc.line_len_chars(0), 2);

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_document_insert_preserves_utf8_text() {
        let mut doc = Document::new();

        let (line0, col0) = doc.try_insert_text_at(0, 0, "😀привет").unwrap();

        assert_eq!((line0, col0), (0, 7));
        assert_eq!(doc.text_lossy(), "😀привет");
        assert_eq!(doc.line_len_chars(0), 7);
    }

    #[test]
    fn piece_table_insert_at_end_does_not_create_zero_len_pieces() {
        let dir = std::env::temp_dir().join(format!("qem-piece-end-insert-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("end.txt");
        std::fs::write(&path, b"abc").unwrap();
        let storage = FileStorage::open(&path).unwrap();
        let mut piece_table = PieceTable::new(storage, vec![3], true);

        let (line0, col0) = piece_table
            .insert_text_at(LineEnding::Lf, 0, 3, "Z")
            .unwrap();

        assert_eq!((line0, col0), (0, 4));
        assert_eq!(piece_table.to_string_lossy(), "abcZ");
        assert!(piece_table
            .pieces
            .to_vec()
            .iter()
            .all(|piece| piece.len > 0));

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn piece_table_insert_after_multibyte_char_preserves_utf8_boundaries() {
        let dir = std::env::temp_dir().join(format!("qem-piece-utf8-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("utf8.txt");
        let content = "A😀Б\n".repeat((PIECE_TABLE_MIN_BYTES / "A😀Б\n".len()) + 16);
        std::fs::write(&path, content).unwrap();

        let mut doc = Document::open(path.clone()).unwrap();
        let (line0, col0) = doc.try_insert_text_at(0, 2, "X").unwrap();

        assert_eq!((line0, col0), (0, 3));
        assert!(doc.has_edit_buffer());
        assert!(doc.text_lossy().starts_with("A😀XБ\n"));

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn detected_crlf_style_is_used_for_inserted_newlines() {
        let dir = std::env::temp_dir().join(format!("qem-crlf-style-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let src = dir.join("windows.txt");
        let dst = dir.join("windows-saved.txt");
        let content = b"alpha\r\nbeta\r\n".repeat((PIECE_TABLE_MIN_BYTES / 12) + 16);
        std::fs::write(&src, &content).unwrap();

        let mut doc = Document::open(src.clone()).unwrap();
        let (line0, col0) = doc.try_insert_text_at(0, 5, "\nX").unwrap();
        doc.save_to(&dst).unwrap();

        assert_eq!((line0, col0), (1, 1));
        assert!(std::fs::read(&dst)
            .unwrap()
            .starts_with(b"alpha\r\nX\r\nbeta\r\n"));

        let _ = std::fs::remove_file(&src);
        let _ = std::fs::remove_file(&dst);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rope_save_preserves_detected_crlf_style() {
        let dir = std::env::temp_dir().join(format!("qem-crlf-rope-style-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let src = dir.join("windows-small.txt");
        let dst = dir.join("windows-small-saved.txt");
        std::fs::write(&src, b"alpha\r\nbeta\r\n").unwrap();

        let mut doc = Document::open(src.clone()).unwrap();
        let (line0, col0) = doc.try_insert_text_at(0, 5, "\nX").unwrap();
        assert!(doc.rope.is_some());
        doc.save_to(&dst).unwrap();

        assert_eq!((line0, col0), (1, 1));
        assert_eq!(std::fs::read(&dst).unwrap(), b"alpha\r\nX\r\nbeta\r\n");

        let _ = std::fs::remove_file(&src);
        let _ = std::fs::remove_file(&dst);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn escaped_utf8_insert_preserves_multibyte_text() {
        let mut doc = Document::new();
        let sample = "\u{1F600}\u{043F}\u{0440}\u{0438}\u{0432}\u{0435}\u{0442}";

        let (line0, col0) = doc.try_insert_text_at(0, 0, sample).unwrap();

        assert_eq!((line0, col0), (0, 7));
        assert_eq!(doc.text_lossy(), sample);
        assert_eq!(doc.line_len_chars(0), 7);
    }

    #[test]
    fn piece_table_insert_after_escaped_multibyte_char_preserves_boundaries() {
        let dir =
            std::env::temp_dir().join(format!("qem-piece-utf8-escaped-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("utf8-escaped.txt");
        let line = "A\u{1F600}\u{0411}\n";
        let content = line.repeat((PIECE_TABLE_MIN_BYTES / line.len()) + 16);
        std::fs::write(&path, content).unwrap();

        let mut doc = Document::open(path.clone()).unwrap();
        let (line0, col0) = doc.try_insert_text_at(0, 2, "X").unwrap();

        assert_eq!((line0, col0), (0, 3));
        assert!(doc.has_edit_buffer());
        assert!(doc.text_lossy().starts_with("A\u{1F600}X\u{0411}\n"));

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn line_slices_match_exact_mmap_lines() {
        let dir = std::env::temp_dir().join(format!("qem-line-slices-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("lines.txt");
        std::fs::write(&path, b"zero\none\ntwo\nthree\n").unwrap();

        let doc = Document::open(&path).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while doc.is_indexing() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }

        let slices = doc.line_slices(1, 3, 0, 16);
        let texts: Vec<String> = slices.into_iter().map(LineSlice::into_text).collect();

        assert_eq!(texts, vec!["one", "two", "three"]);

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn line_slices_use_exact_piece_table_fast_path_after_edit() {
        let dir =
            std::env::temp_dir().join(format!("qem-piece-table-slices-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("large.txt");
        write_disk_backed_fixture(&path);

        let mut doc = Document::open(path.clone()).unwrap();
        let _ = doc.try_insert_text_at(0, 0, "123").unwrap();

        let slices = doc.line_slices(0, 2, 0, 16);
        let texts: Vec<String> = slices.iter().map(|slice| slice.text().to_owned()).collect();

        assert_eq!(texts, vec!["123abc", "def"]);
        assert!(slices.iter().all(LineSlice::is_exact));

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lines_iterator_yields_current_document_lines() {
        let mut doc = Document::new();
        let _ = doc.try_insert_text_at(0, 0, "zero\none\ntwo").unwrap();

        let lines: Vec<String> = doc.lines().map(LineSlice::into_text).collect();

        assert_eq!(lines, vec!["zero", "one", "two"]);
    }

    #[test]
    fn replace_range_updates_rope_backed_documents() {
        let mut doc = Document::new();
        let _ = doc.try_insert_text_at(0, 0, "hello world").unwrap();

        let cursor = doc.try_replace_range(0, 6, 5, "qem").unwrap();

        assert_eq!(cursor, (0, 9));
        assert_eq!(doc.text_lossy(), "hello qem");
    }

    #[test]
    fn replace_range_updates_piece_table_backed_documents() {
        let dir =
            std::env::temp_dir().join(format!("qem-piece-table-replace-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("replace.txt");
        write_disk_backed_fixture(&path);

        let mut doc = Document::open(path.clone()).unwrap();
        let cursor = doc.try_replace_range(0, 0, 3, "XYZ").unwrap();

        assert_eq!(cursor, (0, 3));
        assert!(doc.text_lossy().starts_with("XYZ\ndef\n"));

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fully_indexed_short_line_mmap_uses_exact_line_count() {
        let dir =
            std::env::temp_dir().join(format!("qem-short-lines-exact-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("short.txt");
        std::fs::write(&path, "x\n".repeat(10_000)).unwrap();

        let doc = Document::open(&path).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while doc.is_indexing() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }

        assert!(doc.is_fully_indexed());
        assert!(doc.has_precise_line_lengths());
        assert_eq!(doc.line_count(), LineCount::Exact(10_001));
        assert_eq!(doc.exact_line_count(), Some(10_001));

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn line_slices_near_tail_read_from_file_end_before_full_index() {
        let dir = std::env::temp_dir().join(format!("qem-tail-fast-path-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("tail.txt");
        std::fs::write(&path, b"a\nb\nc\nd\n").unwrap();
        let storage = FileStorage::open(&path).unwrap();

        let doc = Document {
            path: Some(path.clone()),
            storage: Some(storage),
            line_offsets: Arc::new(RwLock::new(LineOffsets::default())),
            disk_index: None,
            indexing: Arc::new(AtomicBool::new(true)),
            indexing_started: None,
            file_len: 8,
            indexed_bytes: Arc::new(AtomicUsize::new(0)),
            avg_line_len: Arc::new(AtomicUsize::new(2)),
            line_ending: LineEnding::Lf,
            rope: None,
            piece_table: None,
            dirty: false,
        };

        let slices = doc.line_slices(9_999, 3, 0, 16);
        let texts: Vec<String> = slices.into_iter().map(LineSlice::into_text).collect();

        assert_eq!(texts, vec!["c", "d", ""]);

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn trailing_tail_fast_path_bails_out_for_huge_final_line() {
        let mut bytes = b"a\nb\n".to_vec();
        bytes.resize(
            TAIL_FAST_PATH_MAX_BACKSCAN_BYTES.saturating_add(bytes.len() + 1),
            b'x',
        );

        let ranges =
            trailing_mmap_line_ranges(&bytes, bytes.len(), 3, TAIL_FAST_PATH_MAX_BACKSCAN_BYTES);

        assert!(ranges.is_none());
    }

    #[test]
    fn line_slices_bail_out_when_next_line_scan_would_be_unbounded() {
        let dir =
            std::env::temp_dir().join(format!("qem-midline-fast-path-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("midline.txt");

        let mut bytes = vec![b'a'; FALLBACK_NEXT_LINE_SCAN_BYTES + 1];
        bytes.push(b'\n');
        bytes.extend_from_slice(b"tail\n");
        std::fs::write(&path, &bytes).unwrap();
        let storage = FileStorage::open(&path).unwrap();

        let doc = Document {
            path: Some(path.clone()),
            storage: Some(storage),
            line_offsets: Arc::new(RwLock::new(LineOffsets::default())),
            disk_index: None,
            indexing: Arc::new(AtomicBool::new(true)),
            indexing_started: None,
            file_len: bytes.len(),
            indexed_bytes: Arc::new(AtomicUsize::new(0)),
            avg_line_len: Arc::new(AtomicUsize::new(1)),
            line_ending: LineEnding::Lf,
            rope: None,
            piece_table: None,
            dirty: false,
        };

        let slices = doc.line_slices(0, 3, 0, 16);
        let texts: Vec<String> = slices.into_iter().map(LineSlice::into_text).collect();

        assert_eq!(texts[0], "aaaaaaaaaaaaaaaa");
        assert_eq!(texts[1], "");
        assert_eq!(texts[2], "");

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn indexing_progress_reports_inflight_state() {
        let doc = Document {
            path: None,
            storage: None,
            line_offsets: Arc::new(RwLock::new(LineOffsets::default())),
            disk_index: None,
            indexing: Arc::new(AtomicBool::new(true)),
            indexing_started: None,
            file_len: 128,
            indexed_bytes: Arc::new(AtomicUsize::new(32)),
            avg_line_len: Arc::new(AtomicUsize::new(AVG_LINE_LEN_ESTIMATE)),
            line_ending: LineEnding::Lf,
            rope: None,
            piece_table: None,
            dirty: false,
        };

        assert_eq!(doc.indexing_progress(), Some((32, 128)));
    }

    #[test]
    fn save_to_reopens_clean_mmap_documents() {
        let dir = std::env::temp_dir().join(format!("qem-doc-save-mmap-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let src = dir.join("source.txt");
        let dst = dir.join("copy.txt");
        std::fs::write(&src, b"alpha\nbeta\n").unwrap();

        let mut doc = Document::open(src.clone()).unwrap();
        doc.save_to(&dst).unwrap();

        assert_eq!(doc.path(), Some(dst.as_path()));
        assert_eq!(doc.mmap_bytes(), b"alpha\nbeta\n");
        assert!(!doc.is_dirty());
        assert_eq!(std::fs::read(&dst).unwrap(), b"alpha\nbeta\n");

        let _ = std::fs::remove_file(&src);
        let _ = std::fs::remove_file(&dst);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_to_keeps_large_piece_table_documents_clean_without_reopen() {
        let dir =
            std::env::temp_dir().join(format!("qem-doc-save-piece-table-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let src = dir.join("large.txt");
        let dst = dir.join("large-copy.txt");
        let original = b"abc\ndef\n".repeat((PIECE_TABLE_MIN_BYTES / 8) + 1);
        std::fs::write(&src, &original).unwrap();

        let mut doc = Document::open(src.clone()).unwrap();
        let _ = doc.try_insert_text_at(0, 0, "123").unwrap();
        assert!(doc.is_dirty());

        doc.save_to(&dst).unwrap();

        assert_eq!(doc.path(), Some(dst.as_path()));
        assert!(!doc.is_dirty());
        assert!(doc.has_edit_buffer());
        assert!(doc.text_lossy().starts_with("123abc\ndef\n"));
        assert!(std::fs::read(&dst).unwrap().starts_with(b"123abc\ndef\n"));

        let _ = std::fs::remove_file(&src);
        let _ = std::fs::remove_file(&dst);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn open_recovers_piece_table_session_from_editlog() {
        let dir = std::env::temp_dir().join(format!("qem-doc-session-open-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("session.txt");
        write_disk_backed_fixture(&path);

        {
            let mut doc = Document::open(path.clone()).unwrap();
            let _ = doc.try_insert_text_at(0, 0, "123").unwrap();
            doc.flush_session().unwrap();
        }

        let recovered = Document::open(path.clone()).unwrap();

        assert!(recovered.is_dirty());
        assert!(recovered.has_edit_buffer());
        assert!(recovered.text_lossy().starts_with("123abc\ndef\n"));

        clear_session_sidecar(&path);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn editlog_flush_failure_surfaces_error_and_falls_back_to_memory() {
        let dir =
            std::env::temp_dir().join(format!("qem-doc-session-failure-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("failure.txt");
        write_disk_backed_fixture(&path);

        let mut doc = Document::open(path.clone()).unwrap();
        let _ = doc.try_insert_text_at(0, 0, "123").unwrap();
        doc.piece_table
            .as_mut()
            .expect("piece table expected")
            .pieces
            .poison_persistence_for_test();

        let err = doc.flush_session();
        assert!(matches!(err, Err(DocumentError::Write { .. })));

        let _ = doc.try_insert_text_at(0, 3, "X").unwrap();
        assert!(doc.text_lossy().starts_with("123Xabc\ndef\n"));
        assert!(
            doc.flush_session().is_ok(),
            "in-memory fallback should stop retrying failed persistence"
        );

        clear_session_sidecar(&path);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn recovered_piece_table_session_supports_undo_and_redo() {
        let dir =
            std::env::temp_dir().join(format!("qem-doc-session-history-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("history.txt");
        write_disk_backed_fixture(&path);

        {
            let mut doc = Document::open(path.clone()).unwrap();
            let _ = doc.try_insert_text_at(0, 0, "123").unwrap();
            doc.flush_session().unwrap();
        }

        let mut recovered = Document::open(path.clone()).unwrap();
        assert!(recovered.undo());
        assert!(recovered.text_lossy().starts_with("abc\ndef\n"));
        assert!(recovered.redo());
        assert!(recovered.text_lossy().starts_with("123abc\ndef\n"));

        clear_session_sidecar(&path);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_clears_recoverable_piece_table_session() {
        let dir = std::env::temp_dir().join(format!("qem-doc-session-save-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("saved.txt");
        write_disk_backed_fixture(&path);

        {
            let mut doc = Document::open(path.clone()).unwrap();
            let _ = doc.try_insert_text_at(0, 0, "123").unwrap();
            doc.flush_session().unwrap();
            doc.save_to(&path).unwrap();
        }

        let reopened = Document::open(path.clone()).unwrap();

        assert!(!reopened.is_dirty());
        assert!(!reopened.has_edit_buffer());
        assert!(std::fs::read(&path).unwrap().starts_with(b"123abc\ndef\n"));

        clear_session_sidecar(&path);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn try_insert_rejects_large_mmap_rope_fallback() {
        let dir =
            std::env::temp_dir().join(format!("qem-doc-large-edit-reject-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("huge.bin");
        let file = std::fs::File::create(&path).unwrap();
        file.set_len((MAX_ROPE_EDIT_FILE_BYTES + 1) as u64).unwrap();
        drop(file);

        let mut doc = Document::open(path.clone()).unwrap();
        let err = doc.try_insert_text_at(PARTIAL_PIECE_TABLE_MAX_LINES + 1, 0, "x");

        assert!(matches!(err, Err(DocumentError::EditUnsupported { .. })));
        assert!(!doc.has_edit_buffer());

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn try_insert_rejects_large_piece_table_promotion_to_rope() {
        let dir = std::env::temp_dir().join(format!(
            "qem-doc-piece-table-promotion-reject-{}",
            std::process::id()
        ));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("small.txt");
        std::fs::write(&path, b"x").unwrap();
        let storage = FileStorage::open(&path).unwrap();

        let mut doc = Document {
            path: Some(path.clone()),
            storage: Some(storage.clone()),
            line_offsets: Arc::new(RwLock::new(LineOffsets::default())),
            disk_index: None,
            indexing: Arc::new(AtomicBool::new(false)),
            indexing_started: None,
            file_len: MAX_ROPE_EDIT_FILE_BYTES + 1,
            indexed_bytes: Arc::new(AtomicUsize::new(0)),
            avg_line_len: Arc::new(AtomicUsize::new(AVG_LINE_LEN_ESTIMATE)),
            line_ending: LineEnding::Lf,
            rope: None,
            piece_table: Some(PieceTable {
                original: storage,
                add: Vec::new(),
                pieces: PieceTree::from_pieces(vec![Piece {
                    src: PieceSource::Original,
                    start: 0,
                    len: 1,
                    line_breaks: 0,
                }]),
                known_line_count: 1,
                known_byte_len: 1,
                total_len: MAX_ROPE_EDIT_FILE_BYTES + 1,
                full_index: false,
                pending_session_flush: false,
                pending_session_edits: 0,
                last_session_flush: None,
                edit_batch_depth: 0,
                edit_batch_dirty: false,
            }),
            dirty: true,
        };

        let err = doc.try_insert_text_at(1, 0, "x");

        assert!(matches!(err, Err(DocumentError::EditUnsupported { .. })));
        assert!(doc.rope.is_none());
        assert!(doc.piece_table.is_some());

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn small_files_index_inline_and_become_precise_immediately() {
        let dir = std::env::temp_dir().join(format!("qem-doc-inline-index-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("small.txt");
        std::fs::write(&path, b"zero\none\ntwo\n").unwrap();

        let doc = Document::open(&path).unwrap();

        assert!(!doc.is_indexing());
        assert!(doc.is_fully_indexed());
        assert!(doc.has_precise_line_lengths());
        assert_eq!(
            doc.indexed_bytes(),
            std::fs::metadata(&path).unwrap().len() as usize
        );
        assert_eq!(doc.line_count(), LineCount::Exact(4));
        assert_eq!(doc.line_slice(2, 0, 16).text(), "two");

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn document_exposes_detected_line_ending_style() {
        let dir = std::env::temp_dir().join(format!("qem-doc-line-ending-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("crlf.txt");
        std::fs::write(&path, b"alpha\r\nbeta\r\n").unwrap();

        let doc = Document::open(&path).unwrap();

        assert_eq!(doc.line_ending(), LineEnding::Crlf);
        assert_eq!(doc.line_ending().as_str(), "\r\n");

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(&dir);
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        #[test]
        fn randomized_in_memory_edits_match_string_model(
            initial in op_text_strategy(),
            ops in prop::collection::vec(edit_op_strategy(), 1..48),
        ) {
            let mut doc = Document::new();
            let mut expected = String::new();

            let initial_cursor = model_insert(&mut expected, 0, 0, &initial);
            let doc_cursor = doc.try_insert_text_at(0, 0, &initial).unwrap();
            prop_assert_eq!(doc_cursor, initial_cursor);
            assert_doc_matches_model(&doc, &expected);

            for op in &ops {
                apply_op_to_doc(&mut doc, &mut expected, op);
            }
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(24))]

        #[test]
        fn randomized_small_file_roundtrip_matches_model(
            initial in op_text_strategy(),
            use_crlf in any::<bool>(),
            ops in prop::collection::vec(edit_op_strategy(), 1..24),
        ) {
            let dir = tempdir().unwrap();
            let source = dir.path().join("input.txt");
            let saved = dir.path().join("output.txt");
            let requested_line_ending = if use_crlf { LineEnding::Crlf } else { LineEnding::Lf };

            let mut expected = String::new();
            let _ = model_insert(&mut expected, 0, 0, &initial);
            let source_text = render_with_line_ending(&expected, requested_line_ending);
            let persisted_line_ending = detect_line_ending(source_text.as_bytes());
            std::fs::write(&source, source_text).unwrap();

            let mut doc = Document::open(&source).unwrap();
            for op in &ops {
                apply_op_to_doc(&mut doc, &mut expected, op);
            }

            doc.save_to(&saved).unwrap();
            let reopened = Document::open(&saved).unwrap();
            let rendered = render_with_line_ending(&expected, persisted_line_ending);

            prop_assert_eq!(reopened.line_ending(), detect_line_ending(rendered.as_bytes()));
            prop_assert_eq!(reopened.text_lossy(), rendered);
            prop_assert_eq!(doc.text_lossy(), expected);
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(10))]

        #[test]
        fn randomized_piece_table_recovery_and_history_roundtrip(
            ops in prop::collection::vec(file_backed_edit_op_strategy(), 1..12),
        ) {
            let dir = tempdir().unwrap();
            let path = dir.path().join("piece-table.txt");
            write_disk_backed_fixture(&path);

            let mut expected = std::fs::read_to_string(&path).unwrap();
            let mut states = vec![expected.clone()];

            {
                let mut doc = Document::open(path.clone()).unwrap();
                for op in &ops {
                    let before = expected.clone();
                    apply_op_to_doc_text_only(&mut doc, &mut expected, op);
                    assert!(
                        doc.piece_table.is_some(),
                        "large file edits should stay on the piece-table path"
                    );
                    assert!(
                        !doc.has_rope(),
                        "low-line edits should not require rope promotion"
                    );
                    if expected != before {
                        states.push(expected.clone());
                    }
                }

                doc.flush_session().unwrap();
            }

            let mut recovered = Document::open(path.clone()).unwrap();
            assert!(recovered.is_dirty());
            assert!(
                recovered.piece_table.is_some(),
                "recovered document should restore piece-table session"
            );
            assert!(
                !recovered.has_rope(),
                "recovered document should keep the piece-table session"
            );
            assert_eq!(recovered.text_lossy(), expected);

            for state in states[..states.len().saturating_sub(1)].iter().rev() {
                assert!(recovered.undo());
                assert_eq!(recovered.text_lossy(), *state);
            }
            assert!(!recovered.undo());

            for state in states.iter().skip(1) {
                assert!(recovered.redo());
                assert_eq!(recovered.text_lossy(), *state);
            }
            assert!(!recovered.redo());
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(8))]

        #[test]
        fn randomized_piece_table_save_to_clears_sessions_and_reopens_clean(
            ops in prop::collection::vec(file_backed_edit_op_strategy(), 1..12),
        ) {
            let dir = tempdir().unwrap();
            let source = dir.path().join("source.txt");
            let saved = dir.path().join("saved.txt");
            write_disk_backed_fixture(&source);

            let original = std::fs::read_to_string(&source).unwrap();
            let mut expected = original.clone();

            {
                let mut doc = Document::open(source.clone()).unwrap();
                for op in &ops {
                    apply_op_to_doc_text_only(&mut doc, &mut expected, op);
                    assert!(
                        doc.piece_table.is_some(),
                        "large file edits should stay on the piece-table path"
                    );
                    assert!(
                        !doc.has_rope(),
                        "save path test should stay on the piece-table path"
                    );
                }

                doc.flush_session().unwrap();
                assert!(
                    editlog_path(&source).exists(),
                    "flush_session should materialize a recoverable sidecar before save"
                );

                doc.save_to(&saved).unwrap();

                assert_eq!(doc.path(), Some(saved.as_path()));
                assert!(!doc.is_dirty());
                assert!(doc.has_edit_buffer());
                assert_eq!(doc.text_lossy(), expected);
                assert_eq!(std::fs::read_to_string(&saved).unwrap(), expected);
                assert_eq!(std::fs::read_to_string(&source).unwrap(), original);
                assert!(
                    !editlog_path(&source).exists(),
                    "save_to should clear the old recoverable sidecar"
                );
                assert!(
                    !editlog_path(&saved).exists(),
                    "save_to should not leave a recoverable sidecar at the destination"
                );
            }

            let reopened = Document::open(saved.clone()).unwrap();
            assert_eq!(reopened.text_lossy(), expected);
            assert!(!reopened.is_dirty());
            assert!(!reopened.has_edit_buffer());
            assert!(
                !editlog_path(&source).exists(),
                "reopening the saved file must not revive the old sidecar"
            );
            assert!(
                !editlog_path(&saved).exists(),
                "clean reopened save must not create a recoverable sidecar"
            );
        }
    }
}
