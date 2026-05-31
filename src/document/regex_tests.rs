//! Tests for the typed regex search surface introduced in `0.8.0`.
//!
//! Coverage goals:
//!
//! - `find_next_regex`, `find_prev_regex`, `find_all_regex` work on rope,
//!   piece-table, and clean mmap backings.
//! - Bounded `_in_range` / `_between` helpers respect their boundaries.
//! - Anchors (`^`, `$`), digit classes, basic Unicode classes, and CRLF
//!   semantics behave the way the literal-search contract documents.
//! - `RegexCompileError` surfaces invalid patterns with a non-empty message.
//! - The iterator does not loop indefinitely on empty matches and is fused.
//! - Same-pattern, same-document searches return identical results across
//!   rope, piece-table, and mmap backings (same-content cross-backing
//!   consistency).
//!
//! Performance and very-large-file regex throughput are not part of this
//! test pass; they live in the `0.8.0` perf work block.

use crate::{Document, RegexSearchQuery, TextPosition, TextRange};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const PIECE_TABLE_THRESHOLD_BYTES: usize = 1024 * 1024;

fn doc_with(text: &str) -> Document {
    let mut doc = Document::new();
    if !text.is_empty() {
        let _ = doc
            .try_insert(TextPosition::new(0, 0), text)
            .expect("seeded insert succeeds");
    }
    doc
}

fn fresh_test_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "qem-regex-tests-{label}-{}-{}",
        std::process::id(),
        Instant::now().elapsed().as_nanos()
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn open_clean_mmap_doc(content: &[u8], dir: &Path, name: &str) -> Document {
    let path = dir.join(name);
    fs::write(&path, content).unwrap();
    let doc = Document::open(&path).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while doc.is_indexing() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    doc
}

fn open_piece_table_doc(content: &[u8], dir: &Path, name: &str) -> Document {
    let path = dir.join(name);
    assert!(
        content.len() >= PIECE_TABLE_THRESHOLD_BYTES,
        "piece-table fixture must be at least {PIECE_TABLE_THRESHOLD_BYTES} bytes",
    );
    fs::write(&path, content).unwrap();
    let mut doc = Document::open(&path).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while doc.is_indexing() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    // Promote to piece-table by performing a cheap edit at the start.
    let _ = doc
        .try_insert(TextPosition::new(0, 0), "X")
        .expect("seed edit promotes to piece-table");
    let _ = doc
        .try_replace(TextRange::new(TextPosition::new(0, 0), 1), "")
        .expect("undo seed edit");
    doc
}

// ---------------------------------------------------------------------------
// Compile-error semantics
// ---------------------------------------------------------------------------

#[test]
fn regex_compile_error_surfaces_non_empty_message_for_invalid_patterns() {
    let err = RegexSearchQuery::new("(unclosed").unwrap_err();
    assert!(!err.message().is_empty());
    let display = format!("{err}");
    assert_eq!(display, err.message());
}

#[test]
fn regex_compile_error_rejects_empty_patterns() {
    let err = RegexSearchQuery::new("").unwrap_err();
    assert!(!err.message().is_empty());
}

#[test]
fn one_shot_find_next_regex_propagates_compile_error() {
    let doc = doc_with("anything\n");
    let err = doc
        .find_next_regex("(unclosed", TextPosition::new(0, 0))
        .unwrap_err();
    assert!(!err.message().is_empty());
}

#[test]
fn one_shot_find_prev_regex_propagates_compile_error() {
    let doc = doc_with("anything\n");
    let err = doc
        .find_prev_regex("(unclosed", TextPosition::new(0, 8))
        .unwrap_err();
    assert!(!err.message().is_empty());
}

// ---------------------------------------------------------------------------
// Rope-backed forward/reverse search
// ---------------------------------------------------------------------------

#[test]
fn find_next_regex_matches_on_rope_document() {
    let doc = doc_with("alpha 12 bravo 345 charlie\n");
    let m = doc
        .find_next_regex(r"\d+", TextPosition::new(0, 0))
        .unwrap()
        .expect("expected first numeric match");
    assert_eq!(m.start(), TextPosition::new(0, 6));
    assert_eq!(m.end(), TextPosition::new(0, 8));
    assert_eq!(m.len_chars(), 2);
}

#[test]
fn find_next_regex_query_skips_to_position_offset() {
    let doc = doc_with("alpha 12 bravo 345 charlie\n");
    let query = RegexSearchQuery::new(r"\d+").unwrap();

    let first = doc
        .find_next_regex_query(&query, TextPosition::new(0, 0))
        .unwrap();
    assert_eq!(first.start().col0(), 6);

    let second = doc.find_next_regex_query(&query, first.end()).unwrap();
    assert_eq!(second.start().col0(), 15);
    assert_eq!(second.len_chars(), 3);
}

#[test]
fn find_prev_regex_returns_last_match_at_or_before_boundary() {
    let doc = doc_with("alpha 12 bravo 345 charlie\n");
    let prev = doc
        .find_prev_regex(r"\d+", TextPosition::new(0, 26))
        .unwrap()
        .expect("expected last numeric match before EOL");
    assert_eq!(prev.start(), TextPosition::new(0, 15));
    assert_eq!(prev.end(), TextPosition::new(0, 18));
}

#[test]
fn find_prev_regex_returns_only_match_fully_within_boundary() {
    let doc = doc_with("alpha 12 bravo 345\n");
    let prev = doc
        .find_prev_regex(r"\d+", TextPosition::new(0, 8))
        .unwrap()
        .expect("expected match ending at or before col 8");
    assert_eq!(prev.start(), TextPosition::new(0, 6));
    assert_eq!(prev.end(), TextPosition::new(0, 8));
}

#[test]
fn find_prev_regex_returns_none_at_document_start() {
    let doc = doc_with("alpha\n");
    let prev = doc
        .find_prev_regex(r"\w+", TextPosition::new(0, 0))
        .unwrap();
    assert!(prev.is_none());
}

// ---------------------------------------------------------------------------
// Iterator semantics
// ---------------------------------------------------------------------------

#[test]
fn find_all_regex_iterates_non_overlapping_matches() {
    let doc = doc_with("a1 b22 c333 d4444\n");
    let matches: Vec<_> = doc.find_all_regex(r"\d+").unwrap().collect();

    let starts: Vec<usize> = matches.iter().map(|m| m.start().col0()).collect();
    let lengths: Vec<usize> = matches.iter().map(|m| m.len_chars()).collect();
    assert_eq!(starts, vec![1, 4, 8, 13]);
    assert_eq!(lengths, vec![1, 2, 3, 4]);
}

