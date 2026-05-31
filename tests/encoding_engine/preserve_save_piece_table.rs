// — non-UTF-8
// piece-tree preserve-save and save-conversion.
//
//
// These tests pin the contract that, once a non-UTF-8 document picks
// up a piece-tree edit buffer through the encoded insert
// path, both flavours of save behave correctly:
//
// 1. Preserve-save (`Document::save_to`) streams the piece-tree
// bytes verbatim through the matching `SaveSnapshot::PieceTable`
// branch in `prepare_save_with_encoding_and_policy`.
// The on-disk bytes after save must equal the byte-for-byte
// splice of the fixture and the inserted encoded payload.
// 2. Save-conversion to UTF-8 (`save_to_with_encoding(UTF-8)`)
// decodes the piece-tree bytes through `encoding_rs` (and *not*
// via `piece_table.to_string_lossy()`, which would corrupt the
// raw legacy bytes by interpreting them as UTF-8) before
// re-encoding into the target encoding. The decoded text must
// match the original document text with the inserted Unicode
// string spliced at the correct offset.
//
// Each fixture is materialised under `fresh_test_dir(...)`
// and is small enough to keep the test suite quick. The fixtures are
// padded above `PIECE_TABLE_MIN_BYTES` (~1 MiB) so that the document
// is guaranteed to land on the piece-tree edit buffer when the first
// insert promotes it from mmap-only state, matching the production
// dispatch in `editing.rs::edit_buffer_plan_for_line`.

#[path = "mod.rs"]
#[allow(clippy::duplicate_mod)] // helpers module is also loaded by sibling per_encoding tests
mod helpers;

use encoding_rs::{SHIFT_JIS, WINDOWS_1251};
use helpers::fresh_test_dir;
use qem::{Document, DocumentEncoding, TextPosition};

/// Pad the fixture above the 1 MiB piece-tree threshold so the first
/// insert always promotes the document to a piece-tree edit buffer.
/// This mirrors the dispatch in `editing.rs::edit_buffer_plan_for_line`.
const PIECE_TABLE_THRESHOLD_BYTES: usize = 1024 * 1024;

/// Helper: encodes `text` through `encoding_rs::Encoding` and asserts
/// the result is round-trippable (no redirect, no unmappable scalar).
fn encode_strict(encoding: &'static encoding_rs::Encoding, text: &str) -> Vec<u8> {
    let (encoded, used, had_errors) = encoding.encode(text);
    assert_eq!(
        used,
        encoding,
        "fixture text must round-trip through {} without redirect",
        encoding.name()
    );
    assert!(
        !had_errors,
        "fixture text must be representable in {}",
        encoding.name()
    );
    encoded.into_owned()
}

/// Build a windows-1251 fixture above the piece-tree threshold. The
/// fixture starts with an ASCII anchor line and then repeats Cyrillic
/// padding so total size exceeds `PIECE_TABLE_THRESHOLD_BYTES`.
fn windows_1251_fixture() -> Vec<u8> {
    let mut text = String::from("anchor\n");
    let pad_line = "падддинг\n"; // 9 chars, all in windows-1251
    while text.len() < PIECE_TABLE_THRESHOLD_BYTES + 64 {
        text.push_str(pad_line);
    }
    encode_strict(WINDOWS_1251, &text)
}

/// Build a UTF-16LE fixture above the piece-tree threshold. UTF-16
/// has no `encoding_rs::Encoding::encode` round-trip (the WHATWG
/// spec makes UTF-16 decode-only), so we hand-encode each `u16` code
/// unit through `str::encode_utf16`.
fn utf16le_fixture() -> Vec<u8> {
    let mut text = String::from("anchor\n");
    let pad_line = "中文 текст\n";
    while text.len() < PIECE_TABLE_THRESHOLD_BYTES + 64 {
        text.push_str(pad_line);
    }
    let mut bytes = Vec::with_capacity(text.len() * 2);
    for unit in text.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    bytes
}

/// Build a Shift_JIS fixture above the piece-tree threshold using
/// Japanese padding that cleanly round-trips through Shift_JIS.
fn shift_jis_fixture() -> Vec<u8> {
    let mut text = String::from("anchor\n");
    let pad_line = "こんにちは\n";
    while text.len() < PIECE_TABLE_THRESHOLD_BYTES + 64 {
        text.push_str(pad_line);
    }
    encode_strict(SHIFT_JIS, &text)
}

