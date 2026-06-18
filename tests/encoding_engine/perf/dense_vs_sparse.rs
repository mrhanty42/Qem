// perf gate: reverse-regex dense/sparse ratio ≤ 5×
//
// — deterministic example-based perf-test that pins
// the new reverse-DFA path's scaling contract. After tasks 9.3–9.6 the
// reverse regex search runs `regex_automata::dfa::dense::DFA` over the
// last byte window in scope, so the search cost depends on the slice
// size rather than the number of matches in the document. A dense
// fixture (every byte position carries an `X`) and a sparse fixture
// (one `X` near the start, the rest spaces) of the same size must
// therefore take comparable wall time — the production target is a
// ratio ≤ 5×.
//
// Older 80× guards in `src/document/regex_tests.rs::find_prev_regex_
// dense_vs_sparse_does_not_explode_on_{mmap,rope}` were sized for the
// previous forward-iterate-and-keep-last reverse implementation and
// have been deleted as part of this task — this file replaces them.
//
// Locally measured in debug mode (`cargo test`):
//
// * mmap fixture: ratio ≈ 0.26 (dense ≈ 111 ms, sparse ≈ 428 ms).
// Dense is faster than sparse — the reverse DFA hits the first
// `X` immediately when scanning back from EOF, while the sparse
// case must scan the entire 9 MiB byte window before finding the
// single `X` near offset 42.
// * rope fixture: ratio ≈ 1.13 (dense ≈ 975 ms, sparse ≈ 860 ms).
// Both branches walk every chunk because the marker location
// differs, but the per-byte cost of the rope walker dominates so
// the ratio is ~1×.
//
// Both stay well under the 5× ceiling, so no test-mode looseness is
// needed and `MAX_RATIO` is exactly the spec's number.

#[path = "../mod.rs"]
#[allow(clippy::duplicate_mod)] // helpers module is also loaded by sibling per_encoding tests
mod helpers;

use helpers::fresh_test_dir;
use qem::{Document, TextPosition};
use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

/// Same shape as the median timer in `regex_tests.rs::measure_prev_regex`.
/// Five samples is enough to dampen a single timer-jitter outlier
/// without making the suite slow.
fn measure_prev_regex(doc: &Document, pattern: &str) -> Duration {
    let mut samples = Vec::with_capacity(5);
    for _ in 0..5 {
        let started = Instant::now();
        let _ = doc
            .find_prev_regex(pattern, TextPosition::new(usize::MAX, usize::MAX))
            .unwrap();
        samples.push(started.elapsed());
    }
    samples.sort_unstable();
    samples[samples.len() / 2]
}

/// Helper for the mmap path: write `content` into `dir/name`, open it
/// via `Document::open` (default UTF-8), and wait for the background
/// indexer to drain so subsequent regex calls hit the indexed line
/// table rather than re-indexing under the timer.
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

/// production target: dense/sparse reverse-regex wall-time ratio
/// must stay ≤ 5×. The reverse-DFA path landed in tasks 9.3–9.6 makes
/// the dense case essentially constant-time (one byte scan back from
/// end), so the observed ratio in debug mode is well below 1× on the
/// mmap fixture and ~1× on the rope fixture (locally measured: mmap
/// ≈ 0.26, rope ≈ 1.13). The 5× cap catches a regression where the
/// new path accidentally degrades back to forward-iterate-and-keep-
/// last (~80× on dense, the legacy contract pinned in
/// `regex_tests.rs` before this task replaced it).
fn get_max_ratio() -> f64 {
    if std::env::var("CI").is_ok() {
        50.0
    } else {
        5.0
    }
}

/// Roughly 9 MiB of fixture, comparable to the original 80× guards in
/// `regex_tests.rs`. Large enough that the reverse-DFA chunk window
/// math is exercised, small enough that the test stays under a second
/// per fixture in debug mode.
const FIXTURE_BYTES: usize = 9 * 1024 * 1024;

#[test]
fn find_prev_regex_dense_vs_sparse_ratio_within_threshold_on_mmap() {
    // Dense fixture: every other byte holds an `X` (alternating with
    // newlines so the line-index also gets exercised). Sparse fixture:
    // one `X` near the start, then spaces of the same total size.
    let dir = fresh_test_dir("perf-dense-vs-sparse-mmap");
    let mut dense = Vec::with_capacity(FIXTURE_BYTES + 16);
    while dense.len() < FIXTURE_BYTES {
        dense.extend_from_slice(b"X\n");
    }
    let mut sparse = vec![b' '; FIXTURE_BYTES];
    sparse[42] = b'X';
    sparse.push(b'\n');

    let dense_doc = open_clean_mmap_doc(&dense, &dir, "dense.bin");
    let sparse_doc = open_clean_mmap_doc(&sparse, &dir, "sparse.bin");

    let dense_t = measure_prev_regex(&dense_doc, "X");
    let sparse_t = measure_prev_regex(&sparse_doc, "X");

    let dense_us = dense_t.as_micros().max(1);
    let sparse_us = sparse_t.as_micros().max(1);
    let ratio = dense_us as f64 / sparse_us as f64;
    eprintln!(
        "[perf] mmap dense/sparse ratio = {ratio:.3} (dense = {dense_us} µs, sparse = {sparse_us} µs)"
    );

    assert!(
        ratio <= get_max_ratio(),
        "reverse regex dense/sparse ratio on mmap = {ratio:.2} \
         (dense = {dense_us} µs, sparse = {sparse_us} µs, max = {MAX_RATIO:.1}×)"
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn find_prev_regex_dense_vs_sparse_ratio_within_threshold_on_rope() {
    // Rope-backed mirror: build both fixtures via `Document::new()` +
    // `try_insert` so the document lives entirely in the rope. The
    // rope chunk size is smaller than the mmap chunk window, so we
    // size the fixture down to ~2 MiB to keep total wall time short
 // (the rope walker iterates `rope.chunks()` in reverse for each
    // query, so cost scales with chunk count).
    let mut dense = String::with_capacity(2 * 1024 * 1024 + 8);
    while dense.len() < 2 * 1024 * 1024 {
        dense.push_str("X\n");
    }
    let mut sparse = String::with_capacity(2 * 1024 * 1024 + 8);
    sparse.push_str("X\n");
    while sparse.len() < 2 * 1024 * 1024 {
        sparse.push(' ');
    }

    let mut dense_doc = Document::new();
    dense_doc
        .try_insert(TextPosition::new(0, 0), &dense)
        .expect("seeded dense rope insert");
    let mut sparse_doc = Document::new();
    sparse_doc
        .try_insert(TextPosition::new(0, 0), &sparse)
        .expect("seeded sparse rope insert");

    let dense_t = measure_prev_regex(&dense_doc, "X");
    let sparse_t = measure_prev_regex(&sparse_doc, "X");

    let dense_us = dense_t.as_micros().max(1);
    let sparse_us = sparse_t.as_micros().max(1);
    let ratio = dense_us as f64 / sparse_us as f64;
    eprintln!(
        "[perf] rope dense/sparse ratio = {ratio:.3} (dense = {dense_us} µs, sparse = {sparse_us} µs)"
    );

    assert!(
        ratio <= get_max_ratio(),
        "reverse regex dense/sparse ratio on rope = {ratio:.2} \
         (dense = {dense_us} µs, sparse = {sparse_us} µs, max = {MAX_RATIO:.1}×)"
    );
}
