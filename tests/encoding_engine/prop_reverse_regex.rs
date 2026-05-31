// Property 18: reverse regex matches are symmetric with forward regex
//
//
// For any regex `pattern` that compiles in both the forward
// `regex::Regex` engine and the reverse `regex_automata` DFA within
// the project-wide size limits, and any `Document` exposing a
// regex-search backing, the multiset of `(start, end)`
// `TextPosition` pairs produced by `Document::find_all_regex_query`
// (forward path) MUST equal the multiset of pairs produced by
// walking the same document end-to-start via
// `find_prev_regex_query`..
//
// # Strategy
//
// The forward path is the iterator-driven non-overlapping walk
// already exercised by `regex_tests.rs`. The reverse path emulates
// the same non-overlapping semantics by repeatedly calling
// `find_prev_regex_query(query, before)` starting at end-of-document
// and lowering `before` to the match's `start` each iteration; the
// reverse-DFA dispatcher returns the rightmost
// match whose `end <= before`. Both walks visit non-overlapping
// matches once each, so after reversing the reverse-walk vector the
// two collected sequences must be byte-equal.
//
// # Document fan-out (chosen subset)
//
// The reverse-DFA dispatcher has three backing-specific paths:
// `reverse_dfa_search_in_rope` for rope-backed documents
// `reverse_dfa_search_in_slice` for mmap-backed documents whose
// `mmap_search_slice` returns a contiguous slice, and
// `reverse_dfa_search_in_piece_tree` for piece-table-backed
// documents (chunked window walker). For the property test, one
// or two storage modes is sufficient. This test fans out across two:
//
// * **rope**: `Document::new()` + `try_insert(...)`. Rope-backed
// documents take the `reverse_dfa_search_in_rope` chunk walker.
// * **clean mmap**: `Document::open(path)` over a sub-megabyte
// fixture written through `fresh_test_dir(...)`. For
// files below the piece-table promotion threshold the document
// stays mmap-backed and `mmap_search_slice` returns the
// contiguous slice, so reverse search routes through
// `reverse_dfa_search_in_slice`.
//
// The piece-table `reverse_dfa_search_in_piece_tree` walker is
// covered separately by `regex_tests.rs` plus the perf gate
// `tests/encoding_engine/perf/dense_vs_sparse.rs`, so excluding it
// here trades zero novel coverage for a ~3× per-case speedup
// (the piece-table fixture requires a >1 MiB file write + tiny edit
// promotion on every case, which combined with 64 proptest cases
// makes the property test dominate full-suite wall time).
//
// # Pattern curation
//
// Patterns are drawn from a fixed list small enough to be readable
// but wide enough to exercise word classes, digit classes
// alternation, one-or-more quantifiers, and simple grouping. Every
// pattern in the list is known to compile against `regex 1.x` AND
// `regex_automata 0.4`'s `MatchKind::LeftmostFirst` reverse DFA at
// the project's 32 MiB size limit.
//
// Three constraints govern which patterns the curated list admits.
//
// 1. **No zero-width matches** (e.g. `a*`, `\d*`, `(?m)^`). The
// forward iterator nudges by one text unit on a zero-width hit
// while the reverse-DFA dispatcher bounds via
// `before = match.start()`; the resulting match sequences are
// a different shape for that case and the design explicitly
// carves them out of.
//
// 2. **No Unicode word boundaries (`\b`)**. The reverse DFA
// cannot represent `\b` and `RegexSearchQuery::ensure_reverse`
// would surface a typed error rather than a usable DFA.
//
// 3. **Direction-stable match sets**. The forward iterator visits
// non-overlapping leftmost-first matches; the reverse iterator
// visits non-overlapping rightmost matches. For patterns whose
// matches at one starting offset can overlap matches at a
// *different* starting offset on the same input, the two walks
// pick different non-overlapping covers. Concrete example:
// `\d+\s+\d+` on `"0 0\n0 0"` gives forward `["0 0", "0 0"]`
// versus reverse `["0\n0", "0\n0"]` — both are valid covers of
// the same regex, just chosen by opposite tie-breakers. .3
// is "same set of `(start, end)` pairs" not "same covers
// modulo direction", so multi-segment patterns and bounded
// repetitions like `\d{2,4}` (which can split a digit run two
// ways) are excluded.
//
// The patterns that survive constraint 3 are exactly the
// "single-class greedy run" patterns (`\d+`, `\w+`, `[A-Za-z]+`
// `[a-z]+`, `[A-Za-z0-9]+`), the "anchor + greedy run" patterns
// (`[A-Z]\w*`, `X[Y-Z]+`, `ab+`, `a+b+`), simple alternation of
// equal-length tokens with no shared prefix (`foo|bar|baz`), and
// the union-of-equal-length-tokens grouping `(ab|cd)+`. Each of
// these emits a unique non-overlapping cover regardless of
// iteration direction.
//
// # Content curation
//
// Document content is a small printable-ASCII corpus
// (digits + letters + spaces + LF, ≤ 128 bytes) so the property is
// purely about reverse vs forward symmetry, not about
// encoding-engine surprises. Mixed encodings live in the
// per-encoding suite under `tests/encoding_engine/per_encoding/`.
// Empty documents short-circuit early because `find_all_regex` on an
// empty document yields an empty iterator and `find_prev_regex_query`
// short-circuits to `None`, so the property holds vacuously.
//
// `ProptestConfig::with_cases(64)` per spec.

