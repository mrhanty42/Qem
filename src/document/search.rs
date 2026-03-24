use super::*;
use memchr::memmem::{Finder, FinderRev};

#[derive(Debug, Clone, Copy, Default)]
struct PositionScanState {
    line0: usize,
    col0: usize,
    prev_was_cr: bool,
}

impl PositionScanState {
    fn new(line0: usize, col0: usize) -> Self {
        Self {
            line0,
            col0,
            prev_was_cr: false,
        }
    }

    fn position(self) -> TextPosition {
        TextPosition::new(self.line0, self.col0)
    }
}

fn scan_position_bytes(bytes: &[u8], state: &mut PositionScanState) {
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'\n' => {
                if !state.prev_was_cr {
                    state.line0 = state.line0.saturating_add(1);
                }
                state.col0 = 0;
                state.prev_was_cr = false;
                i += 1;
            }
            b'\r' => {
                state.line0 = state.line0.saturating_add(1);
                state.col0 = 0;
                state.prev_was_cr = true;
                i += 1;
            }
            _ => {
                state.prev_was_cr = false;
                state.col0 = state.col0.saturating_add(1);
                i += utf8_step(bytes, i, bytes.len());
            }
        }
    }
}

fn search_text_units(text: &str) -> usize {
    let bytes = text.as_bytes();
    let mut units = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'\r' => {
                units += 1;
                i += 1;
                if i < bytes.len() && bytes[i] == b'\n' {
                    i += 1;
                }
            }
            b'\n' => {
                units += 1;
                i += 1;
            }
            _ => {
                units += 1;
                i += utf8_step(bytes, i, bytes.len());
            }
        }
    }
    units
}

fn advance_position_by_bytes(start: TextPosition, bytes: &[u8]) -> TextPosition {
    let mut state = PositionScanState::new(start.line0(), start.col0());
    scan_position_bytes(bytes, &mut state);
    state.position()
}

fn next_line_start_exact(bytes: &[u8], file_len: usize, line_start: usize) -> usize {
    let line_start = line_start.min(file_len);
    if line_start >= file_len {
        return file_len;
    }

    let slice = &bytes[line_start..file_len];
    let Some(rel) = memchr::memchr2(b'\n', b'\r', slice) else {
        return file_len;
    };
    let idx = line_start + rel;
    if bytes[idx] == b'\r' && idx + 1 < file_len && bytes[idx + 1] == b'\n' {
        idx + 2
    } else {
        idx + 1
    }
}

fn line_offsets_anchor_for_line(offsets: &LineOffsets, line0: usize) -> (usize, usize) {
    let anchor_line0 = line0.min(offsets.len().saturating_sub(1));
    let anchor_byte0 = offsets.get_usize(anchor_line0).unwrap_or(0);
    (anchor_line0, anchor_byte0)
}

fn line_offsets_anchor_for_byte(offsets: &LineOffsets, byte_offset: usize) -> (usize, usize) {
    match offsets {
        LineOffsets::U32(v) => {
            let idx = v
                .partition_point(|start| (*start as usize) <= byte_offset)
                .saturating_sub(1);
            (idx, v.get(idx).copied().unwrap_or(0) as usize)
        }
        LineOffsets::U64(v) => {
            let idx = v
                .partition_point(|start| (*start as usize) <= byte_offset)
                .saturating_sub(1);
            (idx, v.get(idx).copied().unwrap_or(0) as usize)
        }
    }
}

fn segment_search_accepts(rel: usize, overlap_len: usize, needle_len: usize) -> bool {
    rel.saturating_add(needle_len) > overlap_len
}

fn refill_search_window(window: &mut Vec<u8>, overlap: &[u8], segment: &[u8]) {
    window.clear();
    window.extend_from_slice(overlap);
    window.extend_from_slice(segment);
}

fn find_window_match(
    finder: &Finder<'_>,
    window: &[u8],
    overlap_len: usize,
    needle_len: usize,
) -> Option<usize> {
    finder
        .find_iter(window)
        .find(|&rel| segment_search_accepts(rel, overlap_len, needle_len))
}

