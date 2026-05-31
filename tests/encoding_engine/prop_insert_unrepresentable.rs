// Property 13: insert with unrepresentable code points returns error without mutation
//
//
// For any encoding `e ∈ Class A ∪ Class B` and any `&str` that
// contains at least one Unicode scalar value not representable in
// `e`, `Document::try_insert(position, text)` MUST return
// `Err(DocumentError::Encoding{ reason: UnrepresentableText, .. })`
// `document.is_dirty()` MUST remain `false`, and the document's
// total byte size MUST be unchanged. If a piece-tree edit buffer
// already exists, its add buffer MUST NOT have grown by even a
// single byte ("no mutation on unrepresentable input").
//
// Strategy. The PBT runs over the five Class A encodings whose
// repertoires are small enough to make unrepresentable code points
// trivial to generate:
//
// * `windows-1251`, `KOI8-R`, `IBM866` — Cyrillic + ASCII.
// * `ISO-8859-1` (canonicalised by `encoding_rs` to `windows-1252`)
// and `ISO-8859-15` — Latin + ASCII.
//
// Each case picks one of these encodings, builds a tiny seed file
// containing only ASCII (representable in every Class A and Class B
// encoding), opens it through `Document::open_with_encoding`, then
// calls `try_insert` with an injected unrepresentable scalar. The
// scalar pool is `{'你', 'こ', '中', '🙂'}` — CJK ideographs and an
// emoji — none of which fall into any of the five chosen Class A
// repertoires. An optional ASCII prefix is interleaved so the test
// also covers strings whose first character is representable but
// later content is not; must hold for *every* placement of the
// unrepresentable scalar.
//
// The pre-call / post-call snapshot uses three observables:
//
// 1. `Document::text_lossy()` — the canonical logical text of the
// document, decoded through `encoding_rs` for non-UTF-8
// backings (see `state.rs::text_lossy`). Equal pre- and
// post-call values prove no byte landed in the piece-tree add
// buffer (an inserted byte would surface in the decoded text).
// 2. `Document::is_dirty()` — must stay `false` because the
// encoded path bails before calling `mark_dirty()`.
// 3. `Document::has_piece_table()` — the seed file is mmap-only
// and the encoded path must not promote it. Confirms no
// add-buffer was even allocated.
//
// `ProptestConfig::with_cases(64)` per the spec.

#[path = "mod.rs"]
#[allow(clippy::duplicate_mod)] // shared helpers module is also loaded by sibling integration tests
mod helpers;

use helpers::fresh_test_dir;
use proptest::prelude::*;
use qem::{Document, DocumentEncoding, DocumentEncodingErrorKind, DocumentError, TextPosition};

/// Class A encodings with small repertoires — easy to construct
/// unrepresentable scalars against. The labels are spelled exactly
/// as `encoding_rs` accepts them; `ISO-8859-1` is canonicalised to
/// `windows-1252` internally, which is fine for this property
/// because the contract is about `try_insert` rejecting
/// unrepresentable scalars under the encoding actually installed on
/// the document.
const SMALL_REPERTOIRE_LABELS: &[&str] = &[
    "windows-1251",
    "KOI8-R",
    "IBM866",
    "ISO-8859-1",
    "ISO-8859-15",
];

/// Pool of Unicode scalars that are not representable in any of the
/// five small-repertoire Class A encodings above. CJK ideographs and
/// emoji sit far outside every Cyrillic and Latin codepage in the
/// list, so any of these chars is guaranteed to trip the
/// `had_unmappable` branch in `try_insert_text_at_encoded`.
const UNREPRESENTABLE_SCALARS: &[char] = &['\u{4F60}', '\u{3053}', '\u{4E2D}', '\u{1F642}'];

/// Generates an encoding from the small-repertoire set.
fn encoding_strategy() -> impl Strategy<Value = DocumentEncoding> {
    prop::sample::select(SMALL_REPERTOIRE_LABELS.to_vec()).prop_map(|label| {
        DocumentEncoding::from_label(label)
            .unwrap_or_else(|| panic!("encoding_rs should know {label}"))
    })
}

/// Generates an unrepresentable scalar from the curated pool.
fn unrepresentable_scalar_strategy() -> impl Strategy<Value = char> {
    prop::sample::select(UNREPRESENTABLE_SCALARS.to_vec())
}

/// Generates a small ASCII fragment (representable in every Class A
/// and Class B encoding). Bounded at 16 chars to keep cases cheap.
fn ascii_fragment_strategy() -> impl Strategy<Value = String> {
    proptest::string::string_regex(r"[A-Za-z0-9 ]{0,16}").expect("valid ASCII regex")
}

/// Generates an `&str` that contains at least one unrepresentable
/// scalar. The unrepresentable scalar may appear at the start, in
/// the middle, or at the end of the string; an optional ASCII
/// prefix and suffix are interleaved so is exercised across
/// placements (a representable prefix must not "leak" into the add
/// buffer before the encoder hits the unrepresentable scalar).
fn payload_with_unrepresentable_strategy() -> impl Strategy<Value = String> {
    (
        ascii_fragment_strategy(),
        unrepresentable_scalar_strategy(),
        ascii_fragment_strategy(),
    )
        .prop_map(|(prefix, bad, suffix)| {
            let mut out = String::with_capacity(prefix.len() + 4 + suffix.len());
            out.push_str(&prefix);
            out.push(bad);
            out.push_str(&suffix);
            out
        })
}

