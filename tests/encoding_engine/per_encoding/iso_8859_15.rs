// per-encoding integration suite.
//
// Class A encoding: ISO-8859-15 (Latin-9; Western European with euro sign).
//
//
// This file ships the fixed four-test contract for
// the `ISO-8859-15` Class A encoding. Each test creates its own fresh
// fixture under `fresh_test_dir(...)` and exercises one of the
// integration contracts (open + line indexing, viewport read, literal
// + regex search, save round-trip without edits).
//
// Unlike `ISO-8859-1`, the WHATWG Encoding Standard does **not** alias
// `ISO-8859-15` to `windows-1252`: it remains its own encoding because
// of the euro sign at byte 0xA4 and other Latin-9-specific glyphs that
// differ from windows-1252's mapping.
//
// The fifth contract `edit_and_save_round_trip` is
//.

#[path = "../mod.rs"]
#[allow(clippy::duplicate_mod)] // helpers module is also loaded by sibling per_encoding tests
mod helpers;

use encoding_rs::ISO_8859_15;
use helpers::fresh_test_dir;
use qem::{Document, DocumentEncoding, RegexSearchQuery, TextPosition};

/// Canonical `encoding_rs` label for this module.
const LABEL: &str = "ISO-8859-15";

const LINE0: &str = "TARGET line0";
const LINE1: &str = "ascii line1";
/// Line containing the encoding-specific non-ASCII payload. The euro
/// sign `€` (U+20AC) lives at byte 0xA4 under `ISO-8859-15` and is
/// the canonical Latin-9 marker. `é` (U+00E9) covers the standard
/// Western European glyph range too.
const LINE2_TEXT: &str = "café €";
const LINE3: &str = "last line";

const EXPECTED_LINE_COUNT: usize = 4;

fn fixture_bytes() -> Vec<u8> {
    let text = format!("{LINE0}\n{LINE1}\r\n{LINE2_TEXT}\n{LINE3}");
    let (encoded, used, had_errors) = ISO_8859_15.encode(&text);
    assert_eq!(used, ISO_8859_15, "fixture must round-trip through {LABEL}");
    assert!(!had_errors, "fixture text must be representable in {LABEL}");
    encoded.into_owned()
}

#[test]
fn opens_and_indexes_lines() {
    let dir = fresh_test_dir("per_encoding_iso_8859_15_open");
    let path = dir.join("fixture.txt");
    let bytes = fixture_bytes();
    std::fs::write(&path, &bytes).expect("write fixture");

    let encoding = DocumentEncoding::from_label(LABEL).unwrap();
    let doc = Document::open_with_encoding(&path, encoding)
        .expect("open_with_encoding(ISO-8859-15) must succeed for valid fixture");

    assert_eq!(doc.encoding(), encoding);
    assert_eq!(
        doc.line_count().exact(),
        Some(EXPECTED_LINE_COUNT),
        "Class A native open must produce an exact line count after open returns",
    );
}

#[test]
fn viewport_first_and_last_window() {
    let dir = fresh_test_dir("per_encoding_iso_8859_15_viewport");
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
    let dir = fresh_test_dir("per_encoding_iso_8859_15_search");
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
    let dir = fresh_test_dir("per_encoding_iso_8859_15_save");
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
/// `café €` exercises both the standard Latin-9 glyph range and the
/// euro sign at byte 0xA4 that distinguishes ISO-8859-15 from
/// windows-1252.
const NON_ASCII_INSERT: &str = "café €";

#[test]
fn edit_and_save_round_trip() {
 // — fifth contract. Validates (edit + save
 // round-trip) and (save fidelity) for `ISO-8859-15` through
 // the encoded edit path: insert ASCII, insert representable
 // non-ASCII (including the euro sign), delete a range, save
 // reopen, and assert the decoded text round-trips byte-identically
 // against the in-memory document.
    let dir = fresh_test_dir("per_encoding_iso_8859_15_edit");
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
