// per-encoding integration suite.
//
// Class B encoding: Shift_JIS (Japanese CJK multibyte).
//
//
// This file ships the fixed four-test contract for the
// `Shift_JIS` Class B encoding. Each test creates its own fresh fixture
// under `fresh_test_dir(...)` and exercises one of the
// integration contracts:
//
// 1. `opens_and_indexes_lines` — open a multi-line Shift_JIS fixture
// mixing ASCII, Kanji + Hiragana non-ASCII payload, LF / CRLF / CR
// terminators and an empty line; assert the exact `line_count`
// and that the document self-identifies as `Shift_JIS`.
// 2. `viewport_first_and_last_window` — read the first and last
// lines through `Document::line_slice(line, 0, 256)`. The decoded
// text must equal the expected first/last line strings.
// 3. `literal_and_regex_search_finds_known_match` — embed an ASCII
// `TARGET` marker on line 0 and exercise both literal and regex
// search paths. `find_next_regex_query` routes through the
// encoding-aware chunked-decode path
// (`find_next_regex_in_class_b_chunked` for CJK) and must
// return `(0, 0)..(0, 6)` in text-unit columns. The literal
// `find_next` byte-finder is also exercised on the same fixture
// because Shift_JIS encodes ASCII bytes verbatim (1 byte per
// ASCII char, no filler bytes), so the contiguous needle bytes
// `[0x54, 0x41, 0x52, 0x47, 0x45, 0x54]` appear at byte offset 0
// in the source and the literal finder maps that to the same
// `(0, 0)..(0, 6)` text-unit position.
// 4. `save_round_trip_no_edits` — `Document::save_to(saved_path)`
// with no prior edits must yield a byte-identical copy of the
// raw Shift_JIS bytes. added a probe-decode of the
// raw bytes at open time so the `decoding_had_errors` flag is
// set for ill-formed CJK input; for a cleanly encoded fixture
// the flag stays `false` and preserve-save streams the original
// mmap bytes verbatim.
//
// The fifth contract `edit_and_save_round_trip` is
// : insert ASCII, insert representable non-ASCII (CJK
// hiragana), delete the inserted prefix through the encoded
// replace-range path, save, reopen, and assert the decoded text
// round-trips byte-identically.

#[path = "../mod.rs"]
#[allow(clippy::duplicate_mod)] // helpers module is also loaded by sibling per_encoding tests
mod helpers;

use encoding_rs::SHIFT_JIS;
use helpers::fresh_test_dir;
use qem::{Document, DocumentEncoding, RegexSearchQuery, TextPosition};

/// Canonical encoding label for this module. `encoding_rs` canonicalises
/// `shift_jis` (and other aliases) to `Shift_JIS`; the round-trip
/// `from_label("shift_jis").name() == "Shift_JIS"` invariant is what
/// the dispatch in `engine_for_encoding` keys on.
const LABEL: &str = "shift_jis";

/// First line of the fixture. ASCII only so the literal `TARGET`
/// marker is byte-identical and contiguous in the encoded stream.
const LINE0: &str = "TARGET line0";
/// Last line — ASCII only, with no trailing newline so the file ends
/// without a phantom empty line.
const LINE4: &str = "last line";

/// Fixture text. The structure mirrors the UTF-16 per-encoding
/// fixtures and exercises:
///
/// * Line 0 — ASCII with the `TARGET` marker, terminated by `LF`.
/// * Line 1 — ASCII, terminated by `CRLF` (CRLF collapses to a
/// single boundary in `MultiByteEngine::next_line_start`).
/// * Line 2 — empty line, terminated by `LF`.
/// * Line 3 — Kanji + Hiragana non-ASCII payload, terminated by
/// lone `CR`. The Kanji `\u{65E5}\u{672C}\u{8A9E}` ("Japanese")
/// and the Hiragana `\u{3072}\u{3089}\u{304C}\u{306A}` ("hiragana")
/// each encode to 2 bytes per character in Shift_JIS, so the line
/// contains 14 multibyte source bytes plus a single ASCII space.
/// * Line 4 — ASCII, no trailing newline.
const FIXTURE_TEXT: &str =
    "TARGET line0\nascii line1\r\n\n\u{65E5}\u{672C}\u{8A9E} \u{3072}\u{3089}\u{304C}\u{306A}\rlast line";

/// Total number of lines the open path must report through
/// `Document::line_count` once indexing finishes.
const EXPECTED_LINE_COUNT: usize = 5;

/// Encodes the fixture text into Shift_JIS bytes. `encoding_rs::SHIFT_JIS`
/// canonicalises `Shift_JIS`, so the assertion that the encoder did not
/// redirect the output to a different encoding pins the fixture to the
/// expected target.
fn fixture_bytes() -> Vec<u8> {
    let (encoded, used, had_errors) = SHIFT_JIS.encode(FIXTURE_TEXT);
    assert_eq!(
        used, SHIFT_JIS,
        "fixture text must round-trip through Shift_JIS without redirect",
    );
    assert!(
        !had_errors,
        "fixture text must be representable in Shift_JIS",
    );
    encoded.into_owned()
}