fn find_window_match_rev(
    finder: &FinderRev<'_>,
    window: &[u8],
    overlap_len: usize,
    needle_len: usize,
) -> Option<usize> {
    finder
        .rfind_iter(window)
        .find(|&rel| segment_search_accepts(rel, overlap_len, needle_len))
}

/// Reusable literal-search query for repeated `find_next` / `find_prev` calls.
///
/// This prebuilds forward and reverse substring searchers once, which avoids
/// repeating that setup cost when the same needle is searched many times.
#[derive(Clone, Debug)]
pub struct LiteralSearchQuery {
    needle: String,
    needle_units: usize,
    finder: Finder<'static>,
    finder_rev: FinderRev<'static>,
}

impl LiteralSearchQuery {
    /// Builds a reusable literal-search query.
    ///
    /// Empty needles return `None`.
    pub fn new(needle: impl Into<String>) -> Option<Self> {
        let needle = needle.into();
        if needle.is_empty() {
            return None;
        }

        let needle_units = search_text_units(&needle);
        if needle_units == 0 {
            return None;
        }

        let finder = Finder::new(needle.as_bytes()).into_owned();
        let finder_rev = FinderRev::new(needle.as_bytes()).into_owned();
        Some(Self {
            needle,
            needle_units,
            finder,
            finder_rev,
        })
    }

    /// Returns the literal text used by this query.
    pub fn needle(&self) -> &str {
        &self.needle
    }

    /// Returns the query length in document text units.
    pub const fn len_chars(&self) -> usize {
        self.needle_units
    }

    fn bytes(&self) -> &[u8] {
        self.needle.as_bytes()
    }
}

impl Document {
    /// Finds the next literal match starting at `from`.
    ///
    /// Empty needles return `None`. The match follows the current backing
    /// representation: clean mmap/piece-table documents search stored bytes
    /// (including stored CRLF), while rope-backed documents search the current
    /// in-memory `\n` representation.
    pub fn find_next(&self, needle: &str, from: TextPosition) -> Option<SearchMatch> {
        if needle.is_empty() {
            return None;
        }

        let needle_bytes = needle.as_bytes();
        let needle_units = search_text_units(needle);
        if needle_units == 0 {
            return None;
        }

        let finder = Finder::new(needle_bytes);
        self.find_next_with_finder(needle_bytes, needle_units, &finder, from)
    }

    /// Finds the previous literal match whose end is at or before `before`.
    ///
    /// Empty needles return `None`. The match follows the current backing
    /// representation: clean mmap/piece-table documents search stored bytes
    /// (including stored CRLF), while rope-backed documents search the current
    /// in-memory `\n` representation.
    pub fn find_prev(&self, needle: &str, before: TextPosition) -> Option<SearchMatch> {
        if needle.is_empty() {
            return None;
        }

        let needle_bytes = needle.as_bytes();
        let needle_units = search_text_units(needle);
        if needle_units == 0 {
            return None;
        }

        let finder_rev = FinderRev::new(needle_bytes);
        self.find_prev_with_finder(needle_bytes, needle_units, &finder_rev, before)
    }

    /// Finds the next literal match for a reusable compiled query.
    pub fn find_next_query(
        &self,
        query: &LiteralSearchQuery,
        from: TextPosition,
    ) -> Option<SearchMatch> {
        self.find_next_with_finder(query.bytes(), query.len_chars(), &query.finder, from)
    }

    /// Finds the previous literal match for a reusable compiled query.
    pub fn find_prev_query(
        &self,
        query: &LiteralSearchQuery,
        before: TextPosition,
    ) -> Option<SearchMatch> {
        self.find_prev_with_finder(query.bytes(), query.len_chars(), &query.finder_rev, before)
    }

    /// Finds the first literal match fully contained within `range`.
    pub fn find_next_in_range(&self, needle: &str, range: TextRange) -> Option<SearchMatch> {
        if needle.is_empty() {
            return None;
        }

        let needle_bytes = needle.as_bytes();
        let needle_units = search_text_units(needle);
        if needle_units == 0 {
            return None;
        }

        let finder = Finder::new(needle_bytes);
        self.find_next_in_range_with_finder(needle_bytes, needle_units, &finder, range)
    }