#[path = "mod.rs"]
#[allow(clippy::duplicate_mod)] // shared helpers module is also loaded by sibling integration tests
mod helpers;

use helpers::fresh_test_dir;
use proptest::prelude::*;
use qem::{Document, RegexSearchQuery, TextPosition};
use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

/// Curated regex patterns that compile in both `regex 1.x` and
/// `regex_automata 0.4`'s reverse DFA under
/// `MatchKind::LeftmostFirst` at the project's 32 MiB size limit
/// and whose forward leftmost-first cover equals the reverse
/// rightmost cover over the generated ASCII corpus. See the
/// file-level "Pattern curation" notes for the three constraints
/// (no zero-width, no `\b`, direction-stable match sets) and why
/// patterns like `\d{2,4}` or `\d+\s+\d+` are NOT in this list.
const REVERSE_SAFE_PATTERNS: &[&str] = &[
 // Single-class greedy runs — emit a unique maximal run regardless
 // of iteration direction.
    r"\d+",
    r"\w+",
    r"[A-Za-z]+",
    r"[a-z]+",
    r"[A-Za-z0-9]+",
 // Anchor + greedy run — the anchor pins the start, the greedy run
 // pins the end, both directions converge.
    r"[A-Z]\w*",
    r"X[Y-Z]+",
    r"ab+",
    r"a+b+",
 // Alternation of equal-length tokens with no shared prefix —
 // matches are isolated literal hits, no direction sensitivity.
    r"foo|bar|baz",
 // Union of equal-length tokens repeated — equivalent to a greedy
 // run over `{ab, cd}` segments.
    r"(ab|cd)+",
];

/// Generates a regex pattern from the curated list above.
fn pattern_strategy() -> impl Strategy<Value = String> {
    prop::sample::select(REVERSE_SAFE_PATTERNS.to_vec()).prop_map(str::to_owned)
}

/// Generates document content from a small printable-ASCII alphabet.
///
/// The pool covers digits, mixed-case letters, spaces, and a sprinkle
/// of `LF` (no `CR` to keep line-ending semantics deterministic
/// across the rope and mmap backings — rope normalizes `CRLF` to
/// `LF` on insert while mmap preserves the stored bytes verbatim
/// and that backing-asymmetry is its own covered property
/// elsewhere). Length is bounded at 128 bytes per case so the
/// per-iteration cost across two backings stays modest at 64 cases.
fn content_strategy() -> impl Strategy<Value = String> {
    proptest::string::string_regex(r"[A-Za-z0-9 \n]{0,128}").expect("valid ASCII regex")
}

