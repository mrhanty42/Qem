// Property 14: public APIs return char-aligned offsets
//
//
// For any document opened in a non-UTF-8 encoding, after applying a
// random sequence of representable edits, every "boundary offset"
// surfaced by the document's public API MUST satisfy
// `align_byte_offset(offset, AlignDirection::Backward) == offset`
// (the `Floor` contract). The original property pinned this
// behaviour against the engine's own boundary surfaces
// (`next_line_start`, `step`); this widens the
// coverage to the full set of public APIs that hand a byte or text
// offset back to the caller, for every implemented encoding.
//
// The boundary surfaces exercised here split into two groups:
//
// **Engine-level surfaces:**
//
// * **Line starts** — every offset returned by the engine's
// `next_line_start` walk from `0` to `file_len`. Mirrors what
// `Document::read_viewport` and the edit paths see when they
// resolve `line0` to a byte offset.
// * **Char-aligned cursor positions** — every offset visited by
// `engine.step(bytes, p, file_len)` while walking forward from
// `0`. Mirrors what `Document::advance_offset_by_text_units` and
// the regex / literal-search post-filter use.
//
// **Public-API surfaces:**
//
// * **Viewport bounds** — `Document::read_viewport` row line
// starts and `line0`-clamped end positions, converted to the
// same byte offset the search backends use.
// * **Line bounds** — for every visible row, the byte offsets of
// `(line0, 0)` (line start) and `(line0, line_len_chars(line0))`
// (line end excluding any trailing line break).
// * **Literal search match endpoints** — `find_next` /
// `find_prev` / `find_next_in_range` / `find_next_between` /
// `find_prev_in_range` / `find_prev_between` (and their `_query`
// variants) plus `find_all` / `find_all_from` / `find_all_query`
// / `find_all_query_from` / `find_all_in_range` /
// `find_all_between` / `find_all_query_in_range` /
// `find_all_query_between`.
// * **Regex search match endpoints** — `find_next_regex` /
// `find_prev_regex` / `find_next_regex_in_range` /
// `find_next_regex_between` (and their `_query` /
// `_query_in_range` / `_query_between` variants) plus
// `find_all_regex` / `find_all_regex_from` /
// `find_all_regex_query` / `find_all_regex_query_from` /
// `find_all_regex_in_range` / `find_all_regex_between` /
// `find_all_regex_query_in_range` /
// `find_all_regex_query_between`.
//
// Match endpoints are typed (`SearchMatch::start()` / `end()` are
// `TextPosition`), so the property converts each through the same
// `Document::search_byte_offset_for_position` helper the search
// backends use internally before checking alignment. A regression
// that returned a sub-character byte offset would surface here
// because the converted offset would no longer be `Floor`-stable.
//
// Both surfaces must be `Floor`-stable: passing a boundary offset
// back through `Document::align_byte_offset(..., Backward)` must
// return the same offset unchanged. A regression that shifted the
// alignment heuristic (e.g. broken anchor recovery for Class B
// `& !1` vs `(offset + 1) & !1` swap for UTF-16, scan-from-anchor
// drift after mid-character storage promotion) would surface as a
// boundary that no longer round-trips.
//
// Strategy. Each case picks one encoding from
// `{windows_1251, koi8_r, ibm866, shift_jis, gb18030, euc_kr
// utf16_le, utf16_be}`, materialises a small per-encoding seed
// fixture (mixing ASCII with non-ASCII content native to the target
// encoding so multi-byte boundaries actually exist in the file)
// opens it through `Document::open_with_encoding`, and then applies
// up to 6 random edits drawn from `Insert`, `Delete`, `Replace`.
//
// Edits drive the same code paths the production document layer
// uses for non-UTF-8 storage:
//
// * `Document::try_insert_text_at` dispatches to
// `try_insert_text_at_encoded`.
// * `Document::try_replace_range` dispatches to
// `try_replace_range_encoded` (or `try_delete_range_at_encoded`
// when the replacement text is empty).
//
// Both encoded paths route their byte-offset arithmetic through
// `Document::align_byte_offset` already. The
// property here is one level higher: after the edit dust settles
// *every* boundary the engine surfaces is a fixed point of the same
// alignment helper.
//
// Edit text is generated from a per-encoding alphabet of
// representable scalars (ASCII + a small tail of non-ASCII glyphs
// native to the target encoding) so `had_unmappable` never trips
// inside `encoding_rs::Encoding::encode`. UTF-16 has no
// representability constraint inside the BMP, so its alphabet
// simply mixes ASCII with a few Han ideographs to exercise mixed
// 2-byte boundaries on both endianness markers.
//
// `ProptestConfig::with_cases(64)` per the spec; `fresh_test_dir`
// honours `$env:TMP` / `$env:TEMP` for fixtures.

