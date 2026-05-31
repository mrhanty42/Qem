//! Typed regex search built on top of the same backing-aware machinery as
//! [`super::search`]. This is the first regex surface in Qem and is part of
//! the `0.8.0` release plan.
//!
//! The public types here mirror the literal-search story:
//!
//! - [`RegexSearchQuery`] is a reusable compiled regex query.
//! - [`RegexCompileError`] is the typed error returned when a pattern cannot
//!   be compiled.
//! - [`RegexSearchIter`] iterates non-overlapping matches forward, mirroring
//!   [`super::search::LiteralSearchIter`].
//!
//! Frontends keep the same [`SearchMatch`] shape across literal and regex
//! search.
//!
//! # Backing semantics
//!
//! - On clean mmap and piece-tree backings the regex runs over the stored
//!   bytes through [`regex::bytes::Regex`]. This preserves the literal-search
//!   contract: stored `\r\n` is matched as the actual two-byte sequence, and
//!   patterns can target raw byte sequences when needed.
//! - On rope-backed (edited) documents the regex runs over the in-memory
//!   `\n`-normalized text through [`regex::Regex`]. This matches the rope
//!   contract used by literal search on the same backing.
//!
//! # Reverse search
//!
//! `regex` 1.x has no native reverse-iteration API. Reverse helpers in this
//! module find the last match whose end is at or before the boundary by
//! forward-iterating up to that boundary and keeping the last result. The
//! semantics match [`super::search::LiteralSearchQuery`]: reverse is bounded
//! by the caller's `before` position, not by document end.

use std::fmt;
use std::sync::OnceLock;

use regex::bytes::Regex as ByteRegex;
use regex::Regex as TextRegex;
use regex_automata::dfa::dense;
use regex_automata::dfa::Automaton;
use regex_automata::nfa::thompson;
use regex_automata::{Input, MatchKind};
use ropey::Rope;

use super::alignment::AlignDirection;
use super::encoding_engine::SingleByteEngine;
use super::search::{advance_position_by_bytes, search_text_units};
use super::{Document, SearchMatch, TextPosition, TextRange};
use crate::DocumentEncoding;

/// Sliding-window chunk size for chunked regex scans on piece-tree backings.
///
/// Sized to keep peak per-call memory bounded while still amortizing the
/// per-chunk regex setup cost over many bytes. Mmap-backed documents bypass
/// this entirely and search the underlying mmap slice directly.
const REGEX_CHUNK_BYTES: usize = 8 * 1024 * 1024;

/// Maximum number of bytes by which two adjacent chunks are allowed to
/// overlap so a regex match that straddles a chunk boundary is still
/// observed by the next iteration.
///
/// 1 MiB is a deliberate hard cap. Patterns whose longest possible match is
/// longer than this against an arbitrary input (like unbounded `(a|b)*`)
/// would not be caught across the boundary, but those patterns are also not
/// realistic for incremental scrolling search and are documented as the
/// streaming-regex limit on huge files.
const REGEX_CHUNK_OVERLAP_BYTES: usize = 1024 * 1024;

/// Hard ceiling on the in-memory size of a compiled reverse DFA used by
/// the reverse-regex search path.
///
/// `regex_automata::dfa::dense::Config::dfa_size_limit` rejects compilation
/// with a typed `BuildError` once determinization tries to grow the DFA
/// past this limit. We surface that as [`RegexCompileError`] rather than
/// panic, so a pathological pattern (e.g. very wide bounded alternation)
/// stays a recoverable user-facing error.
const REVERSE_DFA_SIZE_LIMIT_BYTES: usize = 32 * 1024 * 1024;

/// Reusable compiled regex query used by [`Document::find_next_regex_query`]
/// and the related helpers.
///
/// Compilation is performed once at construction time for the byte-engine
/// form; the text-engine form is only compiled when the first rope-backed
/// search call needs it. Repeated searches with the same query reuse those
/// compiled engines instead of paying compilation cost per call.
///
/// The pattern is interpreted as Rust's `regex` crate syntax. Patterns may use
/// Unicode-aware classes by default; opt out per-class with the standard
/// `(?-u:...)` flag if you need to match raw bytes against a piece-tree or
/// mmap backing.
#[derive(Debug)]
pub struct RegexSearchQuery {
    pattern: String,
    bytes: ByteRegex,
    text: OnceLock<TextRegex>,
    /// Lazily compiled reverse DFA used by the reverse-regex search
    /// path. The cache stores a `Result` so
    /// both successful compilations and size-limit overflows are memoized
    /// — repeated calls neither rebuild the DFA nor re-run determinization
    /// after a hit on the size limit. This is the manual try-init pattern
    /// for `OnceLock` since `OnceLock::get_or_try_init` is not yet stable.
    reverse: OnceLock<Result<dense::DFA<Vec<u32>>, RegexCompileError>>,
}

impl Clone for RegexSearchQuery {
    fn clone(&self) -> Self {
        // Re-clone the eagerly compiled byte engine; the lazy text engine is
        // intentionally re-deferred so the clone cost stays predictable for
        // callers that only ever search byte-backed documents. The reverse
        // DFA cache is also reset so the clone does not pay determinization
        // cost up front for a forward-only consumer.
        Self {
            pattern: self.pattern.clone(),
            bytes: self.bytes.clone(),
            text: OnceLock::new(),
            reverse: OnceLock::new(),
        }
    }
}

impl RegexSearchQuery {
    /// Compiles a regex pattern into a reusable query.
    ///
    /// Returns [`RegexCompileError`] when the pattern is invalid or exceeds
    /// the default size limits enforced by the underlying `regex` crate.
    ///
    /// Only the byte-engine form is compiled here; the text-engine form is
    /// compiled lazily on first rope-backed search to keep construction cost
    /// predictable for callers that only ever search byte-backed documents.
    /// Pattern syntax is validated up front through the byte-engine
    /// compilation, so a query that compiles here is guaranteed to also
    /// compile its text-engine form when needed.
    pub fn new(pattern: impl Into<String>) -> Result<Self, RegexCompileError> {
        let pattern = pattern.into();
        if pattern.is_empty() {
            return Err(RegexCompileError {
                message: "regex pattern must not be empty".to_owned(),
            });
        }
        let bytes = ByteRegex::new(&pattern).map_err(RegexCompileError::from_regex)?;
        Ok(Self {
            pattern,
            bytes,
            text: OnceLock::new(),
            reverse: OnceLock::new(),
        })
    }

    /// Returns the source pattern string.
    pub fn pattern(&self) -> &str {
        &self.pattern
    }

    pub(super) fn bytes_regex(&self) -> &ByteRegex {
        &self.bytes
    }

    pub(super) fn text_regex(&self) -> &TextRegex {
        // The byte-engine form already validated pattern syntax in `new()`,
        // so this `expect` is a true invariant: any pattern that survived
        // construction must also be a valid text regex. We deliberately do
        // not return a `Result` from this accessor because callers reach
        // it through search paths that are documented to never fail at
        // dispatch time; a syntax-valid pattern that fails to compile in
        // `regex::Regex::new` would be a `regex` crate bug, not user input.
        self.text.get_or_init(|| {
            TextRegex::new(&self.pattern)
                .expect("byte-engine compilation already validated pattern syntax")
        })
    }

    /// Returns the lazily compiled reverse DFA used by the reverse-regex
    /// search path.
    ///
    /// Compilation runs at most once per `RegexSearchQuery` instance: on
    /// success the returned `&dense::DFA<Vec<u32>>` references the cached
    /// build; on failure the typed [`RegexCompileError`] is also cached so
    /// repeated calls do not re-pay determinization cost on a pattern that
    /// already exceeded the size limit.
    ///
    /// The DFA is configured with `MatchKind::LeftmostFirst` (the
    /// standard `regex` 1.x semantic) and a 32 MiB size limit on both
    /// the DFA itself and the determinization scratch space. With this
    /// configuration `try_search_rev` returns the rightmost
    /// leftmost-first match in the input — exactly the contract
    /// `find_prev_regex_*` needs. Patterns that would push the DFA past
    /// those limits return a typed error rather than panicking.
    ///
    /// **Routing contract:** forward search paths must not call
    /// this accessor. The reverse DFA is materialised only when a
    /// `find_prev_*` entry point delegates here, so an editor that never
    /// invokes a reverse search never pays the determinization cost.
    pub(crate) fn ensure_reverse(&self) -> Result<&dense::DFA<Vec<u32>>, RegexCompileError> {
        let cached = self
            .reverse
            .get_or_init(|| build_reverse_dfa(&self.pattern));
        match cached {
            Ok(dfa) => Ok(dfa),
            Err(err) => Err(err.clone()),
        }
    }
}

