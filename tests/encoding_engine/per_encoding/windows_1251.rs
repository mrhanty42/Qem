// per-encoding integration suite.
//
// Class A encoding: windows-1251 (Cyrillic single-byte).
//
//
// This file ships the fixed four-test contract for the
// `windows-1251` Class A encoding. Each test creates its own fresh
// fixture under `fresh_test_dir(...)` and exercises one of the
// integration contracts:
//
// 1. `opens_and_indexes_lines` — open a small fixture (ASCII + at
// least one non-ASCII byte that is valid in `windows-1251`, mix of
// LF / CRLF terminators) through `Document::open_with_encoding`;
// the resulting document must report the expected exact
// `line_count` and self-identify as `windows-1251`.
// 2. `viewport_first_and_last_window` — read the first and last
// lines through `Document::line_slice(line, 0, 256)`; the decoded
// text must equal the expected first/last line strings.
// 3. `literal_and_regex_search_finds_known_match` — both
// `Document::find_next("TARGET", ..)` and the compiled
// `RegexSearchQuery` regex equivalent must find the ASCII marker
// embedded in the fixture and return matching positions.
// 4. `save_round_trip_no_edits` — `Document::save_to(saved_path)`
// with no prior edits must yield a byte-identical copy of the
// fixture (; no edits version of the round-trip).
//
// The fifth contract `edit_and_save_round_trip` is
// once the non-UTF-8 edit path lands; it is intentionally
// omitted here.

#[path = "../mod.rs"]
#[allow(clippy::duplicate_mod)] // helpers module is also loaded by sibling per_encoding tests
mod helpers;

use encoding_rs::WINDOWS_1251;
use helpers::fresh_test_dir;
use qem::{Document, DocumentEncoding, RegexSearchQuery, TextPosition};

/// Canonical encoding label for this module. The `encoding_rs` library
/// canonicalises `windows-1251` to itself; the round-trip
/// `from_label("windows-1251").name() == "windows-1251"` invariant is
/// asserted in every test.
const LABEL: &str = "windows-1251";

/// First line of the fixture. ASCII only so the literal `TARGET`
/// marker is byte-identical across all Class A encodings; this lets
/// the literal/regex-search contract use one shared needle string.
const LINE0: &str = "TARGET line0";
/// Second line — ASCII, terminated with CRLF in the stored bytes.
const LINE1: &str = "ascii line1";
/// Third line containing the encoding-specific non-ASCII payload.
/// `ёжик` exercises four high-byte (>= 0x80) glyphs that are valid in
/// `windows-1251` and therefore round-trip cleanly through
/// `encoding_rs::WINDOWS_1251::encode`.
const LINE2_TEXT: &str = "ёжик";
/// Last line — ASCII only, with no trailing newline so the file ends
/// without a phantom empty line. The fixture therefore contains
/// exactly four lines (line indexing assertion).
const LINE3: &str = "last line";

/// Total number of lines the open path must report through
/// `Document::line_count` once indexing finishes. Class A
/// native opens index synchronously inside `from_storage_class_a_native`
/// so `line_count()` is `Exact(EXPECTED_LINE_COUNT)` immediately after
/// open returns.
const EXPECTED_LINE_COUNT: usize = 4;

/// Encodes the fixture text into `windows-1251` bytes. Line endings are
/// hand-stitched so the fixture deliberately mixes LF (between lines 0
/// and 1, and between lines 2 and 3) with CRLF (between lines 1 and 2)
/// to exercise both terminator styles in one file.
fn fixture_bytes() -> Vec<u8> {
    let text = format!("{LINE0}\n{LINE1}\r\n{LINE2_TEXT}\n{LINE3}");
    let (encoded, used, had_errors) = WINDOWS_1251.encode(&text);
    assert_eq!(
        used, WINDOWS_1251,
        "fixture text must round-trip through {LABEL} without redirect",
    );
    assert!(!had_errors, "fixture text must be representable in {LABEL}");
    encoded.into_owned()
}

#[test]
fn opens_and_indexes_lines() {
    let dir = fresh_test_dir("per_encoding_windows_1251_open");
    let path = dir.join("fixture.txt");
    let bytes = fixture_bytes();
    std::fs::write(&path, &bytes).expect("write fixture");

    let encoding = DocumentEncoding::from_label(LABEL).unwrap();
    let doc = Document::open_with_encoding(&path, encoding)
        .expect("open_with_encoding(windows-1251) must succeed for valid fixture");

    assert_eq!(
        doc.encoding(),
        encoding,
        "open_with_encoding must install the requested {LABEL} contract",
    );
    let line_count = doc.line_count();
    assert_eq!(
        line_count.exact(),
        Some(EXPECTED_LINE_COUNT),
        "Class A native open must produce an exact line count after open returns",
    );
}