#[path = "mod.rs"]
#[allow(clippy::duplicate_mod)] // shared helpers module is also loaded by sibling integration tests
mod helpers;

use encoding_rs::{Encoding, EUC_KR, GB18030, IBM866, KOI8_R, SHIFT_JIS, WINDOWS_1251};
use helpers::fresh_test_dir;
use proptest::prelude::*;
use qem::document::__test_support::{
    align_byte_offset_floor, byte_offset_for_text_position, bytes_for_alignment,
    engine_for_encoding, EncodingEngine,
};
use qem::{
    Document, DocumentEncoding, LiteralSearchQuery, RegexSearchQuery, SearchMatch, TextPosition,
    TextRange, ViewportRequest,
};

/// Encodings under test. Eight non-UTF-8 codecs: the three Cyrillic
/// Class A targets (windows-1251, KOI8-R, IBM866), the three CJK
/// Class B multi-byte targets (Shift_JIS, gb18030, EUC-KR), and both
/// UTF-16 endianness markers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EncodingKind {
    Windows1251,
    Koi8R,
    Ibm866,
    ShiftJis,
    Gb18030,
    EucKr,
    Utf16Le,
    Utf16Be,
}

impl EncodingKind {
 /// Canonical `encoding_rs` label. `engine_for_encoding` matches
 /// against this exact name for every Class A and Class B codec.
    fn label(self) -> &'static str {
        match self {
            Self::Windows1251 => "windows-1251",
            Self::Koi8R => "KOI8-R",
            Self::Ibm866 => "IBM866",
            Self::ShiftJis => "Shift_JIS",
            Self::Gb18030 => "gb18030",
            Self::EucKr => "EUC-KR",
            Self::Utf16Le => "UTF-16LE",
            Self::Utf16Be => "UTF-16BE",
        }
    }

 /// Qem `DocumentEncoding` handle.
    fn document_encoding(self) -> DocumentEncoding {
        let label = self.label();
        DocumentEncoding::from_label(label)
            .unwrap_or_else(|| panic!("encoding_rs should know {label}"))
    }

 /// `encoding_rs::Encoding` for the target codec. UTF-16 LE/BE
 /// are decode-only in the WHATWG model, so we hand-encode their
 /// fixtures via `str::encode_utf16` — but the constant is still
 /// useful for representability probes on non-UTF-16 encodings.
    fn encoding_rs(self) -> Option<&'static Encoding> {
        match self {
            Self::Windows1251 => Some(WINDOWS_1251),
            Self::Koi8R => Some(KOI8_R),
            Self::Ibm866 => Some(IBM866),
            Self::ShiftJis => Some(SHIFT_JIS),
            Self::Gb18030 => Some(GB18030),
            Self::EucKr => Some(EUC_KR),
            Self::Utf16Le | Self::Utf16Be => None,
        }
    }

 /// Per-encoding alphabet for edit text. Every emitted character
 /// is representable in the target encoding so
 /// `Encoding::encode(...).had_unmappable` never trips. The
 /// upper bound on string length keeps each case cheap and
 /// shrinking quick.
    fn edit_text_regex(self) -> &'static str {
        match self {
 // ASCII + Cyrillic capital/lowercase blocks.
            Self::Windows1251 | Self::Koi8R | Self::Ibm866 => {
                r"[a-z0-9 \u0410-\u042F\u0430-\u044F]{0,12}"
            }
 // ASCII + Hiragana — fully covered by JIS X 0208.
            Self::ShiftJis => r"[a-z0-9 \u3041-\u3093]{0,12}",
 // ASCII + a small CJK Unified Ideographs slice — every
 // code point has a valid gb18030 encoding (mix of 2- and
 // 4-byte sequences once the trail-byte rules kick in).
            Self::Gb18030 => r"[a-z0-9 \u4E00-\u4E2F]{0,12}",
 // ASCII + a small Hangul Syllables slice — KS X 1001
 // covers the entire U+AC00..=U+D7A3 block, the slice
 // here keeps the alphabet small for shrinking.
            Self::EucKr => r"[a-z0-9 \uAC00-\uAC1F]{0,12}",
 // UTF-16 covers all Unicode; mix ASCII with a few BMP
 // Han ideographs so the engine sees both 2- and 2-byte
 // (single code unit) cells.
            Self::Utf16Le | Self::Utf16Be => r"[a-z0-9 \u4E00-\u4E2F]{0,12}",
        }
    }

 /// Returns a small seed fixture in the target encoding. Each
 /// seed mixes ASCII anchor text, native-script content, LF line
 /// terminators, and a final non-newline tail line so the engine
 /// surfaces a non-trivial set of line and char boundaries from
 /// the very first open.
    fn seed_bytes(self) -> Vec<u8> {
        match self {
            Self::Windows1251 | Self::Koi8R | Self::Ibm866 => {
                let text = "anchor\nПривет\nмир\ntail";
                self.encoding_rs().unwrap().encode(text).0.into_owned()
            }
            Self::ShiftJis => {
                let text = "anchor\nこんにちは\nさようなら\ntail";
                self.encoding_rs().unwrap().encode(text).0.into_owned()
            }
            Self::Gb18030 => {
                let text = "anchor\n你好\n世界\ntail";
                self.encoding_rs().unwrap().encode(text).0.into_owned()
            }
            Self::EucKr => {
                let text = "anchor\n안녕\n세계\ntail";
                self.encoding_rs().unwrap().encode(text).0.into_owned()
            }
            Self::Utf16Le => "anchor\n世界\nтест\ntail"
                .encode_utf16()
                .flat_map(|u| u.to_le_bytes())
                .collect(),
            Self::Utf16Be => "anchor\n世界\nтест\ntail"
                .encode_utf16()
                .flat_map(|u| u.to_be_bytes())
                .collect(),
        }
    }

 /// Probe whether `text` round-trips through the target encoding
 /// without redirect or unmappable scalars. UTF-16 covers every
 /// Unicode scalar inside the BMP (and supplementary points via
 /// surrogate pairs), so the probe short-circuits to `true`.
    fn is_representable(self, text: &str) -> bool {
        match self.encoding_rs() {
            Some(enc) => {
                let (_, used, had_unmappable) = enc.encode(text);
                std::ptr::eq(used, enc) && !had_unmappable
            }
            None => true,
        }
    }
}

