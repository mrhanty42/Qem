// Property 16: insert round-trips through encode/decode
//
//
// For any encoding `e ∈ Class A ∪ Class B` and any `&str` `s`
// composed only of Unicode scalars representable in `e`, after
//
// std::fs::write(path, ascii_seed_bytes_in_e);
// let mut doc = Document::open_with_encoding(path, e)?;
// doc.try_insert(TextPosition::new(0, 0), &s)?;
//
// reading the byte range that the encoded edit path appended into
// the document and decoding it through `e` MUST yield exactly `s`.
//
// The aligned-offset choice here is the start of line 0 (byte 0):
// the seed fixture is written without a leading BOM, so
// `leading_bom_len_for_encoded_insert` returns 0 and the
// `align_byte_offset(.., Backward)` floor of 0 is 0 itself in every
// engine. The inserted bytes therefore live at the very head of the
// piece-tree's storage, which makes the post-insert byte slice
// trivial to address: the first `encoded.len()` bytes of the
// document are the freshly written encoded payload.
//
// Why this matters. forbids transcoding the document into
// UTF-8 on the edit path; the encoded insert path in
// `try_insert_text_at_encoded` writes the bytes
// into `piece_table.add` *as the target encoding*. requires
// that the insertion offset is rounded onto a character boundary of
// the active encoding before the bytes land. Property 16 closes the
// loop on both: the bytes that arrived in storage decode back, byte
// for byte, into the input scalar sequence, with no UTF-8 detour.
//
// Strategy. The PBT runs over the ten Class A and Class B encodings
// listed in the task brief:
//
// * Class A single-byte: `windows-1251`, `KOI8-R`, `IBM866`
// `ISO-8859-1` (canonicalised by `encoding_rs` to
// `windows-1252`), `ISO-8859-15`.
// * Class B UTF-16: `UTF-16LE`, `UTF-16BE`.
// * Class B CJK multibyte: `Shift_JIS`, `gb18030`, `EUC-KR`.
//
// For every Class A / CJK encoding the seed fixture is the four
// ASCII bytes `b"ABC\n"`, which round-trip cleanly through every
// codec on the list (every one is an ASCII-superset). UTF-16 LE / BE
// hand-encode the same logical text via `str::encode_utf16` because
// `encoding_rs` deliberately refuses to *encode* into UTF-16 (the
// WHATWG spec marks the codec decode-only). The seed never contains
// a BOM so the insert always lands at byte offset 0.
//
// The payload alphabet is per-encoding so generated strings are
// almost always representable; a `prop_assume!(is_representable(...))`
// gate catches the few stragglers (e.g. ISO-8859-15's non-mapped
// chars in `U+00A4`, `U+00A6`, `U+00A8`, `U+00B4`, `U+00B8`
// `U+00BC..=U+00BE`). Representability is checked through the
// streaming primitive `encoding_rs::Encoder::encode_from_utf8`, which
// is exactly what the encoded insert path uses internally — its
// `had_unmappables` return flag is the same one
// `try_insert_text_at_encoded` consults before deciding whether to
// reject the input. UTF-16 covers every Unicode scalar by
// definition (surrogate pairs cover the supplementary planes), so
// the gate is a no-op there.
//
// `ProptestConfig::with_cases(64)` per the spec; `fresh_test_dir`
// honours `$env:TMP` / `$env:TEMP` for fixtures.

#[path = "mod.rs"]
#[allow(clippy::duplicate_mod)] // shared helpers module is also loaded by sibling integration tests
mod helpers;

use encoding_rs::Encoding;
use helpers::fresh_test_dir;
use proptest::prelude::*;
use qem::document::__test_support::bytes_for_alignment;
use qem::{Document, DocumentEncoding, TextPosition};

/// Class A ∪ Class B encoding labels exercised by Property 16. The
/// labels are spelled exactly as `encoding_rs` accepts them; the
/// canonicalisation rules of `encoding_rs` may collapse aliases
/// (e.g. `ISO-8859-1` to `windows-1252`), which is fine — the
/// property is asserted against the encoding actually installed on
/// the document, whatever its canonical name. The list mirrors the
/// task brief's set verbatim.
const ENCODING_LABELS: &[&str] = &[
    "windows-1251",
    "KOI8-R",
    "IBM866",
    "Shift_JIS",
    "gb18030",
    "EUC-KR",
    "UTF-16LE",
    "UTF-16BE",
    "ISO-8859-1",
    "ISO-8859-15",
];

