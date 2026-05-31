// Property 4: SingleByteEngine count_columns and advance_offset follow step=1.
//
//
// Property 4 has two halves and the test mirrors that structure:
//
// (a) For every Class A encoding `e` and every byte slice `line` that
// contains no `\n` (`0x0A`) or `\r` (`0x0D`)
// `engine_for_encoding(e).count_columns_exact(line) == line.len()`.
// That is, when the engine steps one byte per text unit, the
// column counter is just the byte length.
//
// (b) For every Class A encoding `e` and a sequence of text units built
// from non-line-ending filler bytes plus injected `CRLF` (`\r\n`)
// separators, `engine.advance_offset_by_text_units(bytes, file_len
// 0, n)` walks exactly `n` text units forward, where each CRLF
// counts as one text unit. The expected byte offset for any `n` is
// computed by a parallel reference table built alongside the
// generator.
//
// The engine is reached through `qem::document::__test_support`, the
// `#[doc(hidden)]` re-export module introduced for the
// integration property tests under `tests/encoding_engine/`. The cases
// count is intentionally pinned at 64 for this spec.

use proptest::prelude::*;
use qem::document::__test_support::{engine_for_encoding, EncodingEngine, SingleByteEngine};
use qem::DocumentEncoding;

/// Class A encodings the spec wires through `SingleByteEngine`. The
/// labels match `encoding_rs` exactly so `DocumentEncoding::from_label`
/// always succeeds. The set deliberately mirrors `prop_newline.rs` so
/// Properties 3 and 4 cover the same encoding surface.
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

/// Generates a byte slice that contains neither `\n` (`0x0A`) nor `\r`
/// (`0x0D`). Bounded to 512 bytes: large enough to exercise non-trivial
/// line lengths, small enough to keep shrinking quick. Used by Branch A.
fn line_without_line_endings_strategy() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(
        any::<u8>().prop_filter("line content must not contain LF or CR", |b| {
            *b != b'\n' && *b != b'\r'
        }),
        0..=512,
    )
}

/// One token in the Branch B generator: either a non-line-ending byte
/// or a CRLF separator. Each token represents exactly one text unit.
#[derive(Debug, Clone)]
enum Token {
 /// A plain byte that is neither `\n` nor `\r`. Encodes to 1 byte
 /// and counts as 1 text unit.
    Byte(u8),
 /// A `\r\n` pair. Encodes to 2 bytes (`0x0D 0x0A`) and counts as
 /// 1 text unit per the line-ending semantics shared by every
 /// `EncodingEngine` (see CRLF handling in
 /// `SingleByteEngine::advance_offset_by_text_units`).
    Crlf,
}

/// Generates a single Branch B token. CRLFs are injected with a 1:5
/// bias so most cases mix multiple lines without saturating the
/// generator with line endings. Filler bytes prefer Class A high-byte
/// glyphs and printable ASCII so the byte content visually resembles
/// real Class A text.
fn token_strategy() -> impl Strategy<Value = Token> {
    prop_oneof![
        5 => prop_oneof![
 // High bytes: Cyrillic / Latin-1 glyph range in Class A.
            3 => 0xC0u8..=0xFE,
 // Printable ASCII (excludes 0x0A and 0x0D by construction).
            3 => b' '..=b'~',
        ].prop_map(Token::Byte),
        1 => Just(Token::Crlf),
    ]
}

/// Generates `(bytes, byte_prefix_lengths)`: the encoded byte sequence
/// plus a parallel table where `byte_prefix_lengths[k]` is the byte
/// offset that corresponds to advancing exactly `k` text units from
/// offset `0`. The table has length `tokens.len() + 1`: index `0` is
/// the empty prefix (offset `0`), and index `tokens.len()` is the full
/// byte length. Branch B uses this table to compute the expected
/// `advance_offset_by_text_units` result for every `n`.
fn crlf_corpus_strategy() -> impl Strategy<Value = (Vec<u8>, Vec<usize>)> {
    prop::collection::vec(token_strategy(), 0..=256).prop_map(|tokens| {
        let mut bytes: Vec<u8> = Vec::with_capacity(tokens.len() * 2);
        let mut prefix_lengths: Vec<usize> = Vec::with_capacity(tokens.len() + 1);
        prefix_lengths.push(0);
        for token in &tokens {
            match token {
                Token::Byte(b) => bytes.push(*b),
                Token::Crlf => {
                    bytes.push(b'\r');
                    bytes.push(b'\n');
                }
            }
            prefix_lengths.push(bytes.len());
        }
        (bytes, prefix_lengths)
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

 /// Property 4 Branch A: `count_columns_exact` equals the byte length
 /// when no line endings are present in the slice. This is the direct
 /// consequence of the column counter walking until the first
 /// line-ending byte: with none in the input, the scan reaches
 /// `bytes.len()` and that value becomes the column count.
 ///
 /// The `SingleByteEngine::supports` runtime check guards us against
 /// silently testing the UTF-8 fallback if a label were ever moved
 /// out of Class A by mistake. The same engine is used in Branch B
 /// below so both branches share the same dispatch promise.
    #[test]
    fn property_4a_count_columns_exact_equals_byte_len_without_line_endings(
        encoding in class_a_encoding_strategy(),
        line in line_without_line_endings_strategy(),
    ) {
        prop_assert!(
            SingleByteEngine::supports(encoding),
            "{} must be claimed by SingleByteEngine for Property 4 to be meaningful",
            encoding.name()
        );

        let engine: &dyn EncodingEngine = engine_for_encoding(encoding);
        prop_assert_eq!(
            engine.encoding(),
            encoding,
            "engine_for_encoding must return an engine bound to {}",
            encoding.name()
        );

        let columns = engine.count_columns_exact(&line);
        prop_assert_eq!(
            columns,
            line.len(),
            "count_columns_exact must equal byte length when the slice contains \
             no line endings (encoding {}, len {})",
            encoding.name(),
            line.len(),
        );
    }

 /// Property 4 Branch B: `advance_offset_by_text_units(bytes, file_len
 /// 0, n)` lands on the byte offset that corresponds to consuming
 /// exactly the first `n` tokens of the corpus, where each plain byte
 /// token is 1 byte / 1 text unit and each CRLF token is 2 bytes / 1
 /// text unit. The single sweep over `n in 0..=token_count + extra`
 /// also exercises the saturating clamp: when the caller asks for
 /// more text units than the file contains, the result clamps at
 /// `file_len`.
    #[test]
    fn property_4b_advance_offset_walks_n_units_with_crlf_as_one_unit(
        encoding in class_a_encoding_strategy(),
        corpus in crlf_corpus_strategy(),
        extra_units in 0usize..=8,
    ) {
        prop_assert!(
            SingleByteEngine::supports(encoding),
            "{} must be claimed by SingleByteEngine for Property 4 to be meaningful",
            encoding.name()
        );

        let (bytes, prefix_lengths) = corpus;
        let token_count = prefix_lengths.len().saturating_sub(1);

        let engine: &dyn EncodingEngine = engine_for_encoding(encoding);
        let file_len = bytes.len();

        let max_n = token_count + extra_units;
        for n in 0..=max_n {
            let actual = engine.advance_offset_by_text_units(&bytes, file_len, 0, n);
            let expected = prefix_lengths.get(n).copied().unwrap_or(file_len);
            prop_assert_eq!(
                actual,
                expected,
                "advance_offset_by_text_units(bytes, file_len, 0, {}) must equal \
                 {} for encoding {} (corpus byte length {}, token count {})",
                n,
                expected,
                encoding.name(),
                file_len,
                token_count,
            );
        }
    }
}
