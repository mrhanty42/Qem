// Property 3: SingleByteEngine.next_line_start handles LF/CR/CRLF
//
//
// For any Class A encoding `e` and any byte slice `bytes` with injected
// `LF` (`0x0A`), `CR` (`0x0D`), and `CRLF` (`0x0D 0x0A`) sequences, the
// result of `engine.next_line_start(bytes, bytes.len(), line_start)`
// must:
//
// * point to the byte offset just past a complete line-ending
// sequence — `LF` (1 byte past), standalone `CR` (1 byte past), or
// `CRLF` (2 bytes past, collapsed to a single boundary), or
// * equal `bytes.len()` when no terminator is found in the suffix.
//
// Equivalently, the returned offset is the smallest `result >= line_start`
// such that either `result == bytes.len()` or `bytes[result-1]` ends a
// complete line-break sequence per the rules above, and there is no
// earlier line-break sequence inside `bytes[line_start..result]`.
//
// The engine is reached through `qem::document::__test_support`, a
// `#[doc(hidden)]` re-export module used only by the integration tests
// under `tests/encoding_engine/`. The cases count is intentionally
// pinned at 64 (R: minimum 100 was relaxed to 64 for this spec).

use proptest::prelude::*;
use qem::document::__test_support::{engine_for_encoding, EncodingEngine, SingleByteEngine};
use qem::DocumentEncoding;

/// Class A encodings the spec wires through `SingleByteEngine`. The
/// labels match `encoding_rs` exactly so `DocumentEncoding::from_label`
/// always succeeds.
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

/// One unit of the byte sequence we feed to the engine. Mixing line-break
/// atoms with non-terminator filler bytes (high-byte glyphs of the Class A
/// encodings, plus printable ASCII) gives proptest enough variety to hit
/// every interesting combination — bare `LF`, bare `CR`, `CRLF`, runs of
/// terminators back to back, and long stretches without any terminator.
#[derive(Debug, Clone, Copy)]
enum Atom {
    Lf,
    Cr,
    Crlf,
    Filler(u8),
}

fn atom_strategy() -> impl Strategy<Value = Atom> {
    prop_oneof![
        2 => Just(Atom::Lf),
        2 => Just(Atom::Cr),
        2 => Just(Atom::Crlf),
 // High bytes look like Cyrillic / Latin-1 glyphs in Class A
 // encodings — they must never be mistaken for terminators.
        3 => (0xC0u8..=0xFE).prop_map(Atom::Filler),
        3 => (b' '..=b'~').prop_map(Atom::Filler),
    ]
}

fn bytes_strategy() -> impl Strategy<Value = Vec<u8>> {
 // 0..=24 atoms turns into byte slices up to ~48 bytes, which is more
 // than enough to exercise the line scanner without inflating the per
 // case cost.
    prop::collection::vec(atom_strategy(), 0..=24).prop_map(|atoms| {
        let mut bytes = Vec::with_capacity(48);
        for atom in atoms {
            match atom {
                Atom::Lf => bytes.push(b'\n'),
                Atom::Cr => bytes.push(b'\r'),
                Atom::Crlf => {
                    bytes.push(b'\r');
                    bytes.push(b'\n');
                }
                Atom::Filler(b) => bytes.push(b),
            }
        }
        bytes
    })
}