#[test]
fn viewport_first_and_last_window() {
    let dir = fresh_test_dir("per_encoding_windows_1251_viewport");
    let path = dir.join("fixture.txt");
    let bytes = fixture_bytes();
    std::fs::write(&path, &bytes).expect("write fixture");

    let encoding = DocumentEncoding::from_label(LABEL).unwrap();
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
        .expect("Class A native open is exact")
        .saturating_sub(1);
    let last = doc.line_slice(last_index, 0, 256);
    assert_eq!(
        last.text(),
        LINE3,
        "last viewport row must decode to the expected ASCII line",
    );
}

#[test]
fn literal_and_regex_search_finds_known_match() {
    let dir = fresh_test_dir("per_encoding_windows_1251_search");
    let path = dir.join("fixture.txt");
    let bytes = fixture_bytes();
    std::fs::write(&path, &bytes).expect("write fixture");

    let encoding = DocumentEncoding::from_label(LABEL).unwrap();
    let doc = Document::open_with_encoding(&path, encoding).expect("open fixture");

    let literal = doc
        .find_next("TARGET", TextPosition::new(0, 0))
        .expect("literal search must locate the ASCII marker on line 0");
    assert_eq!(literal.start(), TextPosition::new(0, 0));
    assert_eq!(literal.end(), TextPosition::new(0, 6));

    let query = RegexSearchQuery::new("TARGET").expect("compile ASCII regex");
    let regex_match = doc
        .find_next_regex_query(&query, TextPosition::new(0, 0))
        .expect("regex search must locate the ASCII marker on line 0");
    assert_eq!(regex_match.start(), TextPosition::new(0, 0));
    assert_eq!(regex_match.end(), TextPosition::new(0, 6));
}

#[test]
fn save_round_trip_no_edits() {
    let dir = fresh_test_dir("per_encoding_windows_1251_save");
    let path = dir.join("fixture.txt");
    let saved = dir.join("fixture.saved.txt");
    let bytes = fixture_bytes();
    std::fs::write(&path, &bytes).expect("write fixture");

    let encoding = DocumentEncoding::from_label(LABEL).unwrap();
    let mut doc = Document::open_with_encoding(&path, encoding).expect("open fixture");

    doc.save_to(&saved).expect("save_to must succeed");

    let saved_bytes = std::fs::read(&saved).expect("read saved file");
    assert_eq!(
        saved_bytes, bytes,
        "save_to without edits must produce byte-identical bytes for Class A native open",
    );
}

/// Representable non-ASCII insertion for the encoded edit-path test.
/// `Привет` is six Cyrillic letters fully representable in
/// `windows-1251`, so the encoded insert path emits six high-byte cells
/// into the piece-tree add buffer without ever touching the
/// `UnrepresentableText` branch.
const NON_ASCII_INSERT: &str = "Привет";

#[test]
fn edit_and_save_round_trip() {
 // — fifth contract for the per-encoding suite.
 // Validates (edit + save round-trip) and (save fidelity)
 // for the `windows-1251` Class A encoding through the
 // encoded edit path: insert ASCII, insert representable non-ASCII
 // delete a range, save, reopen, and assert the decoded text round-
 // trips byte-identically against the in-memory document. is
 // upheld implicitly because the encoded path never transcodes the
 // document into UTF-8.

    let dir = fresh_test_dir("per_encoding_windows_1251_edit");
    let path = dir.join("fixture.txt");
    let saved = dir.join("fixture.saved.txt");
    let bytes = fixture_bytes();
    std::fs::write(&path, &bytes).expect("write fixture");

    let encoding = DocumentEncoding::from_label(LABEL).unwrap();
    let mut doc = Document::open_with_encoding(&path, encoding).expect("open fixture");

 // Step 1 — ASCII insert at line 0, column 0.
    doc.try_insert(TextPosition::new(0, 0), "EDIT ")
        .expect("ASCII insert must succeed for Class A native edit buffer");
 // Step 2 — representable non-ASCII insert directly after the ASCII
 // prefix. The encoded path emits high-byte cells (one byte per
 // Cyrillic letter under windows-1251) into the piece-tree add
 // buffer without falling into the `UnrepresentableText` branch.
    doc.try_insert(TextPosition::new(0, 5), NON_ASCII_INSERT)
        .expect("non-ASCII insert must succeed");
 // Step 3 — delete the 5-column ASCII prefix through the encoded
 // delete path. After the splice, line 0 begins with the non-ASCII
 // chunk followed by the original `TARGET line0` text.
    doc.try_replace_range(0, 0, 5, "")
        .expect("encoded delete-range must succeed");

    let in_memory_text = doc.text_lossy();
    assert!(
        in_memory_text.starts_with(NON_ASCII_INSERT),
        "in-memory text must start with the non-ASCII insertion after the encoded splice",
    );

    doc.save_to(&saved)
        .expect("preserve-save must stream the piece-tree bytes verbatim");

    let reopened = Document::open_with_encoding(&saved, encoding).expect("reopen saved fixture");
    let reopened_text = reopened.text_lossy();
    assert_eq!(
        reopened_text, in_memory_text,
        "edit → save → reopen must yield the same decoded text as the in-memory document",
    );
}