/// One edit drawn from the public, non-UTF-8 edit surface. All three
/// variants route through `try_insert_text_at_encoded` /
/// `try_delete_range_at_encoded` / `try_replace_range_encoded`.
#[derive(Debug, Clone)]
enum Edit {
    Insert {
        line: usize,
        col: usize,
        text: String,
    },
    Delete {
        line: usize,
        col: usize,
        len_chars: usize,
    },
    Replace {
        line: usize,
        col: usize,
        len_chars: usize,
        text: String,
    },
}

fn encoding_kind_strategy() -> impl Strategy<Value = EncodingKind> {
    prop_oneof![
        Just(EncodingKind::Windows1251),
        Just(EncodingKind::Koi8R),
        Just(EncodingKind::Ibm866),
        Just(EncodingKind::ShiftJis),
        Just(EncodingKind::Gb18030),
        Just(EncodingKind::EucKr),
        Just(EncodingKind::Utf16Le),
        Just(EncodingKind::Utf16Be),
    ]
}

/// Position bounds intentionally span beyond the seed's actual line
/// count / column count so the document's clamping logic in
/// `try_insert_text_at_encoded` and the rest of the encoded edit
/// path is exercised. Out-of-range positions clamp to the document
/// end rather than failing.
fn position_strategy() -> impl Strategy<Value = (usize, usize)> {
    (0usize..=4, 0usize..=12)
}

fn len_chars_strategy() -> impl Strategy<Value = usize> {
    0usize..=6
}

fn edit_strategy(kind: EncodingKind) -> impl Strategy<Value = Edit> {
    let text = || proptest::string::string_regex(kind.edit_text_regex()).expect("valid regex");
    prop_oneof![
        2 => (position_strategy(), text()).prop_map(|((line, col), text)| Edit::Insert {
            line,
            col,
            text,
        }),
        1 => (position_strategy(), len_chars_strategy()).prop_map(
            |((line, col), len_chars)| Edit::Delete {
                line,
                col,
                len_chars,
            }
        ),
        2 => (position_strategy(), len_chars_strategy(), text()).prop_map(
            |((line, col), len_chars, text)| Edit::Replace {
                line,
                col,
                len_chars,
                text,
            }
        ),
    ]
}

