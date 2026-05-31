// Property 8: Utf16Engine::step distinguishes BMP and supplementary characters.
//
//
// For both `Utf16Engine<LittleEndian>` and `Utf16Engine<BigEndian>`
// `engine.step(bytes, p, bytes.len())` must return the byte size of the
// character starting at `p`, with the surrogate-aware semantics required
//
//
// * If `p` starts a BMP code unit (a `u16` not in the surrogate range
// i.e. outside `0xD800..=0xDFFF`), `step == 2`.
// * If `p` starts a high surrogate (`0xD800..=0xDBFF`) immediately
// followed by a low surrogate (`0xDC00..=0xDFFF`) — i.e. a complete
// supplementary character `U+10000..=U+10FFFF` — `step == 4`.
// * If `p` starts a high surrogate that is *not* followed by a low
// surrogate (truncated pair, end-of-input, or follow-up byte that is
// not a low surrogate), `step == 2`. The engine treats the lone unit
// as a malformed code unit and advances by one cell so iteration
// cannot stall.
// * If `p` starts a low surrogate cell on its own (e.g. in the middle
// of a supplementary pair when we walk the bytes by 2-byte cells)
// `step == 2` for the same reason.
// * If fewer than 2 bytes remain at `p`, `step == 0`.
//
// The strategy emits a sequence of "atoms" — BMP scalars, supplementary
// scalars (encoded as a high+low surrogate pair), and lone high
// surrogates — encodes the resulting `u16` code unit stream into both
// UTF-16LE and UTF-16BE bytes, and asserts the contract above at every
// 2-byte aligned offset in the byte sequence. The assertions live both
// in a per-atom form (so failures point at the kind of code unit that
// broke the property) and in a structural form computed directly from
// the bytes (so the oracle never trusts only the atom layout).
//
// The engine is reached through `qem::document::__test_support`, the
// `#[doc(hidden)]` re-export module introduced for the
// integration property tests under `tests/encoding_engine/`. The cases
// count is intentionally pinned at 64 for this spec.

use proptest::prelude::*;
use qem::document::__test_support::{engine_for_encoding, EncodingEngine};
use qem::DocumentEncoding;

/// One generator atom. Each atom produces one or more UTF-16 code units
/// when materialized through [`encode_atoms`].
#[derive(Debug, Clone, Copy)]
enum Atom {
 /// A BMP scalar in `U+0000..=U+D7FF` ∪ `U+E000..=U+FFFF`. Emits
 /// exactly one 2-byte code unit; `step` at its start must be `2`.
    Bmp(u16),
 /// A supplementary scalar in `U+10000..=U+10FFFF`. Emits two 2-byte
 /// code units (high + low surrogate). `step` at the high surrogate
 /// must be `4`; `step` at the low surrogate (if visited as a 2-byte
 /// cell on its own) must be `2`.
    Supplementary(u32),
 /// A lone high surrogate in `U+D800..=U+DBFF` with no paired low
 /// surrogate. Emits exactly one 2-byte code unit; `step` at its
 /// start must be `2` (malformed code unit).
    LoneHighSurrogate(u16),
}

/// Strategy for a BMP scalar value (any `u16` not in the surrogate
/// range `0xD800..=0xDFFF`). Two disjoint sub-ranges, sampled
/// uniformly so both halves of the BMP get exercised.
fn bmp_value() -> impl Strategy<Value = u16> {
    prop_oneof![(0u16..=0xD7FF), (0xE000u16..=0xFFFF),]
}

/// Strategy for a supplementary scalar value in `U+10000..=U+10FFFF`.
fn supplementary_value() -> impl Strategy<Value = u32> {
    0x10000u32..=0x10FFFF
}

/// Strategy for a lone high surrogate (`U+D800..=U+DBFF`).
fn lone_high_surrogate() -> impl Strategy<Value = u16> {
    0xD800u16..=0xDBFF
}