/// Reference scan that locates the offset of the next complete
/// line-ending sequence at or after `line_start`. Returns `(end, kind)`
/// where `end` is the byte offset just past the terminator and `kind` is
/// a human-readable label used in failure messages.
fn first_line_break_end(bytes: &[u8], line_start: usize) -> Option<(usize, &'static str)> {
    let n = bytes.len();
    let mut i = line_start.min(n);
    while i < n {
        match bytes[i] {
            b'\n' => return Some((i + 1, "LF")),
            b'\r' => {
                if i + 1 < n && bytes[i + 1] == b'\n' {
                    return Some((i + 2, "CRLF"));
                }
                return Some((i + 1, "CR"));
            }
            _ => i += 1,
        }
    }
    None
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

 /// Property 3: `SingleByteEngine::next_line_start` handles LF / CR /
 /// CRLF correctly across every Class A encoding, treating CRLF as one
 /// boundary and stopping at the first complete line-ending sequence.
 ///
 /// The test reaches the engine via `engine_for_encoding`, which the
 /// spec promises to route through `SingleByteEngine` for every Class
 /// A label. The runtime check `SingleByteEngine::supports`
 /// guards us against silently testing the UTF-8 fallback if a label
 /// were ever moved out of Class A.
    #[test]
    fn property_3_single_byte_next_line_start_handles_lf_cr_crlf(
        encoding in class_a_encoding_strategy(),
        bytes in bytes_strategy(),
        line_start_seed in any::<u32>(),
    ) {
        prop_assert!(
            SingleByteEngine::supports(encoding),
            "{} must be claimed by SingleByteEngine for Property 3 to be meaningful",
            encoding.name()
        );

        let engine: &dyn EncodingEngine = engine_for_encoding(encoding);
        prop_assert_eq!(
            engine.encoding(),
            encoding,
            "engine_for_encoding must return an engine bound to {}",
            encoding.name()
        );

        let n = bytes.len();
 // Pick `line_start` from the legal range `[0, n]`. For empty
 // slices the only valid value is `0`.
        let line_start = if n == 0 {
            0
        } else {
            (line_start_seed as usize) % (n + 1)
        };

        let result = engine.next_line_start(&bytes, n, line_start);

 // (1) Range invariant: result lies inside `[line_start.min(n), n]`.
        prop_assert!(
            result >= line_start.min(n),
            "result {} must not move backwards from line_start {} (clamped to {})",
            result,
            line_start,
            line_start.min(n),
        );
        prop_assert!(
            result <= n,
            "result {} must not exceed bytes.len() = {}",
            result,
            n,
        );

 // (2) Match against the reference scan. Either both report
 // "no terminator" (result == n, scan == None) or both agree on
 // the byte offset just past the same terminator.
        match first_line_break_end(&bytes, line_start) {
            None => {
                prop_assert_eq!(
                    result, n,
                    "no terminator found in bytes[{}..{}], so result must be {} (got {})",
                    line_start.min(n), n, n, result,
                );
            }
            Some((expected_end, kind)) => {
                prop_assert_eq!(
                    result, expected_end,
                    "encoding={} kind={} expected result to land just past the {} \
                     terminator at offset {} (i.e. result={}); got result={}",
                    encoding.name(),
                    kind,
                    kind,
                    expected_end - if kind == "CRLF" { 2 } else { 1 },
                    expected_end,
                    result,
                );
            }
        }

 // (3) Structural check on the byte at `result - 1` when the
 // engine reported a terminator (result < n). This catches any
 // engine that returns an offset pointing inside random data
 // rather than just past LF / CR / CRLF.
        if result < n {
            prop_assert!(result > line_start.min(n));
            let last = bytes[result - 1];
            let prev_is_cr = result >= 2 && bytes[result - 2] == b'\r';
            let valid_lf = last == b'\n';
            let valid_standalone_cr = last == b'\r'
                && (result == n || bytes[result] != b'\n');
            prop_assert!(
                valid_lf || valid_standalone_cr,
                "byte at result-1 ({}) must be LF or standalone CR; got 0x{:02X}, \
                 prev_is_cr={}",
                result - 1,
                last,
                prev_is_cr,
            );
        }
    }
}