/// `prop_flat_map` so the edit strategy can specialise its alphabet
/// to the just-sampled `EncodingKind`. Without this, every encoding
/// would have to share one global alphabet and either over-filter
/// (most cases skipped) or fail unmappable checks at runtime.
fn case_strategy() -> impl Strategy<Value = (EncodingKind, Vec<Edit>)> {
    encoding_kind_strategy().prop_flat_map(|kind| {
        prop::collection::vec(edit_strategy(kind), 0..=6).prop_map(move |edits| (kind, edits))
    })
}

/// Applies one edit through the public, non-UTF-8 edit surface.
/// Edits are best-effort: if the encoded path rejects the input
/// (e.g. clamping to an empty range, an unrepresentable scalar that
/// snuck past the strategy alphabet, or any other typed
/// `DocumentError`), the result is silently dropped — the property
/// only cares about the document's post-attempt state.
fn apply_edit(doc: &mut Document, edit: &Edit, kind: EncodingKind) {
    match edit {
        Edit::Insert { line, col, text } => {
            if !text.is_empty() && kind.is_representable(text) {
                let _ = doc.try_insert_text_at(*line, *col, text);
            }
        }
        Edit::Delete {
            line,
            col,
            len_chars,
        } => {
            if *len_chars > 0 {
 // try_replace_range with an empty replacement string
 // dispatches to `try_delete_range_at_encoded` for
 // non-UTF-8 documents (commands.rs).
                let _ = doc.try_replace_range(*line, *col, *len_chars, "");
            }
        }
        Edit::Replace {
            line,
            col,
            len_chars,
            text,
        } => {
            if kind.is_representable(text) {
                let _ = doc.try_replace_range(*line, *col, *len_chars, text);
            }
        }
    }
}

/// Asserts Property 14 against the document's current state.
///
/// Walks every public boundary surface that the spec promises must
/// be aligned on a character boundary of `doc.encoding()`:
///
/// 1. **Engine-level:** line starts via
/// `engine.next_line_start` from `0` to `file_len`, and
/// char-aligned cursor positions via `engine.step` from `0` to
/// `file_len`.
/// 2. **Public APIs:** the byte offsets
/// reachable by converting each `TextPosition` returned by
/// viewport reads, line bounds, literal-search APIs, and
/// regex-search APIs through the same
/// `Document::search_byte_offset_for_position` helper the
/// search backends use internally.
///
/// Each visited offset must be a fixed point of
/// `align_byte_offset(.., Backward)` — i.e. the offset already lies
/// on a character boundary of `doc.encoding()`.
///
/// The walks are bounded — a sentinel iteration cap guards against
/// any future engine regression that could stall the loop.
fn check_property_14(doc: &Document, kind: EncodingKind) -> Result<(), TestCaseError> {
    let bytes = bytes_for_alignment(doc);
    let bytes_len = bytes.len();
    let engine: &dyn EncodingEngine = engine_for_encoding(doc.encoding());

 // Sentinel: bytes_len + 2 is plenty for any legal walk (each
 // step advances by at least 1 byte, so bytes_len steps cover
 // every offset). The +2 leaves room for the `bytes_len`
 // terminator visit. A regression that introduced an infinite
 // loop in `next_line_start` or `step` would trip the cap
 // instead of hanging the test.
    let cap = bytes_len.saturating_add(2);

 // (a) Char-aligned cursor positions. `engine.step` is the
 // canonical forward walker; it returns 0 only at file_len, so
 // the loop terminates naturally.
    let mut p = 0usize;
    let mut visited = 0usize;
    loop {
        prop_assert_eq!(
            align_byte_offset_floor(doc, p),
            p,
            "char-boundary offset {} is not Floor-stable (encoding {}, file_len {})",
            p,
            kind.label(),
            bytes_len,
        );
        if p >= bytes_len {
            break;
        }
        let step = engine.step(&bytes, p, bytes_len);
        prop_assert!(
            step > 0,
            "engine.step returned 0 mid-stream at offset {} (encoding {}, file_len {})",
            p,
            kind.label(),
            bytes_len,
        );
        prop_assert!(
            p.saturating_add(step) <= bytes_len,
            "engine.step ({}) at offset {} would overshoot file_len {} (encoding {})",
            step,
            p,
            bytes_len,
            kind.label(),
        );
        p = p.saturating_add(step);
        visited = visited.saturating_add(1);
        prop_assert!(
            visited <= cap,
            "engine.step walk did not terminate within {} iterations (encoding {})",
            cap,
            kind.label(),
        );
    }

 // (b) Line-start offsets. `engine.next_line_start` returns
 // `file_len` once the last line break has been consumed, so the
 // loop terminates by detecting a non-advancing tail.
    let mut line_start = 0usize;
    let mut iterations = 0usize;
    loop {
        prop_assert_eq!(
            align_byte_offset_floor(doc, line_start),
            line_start,
            "line-start offset {} is not Floor-stable (encoding {}, file_len {})",
            line_start,
            kind.label(),
            bytes_len,
        );
        if line_start >= bytes_len {
            break;
        }
        let next = engine.next_line_start(&bytes, bytes_len, line_start);
        prop_assert!(
            next >= line_start,
            "engine.next_line_start returned {} < line_start {} (encoding {})",
            next,
            line_start,
            kind.label(),
        );
        if next == line_start {
 // Would have stalled; advance manually so we still
 // visit the `file_len` terminator on the next round.
            line_start = bytes_len;
            continue;
        }
        line_start = next;
        iterations = iterations.saturating_add(1);
        prop_assert!(
            iterations <= cap,
            "next_line_start walk did not terminate within {} iterations (encoding {})",
            cap,
            kind.label(),
        );
    }

 // (c) Public-API surfaces: viewport
 // bounds, line bounds, and the full literal / regex search
 // surface. Each helper converts every `TextPosition` it
 // observes through `byte_offset_for_text_position` and asserts
 // the resulting byte offset is `Floor`-stable.
    check_viewport_and_line_bounds(doc, kind, bytes_len)?;
    check_literal_search_offsets(doc, kind, bytes_len)?;
    check_regex_search_offsets(doc, kind, bytes_len)?;

    Ok(())
}