fn atom_strategy() -> impl Strategy<Value = Atom> {
 // Weighted so most atoms are well-formed (BMP or supplementary) and
 // the malformed case still fires often enough to exercise the
 // lone-high-surrogate branch every few cases.
    prop_oneof![
        4 => bmp_value().prop_map(Atom::Bmp),
        3 => supplementary_value().prop_map(Atom::Supplementary),
        1 => lone_high_surrogate().prop_map(Atom::LoneHighSurrogate),
    ]
}

fn atoms_strategy() -> impl Strategy<Value = Vec<Atom>> {
 // Up to 24 atoms gives byte sequences up to ~96 bytes; large enough
 // to exercise long mixed BMP + surrogate runs and small enough to
 // keep shrinking quick.
    prop::collection::vec(atom_strategy(), 0..=24)
}

/// Encodes an `Atom` slice into a `Vec<u8>` for the chosen endianness
/// returning the bytes plus the byte offset at which each atom starts.
/// The offset table makes per-atom assertions trivial: the `i`-th
/// recorded offset is the start of the `i`-th atom in `atoms`.
fn encode_atoms(atoms: &[Atom], endian: Endian) -> (Vec<u8>, Vec<(usize, Atom)>) {
    let mut bytes = Vec::with_capacity(atoms.len() * 4);
    let mut offsets = Vec::with_capacity(atoms.len());
    for atom in atoms {
        offsets.push((bytes.len(), *atom));
        match *atom {
            Atom::Bmp(unit) => push_unit(&mut bytes, unit, endian),
            Atom::Supplementary(scalar) => {
 // : encode the scalar as a UTF-16 surrogate pair.
 // High = 0xD800 + ((scalar - 0x10000) >> 10)
 // Low = 0xDC00 + ((scalar - 0x10000) & 0x3FF).
                let v = scalar - 0x10000;
                let high = 0xD800 + ((v >> 10) as u16);
                let low = 0xDC00 + ((v & 0x3FF) as u16);
                push_unit(&mut bytes, high, endian);
                push_unit(&mut bytes, low, endian);
            }
            Atom::LoneHighSurrogate(unit) => push_unit(&mut bytes, unit, endian),
        }
    }
    (bytes, offsets)
}

#[derive(Debug, Clone, Copy)]
enum Endian {
    Le,
    Be,
}

fn push_unit(bytes: &mut Vec<u8>, unit: u16, endian: Endian) {
    let pair = match endian {
        Endian::Le => unit.to_le_bytes(),
        Endian::Be => unit.to_be_bytes(),
    };
    bytes.extend_from_slice(&pair);
}

fn read_u16(bytes: &[u8], offset: usize, endian: Endian) -> u16 {
    let pair = [bytes[offset], bytes[offset + 1]];
    match endian {
        Endian::Le => u16::from_le_bytes(pair),
        Endian::Be => u16::from_be_bytes(pair),
    }
}

