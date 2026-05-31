// Property 17: save round-trips after representable edits
//
//
// For any encoding `e ∈ Class A ∪ Class B`, any initial file in `e`
// and any sequence of edits restricted to scalars representable in
// `e`, the round-trip
//
// write(path, bytes_in_e)
// let mut doc = Document::open_with_encoding(path, e)?;
// for (pos, s) in edits { doc.try_insert(pos, s)?; }
// let text_before_save = doc.text_lossy();
// doc.save_to(saved)?;
// let reopened = Document::open_with_encoding(saved, e)?;
// reopened.text_lossy() == text_before_save
//
// must hold. Save-to here is the preserve-encoding path
// (`Document::save_to` → `prepare_save_with_encoding_and_policy`) used
// by the existing preserve-save piece-tree contract tests; for an
// edited non-UTF-8 document it streams the piece-tree's raw
// target-encoding bytes verbatim through the matching
// `SaveSnapshot::PieceTable` branch, and for an
// unmodified document it streams the original mmap bytes through the
// matching `SaveSnapshot::Mmap` branch.
//
// Strategy. The PBT parametrises over a representative slice of
// Class A and Class B encodings:
//
// * Class A (single-byte, ASCII-superset): `windows-1251`, `KOI8-R`
// `IBM866`, `ISO-8859-1` (canonicalised by `encoding_rs` to
// `windows-1252`), `ISO-8859-15`.
// * Class B UTF-16: `UTF-16LE`, `UTF-16BE`.
// * Class B CJK multibyte: `Shift_JIS`, `gb18030`, `EUC-KR`.
//
// Seed text is a small ASCII-only string (representable in every
// encoding above) and is encoded into the target encoding's bytes
// before being written to disk; UTF-16 hand-encodes via
// `str::encode_utf16` because `encoding_rs::Encoding::encode` is
// decode-only for the UTF-16 family. Each insert payload is filtered
// against the encoding's encoder so only representable scalars reach
// `try_insert` (`encoder.encode_from_utf8` semantics: every payload
// satisfies `had_unmappables == false` and `used == encoding` for the
// non-UTF-16 cases). UTF-16 accepts every Unicode scalar by
// definition, so the filter is a no-op there.
//
// Per fixtures live under `fresh_test_dir(...)`. Each iteration
// keeps the on-disk fixture and the in-memory edits well below 1 MiB
// so the round-trip stays cheap (the seed regex caps at 256 chars and
// each insert at 6 chars; even at the UTF-16 worst case of 2 bytes
// per code unit the iteration's byte budget is in the low kilobytes).
//
// `ProptestConfig::with_cases(64)` per the spec.

#[path = "mod.rs"]
#[allow(clippy::duplicate_mod)] // shared helpers module is also loaded by sibling integration tests
mod helpers;

use encoding_rs::Encoding;
use helpers::fresh_test_dir;
use proptest::prelude::*;
use qem::{Document, DocumentEncoding, TextPosition};

/// Looks up the raw `encoding_rs::Encoding` for a label. The PBT
/// goes directly through `encoding_rs` (rather than the
/// `pub(crate)` `DocumentEncoding::as_encoding` accessor) so the
/// test stays inside the public surface; the canonical `Encoding`
/// pointer is still globally unique, so equality checks against
/// `target.encode(text).1` are well-defined.
fn encoding_rs_for_label(label: &str) -> &'static Encoding {
    Encoding::for_label(label.as_bytes())
        .unwrap_or_else(|| panic!("encoding_rs should know label {label}"))
}

/// Class A ∪ Class B encoding labels exercised by Property 17. The
/// labels are spelled exactly as `encoding_rs` accepts them; the
/// canonicalisation rules of `encoding_rs` may collapse aliases
/// (e.g. `ISO-8859-1` to `windows-1252`), which is fine — the
/// property is asserted against the encoding actually installed on
/// the document, whatever its canonical name.
const ENCODING_LABELS: &[&str] = &[
    "windows-1251",
    "KOI8-R",
    "IBM866",
    "ISO-8859-1",
    "ISO-8859-15",
    "Shift_JIS",
    "gb18030",
    "EUC-KR",
    "UTF-16LE",
    "UTF-16BE",
];