/// Asserts Property 14 for every byte offset reachable through a
/// single `TextPosition` returned by a public API.
///
/// `byte_offset_for_text_position` is the same conversion the
/// search backends apply to `from` / `before` / range bounds before
/// they hand the offset to the literal-finder or regex chunker, so
/// a non-aligned result here would surface as a sub-character byte
/// offset feeding straight into one of those paths.
fn assert_text_position_aligned(
    doc: &Document,
    kind: EncodingKind,
    bytes_len: usize,
    position: TextPosition,
    surface: &str,
) -> Result<(), TestCaseError> {
    let offset = byte_offset_for_text_position(doc, position);
    prop_assert!(
        offset <= bytes_len,
        "{} produced offset {} > file_len {} (encoding {})",
        surface,
        offset,
        bytes_len,
        kind.label(),
    );
    prop_assert_eq!(
        align_byte_offset_floor(doc, offset),
        offset,
        "{} surfaced offset {} (from position {:?}) that is not Floor-stable \
         (encoding {}, file_len {})",
        surface,
        offset,
        position,
        kind.label(),
        bytes_len,
    );
    Ok(())
}

/// Asserts Property 14 for both endpoints of a `SearchMatch`.
fn assert_match_aligned(
    doc: &Document,
    kind: EncodingKind,
    bytes_len: usize,
    found: SearchMatch,
    surface: &str,
) -> Result<(), TestCaseError> {
    assert_text_position_aligned(doc, kind, bytes_len, found.start(), surface)?;
    assert_text_position_aligned(doc, kind, bytes_len, found.end(), surface)?;
    Ok(())
}

/// Asserts Property 14 for the public viewport API and the
/// per-line "line start" / "line end" bounds it reveals.
///
/// `Document::read_viewport` returns one `ViewportRow` per visible
/// line and is the canonical surface external editors use to render
/// the document. For each row we convert
/// `(line0, 0)` → byte offset (line start) and
/// `(line0, line_len_chars(line0))` → byte offset (line end without
/// trailing line break) and assert both are `Floor`-stable.
fn check_viewport_and_line_bounds(
    doc: &Document,
    kind: EncodingKind,
    bytes_len: usize,
) -> Result<(), TestCaseError> {
    let total_lines = doc.line_count().display_rows().max(1);
 // Read enough rows to cover the entire document. The seed
 // fixtures stay small under proptest shrinking, but a generous
 // upper bound keeps the assertion meaningful even when an edit
 // sequence grows the document past the seed's line count.
    let request = ViewportRequest::new(0, total_lines.saturating_add(2));
    let viewport = doc.read_viewport(request);

    for row in viewport.rows() {
        let line0 = row.line0();
        let start_pos = TextPosition::new(line0, 0);
        assert_text_position_aligned(doc, kind, bytes_len, start_pos, "read_viewport line start")?;

        let end_col = doc.line_len_chars(line0);
        let end_pos = TextPosition::new(line0, end_col);
        assert_text_position_aligned(doc, kind, bytes_len, end_pos, "read_viewport line end")?;
    }

 // Even when the viewport returns an empty row vec (e.g. a
 // freshly-truncated edit sequence emptied the document), the
 // first row's start at `(0, 0)` is still a public boundary
 // offset. Assert it explicitly so an empty-document regression
 // never silently skips this surface.
    assert_text_position_aligned(
        doc,
        kind,
        bytes_len,
        TextPosition::new(0, 0),
        "document origin",
    )?;
    Ok(())
}