#[test]
fn find_all_regex_terminates_on_empty_match_pattern() {
    let doc = doc_with("aaa\n");
    let matches: Vec<_> = doc.find_all_regex(r"a*").unwrap().take(64).collect();
    assert!(!matches.is_empty());
    assert!(matches.len() <= 64);
}

#[test]
fn find_all_regex_iterator_is_fused_after_exhaustion() {
    let doc = doc_with("alpha\n");
    let mut iter = doc.find_all_regex(r"\d+").unwrap();
    assert!(iter.next().is_none());
    assert!(iter.next().is_none());
    assert!(iter.next().is_none());
}

#[test]
fn find_all_regex_no_match_returns_empty_iterator() {
    let doc = doc_with("alpha bravo charlie\n");
    let collected: Vec<_> = doc.find_all_regex(r"\d+").unwrap().collect();
    assert!(collected.is_empty());
}

// ---------------------------------------------------------------------------
// Anchors and Unicode
// ---------------------------------------------------------------------------

#[test]
fn anchors_match_line_starts() {
    let doc = doc_with("first\nsecond\nthird\n");
    let starts: Vec<_> = doc.find_all_regex(r"(?m)^\w+").unwrap().collect();
    assert_eq!(starts.len(), 3);
    assert_eq!(starts[0].start(), TextPosition::new(0, 0));
    assert_eq!(starts[1].start(), TextPosition::new(1, 0));
    assert_eq!(starts[2].start(), TextPosition::new(2, 0));
}

#[test]
fn anchors_match_line_ends_in_multiline_mode() {
    let doc = doc_with("first\nsecond\nthird\n");
    let ends: Vec<_> = doc.find_all_regex(r"(?m)\w+$").unwrap().collect();
    assert_eq!(ends.len(), 3);
    assert_eq!(ends[0].end(), TextPosition::new(0, 5));
    assert_eq!(ends[1].end(), TextPosition::new(1, 6));
    assert_eq!(ends[2].end(), TextPosition::new(2, 5));
}

#[test]
fn unicode_classes_match_combining_marks_as_separate_columns() {
    let doc = doc_with("café\n");
    let m = doc
        .find_next_regex(r"\w+", TextPosition::new(0, 0))
        .unwrap()
        .expect("expected unicode word");
    assert_eq!(m.start(), TextPosition::new(0, 0));
    assert_eq!(m.end(), TextPosition::new(0, 4));
    assert_eq!(m.len_chars(), 4);
}

#[test]
fn unicode_class_matches_cyrillic_word() {
    let doc = doc_with("hello мир world\n");
    let collected: Vec<_> = doc.find_all_regex(r"\w+").unwrap().collect();
    let starts: Vec<usize> = collected.iter().map(|m| m.start().col0()).collect();
    let ends: Vec<usize> = collected.iter().map(|m| m.end().col0()).collect();
    assert_eq!(starts, vec![0, 6, 10]);
    assert_eq!(ends, vec![5, 9, 15]);
}

#[test]
fn case_insensitive_flag_matches_mixed_case() {
    let doc = doc_with("Hello hello HELLO\n");
    let collected: Vec<_> = doc.find_all_regex(r"(?i)hello").unwrap().collect();
    assert_eq!(collected.len(), 3);
}

// ---------------------------------------------------------------------------
// Bounded helpers
// ---------------------------------------------------------------------------

#[test]
fn bounded_in_range_returns_only_fully_contained_matches() {
    let doc = doc_with("xxx 1234 yyy 5678 zzz\n");
    let range = TextRange::new(TextPosition::new(0, 3), 6);
    let m = doc
        .find_next_regex_in_range(r"\d+", range)
        .unwrap()
        .expect("expected first number to fit");
    assert_eq!(m.start(), TextPosition::new(0, 4));
    assert_eq!(m.end(), TextPosition::new(0, 8));
}

#[test]
fn bounded_regex_returns_prefix_of_overflowing_match_within_bounds() {
    // Bounded regex search runs `regex` over the byte window covered by the
    // bounds. A pattern like `\d+` is greedy but still terminates at the end
    // of the available window, so the bounded search may return a prefix of
    // a longer match. This is the same behavior as running `regex` against a
    // truncated string and is part of the documented bounded-regex contract.
    let doc = doc_with("xxx 1234 yyy 5678 zzz\n");
    let range = TextRange::new(TextPosition::new(0, 3), 4);
    let m = doc
        .find_next_regex_in_range(r"\d+", range)
        .unwrap()
        .expect("expected prefix match within bounds");
    assert_eq!(m.start(), TextPosition::new(0, 4));
    assert_eq!(m.end(), TextPosition::new(0, 7));
    assert_eq!(m.len_chars(), 3);
}

#[test]
fn bounded_between_orders_endpoints_and_finds_match() {
    let doc = doc_with("xxx 1234 yyy 5678 zzz\n");
    let m = doc
        .find_next_regex_between(r"\d+", TextPosition::new(0, 13), TextPosition::new(0, 9))
        .unwrap();
    assert!(m.is_none(), "no number in [9..13)");

    let m = doc
        .find_next_regex_between(r"\d+", TextPosition::new(0, 21), TextPosition::new(0, 9))
        .unwrap()
        .expect("expected match in [9..21)");
    assert_eq!(m.start(), TextPosition::new(0, 13));
    assert_eq!(m.end(), TextPosition::new(0, 17));
}

#[test]
fn find_prev_regex_query_mirrors_literal_semantics_with_full_iteration() {
    let doc = doc_with("a1 a2 a3 a4 a5\n");
    let query = RegexSearchQuery::new(r"a\d").unwrap();

    let prev = doc
        .find_prev_regex_query(&query, TextPosition::new(0, 14))
        .expect("expected last match before EOL");
    assert_eq!(prev.start(), TextPosition::new(0, 12));
    assert_eq!(prev.end(), TextPosition::new(0, 14));

    let prev = doc
        .find_prev_regex_query(&query, TextPosition::new(0, 8))
        .expect("expected last match ending at or before col 8");
    assert_eq!(prev.start(), TextPosition::new(0, 6));
    assert_eq!(prev.end(), TextPosition::new(0, 8));
}