/// Returns `true` when every scalar in `text` is representable in
/// `encoding`. The check mirrors the encoder gate in
/// `try_insert_text_at_encoded`: for non-UTF-16
/// encodings we encode through `encoding_rs::Encoding::encode` and
/// require `used == encoding && !had_unmappables` (i.e. no redirect
/// and no unmappable scalar). UTF-16 LE / BE accept every Unicode
/// scalar by definition (surrogate pairs cover the supplementary
/// planes), and `encoding_rs` deliberately refuses to *encode* into
/// UTF-16 (the WHATWG spec marks the codec decode-only, so
/// `Encoding::encode` would redirect to UTF-8 here); the encoded
/// edit path therefore hand-encodes UTF-16 via `str::encode_utf16`
/// and the filter is a no-op for UTF-16 cases.
fn is_representable(text: &str, encoding: DocumentEncoding) -> bool {
    if matches!(encoding.name(), "UTF-16LE" | "UTF-16BE") {
        return true;
    }
    let enc = encoding_rs_for_label(encoding.name());
    let (_bytes, used, had_unmappables) = enc.encode(text);
    used == enc && !had_unmappables
}

/// Encodes the ASCII seed text into the target encoding's bytes.
///
/// UTF-16 LE / BE bypass `encoding_rs::Encoding::encode` for the same
/// reason as the encoded insert path: the WHATWG spec only labels
/// UTF-16 as a decoder, so encode would redirect to UTF-8.
/// `str::encode_utf16` is total over Unicode and matches what the
/// open path expects. All other encodings round-trip
/// ASCII through their `Encoding::encode` cleanly (every Class A and
/// CJK encoding is an ASCII-superset on the low byte range).
fn encode_seed(encoding: DocumentEncoding, text: &str) -> Vec<u8> {
    match encoding.name() {
        "UTF-16LE" => text.encode_utf16().flat_map(u16::to_le_bytes).collect(),
        "UTF-16BE" => text.encode_utf16().flat_map(u16::to_be_bytes).collect(),
        label => {
            let enc = encoding_rs_for_label(label);
            let (bytes, used, had_errors) = enc.encode(text);
            assert_eq!(
                used, enc,
                "ASCII seed must round-trip through {label} without redirect",
            );
            assert!(!had_errors, "ASCII seed must be representable in {label}",);
            bytes.into_owned()
        }
    }
}

fn encoding_strategy() -> impl Strategy<Value = DocumentEncoding> {
    prop::sample::select(ENCODING_LABELS.to_vec()).prop_map(|label| {
        DocumentEncoding::from_label(label)
            .unwrap_or_else(|| panic!("encoding_rs should know {label}"))
    })
}

/// ASCII-only seed text. Bounded at 256 chars so the encoded fixture
/// stays a few hundred bytes (≤ 512 bytes for UTF-16). The newline
/// characters in the character class let the seed cover multi-line
/// documents so the open-time line indexer is exercised across all
/// engines.
fn seed_strategy() -> impl Strategy<Value = String> {
    proptest::string::string_regex(r"[A-Za-z0-9 \n.,]{0,256}").expect("valid ASCII seed regex")
}

/// Insert payload pool. Bounded at 6 chars; ASCII letters, digits
/// spaces, and punctuation are universally representable in every
/// encoding in `ENCODING_LABELS`, but the strategy still passes
/// through `is_representable` for parity with the contract: each
/// generated string is checked against the encoding's encoder before
/// being handed to `try_insert`.
fn payload_pool_strategy() -> impl Strategy<Value = String> {
    proptest::string::string_regex(r"[A-Za-z0-9 .,]{1,6}").expect("valid ASCII payload regex")
}

/// Insert position. Bounded so cases stay deterministic and don't
/// depend on the seed's exact size; `Document::try_insert` clamps
/// out-of-range positions automatically, which is the published
/// contract — the goal of this test is the round-trip property, not
/// position validation.
fn position_strategy() -> impl Strategy<Value = TextPosition> {
    (0usize..=4, 0usize..=24).prop_map(|(line0, col0)| TextPosition::new(line0, col0))
}

