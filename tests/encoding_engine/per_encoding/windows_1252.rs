// per-encoding integration suite.
//
// Class A encoding: windows-1252 (Western European single-byte).
//
//
// This file ships the fixed four-test contract for the
// `windows-1252` Class A encoding. Each test creates its own fresh
// fixture under `fresh_test_dir(...)` and exercises one of the
// integration contracts:
//
// 1. `opens_and_indexes_lines` — open a small fixture (ASCII + at
// least one non-ASCII byte that is valid in `windows-1252`, mix of
// LF / CRLF terminators) and assert the exact `line_count` and
// that the document self-identifies as `windows-1252`.
// 2. `viewport_first_and_last_window` — read the first and last
// lines through `Document::line_slice(line, 0, 256)` and assert
// decoded text equals the expected first/last line strings.
// 3. `literal_and_regex_search_finds_known_match` — both
// `Document::find_next("TARGET", ..)` and the compiled
// `RegexSearchQuery` regex equivalent must find the ASCII marker
// embedded in the fixture.
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

use encoding_rs::WINDOWS_1252;
use helpers::fresh_test_dir;
use qem::{Document, DocumentEncoding, RegexSearchQuery, TextPosition};

/// Canonical encoding label for this module.
const LABEL: &str = "windows-1252";

const LINE0: &str = "TARGET line0";
const LINE1: &str = "ascii line1";
/// Line containing the encoding-specific non-ASCII payload. `café` is
/// the canonical Western European test string: `é` (U+00E9) sits at the
/// 0xE9 byte under `windows-1252` and round-trips cleanly.
const LINE2_TEXT: &str = "café";
const LINE3: &str = "last line";

const EXPECTED_LINE_COUNT: usize = 4;

fn fixture_bytes() -> Vec<u8> {
    let text = format!("{LINE0}\n{LINE1}\r\n{LINE2_TEXT}\n{LINE3}");
    let (encoded, used, had_errors) = WINDOWS_1252.encode(&text);
    assert_eq!(
        used, WINDOWS_1252,
        "fixture must round-trip through {LABEL}"
    );
    assert!(!had_errors, "fixture text must be representable in {LABEL}");
    encoded.into_owned()
}

#[test]
fn opens_and_indexes_lines() {
    let dir = fresh_test_dir("per_encoding_windows_1252_open");
    let path = dir.join("fixture.txt");
    let bytes = fixture_bytes();
    std::fs::write(&path, &bytes).expect("write fixture");

    let encoding = DocumentEncoding::from_label(LABEL).unwrap();
    let doc = Document::open_with_encoding(&path, encoding)
        .expect("open_with_encoding(windows-1252) must succeed for valid fixture");

    assert_eq!(doc.encoding(), encoding);
    assert_eq!(
        doc.line_count().exact(),
        Some(EXPECTED_LINE_COUNT),
        "Class A native open must produce an exact line count after open returns",
    );
}

#[test]
fn viewport_first_and_last_window() {
    let dir = fresh_test_dir("per_encoding_windows_1252_viewport");
    let path = dir.join("fixture.txt");
    let bytes = fixture_bytes();
    std::fs::write(&path, &bytes).expect("write fixture");

    let encoding = DocumentEncoding::from_label(LABEL).unwrap();
    let doc = Document::open_with_encoding(&path, encoding).expect("open fixture");

    let first = doc.line_slice(0, 0, 256);
    assert_eq!(first.text(), LINE0);

    let last_index = doc
        .line_count()
        .exact()
        .expect("Class A native open is exact")
        .saturating_sub(1);
    let last = doc.line_slice(last_index, 0, 256);
    assert_eq!(last.text(), LINE3);
}

#[test]
fn literal_and_regex_search_finds_known_match() {
    let dir = fresh_test_dir("per_encoding_windows_1252_search");
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
    let dir = fresh_test_dir("per_encoding_windows_1252_save");
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
/// `café` keeps the test string short while exercising the high-byte
/// `é` (U+00E9 → 0xE9) cell that is canonical for windows-1252.
const NON_ASCII_INSERT: &str = "café";

#[test]
fn edit_and_save_round_trip() {
 // — fifth contract. Validates (edit + save
 // round-trip) and (save fidelity) for `windows-1252` through
 // the encoded edit path: insert ASCII, insert representable
 // non-ASCII, delete a range, save, reopen, and assert the decoded
 // text round-trips byte-identically against the in-memory document.
    let dir = fresh_test_dir("per_encoding_windows_1252_edit");
    let path = dir.join("fixture.txt");
    let saved = dir.join("fixture.saved.txt");
    let bytes = fixture_bytes();
    std::fs::write(&path, &bytes).expect("write fixture");

    let encoding = DocumentEncoding::from_label(LABEL).unwrap();
    let mut doc = Document::open_with_encoding(&path, encoding).expect("open fixture");

    doc.try_insert(TextPosition::new(0, 0), "EDIT ")
        .expect("ASCII insert must succeed");
    doc.try_insert(TextPosition::new(0, 5), NON_ASCII_INSERT)
        .expect("non-ASCII insert must succeed");
    doc.try_replace_range(0, 0, 5, "")
        .expect("encoded delete-range must succeed");

    let in_memory_text = doc.text_lossy();
    assert!(
        in_memory_text.starts_with(NON_ASCII_INSERT),
        "in-memory text must start with the non-ASCII insertion after the encoded splice",
    );

    doc.save_to(&saved).expect("preserve-save must succeed");

    let reopened = Document::open_with_encoding(&saved, encoding).expect("reopen saved fixture");
    assert_eq!(
        reopened.text_lossy(),
        in_memory_text,
        "edit → save → reopen must yield the same decoded text as the in-memory document",
    );
}
