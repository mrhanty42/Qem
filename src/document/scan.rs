//! Byte-level line-model scans shared by the open/index/edit paths:
//! line-length walks, line-break counting, encoding-cell helpers, and
//! cursor / byte-offset arithmetic over raw byte slices.

use super::*;

pub(super) fn line_lengths_from_offsets(offsets: &LineOffsets, file_len: usize) -> Vec<usize> {
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

pub(super) fn prefix_line_lengths_from_offsets(
    offsets: &LineOffsets,
    max_lines: usize,
) -> Vec<usize> {
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

pub(super) fn line_lengths_from_bytes(bytes: &[u8], max_lines: usize) -> Option<Vec<usize>> {
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

pub(super) fn scan_line_lengths_from(
    bytes: &[u8],
    start: usize,
    max_lines: usize,
    max_bytes: usize,
    utf16_cells: Option<([u8; 2], [u8; 2])>,
) -> Vec<usize> {
    if max_lines == 0 || start >= bytes.len() {
        return Vec::new();
    }

    let end = start.saturating_add(max_bytes).min(bytes.len());
    let mut lengths = Vec::with_capacity(max_lines.min(256));
    let mut line_start = start;
    if let Some((lf, cr)) = utf16_cells {
        // Cell-aware UTF-16 walk (the same discipline as the other
        // line-model walks): line lengths are measured to the end of the
        // whole LF / CR cell, and a raw 0x0A byte inside a non-break cell
        // (U+4E0A is [0x0A, 0x4E] in LE) must not split the line.
        let mut i = start;
        while i + 1 < end {
            let cell = [bytes[i], bytes[i + 1]];
            if cell == lf {
                lengths.push(i + 2 - line_start);
                line_start = i + 2;
                if lengths.len() >= max_lines {
                    return lengths;
                }
                i += 2;
            } else if cell == cr {
                let crlf = i + 3 < end && [bytes[i + 2], bytes[i + 3]] == lf;
                let after = if crlf { i + 4 } else { i + 2 };
                lengths.push(after - line_start);
                line_start = after;
                if lengths.len() >= max_lines {
                    return lengths;
                }
                i = after;
            } else {
                i += 2;
            }
        }
        if end == bytes.len() && lengths.len() < max_lines {
            lengths.push(end.saturating_sub(line_start));
        }
        return lengths;
    }

    let slice = &bytes[start..end];
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

/// The UTF-16 LF / CR cells for `name`, when the encoding's cells are two
/// bytes wide. Every other encoding counts line breaks on raw bytes:
/// single-byte cells are one byte, and the CJK multibyte trail ranges
/// (Shift_JIS 0x40-0xFC, GB18030 0x30-0xFE, EUC-KR 0xA1-0xFE) exclude
/// 0x0A / 0x0D, so a trail byte can never alias a break byte there.
/// The disk-index header's record of which cell width the line numbers
/// were counted in: 0 = raw bytes, 1 = UTF-16LE cells, 2 = UTF-16BE cells.
pub(crate) fn utf16_cells_byte(name: &str) -> u8 {
    match name {
        "UTF-16LE" => 1,
        "UTF-16BE" => 2,
        _ => 0,
    }
}

pub(crate) fn utf16_break_cells(name: &str) -> Option<([u8; 2], [u8; 2])> {
    match name {
        "UTF-16LE" => Some(([0x0A, 0x00], [0x0D, 0x00])),
        "UTF-16BE" => Some(([0x00, 0x0A], [0x00, 0x0D])),
        _ => None,
    }
}

/// Counts line breaks on WHOLE encoding cells. In UTF-16 the LF cell is a
/// two-byte code unit (`[0x0A, 0x00]` in LE, `[0x00, 0x0A]` in BE), so a
/// raw byte scan miscounts inserted text whose characters alias `0x0A` /
/// `0x0D` bytes — e.g. U+4E0A is `[0x0A, 0x4E]` in LE — and the piece's
/// line-break metadata explodes, dragging the whole position model with
/// it. CRLF collapses into one break, matching `scan_position_bytes`.
pub(super) fn count_line_breaks_in_bytes(
    bytes: &[u8],
    utf16_cells: Option<([u8; 2], [u8; 2])>,
) -> usize {
    let Some((lf, cr)) = utf16_cells else {
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
        return count;
    };
    let mut count = 0usize;
    let mut i = 0usize;
    while i + 1 < bytes.len() {
        let cell = [bytes[i], bytes[i + 1]];
        if cell == lf {
            count += 1;
            i += 2;
        } else if cell == cr {
            if i + 3 < bytes.len() && [bytes[i + 2], bytes[i + 3]] == lf {
                count += 1;
                i += 4;
            } else {
                count += 1;
                i += 2;
            }
        } else {
            i += 2;
        }
    }
    // A truncated one-byte tail is text and carries no break.
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

    pub(super) fn capacity_for<T>(file_len: usize) -> usize {
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
pub(super) struct InlineOpenAnalysis {
    pub(super) line_offsets: LineOffsets,
    pub(super) line_ending: LineEnding,
    pub(super) avg_line_len: usize,
    pub(super) utf8_had_errors: bool,
}

pub(super) fn analyze_inline_open(bytes: &[u8]) -> InlineOpenAnalysis {
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
            utf8_had_errors: std::str::from_utf8(bytes).is_err(),
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
        utf8_had_errors: std::str::from_utf8(bytes).is_err(),
    }
}

pub(super) fn estimate_avg_line_len(bytes: &[u8]) -> usize {
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
pub(super) fn utf8_step(bytes: &[u8], start: usize, end: usize) -> usize {
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
pub(super) fn is_utf8_continuation(b: u8) -> bool {
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
pub(super) fn count_text_columns(bytes: &[u8], max_cols: usize) -> usize {
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

#[inline]
pub(super) fn count_text_columns_exact(bytes: &[u8]) -> usize {
    let mut cols = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        if matches!(bytes[i], b'\n' | b'\r') {
            break;
        }
        i += utf8_step(bytes, i, bytes.len());
        cols += 1;
    }
    cols
}

#[derive(Debug, Clone, Copy)]
pub(super) struct CursorScanState {
    target: usize,
    seen: usize,
    pub(super) line0: usize,
    pub(super) col0: usize,
    prev_was_cr: bool,
}

impl CursorScanState {
    pub(super) fn new(target: usize) -> Self {
        Self {
            target,
            seen: 0,
            line0: 0,
            col0: 0,
            prev_was_cr: false,
        }
    }

    pub(super) fn is_done(self) -> bool {
        self.seen >= self.target
    }

    pub(super) fn position(self) -> (usize, usize) {
        (self.line0, self.col0)
    }
}

pub(super) fn scan_cursor_position_bytes(
    bytes: &[u8],
    state: &mut CursorScanState,
    engine: &dyn encoding_engine::EncodingEngine,
) {
    // Line breaks are recognised on WHOLE encoding cells (the same
    // discipline as the other line-model walks): a raw byte check fires on
    // the first byte of a non-break UTF-16LE cell like U+4E0A
    // (`[0x0A, 0x4E]`), counting a phantom line break and a phantom column.
    let utf16_cells = utf16_break_cells(engine.encoding().name());
    let mut i = 0usize;
    while i < bytes.len() && !state.is_done() {
        let step = engine
            .step(bytes, i, bytes.len())
            .max(1)
            .min(bytes.len() - i);
        let cell = &bytes[i..i + step];
        let is_lf = match utf16_cells {
            Some((lf, _)) => cell == lf.as_slice(),
            None => cell == b"\n",
        };
        let is_cr = match utf16_cells {
            Some((_, cr)) => cell == cr.as_slice(),
            None => cell == b"\r",
        };
        if is_lf {
            state.seen = state.seen.saturating_add(1);
            if !state.prev_was_cr {
                state.line0 = state.line0.saturating_add(1);
            }
            state.col0 = 0;
            state.prev_was_cr = false;
            i += step;
        } else if is_cr {
            state.seen = state.seen.saturating_add(1);
            state.line0 = state.line0.saturating_add(1);
            state.col0 = 0;
            state.prev_was_cr = true;
            i += step;
        } else {
            state.prev_was_cr = false;
            state.seen = state.seen.saturating_add(1);
            state.col0 = state.col0.saturating_add(1);
            i = i.saturating_add(step).min(bytes.len());
        }
    }
}

pub(super) fn mmap_line_byte_range(
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

pub(super) fn byte_offset_for_text_col_with_engine(
    bytes: &[u8],
    line_range: (usize, usize),
    col0: usize,
    engine: &dyn encoding_engine::EncodingEngine,
) -> usize {
    let (start, end) = line_range;
    if col0 == 0 || start >= end {
        return start.min(end);
    }
    // Line breaks are recognised on WHOLE encoding cells (the same
    // discipline as the other line-model walks): a raw byte check fires
    // on the first byte of a non-break UTF-16LE cell like U+4E0A
    // (`[0x0A, 0x4E]`), mapping every later column of the line onto the
    // wrong byte.
    let utf16_cells = utf16_break_cells(engine.encoding().name());
    let mut col = 0;
    let mut i = start;
    while i < end && col < col0 {
        let step = engine.step(bytes, i, end).max(1).min(end.saturating_sub(i));
        let cell = &bytes[i..i + step];
        let is_break = match utf16_cells {
            Some((lf, cr)) => cell == lf.as_slice() || cell == cr.as_slice(),
            None => cell == b"\n" || cell == b"\r",
        };
        if is_break {
            break;
        }
        i = i.saturating_add(step);
        col += 1;
    }
    i.min(end)
}

pub(super) fn advance_offset_by_text_units_in_bytes(
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

/// A chunk of freshly scanned line start offsets: the on-disk index is
/// written as `u32` while the file is small and as `u64` once it is not.
#[derive(Debug)]
pub(super) enum OffsetsChunk {
    U32(Vec<u32>),
    U64(Vec<u64>),
}