/// One iteration's edit list: 0..=6 (position, payload) pairs.
fn edits_strategy() -> impl Strategy<Value = Vec<(TextPosition, String)>> {
    prop::collection::vec((position_strategy(), payload_pool_strategy()), 0..=6)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

 /// Property 17. After opening a file in an encoding `e ∈ Class A
 /// ∪ Class B`, applying a sequence of inserts whose payloads are
 /// all representable in `e`, and saving through preserve-encoding
 /// `save_to`, reopening the saved file in `e` must yield a
 /// `text_lossy()` equal to the in-memory `text_lossy()` captured
 /// just before save.
 ///
 /// The test:
 /// 1. Encodes the ASCII seed text into `e`'s bytes and writes
 /// the fixture under `fresh_test_dir(...)`.
 /// 2. Opens it with `Document::open_with_encoding(seed, e)`.
 /// 3. Filters the generated edits to only the payloads that
 /// pass `is_representable(payload, e)` — the same
 /// `had_unmappables == false` gate the encoded insert path
 /// uses internally (here it just means we don't try to
 /// insert something the encoding can't carry).
 /// 4. Applies each insert via `Document::try_insert(pos, &s)`.
 /// Every call must succeed because the input is
 /// representable.
 /// 5. Snapshots `doc.text_lossy()` — the decoded post-edit
 /// text — *before* save.
 /// 6. Calls `doc.save_to(saved)`. For an edited non-UTF-8
 /// document this routes through the
 /// `SaveSnapshot::PieceTable` branch in
 /// `prepare_save_with_encoding_and_policy` and streams the
 /// piece-tree's raw bytes verbatim. For an
 /// unmodified document it streams the original mmap bytes
 /// verbatim through `SaveSnapshot::Mmap`.
 /// 7. Reopens the saved file with the same encoding and
 /// asserts `reopened.text_lossy() == text_before_save`.
    #[test]
    fn property_17_save_roundtrips_after_representable_edits(
        encoding in encoding_strategy(),
        seed_text in seed_strategy(),
        edits in edits_strategy(),
    ) {
        let dir = fresh_test_dir("prop_edit_save_roundtrip");
        let src_path = dir.join("seed.bin");
        let saved_path = dir.join("saved.bin");

        let seed_bytes = encode_seed(encoding, &seed_text);
 // : cap iteration size well below 1 MiB. The bound is
 // formal — the strategy already keeps each fixture in the
 // low kilobytes — but failing fast here surfaces any
 // accidental strategy regression that blew the budget
 // instead of letting the iteration run.
        prop_assert!(
            seed_bytes.len() <= 1024 * 1024,
            "seed fixture must stay below 1 MiB per iteration (got {} bytes for {})",
            seed_bytes.len(),
            encoding.name(),
        );
        std::fs::write(&src_path, &seed_bytes).expect("write seed fixture");

        let mut doc = Document::open_with_encoding(&src_path, encoding)
            .expect("open_with_encoding(seed, e) must succeed for representable bytes");

        prop_assert_eq!(
            doc.encoding(),
            encoding,
            "open must install the requested encoding contract for {}",
            encoding.name(),
        );

 // Apply only the edits whose payload is representable in `e`.
 // Non-representable payloads would be rejected by the encoded
 // insert path and are not part of Property 17's
 // hypothesis.
        for (pos, payload) in &edits {
            if !is_representable(payload, encoding) {
                continue;
            }
            let result = doc.try_insert(*pos, payload);
            prop_assert!(
                result.is_ok(),
                "try_insert with representable payload must succeed for {} \
                 (pos = {:?}, payload = {:?}, error = {:?})",
                encoding.name(),
                pos,
                payload,
                result.err(),
            );
        }

 // Snapshot the canonical decoded text right before save.
 // `text_lossy` for non-UTF-8 piece-tree / mmap backings
 // decodes through `encoding_rs::Encoding::decode_with_bom_removal`
 // (per `state.rs::text_lossy`), so the comparison is in
 // canonical UTF-8 regardless of the document's storage
 // encoding.
        let text_before_save = doc.text_lossy();

        doc.save_to(&saved_path).expect("save_to must succeed for representable edits");

        let reopened = Document::open_with_encoding(&saved_path, encoding)
            .expect("reopen saved file with the same encoding must succeed");
        let text_after_reopen = reopened.text_lossy();

        prop_assert_eq!(
            &text_after_reopen,
            &text_before_save,
            "save→reopen must yield decoded text identical to the in-memory \
             text just before save (encoding = {})",
            encoding.name(),
        );

 // Best-effort cleanup; tmp files may linger on shrink failures.
        let _ = std::fs::remove_file(&src_path);
        let _ = std::fs::remove_file(&saved_path);
        let _ = std::fs::remove_dir(&dir);
    }
}
