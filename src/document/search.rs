use super::alignment::AlignDirection;
use super::encoding_engine::EncodingEngine;
use super::*;
use memchr::memmem::{Finder, FinderRev};

const REVERSE_POSITION_FAST_PATH_BYTES: usize = 1024;
const BUFFERED_LITERAL_RANGE_MAX_BYTES: usize = 8 * 1024 * 1024;

/// Maximum residual mmap byte distance from the highest indexed line anchor to
/// EOF for which `mmap_byte_offset_for_position` will still pay the precise
/// per-line walk for an out-of-range typed line. Above this threshold the
/// helper collapses out-of-range lines to `file_len`, which preserves the
/// "at or past EOF" semantics that typed search relies on while avoiding
/// multi-gigabyte mmap scans on huge files.
const LARGE_MMAP_OFFSET_FAST_PATH_BYTES: usize = 64 * 1024 * 1024;

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

fn scan_position_bytes(
    engine: &dyn super::encoding_engine::EncodingEngine,
    bytes: &[u8],
    state: &mut PositionScanState,
) {
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
                let step = engine.step(bytes, i, bytes.len()).max(1);
                i = i.saturating_add(step).min(bytes.len());
            }
        }
    }
}

pub(super) fn search_text_units(text: &str) -> usize {
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
                let step = super::encoding_engine::Utf8Engine
                    .step(bytes, i, bytes.len())
                    .max(1);
                i = i.saturating_add(step).min(bytes.len());
            }
        }
    }
    units
}

pub(super) fn advance_position_by_bytes(
    start: TextPosition,
    bytes: &[u8],
    engine: &dyn super::encoding_engine::EncodingEngine,
) -> TextPosition {
    let mut state = PositionScanState::new(start.line0(), start.col0());
    scan_position_bytes(engine, bytes, &mut state);
    state.position()
}

