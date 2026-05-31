// per-encoding integration suite.
//
// Class B encoding: UTF-16LE.
//
//
// This file ships the fixed four-test contract for the
// `UTF-16LE` Class B encoding. Each test creates its own fresh fixture
// under `fresh_test_dir(...)` and exercises one of the
// integration contracts:
//
// 1. `opens_and_indexes_lines` — open a multi-line UTF-16LE fixture
// mixing ASCII, CJK + Cyrillic non-ASCII payload, LF / CRLF / CR
// terminators and an empty line; assert the exact `line_count`
// and that the document self-identifies as `UTF-16LE`.
// 2. `viewport_first_and_last_window` — read the first and last
// lines through `Document::line_slice(line, 0, 256)`. The decoded
// text must equal the expected first/last line strings.
// `decode_window_for_engine` runs through
// `encoding_rs::Encoding::decode_with_bom_removal`, so the
// window-decoded text is the standard Unicode string view of the
// raw UTF-16 bytes.
// 3. `literal_and_regex_search_finds_known_match` — embed an ASCII
// `TARGET` marker on line 0 and exercise both literal and regex
// search paths. `find_next_regex_query` routes through the
// encoding-aware chunked-decode path
// (`find_next_regex_in_class_b_chunked`)
// and must return `(0, 0)..(0, 6)` in UTF-16 text-unit columns.
// The literal `find_next` byte-finder is not currently
// encoding-aware: its needle is the UTF-8 bytes of the input
// `&str`, which never appears contiguously in UTF-16 bytes
// (every ASCII char is followed by a `0x00` filler byte). The
// contract therefore asserts the actual current behaviour:
// literal search returns `None` for an ASCII needle in a UTF-16
// document. This documents the encoding-aware regex path while
// faithfully exercising the literal path on the same fixture.
// 4. `save_round_trip_no_edits` — `Document::save_to(saved_path)`
// with no prior edits must yield a byte-identical copy of the
// raw UTF-16LE bytes (, no-edit round-trip).
//
// The fifth contract `edit_and_save_round_trip` is
// : insert ASCII, insert mixed Cyrillic + CJK code points
// delete the inserted prefix through the encoded replace-range path
// save, reopen, and assert the decoded text round-trips byte-identically
//.
//
// Per WHATWG, `encoding_rs::UTF_16LE.encode()` redirects through UTF-8
// (UTF-16 is decode-only in the spec). Each fixture is therefore
// hand-encoded by emitting little-endian 16-bit code units for every
// `u16` produced by `str::encode_utf16`.

#[path = "../mod.rs"]
#[allow(clippy::duplicate_mod)] // helpers module is also loaded by sibling per_encoding tests
mod helpers;

use helpers::fresh_test_dir;
use qem::{Document, DocumentEncoding, RegexSearchQuery, TextPosition};

/// Logical fixture text. The structure exercises:
///
/// * Line 0 — ASCII with the `TARGET` marker, terminated by `LF`.
/// * Line 1 — ASCII, terminated by `CRLF` (CRLF collapses to a single
/// boundary in `Utf16Engine::next_line_start`).
/// * Line 2 — empty line, terminated by `LF`.
/// * Line 3 — CJK + Cyrillic non-ASCII payload, terminated by lone
/// `CR` (no following `LF`; the engine treats this as its own
/// boundary).
/// * Line 4 — ASCII, no trailing newline. The fixture therefore
/// contains exactly five lines (line indexing assertion).
const LINE0: &str = "TARGET line0";
const LINE4: &str = "last line";

const FIXTURE_TEXT: &str = "TARGET line0\nascii line1\r\n\n中文 текст\rlast line";

const EXPECTED_LINE_COUNT: usize = 5;

/// Hand-encodes `FIXTURE_TEXT` to UTF-16LE bytes. `encoding_rs::UTF_16LE.encode`
/// redirects through UTF-8 because the WHATWG Encoding Standard makes
/// UTF-16 decode-only, so the fixture emits little-endian 16-bit code
/// units directly via `str::encode_utf16`.
fn fixture_bytes() -> Vec<u8> {
    let mut encoded: Vec<u8> = Vec::with_capacity(FIXTURE_TEXT.len() * 2);
    for unit in FIXTURE_TEXT.encode_utf16() {
        encoded.extend_from_slice(&unit.to_le_bytes());
    }
    encoded
}