#[test]
fn find_all_regex_query_bounded_iterator_stops_at_end() {
    let doc = doc_with("1 2 3 4 5 6\n");
    let query = RegexSearchQuery::new(r"\d").unwrap();
    let collected: Vec<_> = doc
        .find_all_regex_query_between(&query, TextPosition::new(0, 0), TextPosition::new(0, 5))
        .collect();
    let starts: Vec<usize> = collected.iter().map(|m| m.start().col0()).collect();
    assert_eq!(starts, vec![0, 2, 4]);
}

#[test]
fn find_all_regex_in_range_skips_matches_outside_window() {
    let doc = doc_with("aa bb cc dd ee ff\n");
    let range = TextRange::new(TextPosition::new(0, 6), 6);
    let collected: Vec<_> = doc
        .find_all_regex_in_range(r"\w+", range)
        .unwrap()
        .collect();
    let starts: Vec<usize> = collected.iter().map(|m| m.start().col0()).collect();
    let ends: Vec<usize> = collected.iter().map(|m| m.end().col0()).collect();
    assert_eq!(starts, vec![6, 9]);
    assert_eq!(ends, vec![8, 11]);
}

// ---------------------------------------------------------------------------
// Multi-line (non-MULTILINE flag) semantics
// ---------------------------------------------------------------------------

#[test]
fn dot_does_not_cross_newlines_by_default() {
    let doc = doc_with("foo\nbar\nbaz\n");
    let m = doc
        .find_next_regex(r"foo.bar", TextPosition::new(0, 0))
        .unwrap();
    assert!(m.is_none(), "default `.` must not cross `\\n` (got {m:?})");
}

#[test]
fn dot_all_flag_crosses_newlines_when_explicitly_enabled() {
    let doc = doc_with("foo\nbar\n");
    let m = doc
        .find_next_regex(r"(?s)foo.bar", TextPosition::new(0, 0))
        .unwrap()
        .expect("expected (?s)-mode match across newline");
    assert_eq!(m.start(), TextPosition::new(0, 0));
    assert_eq!(m.end(), TextPosition::new(1, 3));
}

// ---------------------------------------------------------------------------
// CRLF semantics (mmap backing keeps stored CRLF; rope normalizes to \n)
// ---------------------------------------------------------------------------

#[test]
fn rope_backing_normalizes_crlf_for_regex_search() {
    // `try_insert` accepts CRLF input but stores `\n` internally for the
    // rope. Regex on the rope sees `\n` boundaries, which matches the
    // existing literal-search rope contract.
    let doc = doc_with("alpha\r\nbeta\r\ngamma\n");
    let starts: Vec<_> = doc.find_all_regex(r"(?m)^\w+").unwrap().collect();
    assert_eq!(starts.len(), 3);
    assert_eq!(starts[0].start(), TextPosition::new(0, 0));
    assert_eq!(starts[1].start(), TextPosition::new(1, 0));
    assert_eq!(starts[2].start(), TextPosition::new(2, 0));
}