#[test]
fn windows_1251_piece_table_preserve_save_round_trip() {
    let dir = fresh_test_dir("piece_table_preserve_windows_1251");
    let path = dir.join("source.txt");
    let saved = dir.join("source.saved.txt");
    let bytes = windows_1251_fixture();
    std::fs::write(&path, &bytes).expect("write fixture");

    let encoding = DocumentEncoding::from_label("windows-1251").unwrap();
    let mut doc = Document::open_with_encoding(&path, encoding).expect("open fixture");

 // Insert representable Cyrillic text at line 0, column 0. The
 // encoded path appends the windows-1251 bytes of `insertion`
 // verbatim into the piece-tree add buffer.
    let insertion = "Привет, мир!";
    let _ = doc.try_insert(TextPosition::new(0, 0), insertion).unwrap();
    assert!(
        doc.has_piece_table(),
        "non-UTF-8 insert must promote document to piece-tree edit buffer",
    );
    assert!(!doc.has_rope(), "preserve-save path requires rope=None");

 // Preserve-save streams piece-tree bytes verbatim.
    doc.save_to(&saved).expect("preserve-save must succeed");

    let saved_bytes = std::fs::read(&saved).expect("read saved file");
    let inserted_bytes = encode_strict(WINDOWS_1251, insertion);
    let mut expected = Vec::with_capacity(bytes.len() + inserted_bytes.len());
    expected.extend_from_slice(&inserted_bytes);
    expected.extend_from_slice(&bytes);

    assert_eq!(
        saved_bytes, expected,
        "preserve-save must produce a byte-identical splice of inserted bytes \
         and the original fixture",
    );
}

#[test]
fn windows_1251_piece_table_save_to_utf8_decodes_correctly() {
    let dir = fresh_test_dir("piece_table_convert_windows_1251");
    let path = dir.join("source.txt");
    let saved = dir.join("source.utf8.txt");
    let bytes = windows_1251_fixture();
    std::fs::write(&path, &bytes).expect("write fixture");

    let encoding = DocumentEncoding::from_label("windows-1251").unwrap();
    let mut doc = Document::open_with_encoding(&path, encoding).expect("open fixture");

    let insertion = "Привет, мир!";
    let _ = doc.try_insert(TextPosition::new(0, 0), insertion).unwrap();
    assert!(doc.has_piece_table());
    assert!(!doc.has_rope());

 // Save-conversion to UTF-8 must decode the piece-tree bytes
 // through `encoding_rs` (not through `to_string_lossy()`), then
 // re-encode into UTF-8.
    let utf8 = DocumentEncoding::utf8();
    doc.save_to_with_encoding(&saved, utf8)
        .expect("save_to_with_encoding(UTF-8) must succeed");

    let saved_text = std::fs::read_to_string(&saved).expect("saved file must be valid UTF-8");
    let (original_text, _, _) = WINDOWS_1251.decode(&bytes);
    let expected = format!("{insertion}{}", original_text.as_ref());

    assert_eq!(
        saved_text, expected,
        "save-conversion must decode piece-tree bytes through encoding_rs and \
         produce the canonical UTF-8 representation of the document",
    );
}

#[test]
fn utf16le_piece_table_preserve_save_round_trip() {
    let dir = fresh_test_dir("piece_table_preserve_utf16_le");
    let path = dir.join("source.txt");
    let saved = dir.join("source.saved.txt");
    let bytes = utf16le_fixture();
    std::fs::write(&path, &bytes).expect("write fixture");

    let encoding = DocumentEncoding::utf16le();
    let mut doc = Document::open_with_encoding(&path, encoding).expect("open fixture");

 // Insert a Unicode string. UTF-16 encodes via `str::encode_utf16`
 // in the encoded path, so any Unicode scalar (including
 // supplementary code points via surrogate pairs) is representable.
    let insertion = "Привет 世界!";
    let _ = doc.try_insert(TextPosition::new(0, 0), insertion).unwrap();
    assert!(doc.has_piece_table());
    assert!(!doc.has_rope());

    doc.save_to(&saved).expect("preserve-save must succeed");

    let saved_bytes = std::fs::read(&saved).expect("read saved file");
    let mut inserted_bytes = Vec::with_capacity(insertion.len() * 2);
    for unit in insertion.encode_utf16() {
        inserted_bytes.extend_from_slice(&unit.to_le_bytes());
    }
    let mut expected = Vec::with_capacity(bytes.len() + inserted_bytes.len());
    expected.extend_from_slice(&inserted_bytes);
    expected.extend_from_slice(&bytes);

    assert_eq!(
        saved_bytes, expected,
        "preserve-save must produce a byte-identical splice of UTF-16 inserted \
         bytes and the original fixture",
    );
}

#[test]
fn shift_jis_piece_table_preserve_save_round_trip() {
    let dir = fresh_test_dir("piece_table_preserve_shift_jis");
    let path = dir.join("source.txt");
    let saved = dir.join("source.saved.txt");
    let bytes = shift_jis_fixture();
    std::fs::write(&path, &bytes).expect("write fixture");

    let encoding = DocumentEncoding::from_label("Shift_JIS").unwrap();
    let mut doc = Document::open_with_encoding(&path, encoding).expect("open fixture");

    let insertion = "こんにちは!";
    let _ = doc.try_insert(TextPosition::new(0, 0), insertion).unwrap();
    assert!(doc.has_piece_table());
    assert!(!doc.has_rope());

    doc.save_to(&saved).expect("preserve-save must succeed");

    let saved_bytes = std::fs::read(&saved).expect("read saved file");
    let inserted_bytes = encode_strict(SHIFT_JIS, insertion);
    let mut expected = Vec::with_capacity(bytes.len() + inserted_bytes.len());
    expected.extend_from_slice(&inserted_bytes);
    expected.extend_from_slice(&bytes);

    assert_eq!(
        saved_bytes, expected,
        "preserve-save must produce a byte-identical splice of Shift_JIS inserted \
         bytes and the original fixture",
    );
}