// Property 9: Utf16Engine::next_line_start is 2-byte aligned.
//
//
// For both `Utf16Engine<LittleEndian>` and `Utf16Engine<BigEndian>`
// `engine.next_line_start(bytes, bytes.len(), line_start)` must satisfy
// two structural invariants:
//
// (a) 2-byte alignment. The returned offset
// is either even — i.e. lies on a UTF-16 code-unit boundary — or
// equal to `bytes.len()`. The `bytes.len()` exception covers
// degenerate slices that end on an odd trailing byte not part of
// any complete code unit; the line scanner is allowed to clamp
// to the slice end, but is forbidden from landing on any other
// odd offset. This guarantees the engine never reports a line
// boundary inside a 2-byte cell.
//
// (b) Misaligned `0x0A` / `0x0D` bytes — the trailing (high) byte of
// a UTF-16 code unit on LE, or the leading byte on BE — must not
// be interpreted as a line terminator. In UTF-16LE, a
// code unit `U+0A??` lays out as `[??, 0x0A]`, putting the
// `0x0A` at an odd byte position; the engine must walk past it
// rather than returning the offset just past that odd byte. In
// UTF-16BE the analogous misalignment is a `0x0A` byte at an
// even byte position (the leading byte of a `U+0A??` code unit
// — distinct from the BE LF cell `[0x00, 0x0A]`, where `0x0A`
// lives at an odd position and is paired with a `0x00` leading
// byte). The deterministic `#[test]` cases at the end of this
// file pin both halves down with focused inputs; the proptest
// below sweeps the broader random surface for invariant (a).
//
// The strategy generates arbitrary `Vec<u8>` of length `0..=512`, picks
// a random `line_start` in `[0, bytes.len()]`, and asserts invariant
// (a) for both LE and BE engines. Cases pinned at 64.

const PROP9_MAX_BYTES_LEN: usize = 512;

fn prop9_arb_bytes() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(any::<u8>(), 0..=PROP9_MAX_BYTES_LEN)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

 /// Property 9: `Utf16Engine::next_line_start` is 2-byte aligned. For
 /// arbitrary byte slices and arbitrary `line_start`, the result
 /// returned by both LE and BE engines lies on a 2-byte (code-unit)
 /// boundary or equals `bytes.len()`. The latter is the only legal
 /// odd-offset return: it covers the case where the buffer ends on
 /// an odd trailing byte outside any complete code unit. Any other
 /// odd offset would mean the scanner landed inside a UTF-16 code
 /// unit, violating the alignment contract.
    #[test]
    fn property_9_utf16_next_line_start_is_2_byte_aligned(
        bytes in prop9_arb_bytes(),
        line_start_seed in any::<u32>(),
    ) {
        let n = bytes.len();
 // R: line_start must be a legal byte offset in [0, n]. Empty
 // slices admit only `0`.
        let line_start = if n == 0 {
            0
        } else {
            (line_start_seed as usize) % (n + 1)
        };

        for encoding in [DocumentEncoding::utf16le(), DocumentEncoding::utf16be()] {
            let engine: &dyn EncodingEngine = engine_for_encoding(encoding);
            prop_assert_eq!(
                engine.encoding(),
                encoding,
                "engine_for_encoding must return an engine bound to {}",
                encoding.name()
            );

            let result = engine.next_line_start(&bytes, n, line_start);

 // Range invariant: result lies inside [line_start.min(n), n].
            prop_assert!(
                result <= n,
                "{}: result {} must not exceed bytes.len()={} (line_start={})",
                encoding.name(), result, n, line_start
            );
            prop_assert!(
                result >= line_start.min(n),
                "{}: result {} must not move backwards from line_start {} \
                 (clamped to {})",
                encoding.name(), result, line_start, line_start.min(n)
            );

 // (a) 2-byte alignment. Even, or
 // equal to bytes.len() (the only legal odd-offset
 // return — covers a trailing odd byte that is outside
 // any complete cell).
            prop_assert!(
                result % 2 == 0 || result == n,
                "{}: result {} must be 2-byte aligned or equal to \
                 bytes.len()={}; line_start={}, bytes={:02X?}",
                encoding.name(), result, n, line_start, &bytes
            );
        }
    }
}