/// Reference oracle: classify the cell at `offset` directly from the
/// byte stream, mirroring the contract laid out in the file header.
/// Returns the expected value of `engine.step(bytes, offset, bytes.len())`.
fn expected_step(bytes: &[u8], offset: usize, endian: Endian) -> usize {
    let len = bytes.len();
    if offset + 2 > len {
        return 0;
    }
    let unit = read_u16(bytes, offset, endian);
    if (0xD800..=0xDBFF).contains(&unit) {
 // High surrogate — only forms a 4-byte character if followed by
 // a low surrogate.
        if offset + 4 <= len {
            let next = read_u16(bytes, offset + 2, endian);
            if (0xDC00..=0xDFFF).contains(&next) {
                return 4;
            }
        }
        return 2;
    }
 // Either a BMP code unit or a lone low surrogate cell mid-pair —
 // both step by 2.
    2
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

 /// Property 8: `Utf16Engine::step` distinguishes BMP and supplementary
 /// characters, returning `2` for any 2-byte code unit (BMP or lone /
 /// unpaired surrogate) and `4` for a complete high+low surrogate pair.
 /// The same byte stream is encoded for both LE and BE, and the engine
 /// for each endianness must agree with the oracle on every 2-byte
 /// aligned offset and on every recorded atom start.
    #[test]
    fn property_8_utf16_step_distinguishes_bmp_and_supplementary(
        atoms in atoms_strategy(),
    ) {
        for endian in [Endian::Le, Endian::Be] {
            let (bytes, atom_offsets) = encode_atoms(&atoms, endian);
            let encoding = match endian {
                Endian::Le => DocumentEncoding::utf16le(),
                Endian::Be => DocumentEncoding::utf16be(),
            };
            let engine: &dyn EncodingEngine = engine_for_encoding(encoding);
            prop_assert_eq!(
                engine.encoding(),
                encoding,
                "engine_for_encoding must return an engine bound to {}",
                encoding.name()
            );

 // (a) Per-atom assertion — failures point to the offending
 // atom kind. BMP and lone high surrogate atoms occupy 2
 // bytes (step == 2); supplementary atoms occupy 4 bytes
 // (step at the high surrogate == 4, step at the low
 // surrogate cell == 2).
            for (offset, atom) in atom_offsets.iter().copied() {
                let step = engine.step(&bytes, offset, bytes.len());
                match atom {
                    Atom::Bmp(unit) => prop_assert_eq!(
                        step,
                        2,
                        "BMP unit 0x{:04X} at offset {} (endian {:?}, len {}) must step by 2, got {}",
                        unit,
                        offset,
                        endian,
                        bytes.len(),
                        step
                    ),
                    Atom::Supplementary(scalar) => {
                        prop_assert_eq!(
                            step,
                            4,
                            "supplementary U+{:04X} at offset {} (endian {:?}, len {}) must step by 4 from its high surrogate, got {}",
                            scalar,
                            offset,
                            endian,
                            bytes.len(),
                            step
                        );
 // The low surrogate cell, when treated as the
 // start of a 2-byte cell on its own, must step
 // by 2 (lone low surrogate is malformed
 // advance by one code unit).
                        let low_offset = offset + 2;
                        let low_step =
                            engine.step(&bytes, low_offset, bytes.len());
                        prop_assert_eq!(
                            low_step,
                            2,
                            "low surrogate of supplementary U+{:04X} at offset {} (endian {:?}) must step by 2, got {}",
                            scalar,
                            low_offset,
                            endian,
                            low_step
                        );
                    }
                    Atom::LoneHighSurrogate(unit) => prop_assert_eq!(
                        step,
                        2,
                        "lone high surrogate 0x{:04X} at offset {} (endian {:?}, len {}) must step by 2 (malformed code unit), got {}",
                        unit,
                        offset,
                        endian,
                        bytes.len(),
                        step
                    ),
                }
            }

 // (b) Structural sweep across every 2-byte aligned offset in
 // the byte stream, including the trailing `bytes.len()`
 // boundary. Failures here catch oracle drift between the
 // atom view and the raw byte view (e.g. a generator that
 // accidentally emits bytes that look like a surrogate pair
 // across an atom boundary).
            let mut p = 0usize;
            while p <= bytes.len() {
                let step = engine.step(&bytes, p, bytes.len());
                let expected = expected_step(&bytes, p, endian);
                prop_assert_eq!(
                    step,
                    expected,
                    "step mismatch at offset {} (endian {:?}, len {}): expected {}, got {}",
                    p,
                    endian,
                    bytes.len(),
                    expected,
                    step
                );
 // Advance by at least 2 bytes so we visit every cell;
 // the trailing odd byte (if any) is covered by the
 // `expected_step` `offset + 2 > len` short-circuit.
                p += 2;
            }
        }
    }
}