    /// Finds the last literal match fully contained within `range`.
    pub fn find_prev_in_range(&self, needle: &str, range: TextRange) -> Option<SearchMatch> {
        if needle.is_empty() {
            return None;
        }

        let needle_bytes = needle.as_bytes();
        let needle_units = search_text_units(needle);
        if needle_units == 0 {
            return None;
        }

        let finder_rev = FinderRev::new(needle_bytes);
        self.find_prev_in_range_with_finder(needle_bytes, needle_units, &finder_rev, range)
    }

    /// Finds the first query match fully contained within `range`.
    pub fn find_next_query_in_range(
        &self,
        query: &LiteralSearchQuery,
        range: TextRange,
    ) -> Option<SearchMatch> {
        self.find_next_in_range_with_finder(query.bytes(), query.len_chars(), &query.finder, range)
    }

    /// Finds the last query match fully contained within `range`.
    pub fn find_prev_query_in_range(
        &self,
        query: &LiteralSearchQuery,
        range: TextRange,
    ) -> Option<SearchMatch> {
        self.find_prev_in_range_with_finder(
            query.bytes(),
            query.len_chars(),
            &query.finder_rev,
            range,
        )
    }

    fn find_next_in_range_with_finder(
        &self,
        needle_bytes: &[u8],
        needle_units: usize,
        finder: &Finder<'_>,
        range: TextRange,
    ) -> Option<SearchMatch> {
        let (start, end) = self.search_range_bounds(range);
        if start >= end {
            return None;
        }

        let found = self.find_next_with_finder(needle_bytes, needle_units, finder, start)?;
        (found.end() <= end).then_some(found)
    }

    fn find_prev_in_range_with_finder(
        &self,
        needle_bytes: &[u8],
        needle_units: usize,
        finder_rev: &FinderRev<'_>,
        range: TextRange,
    ) -> Option<SearchMatch> {
        let (start, end) = self.search_range_bounds(range);
        if start >= end {
            return None;
        }

        let found = self.find_prev_with_finder(needle_bytes, needle_units, finder_rev, end)?;
        (found.start() >= start).then_some(found)
    }

    fn search_range_bounds(&self, range: TextRange) -> (TextPosition, TextPosition) {
        let start = self.clamp_position(range.start());
        let start_idx = self.char_index_for_position(start);
        let end = self.position_for_char_index(start_idx.saturating_add(range.len_chars()));
        (start, end)
    }

    fn find_next_with_finder(
        &self,
        needle_bytes: &[u8],
        needle_units: usize,
        finder: &Finder<'_>,
        from: TextPosition,
    ) -> Option<SearchMatch> {
        if let Some(rope) = &self.rope {
            return self.find_next_in_rope(rope, needle_bytes, needle_units, finder, from);
        }

        let from = self.clamp_position(from);
        if let Some(piece_table) = &self.piece_table {
            return self.find_next_in_piece_table(
                piece_table,
                needle_bytes,
                needle_units,
                finder,
                from,
            );
        }

        self.find_next_in_mmap(needle_bytes, needle_units, finder, from)
    }

    fn find_prev_with_finder(
        &self,
        needle_bytes: &[u8],
        needle_units: usize,
        finder_rev: &FinderRev<'_>,
        before: TextPosition,
    ) -> Option<SearchMatch> {
        if let Some(rope) = &self.rope {
            return self.find_prev_in_rope(rope, needle_bytes, needle_units, finder_rev, before);
        }

        let before = self.clamp_position(before);
        if let Some(piece_table) = &self.piece_table {
            return self.find_prev_in_piece_table(
                piece_table,
                needle_bytes,
                needle_units,
                finder_rev,
                before,
            );
        }

        self.find_prev_in_mmap(needle_bytes, needle_units, finder_rev, before)
    }