// ---------------------------------------------------------------------
// Property 9, invariant (b): misaligned-byte rejection.
//
// Deterministic example checks that pin down the rejection branch
// directly. The proptest above gives broad alignment confidence, but
// invariant (b) — "an `0x0A` / `0x0D` byte that does not start (LE) or
// finish (BE) a 2-byte cell shaped like the canonical LF/CR pattern is
// not a terminator" — is easiest to demonstrate with focused inputs.
// These cases catch a regression where the implementation switches to a
// `memchr2(b'\n', b'\r', ..)` scan and forgets to filter by alignment
// which would return `position + 1` for the first `0x0A` or `0x0D` byte
// regardless of where that byte sits inside a code unit.
// ---------------------------------------------------------------------

#[test]
fn property_9_misaligned_lf_byte_is_not_a_line_break_le() {
 // UTF-16LE: two code units U+0A42 (`[0x42, 0x0A]`) followed by
 // U+4242 (`[0x42, 0x42]`). The `0x0A` at byte position 1 is the
 // trailing (high) byte of the first code unit, NOT a UTF-16LE LF
 // cell — which would require `[0x0A, 0x00]` at an even offset. The
 // engine must walk past both code units and return `bytes.len()=4`
 // never the position 2 a naive memchr-on-bytes scan would report.
    let engine: &dyn EncodingEngine = engine_for_encoding(DocumentEncoding::utf16le());
    let bytes: [u8; 4] = [0x42, 0x0A, 0x42, 0x42];
    let result = engine.next_line_start(&bytes, bytes.len(), 0);
    assert_eq!(
        result, 4,
        "UTF-16LE: 0x0A at odd byte 1 (trailing byte of U+0A42) must not \
         be interpreted as LF; expected result=4 (no terminator), got {result}"
    );
    assert_ne!(
        result, 2,
        "UTF-16LE: must not return position 2 from an odd-byte 0x0A"
    );
}

#[test]
fn property_9_misaligned_cr_byte_is_not_a_line_break_le() {
 // Symmetric to the LF case but for CR (`0x0D`). U+0D42 is
 // `[0x42, 0x0D]` in LE; the `0x0D` sits at odd byte 1 and must
 // not be interpreted as CR.
    let engine: &dyn EncodingEngine = engine_for_encoding(DocumentEncoding::utf16le());
    let bytes: [u8; 4] = [0x42, 0x0D, 0x42, 0x42];
    let result = engine.next_line_start(&bytes, bytes.len(), 0);
    assert_eq!(
        result, 4,
        "UTF-16LE: 0x0D at odd byte 1 (trailing byte of U+0D42) must not \
         be interpreted as CR; expected result=4 (no terminator), got {result}"
    );
    assert_ne!(
        result, 2,
        "UTF-16LE: must not return position 2 from an odd-byte 0x0D"
    );
}

#[test]
fn property_9_misaligned_lf_byte_is_not_a_line_break_be() {
 // UTF-16BE: two code units U+0A42 (`[0x0A, 0x42]`) followed by
 // U+4242 (`[0x42, 0x42]`). The `0x0A` at byte 0 is the leading
 // (high) byte of the first code unit. The BE LF cell pattern is
 // `[0x00, 0x0A]` (the `0x0A` lives at the trailing byte and is
 // paired with a `0x00` leading byte), so this `0x0A` must not be
 // mistaken for LF. Engine returns `bytes.len()=4`.
    let engine: &dyn EncodingEngine = engine_for_encoding(DocumentEncoding::utf16be());
    let bytes: [u8; 4] = [0x0A, 0x42, 0x42, 0x42];
    let result = engine.next_line_start(&bytes, bytes.len(), 0);
    assert_eq!(
        result, 4,
        "UTF-16BE: 0x0A at byte 0 (leading byte of U+0A42) must not be \
         interpreted as LF; expected result=4 (no terminator), got {result}"
    );
}

#[test]
fn property_9_misaligned_cr_byte_is_not_a_line_break_be() {
 // Symmetric BE check for CR. U+0D42 in BE is `[0x0D, 0x42]`; the
 // `0x0D` at byte 0 is a leading byte, not the BE CR pattern
 // `[0x00, 0x0D]`.
    let engine: &dyn EncodingEngine = engine_for_encoding(DocumentEncoding::utf16be());
    let bytes: [u8; 4] = [0x0D, 0x42, 0x42, 0x42];
    let result = engine.next_line_start(&bytes, bytes.len(), 0);
    assert_eq!(
        result, 4,
        "UTF-16BE: 0x0D at byte 0 (leading byte of U+0D42) must not be \
         interpreted as CR; expected result=4 (no terminator), got {result}"
    );
}