// ============================================================================
// Property 11: `MultiByteEngine::step` matches `encoding_rs::Decoder`
// character boundaries.
// ============================================================================
//
//
// For each CJK kind handled by `MultiByteEngine` (`Shift_JIS`, `gb18030`
// `EUC-KR`), the strategy generates a `&str` that is fully representable
// in the target encoding, hands it to `encoding_rs::Encoding::encode` to
// obtain a byte sequence, and then walks the bytes two ways:
//
// 1. *Reference walk* — iterate `text.chars()`; for each `char`
// re-encode it on its own through `encoding_rs` and accumulate its
// byte length. The running sum gives the expected sequence of
// character boundaries (`0, len(c0), len(c0)+len(c1), …`). This
// mirrors `encoding_rs::Decoder` behaviour: every boundary the
// decoder would yield while replaying the encoded bytes is
// exactly one of these prefix sums.
// 2. *Engine walk* — start at offset `0` and step through the encoded
// bytes via `engine.step(bytes, p, bytes.len())`, recording the
// cursor at every iteration.
//
// The assertion is that the two boundary sequences are identical and
// that the engine never returns `step == 0` before reaching the end of
// the encoded slice (forward iteration must terminate). Strategy
// regexes are kept narrow so every generated character is representable
// in its target encoding without invoking `encoding_rs`'s HTML
// numeric-character-reference fallback; `had_unmappable` is still
// guarded by `prop_assume!` for defence in depth.
//
// The encoding regexes:
// - Shift_JIS: ASCII + Hiragana (`U+3041..=U+3093`) — every glyph in
// the chosen ranges has a valid 1- or 2-byte Shift_JIS encoding.
// - gb18030: ASCII + CJK Unified Ideographs (`U+4E00..=U+9FFF`) —
// mix of 1-byte ASCII and 2-byte / 4-byte CJK sequences.
// - EUC-KR: ASCII + Hangul Syllables (`U+AC00..=U+D7A3`) — the
// 11 172 KS X 1001 syllable block, every entry has a 2-byte EUC-KR
// encoding.
//
// `engine_for_encoding` is reached through the same hidden
// `qem::document::__test_support` re-export plugged in for
// Property 8; no MultiByteEngine concrete type is touched directly so
// the property exercises the same `&'static dyn EncodingEngine`
// dispatch the document layer uses.

use encoding_rs::{Encoding, EUC_KR, GB18030, SHIFT_JIS};

/// CJK encoding kinds covered by Property 11.
#[derive(Debug, Clone, Copy)]
enum CjkKindUnderTest {
    ShiftJis,
    Gb18030,
    EucKr,
}

impl CjkKindUnderTest {
 /// `encoding_rs` static for this kind. Used for `encode` calls in
 /// both the input encoding step and the reference per-char
 /// re-encoding.
    fn encoding_rs(self) -> &'static Encoding {
        match self {
            Self::ShiftJis => SHIFT_JIS,
            Self::Gb18030 => GB18030,
            Self::EucKr => EUC_KR,
        }
    }

 /// Canonical Qem `DocumentEncoding` for this kind. The canonical
 /// `encoding_rs` labels (`"Shift_JIS"`, `"gb18030"`, `"EUC-KR"`)
 /// are exactly what `engine_for_encoding` matches against when
 /// dispatching to `MultiByteEngine`.
    fn document_encoding(self) -> DocumentEncoding {
        let label = match self {
            Self::ShiftJis => "Shift_JIS",
            Self::Gb18030 => "gb18030",
            Self::EucKr => "EUC-KR",
        };
        DocumentEncoding::from_label(label)
            .unwrap_or_else(|| panic!("encoding_rs should know {label}"))
    }

 /// Regex for `proptest::string::string_regex` that emits only
 /// characters representable in this kind. Length is capped at 32
 /// chars so encoded byte sequences stay small (≤ 128 bytes for
 /// gb18030's 4-byte path) and shrinking remains quick.
    fn text_regex(self) -> &'static str {
        match self {
 // ASCII + Hiragana — fully covered by JIS X 0208 (Shift_JIS).
            Self::ShiftJis => "[a-z0-9\\u3041-\\u3093]{1,32}",
 // ASCII + CJK Unified Ideographs — every code point has a
 // valid gb18030 encoding (mix of 2- and 4-byte sequences).
            Self::Gb18030 => "[a-z0-9\\u4E00-\\u9FFF]{1,32}",
 // ASCII + Hangul Syllables — KS X 1001 covers the entire
 // U+AC00..=U+D7A3 block.
            Self::EucKr => "[a-z0-9\\uAC00-\\uD7A3]{1,32}",
        }
    }
}

