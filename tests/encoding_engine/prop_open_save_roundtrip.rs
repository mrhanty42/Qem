// Property 7: open_with_encoding -> save round-trip is byte-identical for Class A.
//
//
// For any encoding `e ∈ {UTF-8} ∪ Class A` and any byte sequence valid
// in `e`, the round-trip:
//
// write(path, bytes)
// let mut doc = Document::open_with_encoding(path, e)?;
// doc.save_to(saved_path)?;
// std::fs::read(saved_path)? == bytes
//
// must hold byte-for-byte. Class A is total over `0x00..=0xFF` (every
// byte sequence is a legal Class A document under
// `SingleByteEngine`), so the strategy generates arbitrary
// `Vec<u8>` of length `0..=4096`. UTF-8 is restricted to byte
// sequences that are valid UTF-8 — `proptest::string::string_regex`
// guarantees this — and that do not start with the UTF-8 BOM
// (`EF BB BF`). The current open path strips a leading UTF-8 BOM
// through the rope decode pipeline (see `from_storage_with_encoding`
// in `src/document/lifecycle.rs`), and the saved bytes would
// therefore drop those three bytes; a leading BOM would invalidate
// the round-trip without indicating a bug. A `\u{FEFF}` anywhere
// past byte 0 is preserved by the mmap fast path and is left in.
//
// The save path under test is the preserve-encoding `Document::save_to`
// route: every Class A open lands as `rope: None, piece_table: None
// storage: Some(mmap)`, which `prepare_save_with_encoding_and_policy`
// resolves to `SaveSnapshot::Mmap` — the original bytes streamed
// straight back to disk. The UTF-8 fast path follows
// the same branch through the matching UTF-8 → UTF-8 mmap snapshot
// or the empty-rope snapshot for `file_len == 0`.
//
// Class B encodings (UTF-16 LE/BE, Shift_JIS, GB18030, EUC-KR) are
// covered by / once their native open paths land;
// they are explicitly out of scope for and are left to the
// task list owners.
//
// The cases count is intentionally pinned at 64 for this spec.

#[path = "mod.rs"]
#[allow(clippy::duplicate_mod)] // shared helpers module is also loaded by prop_backing.rs
mod helpers;

use helpers::fresh_test_dir;
use proptest::prelude::*;
use qem::{Document, DocumentEncoding};
use std::path::{Path, PathBuf};

/// Class A encoding labels. The set mirrors `prop_backing.rs`
/// and `prop_newline.rs` so Property 7 covers the same ASCII-superset
/// single-byte surface as Properties 3, 4, and 6. Labels match
/// `encoding_rs` exactly so `DocumentEncoding::from_label` always
/// succeeds. Note: WHATWG aliases like `ISO-8859-1` collapse to
/// `windows-1252` in `encoding_rs`, so the labels here use the
/// canonical names.
const CLASS_A_LABELS: &[&str] = &[
    "windows-1250",
    "windows-1251",
    "windows-1252",
    "windows-1253",
    "windows-1254",
    "windows-1255",
    "windows-1256",
    "windows-1257",
    "windows-1258",
    "windows-874",
    "ISO-8859-2",
    "ISO-8859-3",
    "ISO-8859-4",
    "ISO-8859-5",
    "ISO-8859-7",
    "ISO-8859-10",
    "ISO-8859-13",
    "ISO-8859-14",
    "ISO-8859-15",
    "ISO-8859-16",
    "KOI8-R",
    "KOI8-U",
    "IBM866",
    "macintosh",
    "x-mac-cyrillic",
];

fn class_a_encoding_strategy() -> impl Strategy<Value = DocumentEncoding> {
    prop::sample::select(CLASS_A_LABELS.to_vec()).prop_map(|label| {
        DocumentEncoding::from_label(label)
            .unwrap_or_else(|| panic!("encoding_rs should know {label}"))
    })
}

/// Generates byte sequences that are valid UTF-8 and do not begin with
/// the UTF-8 BOM. `proptest::string::string_regex` produces valid
/// `String` values directly; the byte sequence is recovered through
/// `String::into_bytes`. The regex caps the length at 512 chars so
/// each case stays cheap; `(?s)` lets `.` match newlines, which the
/// mmap fast path must preserve verbatim regardless of LF / CR / CRLF
/// content.
fn utf8_bytes_strategy() -> impl Strategy<Value = Vec<u8>> {
    proptest::string::string_regex(r"(?s).{0,512}")
        .expect("valid UTF-8 regex strategy")
        .prop_map(|s| s.into_bytes())
        .prop_filter(
            "exclude UTF-8 BOM-prefixed strings: open path strips the BOM",
            |bytes| !bytes.starts_with(&[0xEF, 0xBB, 0xBF]),
        )
}