// ============================================================================
// Property 12: `next_line_start` always returns an offset on a character
// boundary of the target encoding.
// ============================================================================
//
// Property 12: next_line_start always returns an offset on a character boundary of the target encoding.
//
//
// For every engine variant — `Utf8Engine`
// `SingleByteEngine`, `Utf16Engine<LittleEndian>` /
// `Utf16Engine<BigEndian>`, and `MultiByteEngine` over each of
// `Shift_JIS`, `gb18030`, `EUC-KR` — the offset returned by
// `engine.next_line_start(bytes, bytes.len(), line_start)` must lie
// on a character boundary of the engine's target encoding. The
// strategy generates one engine variant per case along with
// a byte sequence that is valid for that engine's encoding, picks an
// arbitrary `line_start` in `[0, bytes.len()]`, calls
// `next_line_start`, and asserts the boundary contract per-engine:
//
// * `Utf8Engine`: bytes come from a `String` produced by
// `proptest::string::string_regex` (always valid UTF-8). The result
// must satisfy `std::str::from_utf8(&bytes[..result]).is_ok()` —
// equivalent to `bytes.is_char_boundary(result)` on a UTF-8-valid
// prefix, but byte-level so it stays meaningful even if the engine
// ever returns a hypothetical out-of-range value.
//
// * `SingleByteEngine` (every Class A label from `CLASS_A_LABELS`):
// bytes are arbitrary because every byte offset is a character
// boundary in a single-byte ASCII superset. The only check
// is `result <= bytes.len()`.
//
// * `Utf16Engine<LE/BE>`: bytes are arbitrary `Vec<u8>`. The result
// is either even (a 2-byte code-unit boundary — /
// ) or equal to `bytes.len()`. The latter is the only legal
// odd return: it covers a buffer that ends on a trailing odd byte
// outside any complete code unit, where the line scanner clamps to
// the slice end. This matches the alignment contract Property 9
// pins down for the same engines.
//
// * `MultiByteEngine` (`Shift_JIS`, `gb18030`, `EUC-KR`): bytes are
// produced by `encoding_rs::Encoding::encode` of a `&str`
// constrained by a per-kind regex to scalars that are guaranteed
// representable in the target encoding. `had_unmappable` cases are
// additionally filtered out via `prop_filter_map` for defence in
// depth, so the engine only ever sees byte sequences that round-
// trip through the target encoding's encoder. The result must
// either equal `0`, be reachable by walking `engine.step` forward
// from offset `0` (the walk traces out exactly the character
// boundary set of the encoding — .1), or equal
// `bytes.len()` (end-of-input clamp).
//
// Cases pinned at 64.

use encoding_rs::{Encoding, EUC_KR, GB18030, SHIFT_JIS};

const PROP12_MAX_BYTES_LEN: usize = 256;

/// CJK kinds covered by Property 12. Mirrors the list `MultiByteEngine`
/// dispatches on (`CjkKind::{ShiftJis, Gb18030, EucKr}` in
/// `src/document/encoding_engine.rs`).
#[derive(Debug, Clone, Copy)]
enum Prop12CjkKind {
    ShiftJis,
    Gb18030,
    EucKr,
}

