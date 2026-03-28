use super::*;
use memchr::memmem::{Finder, FinderRev};

const REVERSE_POSITION_FAST_PATH_BYTES: usize = 1024;
const BUFFERED_LITERAL_RANGE_MAX_BYTES: usize = 8 * 1024 * 1024;

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

pub(super) fn next_line_start_exact(bytes: &[u8], file_len: usize, line_start: usize) -> usize {
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

#[derive(Clone, Copy, Debug)]
struct PieceTableLineScan {
    range: (usize, usize),
    complete: bool,
}

fn next_piece_table_scan_line_range(
    bytes: &[u8],
    start0: usize,
    buffer_reaches_eof: bool,
) -> Option<PieceTableLineScan> {
    if start0 >= bytes.len() {
        return None;
    }

    let slice = &bytes[start0..];
    let end0 = if let Some(rel) = memchr::memchr2(b'\n', b'\r', slice) {
        let idx = start0 + rel;
        if bytes[idx] == b'\r' && idx + 1 < bytes.len() && bytes[idx + 1] == b'\n' {
            idx + 2
        } else {
            idx + 1
        }
    } else {
        bytes.len()
    };

    Some(PieceTableLineScan {
        range: (start0, end0.max(start0)),
        complete: end0 < bytes.len() || buffer_reaches_eof,
    })
}

fn scanned_piece_table_byte_offset_for_position(
    piece_table: &PieceTable,
    position: TextPosition,
) -> Option<usize> {
    if piece_table.full_index() || position.line0() < piece_table.line_count() {
        let actual_col0 = position
            .col0()
            .min(piece_table.line_len_chars(position.line0()));
        return Some(piece_table.byte_offset_for_col(position.line0(), actual_col0));
    }

    let scan_start = piece_table.known_byte_len.min(piece_table.total_len());
    let scan_end = scan_start
        .saturating_add(PARTIAL_PIECE_TABLE_SCAN_BYTES)
        .min(piece_table.total_len());
    if scan_start >= scan_end {
        return None;
    }

    let bytes = piece_table.read_range(scan_start, scan_end);
    let buffer_reaches_eof = scan_end == piece_table.total_len();
    let mut rel_start = 0usize;
    let mut skip_lines = position.line0().saturating_sub(piece_table.line_count());
    while skip_lines > 0 {
        let scanned = next_piece_table_scan_line_range(&bytes, rel_start, buffer_reaches_eof)?;
        if scanned.range.1 <= rel_start || !scanned.complete {
            return None;
        }
        rel_start = scanned.range.1;
        skip_lines -= 1;
    }

    let range = next_piece_table_scan_line_range(&bytes, rel_start, buffer_reaches_eof)?;
    Some(scan_start.saturating_add(byte_offset_for_text_col_in_bytes(
        &bytes,
        range.range,
        position.col0(),
    )))
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

/// Iterator over non-overlapping literal matches in the current document.
///
/// The iterator owns its compiled query and advances from the end of each
/// match, so overlapping matches are intentionally skipped.
#[derive(Debug)]
pub struct LiteralSearchIter<'a> {
    doc: &'a Document,
    query: Option<LiteralSearchQuery>,
    next_from: TextPosition,
    next_offset: usize,
    end: Option<TextPosition>,
    end_offset: Option<usize>,
    buffered_range: Option<BufferedLiteralSearchRange>,
    finished: bool,
}

#[derive(Debug)]
struct BufferedLiteralSearchRange {
    base_offset: usize,
    bytes: Vec<u8>,
    next_rel_offset: usize,
}

impl<'a> LiteralSearchIter<'a> {
    fn from_query(
        doc: &'a Document,
        query: Option<LiteralSearchQuery>,
        next_from: TextPosition,
        end: Option<TextPosition>,
    ) -> Self {
        let next_from = doc.clamp_position(next_from);
        let end = end.map(|position| doc.clamp_position(position));
        let next_offset = doc.search_byte_offset_for_position(next_from);
        let end_offset = end.map(|position| doc.search_byte_offset_for_position(position));
        let buffered_range = if query.is_some() {
            match end_offset {
                Some(end_offset) => {
                    doc.buffered_literal_range(next_offset, end_offset)
                        .map(|bytes| BufferedLiteralSearchRange {
                            base_offset: next_offset,
                            bytes,
                            next_rel_offset: 0,
                        })
                }
                None => doc
                    .buffered_literal_range(next_offset, doc.file_len())
                    .map(|bytes| BufferedLiteralSearchRange {
                        base_offset: next_offset,
                        bytes,
                        next_rel_offset: 0,
                    }),
            }
        } else {
            None
        };
        Self {
            doc,
            query,
            next_offset,
            next_from,
            end_offset,
            end,
            buffered_range,
            finished: false,
        }
    }
}

impl Iterator for LiteralSearchIter<'_> {
    type Item = SearchMatch;

    fn next(&mut self) -> Option<Self::Item> {
        if self.finished {
            return None;
        }
        let query = self.query.as_ref()?;
        if let Some(buffered) = self.buffered_range.as_mut() {
            if buffered.next_rel_offset >= buffered.bytes.len() {
                self.finished = true;
                return None;
            }
            let Some(rel) = query
                .finder
                .find(&buffered.bytes[buffered.next_rel_offset..])
            else {
                self.finished = true;
                return None;
            };
            let match_start_rel = buffered.next_rel_offset.saturating_add(rel);
            let start_pos = if match_start_rel == buffered.next_rel_offset {
                self.next_from
            } else {
                advance_position_by_bytes(
                    self.next_from,
                    &buffered.bytes[buffered.next_rel_offset..match_start_rel],
                )
            };
            let match_end_rel = match_start_rel
                .saturating_add(query.bytes().len())
                .min(buffered.bytes.len());
            let end_pos = advance_position_by_bytes(start_pos, query.bytes());
            buffered.next_rel_offset = match_end_rel;
            self.next_from = end_pos;
            self.next_offset = buffered.base_offset.saturating_add(match_end_rel);
            return Some(SearchMatch::new(
                TextRange::new(start_pos, query.len_chars()),
                end_pos,
            ));
        }
        let found = if let Some(end) = self.end {
            if self.next_from >= end {
                self.finished = true;
                return None;
            }
            let end_offset = self.end_offset.unwrap_or(self.next_offset);
            self.doc.find_next_with_offset_hint_bounded(
                query.bytes(),
                query.len_chars(),
                &query.finder,
                (self.next_from, self.next_offset),
                (end, end_offset),
            )
        } else {
            self.doc.find_next_with_offset_hint(
                query.bytes(),
                query.len_chars(),
                &query.finder,
                self.next_from,
                self.next_offset,
            )
        };
        let Some(found) = found else {
            self.finished = true;
            return None;
        };
        self.next_offset = found.1;
        self.next_from = found.0.end();
        Some(found.0)
    }
}

