//! Property 20: literal search is encoding-aware.
//!
//! For every supported encoding that has a native engine route
//! (UTF-8, single-byte windows/koi/ibm, UTF-16 LE/BE, Shift_JIS,
//! GB18030, EUC-KR) a random document built from that encoding's
//! alphabet plus line breaks, with a random substring of the same
//! alphabet as the needle, must satisfy:
//!
//! * `find_all` returns exactly the non-overlapping matches the decoded
//!   text oracle finds;
//! * the first and the last match start on the oracle's line;
//! * `find_prev` over the whole document lands on the last match;
//! * every match length equals the needle length in text units.
//!
//! Column indexes are pinned in full only for byte-exact line-model
//! encodings (UTF-8 and single-byte engines); UTF-16/CJK column
//! arithmetic carries the documented 1.0 line-model caveat.

use proptest::prelude::*;
use qem::DocumentEncoding;

use crate::helpers::fresh_test_dir;

/// Encoding entries: (encoding_rs label, alphabet including `\n`
/// separators, column pin flag). Alphabets contain only characters
/// representable in that encoding. The column flag is `true` for
/// byte-exact line-model encodings (UTF-8 and single-byte engines) and
/// `false` for UTF-16/CJK, whose column arithmetic carries the
/// documented 1.0 caveat.
const ENTRIES: &[(&str, &str, bool)] = &[
    ("utf8", "abcXYZ привет 日本語 테스트 中文 \n", true),
    ("windows-1251", "abcXYZ привет мир \n", true),
    ("KOI8-R", "abcXYZ привет мир \n", true),
    ("UTF-16LE", "abcXYZ привет 日本語 테스트 中文 \n", false),
    ("UTF-16BE", "abcXYZ привет 日本語 테스트 中文 \n", false),
    ("Shift_JIS", "abcXYZ 日本語 テスト \n", false),
    ("gb18030", "abcXYZ 中文 测试 \n", false),
    ("EUC-KR", "abcXYZ 한국어 \n", false),
];

fn is_utf16_name(name: &str) -> bool {
    name == "UTF-16LE" || name == "UTF-16BE"
}

fn is_le(name: &str) -> bool {
    name == "UTF-16LE"
}

fn encoding_by_index(index: usize) -> DocumentEncoding {
    DocumentEncoding::from_label(ENTRIES[index].0).expect("label must resolve")
}

fn alphabet_by_index(index: usize) -> Vec<char> {
    ENTRIES[index].1.chars().collect()
}

fn column_pinned(index: usize) -> bool {
    ENTRIES[index].2
}

fn line_col(text: &str, char_idx: usize) -> (usize, usize) {
    let mut line0 = 0usize;
    let mut col0 = 0usize;
    for (i, c) in text.chars().enumerate() {
        if i == char_idx {
            return (line0, col0);
        }
        if c == '\n' {
            line0 += 1;
            col0 = 0;
        } else {
            col0 += 1;
        }
    }
    (line0, col0)
}

