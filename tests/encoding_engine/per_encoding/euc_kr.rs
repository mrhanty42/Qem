// per-encoding integration suite.
//
// Class B encoding: EUC-KR (Korean CJK multibyte).
//
//
// This file ships the fixed four-test contract for the
// `EUC-KR` Class B encoding. The structure mirrors `shift_jis.rs`
// exactly; the only difference is the non-ASCII payload uses Hangul
// syllables that round-trip cleanly through `encoding_rs::EUC_KR`. See
// the `shift_jis.rs` module header for a detailed description of every
// test contract.
//
// The fifth contract `edit_and_save_round_trip` is
// : insert ASCII, insert representable Hangul syllables
// delete the inserted prefix through the encoded replace-range path
// save, reopen, and assert the decoded text round-trips byte-identically
//.

#[path = "../mod.rs"]
#[allow(clippy::duplicate_mod)] // helpers module is also loaded by sibling per_encoding tests
mod helpers;

use encoding_rs::EUC_KR;
use helpers::fresh_test_dir;
use qem::{Document, DocumentEncoding, RegexSearchQuery, TextPosition};

/// Canonical encoding label for this module.
const LABEL: &str = "EUC-KR";

/// First line of the fixture. ASCII only so the literal `TARGET`
/// marker is contiguous in the encoded byte stream.
const LINE0: &str = "TARGET line0";
/// Last line — ASCII only, with no trailing newline.
const LINE4: &str = "last line";

/// Fixture text. Line 3 carries Hangul syllables which encode to 2
/// bytes per character in EUC-KR (UHC superset; lead `0x81..=0xFE`)
/// exercising the 2-byte branch of the
/// `MultiByteEngine` leading-byte detector. The Hangul syllables
/// `\u{C548}\u{B155}\u{D558}\u{C138}\u{C694}` ("hello") are valid in
/// EUC-KR and round-trip cleanly.
const FIXTURE_TEXT: &str =
    "TARGET line0\nascii line1\r\n\n\u{C548}\u{B155}\u{D558}\u{C138}\u{C694}\rlast line";

/// Total number of lines the open path must report.
const EXPECTED_LINE_COUNT: usize = 5;

/// Encodes the fixture text into EUC-KR bytes.
fn fixture_bytes() -> Vec<u8> {
    let (encoded, used, had_errors) = EUC_KR.encode(FIXTURE_TEXT);
    assert_eq!(
        used, EUC_KR,
        "fixture text must round-trip through EUC-KR without redirect",
    );
    assert!(!had_errors, "fixture text must be representable in EUC-KR");
    encoded.into_owned()
}

#[test]
fn opens_and_indexes_lines() {
    let dir = fresh_test_dir("per_encoding_euc_kr_open");
    let path = dir.join("fixture.txt");
    let bytes = fixture_bytes();
    std::fs::write(&path, &bytes).expect("write fixture");

    let encoding = DocumentEncoding::from_label(LABEL).unwrap();
    let doc = Document::open_with_encoding(&path, encoding)
        .expect("open_with_encoding(EUC-KR) must succeed for valid fixture");

    assert_eq!(
        doc.encoding(),
        encoding,
        "open_with_encoding must install the requested EUC-KR contract",
    );
    assert_eq!(
        doc.line_count().exact(),
        Some(EXPECTED_LINE_COUNT),
        "Class B native open must produce an exact line count after open returns",
    );
}

#[test]
fn viewport_first_and_last_window() {
    let dir = fresh_test_dir("per_encoding_euc_kr_viewport");
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
    let dir = fresh_test_dir("per_encoding_euc_kr_search");
    let path = dir.join("fixture.txt");
    let bytes = fixture_bytes();
    std::fs::write(&path, &bytes).expect("write fixture");

    let encoding = DocumentEncoding::from_label(LABEL).unwrap();
    let doc = Document::open_with_encoding(&path, encoding).expect("open fixture");

 // Regex search routes through `find_next_regex_in_class_b_chunked`
 // for CJK multibyte.
    let query = RegexSearchQuery::new("TARGET").expect("compile ASCII regex");
    let regex_match = doc
        .find_next_regex_query(&query, TextPosition::new(0, 0))
        .expect("regex search must locate the ASCII marker on line 0");
    assert_eq!(regex_match.start(), TextPosition::new(0, 0));
    assert_eq!(regex_match.end(), TextPosition::new(0, 6));

 // EUC-KR encodes ASCII bytes verbatim, so the literal byte
 // finder hits the marker on line 0 without ambiguity.
    let literal = doc
        .find_next("TARGET", TextPosition::new(0, 0))
        .expect("literal search must locate the ASCII marker on line 0");
    assert_eq!(literal.start(), TextPosition::new(0, 0));
    assert_eq!(literal.end(), TextPosition::new(0, 6));
}

#[test]
fn save_round_trip_no_edits() {
    let dir = fresh_test_dir("per_encoding_euc_kr_save");
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
        "save_to without edits must produce byte-identical bytes for EUC-KR native open",
    );
}

/// Representable non-ASCII insertion for the encoded edit-path test.
/// `안녕` is two Hangul syllables, each encoding to two bytes under
/// EUC-KR (UHC superset), so the encoded insert path appends four
/// multibyte cells into the piece-tree add buffer without falling
/// into the `UnrepresentableText` branch.
const NON_ASCII_INSERT: &str = "안녕";

#[test]
fn edit_and_save_round_trip() {
 // — fifth contract for the per-encoding suite.
 // Validates (edit + save round-trip) and (save fidelity)
 // for `EUC-KR` through the encoded edit path: insert ASCII
 // insert representable Hangul, delete the ASCII prefix through the
 // encoded replace-range path, save, reopen, and assert the decoded
 // text round-trips byte-identically against the in-memory document.
 // holds implicitly because the encoded path never transcodes
 // the document into UTF-8.
    let dir = fresh_test_dir("per_encoding_euc_kr_edit");
    let path = dir.join("fixture.txt");
    let saved = dir.join("fixture.saved.txt");
    let bytes = fixture_bytes();
    std::fs::write(&path, &bytes).expect("write fixture");

    let encoding = DocumentEncoding::from_label(LABEL).unwrap();
    let mut doc = Document::open_with_encoding(&path, encoding).expect("open fixture");

    doc.try_insert(TextPosition::new(0, 0), "EDIT ")
        .expect("ASCII insert must succeed for EUC-KR encoded edit buffer");
    doc.try_insert(TextPosition::new(0, 5), NON_ASCII_INSERT)
        .expect("non-ASCII Hangul insert must succeed");
    doc.try_replace_range(0, 0, 5, "")
        .expect("encoded delete-range must succeed");

    let in_memory_text = doc.text_lossy();
    assert!(
        in_memory_text.starts_with(NON_ASCII_INSERT),
        "in-memory text must start with the Hangul insertion after the encoded splice",
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