    fn find_next_in_rope(
        &self,
        rope: &Rope,
        needle: &[u8],
        needle_units: usize,
        finder: &Finder<'_>,
        from: TextPosition,
    ) -> Option<SearchMatch> {
        let start = self.clamp_position(from);
        let start_char = Self::line_col_to_char_index(rope, start.line0(), start.col0());
        let start_byte = rope.char_to_byte(start_char);
        let match_start = find_next_in_rope_chunks(rope, start_byte, needle.len(), finder)?;
        let start_char = rope.byte_to_char(match_start);
        let start_pos = self.position_for_char_index(start_char);
        let end_pos = advance_position_by_bytes(start_pos, needle);
        Some(SearchMatch::new(
            TextRange::new(start_pos, needle_units),
            end_pos,
        ))
    }

    fn find_next_in_mmap(
        &self,
        needle: &[u8],
        needle_units: usize,
        finder: &Finder<'_>,
        from: TextPosition,
    ) -> Option<SearchMatch> {
        let bytes = self.mmap_bytes();
        let file_len = self.file_len.min(bytes.len());
        if file_len == 0 {
            return None;
        }

        let start_offset = self.mmap_byte_offset_for_position(from);
        let rel = finder.find(&bytes[start_offset..file_len])?;
        let match_start = start_offset.saturating_add(rel);
        let match_end = match_start.saturating_add(needle.len()).min(file_len);
        let start_pos = self.mmap_position_for_byte_offset(match_start);
        let end_pos = self.mmap_position_for_byte_offset(match_end);
        Some(SearchMatch::new(
            TextRange::new(start_pos, needle_units),
            end_pos,
        ))
    }

    fn find_prev_in_mmap(
        &self,
        needle: &[u8],
        needle_units: usize,
        finder_rev: &FinderRev<'_>,
        before: TextPosition,
    ) -> Option<SearchMatch> {
        let bytes = self.mmap_bytes();
        let file_len = self.file_len.min(bytes.len());
        if file_len == 0 {
            return None;
        }

        let end_offset = self.mmap_byte_offset_for_position(before);
        let match_start = finder_rev.rfind(&bytes[..end_offset.min(file_len)])?;
        let match_end = match_start.saturating_add(needle.len()).min(file_len);
        let start_pos = self.mmap_position_for_byte_offset(match_start);
        let end_pos = self.mmap_position_for_byte_offset(match_end);
        Some(SearchMatch::new(
            TextRange::new(start_pos, needle_units),
            end_pos,
        ))
    }

    fn find_next_in_piece_table(
        &self,
        piece_table: &PieceTable,
        needle: &[u8],
        needle_units: usize,
        finder: &Finder<'_>,
        from: TextPosition,
    ) -> Option<SearchMatch> {
        let start_col0 = from.col0().min(piece_table.line_len_chars(from.line0()));
        let start_offset = piece_table.byte_offset_for_col(from.line0(), start_col0);
        let match_start = piece_table.find_literal_next(start_offset, needle.len(), finder)?;
        let start_pos = piece_table.position_for_byte_offset(match_start);
        let end_pos = advance_position_by_bytes(start_pos, needle);
        Some(SearchMatch::new(
            TextRange::new(start_pos, needle_units),
            end_pos,
        ))
    }

    fn find_prev_in_piece_table(
        &self,
        piece_table: &PieceTable,
        needle: &[u8],
        needle_units: usize,
        finder_rev: &FinderRev<'_>,
        before: TextPosition,
    ) -> Option<SearchMatch> {
        let end_col0 = before
            .col0()
            .min(piece_table.line_len_chars(before.line0()));
        let end_offset = piece_table.byte_offset_for_col(before.line0(), end_col0);
        let match_start = piece_table.find_literal_prev(end_offset, needle.len(), finder_rev)?;
        let start_pos = piece_table.position_for_byte_offset(match_start);
        let end_pos = advance_position_by_bytes(start_pos, needle);
        Some(SearchMatch::new(
            TextRange::new(start_pos, needle_units),
            end_pos,
        ))
    }