/// Looks up the raw `encoding_rs::Encoding` for a label. The PBT
/// goes directly through `encoding_rs` (rather than the
/// `pub(crate)` `DocumentEncoding::as_encoding` accessor) so the
/// test stays inside the public surface; the canonical `Encoding`
/// pointer is still globally unique, so the result of
/// `decode_without_bom_handling` here decodes against the same
/// codec the document holds.
fn encoding_rs_for_label(label: &str) -> &'static Encoding {
    Encoding::for_label(label.as_bytes())
        .unwrap_or_else(|| panic!("encoding_rs should know label {label}"))
}

/// Returns `true` when every scalar in `text` is representable in
/// `encoding`.
///
/// The probe uses the streaming `encoding_rs::Encoder::encode_from_utf8`
/// primitive because that's the same code path the encoded edit
/// surface relies on internally — its `had_unmappables` return flag
/// is the contract gate for . UTF-16 LE / BE accept every
/// Unicode scalar by definition (surrogate pairs cover the
/// supplementary planes), and `encoding_rs` deliberately refuses to
/// *encode* into UTF-16 (the WHATWG spec marks the codec
/// decode-only), so the streaming probe is short-circuited there.
fn is_representable(text: &str, encoding: DocumentEncoding) -> bool {
    if matches!(encoding.name(), "UTF-16LE" | "UTF-16BE") {
        return true;
    }
    let enc = encoding_rs_for_label(encoding.name());
    let mut encoder = enc.new_encoder();
 // `max_buffer_length_from_utf8_if_no_unmappables` returns the
 // exact bound when the encode succeeds without redirects; on
 // overflow we fall back to a conservative `len * 4 + 8` cap
 // (every scalar in a Class A / CJK encoding fits within 4
 // bytes per char in the worst case).
    let cap = encoder
        .max_buffer_length_from_utf8_if_no_unmappables(text.len())
        .unwrap_or_else(|| text.len().saturating_mul(4).saturating_add(8));
    let mut buf = vec![0u8; cap.max(1)];
    let (_result, _read, _written, had_unmappables) =
        encoder.encode_from_utf8(text, &mut buf, true);
    !had_unmappables
}

/// Encodes `text` into the target encoding's bytes.
///
/// UTF-16 LE / BE bypass `encoding_rs::Encoding::encode` for the
/// reason the encoded insert path documents in `commands.rs`: the
/// WHATWG spec only labels UTF-16 as a decoder, so encode would
/// redirect to UTF-8. `str::encode_utf16` is total over Unicode and
/// matches what the open path expects. All other
/// encodings round-trip representable scalars through their
/// `Encoding::encode` cleanly.
fn encode_input(encoding: DocumentEncoding, text: &str) -> Vec<u8> {
    match encoding.name() {
        "UTF-16LE" => text.encode_utf16().flat_map(u16::to_le_bytes).collect(),
        "UTF-16BE" => text.encode_utf16().flat_map(u16::to_be_bytes).collect(),
        label => {
            let enc = encoding_rs_for_label(label);
            let (bytes, used, had_errors) = enc.encode(text);
            assert!(
                std::ptr::eq(used, enc) && !had_errors,
                "encode_input invoked on a payload that is not strictly representable in {label}; \
                 callers must guard with is_representable() first",
            );
            bytes.into_owned()
        }
    }
}

/// ASCII-only seed encoded into the target encoding's bytes. Every
/// codec on `ENCODING_LABELS` is an ASCII-superset on the low byte
/// range, so the seed round-trips cleanly through `Encoding::encode`
/// for the Class A / CJK paths and through `str::encode_utf16` for
/// UTF-16. The seed is intentionally tiny (under 16 bytes after
/// encoding) so each PBT case is cheap to materialise and cheap to
/// shrink. No BOM is written, which keeps `bom_len == 0` for the
/// encoded insert path — the inserted bytes therefore land at byte
/// offset 0 of the document, immediately addressable in the
/// post-insert byte slice.
fn seed_bytes(encoding: DocumentEncoding) -> Vec<u8> {
    let text = "ABC\n";
    match encoding.name() {
        "UTF-16LE" => text.encode_utf16().flat_map(u16::to_le_bytes).collect(),
        "UTF-16BE" => text.encode_utf16().flat_map(u16::to_be_bytes).collect(),
        _ => text.as_bytes().to_vec(),
    }
}

fn encoding_strategy() -> impl Strategy<Value = DocumentEncoding> {
    prop::sample::select(ENCODING_LABELS.to_vec()).prop_map(|label| {
        DocumentEncoding::from_label(label)
            .unwrap_or_else(|| panic!("encoding_rs should know {label}"))
    })
}