#[test]
fn opens_and_indexes_lines() {
    let dir = fresh_test_dir("per_encoding_shift_jis_open");
    let path = dir.join("fixture.txt");
    let bytes = fixture_bytes();
    std::fs::write(&path, &bytes).expect("write fixture");

    let encoding = DocumentEncoding::from_label(LABEL).unwrap();
    let doc = Document::open_with_encoding(&path, encoding)
        .expect("open_with_encoding(Shift_JIS) must succeed for valid fixture");

    assert_eq!(
        doc.encoding(),
        encoding,
        "open_with_encoding must install the requested Shift_JIS contract",
    );
    assert_eq!(
        doc.line_count().exact(),
        Some(EXPECTED_LINE_COUNT),
        "Class B native open must produce an exact line count after open returns",
    );
}

#[test]
fn viewport_first_and_last_window() {
    let dir = fresh_test_dir("per_encoding_shift_jis_viewport");
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
    let dir = fresh_test_dir("per_encoding_shift_jis_search");
    let path = dir.join("fixture.txt");
    let bytes = fixture_bytes();
    std::fs::write(&path, &bytes).expect("write fixture");

    let encoding = DocumentEncoding::from_label(LABEL).unwrap();
    let doc = Document::open_with_encoding(&path, encoding).expect("open fixture");

 // Regex search routes through `find_next_regex_in_class_b_chunked`
 // for CJK multibyte. The chunked decode +
 // glue path locates the ASCII marker on line 0 and re-encodes the
 // match through `encoding_rs::Encoding::encode` to map back to the
 // source byte offset.
    let query = RegexSearchQuery::new("TARGET").expect("compile ASCII regex");
    let regex_match = doc
        .find_next_regex_query(&query, TextPosition::new(0, 0))
        .expect("regex search must locate the ASCII marker on line 0");
    assert_eq!(regex_match.start(), TextPosition::new(0, 0));
    assert_eq!(regex_match.end(), TextPosition::new(0, 6));

 // Literal search runs a byte-level finder against the raw mmap
 // bytes. Shift_JIS encodes ASCII bytes verbatim (no filler bytes)
 // so the six contiguous UTF-8 needle bytes for `"TARGET"` appear
 // at byte offset 0 in the source. The literal finder therefore
 // returns the same `(0, 0)..(0, 6)` text-unit position the regex
 // path produces.
    let literal = doc
        .find_next("TARGET", TextPosition::new(0, 0))
        .expect("literal search must locate the ASCII marker on line 0");
    assert_eq!(literal.start(), TextPosition::new(0, 0));
    assert_eq!(literal.end(), TextPosition::new(0, 6));
}

#[test]
fn save_round_trip_no_edits() {
    let dir = fresh_test_dir("per_encoding_shift_jis_save");
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
        "save_to without edits must produce byte-identical bytes for Shift_JIS native open",
    );
}

/// Representable non-ASCII insertion for the encoded edit-path test.
/// `こんにちは` is five Hiragana code points, each encoding to two
/// bytes under Shift_JIS, so the encoded insert path appends ten
/// multibyte cells into the piece-tree add buffer without falling
/// into the `UnrepresentableText` branch.
const NON_ASCII_INSERT: &str = "こんにちは";

#[test]
fn edit_and_save_round_trip() {
 // — fifth contract for the per-encoding suite.
 // Validates (edit + save round-trip) and (save fidelity)
 // for `Shift_JIS` through the encoded edit path: insert
 // ASCII, insert representable Hiragana, delete the ASCII prefix
 // through the encoded replace-range path, save, reopen, and
 // assert the decoded text round-trips byte-identically against
 // the in-memory document. holds implicitly because the
 // encoded path never transcodes the document into UTF-8.
    let dir = fresh_test_dir("per_encoding_shift_jis_edit");
    let path = dir.join("fixture.txt");
    let saved = dir.join("fixture.saved.txt");
    let bytes = fixture_bytes();
    std::fs::write(&path, &bytes).expect("write fixture");

    let encoding = DocumentEncoding::from_label(LABEL).unwrap();
    let mut doc = Document::open_with_encoding(&path, encoding).expect("open fixture");

 // Step 1 — ASCII insert at line 0, column 0.
    doc.try_insert(TextPosition::new(0, 0), "EDIT ")
        .expect("ASCII insert must succeed for Shift_JIS encoded edit buffer");
 // Step 2 — representable Hiragana insert directly after the ASCII
 // prefix. Each Hiragana code point encodes to two Shift_JIS bytes.
    doc.try_insert(TextPosition::new(0, 5), NON_ASCII_INSERT)
        .expect("non-ASCII Hiragana insert must succeed");
 // Step 3 — delete the 5-column ASCII prefix through the encoded
 // replace-range path. After the splice, line 0 begins with the
 // Hiragana chunk followed by the original `TARGET line0` text.
    doc.try_replace_range(0, 0, 5, "")
        .expect("encoded delete-range must succeed");

    let in_memory_text = doc.text_lossy();
    assert!(
        in_memory_text.starts_with(NON_ASCII_INSERT),
        "in-memory text must start with the Hiragana insertion after the encoded splice",
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