/// Insertion position. Restricted to (0, 0) plus a few small offsets
/// so cases are deterministic and don't depend on the seed file's
/// layout. The seed file is single-line ASCII so any (0, col0) pair
/// is valid; clamping inside `try_insert` handles out-of-range
/// columns gracefully.
fn position_strategy() -> impl Strategy<Value = TextPosition> {
    (0usize..=0, 0usize..=8).prop_map(|(line0, col0)| TextPosition::new(line0, col0))
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

 /// Property 13: `try_insert` with an unrepresentable scalar in
 /// the input must return `Err(UnrepresentableText)` without
 /// mutating the document.
 ///
 /// The test:
 /// 1. Materialises a tiny ASCII seed file under
 /// `fresh_test_dir(...)`.
 /// 2. Opens it with one of the five small-repertoire Class A
 /// encodings via `Document::open_with_encoding`. The seed
 /// contains only ASCII so the open succeeds for every
 /// candidate encoding and lands as `rope: None,
 /// piece_table: None, storage: Some(mmap)` (Class A
 /// native open path).
 /// 3. Snapshots `text_lossy()` and `is_dirty()` before the
 /// insert call.
 /// 4. Calls `try_insert(position, payload)` where `payload`
 /// contains at least one unrepresentable scalar. Asserts
 /// the call returns
 /// `Err(DocumentError::Encoding {
 /// operation: "insert"
 /// reason: UnrepresentableText
 /// encoding: e
 /// ..
 /// })`.
 /// 5. Re-snapshots `text_lossy()` and `is_dirty()` and
 /// `has_piece_table()`. Asserts:
 /// * the decoded text is unchanged byte-for-byte;
 /// * `is_dirty()` is still `false`;
 /// * the document was not promoted to a piece-tree
 /// edit buffer (the encoded path bails before calling
 /// `prepare_edit_at`, so no add buffer can exist).
    #[test]
    fn property_13_insert_unrepresentable_returns_error_without_mutation(
        encoding in encoding_strategy(),
        payload in payload_with_unrepresentable_strategy(),
        position in position_strategy(),
    ) {
        let dir = fresh_test_dir("prop_insert_unrepresentable");
        let src_path = dir.join("seed.txt");
 // ASCII-only seed: every Class A encoding can decode it
 // verbatim, and `text_lossy()` will return the same string
 // pre- and post-call as long as the document is not
 // mutated.
        let seed = b"hello world\n";
        std::fs::write(&src_path, seed).expect("write seed fixture");

        let mut doc = Document::open_with_encoding(&src_path, encoding)
            .expect("open_with_encoding should succeed for ASCII seed");

 // Sanity: the native open path for Class A leaves the
 // document mmap-only. If that ever changes we want this
 // property test to fail loudly rather than silently
 // exercise the rope bridge.
        prop_assert!(
            !doc.has_piece_table(),
            "Class A native open should not allocate a piece-tree on open",
        );
        prop_assert!(
            !doc.has_rope(),
            "Class A native open should not materialise a UTF-8 rope",
        );
        prop_assert!(!doc.is_dirty(), "freshly opened document must be clean");

        let text_before = doc.text_lossy();

        let result = doc.try_insert(position, &payload);

 // : the call must fail with the typed
 // `UnrepresentableText` reason. The `operation` tag must be
 // `"insert"` and the carried encoding must match the
 // document's contract — frontends rely on both fields to
 // route the error.
        match result {
            Err(DocumentError::Encoding {
                operation,
                encoding: failed_encoding,
                reason,
                ..
            }) => {
                prop_assert_eq!(
                    operation,
                    "insert",
                    "DocumentError::Encoding.operation must be \"insert\" for try_insert failures",
                );
                prop_assert_eq!(
                    failed_encoding,
                    encoding,
                    "DocumentError::Encoding.encoding must match the document's contract",
                );
                prop_assert!(
                    matches!(reason, DocumentEncodingErrorKind::UnrepresentableText),
                    "expected UnrepresentableText, got {:?}",
                    reason,
                );
            }
            Err(other) => {
                prop_assert!(
                    false,
                    "expected Err(DocumentError::Encoding {{ UnrepresentableText, .. }}), \
                     got Err({:?})",
                    other,
                );
            }
            Ok(cursor) => {
                prop_assert!(
                    false,
                    "try_insert with unrepresentable scalar must fail, got Ok({:?})",
                    cursor,
                );
            }
        }

 // : post-conditions on the document.
        prop_assert!(
            !doc.is_dirty(),
            "is_dirty() must remain false after a rejected encoded insert",
        );
        prop_assert!(
            !doc.has_piece_table(),
            "encoded insert path must not promote the document to a piece-tree \
             when the input is unrepresentable (no add-buffer growth)",
        );
        prop_assert!(
            !doc.has_rope(),
            "encoded insert path must not materialise a rope when the input is \
             unrepresentable",
        );

        let text_after = doc.text_lossy();
        prop_assert_eq!(
            &text_after,
            &text_before,
            "document text must be byte-identical to its pre-call snapshot \
             after a rejected encoded insert (encoding = {})",
            encoding.name(),
        );

 // The seed file on disk must also be untouched — the
 // encoded path never writes to disk, but a regression that
 // accidentally triggered preserve-save would surface here.
        let on_disk = std::fs::read(&src_path).expect("read seed fixture back");
        prop_assert_eq!(
            on_disk.as_slice(),
            seed.as_slice(),
            "seed file on disk must remain byte-identical after rejected insert",
        );

 // Best-effort cleanup; tmp files may linger on shrink failures.
        let _ = std::fs::remove_file(&src_path);
        let _ = std::fs::remove_dir(&dir);
    }
}