/// Per-encoding payload alphabet. Every regex range emits scalars
/// that are representable in the target encoding by construction;
/// the `prop_assume!(is_representable(...))` gate inside the test
/// catches the rare ISO-8859-15 / windows-1252 outliers (e.g. the
/// few `U+00A0..=U+00FF` characters that ISO-8859-15 remaps).
/// String lengths are bounded so each case stays small enough to
/// shrink quickly.
fn payload_strategy(encoding: DocumentEncoding) -> impl Strategy<Value = String> {
    let regex = match encoding.name() {
 // ASCII + Cyrillic capital/lowercase blocks — fully covered
 // by every Cyrillic Class A codec.
        "windows-1251" | "KOI8-R" | "IBM866" => r"[A-Za-z0-9 \u0410-\u042F\u0430-\u044F]{1,16}",
 // ASCII + Hiragana — fully covered by JIS X 0208.
        "Shift_JIS" => r"[A-Za-z0-9 \u3041-\u3093]{1,16}",
 // ASCII + a small CJK Unified Ideographs slice — every
 // scalar in the slice has a valid gb18030 encoding.
        "gb18030" => r"[A-Za-z0-9 \u4E00-\u4E2F]{1,16}",
 // ASCII + a small Hangul Syllables slice — KS X 1001 covers
 // the entire `U+AC00..=U+D7A3` block; the slice here keeps
 // the alphabet small for shrinking.
        "EUC-KR" => r"[A-Za-z0-9 \uAC00-\uAC1F]{1,16}",
 // UTF-16 covers all Unicode; mix ASCII with a few BMP Han
 // ideographs and Cyrillic letters so the engine sees both
 // single-unit (2-byte) and multi-unit (still 2-byte for
 // BMP) cells across both endianness markers.
        "UTF-16LE" | "UTF-16BE" => r"[A-Za-z0-9 \u4E00-\u4E2F\u0410-\u042F]{1,16}",
 // ASCII + Latin-1 supplement (`U+00A0..=U+00FF`). All 96
 // characters in the supplement are representable in
 // windows-1252; ISO-8859-15 remaps eight of them, which the
 // `prop_assume!` gate inside the test catches.
        "windows-1252" | "ISO-8859-15" => r"[A-Za-z0-9 \u00A0-\u00FF]{1,16}",
 // Defensive fallback: ASCII only. Reachable only if a new
 // encoding gets added to `ENCODING_LABELS` without a
 // matching alphabet entry above.
        _ => r"[A-Za-z0-9 ]{1,16}",
    };
    proptest::string::string_regex(regex).expect("valid payload regex")
}