/// Build a reverse DFA for `pattern` under the project-wide size limit.
///
/// Translates `regex_automata::dfa::dense::BuildError` into the typed
/// [`RegexCompileError`] used by the rest of the regex search surface.
/// Both real size-limit overflows and unsupported features (e.g. Unicode
/// word boundaries on a DFA) come through as `Err` here — callers do not
/// need to distinguish: the `find_prev_*` paths simply surface the typed
/// error to the editor without panicking.
fn build_reverse_dfa(pattern: &str) -> Result<dense::DFA<Vec<u32>>, RegexCompileError> {
    build_reverse_dfa_with_limit(pattern, REVERSE_DFA_SIZE_LIMIT_BYTES)
}

/// Test-only escape hatch: build a reverse DFA with a caller-
/// chosen `dfa_size_limit` / `determinize_size_limit`.
///
/// Production code paths must keep going through
/// [`RegexSearchQuery::ensure_reverse`] (which routes through
/// [`build_reverse_dfa`] with the project-wide
/// [`REVERSE_DFA_SIZE_LIMIT_BYTES`]). This helper exists so unit tests can
/// reliably trip the typed-error path on a tiny, deterministic limit
/// instead of having to construct a pattern that overflows the production
/// 32 MiB ceiling under `MatchKind::LeftmostFirst` — that ceiling is
/// generous enough that any pattern wide enough to bust it is also expensive
/// to validate across `regex_automata` versions.
///
/// Configuration mirrors [`build_reverse_dfa`] exactly except for the size
/// limit, so a successful build here is structurally identical to the
/// production reverse DFA modulo capacity.
pub(crate) fn build_reverse_dfa_with_limit(
    pattern: &str,
    limit_bytes: usize,
) -> Result<dense::DFA<Vec<u32>>, RegexCompileError> {
    let config = dense::Config::new()
        // `MatchKind::LeftmostFirst` is the standard regex semantic
        // (also the default of `regex 1.x`). For a reverse-thompson DFA
        // run via `try_search_rev`, this is what makes the helper
        // return the **rightmost** leftmost-first match in the input —
        // exactly the "last match before `before`" we need for
        // `find_prev_regex_*`. With `MatchKind::All` (which we initially
        // tried) the same call returns the leftmost match start
        // because `All` semantics treat every reachable accept state
        // as a candidate; that surfaced as `find_prev_regex` returning
        // the first match instead of the last (regression in the
        // reverse-regex test trio).
        .match_kind(MatchKind::LeftmostFirst)
        .dfa_size_limit(Some(limit_bytes))
        .determinize_size_limit(Some(limit_bytes));
    dense::Builder::new()
        .configure(config)
        .thompson(thompson::Config::new().reverse(true))
        .build(pattern)
        .map_err(RegexCompileError::from_dense_build)
}

/// Typed error returned when a regex pattern cannot be compiled.
///
/// The wrapped `message` is the human-readable diagnostic produced by the
/// `regex` crate. The exact text is not part of the public contract beyond
/// "non-empty, ASCII-friendly explanation suitable for surfacing in a UI".
#[derive(Clone, Debug)]
pub struct RegexCompileError {
    message: String,
}

impl RegexCompileError {
    fn from_regex(error: regex::Error) -> Self {
        Self {
            message: error.to_string(),
        }
    }

    /// Wraps a `regex_automata::dfa::dense::BuildError` from the reverse
    /// DFA compile path into a typed compile error.
    ///
    /// The `BuildError::Display` impl already produces a non-empty,
    /// human-readable diagnostic for both real size-limit overflows and
    /// unsupported features (e.g. Unicode word boundaries on a DFA), so
    /// reusing it keeps the surfaced message stable across the two
    /// underlying engines. The contract is just "non-empty message,
    /// no panic, no unwrap"; we add a deterministic prefix so callers can
    /// route on the source engine if they ever want to.
    fn from_dense_build(error: dense::BuildError) -> Self {
        Self {
            message: format!("reverse DFA compilation failed: {error}"),
        }
    }

    /// Returns the diagnostic message describing why compilation failed.
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for RegexCompileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for RegexCompileError {}

/// Iterator over non-overlapping regex matches in a document.
///
/// The iterator owns its compiled query and advances from the end of each
/// match, mirroring [`super::search::LiteralSearchIter`]. Empty matches still
/// advance by at least one document text unit so the iterator cannot loop
/// indefinitely on patterns like `(a*)`.
#[derive(Debug)]
pub struct RegexSearchIter<'a> {
    doc: &'a Document,
    query: RegexSearchQuery,
    next_from: TextPosition,
    end: Option<TextPosition>,
    finished: bool,
}

impl<'a> RegexSearchIter<'a> {
    fn new(
        doc: &'a Document,
        query: RegexSearchQuery,
        next_from: TextPosition,
        end: Option<TextPosition>,
    ) -> Self {
        let next_from = doc.clamp_position(next_from);
        let end = end.map(|position| doc.clamp_position(position));
        Self {
            doc,
            query,
            next_from,
            end,
            finished: false,
        }
    }
}

impl Iterator for RegexSearchIter<'_> {
    type Item = SearchMatch;

    fn next(&mut self) -> Option<Self::Item> {
        if self.finished {
            return None;
        }

        let from = self.next_from;
        let found = match self.end {
            Some(end) => {
                if from >= end {
                    self.finished = true;
                    return None;
                }
                self.doc.find_next_regex_bounded(&self.query, from, end)
            }
            None => self.doc.find_next_regex_query(&self.query, from),
        };

        let Some(found) = found else {
            self.finished = true;
            return None;
        };

        let advanced = if found.end() <= from {
            // Empty or zero-width match at the same position: nudge forward
            // by one text unit so the next iteration cannot return the same
            // match again.
            advance_one_text_unit(self.doc, from)
        } else {
            found.end()
        };
        self.next_from = advanced;
        if let Some(end) = self.end {
            if self.next_from >= end {
                self.finished = true;
            }
        }
        Some(found)
    }
}

impl std::iter::FusedIterator for RegexSearchIter<'_> {}

fn advance_one_text_unit(doc: &Document, position: TextPosition) -> TextPosition {
    let line_len = doc.line_len_chars(position.line0());
    if position.col0() < line_len {
        return TextPosition::new(position.line0(), position.col0().saturating_add(1));
    }
    TextPosition::new(position.line0().saturating_add(1), 0)
}

impl Document {
    /// Finds the next regex match starting at `from`.
    ///
    /// One-shot helper that compiles the pattern on each call. For repeated
    /// searches against the same pattern, prefer [`RegexSearchQuery`] and
    /// [`Document::find_next_regex_query`].
    pub fn find_next_regex(
        &self,
        pattern: &str,
        from: TextPosition,
    ) -> Result<Option<SearchMatch>, RegexCompileError> {
        let query = RegexSearchQuery::new(pattern)?;
        Ok(self.find_next_regex_query(&query, from))
    }

    /// Finds the previous regex match whose end is at or before `before`.
    ///
    /// One-shot helper that compiles the pattern on each call.
    pub fn find_prev_regex(
        &self,
        pattern: &str,
        before: TextPosition,
    ) -> Result<Option<SearchMatch>, RegexCompileError> {
        let query = RegexSearchQuery::new(pattern)?;
        Ok(self.find_prev_regex_query(&query, before))
    }

    /// Finds the next regex match for a reusable compiled query.
    pub fn find_next_regex_query(
        &self,
        query: &RegexSearchQuery,
        from: TextPosition,
    ) -> Option<SearchMatch> {
        let from = self.clamp_position(from);
        if let Some(rope) = &self.rope {
            return find_next_regex_in_rope(self, rope, query, from);
        }
        find_next_regex_in_bytes(self, query, from, None)
    }

    /// Finds the previous regex match for a reusable compiled query.
    ///
    /// Returns the last non-overlapping match whose end is at or before
    /// `before`. This matches the literal-search semantics of
    /// [`Document::find_prev_query`].
    ///
    /// Routing: dispatched to the reverse-DFA
    /// backend through [`find_prev_regex_via_reverse_dfa`]. The choice of
    /// backend (rope, mmap slice, piece-tree chunked) is made by the
    /// dispatcher, not by an in-band `direction` field on the query.
    pub fn find_prev_regex_query(
        &self,
        query: &RegexSearchQuery,
        before: TextPosition,
    ) -> Option<SearchMatch> {
        let before = self.clamp_position(before);
        if before == TextPosition::new(0, 0) {
            return None;
        }
        find_prev_regex_via_reverse_dfa(self, query, TextPosition::new(0, 0), before)
    }

    /// Finds the first regex match fully contained within `range`.
    pub fn find_next_regex_in_range(
        &self,
        pattern: &str,
        range: TextRange,
    ) -> Result<Option<SearchMatch>, RegexCompileError> {
        let query = RegexSearchQuery::new(pattern)?;
        Ok(self.find_next_regex_query_in_range(&query, range))
    }

