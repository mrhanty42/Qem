// Deterministic regression for the repeated reverse-regex walk that used to
// be explored by a temporary 2,000-case diagnostic harness. The old harness
// created one mmap fixture per generated case and only printed mismatches;
// this bounded test asserts the same forward/reverse symmetry on both rope
// and mmap backings using a dense match set that exercises repeated `find_prev`.

#[path = "mod.rs"]
#[allow(clippy::duplicate_mod)]
mod helpers;

use helpers::fresh_test_dir;
use qem::{Document, RegexSearchQuery, TextPosition};
use std::fs;
use std::time::{Duration, Instant};

fn collect_forward(doc: &Document, query: &RegexSearchQuery) -> Vec<(TextPosition, TextPosition)> {
    doc.find_all_regex_query(query)
        .map(|matched| (matched.start(), matched.end()))
        .collect()
}

fn collect_reverse(
    doc: &Document,
    query: &RegexSearchQuery,
    expected_match_count: usize,
) -> Vec<(TextPosition, TextPosition)> {
    let mut out = Vec::with_capacity(expected_match_count);
    let mut before = TextPosition::new(usize::MAX, usize::MAX);

    for _ in 0..expected_match_count.saturating_add(2) {
        let Some(matched) = doc.find_prev_regex_query(query, before) else {
            out.reverse();
            return out;
        };
        assert!(
            matched.start() < before,
            "reverse regex walk must strictly lower its before position"
        );
        out.push((matched.start(), matched.end()));
        before = matched.start();
    }

    panic!(
        "reverse regex walk exceeded forward match count: expected {expected_match_count}, observed at least {}",
        out.len()
    );
}

fn assert_symmetric(doc: &Document, query: &RegexSearchQuery, backing: &str) {
    let forward = collect_forward(doc, query);
    let reverse = collect_reverse(doc, query, forward.len());
    assert_eq!(
        forward, reverse,
        "{backing}: repeated reverse walk must equal the forward match sequence"
    );
}

#[test]
fn repeated_reverse_walk_is_symmetric_on_rope_and_mmap() {
    let content = "X1 alpha 22 beta X3\nfoo 444 bar X5\n".repeat(8);
    let query = RegexSearchQuery::new(r"[A-Za-z0-9]+").expect("valid regression regex");

    let mut rope_doc = Document::new();
    rope_doc
        .try_insert(TextPosition::new(0, 0), &content)
        .expect("seed rope regression document");
    assert_symmetric(&rope_doc, &query, "rope");

    let dir = fresh_test_dir("reverse-regex-repeated-walk");
    let path = dir.join("mmap.txt");
    fs::write(&path, content.as_bytes()).expect("write mmap regression fixture");
    let mmap_doc = Document::open(&path).expect("open mmap regression fixture");
    let deadline = Instant::now() + Duration::from_secs(5);
    while mmap_doc.is_indexing() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_symmetric(&mmap_doc, &query, "mmap");

    drop(mmap_doc);
    let _ = fs::remove_dir_all(&dir);
}