/// Strategy that picks a CJK kind uniformly and generates a string
/// matching that kind's representable-character regex. The output pair
/// `(kind, text)` carries enough information for the property body to
/// pick the matching encoding without re-deriving it from the string.
fn cjk_kind_and_text_strategy() -> impl Strategy<Value = (CjkKindUnderTest, String)> {
    prop_oneof![
        proptest::string::string_regex(CjkKindUnderTest::ShiftJis.text_regex())
            .expect("Shift_JIS regex must compile")
            .prop_map(|s| (CjkKindUnderTest::ShiftJis, s)),
        proptest::string::string_regex(CjkKindUnderTest::Gb18030.text_regex())
            .expect("gb18030 regex must compile")
            .prop_map(|s| (CjkKindUnderTest::Gb18030, s)),
        proptest::string::string_regex(CjkKindUnderTest::EucKr.text_regex())
            .expect("EUC-KR regex must compile")
            .prop_map(|s| (CjkKindUnderTest::EucKr, s)),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

 /// Property 11: forward `step` over encoded CJK bytes produces the
 /// same character boundaries that `encoding_rs` would yield while
 /// decoding those bytes back to the original text.
    #[test]
    fn property_11_multibyte_step_matches_decoder_boundaries(
        (kind, text) in cjk_kind_and_text_strategy(),
    ) {
        let target_encoding = kind.encoding_rs();
        let document_encoding = kind.document_encoding();

 // Encode the input text into the target encoding. Discard
 // cases where any character is not representable: the regexes
 // above are tight enough that this should never trigger, but
 // the guard makes the property robust against future regex
 // tweaks.
        let (encoded_cow, _, had_unmappable) = target_encoding.encode(&text);
        prop_assume!(
            !had_unmappable,
            "discard case: input contains characters unmappable in target encoding"
        );
        let encoded: &[u8] = &encoded_cow;

 // Reference boundary list — walk the original text char by
 // char and re-encode each through the same `Encoding`. The
 // running sum of per-char byte lengths is the expected
 // sequence of character boundaries `MultiByteEngine::step`
 // must reproduce.
        let mut expected_boundaries: Vec<usize> = Vec::with_capacity(text.chars().count() + 1);
        expected_boundaries.push(0);
        let mut acc = 0usize;
        for ch in text.chars() {
            let mut buf = [0u8; 4];
            let ch_str: &str = ch.encode_utf8(&mut buf);
            let (ch_encoded, _, ch_had_unmappable) = target_encoding.encode(ch_str);
            prop_assume!(
                !ch_had_unmappable,
                "discard case: per-char re-encoding hit unmappable scalar"
            );
            acc += ch_encoded.len();
            expected_boundaries.push(acc);
        }
        prop_assert_eq!(
            acc,
            encoded.len(),
            "reference walk total ({}) must equal full encoded length ({}) for {} text {:?}",
            acc,
            encoded.len(),
            target_encoding.name(),
            text
        );

 // Engine boundary list — drive `engine.step` from offset 0 to
 // `encoded.len()` and record the cursor after every step.
        let engine: &dyn EncodingEngine = engine_for_encoding(document_encoding);
        prop_assert_eq!(
            engine.encoding(),
            document_encoding,
            "engine_for_encoding must hand back an engine bound to {}",
            document_encoding.name()
        );

        let mut engine_boundaries: Vec<usize> = Vec::with_capacity(expected_boundaries.len());
        engine_boundaries.push(0);
        let end = encoded.len();
        let mut p = 0usize;
        while p < end {
            let step = engine.step(encoded, p, end);
            prop_assert!(
                step > 0,
                "engine.step returned 0 at offset {} (encoded len {}, kind {:?}, text {:?})",
                p,
                end,
                kind,
                text
            );
            prop_assert!(
                p + step <= end,
                "engine.step ({}) at offset {} would overshoot end {} (kind {:?})",
                step,
                p,
                end,
                kind
            );
            p += step;
            engine_boundaries.push(p);
        }

        prop_assert_eq!(
            &engine_boundaries,
            &expected_boundaries,
            "engine boundaries differ from encoding_rs reference for kind {:?}, text {:?}, encoded {:02X?}",
            kind,
            text,
            encoded
        );
    }
}

// Property 11: MultiByteEngine::step matches encoding_rs::Decoder character boundaries.