impl std::iter::FusedIterator for LiteralSearchIter<'_> {}

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

    /// Iterates non-overlapping literal matches in the whole document.
    ///
    /// Empty needles yield an empty iterator.
    pub fn find_all(&self, needle: impl Into<String>) -> LiteralSearchIter<'_> {
        self.find_all_from(needle, TextPosition::new(0, 0))
    }

    /// Iterates non-overlapping literal matches from `from` to the end of the document.
    ///
    /// Empty needles yield an empty iterator.
    pub fn find_all_from(
        &self,
        needle: impl Into<String>,
        from: TextPosition,
    ) -> LiteralSearchIter<'_> {
        LiteralSearchIter::from_query(
            self,
            LiteralSearchQuery::new(needle),
            self.clamp_position(from),
            None,
        )
    }

    /// Iterates non-overlapping literal matches from `from` onward using a
    /// reusable compiled query.
    pub fn find_all_query_from(
        &self,
        query: &LiteralSearchQuery,
        from: TextPosition,
    ) -> LiteralSearchIter<'_> {
        LiteralSearchIter::from_query(self, Some(query.clone()), self.clamp_position(from), None)
    }

    /// Iterates non-overlapping literal matches in the whole document using a
    /// reusable compiled query.
    pub fn find_all_query(&self, query: &LiteralSearchQuery) -> LiteralSearchIter<'_> {
        self.find_all_query_from(query, TextPosition::new(0, 0))
    }

    /// Iterates non-overlapping literal matches fully contained within `range`.
    ///
    /// Empty needles yield an empty iterator.
    pub fn find_all_in_range(
        &self,
        needle: impl Into<String>,
        range: TextRange,
    ) -> LiteralSearchIter<'_> {
        let (start, end) = self.search_range_bounds(range);
        self.find_all_between_internal(LiteralSearchQuery::new(needle), start, end)
    }

    /// Iterates non-overlapping literal matches between two typed positions.
    ///
    /// The bounds are clamped and ordered before searching. Empty needles yield
    /// an empty iterator.
    pub fn find_all_between(
        &self,
        needle: impl Into<String>,
        start: TextPosition,
        end: TextPosition,
    ) -> LiteralSearchIter<'_> {
        let (start, end) = self.ordered_positions(start, end);
        self.find_all_between_internal(LiteralSearchQuery::new(needle), start, end)
    }

    /// Iterates non-overlapping literal matches fully contained within `range`
    /// using a reusable compiled query.
    pub fn find_all_query_in_range(
        &self,
        query: &LiteralSearchQuery,
        range: TextRange,
    ) -> LiteralSearchIter<'_> {
        let (start, end) = self.search_range_bounds(range);
        self.find_all_query_between(query, start, end)
    }

    /// Iterates non-overlapping literal matches between two typed positions
    /// using a reusable compiled query.
    pub fn find_all_query_between(
        &self,
        query: &LiteralSearchQuery,
        start: TextPosition,
        end: TextPosition,
    ) -> LiteralSearchIter<'_> {
        self.find_all_between_internal(Some(query.clone()), start, end)
    }

    fn find_all_between_internal(
        &self,
        query: Option<LiteralSearchQuery>,
        start: TextPosition,
        end: TextPosition,
    ) -> LiteralSearchIter<'_> {
        let (start, end) = self.ordered_positions(start, end);
        LiteralSearchIter::from_query(self, query, start, Some(end))
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
        let (start, end) = self.search_range_bounds(range);
        self.find_next_bounded_with_finder(needle_bytes, needle_units, &finder, start, end)
    }

    /// Finds the first literal match fully contained between two typed positions.
    pub fn find_next_between(
        &self,
        needle: &str,
        start: TextPosition,
        end: TextPosition,
    ) -> Option<SearchMatch> {
        if needle.is_empty() {
            return None;
        }

        let needle_bytes = needle.as_bytes();
        let needle_units = search_text_units(needle);
        if needle_units == 0 {
            return None;
        }

        let finder = Finder::new(needle_bytes);
        let (start, end) = self.ordered_positions(start, end);
        self.find_next_bounded_with_finder(needle_bytes, needle_units, &finder, start, end)
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
        let (start, end) = self.search_range_bounds(range);
        self.find_prev_bounded_with_finder(needle_bytes, needle_units, &finder_rev, start, end)
    }

    /// Finds the last literal match fully contained between two typed positions.
    pub fn find_prev_between(
        &self,
        needle: &str,
        start: TextPosition,
        end: TextPosition,
    ) -> Option<SearchMatch> {
        if needle.is_empty() {
            return None;
        }

        let needle_bytes = needle.as_bytes();
        let needle_units = search_text_units(needle);
        if needle_units == 0 {
            return None;
        }

        let finder_rev = FinderRev::new(needle_bytes);
        let (start, end) = self.ordered_positions(start, end);
        self.find_prev_bounded_with_finder(needle_bytes, needle_units, &finder_rev, start, end)
    }

    /// Finds the first query match fully contained within `range`.
    pub fn find_next_query_in_range(
        &self,
        query: &LiteralSearchQuery,
        range: TextRange,
    ) -> Option<SearchMatch> {
        let (start, end) = self.search_range_bounds(range);
        self.find_next_bounded_with_finder(
            query.bytes(),
            query.len_chars(),
            &query.finder,
            start,
            end,
        )
    }

    /// Finds the first query match fully contained between two typed positions.
    pub fn find_next_query_between(
        &self,
        query: &LiteralSearchQuery,
        start: TextPosition,
        end: TextPosition,
    ) -> Option<SearchMatch> {
        let (start, end) = self.ordered_positions(start, end);
        self.find_next_bounded_with_finder(
            query.bytes(),
            query.len_chars(),
            &query.finder,
            start,
            end,
        )
    }

    /// Finds the last query match fully contained within `range`.
    pub fn find_prev_query_in_range(
        &self,
        query: &LiteralSearchQuery,
        range: TextRange,
    ) -> Option<SearchMatch> {
        let (start, end) = self.search_range_bounds(range);
        self.find_prev_bounded_with_finder(
            query.bytes(),
            query.len_chars(),
            &query.finder_rev,
            start,
            end,
        )
    }

    /// Finds the last query match fully contained between two typed positions.
    pub fn find_prev_query_between(
        &self,
        query: &LiteralSearchQuery,
        start: TextPosition,
        end: TextPosition,
    ) -> Option<SearchMatch> {
        let (start, end) = self.ordered_positions(start, end);
        self.find_prev_bounded_with_finder(
            query.bytes(),
            query.len_chars(),
            &query.finder_rev,
            start,
            end,
        )
    }

    fn find_next_bounded_with_finder(
        &self,
        needle_bytes: &[u8],
        needle_units: usize,
        finder: &Finder<'_>,
        start: TextPosition,
        end: TextPosition,
    ) -> Option<SearchMatch> {
        if start >= end {
            return None;
        }

        let found = self.find_next_with_finder(needle_bytes, needle_units, finder, start)?;
        (found.end() <= end).then_some(found)
    }

    fn find_next_with_offset_hint(
        &self,
        needle_bytes: &[u8],
        needle_units: usize,
        finder: &Finder<'_>,
        from: TextPosition,
        from_offset: usize,
    ) -> Option<(SearchMatch, usize)> {
        if let Some(rope) = &self.rope {
            return self.find_next_in_rope_from_offset(
                rope,
                needle_bytes,
                needle_units,
                finder,
                from,
                from_offset,
            );
        }

        let from = self.clamp_position(from);
        if let Some(piece_table) = &self.piece_table {
            return self.find_next_in_piece_table_from_offset(
                piece_table,
                needle_bytes,
                needle_units,
                finder,
                from,
                from_offset,
            );
        }

        self.find_next_in_mmap_from_offset(needle_bytes, needle_units, finder, from, from_offset)
    }

    fn find_next_with_offset_hint_bounded(
        &self,
        needle_bytes: &[u8],
        needle_units: usize,
        finder: &Finder<'_>,
        start: (TextPosition, usize),
        end: (TextPosition, usize),
    ) -> Option<(SearchMatch, usize)> {
        let (start, start_offset) = start;
        let (end, end_offset) = end;
        if start >= end || start_offset >= end_offset {
            return None;
        }

        let found = self.find_next_with_offset_hint(
            needle_bytes,
            needle_units,
            finder,
            start,
            start_offset,
        )?;
        (found.0.end() <= end).then_some(found)
    }

    fn find_prev_bounded_with_finder(
        &self,
        needle_bytes: &[u8],
        needle_units: usize,
        finder_rev: &FinderRev<'_>,
        start: TextPosition,
        end: TextPosition,
    ) -> Option<SearchMatch> {
        if start >= end {
            return None;
        }

        let found = self.find_prev_with_finder(needle_bytes, needle_units, finder_rev, end)?;
        (found.start() >= start).then_some(found)
    }

    fn search_range_bounds(&self, range: TextRange) -> (TextPosition, TextPosition) {
        let start = self.clamp_position(range.start());
        if range.is_empty() {
            return (start, start);
        }

        if let Some(rope) = &self.rope {
            let start_idx = Self::line_col_to_char_index(rope, start.line0(), start.col0());
            let end = self.position_for_char_index(start_idx.saturating_add(range.len_chars()));
            return (start, end);
        }

        if let Some(piece_table) = &self.piece_table {
            let start_offset = piece_table.byte_offset_for_col(start.line0(), start.col0());
            let start_offset = scanned_piece_table_byte_offset_for_position(piece_table, start)
                .unwrap_or(start_offset);
            let end_offset =
                piece_table.advance_offset_by_text_units(start_offset, range.len_chars());
            let end = piece_table.position_for_byte_offset_from(start_offset, start, end_offset);
            return (start, end);
        }

        let start_offset = self.mmap_byte_offset_for_position(start);
        let end_offset = self.mmap_advance_offset_by_text_units(start_offset, range.len_chars());
        let end = self.mmap_position_for_byte_offset_from(start_offset, start, end_offset);
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

    fn find_next_in_rope_from_offset(
        &self,
        rope: &Rope,
        needle: &[u8],
        needle_units: usize,
        finder: &Finder<'_>,
        from: TextPosition,
        start_byte: usize,
    ) -> Option<(SearchMatch, usize)> {
        let start_byte = start_byte.min(rope.len_bytes());
        let match_start = find_next_in_rope_chunks(rope, start_byte, needle.len(), finder)?;
        let start_pos = if match_start == start_byte {
            from
        } else {
            let start_char = rope.byte_to_char(match_start);
            self.position_for_char_index(start_char)
        };
        let end_pos = advance_position_by_bytes(start_pos, needle);
        let match_end = match_start
            .saturating_add(needle.len())
            .min(rope.len_bytes());
        Some((
            SearchMatch::new(TextRange::new(start_pos, needle_units), end_pos),
            match_end,
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

    fn find_next_in_mmap_from_offset(
        &self,
        needle: &[u8],
        needle_units: usize,
        finder: &Finder<'_>,
        from: TextPosition,
        start_offset: usize,
    ) -> Option<(SearchMatch, usize)> {
        let bytes = self.mmap_bytes();
        let file_len = self.file_len.min(bytes.len());
        if file_len == 0 {
            return None;
        }

        let start_offset = start_offset.min(file_len);
        let rel = finder.find(&bytes[start_offset..file_len])?;
        let match_start = start_offset.saturating_add(rel);
        let match_end = match_start.saturating_add(needle.len()).min(file_len);
        let start_pos = self.mmap_position_for_byte_offset_from(start_offset, from, match_start);
        let end_pos = advance_position_by_bytes(start_pos, needle);
        Some((
            SearchMatch::new(TextRange::new(start_pos, needle_units), end_pos),
            match_end,
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
        let start_col0 = from.col0();
        let start_pos = TextPosition::new(from.line0(), start_col0);
        let start_offset = scanned_piece_table_byte_offset_for_position(piece_table, start_pos)
            .unwrap_or_else(|| piece_table.byte_offset_for_col(from.line0(), start_col0));
        let (start_pos, _) = piece_table.find_literal_next_from_position(
            start_offset,
            start_pos,
            needle.len(),
            finder,
        )?;
        let end_pos = advance_position_by_bytes(start_pos, needle);
        Some(SearchMatch::new(
            TextRange::new(start_pos, needle_units),
            end_pos,
        ))
    }

    fn find_next_in_piece_table_from_offset(
        &self,
        piece_table: &PieceTable,
        needle: &[u8],
        needle_units: usize,
        finder: &Finder<'_>,
        from: TextPosition,
        start_offset: usize,
    ) -> Option<(SearchMatch, usize)> {
        let start_offset = start_offset.min(piece_table.total_len);
        let (start_pos, match_start) = piece_table.find_literal_next_from_position(
            start_offset,
            from,
            needle.len(),
            finder,
        )?;
        let match_end = match_start
            .saturating_add(needle.len())
            .min(piece_table.total_len);
        let end_pos = advance_position_by_bytes(start_pos, needle);
        Some((
            SearchMatch::new(TextRange::new(start_pos, needle_units), end_pos),
            match_end,
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
        let mut search_before = before;
        let mut end_offset = if let Some(offset) =
            scanned_piece_table_byte_offset_for_position(piece_table, before)
        {
            offset
        } else {
            // For unresolved positions past the indexed/scannable prefix we must
            // not widen reverse search to EOF. That can return matches after the
            // caller's logical `before` position, so fall back to the last known
            // safe boundary instead.
            let fallback = piece_table.known_byte_len.min(piece_table.total_len);
            search_before = piece_table.position_for_byte_offset(fallback);
            fallback
        };
        if memchr::memchr2(b'\n', b'\r', needle).is_none() {
            if let Some((adjusted_before, adjusted_end_offset)) =
                piece_table.prev_search_anchor_before_trailing_newline(before, end_offset)
            {
                search_before = adjusted_before;
                end_offset = adjusted_end_offset;
            }
        }
        let match_start = piece_table.find_literal_prev(end_offset, needle.len(), finder_rev)?;
        let start_pos = piece_table
            .position_for_byte_offset_before_same_line(end_offset, search_before, match_start)
            .unwrap_or_else(|| piece_table.position_for_byte_offset(match_start));
        let end_pos = advance_position_by_bytes(start_pos, needle);
        Some(SearchMatch::new(
            TextRange::new(start_pos, needle_units),
            end_pos,
        ))
    }

    pub(super) fn mmap_line_start_offset_exact(&self, target_line0: usize) -> Option<usize> {
        let bytes = self.mmap_bytes();
        let file_len = self.file_len.min(bytes.len());
        if file_len == 0 {
            return (target_line0 == 0).then_some(0);
        }

        let mut line0 = 0usize;
        let mut line_start = 0usize;
        if let Ok(offsets) = self.line_offsets.read() {
            (line0, line_start) = line_offsets_anchor_for_line(&offsets, target_line0);
            line_start = line_start.min(file_len);
        }

        while line0 < target_line0 && line_start < file_len {
            let next = next_line_start_exact(bytes, file_len, line_start);
            if next <= line_start {
                break;
            }
            line_start = next;
            line0 += 1;
        }

        (line0 == target_line0).then_some(line_start)
    }

    pub(super) fn mmap_byte_offset_for_position(&self, position: TextPosition) -> usize {
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

    pub(super) fn mmap_position_for_byte_offset(&self, byte_offset: usize) -> TextPosition {
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

    fn mmap_position_for_byte_offset_from(
        &self,
        anchor_offset: usize,
        anchor_position: TextPosition,
        byte_offset: usize,
    ) -> TextPosition {
        let bytes = self.mmap_bytes();
        let file_len = self.file_len.min(bytes.len());
        let anchor_offset = anchor_offset.min(file_len);
        let target = byte_offset.min(file_len);
        if target <= anchor_offset {
            return anchor_position;
        }

        let mut state = PositionScanState::new(anchor_position.line0(), anchor_position.col0());
        scan_position_bytes(&bytes[anchor_offset..target], &mut state);
        state.position()
    }

    fn mmap_advance_offset_by_text_units(&self, start: usize, text_units: usize) -> usize {
        let bytes = self.mmap_bytes();
        let file_len = self.file_len.min(bytes.len());
        let start = start.min(file_len);
        if text_units == 0 || start >= file_len {
            return start;
        }

        let mut remaining = text_units;
        let mut offset = start;
        let mut pending_cr = false;
        let mut i = start;
        while i < file_len && (remaining > 0 || pending_cr) {
            if pending_cr {
                pending_cr = false;
                if bytes[i] == b'\n' {
                    i += 1;
                    offset = offset.saturating_add(1);
                    continue;
                }
            }

            match bytes[i] {
                b'\n' => {
                    remaining = remaining.saturating_sub(1);
                    i += 1;
                    offset = offset.saturating_add(1);
                }
                b'\r' => {
                    remaining = remaining.saturating_sub(1);
                    pending_cr = true;
                    i += 1;
                    offset = offset.saturating_add(1);
                }
                _ => {
                    remaining = remaining.saturating_sub(1);
                    let step = utf8_step(bytes, i, file_len);
                    i += step;
                    offset = offset.saturating_add(step);
                }
            }
        }
        offset.min(file_len)
    }

    fn search_byte_offset_for_position(&self, position: TextPosition) -> usize {
        let position = self.clamp_position(position);
        if let Some(rope) = &self.rope {
            let char_index = Self::line_col_to_char_index(rope, position.line0(), position.col0());
            return rope.char_to_byte(char_index);
        }
        if let Some(piece_table) = &self.piece_table {
            return scanned_piece_table_byte_offset_for_position(piece_table, position)
                .unwrap_or_else(|| {
                    piece_table.byte_offset_for_col(position.line0(), position.col0())
                });
        }
        self.mmap_byte_offset_for_position(position)
    }

    fn buffered_literal_range(&self, start_offset: usize, end_offset: usize) -> Option<Vec<u8>> {
        if start_offset >= end_offset {
            return Some(Vec::new());
        }
        let span = end_offset.saturating_sub(start_offset);
        if span > BUFFERED_LITERAL_RANGE_MAX_BYTES {
            return None;
        }

        if let Some(piece_table) = &self.piece_table {
            return Some(piece_table.read_range(start_offset, end_offset));
        }

        if self.rope.is_some() {
            return None;
        }

        let bytes = self.mmap_bytes();
        let file_len = self.file_len.min(bytes.len());
        let start = start_offset.min(file_len);
        let end = end_offset.min(file_len).max(start);
        Some(bytes[start..end].to_vec())
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
    fn prev_search_anchor_before_trailing_newline(
        &self,
        before: TextPosition,
        end_offset: usize,
    ) -> Option<(TextPosition, usize)> {
        if before.line0() == 0
            || before.col0() != 0
            || end_offset == 0
            || end_offset != self.total_len
        {
            return None;
        }

        let newline_len = self.newline_len_before(end_offset);
        if newline_len == 0 {
            return None;
        }

        let prev_line0 = before.line0().saturating_sub(1);
        let prev_col0 = self.line_len_chars(prev_line0);
        Some((
            TextPosition::new(prev_line0, prev_col0),
            end_offset.saturating_sub(newline_len),
        ))
    }

    fn position_for_byte_offset_before_same_line(
        &self,
        anchor_offset: usize,
        anchor_position: TextPosition,
        byte_offset: usize,
    ) -> Option<TextPosition> {
        let anchor_offset = anchor_offset.min(self.total_len);
        let target = byte_offset.min(anchor_offset);
        if target == anchor_offset {
            return Some(anchor_position);
        }

        let span = anchor_offset.saturating_sub(target);
        if span == 0 || span > REVERSE_POSITION_FAST_PATH_BYTES || anchor_position.col0() == 0 {
            return None;
        }

        let bytes = self.read_range(target, anchor_offset);
        if memchr::memchr2(b'\n', b'\r', &bytes).is_some() {
            return None;
        }

        let mut units = 0usize;
        let mut i = 0usize;
        while i < bytes.len() {
            units = units.saturating_add(1);
            i += utf8_step(&bytes, i, bytes.len());
        }
        (units <= anchor_position.col0())
            .then(|| TextPosition::new(anchor_position.line0(), anchor_position.col0() - units))
    }

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

    pub(crate) fn position_for_byte_offset_from(
        &self,
        anchor_offset: usize,
        anchor_position: TextPosition,
        byte_offset: usize,
    ) -> TextPosition {
        let anchor_offset = anchor_offset.min(self.total_len);
        let target = byte_offset.min(self.total_len);
        if target <= anchor_offset {
            return anchor_position;
        }

        let mut state = PositionScanState::new(anchor_position.line0(), anchor_position.col0());
        self.pieces
            .visit_range(anchor_offset, target, |piece, local_start, local_end| {
                let seg_start = piece.start + local_start;
                let seg_end = piece.start + local_end;
                let src = self.source_bytes(piece.src);
                scan_position_bytes(&src[seg_start..seg_end], &mut state);
            });
        state.position()
    }

    fn find_literal_next_from_position(
        &self,
        start: usize,
        start_pos: TextPosition,
        needle_len: usize,
        finder: &Finder<'_>,
    ) -> Option<(TextPosition, usize)> {
        if needle_len == 0 || start >= self.total_len {
            return None;
        }

        let mut overlap = Vec::with_capacity(needle_len.saturating_sub(1));
        let mut window = Vec::new();
        let mut overlap_start = start;
        let mut overlap_position = start_pos;
        let mut found = None;

        self.pieces
            .visit_range_while(start, self.total_len, |piece, local_start, local_end| {
                let seg_start = piece.start + local_start;
                let seg_end = piece.start + local_end;
                let src = self.source_bytes(piece.src);
                let segment = &src[seg_start..seg_end];
                if segment.is_empty() {
                    return true;
                }

                let overlap_len = overlap.len();
                refill_search_window(&mut window, &overlap, segment);
                if let Some(rel) = find_window_match(finder, &window, overlap_len, needle_len) {
                    let match_position = if rel == 0 {
                        overlap_position
                    } else {
                        advance_position_by_bytes(overlap_position, &window[..rel])
                    };
                    found = Some((match_position, overlap_start.saturating_add(rel)));
                    return false;
                }

                let keep = needle_len.saturating_sub(1).min(window.len());
                let consumed = window.len().saturating_sub(keep);
                if consumed > 0 {
                    overlap_position =
                        advance_position_by_bytes(overlap_position, &window[..consumed]);
                }
                overlap.clear();
                overlap.extend_from_slice(&window[window.len().saturating_sub(keep)..]);
                overlap_start = overlap_start.saturating_add(consumed);
                true
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
        let mut segment_end = end.min(self.total_len);
        let mut found = None;

        self.pieces.visit_range_rev_while(
            0,
            end.min(self.total_len),
            |piece, local_start, local_end| {
                let seg_start = piece.start + local_start;
                let seg_end = piece.start + local_end;
                let src = self.source_bytes(piece.src);
                let segment = &src[seg_start..seg_end];
                if segment.is_empty() {
                    return true;
                }

                let segment_start = segment_end.saturating_sub(segment.len());
                window.clear();
                window.extend_from_slice(segment);
                window.extend_from_slice(&overlap);
                if let Some(rel) = finder.rfind_iter(&window).find(|&rel| rel < segment.len()) {
                    found = Some(segment_start.saturating_add(rel));
                    return false;
                }

                let keep = needle_len.saturating_sub(1).min(window.len());
                overlap.clear();
                overlap.extend_from_slice(&window[..keep]);
                segment_end = segment_start;
                true
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