impl Prop12CjkKind {
    fn encoding_rs(self) -> &'static Encoding {
        match self {
            Self::ShiftJis => SHIFT_JIS,
            Self::Gb18030 => GB18030,
            Self::EucKr => EUC_KR,
        }
    }

    fn document_encoding(self) -> DocumentEncoding {
        let label = match self {
            Self::ShiftJis => "Shift_JIS",
            Self::Gb18030 => "gb18030",
            Self::EucKr => "EUC-KR",
        };
        DocumentEncoding::from_label(label)
            .unwrap_or_else(|| panic!("encoding_rs should know {label}"))
    }

 /// Regex passed to `proptest::string::string_regex`. Each per-kind
 /// regex restricts the alphabet to scalars that the corresponding
 /// `encoding_rs` encoder can map without `had_unmappable`. Length
 /// is capped at 32 chars so encoded byte sequences stay small (≤
 /// 128 bytes for the gb18030 4-byte path) and shrinking remains
 /// quick. The non-ASCII ranges intentionally include line-break
 /// adjacent code points (e.g. CJK Unified Ideographs containing
 /// trail bytes that look like `0x0A` / `0x0D`) so the false-
 /// positive rejection branch in `MultiByteEngine::next_line_start`
 /// is exercised, plus injected ASCII LF / CR atoms via the regex
 /// alternation.
    fn text_regex(self) -> &'static str {
        match self {
 // ASCII (incl. `\n` and `\r`) + Hiragana — fully covered
 // by JIS X 0208 (Shift_JIS).
            Self::ShiftJis => "[\\n\\ra-z0-9\\u3041-\\u3093]{0,32}",
 // ASCII (incl. `\n` and `\r`) + CJK Unified Ideographs —
 // every code point has a valid gb18030 encoding (mix of
 // 2- and 4-byte sequences).
            Self::Gb18030 => "[\\n\\ra-z0-9\\u4E00-\\u9FFF]{0,32}",
 // ASCII (incl. `\n` and `\r`) + Hangul Syllables — KS X
 // 1001 covers the entire `U+AC00..=U+D7A3` block.
            Self::EucKr => "[\\n\\ra-z0-9\\uAC00-\\uD7A3]{0,32}",
        }
    }
}

/// One generated case for Property 12. Each variant carries the input
/// bytes that drive the chosen engine. Wiring through an enum lets one
/// `proptest!` block exercise every engine kind with a single case
/// budget.
#[derive(Debug, Clone)]
enum Prop12Engine {
    Utf8(Vec<u8>),
    SingleByte(DocumentEncoding, Vec<u8>),
    Utf16Le(Vec<u8>),
    Utf16Be(Vec<u8>),
    Multibyte(Prop12CjkKind, Vec<u8>),
}

/// Strategy that picks one engine variant per case. Each branch carries
/// its own byte generator: valid UTF-8 from a regex-driven `String` for
/// the UTF-8 branch, arbitrary `Vec<u8>` for the single-byte and
/// UTF-16 branches (every byte offset is a boundary in single-byte;
/// the UTF-16 alignment contract is the property under test), and
/// pre-encoded bytes from `encoding_rs::Encoding::encode` for the CJK
/// branches so the input always sits on character boundaries the
/// `MultiByteEngine` knows how to walk.
fn prop12_engine_strategy() -> impl Strategy<Value = Prop12Engine> {
    let utf8 = proptest::string::string_regex(r"(?s).{0,256}")
        .expect("UTF-8 regex strategy must compile")
        .prop_map(|s| Prop12Engine::Utf8(s.into_bytes()));

    let single_byte = (
        class_a_encoding_strategy(),
        prop::collection::vec(any::<u8>(), 0..=PROP12_MAX_BYTES_LEN),
    )
        .prop_map(|(encoding, bytes)| Prop12Engine::SingleByte(encoding, bytes));

    let utf16_le = prop::collection::vec(any::<u8>(), 0..=PROP12_MAX_BYTES_LEN)
        .prop_map(Prop12Engine::Utf16Le);
    let utf16_be = prop::collection::vec(any::<u8>(), 0..=PROP12_MAX_BYTES_LEN)
        .prop_map(Prop12Engine::Utf16Be);

    let multibyte = prop_oneof![
        proptest::string::string_regex(Prop12CjkKind::ShiftJis.text_regex())
            .expect("Shift_JIS regex must compile")
            .prop_map(|s| (Prop12CjkKind::ShiftJis, s)),
        proptest::string::string_regex(Prop12CjkKind::Gb18030.text_regex())
            .expect("gb18030 regex must compile")
            .prop_map(|s| (Prop12CjkKind::Gb18030, s)),
        proptest::string::string_regex(Prop12CjkKind::EucKr.text_regex())
            .expect("EUC-KR regex must compile")
            .prop_map(|s| (Prop12CjkKind::EucKr, s)),
    ]
    .prop_filter_map(
        "discard CJK case: input contains characters unmappable in target encoding",
        |(kind, text)| {
            let (encoded, _, had_unmappable) = kind.encoding_rs().encode(&text);
            if had_unmappable {
                None
            } else {
                Some(Prop12Engine::Multibyte(kind, encoded.into_owned()))
            }
        },
    );

    prop_oneof![
        2 => utf8,
        2 => single_byte,
        2 => utf16_le,
        2 => utf16_be,
        3 => multibyte,
    ]
}