pub(super) fn utf8_next_line_start(bytes: &[u8], file_len: usize, line_start: usize) -> usize {
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
    Some(
        scan_start.saturating_add(byte_offset_for_text_col_with_engine(
            &bytes,
            range.range,
            position.col0(),
            piece_table.engine(),
        )),
    )
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
///
/// The searchers are built over the needle's bytes **in the document's
/// encoding** (`new_with_encoding`), so literal search works in encoded
/// documents (windows-1251, UTF-16, ...) instead of silently missing.
/// `utf8_bytes` / the UTF-8 searchers serve the rope-backed path, where the
/// buffer always holds UTF-8 text. `dead` marks a needle that cannot be
/// represented in the document's encoding at all — no match is possible.
fn is_utf16_name(name: &str) -> bool {
    name == "UTF-16LE" || name == "UTF-16BE"
}

#[derive(Clone, Debug)]
pub struct LiteralSearchQuery {
    needle: String,
    needle_units: usize,
    dead: bool,
    even_grid: bool,
    boundary_walk: bool,
    search_bytes: Vec<u8>,
    finder: Finder<'static>,
    finder_rev: FinderRev<'static>,
    utf8_bytes: Vec<u8>,
    utf8_finder: Finder<'static>,
    utf8_finder_rev: FinderRev<'static>,
}

impl LiteralSearchQuery {
    /// Builds a reusable literal-search query.
    ///
    /// Empty needles return `None`. Kept for UTF-8-backed call sites;
    /// document call sites go through [`Self::new_with_encoding`].
    pub fn new(needle: impl Into<String>) -> Option<Self> {
        Self::new_with_encoding(needle, DocumentEncoding::utf8())
    }

    /// Builds a reusable literal-search query for a document encoding.
    ///
    /// The needle is encoded into the document's encoding before the byte
    /// searchers are built, so the literal search finds words in encoded
    /// documents (windows-1251, UTF-16, ...). An empty needle returns
    /// `None`; a needle that cannot be represented in the document's
    /// encoding produces a dead query whose searches always answer `None`
    /// (the text cannot occur in the document).
    pub fn new_with_encoding(
        needle: impl Into<String>,
        encoding: DocumentEncoding,
    ) -> Option<Self> {
        let needle = needle.into();
        if needle.is_empty() {
            return None;
        }

        let needle_units = search_text_units(&needle);
        if needle_units == 0 {
            return None;
        }

        let utf8_bytes = needle.as_bytes().to_vec();
        let utf8_finder = Finder::new(&utf8_bytes).into_owned();
        let utf8_finder_rev = FinderRev::new(&utf8_bytes).into_owned();

        let mut dead = false;
        let even_grid = is_utf16_name(encoding.name());
        // CJK multibyte byte grids are not self-synchronizing: a byte pair
        // can appear starting on a trail byte of a neighboring character.
        // Candidates get a boundary walk check (see candidate_boundary_ok).
        let boundary_walk = !encoding.is_utf8()
            && !even_grid
            && !encoding_engine::SingleByteEngine::supports(encoding);
        let search_bytes = if encoding.is_utf8() {
            utf8_bytes.clone()
        } else if is_utf16_name(encoding.name()) {
            let mut bytes = Vec::with_capacity(needle.len() * 2);
            for unit in needle.encode_utf16() {
                if encoding.name() == "UTF-16LE" {
                    bytes.extend_from_slice(&unit.to_le_bytes());
                } else {
                    bytes.extend_from_slice(&unit.to_be_bytes());
                }
            }
            bytes
        } else {
            let (bytes, _, had_unmappable) = encoding.as_encoding().encode(&needle);
            if had_unmappable {
                // The needle cannot exist in this document's encoding:
                // keep the query alive but dead, so searches answer None.
                dead = true;
                Vec::new()
            } else {
                bytes.into_owned()
            }
        };

        let finder = Finder::new(&search_bytes).into_owned();
        let finder_rev = FinderRev::new(&search_bytes).into_owned();
        Some(Self {
            needle,
            needle_units,
            dead,
            even_grid,
            boundary_walk,
            search_bytes,
            finder,
            finder_rev,
            utf8_bytes,
            utf8_finder,
            utf8_finder_rev,
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

    /// Returns `true` when the needle cannot be represented in the
    /// document's encoding, so no match is possible.
    pub const fn is_dead(&self) -> bool {
        self.dead
    }

    /// Byte-level needle for the piece-table / mmap backends (document
    /// encoding).
    fn bytes(&self) -> &[u8] {
        &self.search_bytes
    }

    /// Byte-level needle for the rope backend (always UTF-8).
    fn utf8_bytes(&self) -> &[u8] {
        &self.utf8_bytes
    }

    /// True when a byte-grid candidate at the absolute offset is a real
    /// match: UTF-16 candidates must sit on the 2-byte cell grid (the
    /// document starts at byte 0 and every edit lands on a cell boundary),
    /// so a hit at an odd offset is a byte collision, not the text.
    const fn candidate_grid_ok(&self, abs_offset: usize) -> bool {
        !self.even_grid || abs_offset % 2 == 0
    }

    /// Walks `bytes` from the known character boundary `from` (byte 0 of
    /// the slice is always a boundary for the callers of this helper)
    /// through the encoding's cell sizes and reports whether the walk
    /// lands exactly on `to`. Multibyte byte grids are not
    /// self-synchronizing, so a memmem candidate must be proven to sit on
    /// a real character boundary this way.
    fn boundary_walk_ok(
        &self,
        engine: &dyn super::encoding_engine::EncodingEngine,
        bytes: &[u8],
        from: usize,
        to: usize,
    ) -> bool {
        if !self.boundary_walk {
            return true;
        }
        let mut cursor = from;
        while cursor < to {
            let step = engine.step(bytes, cursor, bytes.len()).max(1);
            cursor = (cursor + step).min(to + 1);
            if cursor > to {
                return false;
            }
        }
        cursor == to
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
        if query.is_dead() {
            self.finished = true;
            return None;
        }
        if let Some(buffered) = self.buffered_range.as_mut() {
            loop {
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
                let match_start_abs = buffered.base_offset.saturating_add(match_start_rel);
                if !query.candidate_grid_ok(match_start_abs) {
                    // UTF-16 byte collision at an odd offset: skip it and
                    // keep scanning the buffered range.
                    buffered.next_rel_offset = match_start_rel.saturating_add(1);
                    continue;
                }
                let start_pos = if match_start_rel == buffered.next_rel_offset {
                    self.next_from
                } else {
                    advance_position_by_bytes(
                        self.next_from,
                        &buffered.bytes[buffered.next_rel_offset..match_start_rel],
                        self.doc.encoding_engine(),
                    )
                };
                let match_end_rel = match_start_rel
                    .saturating_add(query.bytes().len())
                    .min(buffered.bytes.len());
                let end_pos =
                    advance_position_by_bytes(start_pos, query.bytes(), self.doc.encoding_engine());
                buffered.next_rel_offset = match_end_rel;
                self.next_from = end_pos;
                self.next_offset = buffered.base_offset.saturating_add(match_end_rel);
                return Some(SearchMatch::new(
                    TextRange::new(start_pos, query.len_chars()),
                    end_pos,
                ));
            }
        }
        let found = if let Some(end) = self.end {
            if self.next_from >= end {
                self.finished = true;
                return None;
            }
            let end_offset = self.end_offset.unwrap_or(self.next_offset);
            self.doc.find_next_with_offset_hint_bounded(
                query,
                (self.next_from, self.next_offset),
                (end, end_offset),
            )
        } else {
            self.doc
                .find_next_with_offset_hint(query, self.next_from, self.next_offset)
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
        let query = LiteralSearchQuery::new_with_encoding(needle, self.encoding)?;
        if query.is_dead() {
            return None;
        }
        self.find_next_with_finder(&query, from)
    }

    /// Finds the previous literal match whose end is at or before `before`.
    ///
    /// Empty needles return `None`. The match follows the current backing
    /// representation: clean mmap/piece-table documents search stored bytes
    /// (including stored CRLF), while rope-backed documents search the current
    /// in-memory `\n` representation.
    pub fn find_prev(&self, needle: &str, before: TextPosition) -> Option<SearchMatch> {
        let query = LiteralSearchQuery::new_with_encoding(needle, self.encoding)?;
        if query.is_dead() {
            return None;
        }
        self.find_prev_with_finder(&query, before)
    }

    /// Finds the next literal match for a reusable compiled query.
    pub fn find_next_query(
        &self,
        query: &LiteralSearchQuery,
        from: TextPosition,
    ) -> Option<SearchMatch> {
        self.find_next_with_finder(query, from)
    }

    /// Finds the previous literal match for a reusable compiled query.
    pub fn find_prev_query(
        &self,
        query: &LiteralSearchQuery,
        before: TextPosition,
    ) -> Option<SearchMatch> {
        self.find_prev_with_finder(query, before)
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
            LiteralSearchQuery::new_with_encoding(needle, self.encoding),
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
        self.find_all_between_internal(
            LiteralSearchQuery::new_with_encoding(needle, self.encoding),
            start,
            end,
        )
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
        self.find_all_between_internal(
            LiteralSearchQuery::new_with_encoding(needle, self.encoding),
            start,
            end,
        )
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

        let query = LiteralSearchQuery::new_with_encoding(needle, self.encoding)?;
        if query.is_dead() {
            return None;
        }
        let (start, end) = self.search_range_bounds(range);
        let start_offset = self.search_byte_offset_for_position(start);
        let end_offset = self.search_byte_offset_for_position(end);
        self.find_next_bounded_with_finder(&query, (start, start_offset), (end, end_offset))
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

        let query = LiteralSearchQuery::new_with_encoding(needle, self.encoding)?;
        if query.is_dead() {
            return None;
        }
        let start_offset = self.search_byte_offset_for_position(start);
        let end_offset = self.search_byte_offset_for_position(end);
        self.find_next_bounded_with_finder(&query, (start, start_offset), (end, end_offset))
    }

    /// Finds the last literal match fully contained within `range`.
    pub fn find_prev_in_range(&self, needle: &str, range: TextRange) -> Option<SearchMatch> {
        if needle.is_empty() {
            return None;
        }

        let query = LiteralSearchQuery::new_with_encoding(needle, self.encoding)?;
        if query.is_dead() {
            return None;
        }
        let (start, end) = self.search_range_bounds(range);
        self.find_prev_bounded_with_finder(&query, start, end)
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

        let query = LiteralSearchQuery::new_with_encoding(needle, self.encoding)?;
        if query.is_dead() {
            return None;
        }
        let (start, end) = self.ordered_positions(start, end);
        self.find_prev_bounded_with_finder(&query, start, end)
    }

    /// Finds the first query match fully contained within `range`.
    pub fn find_next_query_in_range(
        &self,
        query: &LiteralSearchQuery,
        range: TextRange,
    ) -> Option<SearchMatch> {
        let (start, end) = self.search_range_bounds(range);
        let start_offset = self.search_byte_offset_for_position(start);
        let end_offset = self.search_byte_offset_for_position(end);
        self.find_next_bounded_with_finder(query, (start, start_offset), (end, end_offset))
    }

    /// Finds the first query match fully contained between two typed positions.
    pub fn find_next_query_between(
        &self,
        query: &LiteralSearchQuery,
        start: TextPosition,
        end: TextPosition,
    ) -> Option<SearchMatch> {
        let (start, end) = self.ordered_positions(start, end);
        let start_offset = self.search_byte_offset_for_position(start);
        let end_offset = self.search_byte_offset_for_position(end);
        self.find_next_bounded_with_finder(query, (start, start_offset), (end, end_offset))
    }

    /// Finds the last query match fully contained within `range`.
    pub fn find_prev_query_in_range(
        &self,
        query: &LiteralSearchQuery,
        range: TextRange,
    ) -> Option<SearchMatch> {
        let (start, end) = self.search_range_bounds(range);
        self.find_prev_bounded_with_finder(query, start, end)
    }

    /// Finds the last query match fully contained between two typed positions.
    pub fn find_prev_query_between(
        &self,
        query: &LiteralSearchQuery,
        start: TextPosition,
        end: TextPosition,
    ) -> Option<SearchMatch> {
        let (start, end) = self.ordered_positions(start, end);
        self.find_prev_bounded_with_finder(query, start, end)
    }

    fn find_next_bounded_with_finder(
        &self,
        query: &LiteralSearchQuery,
        start: (TextPosition, usize),
        end: (TextPosition, usize),
    ) -> Option<SearchMatch> {
        let (start, start_offset) = start;
        let (end, end_offset) = end;
        if start >= end || start_offset >= end_offset {
            return None;
        }

        let found = self.find_next_with_finder(query, start)?;
        (found.end() <= end).then_some(found)
    }

    fn find_next_with_offset_hint(
        &self,
        query: &LiteralSearchQuery,
        from: TextPosition,
        from_offset: usize,
    ) -> Option<(SearchMatch, usize)> {
        if query.is_dead() {
            return None;
        }
        if let Some(rope) = &self.rope {
            return self.find_next_in_rope_from_offset(
                rope,
                query.utf8_bytes(),
                query.len_chars(),
                &query.utf8_finder,
                from,
                from_offset,
            );
        }

        let from = self.clamp_position(from);
        if let Some(piece_table) = &self.piece_table {
            return self.find_next_in_piece_table_from_offset(
                piece_table,
                query,
                from,
                from_offset,
            );
        }

        self.find_next_in_mmap_from_offset(query, from, from_offset)
    }

    fn find_next_with_offset_hint_bounded(
        &self,
        query: &LiteralSearchQuery,
        start: (TextPosition, usize),
        end: (TextPosition, usize),
    ) -> Option<(SearchMatch, usize)> {
        let (start, start_offset) = start;
        let (end, end_offset) = end;
        if start >= end || start_offset >= end_offset {
            return None;
        }

        let found = self.find_next_with_offset_hint(query, start, start_offset)?;
        (found.0.end() <= end).then_some(found)
    }

    fn find_prev_bounded_with_finder(
        &self,
        query: &LiteralSearchQuery,
        start: TextPosition,
        end: TextPosition,
    ) -> Option<SearchMatch> {
        if start >= end {
            return None;
        }

        let found = self.find_prev_with_finder(query, end)?;
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
            // Engine-aware bounds: the piece table may hold raw non-UTF-8
            // bytes (Class A / UTF-16 / CJK), so the range end must be
            // walked through the document's engine, not UTF-8 stepping that
            // would swallow collision pairs as one unit and overshoot the
            // requested range.
            let engine = piece_table.engine();
            let end_offset = piece_table.advance_offset_by_text_units_with_engine(
                start_offset,
                range.len_chars(),
                engine,
            );
            let end =
                piece_table.position_for_byte_offset_from(start_offset, start, end_offset, engine);
            return (start, end);
        }

        let start_offset = self.mmap_byte_offset_for_position(start);
        let end_offset = self.mmap_advance_offset_by_text_units(start_offset, range.len_chars());
        let end = self.mmap_position_for_byte_offset_from(start_offset, start, end_offset);
        (start, end)
    }

    fn find_next_with_finder(
        &self,
        query: &LiteralSearchQuery,
        from: TextPosition,
    ) -> Option<SearchMatch> {
        if let Some(rope) = &self.rope {
            return self.find_next_in_rope(
                rope,
                query.utf8_bytes(),
                query.len_chars(),
                &query.utf8_finder,
                from,
            );
        }

        let from = self.clamp_position(from);
        if let Some(piece_table) = &self.piece_table {
            return self.find_next_in_piece_table(piece_table, query, from);
        }

        self.find_next_in_mmap(query, from)
    }

    fn find_prev_with_finder(
        &self,
        query: &LiteralSearchQuery,
        before: TextPosition,
    ) -> Option<SearchMatch> {
        if let Some(rope) = &self.rope {
            return self.find_prev_in_rope(
                rope,
                query.utf8_bytes(),
                query.len_chars(),
                &query.utf8_finder_rev,
                before,
            );
        }

        let before = self.clamp_position(before);
        if let Some(piece_table) = &self.piece_table {
            return self.find_prev_in_piece_table(piece_table, query, before);
        }

        self.find_prev_in_mmap(query, before)
    }

    fn find_next_in_rope(
        &self,
        rope: &EditBuffer,
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
        let end_pos =
            advance_position_by_bytes(start_pos, needle, &super::encoding_engine::Utf8Engine);
        Some(SearchMatch::new(
            TextRange::new(start_pos, needle_units),
            end_pos,
        ))
    }

    fn find_next_in_rope_from_offset(
        &self,
        rope: &EditBuffer,
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
        let end_pos =
            advance_position_by_bytes(start_pos, needle, &super::encoding_engine::Utf8Engine);
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
        query: &LiteralSearchQuery,
        from: TextPosition,
    ) -> Option<SearchMatch> {
        let from_offset = self.mmap_byte_offset_for_position(from);
        self.find_next_in_mmap_from_offset(query, from, from_offset)
            .map(|(m, _)| m)
    }

    #[allow(dead_code)]
    fn find_next_in_mmap_legacy(
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
        query: &LiteralSearchQuery,
        from: TextPosition,
        start_offset: usize,
    ) -> Option<(SearchMatch, usize)> {
        let engine = self.encoding_engine();
        let bytes = self.mmap_bytes();
        let file_len = self.file_len.min(bytes.len());
        if file_len == 0 {
            return None;
        }

        let mut search_from = start_offset.min(file_len);
        let boundary = start_offset.min(file_len);
        loop {
            let rel = query.finder.find(&bytes[search_from..file_len])?;
            let match_start = search_from.saturating_add(rel);
            if !query.candidate_grid_ok(match_start)
                || !query.boundary_walk_ok(
                    engine,
                    bytes,
                    boundary.saturating_sub(search_from),
                    match_start.saturating_sub(search_from),
                )
            {
                // A candidate that is not on a real character boundary
                // (odd-offset UTF-16 collision, multibyte splice) is a
                // byte splice, not the text: skip it and keep scanning.
                search_from = match_start.saturating_add(1);
                continue;
            }
            let match_end = match_start
                .saturating_add(query.bytes().len())
                .min(file_len);
            let start_pos =
                self.mmap_position_for_byte_offset_from(start_offset, from, match_start);
            let end_pos = advance_position_by_bytes(start_pos, query.bytes(), engine);
            return Some((
                SearchMatch::new(TextRange::new(start_pos, query.len_chars()), end_pos),
                match_end,
            ));
        }
    }

    fn find_prev_in_mmap(
        &self,
        query: &LiteralSearchQuery,
        before: TextPosition,
    ) -> Option<SearchMatch> {
        let bytes = self.mmap_bytes();
        let file_len = self.file_len.min(bytes.len());
        if file_len == 0 {
            return None;
        }

        let engine = self.encoding_engine();
        let end_offset = self.mmap_byte_offset_for_position(before);
        let mut search_end = end_offset.min(file_len);
        loop {
            let match_start = query.finder_rev.rfind(&bytes[..search_end])?;
            // Boundary proof: walk forward from the nearest line break at
            // or before the candidate (a line start is always a character
            // boundary; byte 0 is one too).
            let window_start = match_start.saturating_sub(64 * 1024);
            let anchor = memchr::memrchr2(b'\n', b'\r', &bytes[window_start..match_start])
                .map(|p| window_start + p + 1)
                .unwrap_or(0);
            if !query.boundary_walk_ok(engine, bytes, anchor, match_start) {
                // Spliced non-boundary candidate: exclude it and keep
                // scanning backwards.
                search_end = match_start;
                continue;
            }
            let match_end = match_start
                .saturating_add(query.bytes().len())
                .min(file_len);
            let start_pos = self.mmap_position_for_byte_offset(match_start);
            let end_pos = self.mmap_position_for_byte_offset(match_end);
            return Some(SearchMatch::new(
                TextRange::new(start_pos, query.len_chars()),
                end_pos,
            ));
        }
    }

    fn find_next_in_piece_table(
        &self,
        piece_table: &PieceTable,
        query: &LiteralSearchQuery,
        from: TextPosition,
    ) -> Option<SearchMatch> {
        let start_col0 = from.col0();
        let start_pos = TextPosition::new(from.line0(), start_col0);
        let engine = piece_table.engine();
        let start_offset = scanned_piece_table_byte_offset_for_position(piece_table, start_pos)
            .unwrap_or_else(|| piece_table.byte_offset_for_col(from.line0(), start_col0));
        let mut search_offset = start_offset;
        loop {
            let (start_pos, match_start) = piece_table.find_literal_next_from_position(
                search_offset,
                start_pos,
                query.bytes().len(),
                &query.finder,
            )?;
            if !query.candidate_grid_ok(match_start) {
                // UTF-16 byte collision at an odd offset: skip it and keep
                // scanning.
                search_offset = match_start.saturating_add(1);
                continue;
            }
            let end_pos = advance_position_by_bytes(start_pos, query.bytes(), engine);
            return Some(SearchMatch::new(
                TextRange::new(start_pos, query.len_chars()),
                end_pos,
            ));
        }
    }

    fn find_next_in_piece_table_from_offset(
        &self,
        piece_table: &PieceTable,
        query: &LiteralSearchQuery,
        from: TextPosition,
        start_offset: usize,
    ) -> Option<(SearchMatch, usize)> {
        let mut search_offset = start_offset.min(piece_table.total_len);
        let boundary = search_offset;
        let engine = piece_table.engine();
        loop {
            let (start_pos, match_start) = piece_table.find_literal_next_from_position(
                search_offset,
                from,
                query.bytes().len(),
                &query.finder,
            )?;
            let walk_ok = if query.boundary_walk {
                // Walk the stored bytes from the last known boundary to the
                // candidate. The slice is small when candidates are close;
                // far candidates pay one ranged read.
                let base = boundary.min(match_start);
                let slice = piece_table
                    .read_range(base, match_start.max(boundary).min(piece_table.total_len));
                query.boundary_walk_ok(
                    engine,
                    &slice,
                    boundary.min(match_start).saturating_sub(base),
                    match_start.saturating_sub(base),
                )
            } else {
                query.candidate_grid_ok(match_start)
            };
            if !walk_ok {
                // A candidate that is not on a real character boundary
                // (odd-offset UTF-16 collision, multibyte splice) is a
                // byte splice, not the text: skip it and keep scanning.
                search_offset = match_start.saturating_add(1);
                continue;
            }
            let match_end = match_start
                .saturating_add(query.bytes().len())
                .min(piece_table.total_len);
            let end_pos = advance_position_by_bytes(start_pos, query.bytes(), engine);
            return Some((
                SearchMatch::new(TextRange::new(start_pos, query.len_chars()), end_pos),
                match_end,
            ));
        }
    }

    fn find_prev_in_piece_table(
        &self,
        piece_table: &PieceTable,
        query: &LiteralSearchQuery,
        before: TextPosition,
    ) -> Option<SearchMatch> {
        let engine = piece_table.engine();
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
            search_before = piece_table.position_for_byte_offset(fallback, engine);
            fallback
        };
        if memchr::memchr2(b'\n', b'\r', query.utf8_bytes()).is_none() {
            if let Some((adjusted_before, adjusted_end_offset)) =
                piece_table.prev_search_anchor_before_trailing_newline(before, end_offset)
            {
                search_before = adjusted_before;
                end_offset = adjusted_end_offset;
            }
        }
        let mut search_end = end_offset;
        let match_start = loop {
            let candidate = piece_table.find_literal_prev(
                search_end,
                query.bytes().len(),
                &query.finder_rev,
            )?;
            let walk_ok = if query.boundary_walk {
                // Boundary proof: walk forward from the nearest line break
                // at or before the candidate (a line start is always a
                // character boundary). The window read is small; a candidate
                // without a line break in the window is rejected
                // conservatively.
                let window_start = candidate.saturating_sub(64 * 1024);
                let slice = piece_table.read_range(
                    window_start,
                    (candidate + query.bytes().len()).min(piece_table.total_len),
                );
                let needle_rel = candidate.saturating_sub(window_start);
                let anchor = memchr::memrchr2(b'\n', b'\r', &slice[..needle_rel])
                    .map(|p| p + 1)
                    .unwrap_or(0);
                query.boundary_walk_ok(engine, &slice, anchor, needle_rel)
            } else {
                query.candidate_grid_ok(candidate)
            };
            if !walk_ok {
                // Spliced non-boundary candidate: exclude it and keep
                // scanning backwards.
                search_end = candidate;
                continue;
            }
            break candidate;
        };
        let start_pos = piece_table
            .position_for_byte_offset_before_same_line(
                end_offset,
                search_before,
                match_start,
                engine,
            )
            .unwrap_or_else(|| piece_table.position_for_byte_offset(match_start, engine));
        let end_pos = advance_position_by_bytes(start_pos, query.bytes(), engine);
        Some(SearchMatch::new(
            TextRange::new(start_pos, query.len_chars()),
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
        // A disk index keeps sparse checkpoints even while the in-memory
        // offsets are still only a prefix. Prefer the newest checkpoint so a
        // random viewport read scans only the bounded gap to its target line,
        // rather than treating the average-line estimate as authoritative.
        if let Some((checkpoint_line0, checkpoint_byte0)) =
            self.disk_index_checkpoint_for_line(target_line0)
        {
            if checkpoint_line0 >= line0 {
                line0 = checkpoint_line0;
                line_start = checkpoint_byte0.min(file_len);
            }
        }

        let engine = self.encoding_engine();
        while line0 < target_line0 && line_start < file_len {
            let next = engine.next_line_start(bytes, file_len, line_start);
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
        let mut last_anchor_line = 0usize;
        if let Ok(offsets) = self.line_offsets.read() {
            (line0, line_start) = line_offsets_anchor_for_line(&offsets, position.line0());
            line_start = line_start.min(file_len);
            last_anchor_line = offsets.len().saturating_sub(1);
        }

        // Fast path for out-of-range positions on huge mmap files. When the
        // requested line is past the highest indexed line anchor and the
        // residual byte distance to EOF is large, walking line-by-line from
        // the last anchor up to that out-of-range line would scan multiple
        // gigabytes through the file. For typed-search semantics that scan
        // produces no useful information beyond "this is at or past EOF", so
        // we collapse it to `file_len` directly. Inputs like
        // `TextPosition::new(usize::MAX, usize::MAX)` (used by `find_prev` to
        // mean "from the end of the document") used to take many minutes on a
        // 50 GiB file before this short-circuit.
        if position.line0() > last_anchor_line {
            let residual = file_len.saturating_sub(line_start);
            if residual >= LARGE_MMAP_OFFSET_FAST_PATH_BYTES {
                return file_len;
            }
        }

        while line0 < position.line0() && line_start < file_len {
            let next = self
                .encoding_engine()
                .next_line_start(bytes, file_len, line_start);
            if next <= line_start {
                break;
            }
            line_start = next;
            line0 += 1;
        }

        let line_end = self
            .encoding_engine()
            .next_line_start(bytes, file_len, line_start);
        super::byte_offset_for_text_col_with_engine(
            bytes,
            (line_start, line_end),
            position.col0(),
            self.encoding_engine(),
        )
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
        scan_position_bytes(
            self.encoding_engine(),
            &bytes[line_start..target],
            &mut state,
        );
        state.position()
    }

    /// Encoding-aware mapping from a raw byte offset to a `TextPosition`
    /// for Class B documents (UTF-16LE / UTF-16BE and the multibyte CJK
    /// encodings).
    ///
    /// Walks the underlying byte storage through the document's encoding
    /// engine: line breaks are counted by repeatedly calling
    /// `engine.next_line_start` from the nearest known line anchor, and
    /// the column on the partial trailing line is the engine's
    /// `count_columns_exact` over the partial-line byte window. This
    /// produces a `TextPosition` whose `line0()` and `col0()` honour the
    /// document's encoding-aware text-unit semantics rather
    /// than treating the bytes as if they were UTF-8.
    ///
    /// Used only for mmap-backed Class B documents; piece-table-backed
    /// matches resolve through the piece table's engine-aware position
    /// scan instead, because the mmap snapshot and the open-time line
    /// offsets go stale on the first edit.
    pub(super) fn position_for_byte_offset_in_class_b(&self, byte_offset: usize) -> TextPosition {
        let bytes = self.mmap_bytes();
        let file_len = self.file_len.min(bytes.len());
        let target = byte_offset.min(file_len);
        if target == 0 {
            return TextPosition::new(0, 0);
        }

        let engine = self.encoding_engine();

        // Start from the nearest indexed line-start anchor at or before
        // `target` so we never have to scan from offset 0 on a multi-MiB
        // file. The anchor line0 is exact (built by the engine in
        // `from_storage_class_b_native`).
        let (mut line0, mut line_start) = if let Ok(offsets) = self.line_offsets.read() {
            line_offsets_anchor_for_byte(&offsets, target)
        } else {
            (0usize, 0usize)
        };
        line_start = line_start.min(target);

        // Walk forward line-by-line through the engine until the next
        // line-start would land past `target`. CRLF is collapsed into a
        // single line boundary by the engine, so this loop counts logical
        // lines, not raw `\n` bytes.
        loop {
            let next = engine.next_line_start(bytes, file_len, line_start);
            if next > target || next <= line_start {
                break;
            }
            line0 = line0.saturating_add(1);
            line_start = next;
        }

        // Column within the partial trailing line: the engine's exact
        // column counter over `[line_start..target]`. For UTF-16 a
        // supplementary surrogate pair counts as 1 column (4 bytes); a
        // misaligned byte at the very end of the slice is left untouched
        // because `count_columns_exact` walks via `step` which stops at
        // the last full code-unit cell.
        let col0 = engine.count_columns_exact(&bytes[line_start..target]);
        TextPosition::new(line0, col0)
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
        scan_position_bytes(
            self.encoding_engine(),
            &bytes[anchor_offset..target],
            &mut state,
        );
        state.position()
    }

    fn mmap_advance_offset_by_text_units(&self, start: usize, text_units: usize) -> usize {
        let bytes = self.mmap_bytes();
        let file_len = self.file_len.min(bytes.len());
        let start = start.min(file_len);
        if text_units == 0 || start >= file_len {
            return start;
        }

        let engine = self.encoding_engine();
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
                    // A malformed or truncated tail (notably an odd final byte
                    // in UTF-16 storage) must still advance. UTF-16 `step`
                    // returns 0 when fewer than two bytes remain, so using it
                    // unchecked here made huge open-ended ranges loop forever
                    // on the final byte.
                    let step = engine.step(bytes, i, file_len).max(1);
                    i = i.saturating_add(step).min(file_len);
                    offset = offset.saturating_add(step).min(file_len);
                }
            }
        }
        offset.min(file_len)
    }

    pub(super) fn search_byte_offset_for_position(&self, position: TextPosition) -> usize {
        let position = self.clamp_position(position);
        if let Some(rope) = &self.rope {
            let char_index = Self::line_col_to_char_index(rope, position.line0(), position.col0());
            return rope.char_to_byte(char_index);
        }
        if let Some(piece_table) = &self.piece_table {
            let offset = scanned_piece_table_byte_offset_for_position(piece_table, position)
                .unwrap_or_else(|| {
                    piece_table.byte_offset_for_col(position.line0(), position.col0())
                });
            return self.align_byte_offset(offset, AlignDirection::Backward);
        }
        self.mmap_byte_offset_for_position(position)
    }

    pub(super) fn buffered_literal_range(
        &self,
        start_offset: usize,
        end_offset: usize,
    ) -> Option<Vec<u8>> {
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

    /// Returns a zero-copy mmap byte slice for the requested range.
    ///
    /// Used by streaming regex search on clean file-backed documents to avoid
    /// the buffered-range copy and the `BUFFERED_LITERAL_RANGE_MAX_BYTES` cap.
    /// Returns `None` for rope or piece-table backings, where the bytes are
    /// not exposed as one contiguous mmap region.
    pub(super) fn mmap_search_slice(
        &self,
        start_offset: usize,
        end_offset: usize,
    ) -> Option<&[u8]> {
        if self.rope.is_some() || self.piece_table.is_some() {
            return None;
        }
        let bytes = self.mmap_bytes();
        let file_len = self.file_len.min(bytes.len());
        let start = start_offset.min(file_len);
        let end = end_offset.min(file_len).max(start);
        Some(&bytes[start..end])
    }

    /// Reads a piece-table byte range without the literal-search 8 MiB cap.
    ///
    /// Used by chunked streaming regex search on edited documents. Returns
    /// `None` for backings other than piece-table.
    pub(super) fn piece_table_uncapped_range(
        &self,
        start_offset: usize,
        end_offset: usize,
    ) -> Option<Vec<u8>> {
        let piece_table = self.piece_table.as_ref()?;
        if start_offset >= end_offset {
            return Some(Vec::new());
        }
        Some(piece_table.read_range(start_offset, end_offset))
    }

    fn find_prev_in_rope(
        &self,
        rope: &EditBuffer,
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
        let end_pos =
            advance_position_by_bytes(start_pos, needle, &super::encoding_engine::Utf8Engine);
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
        engine: &dyn super::encoding_engine::EncodingEngine,
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
            let step = engine.step(&bytes, i, bytes.len()).max(1);
            i = i.saturating_add(step).min(bytes.len());
        }
        (units <= anchor_position.col0())
            .then(|| TextPosition::new(anchor_position.line0(), anchor_position.col0() - units))
    }

    fn position_for_byte_offset(
        &self,
        byte_offset: usize,
        engine: &dyn super::encoding_engine::EncodingEngine,
    ) -> TextPosition {
        let target = byte_offset.min(self.total_len);
        if target == 0 {
            return TextPosition::new(0, 0);
        }

        let mut state = PositionScanState::default();
        self.pieces
            .visit_range(0, target, |piece, local_start, local_end| {
                let src = self.source_bytes(piece.src);
                let (seg_start, seg_end) =
                    self.clamped_piece_segment(piece, local_start, local_end, src.len());
                scan_position_bytes(engine, &src[seg_start..seg_end], &mut state);
            });
        state.position()
    }

    pub(crate) fn position_for_byte_offset_from(
        &self,
        anchor_offset: usize,
        anchor_position: TextPosition,
        byte_offset: usize,
        engine: &dyn super::encoding_engine::EncodingEngine,
    ) -> TextPosition {
        let anchor_offset = anchor_offset.min(self.total_len);
        let target = byte_offset.min(self.total_len);
        if target <= anchor_offset {
            return anchor_position;
        }

        let mut state = PositionScanState::new(anchor_position.line0(), anchor_position.col0());
        self.pieces
            .visit_range(anchor_offset, target, |piece, local_start, local_end| {
                let src = self.source_bytes(piece.src);
                let (seg_start, seg_end) =
                    self.clamped_piece_segment(piece, local_start, local_end, src.len());
                scan_position_bytes(engine, &src[seg_start..seg_end], &mut state);
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
                let src = self.source_bytes(piece.src);
                let (seg_start, seg_end) =
                    self.clamped_piece_segment(piece, local_start, local_end, src.len());
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
                        advance_position_by_bytes(overlap_position, &window[..rel], self.engine)
                    };
                    found = Some((match_position, overlap_start.saturating_add(rel)));
                    return false;
                }

                let keep = needle_len.saturating_sub(1).min(window.len());
                let consumed = window.len().saturating_sub(keep);
                if consumed > 0 {
                    overlap_position = advance_position_by_bytes(
                        overlap_position,
                        &window[..consumed],
                        self.engine,
                    );
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
                let src = self.source_bytes(piece.src);
                let (seg_start, seg_end) =
                    self.clamped_piece_segment(piece, local_start, local_end, src.len());
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
    rope: &EditBuffer,
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
    rope: &EditBuffer,
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