/// Pairs the encoding with a representable payload. `prop_flat_map`
/// is required so the payload alphabet specialises to the
/// just-sampled encoding; without it every encoding would have to
/// share one global alphabet and the per-encoding alphabets would
/// over-filter (most cases skipped).
fn case_strategy() -> impl Strategy<Value = (DocumentEncoding, String)> {
    encoding_strategy().prop_flat_map(|encoding| {
        payload_strategy(encoding).prop_map(move |payload| (encoding, payload))
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

 /// Property 16. After `Document::open_with_encoding(seed, e)`
 /// followed by `try_insert(TextPosition::new(0, 0), &s)`,
 /// decoding the inserted byte range from the document via the
 /// active encoding `e` must yield exactly `s`.
 ///
 /// The test:
 /// 1. Generates an encoding and a representable payload via
 /// `case_strategy()`. The payload's representability is
 /// double-checked through the streaming
 /// `encoding_rs::Encoder::encode_from_utf8` primitive so
 /// the same `had_unmappables` flag the encoded edit path
 /// uses internally gates the test (here it just means we don't
 /// try to insert something the encoding can't carry).
 /// 2. Materialises a tiny ASCII-equivalent seed in `e`'s
 /// bytes under `fresh_test_dir(...)`. The seed has
 /// no BOM, so `leading_bom_len_for_encoded_insert` returns
 /// 0 and the encoded insert lands at byte offset 0 of the
 /// document.
 /// 3. Opens it with `Document::open_with_encoding(seed, e)`
 /// and calls `try_insert(TextPosition::new(0, 0), &s)`.
 /// Every call must succeed because the input is
 /// representable.
 /// 4. Reads the document's full byte stream through the
 /// crate-internal `bytes_for_alignment(doc)` (mirrors
 /// `Document::align_byte_offset`'s scan source — for a
 /// promoted piece-tree this returns
 /// `piece_table.read_range(0, total_len)`). Slices out the
 /// first `encoded_expected.len()` bytes; that's the byte
 /// range the encoded insert path appended.
 /// 5. Decodes the slice through
 /// `encoding.decode_without_bom_handling(...)`. The decode
 /// must yield `s` byte-for-byte and the `had_errors` flag
 /// must be `false` — the bytes arrived in storage exactly
 /// as the encoder wrote them, with no UTF-8 detour
 ///.
    #[test]
    fn property_16_insert_round_trips_through_encode_decode(
        (encoding, payload) in case_strategy(),
    ) {
 // Defensive: shrink occasionally produces an empty payload
 // (`{1,16}` range with proptest's regex sampler) or an
 // unrepresentable scalar (the `\u00A4`/`\u00A6`/etc.
 // ISO-8859-15 outliers). Both cases are out of scope for
 // Property 16 and dropping them keeps each PBT case cheap.
        prop_assume!(!payload.is_empty());
        prop_assume!(is_representable(&payload, encoding));

        let dir = fresh_test_dir("prop_insert_roundtrip");
        let src_path = dir.join("seed.bin");

 // Materialise the seed in the target encoding's bytes. No
 // BOM is written — the encoded insert path therefore sees
 // `bom_len == 0` and lands the inserted bytes at byte
 // offset 0 of the document, which is what the post-insert
 // slice arithmetic below relies on.
        let seed = seed_bytes(encoding);
        std::fs::write(&src_path, &seed).expect("write seed fixture");

        let mut doc = Document::open_with_encoding(&src_path, encoding)
            .expect("open_with_encoding(seed, e) must succeed for representable seed");

        prop_assert_eq!(
            doc.encoding(),
            encoding,
            "open must install the requested encoding contract for {}",
            encoding.name(),
        );

 // : TextPosition::new(0, 0) is line start of line 0
 // which is byte 0. Without a BOM in the seed, this is the
 // encoded insert path's aligned offset by construction —
 // every engine's `align_byte_offset(0, Backward)` returns 0.
        let _cursor = doc
            .try_insert(TextPosition::new(0, 0), &payload)
            .expect("try_insert must succeed for representable payload");

 // The encoded insert path must promote a non-UTF-8
 // storage-backed document to a piece-tree edit buffer
 //. If a regression sent us down the rope
 // bridge instead, `bytes_for_alignment` would return
 // canonical UTF-8 bytes rather than the target encoding's
 // bytes and the decode below would fail on every non-ASCII
 // payload — but we surface the regression eagerly here for
 // a clearer failure mode.
        prop_assert!(
            doc.has_piece_table(),
            "encoded insert must promote document to piece-tree (encoding {})",
            encoding.name(),
        );
        prop_assert!(
            !doc.has_rope(),
            "encoded insert must not transcode the document into a UTF-8 rope \
             (encoding {})",
            encoding.name(),
        );

 // The expected bytes the encoded path wrote into the add
 // buffer are exactly what `Encoding::encode` (or
 // `str::encode_utf16` for UTF-16) produces for the same
 // payload. Computing them out-of-band here gives the test
 // an independent oracle for both the insertion length and
 // the byte content.
        let encoded_expected = encode_input(encoding, &payload);

 // Bytes the document holds right now. For a non-UTF-8
 // piece-tree document this is `read_range(0, total_len)`
 // i.e. encoded payload (at offset 0) followed by the
 // original seed.
        let bytes = bytes_for_alignment(&doc);
        let total_expected_len = encoded_expected.len().saturating_add(seed.len());
        prop_assert_eq!(
            bytes.len(),
            total_expected_len,
            "post-insert document length must equal encoded payload + seed \
             (encoding {}, encoded {} bytes, seed {} bytes)",
            encoding.name(),
            encoded_expected.len(),
            seed.len(),
        );
        let inserted = &bytes[..encoded_expected.len()];

 // Bytes-for-bytes equality on the inserted range. .6
 // demands the encoded insert path appends target-encoding
 // bytes verbatim, with no UTF-8 detour — so the slice must
 // equal `encoded_expected` exactly.
        prop_assert_eq!(
            inserted,
            encoded_expected.as_slice(),
            "inserted byte range must equal encoder output (encoding {})",
            encoding.name(),
        );

 // Decode the inserted slice through the same encoding the
 // document holds. `decode_without_bom_handling` is the
 // appropriate accessor for a slice that does not start
 // with a BOM (the encoded insert never writes one).
        let enc_rs = encoding_rs_for_label(encoding.name());
        let (decoded, had_errors) = enc_rs.decode_without_bom_handling(inserted);
        prop_assert!(
            !had_errors,
            "decoding the inserted byte range must not surface decode errors \
             (encoding {})",
            encoding.name(),
        );
        prop_assert_eq!(
            decoded.as_ref(),
            payload.as_str(),
            "decoded inserted bytes must equal the original payload \
             (encoding {})",
            encoding.name(),
        );

 // Best-effort cleanup; tmp files may linger on shrink failures.
        let _ = std::fs::remove_file(&src_path);
        let _ = std::fs::remove_dir(&dir);
    }
}