/// Walks `bytes` from offset `0` via `engine.step(...)` and returns
/// the ordered list of character boundaries the engine yields
/// including the implicit `0` start and the natural endpoint reached
/// when `step` returns `0` (which equals `bytes.len()` for any
/// well-formed input). The returned vector is what
/// `next_line_start` is allowed to land on
/// every line boundary must coincide with a character boundary the
/// engine itself acknowledges.
fn prop12_step_boundaries(engine: &dyn EncodingEngine, bytes: &[u8]) -> Vec<usize> {
    let n = bytes.len();
    let mut boundaries = Vec::with_capacity(n / 2 + 1);
    boundaries.push(0);
    let mut p = 0usize;
    while p < n {
        let step = engine.step(bytes, p, n);
        if step == 0 {
            break;
        }
        p += step;
        boundaries.push(p);
    }
    boundaries
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

 /// Property 12: `engine.next_line_start` always returns an offset
 /// on a character boundary of the engine's target encoding. The
 /// per-engine assertion enforces the strongest contract that makes
 /// sense for the engine's encoding — UTF-8 char-boundary validity
 /// for `Utf8Engine`, range only for `SingleByteEngine` (every
 /// offset is a boundary in a single-byte ASCII superset)
 /// 2-byte alignment plus the `bytes.len()` clamp exception for
 /// `Utf16Engine`, and step-walk reachability for `MultiByteEngine`.
    #[test]
    fn property_12_next_line_start_lands_on_char_boundary(
        case in prop12_engine_strategy(),
        line_start_seed in any::<u32>(),
    ) {
        match case {
            Prop12Engine::Utf8(bytes) => {
                let n = bytes.len();
                let line_start = if n == 0 { 0 } else { (line_start_seed as usize) % (n + 1) };
                let encoding = DocumentEncoding::utf8();
                let engine: &dyn EncodingEngine = engine_for_encoding(encoding);
                prop_assert_eq!(
                    engine.encoding(), encoding,
                    "engine_for_encoding must return an engine bound to {}", encoding.name()
                );

                let result = engine.next_line_start(&bytes, n, line_start);
                prop_assert!(
                    result <= n,
                    "UTF-8: result {} must not exceed bytes.len()={}", result, n
                );
                prop_assert!(
                    std::str::from_utf8(&bytes[..result]).is_ok(),
                    "UTF-8: result {} must be on a char boundary; \
                     line_start={}, bytes={:02X?}",
                    result, line_start, &bytes
                );
            }

            Prop12Engine::SingleByte(encoding, bytes) => {
 // Every byte offset is a character boundary in a Class A
 // single-byte ASCII superset, so the boundary
 // contract collapses to the range invariant.
                prop_assert!(
                    SingleByteEngine::supports(encoding),
                    "{} must be claimed by SingleByteEngine for Property 12 \
                     to be meaningful", encoding.name()
                );
                let n = bytes.len();
                let line_start = if n == 0 { 0 } else { (line_start_seed as usize) % (n + 1) };
                let engine: &dyn EncodingEngine = engine_for_encoding(encoding);
                prop_assert_eq!(
                    engine.encoding(), encoding,
                    "engine_for_encoding must return an engine bound to {}", encoding.name()
                );

                let result = engine.next_line_start(&bytes, n, line_start);
                prop_assert!(
                    result <= n,
                    "{}: result {} must not exceed bytes.len()={}",
                    encoding.name(), result, n
                );
                prop_assert!(
                    result >= line_start.min(n),
                    "{}: result {} must not move backwards from line_start {} \
                     (clamped to {})",
                    encoding.name(), result, line_start, line_start.min(n)
                );
            }

            Prop12Engine::Utf16Le(bytes) => {
                prop12_assert_utf16(&bytes, line_start_seed, DocumentEncoding::utf16le())?;
            }

            Prop12Engine::Utf16Be(bytes) => {
                prop12_assert_utf16(&bytes, line_start_seed, DocumentEncoding::utf16be())?;
            }

            Prop12Engine::Multibyte(kind, bytes) => {
                let document_encoding = kind.document_encoding();
                let n = bytes.len();
                let line_start = if n == 0 { 0 } else { (line_start_seed as usize) % (n + 1) };
                let engine: &dyn EncodingEngine = engine_for_encoding(document_encoding);
                prop_assert_eq!(
                    engine.encoding(), document_encoding,
                    "engine_for_encoding must return an engine bound to {}",
                    document_encoding.name()
                );

                let result = engine.next_line_start(&bytes, n, line_start);
                prop_assert!(
                    result <= n,
                    "{:?}: result {} must not exceed bytes.len()={}",
                    kind, result, n
                );
                prop_assert!(
                    result >= line_start.min(n),
                    "{:?}: result {} must not move backwards from line_start {} \
                     (clamped to {})",
                    kind, result, line_start, line_start.min(n)
                );

 // Boundary contract via step-walk reachability: the result
 // must coincide with a boundary the engine itself produces
 // while walking the bytes from offset 0. The walk's last
 // entry is the natural end-of-input position (== bytes.len()
 // for well-formed input); end-of-buffer clamps from
 // `next_line_start` therefore stay inside the boundary
 // set without a special case.
                let boundaries = prop12_step_boundaries(engine, &bytes);
                prop_assert!(
                    boundaries.contains(&result),
                    "{:?}: result {} must lie on a char boundary reachable \
                     by step from 0; line_start={}, boundaries={:?}, \
                     bytes={:02X?}",
                    kind, result, line_start, boundaries, &bytes
                );
            }
        }
    }
}

