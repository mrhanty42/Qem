use super::*;
use std::iter::FusedIterator;

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

pub(super) fn trailing_mmap_line_ranges(
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

impl Document {
    /// Returns a lazy iterator over the currently known document lines.
    ///
    /// While mmap indexing is still in progress this follows [`Document::line_count`],
    /// which is a safe lower bound until indexing completes.
    pub fn lines(&self) -> impl ExactSizeIterator<Item = LineSlice> + FusedIterator + '_ {
        Lines {
            doc: self,
            next_line: 0,
            total_lines: self.display_line_count(),
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

    /// Reads a typed text range with lossy UTF-8 decoding.
    ///
    /// The returned slice follows the current backing representation:
    /// mmap/piece-table reads preserve stored line endings, while rope-backed
    /// documents expose their in-memory `\n` newlines until save time.
    ///
    /// When a clean mmap-backed document has not indexed the requested start
    /// line yet, Qem may fall back to a heuristic start range and mark the
    /// returned slice as inexact.
    pub fn read_text(&self, range: TextRange) -> TextSlice {
        let start = self.clamp_position(range.start());
        if range.is_empty() {
            return TextSlice::new(String::new(), true);
        }

        if let Some(rope) = &self.rope {
            let start_idx = Self::line_col_to_char_index(rope, start.line0(), start.col0());
            let end_idx = start_idx
                .saturating_add(range.len_chars())
                .min(rope.len_chars());
            let slice = rope.slice(start_idx..end_idx);
            let text = slice
                .as_str()
                .map(ToOwned::to_owned)
                .unwrap_or_else(|| slice.to_string());
            return TextSlice::new(text, true);
        }

        if let Some(piece_table) = &self.piece_table {
            let start_col0 = start.col0().min(piece_table.line_len_chars(start.line0()));
            let start_offset = piece_table.byte_offset_for_col(start.line0(), start_col0);
            let end_offset =
                piece_table.advance_offset_by_text_units(start_offset, range.len_chars());
            let bytes = piece_table.read_range(start_offset, end_offset);
            return TextSlice::new(String::from_utf8_lossy(&bytes).into_owned(), true);
        }

        let bytes = self.mmap_bytes();
        let file_len = self.file_len.min(bytes.len());
        if file_len == 0 {
            return TextSlice::new(String::new(), true);
        }

        let indexing_complete = self.is_fully_indexed();
        let offsets_guard = self.line_offsets.try_read().ok();
        let offsets: Option<&LineOffsets> = offsets_guard.as_deref();
        let (line_range, exact) = if let Some(line_range) =
            mmap_line_byte_range(offsets, file_len, start.line0(), indexing_complete)
        {
            (line_range, true)
        } else if let Some(line_range) = self.estimated_mmap_line_byte_range(start.line0()) {
            (line_range, false)
        } else {
            return TextSlice::new(String::new(), false);
        };

        let start_offset = byte_offset_for_text_col_in_bytes(bytes, line_range, start.col0());
        let end_offset =
            advance_offset_by_text_units_in_bytes(bytes, file_len, start_offset, range.len_chars());
        let text =
            String::from_utf8_lossy(&bytes[start_offset.min(file_len)..end_offset]).into_owned();
        TextSlice::new(text, exact)
    }

    /// Reads the current selection as a typed text slice.
    pub fn read_selection(&self, selection: TextSelection) -> TextSlice {
        self.read_text(self.text_range_for_selection(selection))
    }

    /// Reads a viewport using a typed request/response model.
    ///
    /// This is the intended frontend-facing API for scrollable viewers and
    /// editors that want to own their own cursor and scrollbar rendering.
    pub fn read_viewport(&self, request: ViewportRequest) -> Viewport {
        let rows = self
            .line_slices(
                request.first_line0(),
                request.line_count(),
                request.start_col(),
                request.max_cols(),
            )
            .into_iter()
            .enumerate()
            .map(|(offset, slice)| ViewportRow::new(request.first_line0() + offset, slice))
            .collect();
        Viewport::new(request, self.line_count(), rows)
    }
}