    fn mmap_byte_offset_for_position(&self, position: TextPosition) -> usize {
        let position = self.clamp_position(position);
        let bytes = self.mmap_bytes();
        let file_len = self.file_len.min(bytes.len());
        if file_len == 0 {
            return 0;
        }

        let mut line0 = 0usize;
        let mut line_start = 0usize;
        if let Ok(offsets) = self.line_offsets.read() {
            (line0, line_start) = line_offsets_anchor_for_line(&offsets, position.line0());
            line_start = line_start.min(file_len);
        }

        while line0 < position.line0() && line_start < file_len {
            let next = next_line_start_exact(bytes, file_len, line_start);
            if next <= line_start {
                break;
            }
            line_start = next;
            line0 += 1;
        }

        let line_end = next_line_start_exact(bytes, file_len, line_start);
        byte_offset_for_text_col_in_bytes(bytes, (line_start, line_end), position.col0())
            .min(file_len)
    }

    fn mmap_position_for_byte_offset(&self, byte_offset: usize) -> TextPosition {
        let bytes = self.mmap_bytes();
        let file_len = self.file_len.min(bytes.len());
        let target = byte_offset.min(file_len);
        if target == 0 {
            return TextPosition::new(0, 0);
        }

        let mut line0 = 0usize;
        let mut line_start = 0usize;
        if let Ok(offsets) = self.line_offsets.read() {
            (line0, line_start) = line_offsets_anchor_for_byte(&offsets, target);
            line_start = line_start.min(target);
        }

        let mut state = PositionScanState::new(line0, 0);
        scan_position_bytes(&bytes[line_start..target], &mut state);
        state.position()
    }

    fn find_prev_in_rope(
        &self,
        rope: &Rope,
        needle: &[u8],
        needle_units: usize,
        finder_rev: &FinderRev<'_>,
        before: TextPosition,
    ) -> Option<SearchMatch> {
        let before = self.clamp_position(before);
        let end_char = Self::line_col_to_char_index(rope, before.line0(), before.col0());
        let end_byte = rope.char_to_byte(end_char);
        let match_start = find_prev_in_rope_chunks(rope, end_byte, needle.len(), finder_rev)?;
        let start_char = rope.byte_to_char(match_start);
        let start_pos = self.position_for_char_index(start_char);
        let end_pos = advance_position_by_bytes(start_pos, needle);
        Some(SearchMatch::new(
            TextRange::new(start_pos, needle_units),
            end_pos,
        ))
    }
}

impl PieceTable {
    fn position_for_byte_offset(&self, byte_offset: usize) -> TextPosition {
        let target = byte_offset.min(self.total_len);
        if target == 0 {
            return TextPosition::new(0, 0);
        }

        let mut state = PositionScanState::default();
        self.pieces
            .visit_range(0, target, |piece, local_start, local_end| {
                let seg_start = piece.start + local_start;
                let seg_end = piece.start + local_end;
                let src = self.source_bytes(piece.src);
                scan_position_bytes(&src[seg_start..seg_end], &mut state);
            });
        state.position()
    }

    fn find_literal_next(
        &self,
        start: usize,
        needle_len: usize,
        finder: &Finder<'_>,
    ) -> Option<usize> {
        if needle_len == 0 || start >= self.total_len {
            return None;
        }

        let mut overlap = Vec::with_capacity(needle_len.saturating_sub(1));
        let mut window = Vec::new();
        let mut overlap_start = start;
        let mut segment_start = start;
        let mut found = None;

        self.pieces
            .visit_range(start, self.total_len, |piece, local_start, local_end| {
                if found.is_some() {
                    return;
                }

                let seg_start = piece.start + local_start;
                let seg_end = piece.start + local_end;
                let src = self.source_bytes(piece.src);
                let segment = &src[seg_start..seg_end];
                if segment.is_empty() {
                    return;
                }

                let overlap_len = overlap.len();
                refill_search_window(&mut window, &overlap, segment);
                if let Some(rel) = find_window_match(finder, &window, overlap_len, needle_len) {
                    found = Some(overlap_start.saturating_add(rel));
                }

                segment_start = segment_start.saturating_add(segment.len());
                let keep = needle_len.saturating_sub(1).min(window.len());
                overlap.clear();
                overlap.extend_from_slice(&window[window.len().saturating_sub(keep)..]);
                overlap_start = segment_start.saturating_sub(overlap.len());
            });

        found
    }