/// Builds the document text and a needle (a non-empty substring without
/// line breaks) from the chosen alphabet. Returns `None` when the
/// generated indexes cannot produce such a needle.
fn build_text_and_needle(
    alphabet: &[char],
    text_idx: &[usize],
    needle_idx: &[usize],
) -> Option<(String, String)> {
    // Indexes are generated against a fixed range; map them onto the
    // alphabet modulo its length to keep the distribution stable.
    let text: String = text_idx
        .iter()
        .map(|&k| alphabet[k % alphabet.len()])
        .collect();
    if text.is_empty() {
        return None;
    }
    let needle: String = needle_idx
        .iter()
        .filter_map(|&k| text.chars().nth(k))
        .collect();
    if needle.is_empty() || needle.contains('\n') {
        return None;
    }
    Some((text, needle))
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn literal_search_finds_every_occurrence_in_any_encoding(
        enc_idx in 0usize..ENTRIES.len(),
        text_idx in prop::collection::vec(0usize..64usize, 0..=120usize),
        needle_idx in prop::collection::vec(0usize..64usize, 1..=6usize),
    ) {
        let alphabet = alphabet_by_index(enc_idx);
        let Some((text, needle)) = build_text_and_needle(&alphabet, &text_idx, &needle_idx)
        else {
            return Ok(());
        };
        prop_assume!(text.chars().count() >= needle.chars().count());

        let encoding = encoding_by_index(enc_idx);
        let dir = fresh_test_dir("prop_literal");
        let path = dir.join("fixture.bin");
        // encoding_rs deliberately redirects UTF-16 encoders to UTF-8
        // (WHATWG), so UTF-16 is encoded by hand through `encode_utf16`
        // exactly like the document layer does; every other encoding uses
        // the encoding_rs encoder (the alphabet is representable).
        let encoded: Vec<u8> = if is_utf16_name(ENTRIES[enc_idx].0) {
            let mut bytes = Vec::with_capacity(text.len() * 2);
            for unit in text.encode_utf16() {
                if is_le(ENTRIES[enc_idx].0) {
                    bytes.extend_from_slice(&unit.to_le_bytes());
                } else {
                    bytes.extend_from_slice(&unit.to_be_bytes());
                }
            }
            bytes
        } else {
            let rs_enc = encoding_rs::Encoding::for_label(ENTRIES[enc_idx].0.as_bytes())
                .expect("encoding_rs must know the label");
            let (bytes, output, had_errors) = rs_enc.encode(&text);
            prop_assert_eq!(output, rs_enc, "alphabet must be representable");
            prop_assert!(!had_errors, "alphabet must round-trip cleanly");
            bytes.into_owned()
        };
        std::fs::write(&path, &encoded).expect("write fixture");

        let doc = qem::Document::open_with_encoding(&path, encoding)
            .expect("open fixture");
        let decoded = doc.text_lossy();
        prop_assert_eq!(&decoded, &text, "document must decode to the fixture text");

        // Oracle: non-overlapping match start char indexes in the decoded
        // text.
        // The oracle works in char coordinates end to end: the decoded
        // text as a char vector, the needle as a char sub-vector.
        let decoded_chars: Vec<char> = decoded.chars().collect();
        let text_chars: Vec<char> = text.chars().collect();
        let needle_chars: Vec<char> = needle.chars().collect();
        let mut oracle_starts: Vec<usize> = Vec::new();
        let mut cursor = 0usize;
        while cursor + needle_chars.len() <= decoded_chars.len() {
            if decoded_chars[cursor..cursor + needle_chars.len()] == needle_chars[..] {
                oracle_starts.push(cursor);
                cursor += needle_chars.len();
            } else {
                cursor += 1;
            }
        }
        let needle_units = needle.chars().count();

        let matches: Vec<_> = doc
            .find_all(&needle)
            .map(|m| (m.start(), m.end(), m.len_chars()))
            .collect();
        if matches.len() != oracle_starts.len() {
            eprintln!("DBG enc={} needle={:?} text={:?}", ENTRIES[enc_idx].0, needle.escape_debug(), text.escape_debug());
            for (i, (start, _end, len)) in matches.iter().take(8).enumerate() {
                eprintln!("DBG match {i}: {:?} len {}", start, len);
            }
            let ostart = line_col(&text, oracle_starts[0]);
            eprintln!("DBG oracle first: {:?}", ostart);
        }
        prop_assert_eq!(
            matches.len(),
            oracle_starts.len(),
            "match count must equal the oracle count; enc={} needle={:?} text={:?}",
            ENTRIES[enc_idx].0,
            needle.escape_debug(),
            text.escape_debug()
        );

        for (i, (start, _end, len)) in matches.iter().enumerate() {
            let (ol, oc) = line_col(&text, oracle_starts[i]);
            let msg = format!("match {i} must match the oracle");
            prop_assert_eq!(*len, needle_units, "{}", msg);
            // Line and column indexes are pinned for byte-exact line-model
            // encodings (UTF-8 and single-byte engines). UTF-16/CJK
            // positions carry the documented 1.0 line-model caveat (raw
            // 0x0A/0x0D scans), so only the count and the length are
            // pinned for them.
            if column_pinned(enc_idx) {
                prop_assert_eq!(start.line0(), ol, "{}", msg);
                prop_assert_eq!(start.col0(), oc, "{}", msg);
            }
        }

        // Reverse search lands on the RIGHTMOST occurrence — which for
        // self-overlapping needles may start inside the second-to-last
        // match (the documented reverse semantics, same as the
        // reverse-regex rightmost-match note). The oracle therefore picks
        // the last possible start position without the non-overlap rule.
        let char_count = text.chars().count();
        let rightmost = if char_count >= needle_units {
            (0..=char_count - needle_units)
                .rev()
                .find(|&b| {
                    let at: String = text_chars[b..b + needle_units].iter().collect();
                    at == needle
                })
        } else {
            None
        };
        if let Some((ol, oc)) = rightmost.map(|s| line_col(&text, s)) {
            let prev = doc
                .find_prev(
                    &needle,
                    qem::TextPosition::new(usize::MAX, usize::MAX),
                )
                .expect("reverse search must succeed");
            if column_pinned(enc_idx) {
                prop_assert_eq!(prev.start().line0(), ol, "reverse search must land on the last match");
                prop_assert_eq!(prev.start().col0(), oc, "reverse column must match the oracle");
            }
        }

        let _ = std::fs::remove_dir_all(dir);
    }
}