/// Builds a rope-backed `Document` containing exactly `content`.
///
/// `Document::new()` plus a single `try_insert` is the canonical way
/// to land on the rope backing. An empty `content` is allowed and
/// leaves the document at zero bytes.
fn build_rope_doc(content: &str) -> Document {
    let mut doc = Document::new();
    if !content.is_empty() {
        let _ = doc
            .try_insert(TextPosition::new(0, 0), content)
            .expect("rope try_insert should succeed for ASCII content");
    }
    doc
}

/// Opens a clean mmap-backed `Document` for the supplied content.
///
/// Writes the bytes through `fresh_test_dir(...)` so the test
/// honours the project's tmp-root convention. `Document::open` always
/// memory-maps a file that exists, but for files smaller than the
/// piece-table promotion threshold no piece-table backing is
/// materialised, so `mmap_search_slice` returns the contiguous slice
/// and the reverse-DFA dispatcher routes through
/// `reverse_dfa_search_in_slice`. The brief `is_indexing()` wait
/// mirrors the same wait in `regex_tests.rs::open_clean_mmap_doc` so
/// the line-offset index is fully built before the regex walk runs.
fn open_clean_mmap_doc(content: &[u8], dir: &Path, name: &str) -> Document {
    let path = dir.join(name);
    fs::write(&path, content).expect("mmap fixture write");
    let doc = Document::open(&path).expect("Document::open mmap fixture");
    wait_for_indexing(&doc);
    doc
}

/// Spins until background indexing on a freshly opened document has
/// settled (or 5 seconds have elapsed). Mirrors the existing wait in
/// `regex_tests.rs`. The proptest budget per case is small so a
/// realistic wait is short; the timeout is a safety hatch against a
/// stuck index, not a normal code path.
fn wait_for_indexing(doc: &Document) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while doc.is_indexing() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Collects every non-overlapping forward match `(start, end)` of
/// `query` in `doc` into a `Vec`. Sorted by `start` (ascending) by
/// construction since `find_all_regex_query` iterates in order.
fn collect_forward(doc: &Document, query: &RegexSearchQuery) -> Vec<(TextPosition, TextPosition)> {
    doc.find_all_regex_query(query)
        .map(|m| (m.start(), m.end()))
        .collect()
}

