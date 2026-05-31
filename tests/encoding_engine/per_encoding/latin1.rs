// per-encoding integration suite.
//
// Class A encoding: ISO-8859-1 (Latin-1).
//
//
// Special note on `encoding_rs` aliasing:
//
// The WHATWG Encoding Standard — and therefore `encoding_rs` —
// intentionally aliases the legacy label `ISO-8859-1` to
// `windows-1252`. `DocumentEncoding::from_label("ISO-8859-1")` returns
// the `windows-1252` encoding object, and that encoding's canonical
// `name()` is the string `"windows-1252"`, not `"ISO-8859-1"`. This
// mirrors how every modern browser interprets the label and is the
// contract Qem inherits from `encoding_rs`. The four-test contract
// below therefore asserts `doc.encoding() == "windows-1252"` after
// opening with `from_label("ISO-8859-1")`: that is the canonical
// alias and the only correct outcome under the WHATWG spec.
//
// This file ships the fixed four-test contract for
// the `ISO-8859-1` label. Each test creates its own fresh fixture
// under `fresh_test_dir(...)` and exercises one of the
// integration contracts (open + line indexing, viewport read, literal
// + regex search, save round-trip without edits).
//
// The fifth contract `edit_and_save_round_trip` is
//.

#[path = "../mod.rs"]
#[allow(clippy::duplicate_mod)] // helpers module is also loaded by sibling per_encoding tests
mod helpers;

// `encoding_rs` aliases the `ISO-8859-1` label to `windows-1252`, so
// the fixture is encoded through the canonical `WINDOWS_1252` constant.
// This matches what `DocumentEncoding::from_label("ISO-8859-1")`
// installs on the document at open time.
use encoding_rs::WINDOWS_1252;
use helpers::fresh_test_dir;
use qem::{Document, DocumentEncoding, RegexSearchQuery, TextPosition};

/// Label the caller asks for. Per the WHATWG Encoding Standard
/// `encoding_rs` canonicalises this to `windows-1252`; the canonical
/// alias is what gets installed on the document.
const REQUESTED_LABEL: &str = "ISO-8859-1";
/// Canonical alias `encoding_rs` returns for the `ISO-8859-1` label.
const CANONICAL_LABEL: &str = "windows-1252";

const LINE0: &str = "TARGET line0";
const LINE1: &str = "ascii line1";
/// Western European text containing characters representable across
/// both the WHATWG `ISO-8859-1` alias (windows-1252) and the original
/// ISO-8859-1 repertoire: `é` (U+00E9), `à` (U+00E0), `ñ` (U+00F1)
/// `ü` (U+00FC).
const LINE2_TEXT: &str = "café à ñü";
const LINE3: &str = "last line";

const EXPECTED_LINE_COUNT: usize = 4;

fn fixture_bytes() -> Vec<u8> {
    let text = format!("{LINE0}\n{LINE1}\r\n{LINE2_TEXT}\n{LINE3}");
    let (encoded, used, had_errors) = WINDOWS_1252.encode(&text);
    assert_eq!(
        used, WINDOWS_1252,
        "fixture must round-trip through the {REQUESTED_LABEL} alias",
    );
    assert!(
        !had_errors,
        "fixture text must be representable in the {REQUESTED_LABEL} alias",
    );
    encoded.into_owned()
}

#[test]
fn opens_and_indexes_lines() {
    let dir = fresh_test_dir("per_encoding_latin1_open");
    let path = dir.join("fixture.txt");
    let bytes = fixture_bytes();
    std::fs::write(&path, &bytes).expect("write fixture");

    let requested = DocumentEncoding::from_label(REQUESTED_LABEL).unwrap();
    let doc = Document::open_with_encoding(&path, requested)
        .expect("open_with_encoding(ISO-8859-1) must succeed for valid fixture");

 // `encoding_rs` aliases `ISO-8859-1` to `windows-1252`. The
 // installed contract reflects the canonical alias rather than the
 // requested label string; the document still self-reports as
 // equal to the encoding the caller passed in (the alias and the
 // requested encoding share `&'static Encoding`).
    assert_eq!(doc.encoding(), requested);
    assert_eq!(
        doc.encoding().name(),
        CANONICAL_LABEL,
        "ISO-8859-1 must canonicalise to {CANONICAL_LABEL} per the WHATWG Encoding Standard",
    );
    assert_eq!(
        doc.line_count().exact(),
        Some(EXPECTED_LINE_COUNT),
        "Class A native open must produce an exact line count after open returns",
    );
}

#[test]
fn viewport_first_and_last_window() {
    let dir = fresh_test_dir("per_encoding_latin1_viewport");
    let path = dir.join("fixture.txt");
    let bytes = fixture_bytes();
    std::fs::write(&path, &bytes).expect("write fixture");

    let requested = DocumentEncoding::from_label(REQUESTED_LABEL).unwrap();
    let doc = Document::open_with_encoding(&path, requested).expect("open fixture");

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
    let dir = fresh_test_dir("per_encoding_latin1_search");
    let path = dir.join("fixture.txt");
    let bytes = fixture_bytes();
    std::fs::write(&path, &bytes).expect("write fixture");

    let requested = DocumentEncoding::from_label(REQUESTED_LABEL).unwrap();
    let doc = Document::open_with_encoding(&path, requested).expect("open fixture");

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
    let dir = fresh_test_dir("per_encoding_latin1_save");
    let path = dir.join("fixture.txt");
    let saved = dir.join("fixture.saved.txt");
    let bytes = fixture_bytes();
    std::fs::write(&path, &bytes).expect("write fixture");

    let requested = DocumentEncoding::from_label(REQUESTED_LABEL).unwrap();
    let mut doc = Document::open_with_encoding(&path, requested).expect("open fixture");

    doc.save_to(&saved).expect("save_to must succeed");

    let saved_bytes = std::fs::read(&saved).expect("read saved file");
    assert_eq!(
        saved_bytes, bytes,
        "save_to without edits must produce byte-identical bytes for Class A native open",
    );
}

/// Representable non-ASCII insertion for the encoded edit-path test.
/// `café` keeps the test string short while exercising the canonical
/// high-byte glyphs of the WHATWG `ISO-8859-1` alias (windows-1252).
const NON_ASCII_INSERT: &str = "café";

#[test]
fn edit_and_save_round_trip() {
 // — fifth contract. Validates (edit + save
 // round-trip) and (save fidelity) for the WHATWG `ISO-8859-1`
 // alias (canonicalised to `windows-1252`) through the
 // encoded edit path: insert ASCII, insert representable non-ASCII
 // delete a range, save, reopen, and assert the decoded text round-
 // trips byte-identically against the in-memory document.
    let dir = fresh_test_dir("per_encoding_latin1_edit");
    let path = dir.join("fixture.txt");
    let saved = dir.join("fixture.saved.txt");
    let bytes = fixture_bytes();
    std::fs::write(&path, &bytes).expect("write fixture");

    let requested = DocumentEncoding::from_label(REQUESTED_LABEL).unwrap();
    let mut doc = Document::open_with_encoding(&path, requested).expect("open fixture");

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

    let reopened = Document::open_with_encoding(&saved, requested).expect("reopen saved fixture");
    assert_eq!(
        reopened.text_lossy(),
        in_memory_text,
        "edit → save → reopen must yield the same decoded text as the in-memory document",
    );
}
