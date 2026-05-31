// Temporary diagnostic test. Replays the saved proptest seed against
// the current pattern_strategy() / content_strategy() to find the
// counterexample that the cc seed re-derives. Will be deleted once
// the root cause is identified.

#[path = "mod.rs"]
#[allow(clippy::duplicate_mod)]
mod helpers;

use helpers::fresh_test_dir;
use proptest::prelude::*;
use proptest::strategy::ValueTree;
use proptest::test_runner::{Config, TestRunner};
use qem::{Document, RegexSearchQuery, TextPosition};
use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

const REVERSE_SAFE_PATTERNS: &[&str] = &[
    r"\d+",
    r"\w+",
    r"[A-Za-z]+",
    r"[a-z]+",
    r"[A-Za-z0-9]+",
    r"[A-Z]\w*",
    r"X[Y-Z]+",
    r"ab+",
    r"a+b+",
    r"foo|bar|baz",
    r"(ab|cd)+",
];

fn pattern_strategy() -> impl Strategy<Value = String> {
    prop::sample::select(REVERSE_SAFE_PATTERNS.to_vec()).prop_map(str::to_owned)
}

fn content_strategy() -> impl Strategy<Value = String> {
    proptest::string::string_regex(r"[A-Za-z0-9 \n]{0,128}").expect("valid ASCII regex")
}

fn build_rope_doc(content: &str) -> Document {
    let mut doc = Document::new();
    if !content.is_empty() {
        let _ = doc
            .try_insert(TextPosition::new(0, 0), content)
            .expect("rope try_insert");
    }
    doc
}

fn open_clean_mmap_doc(content: &[u8], dir: &Path, name: &str) -> Document {
    let path = dir.join(name);
    fs::write(&path, content).expect("mmap fixture write");
    let doc = Document::open(&path).expect("Document::open mmap fixture");
    let deadline = Instant::now() + Duration::from_secs(5);
    while doc.is_indexing() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    doc
}

fn collect_forward(doc: &Document, query: &RegexSearchQuery) -> Vec<(TextPosition, TextPosition)> {
    doc.find_all_regex_query(query)
        .map(|m| (m.start(), m.end()))
        .collect()
}

fn collect_reverse(
    doc: &Document,
    query: &RegexSearchQuery,
) -> Option<Vec<(TextPosition, TextPosition)>> {
    let mut out = Vec::new();
    let mut before = TextPosition::new(usize::MAX, usize::MAX);
    for _ in 0..4096 {
        let Some(m) = doc.find_prev_regex_query(query, before) else {
            out.reverse();
            return Some(out);
        };
        if m.start() >= before {
            return None;
        }
        out.push((m.start(), m.end()));
        before = m.start();
        if before == TextPosition::new(0, 0) {
            out.reverse();
            return Some(out);
        }
    }
    None
}

#[test]
fn diag_explore_strategy() {
 // Run a focused TestRunner over the strategy, looking only at
 // (pattern, content) pairs that fail the property. Don't assert
 // just print so we can find counterexamples deterministically.
    let strategy = (pattern_strategy(), content_strategy());
    let cfg = Config {
        cases: 2000,
        max_shrink_iters: 0,
        ..Config::default()
    };
    let mut runner = TestRunner::new(cfg);
    let dir = fresh_test_dir("diag_explore_strategy");
    let mut mismatches = 0usize;
    for i in 0..2000 {
        let tree = strategy.new_tree(&mut runner).expect("new_tree");
        let (pattern, content) = tree.current();
        if content.is_empty() {
            continue;
        }
        let q = match RegexSearchQuery::new(&pattern) {
            Ok(q) => q,
            Err(_) => continue,
        };
        let rope_doc = build_rope_doc(&content);
        let f_rope = collect_forward(&rope_doc, &q);
        let r_rope = match collect_reverse(&rope_doc, &q) {
            Some(v) => v,
            None => continue,
        };
        if f_rope != r_rope {
            mismatches += 1;
            println!(
                "[{i}] ROPE MISMATCH: pattern={:?} content={:?}\n  forward={:?}\n  reverse={:?}",
                pattern, content, f_rope, r_rope
            );
            if mismatches >= 3 {
                break;
            }
            continue;
        }
        let mmap_doc = open_clean_mmap_doc(content.as_bytes(), &dir, &format!("f_{i}.txt"));
        let f_mmap = collect_forward(&mmap_doc, &q);
        let r_mmap = match collect_reverse(&mmap_doc, &q) {
            Some(v) => v,
            None => continue,
        };
        if f_mmap != r_mmap {
            mismatches += 1;
            println!(
                "[{i}] MMAP MISMATCH: pattern={:?} content={:?}\n  forward={:?}\n  reverse={:?}",
                pattern, content, f_mmap, r_mmap
            );
            if mismatches >= 3 {
                break;
            }
        }
    }
    let _ = fs::remove_dir_all(&dir);
    println!("total mismatches in 2000 cases: {mismatches}");
}