/// Stable set of literal search needles. Mixing very short ASCII
/// needles with longer ones gives the literal byte-finder both an
/// easy match (high hit rate on Class A / Class B fixtures whose
/// seed text contains "anchor" / "tail") and a guaranteed-miss case
/// (`"\u{FFFD}"` is unrepresentable in every Class A / Class B
/// codec, and its UTF-8 byte sequence cannot occur in a UTF-16
/// stored document either, so the search backend returns `None`).
/// Either path must keep alignment; only the surfaces that *do*
/// return a match contribute new assertions.
const LITERAL_NEEDLES: &[&str] = &["a", "anchor", "tail", "\u{FFFD}"];

/// Asserts Property 14 for the entire public literal-search API.
fn check_literal_search_offsets(
    doc: &Document,
    kind: EncodingKind,
    bytes_len: usize,
) -> Result<(), TestCaseError> {
    let total_lines = doc.line_count().display_rows().max(1);
    let from = TextPosition::new(0, 0);
    let before = TextPosition::new(total_lines.saturating_add(2), 0);
    let mid = TextPosition::new(total_lines.saturating_sub(1), 0);
    let full_range = TextRange::new(from, usize::MAX / 4);

    for needle in LITERAL_NEEDLES {
 // find_next / find_prev / find_next_query / find_prev_query.
        if let Some(found) = doc.find_next(needle, from) {
            assert_match_aligned(doc, kind, bytes_len, found, "find_next")?;
        }
        if let Some(found) = doc.find_prev(needle, before) {
            assert_match_aligned(doc, kind, bytes_len, found, "find_prev")?;
        }
        if let Some(query) = LiteralSearchQuery::new(*needle) {
            if let Some(found) = doc.find_next_query(&query, from) {
                assert_match_aligned(doc, kind, bytes_len, found, "find_next_query")?;
            }
            if let Some(found) = doc.find_prev_query(&query, before) {
                assert_match_aligned(doc, kind, bytes_len, found, "find_prev_query")?;
            }

 // find_all / find_all_from / find_all_query / find_all_query_from.
            for found in doc.find_all(*needle) {
                assert_match_aligned(doc, kind, bytes_len, found, "find_all")?;
            }
            for found in doc.find_all_from(*needle, from) {
                assert_match_aligned(doc, kind, bytes_len, found, "find_all_from")?;
            }
            for found in doc.find_all_query(&query) {
                assert_match_aligned(doc, kind, bytes_len, found, "find_all_query")?;
            }
            for found in doc.find_all_query_from(&query, from) {
                assert_match_aligned(doc, kind, bytes_len, found, "find_all_query_from")?;
            }

 // find_all_in_range / find_all_between / their _query variants.
            for found in doc.find_all_in_range(*needle, full_range) {
                assert_match_aligned(doc, kind, bytes_len, found, "find_all_in_range")?;
            }
            for found in doc.find_all_between(*needle, from, before) {
                assert_match_aligned(doc, kind, bytes_len, found, "find_all_between")?;
            }
            for found in doc.find_all_query_in_range(&query, full_range) {
                assert_match_aligned(doc, kind, bytes_len, found, "find_all_query_in_range")?;
            }
            for found in doc.find_all_query_between(&query, from, before) {
                assert_match_aligned(doc, kind, bytes_len, found, "find_all_query_between")?;
            }

 // find_next_in_range / find_next_between / find_prev_in_range / find_prev_between.
            if let Some(found) = doc.find_next_in_range(needle, full_range) {
                assert_match_aligned(doc, kind, bytes_len, found, "find_next_in_range")?;
            }
            if let Some(found) = doc.find_next_between(needle, from, before) {
                assert_match_aligned(doc, kind, bytes_len, found, "find_next_between")?;
            }
            if let Some(found) = doc.find_prev_in_range(needle, full_range) {
                assert_match_aligned(doc, kind, bytes_len, found, "find_prev_in_range")?;
            }
            if let Some(found) = doc.find_prev_between(needle, from, before) {
                assert_match_aligned(doc, kind, bytes_len, found, "find_prev_between")?;
            }

 // find_next_query_in_range / find_next_query_between /
 // find_prev_query_in_range / find_prev_query_between.
            if let Some(found) = doc.find_next_query_in_range(&query, full_range) {
                assert_match_aligned(doc, kind, bytes_len, found, "find_next_query_in_range")?;
            }
            if let Some(found) = doc.find_next_query_between(&query, from, mid) {
                assert_match_aligned(doc, kind, bytes_len, found, "find_next_query_between")?;
            }
            if let Some(found) = doc.find_prev_query_in_range(&query, full_range) {
                assert_match_aligned(doc, kind, bytes_len, found, "find_prev_query_in_range")?;
            }
            if let Some(found) = doc.find_prev_query_between(&query, from, before) {
                assert_match_aligned(doc, kind, bytes_len, found, "find_prev_query_between")?;
            }
        }
    }
    Ok(())
}