    /// Finds the first regex match fully contained between two typed positions.
    pub fn find_next_regex_between(
        &self,
        pattern: &str,
        start: TextPosition,
        end: TextPosition,
    ) -> Result<Option<SearchMatch>, RegexCompileError> {
        let query = RegexSearchQuery::new(pattern)?;
        Ok(self.find_next_regex_query_between(&query, start, end))
    }

    /// Finds the first compiled-query regex match fully contained within `range`.
    pub fn find_next_regex_query_in_range(
        &self,
        query: &RegexSearchQuery,
        range: TextRange,
    ) -> Option<SearchMatch> {
        let (start, end) = self.search_range_bounds_public(range);
        self.find_next_regex_bounded(query, start, end)
    }

    /// Finds the first compiled-query regex match fully contained between two positions.
    pub fn find_next_regex_query_between(
        &self,
        query: &RegexSearchQuery,
        start: TextPosition,
        end: TextPosition,
    ) -> Option<SearchMatch> {
        let (start, end) = self.ordered_positions(start, end);
        self.find_next_regex_bounded(query, start, end)
    }

    /// Finds the last compiled-query regex match fully contained within `range`.
    pub fn find_prev_regex_query_in_range(
        &self,
        query: &RegexSearchQuery,
        range: TextRange,
    ) -> Option<SearchMatch> {
        let (start, end) = self.search_range_bounds_public(range);
        self.find_prev_regex_bounded(query, start, end)
    }

    /// Finds the last compiled-query regex match fully contained between two positions.
    pub fn find_prev_regex_query_between(
        &self,
        query: &RegexSearchQuery,
        start: TextPosition,
        end: TextPosition,
    ) -> Option<SearchMatch> {
        let (start, end) = self.ordered_positions(start, end);
        self.find_prev_regex_bounded(query, start, end)
    }

    /// Iterates non-overlapping regex matches over the whole document.
    pub fn find_all_regex(&self, pattern: &str) -> Result<RegexSearchIter<'_>, RegexCompileError> {
        let query = RegexSearchQuery::new(pattern)?;
        Ok(self.find_all_regex_query(&query))
    }

    /// Iterates non-overlapping regex matches from `from` onward.
    pub fn find_all_regex_from(
        &self,
        pattern: &str,
        from: TextPosition,
    ) -> Result<RegexSearchIter<'_>, RegexCompileError> {
        let query = RegexSearchQuery::new(pattern)?;
        Ok(self.find_all_regex_query_from(&query, from))
    }

    /// Iterates non-overlapping regex matches over the whole document using a
    /// reusable compiled query.
    pub fn find_all_regex_query(&self, query: &RegexSearchQuery) -> RegexSearchIter<'_> {
        self.find_all_regex_query_from(query, TextPosition::new(0, 0))
    }

    /// Iterates non-overlapping regex matches from `from` onward using a
    /// reusable compiled query.
    pub fn find_all_regex_query_from(
        &self,
        query: &RegexSearchQuery,
        from: TextPosition,
    ) -> RegexSearchIter<'_> {
        RegexSearchIter::new(self, query.clone(), from, None)
    }

    /// Iterates non-overlapping regex matches fully contained within `range`.
    pub fn find_all_regex_in_range(
        &self,
        pattern: &str,
        range: TextRange,
    ) -> Result<RegexSearchIter<'_>, RegexCompileError> {
        let query = RegexSearchQuery::new(pattern)?;
        Ok(self.find_all_regex_query_in_range(&query, range))
    }

    /// Iterates non-overlapping regex matches between two typed positions.
    pub fn find_all_regex_between(
        &self,
        pattern: &str,
        start: TextPosition,
        end: TextPosition,
    ) -> Result<RegexSearchIter<'_>, RegexCompileError> {
        let query = RegexSearchQuery::new(pattern)?;
        Ok(self.find_all_regex_query_between(&query, start, end))
    }

    /// Iterates non-overlapping regex matches in `range` using a compiled query.
    pub fn find_all_regex_query_in_range(
        &self,
        query: &RegexSearchQuery,
        range: TextRange,
    ) -> RegexSearchIter<'_> {
        let (start, end) = self.search_range_bounds_public(range);
        RegexSearchIter::new(self, query.clone(), start, Some(end))
    }

    /// Iterates non-overlapping regex matches between two positions using a
    /// compiled query.
    pub fn find_all_regex_query_between(
        &self,
        query: &RegexSearchQuery,
        start: TextPosition,
        end: TextPosition,
    ) -> RegexSearchIter<'_> {
        let (start, end) = self.ordered_positions(start, end);
        RegexSearchIter::new(self, query.clone(), start, Some(end))
    }

    pub(super) fn find_next_regex_bounded(
        &self,
        query: &RegexSearchQuery,
        start: TextPosition,
        end: TextPosition,
    ) -> Option<SearchMatch> {
        if start >= end {
            return None;
        }
        if let Some(rope) = &self.rope {
            return find_next_regex_in_rope_bounded(self, rope, query, start, end);
        }
        find_next_regex_in_bytes(self, query, start, Some(end))
    }

    pub(super) fn find_prev_regex_bounded(
        &self,
        query: &RegexSearchQuery,
        start: TextPosition,
        end: TextPosition,
    ) -> Option<SearchMatch> {
        if start >= end {
            return None;
        }
        find_prev_regex_via_reverse_dfa(self, query, start, end)
    }

    fn search_range_bounds_public(&self, range: TextRange) -> (TextPosition, TextPosition) {
        let start = self.clamp_position(range.start());
        if range.is_empty() {
            return (start, start);
        }
        let end_offset = self
            .char_index_for_position(start)
            .saturating_add(range.len_chars());
        let end = self.position_for_char_index(end_offset);
        (start, end)
    }
}

fn find_next_regex_in_rope(
    doc: &Document,
    rope: &Rope,
    query: &RegexSearchQuery,
    from: TextPosition,
) -> Option<SearchMatch> {
    let start_char = doc.char_index_for_position(from);
    let start_byte = rope.char_to_byte(start_char);
    if start_byte > rope.len_bytes() {
        return None;
    }
    find_next_regex_in_rope_chunks(doc, rope, query, from, start_byte, rope.len_bytes())
}

fn find_next_regex_in_rope_bounded(
    doc: &Document,
    rope: &Rope,
    query: &RegexSearchQuery,
    start: TextPosition,
    end: TextPosition,
) -> Option<SearchMatch> {
    let start_char = doc.char_index_for_position(start);
    let end_char = doc.char_index_for_position(end).max(start_char);
    if start_char >= rope.len_chars() {
        return None;
    }
    let start_byte = rope.char_to_byte(start_char);
    let end_byte = rope.char_to_byte(end_char.min(rope.len_chars()));
    let m = find_next_regex_in_rope_chunks(doc, rope, query, start, start_byte, end_byte)?;
    if m.end() > end {
        return None;
    }
    Some(m)
}

/// Chunked regex walk over a rope between `start_byte..end_byte`.
///
/// Mirrors the structure of the literal-search rope walker but uses the
/// regex engine instead of a fixed-length finder. The window is refilled
/// chunk by chunk with a `REGEX_CHUNK_OVERLAP_BYTES` carry-over so a match
/// straddling a rope chunk boundary is still observed. Patterns whose
/// longest match against arbitrary input is longer than the overlap may
/// miss boundary-straddling matches; this is the same documented limit as
/// for piece-table chunked regex search.
fn find_next_regex_in_rope_chunks(
    doc: &Document,
    rope: &Rope,
    query: &RegexSearchQuery,
    from: TextPosition,
    start_byte: usize,
    end_byte: usize,
) -> Option<SearchMatch> {
    if start_byte >= end_byte {
        return None;
    }

    // A rope's chunks are guaranteed valid UTF-8 individually but the regex
    // window crosses chunk boundaries, so we keep the carry-over as a
    // String. The carry-over is bounded by REGEX_CHUNK_OVERLAP_BYTES.
    let mut window = String::new();
    let mut window_start_byte = start_byte;
    let mut chunk_base = 0usize;
    let target_chunk = REGEX_CHUNK_BYTES;

    for chunk in rope.chunks() {
        let chunk_bytes = chunk.as_bytes();
        let chunk_end = chunk_base.saturating_add(chunk_bytes.len());

        // Skip chunks that lie entirely before the search window.
        if chunk_end <= start_byte {
            chunk_base = chunk_end;
            continue;
        }
        // Stop once we've consumed past the end bound.
        if chunk_base >= end_byte {
            break;
        }

        // The slice of this chunk that intersects our search window.
        let chunk_window_start = start_byte.saturating_sub(chunk_base);
        let chunk_window_end = end_byte.saturating_sub(chunk_base).min(chunk_bytes.len());
        if chunk_window_end <= chunk_window_start {
            chunk_base = chunk_end;
            continue;
        }
        // Safe: ropey chunks are valid UTF-8 and slices are aligned on
        // char boundaries because we entered this chunk through char-aware
        // start_byte / end_byte.
        let segment = &chunk[chunk_window_start..chunk_window_end];
        window.push_str(segment);

        if window.len() >= target_chunk || chunk_end >= end_byte {
            if let Some(m) = query.text_regex().find(&window) {
                return finalize_rope_window_match(doc, from, &window, m.start(), m.end());
            }
            // Trim the window down to the carry-over overlap before the
            // next chunk so memory stays bounded.
            if window.len() > REGEX_CHUNK_OVERLAP_BYTES {
                let trim_at = window.len().saturating_sub(REGEX_CHUNK_OVERLAP_BYTES);
                let trim_at = utf8_floor_boundary(&window, trim_at);
                let kept_bytes = window.len().saturating_sub(trim_at);
                window_start_byte = window_start_byte
                    .saturating_add(window.len())
                    .saturating_sub(kept_bytes);
                let kept = window.split_off(trim_at);
                window = kept;
            }
        }

        chunk_base = chunk_end;
    }

    // Final tail check (in case we never crossed the chunk threshold).
    if !window.is_empty() {
        if let Some(m) = query.text_regex().find(&window) {
            return finalize_rope_window_match(doc, from, &window, m.start(), m.end());
        }
    }

    let _ = window_start_byte;
    None
}