    fn find_literal_prev(
        &self,
        end: usize,
        needle_len: usize,
        finder: &FinderRev<'_>,
    ) -> Option<usize> {
        if needle_len == 0 || end == 0 {
            return None;
        }

        let mut overlap = Vec::with_capacity(needle_len.saturating_sub(1));
        let mut window = Vec::new();
        let mut overlap_start = 0usize;
        let mut segment_end = 0usize;
        let mut found = None;

        self.pieces.visit_range(
            0,
            end.min(self.total_len),
            |piece, local_start, local_end| {
                let seg_start = piece.start + local_start;
                let seg_end = piece.start + local_end;
                let src = self.source_bytes(piece.src);
                let segment = &src[seg_start..seg_end];
                if segment.is_empty() {
                    return;
                }

                let overlap_len = overlap.len();
                refill_search_window(&mut window, &overlap, segment);
                if let Some(rel) = find_window_match_rev(finder, &window, overlap_len, needle_len) {
                    found = Some(overlap_start.saturating_add(rel));
                }

                segment_end = segment_end.saturating_add(segment.len());
                let keep = needle_len.saturating_sub(1).min(window.len());
                overlap.clear();
                overlap.extend_from_slice(&window[window.len().saturating_sub(keep)..]);
                overlap_start = segment_end.saturating_sub(overlap.len());
            },
        );

        found
    }
}

fn find_next_in_rope_chunks(
    rope: &Rope,
    start_byte: usize,
    needle_len: usize,
    finder: &Finder<'_>,
) -> Option<usize> {
    if needle_len == 0 {
        return None;
    }

    let mut chunk_base = 0usize;
    let mut overlap = Vec::with_capacity(needle_len.saturating_sub(1));
    let mut window = Vec::new();
    let mut overlap_start = start_byte;

    for chunk in rope.chunks() {
        let chunk_bytes = chunk.as_bytes();
        let chunk_end = chunk_base.saturating_add(chunk_bytes.len());
        if chunk_end <= start_byte {
            chunk_base = chunk_end;
            continue;
        }

        let skip = start_byte.saturating_sub(chunk_base).min(chunk_bytes.len());
        let segment = &chunk_bytes[skip..];
        let segment_start = chunk_base.saturating_add(skip);
        let overlap_len = overlap.len();
        refill_search_window(&mut window, &overlap, segment);
        if let Some(rel) = find_window_match(finder, &window, overlap_len, needle_len) {
            return Some(overlap_start.saturating_add(rel));
        }

        let keep = needle_len.saturating_sub(1).min(window.len());
        overlap.clear();
        overlap.extend_from_slice(&window[window.len().saturating_sub(keep)..]);
        overlap_start = segment_start
            .saturating_add(segment.len())
            .saturating_sub(overlap.len());
        chunk_base = chunk_end;
    }

    None
}

fn find_prev_in_rope_chunks(
    rope: &Rope,
    end_byte: usize,
    needle_len: usize,
    finder: &FinderRev<'_>,
) -> Option<usize> {
    if needle_len == 0 || end_byte == 0 {
        return None;
    }

    let mut chunk_base = 0usize;
    let mut overlap = Vec::with_capacity(needle_len.saturating_sub(1));
    let mut window = Vec::new();
    let mut overlap_start = 0usize;
    let mut found = None;

    for chunk in rope.chunks() {
        if chunk_base >= end_byte {
            break;
        }

        let chunk_bytes = chunk.as_bytes();
        let chunk_end = chunk_base.saturating_add(chunk_bytes.len());
        let take = end_byte.saturating_sub(chunk_base).min(chunk_bytes.len());
        let segment = &chunk_bytes[..take];
        if segment.is_empty() {
            chunk_base = chunk_end;
            continue;
        }

        let overlap_len = overlap.len();
        refill_search_window(&mut window, &overlap, segment);
        if let Some(rel) = find_window_match_rev(finder, &window, overlap_len, needle_len) {
            found = Some(overlap_start.saturating_add(rel));
        }

        let keep = needle_len.saturating_sub(1).min(window.len());
        overlap.clear();
        overlap.extend_from_slice(&window[window.len().saturating_sub(keep)..]);
        overlap_start = chunk_base
            .saturating_add(take)
            .saturating_sub(overlap.len());
        chunk_base = chunk_end;
    }

    found
}