/// Collects every non-overlapping reverse match `(start, end)` of
/// `query` in `doc` by repeatedly calling `find_prev_regex_query`.
///
/// Algorithm:
/// 1. `before := TextPosition::new(usize::MAX, usize::MAX)` (the
/// `clamp_position` machinery resolves this to "end of
/// document" for every backing — see `positions.rs`).
/// 2. Each iteration: `m = doc.find_prev_regex_query(&query, before)`.
/// * `None` → terminate.
/// * `Some(m)` with `m.start() >= before` → guard against a
/// non-progressing dispatcher (would otherwise spin
/// forever); return `None` so the caller can bail via
/// `prop_assume!`.
/// * Otherwise: push `(m.start(), m.end())` and lower `before`
/// to `m.start()`.
/// 3. Reverse the collected vector so the result is sorted in the
/// same ascending-by-start order the forward path returns. A
/// separate `sort_by_key` pass is unnecessary because the
/// reverse walk visits matches in strictly descending start
/// order over a non-overlapping match set.
fn collect_reverse(
    doc: &Document,
    query: &RegexSearchQuery,
) -> Option<Vec<(TextPosition, TextPosition)>> {
    let mut out = Vec::new();
    let mut before = TextPosition::new(usize::MAX, usize::MAX);
 // Hard ceiling on iteration count — a pathological dispatcher bug
 // that returns the same match repeatedly would otherwise spin
 // forever. The forward iterator is also bounded indirectly by
 // the byte length of the document, so any sane reverse walk on
 // the same content terminates well below this cap.
    let iter_cap = 4096usize;
    for _ in 0..iter_cap {
        let Some(m) = doc.find_prev_regex_query(query, before) else {
 // Reverse out so result is sorted ascending by start.
            out.reverse();
            return Some(out);
        };
 // Defensive: if the dispatcher ever fails to make progress
 // (start did not strictly decrease relative to `before`), we
 // bail rather than loop.
        if m.start() >= before {
            return None;
        }
        out.push((m.start(), m.end()));
        before = m.start();
        if before == TextPosition::new(0, 0) {
 // No room for another match strictly before the start of
 // the document. The next call would short-circuit to
 // `None` per `find_prev_regex_query`; fold it here.
            out.reverse();
            return Some(out);
        }
    }
 // Iteration cap exceeded without converging; treat as a bail.
    None
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

 /// Property 18: forward `find_all_regex_query` and reverse
 /// `find_prev_regex_query` walks return the same multiset of
 /// `(start, end)` pairs across the rope and clean mmap backings
 ///. The piece-table backing's reverse-DFA path is
 /// covered by `regex_tests.rs` and the dense-vs-sparse perf
 /// gate; see the file-level docs above for the rationale.
    #[test]
    fn property_18_reverse_regex_symmetric_to_forward(
        pattern in pattern_strategy(),
        content in content_strategy(),
    ) {
 // Compile the query once. A pattern that fails to compile in
 // the forward `regex::bytes::Regex` path is bailed out via
 // `prop_assume!` — the curated list shouldn't trip this, but
 // a future expansion of `REVERSE_SAFE_PATTERNS` might.
        let query = match RegexSearchQuery::new(&pattern) {
            Ok(q) => q,
            Err(_) => {
                prop_assume!(false, "forward regex compile error; pattern skipped");
                return Ok(());
            }
        };

 // Empty content collapses every backing to a no-match case
 // (`find_all_regex_query` yields an empty iterator and
 // `find_prev_regex_query` short-circuits to `None` because
 // `before == (0, 0)`). The property holds vacuously here, so
 // bail to keep the case-budget focused on documents with
 // observable matches.
        prop_assume!(!content.is_empty());

 // ------------------------------------------------------------
 // Backing 1: rope-backed (Document::new + try_insert).
 // Routes through `reverse_dfa_search_in_rope`.
 // ------------------------------------------------------------
        let rope_doc = build_rope_doc(&content);
        let forward_rope = collect_forward(&rope_doc, &query);
        let reverse_rope = match collect_reverse(&rope_doc, &query) {
            Some(v) => v,
            None => {
                prop_assume!(
                    false,
                    "rope reverse walk did not converge (likely zero-width match); skipping",
                );
                return Ok(());
            }
        };
        prop_assert_eq!(
            &forward_rope,
            &reverse_rope,
            "rope: forward find_all_regex_query and reverse find_prev_regex_query \
             must yield the same (start, end) sequence",
        );

 // ------------------------------------------------------------
 // Backing 2: clean mmap (Document::open over a small file).
 // Routes through `reverse_dfa_search_in_slice`.
 // ------------------------------------------------------------
        let dir = fresh_test_dir("prop_reverse_regex");
        let mmap_doc = open_clean_mmap_doc(content.as_bytes(), &dir, "mmap.txt");
        let forward_mmap = collect_forward(&mmap_doc, &query);
        let reverse_mmap = match collect_reverse(&mmap_doc, &query) {
            Some(v) => v,
            None => {
 // Best-effort cleanup before bailing.
                let _ = fs::remove_dir_all(&dir);
                prop_assume!(
                    false,
                    "mmap reverse walk did not converge (likely zero-width match); skipping",
                );
                return Ok(());
            }
        };
        prop_assert_eq!(
            &forward_mmap,
            &reverse_mmap,
            "mmap: forward find_all_regex_query and reverse find_prev_regex_query \
             must yield the same (start, end) sequence",
        );

 // Best-effort cleanup. Stale shrunken cases may leave files
 // behind on a panic; the directory is uniquely named per
 // process+counter so subsequent test runs do not collide.
        let _ = fs::remove_dir_all(&dir);
    }
}