fn finalize_rope_window_match(
    doc: &Document,
    from: TextPosition,
    window: &str,
    match_start: usize,
    match_end: usize,
) -> Option<SearchMatch> {
    let prefix_units = window[..match_start].chars().count();
    let match_units = window[match_start..match_end].chars().count();
    let from_char = doc.char_index_for_position(from);
    let match_start_char = from_char.saturating_add(prefix_units);
    let start_pos = doc.position_for_char_index(match_start_char);
    let end_pos = doc.position_for_char_index(match_start_char.saturating_add(match_units));
    Some(SearchMatch::new(
        TextRange::new(start_pos, match_units),
        end_pos,
    ))
}

/// Returns the largest UTF-8 character boundary at or before `byte`.
fn utf8_floor_boundary(text: &str, byte: usize) -> usize {
    let byte = byte.min(text.len());
    let mut i = byte;
    while i > 0 && !text.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn find_next_regex_in_bytes(
    doc: &Document,
    query: &RegexSearchQuery,
    from: TextPosition,
    end: Option<TextPosition>,
) -> Option<SearchMatch> {
    let start_offset = doc.search_byte_offset_for_position(from);
    let end_offset = match end {
        Some(end_pos) => doc.search_byte_offset_for_position(end_pos),
        None => doc.file_len(),
    };
    if start_offset >= end_offset {
        return None;
    }

    // Class B routing: for UTF-16 and the CJK
    // multibyte engines (Shift_JIS, gb18030, EUC-KR), we cannot run
    // the byte-engine regex directly against the raw bytes because:
    //   - patterns like `(?i)`, `\d`, `\w` are Unicode-class oriented and
    //     would silently miss every non-ASCII match if applied to UTF-16
    //     or CJK multibyte bytes;
    //   - ASCII-only patterns would still mis-anchor on `0x00` filler
    //     bytes between UTF-16 code units, or stray-match a `0x0A` /
    //     `0x0D` byte that is actually the trailing byte of a CJK
    //     multibyte sequence.
    // Instead we decode windows of the underlying bytes through
    // `encoding_rs` and run the text engine against the decoded `&str`,
    // mapping match offsets back to the original byte storage with an
    // encoding-dispatched source-byte mapping. UTF-16 keeps the
    // 2-byte alignment post-filter; CJK multibyte uses
    // re-encoding through `encoding_rs::Encoding::encode` for the
    // mapping and skips the alignment post-filter.
    // UTF-8 and Class A keep the existing byte-engine fast path below.
    let encoding = doc.encoding();
    if !encoding.is_utf8() && !SingleByteEngine::supports(encoding) {
        return find_next_regex_in_class_b_chunked(doc, query, start_offset, end_offset).and_then(
            |(match_start, match_end, match_bytes)| {
                build_class_b_search_match(doc, match_start, match_end, &match_bytes, end)
            },
        );
    }

    // Mmap backing: zero-copy. The regex engine walks the mmap slice
    // directly. There is no per-call allocation and no buffered-range cap,
    // so this stays interactive on multi-gigabyte files.
    if let Some(slice) = doc.mmap_search_slice(start_offset, end_offset) {
        let m = query.bytes_regex().find(slice)?;
        return finalize_byte_match(from, &slice[..m.start()], &slice[m.start()..m.end()], end);
    }

    // Piece-table backing: chunked streaming with overlap. We iterate
    // `[start_offset, end_offset)` in REGEX_CHUNK_BYTES windows, keeping a
    // REGEX_CHUNK_OVERLAP_BYTES tail between adjacent chunks so a match
    // straddling a chunk boundary is still observed. To keep matches
    // non-duplicated across chunks, we only accept a hit on chunk N+1 if
    // its start lies inside the new (non-overlap) part of the chunk, or if
    // it is the first chunk we look at.
    let mut chunk_start = start_offset;
    let mut first_chunk = true;
    while chunk_start < end_offset {
        let chunk_end = chunk_start
            .saturating_add(REGEX_CHUNK_BYTES)
            .min(end_offset);
        let chunk = doc.piece_table_uncapped_range(chunk_start, chunk_end)?;

        // For chunks past the first one, the leading
        // REGEX_CHUNK_OVERLAP_BYTES bytes were already part of the
        // previous chunk's search window. A match that begins inside
        // that overlap was either matched on the previous chunk or
        // crosses the boundary; in both cases we keep that overlap-
        // crossing match here so it gets returned.
        let overlap_already_matched = if first_chunk {
            0
        } else {
            REGEX_CHUNK_OVERLAP_BYTES.min(chunk.len())
        };

        if let Some(m) = query.bytes_regex().find(&chunk) {
            // Skip matches that lie entirely inside the previous-chunk
            // overlap on continuation chunks: those would have been
            // surfaced as a match on the previous chunk already, or as
            // an overlap-straddling match earlier.
            let chunk_match_end = m.end();
            if first_chunk || chunk_match_end > overlap_already_matched {
                let from_for_chunk = if first_chunk {
                    from
                } else {
                    let prefix_into_chunk = &chunk[..0];
                    advance_position_by_bytes(from, prefix_into_chunk)
                };
                let absolute_match_start = chunk_start.saturating_add(m.start());
                let absolute_match_end = chunk_start.saturating_add(m.end());
                let prefix_bytes =
                    doc.piece_table_uncapped_range(start_offset, absolute_match_start)?;
                let match_bytes =
                    doc.piece_table_uncapped_range(absolute_match_start, absolute_match_end)?;
                return finalize_byte_match(from_for_chunk, &prefix_bytes, &match_bytes, end);
            }
        }

        if chunk_end >= end_offset {
            break;
        }
        // Slide the window forward, keeping the overlap.
        chunk_start = chunk_end.saturating_sub(REGEX_CHUNK_OVERLAP_BYTES);
        first_chunk = false;
    }

    None
}

fn finalize_byte_match(
    from: TextPosition,
    prefix_bytes: &[u8],
    match_bytes: &[u8],
    end: Option<TextPosition>,
) -> Option<SearchMatch> {
    let start_pos = advance_position_by_bytes(from, prefix_bytes);
    let match_units = search_text_units(std::str::from_utf8(match_bytes).unwrap_or(""));
    let end_pos = advance_position_by_bytes(start_pos, match_bytes);
    if let Some(end_bound) = end {
        if end_pos > end_bound {
            return None;
        }
    }
    Some(SearchMatch::new(
        TextRange::new(start_pos, match_units),
        end_pos,
    ))
}

/// Dispatcher for the reverse-regex routing.
///
/// Picks the right reverse-DFA backend based on the document's storage:
///
/// * Rope-backed (edited) documents go through
///   [`reverse_dfa_search_in_rope`] so the backward chunk walk uses the
///   same UTF-8 rope chunks the forward path relies on.
/// * Byte-backed documents (clean mmap and piece-tree) prefer the
///   contiguous mmap slice when [`Document::mmap_search_slice`] returns
///   `Some(..)` — that path is O(slice) and avoids the chunked
///   piece-tree fallback. Otherwise we route through
///   [`reverse_dfa_search_in_piece_tree`] for the chunked windowed walk.
///
/// Routing is decided strictly by the entry function name: there
/// is no `direction` field on `RegexSearchQuery`. Forward search paths
/// never reach this dispatcher.
///
/// Compile-error semantics: the per-backend helpers each call
/// `query.ensure_reverse().ok()?` internally, so a pattern that
/// overflows the reverse-DFA size limit short-circuits to `None` from
/// the caller's perspective. The typed [`RegexCompileError`] path is
/// reachable directly through [`RegexSearchQuery::ensure_reverse`] for
/// callers that need the error explicitly. Pinning that error path on
/// every `find_prev_*` call would require widening the public surface
/// to return a `Result`, which is out of scope here; for now the
/// documented behaviour is "pathological pattern returns no match".
fn find_prev_regex_via_reverse_dfa(
    doc: &Document,
    query: &RegexSearchQuery,
    bound_start: TextPosition,
    bound_end: TextPosition,
) -> Option<SearchMatch> {
    if let Some(rope) = &doc.rope {
        return reverse_dfa_search_in_rope(doc, rope, query, bound_start, bound_end);
    }
    let start_off = doc.search_byte_offset_for_position(bound_start);
    let end_off = doc.search_byte_offset_for_position(bound_end);
    if start_off >= end_off {
        return None;
    }
    if let Some(slice) = doc.mmap_search_slice(start_off, end_off) {
        return reverse_dfa_search_in_slice(doc, query, bound_start, start_off, slice);
    }
    reverse_dfa_search_in_piece_tree(doc, query, bound_start, bound_end)
}

fn collect_rope_chunks_with_offsets(rope: &Rope) -> Vec<(usize, &str)> {
    let mut out = Vec::new();
    let mut offset = 0usize;
    for chunk in rope.chunks() {
        out.push((offset, chunk));
        offset = offset.saturating_add(chunk.len());
    }
    out
}

fn utf8_floor_boundary_bytes(bytes: &[u8], byte: usize) -> usize {
    let byte = byte.min(bytes.len());
    let mut i = byte;
    while i > 0 && (bytes[i] & 0b1100_0000) == 0b1000_0000 {
        i -= 1;
    }
    i
}

// ---------------------------------------------------------------------------
// Reverse DFA search backends
//
// `regex_automata`'s reverse-thompson DFA finds the leftmost start of the
// rightmost match in a byte slice. The two-pass dance documented in the
// `regex_automata::dfa::dense` rustdoc combines that with a forward scan to
// recover the match end. We reuse the forward `regex::bytes::Regex` engine
// already compiled in `RegexSearchQuery` for the forward leg — its match
// boundaries are guaranteed to agree with the reverse DFA because both
// engines are built from the same pattern string and the same default
// Unicode-aware syntax. This avoids paying determinization cost a second
// time for a forward `dense::DFA<Vec<u32>>` companion (which would also
// have to surface its own size-limit overflow as a typed error); the
// existing `bytes_regex` engine has no such limit and is already
// eagerly compiled at `RegexSearchQuery::new` time.
//
// The helpers take `query: &RegexSearchQuery` rather than the literal
// `dfa: &dense::DFA<Vec<u32>>` from the task description because the
// forward leg needs the same query's `bytes_regex()` engine to recover
// the match end after `try_search_rev` reports the start. The reverse
// DFA is reached through `query.ensure_reverse()` inside each helper,
// so the typed compile error from a pathological pattern is propagated
// up to the caller without panicking. This is a private routing surface
// only consumed by `find_prev_*`.
//
// All three backends post-filter the recovered absolute byte offsets
// through [`Document::align_byte_offset`] (Property 14). For
// UTF-8 / Class A backings the alignment is a no-op (every regex match
// already lands on a character boundary); for UTF-16 it rounds toward
// the matched span (start backward, end forward) so a pattern that
// matched on a `U+FFFD` injected by ill-formed input is rejected via the
// same 2-byte alignment grid the forward path already enforces. Class B
// alignment is preserved through the same scan-from-anchor walk used by
// edits.
// ---------------------------------------------------------------------------

/// Returns the leftmost-start-of-rightmost-match `(start, end)` byte
/// offsets relative to `slice` for `dfa` + `forward_regex`, or `None` when
/// no match exists in `slice`.
///
/// `forward_regex` is the same pattern compiled as `regex::bytes::Regex`;
/// it recovers the match end after the reverse DFA has located the
/// match start. The forward regex's leftmost-first match starting at or
/// after `match_start_in_slice` is taken as the canonical match end.
fn reverse_dfa_locate_in_slice(
    dfa: &dense::DFA<Vec<u32>>,
    forward_regex: &ByteRegex,
    slice: &[u8],
) -> Option<(usize, usize)> {
    if slice.is_empty() {
        return None;
    }
    let input = Input::new(slice);
    // `try_search_rev` on a reverse-thompson DFA built with
    // `MatchKind::LeftmostFirst` returns the **rightmost**
    // leftmost-first match's start offset (inclusive). The probe
    // `examples/probe_reverse_dfa.rs` confirmed: for `\d+` against
    // `"alpha 12 bravo 345 charlie\n"` this returns offset 15 (start
    // of `345`), which is what `find_prev_regex_*` needs. Errors here
    // are typed `MatchError`s — for our DFA configuration (no quit
    // bytes, default anchor mode) the only documented error path is
    // unsupported anchor configuration, which we never request. We
    // map errors to `None` rather than panicking so a defensive
    // caller in production sees a miss rather than a crash.
    let half = match dfa.try_search_rev(&input) {
        Ok(Some(hm)) => hm,
        _ => return None,
    };
    let match_start = half.offset();
    // Recover the match end via a forward leftmost-first scan starting
    // exactly at `match_start`. The forward regex was compiled from the
    // same pattern string as the reverse DFA, so a match must exist
    // here; if for any reason the forward engine disagrees (e.g. a
    // pathological future regex/regex_automata version skew) we surface
    // a miss rather than a wrong span.
    let m = forward_regex.find_at(slice, match_start)?;
    if m.start() != match_start {
        return None;
    }
    Some((m.start(), m.end()))
}

/// Reverse-DFA regex search over a contiguous mmap byte slice.
///
/// `slice` is the zero-copy mmap range corresponding to absolute byte
/// offsets `[slice_start_off, slice_start_off + slice.len())` in the
/// document. There is no 8 MiB cap on the slice — the reverse DFA walks
/// each byte exactly once, so the cost is linear in the slice length
/// rather than in the number of matches the forward chunked path would
/// have produced.
///
/// `bound_start` is the typed start of the search range. It is used as
/// the base for converting the recovered absolute byte offsets into
/// `TextPosition`s through `advance_position_by_bytes`, mirroring the
/// forward-search pattern in `find_next_regex_in_bytes`.
///
/// Post-filter through [`Document::align_byte_offset`] enforces:
/// the absolute match start is rounded backward to the nearest
/// character boundary of the document's encoding, and the absolute
/// match end is rounded forward. For UTF-8 / Class A the alignment is
/// a no-op; for UTF-16 it collapses 2-byte cells; for Class B it walks
/// the engine's `step` from the nearest line anchor. A match that fails
/// alignment (i.e. one whose endpoints already lay on character
/// boundaries) is returned unchanged; alignment can only nudge the
/// reported boundaries when the regex matched against bytes the
/// document's encoding contract would treat as boundary-corrupted, in
/// which case nudging to the nearest valid boundary is the documented
/// recovery path (Property 14).
///
/// **Signature note:** the public reverse-DFA backend takes a
/// `dfa: &dense::DFA<Vec<u32>>` parameter, but the forward leg of the
/// reverse-then-forward dance also needs the same query's compiled
/// `regex::bytes::Regex` engine. Threading the `&RegexSearchQuery`
/// directly here lets the helper reach both engines through one
/// reference and keeps the typed compile-error path from
/// `ensure_reverse` reachable without an out-of-band side channel.
fn reverse_dfa_search_in_slice(
    doc: &Document,
    query: &RegexSearchQuery,
    bound_start: TextPosition,
    slice_start_off: usize,
    slice: &[u8],
) -> Option<SearchMatch> {
    let dfa = query.ensure_reverse().ok()?;
    let (rel_start, rel_end) = reverse_dfa_locate_in_slice(dfa, query.bytes_regex(), slice)?;
    let absolute_start = slice_start_off.saturating_add(rel_start);
    let absolute_end = slice_start_off.saturating_add(rel_end);
    let aligned_start = doc.align_byte_offset(absolute_start, AlignDirection::Backward);
    let aligned_end = doc.align_byte_offset(absolute_end, AlignDirection::Forward);
    if aligned_end <= aligned_start {
        return None;
    }
    // Translate aligned absolute offsets back into slice-relative ones
    // for prefix / match byte slicing. The slice covers
    // `[slice_start_off, slice_start_off + slice.len())`; alignment can
    // only move offsets toward valid character boundaries inside that
    // range, so the rebased indices stay within `slice`.
    let rebased_start = aligned_start
        .saturating_sub(slice_start_off)
        .min(slice.len());
    let rebased_end = aligned_end.saturating_sub(slice_start_off).min(slice.len());
    if rebased_end <= rebased_start {
        return None;
    }
    finalize_byte_match(
        bound_start,
        &slice[..rebased_start],
        &slice[rebased_start..rebased_end],
        None,
    )
}

/// Reverse-DFA regex search over a piece-tree backing in chunked
/// windows from `bound_end` toward `bound_start`.
///
/// Walks `REGEX_CHUNK_BYTES` windows from `end_off` toward `start_off`
/// with `REGEX_CHUNK_OVERLAP_BYTES` of overlap so a match crossing a
/// chunk boundary is still observable. On each window
/// `try_search_rev` is run against the bytes; the first window that
/// yields a match returns it (windows are processed end-to-start, so
/// the match in the first matching window is the rightmost match in
/// the requested range).
///
/// Patterns whose longest possible match against arbitrary input is
/// longer than `REGEX_CHUNK_OVERLAP_BYTES` may miss boundary-straddling
/// matches in reverse; this is the same documented streaming-regex
/// limit the forward chunked path uses.
///
/// Each accepted match is post-filtered through
/// [`Document::align_byte_offset`] before being returned,
/// matching the contract in [`reverse_dfa_search_in_slice`].
///
/// **Signature note:** see [`reverse_dfa_search_in_slice`]
/// for why the helper takes `query: &RegexSearchQuery` rather than the
/// literal `dfa: &dense::DFA<Vec<u32>>` from the task description.
fn reverse_dfa_search_in_piece_tree(
    doc: &Document,
    query: &RegexSearchQuery,
    bound_start: TextPosition,
    bound_end: TextPosition,
) -> Option<SearchMatch> {
    let start_off = doc.search_byte_offset_for_position(bound_start);
    let end_off = doc.search_byte_offset_for_position(bound_end);
    if start_off >= end_off {
        return None;
    }
    let dfa = query.ensure_reverse().ok()?;
    let forward_regex = query.bytes_regex();

    let total_span = end_off.saturating_sub(start_off);
    let mut window_end = end_off;
    loop {
        let window_size = REGEX_CHUNK_BYTES.min(total_span);
        let window_start = window_end.saturating_sub(window_size).max(start_off);
        let chunk = doc.piece_table_uncapped_range(window_start, window_end)?;
        if let Some((rel_start, rel_end)) = reverse_dfa_locate_in_slice(dfa, forward_regex, &chunk)
        {
            let absolute_start = window_start.saturating_add(rel_start);
            let absolute_end = window_start.saturating_add(rel_end);
            let aligned_start = doc.align_byte_offset(absolute_start, AlignDirection::Backward);
            let aligned_end = doc.align_byte_offset(absolute_end, AlignDirection::Forward);
            if aligned_end > aligned_start {
                // Rebase aligned offsets onto the bytes between
                // `start_off` and `aligned_end` so the prefix scan
                // starting from `bound_start` can produce the
                // start/end positions. Reading the prefix as a single
                // piece-tree slice is the same pattern the forward
                // path uses for piece-tree backings.
                let prefix_bytes = doc.piece_table_uncapped_range(start_off, aligned_start)?;
                let match_bytes = doc.piece_table_uncapped_range(aligned_start, aligned_end)?;
                if let Some(found) =
                    finalize_byte_match(bound_start, &prefix_bytes, &match_bytes, Some(bound_end))
                {
                    return Some(found);
                }
            }
            // Alignment collapsed the match to an empty span, or the
            // typed `bound_end` filter rejected it. Fall through to
            // the next earlier window — a rare edge case that only
            // fires on encoding-corrupted input.
        }
        if window_start == start_off {
            break;
        }
        // Slide the window left, keeping overlap so matches that
        // straddle this chunk's left edge are still observable on the
        // next iteration.
        window_end = window_start.saturating_add(REGEX_CHUNK_OVERLAP_BYTES);
        if window_end <= start_off {
            break;
        }
    }
    None
}

/// Reverse-DFA regex search over a rope-backed document in
/// `rope.chunks()` order, walked in reverse.
///
/// Each rope chunk is valid UTF-8; we scan it as raw bytes via
/// [`str::as_bytes`] so the reverse DFA can run directly without an
/// intermediate `String` allocation. Chunks are processed in reverse
/// order; the first chunk to yield a `try_search_rev` hit produces the
/// rightmost match in the requested range. A trailing window of the
/// previous chunk's bytes is kept as overlap so a match straddling the
/// chunk boundary is still observable, mirroring the streaming-regex
/// limit documented for the piece-tree path.
///
/// Patterns whose longest possible match against arbitrary input is
/// longer than `REGEX_CHUNK_OVERLAP_BYTES` may miss boundary-straddling
/// matches in reverse — same contract as
/// [`reverse_dfa_search_in_piece_tree`].
///
/// The returned match is post-filtered through
/// [`Document::align_byte_offset`]. The rope is a UTF-8 buffer,
/// so alignment collapses to the existing UTF-8 char-boundary walkers.
///
/// **Signature note:** see [`reverse_dfa_search_in_slice`]
/// for why the helper takes `query: &RegexSearchQuery` rather than the
/// literal `dfa: &dense::DFA<Vec<u32>>` from the task description.
fn reverse_dfa_search_in_rope(
    doc: &Document,
    rope: &Rope,
    query: &RegexSearchQuery,
    bound_start: TextPosition,
    bound_end: TextPosition,
) -> Option<SearchMatch> {
    let start_char = doc.char_index_for_position(bound_start);
    let end_char = doc.char_index_for_position(bound_end).max(start_char);
    if start_char >= rope.len_chars() {
        return None;
    }
    let start_byte = rope.char_to_byte(start_char);
    let end_byte = rope.char_to_byte(end_char.min(rope.len_chars()));
    if start_byte >= end_byte {
        return None;
    }
    let dfa = query.ensure_reverse().ok()?;
    let forward_regex = query.bytes_regex();

    // Build a sliding byte window from the end of the requested range
    // using `rope.chunks()` walked in reverse. Each chunk in ropey is
    // valid UTF-8, so `chunk.as_bytes()` is a safe slice for the DFA.
    // The window is bounded to REGEX_CHUNK_BYTES + REGEX_CHUNK_OVERLAP_BYTES
    // worth of trailing rope content so peak memory stays predictable.
    let window_capacity = REGEX_CHUNK_BYTES.saturating_add(REGEX_CHUNK_OVERLAP_BYTES);
    let mut window: Vec<u8> = Vec::with_capacity(window_capacity.min(end_byte - start_byte));
    let mut window_start_byte = end_byte;

    let chunks: Vec<(usize, &str)> = collect_rope_chunks_with_offsets(rope);
    for (chunk_base, chunk) in chunks.iter().rev() {
        let chunk_base = *chunk_base;
        let chunk_bytes = chunk.as_bytes();
        let chunk_end_byte = chunk_base.saturating_add(chunk_bytes.len());
        if chunk_base >= end_byte {
            continue;
        }
        if chunk_end_byte <= start_byte {
            break;
        }

        let take_start = start_byte.saturating_sub(chunk_base).min(chunk_bytes.len());
        let take_end = end_byte.saturating_sub(chunk_base).min(chunk_bytes.len());
        if take_end <= take_start {
            continue;
        }
        let segment = &chunk_bytes[take_start..take_end];

        // Prepend this segment to the window (we walk backwards).
        let mut new_window = Vec::with_capacity(segment.len() + window.len());
        new_window.extend_from_slice(segment);
        new_window.extend_from_slice(&window);
        window = new_window;
        window_start_byte = chunk_base.saturating_add(take_start);

        if window.len() >= REGEX_CHUNK_BYTES {
            if let Some(found) = reverse_dfa_finalize_rope_window(
                doc,
                rope,
                dfa,
                forward_regex,
                window_start_byte,
                &window,
                bound_end,
            ) {
                return Some(found);
            }
            // Trim to overlap before prepending the next earlier chunk
            // so peak memory stays bounded. UTF-8 floor boundary guard
            // keeps the trailing slice on a char boundary.
            if window.len() > REGEX_CHUNK_OVERLAP_BYTES {
                let drop_at = window.len().saturating_sub(REGEX_CHUNK_OVERLAP_BYTES);
                let drop_at = utf8_floor_boundary_bytes(&window, drop_at);
                window.truncate(drop_at);
            }
        }
    }

    if !window.is_empty() {
        if let Some(found) = reverse_dfa_finalize_rope_window(
            doc,
            rope,
            dfa,
            forward_regex,
            window_start_byte,
            &window,
            bound_end,
        ) {
            return Some(found);
        }
    }

    None
}

/// Runs the reverse DFA + forward-regex pair against a rope window and
/// builds a `SearchMatch` from the recovered match span, post-filtered
/// through [`Document::align_byte_offset`].
///
/// `window_start_byte` is the absolute rope byte offset of the first
/// byte in `window`; the rope is consulted to translate aligned
/// absolute byte offsets back into char indices and `TextPosition`s.
fn reverse_dfa_finalize_rope_window(
    doc: &Document,
    rope: &Rope,
    dfa: &dense::DFA<Vec<u32>>,
    forward_regex: &ByteRegex,
    window_start_byte: usize,
    window: &[u8],
    bound_end: TextPosition,
) -> Option<SearchMatch> {
    let (rel_start, rel_end) = reverse_dfa_locate_in_slice(dfa, forward_regex, window)?;
    let abs_start = window_start_byte.saturating_add(rel_start);
    let abs_end = window_start_byte.saturating_add(rel_end);
    let aligned_start = doc.align_byte_offset(abs_start, AlignDirection::Backward);
    let aligned_end = doc.align_byte_offset(abs_end, AlignDirection::Forward);
    if aligned_end <= aligned_start {
        return None;
    }
    let start_char = rope.byte_to_char(aligned_start);
    let end_char = rope.byte_to_char(aligned_end);
    let match_units = end_char.saturating_sub(start_char);
    let start_pos = doc.position_for_char_index(start_char);
    let end_pos = doc.position_for_char_index(end_char);
    if end_pos > bound_end {
        return None;
    }
    Some(SearchMatch::new(
        TextRange::new(start_pos, match_units),
        end_pos,
    ))
}

// ---------------------------------------------------------------------------
// Class B chunked regex
// ---------------------------------------------------------------------------
//
// UTF-16 and multibyte CJK regex search runs the text engine against
// decoded `&str` windows of the underlying byte storage and maps match
// offsets back to the source byte stream. The rationale is to avoid
// pattern-conversion to a per-encoding byte form, keeps Unicode class
// semantics, and handles supplementary characters / variable-length CJK
// sequences through the same mapping as decode-time.
//
// The two encoding families differ in the source-byte mapping:
//
//   * UTF-16: every Unicode scalar occupies `c.len_utf16() * 2` source
//     bytes, so the prefix length is a per-character running sum.
//     Match offsets are post-filtered against the 2-byte alignment
//     requirement: UTF-16 character boundaries are always at even
//     byte offsets, so any candidate landing on an odd offset is
//     necessarily straddling a `U+FFFD` injected by the decoder for
//     ill-formed input and must be rejected.
//
//   * CJK multibyte (Shift_JIS, GB18030, EUC-KR): characters are
//     variable-length (1 / 2 / 4 bytes) with no uniform alignment, so
//     the source-byte length of a decoded prefix is computed by
//     re-encoding the prefix through `encoding_rs::Encoding::encode`.
//     The decode → re-encode round-trip preserves character boundaries
//     for byte sequences originating from a real CJK file, so an
//     explicit alignment post-filter is not needed: any candidate
//     whose offsets the round-trip cannot reproduce already implies
//     boundary corruption and is naturally rejected upstream by the
//     decoder. We still verify alignment lazily by relying on the
//     fact that `encoding_rs::Encoding::encode` for the prefix
//     yields a byte-length that, summed with the match's encoded
//     byte-length, equals the slice taken from the original window.
// ---------------------------------------------------------------------------

/// Returns the source-byte length occupied by `decoded_byte_count` UTF-8
/// bytes of `decoded` in the original encoded byte stream.
///
/// Branches on `encoding`:
///
/// * For UTF-16 (LE or BE) the mapping is the per-character UTF-16 byte
///   length: every Unicode scalar contributes `c.len_utf16() * 2`
///   source bytes.
///
/// * For the CJK multibyte encodings (Shift_JIS, GB18030, EUC-KR) the
///   mapping is `encoding.encode(prefix).0.len()`: re-encoding the
///   decoded UTF-8 prefix through `encoding_rs::Encoding::encode` is
///   the canonical inverse of the decode that produced `decoded` from
///   the original window. The re-encode is byte-exact for character
///   sequences that round-trip cleanly through the encoding (which is
///   every well-formed sequence we observe at the regex layer); for
///   any sequence the encoder has to substitute, the chunked walker
///   above retries on subsequent windows.
///
/// Returns `None` when `decoded_byte_count` is not on a UTF-8 character
/// boundary in `decoded`. The regex engine never produces such offsets,
/// so this is a defensive guard rather than a real branch.
fn class_b_source_bytes_for_decoded_prefix(
    encoding: DocumentEncoding,
    decoded: &str,
    decoded_byte_count: usize,
) -> Option<usize> {
    if decoded_byte_count > decoded.len() {
        return None;
    }
    if !decoded.is_char_boundary(decoded_byte_count) {
        return None;
    }
    let prefix = &decoded[..decoded_byte_count];
    if encoding.name() == "UTF-16LE" || encoding.name() == "UTF-16BE" {
        let mut bytes: usize = 0;
        for c in prefix.chars() {
            bytes = bytes.saturating_add(c.len_utf16().saturating_mul(2));
        }
        return Some(bytes);
    }
    // CJK multibyte (Shift_JIS, GB18030, EUC-KR): re-encode the prefix
    // through encoding_rs to obtain the source-byte length. `encode`
    // returns `(Cow<[u8]>, &Encoding, bool had_unmappable)`; we use
    // only the byte length here. `had_unmappable` cannot legitimately
    // fire for input that came from `decode_without_bom_handling` over
    // the original encoded window: every character was produced by a
    // well-formed multibyte sequence (or a U+FFFD for ill-formed
    // bytes, which the encoder maps back to a known representation).
    let encoded = encoding.as_encoding().encode(prefix).0;
    Some(encoded.len())
}

/// Returns `true` when `encoding` requires a 2-byte alignment
/// post-filter on the absolute byte offsets of a Class B regex match.
///
/// Only UTF-16 is alignment-constrained: every code-unit cell is
/// 2 bytes, so a match landing on an odd offset can only happen when
/// it straddles a `U+FFFD` replacement character produced by the
/// decoder for ill-formed surrogate pairs. CJK multibyte encodings
/// have no global alignment grid (characters are 1/2/4 bytes), so the
/// encode-decode round-trip already validates character boundaries
/// and the post-filter is intentionally skipped.
fn class_b_requires_even_alignment(encoding: DocumentEncoding) -> bool {
    matches!(encoding.name(), "UTF-16LE" | "UTF-16BE")
}

/// Chunked decode + glue regex for Class B (UTF-16 and CJK multibyte)
/// backings.
///
/// Iterates `[start_offset, end_offset)` in `REGEX_CHUNK_BYTES` windows
/// (aligned to even byte boundaries for UTF-16; arbitrary boundaries
/// for CJK multibyte, which has no global alignment grid), with
/// `REGEX_CHUNK_OVERLAP_BYTES` of overlap on continuation chunks so a
/// match that straddles a chunk boundary is still observed. Each
/// window is decoded through the document's encoding via
/// `encoding_rs::Encoding::decode_without_bom_handling`, the
/// text-engine regex is run against the resulting `&str`, and the
/// first match is mapped back to absolute source-byte offsets.
///
/// Source-byte mapping is encoding-dispatched: UTF-16 uses
/// `c.len_utf16() * 2` per character; CJK multibyte re-encodes the
/// decoded prefix through `encoding_rs::Encoding::encode`. See
/// [`class_b_source_bytes_for_decoded_prefix`].
///
/// Post-filter on absolute byte offsets is alignment-dispatched:
/// UTF-16 rejects matches whose start or end is not on a 2-byte
/// boundary — that can only happen when a match
/// straddles a `U+FFFD` replacement character produced by ill-formed
/// surrogate pairs in the source. CJK multibyte does not enforce a
/// global alignment grid, so the post-filter is
/// skipped: the encode-decode round-trip itself validates character
/// boundaries (any byte sequence the encoder cannot reproduce was
/// already a `U+FFFD`-substituted boundary corruption upstream and
/// contributes no `match_bytes` slice that would survive the slice
/// arithmetic below).
///
/// On continuation chunks we skip matches that lie entirely inside the
/// previous-chunk overlap, mirroring the byte-engine pattern used by
/// `find_next_regex_in_bytes` for piece-tree backings.
///
/// Returns `Some((absolute_start, absolute_end, match_bytes))` for the
/// first accepted match, or `None` if none of the chunks contained one.
fn find_next_regex_in_class_b_chunked(
    doc: &Document,
    query: &RegexSearchQuery,
    start_offset: usize,
    end_offset: usize,
) -> Option<(usize, usize, Vec<u8>)> {
    if start_offset >= end_offset {
        return None;
    }

    let doc_encoding = doc.encoding();
    let encoding = doc_encoding.as_encoding();
    let requires_even_alignment = class_b_requires_even_alignment(doc_encoding);

    // Round chunk start up to an even byte for UTF-16 so each window
    // starts on a UTF-16 code-unit cell. For CJK
    // multibyte the start can be any byte: callers ensure the initial
    // `start_offset` lands on a character boundary by going through
    // `search_byte_offset_for_position`, and subsequent chunks slide
    // forward by `chunk_size - overlap` which preserves whatever
    // alignment the encoding requires.
    let mut chunk_start = start_offset;
    let mut first_chunk = true;
    while chunk_start < end_offset {
        let chunk_end = chunk_start
            .saturating_add(REGEX_CHUNK_BYTES)
            .min(end_offset);
        // For UTF-16 ensure chunk_end is even so the trailing code-
        // unit cell is never split across two chunks. For CJK
        // multibyte encodings every chunk boundary is allowed; if a
        // multibyte sequence is split across the boundary, the
        // overlap region brings it back into the next window so the
        // decoder sees a complete sequence.
        let chunk_end = if requires_even_alignment && chunk_end & 1 == 1 && chunk_end > chunk_start
        {
            chunk_end - 1
        } else {
            chunk_end
        };
        if chunk_end <= chunk_start {
            break;
        }

        // Fetch the byte window: mmap zero-copy when possible, otherwise
        // a piece-tree byte slice. We materialise both as a `Vec<u8>` so
        // the rest of the loop body shares one code path.
        let window: Vec<u8> = if let Some(slice) = doc.mmap_search_slice(chunk_start, chunk_end) {
            slice.to_vec()
        } else if let Some(buf) = doc.piece_table_uncapped_range(chunk_start, chunk_end) {
            buf
        } else {
            return None;
        };

        // Decode the window into a Cow<str>. `decode_without_bom_handling`
        // is documented to ignore the BOM in the byte stream (we already
        // strip the BOM at open time for UTF-16), so this is the right
        // primitive for window decoding.
        let (decoded, _had_errors) = encoding.decode_without_bom_handling(&window);

        let overlap_already_matched_decoded = if first_chunk {
            0
        } else {
            // The leading `REGEX_CHUNK_OVERLAP_BYTES` source bytes were
            // already part of the previous chunk's window. Translate that
            // source-byte overlap into a decoded-byte threshold so we can
            // reject matches that lie entirely inside the overlap region.
            let overlap_source_bytes = REGEX_CHUNK_OVERLAP_BYTES.min(window.len());
            decoded_prefix_for_source_byte_count(doc_encoding, &decoded, overlap_source_bytes)
        };

        if let Some(m) = query.text_regex().find(&decoded) {
            // Skip matches fully inside the previous-chunk overlap on
            // continuation chunks; they were either already returned or
            // will be returned by an earlier window.
            if first_chunk || m.end() > overlap_already_matched_decoded {
                let prefix_source_len =
                    class_b_source_bytes_for_decoded_prefix(doc_encoding, &decoded, m.start())?;
                let match_source_len = class_b_source_bytes_for_decoded_prefix(
                    doc_encoding,
                    &decoded[m.start()..],
                    m.end() - m.start(),
                )?;
                let absolute_match_start = chunk_start.saturating_add(prefix_source_len);
                let absolute_match_end = absolute_match_start.saturating_add(match_source_len);

                // 2-byte alignment post-filter for UTF-16 only.
                // UTF-16 character boundaries are always at even byte
                // offsets, so any candidate landing on an odd offset is
                // necessarily straddling a U+FFFD injected by the
                // decoder for ill-formed input and must be rejected.
                // CJK multibyte has no such global alignment
                // grid so the post-filter is skipped.
                let alignment_ok = if requires_even_alignment {
                    absolute_match_start & 1 == 0 && absolute_match_end & 1 == 0
                } else {
                    true
                };
                if alignment_ok && prefix_source_len + match_source_len <= window.len() {
                    let match_bytes: Vec<u8> =
                        window[prefix_source_len..(prefix_source_len + match_source_len)].to_vec();
                    return Some((absolute_match_start, absolute_match_end, match_bytes));
                }
                // Misaligned (UTF-16) or out-of-window match — fall
                // through to continue scanning subsequent chunks.
                // Since `find` returns only the first match in the
                // window, a misaligned hit here means the earliest
                // valid match (if any) lies past it; we step the
                // window forward via the overlap-slide and try again.
                // This is rare in practice (only on ill-formed
                // surrogate-pair input or U+FFFD-substituted CJK
                // sequences).
            }
        }

        if chunk_end >= end_offset {
            break;
        }
        // Slide forward keeping the overlap. For UTF-16 both the
        // overlap and the chunk size are even, so the resulting
        // offset stays on a 2-byte boundary. Forward progress
        // is guaranteed by the `next_chunk_start > chunk_start`
        // check below: if the slide would not advance (e.g. when the
        // trimmed chunk is shorter than the overlap), we step forward
        // by the alignment quantum (2 bytes for UTF-16, 1 byte for
        // CJK multibyte) so the loop cannot stall.
        let next_chunk_start = chunk_end.saturating_sub(REGEX_CHUNK_OVERLAP_BYTES);
        chunk_start = if next_chunk_start > chunk_start {
            next_chunk_start
        } else if requires_even_alignment {
            chunk_start.saturating_add(2)
        } else {
            chunk_start.saturating_add(1)
        };
        first_chunk = false;
    }

    None
}

/// Returns the largest decoded-byte prefix length whose corresponding
/// source-byte length is `<= source_bytes` for a Class B backing.
///
/// Used to translate the source-byte overlap region into the decoded-byte
/// space so we can reject continuation-chunk matches that lie entirely
/// inside it. Encoding-dispatched on the same axis as
/// [`class_b_source_bytes_for_decoded_prefix`]:
///
/// * UTF-16: every Unicode scalar contributes `c.len_utf16() * 2`
///   source bytes; we walk the chars and keep the largest prefix that
///   still fits inside `source_bytes`.
///
/// * CJK multibyte (Shift_JIS, GB18030, EUC-KR): each character's
///   source-byte cost is `encoding.encode(c)`; we accumulate per
///   character until the next step would exceed `source_bytes`.
fn decoded_prefix_for_source_byte_count(
    encoding: DocumentEncoding,
    decoded: &str,
    source_bytes: usize,
) -> usize {
    let is_utf16 = matches!(encoding.name(), "UTF-16LE" | "UTF-16BE");
    let mut acc_source = 0usize;
    let mut acc_decoded = 0usize;
    if is_utf16 {
        for c in decoded.chars() {
            let next_source = acc_source.saturating_add(c.len_utf16().saturating_mul(2));
            if next_source > source_bytes {
                break;
            }
            acc_source = next_source;
            acc_decoded = acc_decoded.saturating_add(c.len_utf8());
        }
    } else {
        // CJK multibyte: per-character re-encode. We keep a single
        // small buffer to bound allocations. `encoding_rs::Encoding::encode`
        // accepts `&str`, so we re-encode each char individually.
        let mut buf = [0u8; 4];
        for c in decoded.chars() {
            let s = c.encode_utf8(&mut buf);
            let encoded = encoding.as_encoding().encode(s).0;
            let next_source = acc_source.saturating_add(encoded.len());
            if next_source > source_bytes {
                break;
            }
            acc_source = next_source;
            acc_decoded = acc_decoded.saturating_add(c.len_utf8());
        }
    }
    acc_decoded
}

/// Builds a `SearchMatch` from a Class B chunked match expressed in
/// absolute source-byte offsets.
///
/// Computes encoding-aware start / end `TextPosition`s through
/// `Document::position_for_byte_offset_in_class_b` and counts text-units
/// over the matched byte slice via the document's encoding engine. CRLF
/// is collapsed into a single text unit by the engine, mirroring the
/// UTF-8 byte-engine semantics in `finalize_byte_match`.
///
/// Returns `None` when the typed `end` bound (if any) is exceeded by the
/// computed end position.
fn build_class_b_search_match(
    doc: &Document,
    match_start: usize,
    match_end: usize,
    match_bytes: &[u8],
    end: Option<TextPosition>,
) -> Option<SearchMatch> {
    let start_pos = doc.position_for_byte_offset_in_class_b(match_start);
    let end_pos = doc.position_for_byte_offset_in_class_b(match_end);
    if let Some(end_bound) = end {
        if end_pos > end_bound {
            return None;
        }
    }
    // Count text units over the matched byte slice using the document's
    // engine. We do this by repeatedly advancing one text unit at a time
    // through `advance_offset_by_text_units`, which the engine
    // implements with the correct CRLF-collapse semantics for its
    // encoding. For typical regex matches this is
    // O(match_length / step_size) which is bounded by the match byte
    // length and stays cheap.
    let engine = doc.encoding_engine();
    let mut units = 0usize;
    let mut offset = 0usize;
    let total = match_bytes.len();
    loop {
        let next = engine.advance_offset_by_text_units(match_bytes, total, offset, 1);
        if next <= offset {
            break;
        }
        offset = next;
        units = units.saturating_add(1);
        if offset >= total {
            break;
        }
    }
    Some(SearchMatch::new(TextRange::new(start_pos, units), end_pos))
}