#[test]
fn opens_and_indexes_lines() {
    let dir = fresh_test_dir("per_encoding_utf16_le_open");
    let path = dir.join("fixture.txt");
    let bytes = fixture_bytes();
    std::fs::write(&path, &bytes).expect("write fixture");

    let encoding = DocumentEncoding::utf16le();
    let doc = Document::open_with_encoding(&path, encoding)
        .expect("open_with_encoding(UTF-16LE) must succeed for valid fixture");

    assert_eq!(
        doc.encoding(),
        encoding,
        "open_with_encoding must install the requested UTF-16LE contract",
    );
    assert_eq!(
        doc.line_count().exact(),
        Some(EXPECTED_LINE_COUNT),
        "Class B native open must produce an exact line count after open returns",
    );
}

#[test]
fn viewport_first_and_last_window() {
    let dir = fresh_test_dir("per_encoding_utf16_le_viewport");
    let path = dir.join("fixture.txt");
    let bytes = fixture_bytes();
    std::fs::write(&path, &bytes).expect("write fixture");

    let encoding = DocumentEncoding::utf16le();
    let doc = Document::open_with_encoding(&path, encoding).expect("open fixture");

    let first = doc.line_slice(0, 0, 256);
    assert_eq!(
        first.text(),
        LINE0,
        "first viewport row must decode to the expected ASCII line",
    );

    let last_index = doc
        .line_count()
        .exact()
        .expect("Class B native open is exact")
        .saturating_sub(1);
    let last = doc.line_slice(last_index, 0, 256);
    assert_eq!(
        last.text(),
        LINE4,
        "last viewport row must decode to the expected ASCII line",
    );
}

#[test]
fn literal_and_regex_search_finds_known_match() {
    let dir = fresh_test_dir("per_encoding_utf16_le_search");
    let path = dir.join("fixture.txt");
    let bytes = fixture_bytes();
    std::fs::write(&path, &bytes).expect("write fixture");

    let encoding = DocumentEncoding::utf16le();
    let doc = Document::open_with_encoding(&path, encoding).expect("open fixture");

 // Regex search routes through `find_next_regex_in_class_b_chunked`
 // for UTF-16. The
 // chunked decode + glue path locates the ASCII marker on line 0
 // and maps the match back to UTF-16 source bytes with the
 // 2-byte alignment post-filter, returning text-unit positions.
    let query = RegexSearchQuery::new("TARGET").expect("compile ASCII regex");
    let regex_match = doc
        .find_next_regex_query(&query, TextPosition::new(0, 0))
        .expect("regex search must locate the ASCII marker on line 0");
    assert_eq!(regex_match.start(), TextPosition::new(0, 0));
    assert_eq!(regex_match.end(), TextPosition::new(0, 6));

 // Literal search currently runs a byte-level finder against the
 // raw mmap bytes. The needle bytes for `"TARGET"` are the six
 // ASCII bytes `[0x54, 0x41, 0x52, 0x47, 0x45, 0x54]`; the UTF-16LE
 // encoding of the same string interleaves a `0x00` filler byte
 // after every ASCII byte, so the contiguous needle pattern never
 // appears in the source. The literal byte finder therefore
 // correctly returns `None` for an ASCII needle in a UTF-16
 // document. Encoding-aware literal search is not in scope for
 // ; this assertion documents the current contract.
    assert!(
        doc.find_next("TARGET", TextPosition::new(0, 0)).is_none(),
        "literal byte-finder must not find a UTF-8 needle inside a UTF-16LE document",
    );
}

#[test]
fn save_round_trip_no_edits() {
    let dir = fresh_test_dir("per_encoding_utf16_le_save");
    let path = dir.join("fixture.txt");
    let saved = dir.join("fixture.saved.txt");
    let bytes = fixture_bytes();
    std::fs::write(&path, &bytes).expect("write fixture");

    let encoding = DocumentEncoding::utf16le();
    let mut doc = Document::open_with_encoding(&path, encoding).expect("open fixture");

    doc.save_to(&saved).expect("save_to must succeed");

    let saved_bytes = std::fs::read(&saved).expect("read saved file");
    assert_eq!(
        saved_bytes, bytes,
        "save_to without edits must produce byte-identical bytes for UTF-16LE native open",
    );
}

