// per-encoding integration suite.
//
// Class B encoding: UTF-16BE.
//
//
// This file ships the fixed four-test contract for the
// `UTF-16BE` Class B encoding. The structure mirrors `utf16_le.rs`
// exactly; the only difference is that each `u16` code unit is emitted
// in big-endian byte order. See the `utf16_le.rs` module header for a
// detailed description of every test contract; here we only restate the
// per-encoding specifics.
//
// The fifth contract `edit_and_save_round_trip` is
// : insert ASCII, insert mixed Cyrillic + CJK code points
// delete the inserted prefix through the encoded replace-range path
// save, reopen, and assert the decoded text round-trips byte-identically
//.
//
// Per WHATWG, `encoding_rs::UTF_16BE.encode()` redirects through UTF-8
// (UTF-16 is decode-only in the spec). The fixture is therefore
// hand-encoded by emitting big-endian 16-bit code units for every
// `u16` produced by `str::encode_utf16`.

#[path = "../mod.rs"]
#[allow(clippy::duplicate_mod)] // helpers module is also loaded by sibling per_encoding tests
mod helpers;

use helpers::fresh_test_dir;
use qem::{Document, DocumentEncoding, RegexSearchQuery, TextPosition};

const LINE0: &str = "TARGET line0";
const LINE4: &str = "last line";

const FIXTURE_TEXT: &str = "TARGET line0\nascii line1\r\n\n中文 текст\rlast line";

const EXPECTED_LINE_COUNT: usize = 5;

/// Hand-encodes `FIXTURE_TEXT` to UTF-16BE bytes. `encoding_rs::UTF_16BE.encode`
/// redirects through UTF-8 because the WHATWG Encoding Standard makes
/// UTF-16 decode-only, so the fixture emits big-endian 16-bit code
/// units directly via `str::encode_utf16`.
fn fixture_bytes() -> Vec<u8> {
    let mut encoded: Vec<u8> = Vec::with_capacity(FIXTURE_TEXT.len() * 2);
    for unit in FIXTURE_TEXT.encode_utf16() {
        encoded.extend_from_slice(&unit.to_be_bytes());
    }
    encoded
}

#[test]
fn opens_and_indexes_lines() {
    let dir = fresh_test_dir("per_encoding_utf16_be_open");
    let path = dir.join("fixture.txt");
    let bytes = fixture_bytes();
    std::fs::write(&path, &bytes).expect("write fixture");

    let encoding = DocumentEncoding::utf16be();
    let doc = Document::open_with_encoding(&path, encoding)
        .expect("open_with_encoding(UTF-16BE) must succeed for valid fixture");

    assert_eq!(
        doc.encoding(),
        encoding,
        "open_with_encoding must install the requested UTF-16BE contract",
    );
    assert_eq!(
        doc.line_count().exact(),
        Some(EXPECTED_LINE_COUNT),
        "Class B native open must produce an exact line count after open returns",
    );
}

#[test]
fn viewport_first_and_last_window() {
    let dir = fresh_test_dir("per_encoding_utf16_be_viewport");
    let path = dir.join("fixture.txt");
    let bytes = fixture_bytes();
    std::fs::write(&path, &bytes).expect("write fixture");

    let encoding = DocumentEncoding::utf16be();
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
    let dir = fresh_test_dir("per_encoding_utf16_be_search");
    let path = dir.join("fixture.txt");
    let bytes = fixture_bytes();
    std::fs::write(&path, &bytes).expect("write fixture");

    let encoding = DocumentEncoding::utf16be();
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
 // raw mmap bytes. UTF-16BE places a `0x00` filler byte before
 // every ASCII byte, so the contiguous UTF-8 needle bytes for
 // `"TARGET"` never appear in the source. The literal byte
 // finder therefore correctly returns `None` for an ASCII needle
 // in a UTF-16BE document. Encoding-aware literal search is not
 // in scope for ; this assertion documents the current
 // contract.
    assert!(
        doc.find_next("TARGET", TextPosition::new(0, 0)).is_none(),
        "literal byte-finder must not find a UTF-8 needle inside a UTF-16BE document",
    );
}

#[test]
fn save_round_trip_no_edits() {
    let dir = fresh_test_dir("per_encoding_utf16_be_save");
    let path = dir.join("fixture.txt");
    let saved = dir.join("fixture.saved.txt");
    let bytes = fixture_bytes();
    std::fs::write(&path, &bytes).expect("write fixture");

    let encoding = DocumentEncoding::utf16be();
    let mut doc = Document::open_with_encoding(&path, encoding).expect("open fixture");

    doc.save_to(&saved).expect("save_to must succeed");

    let saved_bytes = std::fs::read(&saved).expect("read saved file");
    assert_eq!(
        saved_bytes, bytes,
        "save_to without edits must produce byte-identical bytes for UTF-16BE native open",
    );
}

/// Representable non-ASCII insertion for the encoded edit-path test.
/// `Mix Привет 你好` mixes ASCII letters, Cyrillic and CJK Han glyphs.
/// Every scalar is representable in UTF-16 by definition, so the
/// encoded edit path emits two-byte BE code units for
/// every code point and never falls into the `UnrepresentableText`
/// branch.
const NON_ASCII_INSERT: &str = "Mix Привет 你好";

#[test]
fn edit_and_save_round_trip() {
 // — fifth contract for the per-encoding suite.
 // Validates (edit + save round-trip) and (save fidelity)
 // for `UTF-16BE` through the encoded edit path: insert
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
    let dir = fresh_test_dir("per_encoding_utf16_be_edit");
    let path = dir.join("fixture.txt");
    let saved = dir.join("fixture.saved.txt");
    let bytes = fixture_bytes();
    std::fs::write(&path, &bytes).expect("write fixture");

    let encoding = DocumentEncoding::utf16be();
    let mut doc = Document::open_with_encoding(&path, encoding).expect("open fixture");

 // Step 1 — ASCII insert at (0, 0). Column 0 is the only column the
 // encoded insert path resolves identically for every encoding
 // because `byte_offset_for_col(line0, 0)` returns the raw byte
 // offset of the line start regardless of cell width.
    doc.try_insert(TextPosition::new(0, 0), "EDIT ")
        .expect("ASCII insert must succeed for UTF-16BE encoded edit buffer");

 // Step 2 — engine-aware replace: the 5-text-unit ASCII prefix
 // installed in step 1 is replaced with the mixed Cyrillic + CJK
 // payload. `try_replace_range` resolves both endpoints through
 // `byte_offset_for_col_with_engine` so the replacement covers
 // exactly the 10 bytes encoding `"EDIT "` under UTF-16BE.
    doc.try_replace_range(0, 0, 5, NON_ASCII_INSERT)
        .expect("encoded replace-range must succeed for UTF-16BE");

 // Step 3 — engine-aware delete: drop the first text unit (the
 // leading `M` of the inserted payload). Length expressed in text
 // units, so `len_chars = 1` removes exactly one BMP cell (two
 // bytes) under UTF-16BE.
    doc.try_replace_range(0, 0, 1, "")
        .expect("encoded delete-range must succeed for UTF-16BE");

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