/// Stable set of regex patterns. ASCII-only so they compile under
/// `RegexSearchQuery::new` regardless of the document's encoding.
/// The mix exercises both byte-finder fast paths (literal patterns
/// like `"anchor"`) and character-class patterns (`"[a-z]+"`,
/// `"\\w"`) which on UTF-16 / Class B documents route through the
/// chunked-decode + re-encode path. The
/// `"$"` pattern intentionally exercises empty-match advancement
/// inside `RegexSearchIter::next` so any zero-width match that
/// returns must still align on a character boundary.
const REGEX_PATTERNS: &[&str] = &["anchor", "[a-z]+", r"\w", "$"];

/// Asserts Property 14 for the entire public regex-search API.
fn check_regex_search_offsets(
    doc: &Document,
    kind: EncodingKind,
    bytes_len: usize,
) -> Result<(), TestCaseError> {
    let total_lines = doc.line_count().display_rows().max(1);
    let from = TextPosition::new(0, 0);
    let before = TextPosition::new(total_lines.saturating_add(2), 0);
    let mid = TextPosition::new(total_lines.saturating_sub(1), 0);
    let full_range = TextRange::new(from, usize::MAX / 4);

    for pattern in REGEX_PATTERNS {
        let Ok(query) = RegexSearchQuery::new(*pattern) else {
            continue;
        };

 // find_next_regex / find_prev_regex (one-shot pattern
 // helpers). Both return `Result<Option<SearchMatch>, _>`;
 // compile errors are unreachable here because the same
 // pattern compiled successfully into `query`, so we
 // unwrap the Result and only check the Option payload.
        if let Ok(Some(found)) = doc.find_next_regex(pattern, from) {
            assert_match_aligned(doc, kind, bytes_len, found, "find_next_regex")?;
        }
        if let Ok(Some(found)) = doc.find_prev_regex(pattern, before) {
            assert_match_aligned(doc, kind, bytes_len, found, "find_prev_regex")?;
        }

 // find_next_regex_query / find_prev_regex_query.
        if let Some(found) = doc.find_next_regex_query(&query, from) {
            assert_match_aligned(doc, kind, bytes_len, found, "find_next_regex_query")?;
        }
        if let Some(found) = doc.find_prev_regex_query(&query, before) {
            assert_match_aligned(doc, kind, bytes_len, found, "find_prev_regex_query")?;
        }

 // find_next_regex_in_range / find_next_regex_between.
        if let Ok(Some(found)) = doc.find_next_regex_in_range(pattern, full_range) {
            assert_match_aligned(doc, kind, bytes_len, found, "find_next_regex_in_range")?;
        }
        if let Ok(Some(found)) = doc.find_next_regex_between(pattern, from, before) {
            assert_match_aligned(doc, kind, bytes_len, found, "find_next_regex_between")?;
        }

 // find_next_regex_query_in_range / find_next_regex_query_between /
 // find_prev_regex_query_in_range / find_prev_regex_query_between.
        if let Some(found) = doc.find_next_regex_query_in_range(&query, full_range) {
            assert_match_aligned(
                doc,
                kind,
                bytes_len,
                found,
                "find_next_regex_query_in_range",
            )?;
        }
        if let Some(found) = doc.find_next_regex_query_between(&query, from, mid) {
            assert_match_aligned(doc, kind, bytes_len, found, "find_next_regex_query_between")?;
        }
        if let Some(found) = doc.find_prev_regex_query_in_range(&query, full_range) {
            assert_match_aligned(
                doc,
                kind,
                bytes_len,
                found,
                "find_prev_regex_query_in_range",
            )?;
        }
        if let Some(found) = doc.find_prev_regex_query_between(&query, from, before) {
            assert_match_aligned(doc, kind, bytes_len, found, "find_prev_regex_query_between")?;
        }

 // find_all_regex / find_all_regex_from / find_all_regex_query /
 // find_all_regex_query_from. The iterators advance internally
 // through the same backend the one-shot `find_next_regex_*`
 // helpers use, so this catches drift introduced by zero-width
 // match advancement (`$`, `(a*)`-style patterns).
        if let Ok(iter) = doc.find_all_regex(pattern) {
            for found in iter {
                assert_match_aligned(doc, kind, bytes_len, found, "find_all_regex")?;
            }
        }
        if let Ok(iter) = doc.find_all_regex_from(pattern, from) {
            for found in iter {
                assert_match_aligned(doc, kind, bytes_len, found, "find_all_regex_from")?;
            }
        }
        for found in doc.find_all_regex_query(&query) {
            assert_match_aligned(doc, kind, bytes_len, found, "find_all_regex_query")?;
        }
        for found in doc.find_all_regex_query_from(&query, from) {
            assert_match_aligned(doc, kind, bytes_len, found, "find_all_regex_query_from")?;
        }

 // find_all_regex_in_range / find_all_regex_between /
 // find_all_regex_query_in_range / find_all_regex_query_between.
        if let Ok(iter) = doc.find_all_regex_in_range(pattern, full_range) {
            for found in iter {
                assert_match_aligned(doc, kind, bytes_len, found, "find_all_regex_in_range")?;
            }
        }
        if let Ok(iter) = doc.find_all_regex_between(pattern, from, before) {
            for found in iter {
                assert_match_aligned(doc, kind, bytes_len, found, "find_all_regex_between")?;
            }
        }
        for found in doc.find_all_regex_query_in_range(&query, full_range) {
            assert_match_aligned(doc, kind, bytes_len, found, "find_all_regex_query_in_range")?;
        }
        for found in doc.find_all_regex_query_between(&query, from, before) {
            assert_match_aligned(doc, kind, bytes_len, found, "find_all_regex_query_between")?;
        }
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

 /// Property 14: every boundary offset returned by the encoding
 /// engine round-trips through `align_byte_offset(.., Backward)`
 /// unchanged after a sequence of representable edits over a
 /// non-UTF-8 document.
 ///
 /// The test:
 /// 1. Picks one of the eight non-UTF-8 encodings.
 /// 2. Materialises a small per-encoding seed fixture under
 /// `fresh_test_dir(...)`.
 /// 3. Opens it with the matching encoding contract. Class A
 /// and Class B fixtures land mmap-only; UTF-16 lands
 /// mmap-only too via `from_storage_class_b_native`.
 /// 4. Asserts Property 14 against the fresh document state
 /// (regression baseline for the open-only path).
 /// 5. Replays the random edit sequence through the public
 /// non-UTF-8 edit surface; every successful edit promotes
 /// the document to a piece-tree. Re-asserts Property 14
 /// after each edit.
 /// 6. Cleans up best-effort.
    #[test]
    fn property_14_engine_boundaries_are_floor_stable(
        (kind, edits) in case_strategy(),
    ) {
        let dir = fresh_test_dir("prop_alignment");
        let path = dir.join("seed.bin");
        let seed = kind.seed_bytes();
        std::fs::write(&path, &seed).expect("write seed fixture");

        let encoding = kind.document_encoding();
        let mut doc = Document::open_with_encoding(&path, encoding)
            .unwrap_or_else(|err| panic!("open_with_encoding({}) failed: {err:?}", kind.label()));

        prop_assert_eq!(
            doc.encoding(),
            encoding,
            "open_with_encoding must install the requested encoding contract for {}",
            kind.label(),
        );

 // Baseline: the freshly opened mmap-backed document already
 // surfaces line / char boundaries through the engine.
 // Property 14 must hold here before any edit lands.
        check_property_14(&doc, kind)?;

        for edit in &edits {
            apply_edit(&mut doc, edit, kind);
            check_property_14(&doc, kind)?;
        }

 // Best-effort cleanup; tmp files may linger on shrink failures.
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }
}