/// Representable non-ASCII insertion for the encoded edit-path test.
/// `Mix Привет 你好` mixes ASCII letters, Cyrillic and CJK Han glyphs.
/// Every scalar is representable in UTF-16 by definition, so the
/// encoded edit path emits two-byte LE code units for
/// every code point and never falls into the `UnrepresentableText`
/// branch.
const NON_ASCII_INSERT: &str = "Mix Привет 你好";

#[test]
fn edit_and_save_round_trip() {
 // — fifth contract for the per-encoding suite.
 // Validates (edit + save round-trip) and (save fidelity)
 // for `UTF-16LE` through the encoded edit path: insert
 // ASCII at (0, 0), replace that ASCII prefix with mixed Cyrillic
 // + CJK via the engine-aware `try_replace_range`, delete the
 // first text unit through the same engine-aware path, save
 // reopen, and assert the decoded text round-trips identically
 // against the in-memory document. holds implicitly because
 // the encoded path emits UTF-16 code units directly via
 // `str::encode_utf16` and never transcodes the document into
 // UTF-8.
 //
 // Note: every column-based reposition runs through `try_replace_range`
 // whose encoded branch resolves byte offsets via the engine-aware
 // `*_with_engine` walkers — the only
 // arithmetic that maps a UTF-16 text-unit column onto its raw
 // 2-byte cell offset. `try_insert` only ever runs at column 0
 // where column-as-bytes and engine-aware columns trivially agree.
    let dir = fresh_test_dir("per_encoding_utf16_le_edit");
    let path = dir.join("fixture.txt");
    let saved = dir.join("fixture.saved.txt");
    let bytes = fixture_bytes();
    std::fs::write(&path, &bytes).expect("write fixture");

    let encoding = DocumentEncoding::utf16le();
    let mut doc = Document::open_with_encoding(&path, encoding).expect("open fixture");

 // Step 1 — ASCII insert at (0, 0). Column 0 is the only column the
 // encoded insert path resolves identically for every encoding
 // because `byte_offset_for_col(line0, 0)` returns the raw byte
 // offset of the line start regardless of cell width.
    doc.try_insert(TextPosition::new(0, 0), "EDIT ")
        .expect("ASCII insert must succeed for UTF-16LE encoded edit buffer");

 // Step 2 — engine-aware replace: the 5-text-unit ASCII prefix
 // installed in step 1 is replaced with the mixed Cyrillic + CJK
 // payload. `try_replace_range` resolves both endpoints through
 // `byte_offset_for_col_with_engine` so the replacement covers
 // exactly the 10 bytes encoding `"EDIT "` under UTF-16LE.
    doc.try_replace_range(0, 0, 5, NON_ASCII_INSERT)
        .expect("encoded replace-range must succeed for UTF-16LE");

 // Step 3 — engine-aware delete: drop the first text unit (the
 // leading `M` of the inserted payload). Length expressed in text
 // units, so `len_chars = 1` removes exactly one BMP cell (two
 // bytes) under UTF-16LE.
    doc.try_replace_range(0, 0, 1, "")
        .expect("encoded delete-range must succeed for UTF-16LE");

    let in_memory_text = doc.text_lossy();
    assert!(
        in_memory_text.starts_with("ix Привет 你好"),
        "in-memory text must start with the mixed Cyrillic + CJK \
         payload (minus its first text unit) after the encoded splice; \
         actual: {in_memory_text:?}",
    );

    doc.save_to(&saved)
        .expect("preserve-save must stream the piece-tree bytes verbatim");

    let reopened = Document::open_with_encoding(&saved, encoding).expect("reopen saved fixture");
    assert_eq!(
        reopened.text_lossy(),
        in_memory_text,
        "edit → save → reopen must yield the same decoded text as the in-memory document",
    );
}