/// Helper used by Property 12 to assert the UTF-16 boundary contract
/// for either endianness without duplicating the body in the match.
/// The contract: result is either even (a 2-byte code-unit boundary
/// ) or equal to `bytes.len()` (the only
/// legal odd return — covers a buffer that ends on a trailing odd
/// byte outside any complete code unit).
fn prop12_assert_utf16(
    bytes: &[u8],
    line_start_seed: u32,
    encoding: DocumentEncoding,
) -> Result<(), TestCaseError> {
    let n = bytes.len();
    let line_start = if n == 0 {
        0
    } else {
        (line_start_seed as usize) % (n + 1)
    };
    let engine: &dyn EncodingEngine = engine_for_encoding(encoding);
    prop_assert_eq!(
        engine.encoding(),
        encoding,
        "engine_for_encoding must return an engine bound to {}",
        encoding.name()
    );

    let result = engine.next_line_start(bytes, n, line_start);
    prop_assert!(
        result <= n,
        "{}: result {} must not exceed bytes.len()={}",
        encoding.name(),
        result,
        n
    );
    prop_assert!(
        result >= line_start.min(n),
        "{}: result {} must not move backwards from line_start {} (clamped to {})",
        encoding.name(),
        result,
        line_start,
        line_start.min(n)
    );
    prop_assert!(
        result % 2 == 0 || result == n,
        "{}: result {} must be 2-byte aligned or equal to bytes.len()={}; \
         line_start={}, bytes={:02X?}",
        encoding.name(),
        result,
        n,
        line_start,
        bytes
    );
    Ok(())
}