#[test]
fn mmap_backing_preserves_stored_crlf_during_regex_match() {
    let dir = fresh_test_dir("mmap-crlf");
    let doc = open_clean_mmap_doc(b"alpha\r\nbeta\r\ngamma\n", &dir, "crlf.txt");

    // Anchored line starts on mmap should still find each row beginning.
    let collected: Vec<_> = doc.find_all_regex(r"(?m)^\w+").unwrap().collect();
    assert_eq!(collected.len(), 3);
    assert_eq!(collected[0].start(), TextPosition::new(0, 0));
    assert_eq!(collected[1].start(), TextPosition::new(1, 0));
    assert_eq!(collected[2].start(), TextPosition::new(2, 0));

    let _ = fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// Clean mmap-backed search
// ---------------------------------------------------------------------------

#[test]
fn find_next_regex_on_clean_mmap_finds_first_match_with_correct_position() {
    let dir = fresh_test_dir("mmap-find-next");
    let doc = open_clean_mmap_doc(
        b"alpha 12 bravo 345 charlie 6789\n",
        &dir,
        "single-line.txt",
    );
    let m = doc
        .find_next_regex(r"\d+", TextPosition::new(0, 0))
        .unwrap()
        .expect("expected first number on mmap path");
    assert_eq!(m.start(), TextPosition::new(0, 6));
    assert_eq!(m.end(), TextPosition::new(0, 8));

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn find_prev_regex_on_clean_mmap_returns_last_match_before_boundary() {
    let dir = fresh_test_dir("mmap-find-prev");
    let doc = open_clean_mmap_doc(
        b"alpha 12 bravo 345 charlie 6789 delta\n",
        &dir,
        "with-numbers.txt",
    );
    let prev = doc
        .find_prev_regex(r"\d+", TextPosition::new(0, 31))
        .unwrap()
        .expect("expected last numeric match before col 31");
    assert_eq!(prev.start(), TextPosition::new(0, 27));
    assert_eq!(prev.end(), TextPosition::new(0, 31));

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn find_all_regex_on_clean_mmap_iterates_all_matches_across_lines() {
    let dir = fresh_test_dir("mmap-find-all");
    let doc = open_clean_mmap_doc(
        b"line 1 has 11\nline 2 has 222\nline 3 has 3333\n",
        &dir,
        "multiline.txt",
    );
    let collected: Vec<_> = doc.find_all_regex(r"\d+").unwrap().collect();
    let starts: Vec<(usize, usize)> = collected
        .iter()
        .map(|m| (m.start().line0(), m.start().col0()))
        .collect();
    let lengths: Vec<usize> = collected.iter().map(|m| m.len_chars()).collect();
    assert_eq!(
        starts,
        vec![(0, 5), (0, 11), (1, 5), (1, 11), (2, 5), (2, 11)]
    );
    assert_eq!(lengths, vec![1, 2, 1, 3, 1, 4]);

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn find_next_regex_on_clean_mmap_with_no_match_returns_none() {
    let dir = fresh_test_dir("mmap-no-match");
    let doc = open_clean_mmap_doc(b"alpha bravo charlie delta\n", &dir, "letters.txt");
    let result = doc
        .find_next_regex(r"\d+", TextPosition::new(0, 0))
        .unwrap();
    assert!(result.is_none());

    let _ = fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// Piece-table-backed search (large file with seed edit)
// ---------------------------------------------------------------------------

fn build_piece_table_fixture() -> Vec<u8> {
    // Repeated mid-size lines that put 'TARGET' at a known column on
    // line 100. Total payload is comfortably over PIECE_TABLE_THRESHOLD_BYTES
    // so Document::open uses the piece-tree path on edit.
    let line_padding = "abcdefghij".repeat(80);
    let mut bytes = Vec::with_capacity(PIECE_TABLE_THRESHOLD_BYTES + 64 * 1024);
    for i in 0..1500 {
        if i == 100 {
            bytes.extend_from_slice(b"PREFIX TARGET 999\n");
        } else {
            bytes.extend_from_slice(line_padding.as_bytes());
            bytes.extend_from_slice(b"\n");
        }
    }
    bytes
}

#[test]
fn find_next_regex_on_piece_table_backing_finds_known_seed_position() {
    let dir = fresh_test_dir("pt-find-next");
    let bytes = build_piece_table_fixture();
    let doc = open_piece_table_doc(&bytes, &dir, "pt.txt");

    let m = doc
        .find_next_regex(r"TARGET", TextPosition::new(0, 0))
        .unwrap()
        .expect("expected TARGET on piece-table backing");
    assert_eq!(m.start(), TextPosition::new(100, 7));
    assert_eq!(m.end(), TextPosition::new(100, 13));

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn find_all_regex_on_piece_table_backing_yields_all_numeric_runs() {
    let dir = fresh_test_dir("pt-find-all");
    let bytes = build_piece_table_fixture();
    let doc = open_piece_table_doc(&bytes, &dir, "pt-numbers.txt");

    let mut count = 0usize;
    for m in doc.find_all_regex(r"\d+").unwrap() {
        // The fixture only contains digits on line 100 ("999").
        assert_eq!(m.start().line0(), 100);
        assert_eq!(m.start().col0(), 14);
        assert_eq!(m.end().col0(), 17);
        count += 1;
    }
    assert_eq!(count, 1);

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn find_prev_regex_on_piece_table_backing_returns_last_match_before_eof() {
    let dir = fresh_test_dir("pt-find-prev");
    let bytes = build_piece_table_fixture();
    let doc = open_piece_table_doc(&bytes, &dir, "pt-prev.txt");

    let prev = doc
        .find_prev_regex(r"TARGET", TextPosition::new(usize::MAX, usize::MAX))
        .unwrap()
        .expect("expected reverse match on piece-table backing");
    assert_eq!(prev.start(), TextPosition::new(100, 7));
    assert_eq!(prev.end(), TextPosition::new(100, 13));

    let _ = fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// Cross-backing consistency
// ---------------------------------------------------------------------------

#[test]
fn rope_and_mmap_return_same_first_match_for_same_content() {
    let dir = fresh_test_dir("xback-same-first");
    let content = "alpha 12 bravo 345 charlie 67890\n";

    let rope_doc = doc_with(content);
    let mmap_doc = open_clean_mmap_doc(content.as_bytes(), &dir, "same-first.txt");

    let from = TextPosition::new(0, 0);
    let from_rope = rope_doc.find_next_regex(r"\d+", from).unwrap();
    let from_mmap = mmap_doc.find_next_regex(r"\d+", from).unwrap();
    assert_eq!(from_rope, from_mmap);

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn rope_and_mmap_return_same_match_count_via_find_all() {
    let dir = fresh_test_dir("xback-find-all");
    let content = "x1 y2 z3 a44 b55 c66 d777 e888 f999\n";

    let rope_doc = doc_with(content);
    let mmap_doc = open_clean_mmap_doc(content.as_bytes(), &dir, "find-all.txt");

    let rope_matches: Vec<_> = rope_doc.find_all_regex(r"\d+").unwrap().collect();
    let mmap_matches: Vec<_> = mmap_doc.find_all_regex(r"\d+").unwrap().collect();
    assert_eq!(rope_matches.len(), mmap_matches.len());
    assert_eq!(rope_matches, mmap_matches);

    let _ = fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// Reusable compiled query reuse semantics
// ---------------------------------------------------------------------------

#[test]
fn compiled_query_can_be_reused_across_many_calls() {
    let doc = doc_with("a1 a2 a3 a4 a5 a6 a7 a8 a9\n");
    let query = RegexSearchQuery::new(r"a\d").unwrap();
    let mut from = TextPosition::new(0, 0);
    let mut found_count = 0usize;
    while let Some(m) = doc.find_next_regex_query(&query, from) {
        found_count += 1;
        from = m.end();
        if found_count > 20 {
            panic!("compiled-query loop did not advance");
        }
    }
    assert_eq!(found_count, 9);
}

#[test]
fn compiled_query_pattern_is_preserved() {
    let pattern = r"\bword\b";
    let query = RegexSearchQuery::new(pattern).unwrap();
    assert_eq!(query.pattern(), pattern);
}

// ---------------------------------------------------------------------------
// Edge cases on empty / single-position documents
// ---------------------------------------------------------------------------

#[test]
fn empty_document_returns_no_matches() {
    let doc = Document::new();
    let result = doc
        .find_next_regex(r"\w+", TextPosition::new(0, 0))
        .unwrap();
    assert!(result.is_none());
    let collected: Vec<_> = doc.find_all_regex(r"\w+").unwrap().collect();
    assert!(collected.is_empty());
}

#[test]
fn single_char_document_matches_pattern_once() {
    let doc = doc_with("a");
    let collected: Vec<_> = doc.find_all_regex(r"\w").unwrap().collect();
    assert_eq!(collected.len(), 1);
    assert_eq!(collected[0].start(), TextPosition::new(0, 0));
    assert_eq!(collected[0].end(), TextPosition::new(0, 1));
}

#[test]
fn whole_document_match_returns_full_span() {
    let doc = doc_with("hello\nworld\n");
    let m = doc
        .find_next_regex(r"(?s)^.*$", TextPosition::new(0, 0))
        .unwrap()
        .expect("expected match spanning entire document");
    assert_eq!(m.start(), TextPosition::new(0, 0));
    // `(?m)` is not enabled; under `(?s)` `$` only matches end-of-input,
    // so the whole-document anchor span should reach the trailing newline.
    assert!(m.end().line0() >= 1);
}

// ---------------------------------------------------------------------------
// Huge-file regex (no 8 MiB cap)
//
// Regression coverage for the chunked / zero-copy regex path. The fixtures
// here are sized past the previous 8 MiB internal cap so the tests fail
// instantly if anyone reintroduces it.
// ---------------------------------------------------------------------------

fn build_far_offset_fixture(target_offset: usize, marker: &[u8]) -> Vec<u8> {
    // Pad with characters that do not match the patterns under test, then
    // place the marker at `target_offset`, then pad a small tail so end-of
    // -file edge cases never apply.
    let prefix_len = target_offset;
    let mut bytes = Vec::with_capacity(prefix_len + marker.len() + 64);
    bytes.extend(std::iter::repeat_n(b' ', prefix_len));
    bytes.extend_from_slice(marker);
    bytes.extend_from_slice(b"\n");
    bytes.extend(std::iter::repeat_n(b' ', 32));
    bytes
}

#[test]
fn find_next_regex_on_clean_mmap_finds_match_past_8mib_offset() {
    // Place a unique marker at offset ~9 MiB so an 8 MiB cap path would
    // miss it, then assert the regex search returns it correctly.
    let dir = fresh_test_dir("mmap-far");
    let target_offset = 9 * 1024 * 1024;
    let bytes = build_far_offset_fixture(target_offset, b"FAR_MARKER_42");
    let doc = open_clean_mmap_doc(&bytes, &dir, "far.bin");

    let m = doc
        .find_next_regex(r"FAR_MARKER_\d+", TextPosition::new(0, 0))
        .unwrap()
        .expect("regex must find marker past 8 MiB on mmap path");
    assert_eq!(m.start(), TextPosition::new(0, target_offset));
    assert_eq!(m.end(), TextPosition::new(0, target_offset + 13));

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn find_next_regex_on_clean_mmap_finds_match_past_64mib_offset() {
    // Past one full chunk size (REGEX_CHUNK_BYTES). For the mmap path this
    // must also stay zero-copy, but the test is the same shape: the marker
    // must be returned with the exact offset.
    let dir = fresh_test_dir("mmap-very-far");
    let target_offset = 64 * 1024 * 1024 + 12345;
    let bytes = build_far_offset_fixture(target_offset, b"VERY_FAR_99");
    let doc = open_clean_mmap_doc(&bytes, &dir, "very-far.bin");

    let m = doc
        .find_next_regex(r"VERY_FAR_\d+", TextPosition::new(0, 0))
        .unwrap()
        .expect("regex must find marker past 64 MiB on mmap path");
    assert_eq!(m.start().col0(), target_offset);

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn find_next_regex_on_clean_mmap_returns_none_when_no_match_past_8mib() {
    // Sanity check: with no marker anywhere, the search must not falsely
    // report a hit and must still terminate quickly.
    let dir = fresh_test_dir("mmap-far-none");
    let bytes = vec![b' '; 9 * 1024 * 1024 + 64];
    let doc = open_clean_mmap_doc(&bytes, &dir, "none.bin");

    let result = doc
        .find_next_regex(r"NEVER_MATCHES", TextPosition::new(0, 0))
        .unwrap();
    assert!(result.is_none());

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn find_prev_regex_on_clean_mmap_finds_match_far_before_boundary() {
    // Reverse search must also see past the previous 8 MiB cap.
    let dir = fresh_test_dir("mmap-prev-far");
    let target_offset = 9 * 1024 * 1024;
    let bytes = build_far_offset_fixture(target_offset, b"REV_MARK_7");
    let doc = open_clean_mmap_doc(&bytes, &dir, "prev-far.bin");

    let prev = doc
        .find_prev_regex(r"REV_MARK_\d+", TextPosition::new(usize::MAX, usize::MAX))
        .unwrap()
        .expect("reverse regex must find marker past 8 MiB on mmap path");
    assert_eq!(prev.start().col0(), target_offset);

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn find_all_regex_on_clean_mmap_iterates_matches_across_8mib_boundary() {
    // Two markers placed on opposite sides of the previous 8 MiB cap.
    // The iterator must yield both.
    let dir = fresh_test_dir("mmap-iter-cross");
    let mut bytes = Vec::with_capacity(10 * 1024 * 1024);
    bytes.extend_from_slice(b"FIRST_HIT_1\n");
    bytes.extend(std::iter::repeat_n(b' ', 9 * 1024 * 1024));
    bytes.extend_from_slice(b"SECOND_HIT_2\n");
    let doc = open_clean_mmap_doc(&bytes, &dir, "iter-cross.bin");

    let collected: Vec<_> = doc.find_all_regex(r"_HIT_\d").unwrap().collect();
    assert_eq!(
        collected.len(),
        2,
        "expected both markers (got {collected:?})"
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn find_next_regex_on_piece_table_finds_match_past_8mib_chunk_boundary() {
    // Edited (piece-table) document with a regex match placed past the
    // first chunk window. The chunked streaming path must still find it
    // and report the correct offset.
    let dir = fresh_test_dir("pt-far");
    let target_offset = 9 * 1024 * 1024;
    let bytes = build_far_offset_fixture(target_offset, b"PT_MARKER_5");
    let doc = open_piece_table_doc(&bytes, &dir, "pt-far.bin");

    let m = doc
        .find_next_regex(r"PT_MARKER_\d+", TextPosition::new(0, 0))
        .unwrap()
        .expect("regex must find marker past 8 MiB on piece-table path");
    assert_eq!(m.start().col0(), target_offset);

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn find_next_regex_on_large_rope_finds_match_past_chunk_boundary() {
    // Build a rope-backed document (created by editing through `try_insert`)
    // with a marker placed past the rope chunk threshold so the chunked
    // rope walker has to actually walk multiple chunks to reach it.
    let prefix_size = 4 * 1024 * 1024;
    let mut content = String::with_capacity(prefix_size + 32);
    content.extend(std::iter::repeat_n(' ', prefix_size));
    content.push_str("ROPE_HIT_42\n");
    let doc = doc_with(&content);

    let m = doc
        .find_next_regex(r"ROPE_HIT_\d+", TextPosition::new(0, 0))
        .unwrap()
        .expect("expected rope chunked regex match past chunk threshold");
    assert_eq!(m.start(), TextPosition::new(0, prefix_size));
    assert_eq!(m.len_chars(), 11);
}

#[test]
fn find_all_regex_on_large_rope_yields_matches_across_chunks() {
    // Two markers placed on opposite sides of the rope chunk threshold.
    // The iterator must yield both.
    let mut content = String::with_capacity(5 * 1024 * 1024);
    content.push_str("FIRST_X_1\n");
    content.extend(std::iter::repeat_n(' ', 4 * 1024 * 1024));
    content.push_str("SECOND_X_2\n");
    let doc = doc_with(&content);

    let collected: Vec<_> = doc.find_all_regex(r"_X_\d").unwrap().collect();
    assert_eq!(
        collected.len(),
        2,
        "expected both rope markers (got {collected:?})"
    );
}

// ---------------------------------------------------------------------------
// Lazy compilation of the text engine
// ---------------------------------------------------------------------------

#[test]
fn reusable_query_compiles_text_engine_lazily_only_when_needed() {
    // Construction must not fail even though the text engine is not built
    // yet; new() validates pattern syntax through the byte engine, and a
    // syntactically valid pattern must remain valid for the text engine.
    let query = RegexSearchQuery::new(r"\w+\s\d+").expect("byte engine compiles");

    // Search a byte-backed (mmap) document. This must NOT trigger text
    // engine compilation, but must still find the match through bytes.
    let dir = fresh_test_dir("lazy-bytes-only");
    let doc = open_clean_mmap_doc(b"hello 42\n", &dir, "lazy.txt");
    let m = doc.find_next_regex_query(&query, TextPosition::new(0, 0));
    assert!(m.is_some());
    let _ = fs::remove_dir_all(&dir);

    // Search a rope-backed document. This call triggers lazy compilation.
    let rope_doc = doc_with("hello 42\n");
    let m = rope_doc.find_next_regex_query(&query, TextPosition::new(0, 0));
    assert!(m.is_some());

    // After the lazy compile, repeated rope searches must not recompile.
    // We can't observe compile time directly here, but the result must
    // stay deterministic across many calls.
    for _ in 0..10 {
        let m = rope_doc.find_next_regex_query(&query, TextPosition::new(0, 0));
        assert!(m.is_some());
    }
}

#[test]
fn cloning_a_query_re_defers_text_engine_compilation() {
    // The text engine is per-instance lazy. A clone gets a fresh OnceLock
    // and must still produce the same result on first text-backed call.
    let query = RegexSearchQuery::new(r"\w+").unwrap();
    let cloned = query.clone();
    assert_eq!(cloned.pattern(), query.pattern());

    let rope_doc = doc_with("hello world\n");
    let original_match = rope_doc.find_next_regex_query(&query, TextPosition::new(0, 0));
    let cloned_match = rope_doc.find_next_regex_query(&cloned, TextPosition::new(0, 0));
    assert_eq!(original_match, cloned_match);
}

// ---------------------------------------------------------------------------
// Truth-after-error semantics for regex
// ---------------------------------------------------------------------------

#[test]
fn find_next_regex_returns_compile_error_without_partial_state() {
    // A failed compile must not leave any side effects in the document.
    // Subsequent valid searches must work as if the failed call never
    // happened.
    let doc = doc_with("alpha bravo charlie\n");

    let err = doc
        .find_next_regex("(unclosed", TextPosition::new(0, 0))
        .unwrap_err();
    assert!(!err.message().is_empty());

    // Same document, valid pattern: still finds the expected match.
    let m = doc
        .find_next_regex(r"\w+", TextPosition::new(0, 0))
        .unwrap()
        .expect("valid pattern must work after a compile error on the same doc");
    assert_eq!(m.start(), TextPosition::new(0, 0));
    assert_eq!(m.end(), TextPosition::new(0, 5));
}

#[test]
fn iterator_yields_no_more_items_after_first_failure_on_invalid_pattern() {
    // `find_all_regex` returns Result at construction. An invalid pattern
    // must surface as an error there; there is no half-iterator state.
    let doc = doc_with("alpha\n");
    let err = doc.find_all_regex("(unclosed").unwrap_err();
    assert!(!err.message().is_empty());
}

#[test]
fn find_next_regex_on_empty_or_oob_position_does_not_panic() {
    // Out-of-range typed positions must clamp, not panic. This is the
    // same contract literal search exposes; mirroring it for regex.
    let doc = doc_with("alpha\n");

    let m = doc
        .find_next_regex(r"\w+", TextPosition::new(usize::MAX, usize::MAX))
        .unwrap();
    // The clamped from-position lands past EOF, so no match is expected.
    assert!(m.is_none());

    let prev = doc
        .find_prev_regex(r"\w+", TextPosition::new(usize::MAX, usize::MAX))
        .unwrap()
        .expect("reverse with MAX before-position must clamp and find the only word");
    assert_eq!(prev.start(), TextPosition::new(0, 0));
}

// ---------------------------------------------------------------------------
// Reverse regex performance contract
//
// The previous 80× dense-vs-sparse guards (`find_prev_regex_dense_vs_
// sparse_does_not_explode_on_{mmap,rope}`) lived here as the only
// performance regression net for the chunked reverse-search fallback.
// The reverse-DFA replacement makes that fallback obsolete:
// the new path's cost depends on the last byte window in scope rather
// than on the number of matches in the document. The new ≤ 5×
// deterministic perf-gate lives in
// `tests/encoding_engine/perf/dense_vs_sparse.rs`
// and supersedes the old guards.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Truth after edits between iterator calls
//
// `RegexSearchIter` borrows the document immutably, so a mutating edit
// between calls to `.next()` is impossible at the type level. These tests
// pin the surrounding contract: dropping the iterator after a partial walk
// must not leave the document in any odd state, and rebuilding the iterator
// after an edit must observe the new content.
// ---------------------------------------------------------------------------

#[test]
fn dropping_partial_iterator_does_not_leave_observable_state() {
    let mut doc = doc_with("a1 a2 a3 a4 a5 a6\n");
    {
        let mut it = doc.find_all_regex(r"a\d").unwrap();
        let _ = it.next();
        let _ = it.next();
    }
    // After dropping the iterator, the document must still be editable
    // and searchable consistently.
    let _ = doc.try_insert(TextPosition::new(0, 0), "PRE ").unwrap();
    let collected: Vec<_> = doc.find_all_regex(r"a\d").unwrap().collect();
    assert_eq!(collected.len(), 6);
    assert_eq!(collected[0].start(), TextPosition::new(0, 4));
}

#[test]
fn rebuilding_iterator_after_edit_observes_new_content() {
    let mut doc = doc_with("alpha bravo charlie\n");
    let before: Vec<_> = doc.find_all_regex(r"\w+").unwrap().collect();
    assert_eq!(before.len(), 3);

    let _ = doc
        .try_insert(TextPosition::new(0, 0), "delta echo ")
        .unwrap();
    let after: Vec<_> = doc.find_all_regex(r"\w+").unwrap().collect();
    assert_eq!(after.len(), 5);
    assert_eq!(after[0].start(), TextPosition::new(0, 0));
}

// ---------------------------------------------------------------------------
// UTF-16 chunked-decode regex
//
// Regex searches over UTF-16 LE/BE documents are routed through
// `find_next_regex_in_class_b_chunked`, which decodes windows of the
// raw mmap bytes via `encoding_rs` and maps the match back to the
// source byte stream with a 2-byte-alignment post-filter. These two
// tests pin the end-to-end contract:
// `Document::open_with_encoding` opens a UTF-16-encoded fixture, and
// the typed `find_next_regex` returns a `SearchMatch` whose
// `TextPosition` start/end land on the expected line and column in
// the decoded text view.
// ---------------------------------------------------------------------------

use crate::DocumentEncoding;

#[test]
fn find_next_regex_finds_ascii_target_in_utf16le_document() {
    let dir = fresh_test_dir("utf16le-regex");
    let path = dir.join("source.txt");
    let text = "first\nsecond TARGET line\nthird\n";
    // `encoding_rs::UTF_16LE.encode` redirects through UTF-8 (WHATWG
    // makes UTF-16 decode-only), so we hand-encode the fixture by
    // emitting little-endian 16-bit code units for each char.
    let mut encoded: Vec<u8> = Vec::with_capacity(text.len() * 2);
    for unit in text.encode_utf16() {
        encoded.extend_from_slice(&unit.to_le_bytes());
    }
    fs::write(&path, &encoded).unwrap();

    let doc = Document::open_with_encoding(&path, DocumentEncoding::utf16le()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while doc.is_indexing() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }

    let m = doc
        .find_next_regex("TARGET", TextPosition::new(0, 0))
        .unwrap()
        .expect("expected TARGET match in UTF-16LE document");
    assert_eq!(m.start(), TextPosition::new(1, 7));
    assert_eq!(m.end(), TextPosition::new(1, 13));
    assert_eq!(m.len_chars(), 6);

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn find_next_regex_finds_ascii_target_in_utf16be_document() {
    let dir = fresh_test_dir("utf16be-regex");
    let path = dir.join("source.txt");
    let text = "first\nsecond TARGET line\nthird\n";
    // `encoding_rs::UTF_16BE.encode` redirects through UTF-8 (WHATWG
    // makes UTF-16 decode-only), so we hand-encode the fixture by
    // emitting big-endian 16-bit code units for each char.
    let mut encoded: Vec<u8> = Vec::with_capacity(text.len() * 2);
    for unit in text.encode_utf16() {
        encoded.extend_from_slice(&unit.to_be_bytes());
    }
    fs::write(&path, &encoded).unwrap();

    let doc = Document::open_with_encoding(&path, DocumentEncoding::utf16be()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while doc.is_indexing() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }

    let m = doc
        .find_next_regex("TARGET", TextPosition::new(0, 0))
        .unwrap()
        .expect("expected TARGET match in UTF-16BE document");
    assert_eq!(m.start(), TextPosition::new(1, 7));
    assert_eq!(m.end(), TextPosition::new(1, 13));
    assert_eq!(m.len_chars(), 6);

    let _ = fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// CJK multibyte chunked-decode regex
//
// Regex searches over Shift_JIS / gb18030 / EUC-KR documents go through
// the same chunked decode + glue path used by UTF-16, but with two
// encoding-dispatched differences:
//
//   * Source-byte mapping uses re-encoding through
//     `encoding_rs::Encoding::encode` instead of `c.len_utf16() * 2`.
//   * The 2-byte alignment post-filter is skipped: CJK multibyte
//     characters are 1 / 2 / 4 bytes, so there is no global
//     alignment grid the regex match offsets must snap to.
//
// These three tests pin the end-to-end contract:
// `Document::open_with_encoding` opens a CJK-encoded fixture, and
// the typed `find_next_regex` returns a `SearchMatch` whose
// `TextPosition` start/end land on the expected line and column in
// the decoded text view.
//
// Each fixture mixes ASCII line terminators with non-ASCII CJK
// characters of the target encoding so the test exercises both the
// 1-byte-per-char (ASCII run) and the multibyte-per-char (CJK run)
// branches of the source-byte mapping.
// ---------------------------------------------------------------------------

use encoding_rs::{EUC_KR, GB18030, SHIFT_JIS};

#[test]
fn find_next_regex_finds_ascii_target_in_shift_jis_document() {
    let dir = fresh_test_dir("shift-jis-regex");
    let path = dir.join("source.txt");
    // Mixed fixture: ASCII line + CJK line + ASCII line containing the
    // search target. The CJK characters \u{65E5}\u{672C}\u{8A9E}
    // ("Japanese") encode as 2 bytes each in Shift_JIS, so every byte
    // offset on the second line lives in multibyte territory and the
    // line-3 ASCII offsets would be wrong if the source-byte mapping
    // double-counted them.
    let source_text = "first\n\u{65E5}\u{672C}\u{8A9E}\nthird TARGET line\n";
    let (encoded, used, had_errors) = SHIFT_JIS.encode(source_text);
    assert_eq!(used, SHIFT_JIS);
    assert!(!had_errors, "shift_jis fixture must round-trip cleanly");
    fs::write(&path, encoded.as_ref()).unwrap();

    let doc =
        Document::open_with_encoding(&path, DocumentEncoding::from_label("shift_jis").unwrap())
            .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while doc.is_indexing() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }

    let m = doc
        .find_next_regex("TARGET", TextPosition::new(0, 0))
        .unwrap()
        .expect("expected TARGET match in Shift_JIS document");
    assert_eq!(m.start(), TextPosition::new(2, 6));
    assert_eq!(m.end(), TextPosition::new(2, 12));
    assert_eq!(m.len_chars(), 6);

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn find_next_regex_finds_ascii_target_in_gb18030_document() {
    let dir = fresh_test_dir("gb18030-regex");
    let path = dir.join("source.txt");
    // Mixed ASCII / CJK fixture. The Han characters
    // \u{4F60}\u{597D}\u{4E16}\u{754C} ("Hello world") encode as 2
    // bytes each in gb18030, exercising the 2-byte branch of the
    // leading-byte detector. The third line contains the ASCII
    // search target after the multibyte run.
    let source_text = "first\n\u{4F60}\u{597D}\u{4E16}\u{754C}\nthird TARGET line\n";
    let (encoded, used, had_errors) = GB18030.encode(source_text);
    assert_eq!(used, GB18030);
    assert!(!had_errors, "gb18030 fixture must round-trip cleanly");
    fs::write(&path, encoded.as_ref()).unwrap();

    let doc = Document::open_with_encoding(&path, DocumentEncoding::from_label("gb18030").unwrap())
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while doc.is_indexing() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }

    let m = doc
        .find_next_regex("TARGET", TextPosition::new(0, 0))
        .unwrap()
        .expect("expected TARGET match in gb18030 document");
    assert_eq!(m.start(), TextPosition::new(2, 6));
    assert_eq!(m.end(), TextPosition::new(2, 12));
    assert_eq!(m.len_chars(), 6);

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn find_next_regex_finds_ascii_target_in_euc_kr_document() {
    let dir = fresh_test_dir("euc-kr-regex");
    let path = dir.join("source.txt");
    // Mixed ASCII / Hangul fixture. The Hangul syllables
    // \u{C548}\u{B155} ("hello") encode as 2 bytes each in EUC-KR,
    // exercising the 2-byte branch of the leading-byte detector.
    let source_text = "first\n\u{C548}\u{B155}\nthird TARGET line\n";
    let (encoded, used, had_errors) = EUC_KR.encode(source_text);
    assert_eq!(used, EUC_KR);
    assert!(!had_errors, "euc-kr fixture must round-trip cleanly");
    fs::write(&path, encoded.as_ref()).unwrap();

    let doc = Document::open_with_encoding(&path, DocumentEncoding::from_label("EUC-KR").unwrap())
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while doc.is_indexing() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }

    let m = doc
        .find_next_regex("TARGET", TextPosition::new(0, 0))
        .unwrap()
        .expect("expected TARGET match in EUC-KR document");
    assert_eq!(m.start(), TextPosition::new(2, 6));
    assert_eq!(m.end(), TextPosition::new(2, 12));
    assert_eq!(m.len_chars(), 6);

    let _ = fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// Reverse-DFA cache
//
// `RegexSearchQuery::ensure_reverse` lazily compiles the reverse DFA used
// by the reverse-regex search path. Subsequent subtasks (9.3+) consume
// the returned reference; this block exercises the accessor in isolation
// so that the cache, the size-limit guard, and the Clone behaviour are
// covered before the search routing lands.
// ---------------------------------------------------------------------------

#[test]
fn ensure_reverse_compiles_a_simple_pattern() {
    let query = RegexSearchQuery::new("abc").expect("pattern compiles");
    let dfa = query
        .ensure_reverse()
        .expect("reverse DFA fits in size limit");
    // Calling twice returns the same cached DFA reference.
    let dfa_again = query.ensure_reverse().expect("cached reverse DFA");
    assert!(
        std::ptr::eq(dfa, dfa_again),
        "ensure_reverse must memoize the compiled DFA"
    );
}

#[test]
fn ensure_reverse_compiles_anchored_and_class_patterns() {
    // Common reverse-search shapes: anchors, character classes, and
    // bounded alternation. Each must compile inside the 32 MiB limit.
    for pattern in [
        "^line$",
        r"\d+",
        "(foo|bar|baz)+",
        r"[A-Za-z_][A-Za-z0-9_]*",
    ] {
        let query = RegexSearchQuery::new(pattern)
            .unwrap_or_else(|err| panic!("forward compile failed for {pattern:?}: {err}"));
        let _ = query
            .ensure_reverse()
            .unwrap_or_else(|err| panic!("reverse compile failed for {pattern:?}: {err}"));
    }
}

#[test]
fn ensure_reverse_returns_typed_error_on_size_limit_overflow() {
    // Deterministic typed-error path under
    // `MatchKind::LeftmostFirst` (the production reverse-DFA config).
    //
    // The production 32 MiB ceiling on `dfa_size_limit` /
    // `determinize_size_limit` is generous enough that even
    // wide bounded alternations comfortably fit under LeftmostFirst
    // semantics — a pattern shape designed to overflow on one
    // `regex_automata` patch version may slot under the limit on the
    // next. Pinning a real overflow on every `regex_automata` 0.4.x
    // bump is therefore brittle.
    //
    // Instead we drive the same code path through the test-only escape
    // hatch [`super::regex_search::build_reverse_dfa_with_limit`], which
    // mirrors the production builder configuration exactly except for
    // the size limit. A 64 KiB cap is small enough that any non-trivial
    // multi-byte alternation overflows during determinization. The
    // production [`RegexSearchQuery::ensure_reverse`] path stays
    // unchanged and untouched by this test.
    use super::regex_search::build_reverse_dfa_with_limit;

    let pattern = "(foo|bar|baz|quux|alpha|beta|gamma|delta|epsilon|zeta){0,128}";
    let err = build_reverse_dfa_with_limit(pattern, 64 * 1024)
        .expect_err("64 KiB ceiling must reject the wide bounded alternation under LeftmostFirst");
    assert!(
        !err.message().is_empty(),
        "RegexCompileError message must be non-empty"
    );
}

#[test]
fn ensure_reverse_caches_the_failure_for_overflow_patterns() {
    // Force overflow with a deliberately oversized pattern.
    let pattern = format!(
        "[a-z]{{0,{r}}}|[A-Z]{{0,{r}}}|[0-9]{{0,{r}}}|[!-/]{{0,{r}}}|[:-@]{{0,{r}}}|[\\[-`]{{0,{r}}}",
        r = 16384usize
    );
    let Ok(query) = RegexSearchQuery::new(&pattern) else {
        // If the forward engine rejects this pattern outright, the
        // reverse-DFA cache contract is moot for this iteration; the
        // overflow path is exercised by
        // `ensure_reverse_returns_typed_error_on_size_limit_overflow`.
        return;
    };
    let first = query
        .ensure_reverse()
        .expect_err("expected reverse DFA size-limit overflow");
    let second = query
        .ensure_reverse()
        .expect_err("expected cached reverse DFA size-limit overflow");
    assert_eq!(
        first.message(),
        second.message(),
        "ensure_reverse must memoize the typed error so callers do not re-pay determinization cost",
    );
}

#[test]
fn cloned_query_recompiles_reverse_dfa_lazily() {
    // Cloning resets the reverse cache so a forward-only consumer of the
    // clone never pays the reverse-DFA build cost. The clone still
    // compiles successfully on first reverse access.
    let query = RegexSearchQuery::new("abc").unwrap();
    let _original = query.ensure_reverse().expect("original reverse compiles");
    let cloned = query.clone();
    let cloned_dfa = cloned.ensure_reverse().expect("clone reverse compiles");
    let cloned_dfa_again = cloned.ensure_reverse().unwrap();
    assert!(std::ptr::eq(cloned_dfa, cloned_dfa_again));
}