/// Generates byte sequences for Class A documents. Class A is total
/// over `0x00..=0xFF`, so any byte sequence is a valid document
/// under `SingleByteEngine`. Bounded at 4096 bytes per the
/// task brief; this keeps every case well under
/// `INLINE_FULL_INDEX_MAX_FILE_BYTES = 8 MiB` and within the same
/// `from_storage_class_a_native` mmap branch.
fn class_a_bytes_strategy() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(any::<u8>(), 0..=4096)
}

/// One round-trip case: a (encoding, bytes) pair drawn either from
/// the UTF-8 sub-strategy or from Class A. Using a single
/// `prop_oneof` lets proptest shrink across branches and produce
/// minimal counterexamples regardless of which encoding family the
/// failure originated
#[derive(Debug, Clone)]
enum Case {
    Utf8 {
        bytes: Vec<u8>,
    },
    ClassA {
        encoding: DocumentEncoding,
        bytes: Vec<u8>,
    },
}

fn case_strategy() -> impl Strategy<Value = Case> {
    prop_oneof![
        utf8_bytes_strategy().prop_map(|bytes| Case::Utf8 { bytes }),
        (class_a_encoding_strategy(), class_a_bytes_strategy())
            .prop_map(|(encoding, bytes)| Case::ClassA { encoding, bytes }),
    ]
}

/// Writes `bytes` to `dir/file_name` and returns the full path. The
/// fixture directory itself is created by `fresh_test_dir` before
/// this is called.
fn write_fixture(dir: &Path, file_name: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.join(file_name);
    std::fs::write(&path, bytes).expect("write_fixture: write");
    path
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

 /// Property 7: `Document::open_with_encoding(path, e) → save_to(saved)`
 /// without intervening edits writes bytes byte-identical to the
 /// source for every `e ∈ {UTF-8} ∪ Class A`.
 ///
 /// The test:
 /// 1. Writes the random `bytes` to `fixture.bin` under a
 /// `fresh_test_dir`.
 /// 2. Opens the fixture through `Document::open_with_encoding`.
 /// Class A and UTF-8-without-BOM both land as
 /// `rope: None, piece_table: None, storage: Some(mmap)`.
 /// 3. Asserts `doc.encoding() == e` (encoding contract was
 /// installed atomically through `set_encoding_contract`).
 /// 4. Saves to `saved.bin` through the preserve-encoding
 /// `save_to` route.
 /// 5. Reads `saved.bin` back and asserts every byte matches
 /// the source. The first byte of divergence (if any) is
 /// surfaced in the failure message so counterexamples are
 /// easy to triage.
    #[test]
    fn property_7_open_save_roundtrip_is_byte_identical(
        case in case_strategy(),
    ) {
        let (encoding, bytes) = match case {
            Case::Utf8 { bytes } => (DocumentEncoding::utf8(), bytes),
            Case::ClassA { encoding, bytes } => (encoding, bytes),
        };

        let dir = fresh_test_dir("prop_open_save_roundtrip");
        let src_path = write_fixture(&dir, "fixture.bin", &bytes);
        let saved_path = dir.join("saved.bin");

        let mut doc = Document::open_with_encoding(&src_path, encoding)
            .expect("open_with_encoding should succeed for valid input");

        prop_assert_eq!(
            doc.encoding(),
            encoding,
            "open_with_encoding must install the requested encoding contract for {}",
            encoding.name(),
        );

        doc.save_to(&saved_path)
            .expect("save_to should succeed for an unmodified document");

        let saved_bytes = std::fs::read(&saved_path)
            .expect("read saved fixture");

        prop_assert_eq!(
            saved_bytes.len(),
            bytes.len(),
            "save_to({}) must produce a byte-identical file (length differs: \
             source={}, saved={})",
            encoding.name(),
            bytes.len(),
            saved_bytes.len(),
        );
        if saved_bytes != bytes {
 // Locate the first byte of divergence to make
 // counterexamples easier to triage.
            let first_diff = saved_bytes
                .iter()
                .zip(bytes.iter())
                .position(|(a, b)| a != b);
            prop_assert!(
                false,
                "save_to({}) must produce a byte-identical file but bytes \
                 diverged at offset {:?} (file_len = {})",
                encoding.name(),
                first_diff,
                bytes.len(),
            );
        }

 // Best-effort cleanup; missing files in tmp tolerated by design.
        let _ = std::fs::remove_file(&src_path);
        let _ = std::fs::remove_file(&saved_path);
        let _ = std::fs::remove_dir(&dir);
    }
}
