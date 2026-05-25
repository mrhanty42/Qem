# Qem Roadmap

This roadmap is a release plan, not a wish list.

The immediate goal is not to add more subsystems before `0.7.0`. The goal is
to make Qem coherent for real frontend integration, then ship a fast and
complete search/encoding contract, then harden the rest, then freeze the
public API at `1.0.0`.

## Planning Rules

- Integration beats expansion. Examples and real frontend workflows decide
  which API changes are justified.
- Keep GUI dependencies out of `qem` core. Frontend demos can live in a
  separate demo crate or workspace member if they need `egui` or another UI
  stack.
- Prefer typed public surface over private-detail escape hatches and glue code.
- Treat performance as a measured contract on real files, not just synthetic
  microbenchmarks.
- Treat sidecars and internal storage layout as implementation details unless
  they are explicitly promoted into the public contract.
- Every release ships with the regression tests it needs. "Truth after error"
  coverage (failed save, failed open, discarded stale worker, deferred close,
  set_path / document_mut while busy) is added incrementally as the surfaces
  it covers stabilize, not bundled into a single later release.

## Current Release Line

Current published release: `0.7.0`

Current target: `0.8.0`

## 0.7.0

`0.7.0` is the integration release.

### Goal

A frontend developer should be able to open the README, run the official
examples, and integrate Qem into an `egui`-style application without reading
through internal modules first.

### Release Gates

Before shipping `0.7.0`, Qem should have all of the following:

- A cleaned-up public integration story around `Document`, `DocumentSession`,
  and `EditorTab`, with only the API changes that real examples actually
  require.
- Two official frontend-oriented examples:
  - a minimal viewer/editor
  - a large-file editor with viewport, gutter, caret, open/save, and visible
    load/save status
- README and rustdoc rewritten around integration workflow:
  - what Qem owns
  - what the application owns
  - basic lifecycle
  - viewport rendering
  - open/save flow
  - huge-file editing boundaries
  - when backing may change
- A support matrix written as a contract instead of general prose:
  - UTF-8 stable guarantees
  - invalid UTF-8 behavior
  - large-file guarantees
  - `EditUnsupported` boundaries
  - sidecar behavior
  - public behavior vs internal format
- Scenario coverage for real workflows:
  - async open -> viewport -> edit -> save
  - large-file open with background indexing while UI stays usable
  - reopen from `.qem.editlog`
  - truthful rejection near huge-file edit limits
  - CRLF behavior
  - invalid UTF-8 behavior
  - very long lines
  - save failure with truthful post-error state
- Release hygiene:
  - changelog
  - migration notes
  - explicit breaking vs non-breaking summary
  - examples built in CI
  - at least one example smoke run in CI
- A baseline real-file perf matrix recorded in-repo so later regressions are
  visible.

### Non-Goals

Do not do these before `0.7.0`:

- regex search (planned for `0.8.0`)
- broader encoding stabilization beyond the current contract (planned for `0.8.0`)
- GUI code inside `qem` core
- speculative refactors "for the future"
- broad abstractions without a demonstrated integration use case

### Exit Criteria

`0.7.0` is ready when:

- the examples are the official frontend entry point
- the API changes are justified by those examples
- the README/support matrix match the actual runtime behavior
- examples and scenario tests make the integration path reproducible
- `cargo test --all-features --workspace` is green on all CI targets

## 0.8.0

`0.8.0` is the search and encoding release. It is the first half of the
larger "fast, full search + stable encodings + general stability" block.

### Focus

- **Regex search.** Add a typed regex search surface alongside the existing
  literal search:
  - `find_next` / `find_prev` / `find_all` analogues for regex
  - reusable compiled `RegexSearchQuery` mirroring `LiteralSearchQuery`
  - bounded `_in_range` / `_between` variants
  - typed `SearchMatch` results so frontends keep one shape across literal
    and regex search
- **Search performance.** Make literal and regex search practically the
  fastest path on `mmap`, `piece_table`, and `rope` backings:
  - real numbers on 1 GiB / 10 GiB / 50 GiB files, not just synthetic
    Criterion microbenches
  - dense-match throughput stays usable when matches are everywhere
  - bounded search must not pay for work outside its bounds
- **Encoding stabilization.**
  - lift the current "non-UTF8 must be rope" limitation where it is safe to do so
  - widen auto-detect beyond BOM-backed UTF-16 where the result is unambiguous
  - finish the `LossyDecodedPreserve` story so frontends never have to guess
    whether preserve-save is allowed
  - tighten `decoding_had_errors` vs `LossyDecodedPreserve` separation in the
    public docs
- **Stability work that lands with the above.**
  - regex semantics tests (Unicode-aware behavior, anchors, line semantics
    across CRLF, bounded vs unbounded)
  - encoding regression tests (legacy reinterpret, save round-trips, lossy
    preserve, conversion preflight)
  - additional "truth after error" tests for the new search and encoding
    surfaces

### Non-Goals

- LSP, syntax highlighting, grapheme clustering, display-cell width — these
  remain frontend concerns.
- New session/job mechanics beyond what the search/encoding work needs.

### Exit Criteria

`0.8.0` is ready when:

- regex and literal search share one typed surface and one set of guarantees
- search performance numbers on real huge files are recorded next to the
  existing perf baseline
- the encoding contract is widened with new tests instead of new prose
- `cargo test --all-features --workspace` is green on all CI targets

## 0.9.0

`0.9.0` is the stability release. It is the second half of the
search/encoding/stability block, focused on locking down the contract rather
than adding new features.

### Focus

- **Backing transitions are explicit.** `mmap → piece_table → rope`
  promotions surface through typed status/event so frontends can react
  instead of guessing.
- **Predictable rejection on huge files.** `edit_capability_*` is the single
  preflight surface; rejection reasons are typed and stable.
- **Sidecar recovery.** `.qem.lineidx` and `.qem.editlog` failure modes
  resolve through typed outcomes (`Rebuild`, `Discard`, `ReopenClean`)
  instead of best-effort heuristics. Version mismatch, corrupt sidecar, and
  stale identity are explicit.
- **Truth after error, completed.** Failed save, failed open, discarded stale
  worker, deferred close, `set_path` / `document_mut` while busy — every one
  of these has at least one regression test that asserts the post-error
  session state.
- **Regression benches in CI.** Open, first viewport, first edit on huge
  files, edited save, literal search, and regex search each get a short
  regression bench tied to the perf baseline.
- **Escape hatch cleanup.** Final pass on `document_mut`, `set_path`, and
  unconditional `compact_piece_table` so the rules around them are
  documented and enforced by tests.

### Non-Goals

- New API surface beyond what is needed to make existing behavior
  predictable.
- Domain expansion beyond text.

### Exit Criteria

`0.9.0` is ready when:

- backing transitions, sidecar recovery, and rejection paths are typed and
  observable
- the "truth after error" matrix is fully covered by tests
- regression benches catch performance drops in CI on at least one platform
- `cargo test --all-features --workspace` is green on all CI targets

## 1.0.0

`1.0.0` is the API freeze.

### Focus

- freeze the public contract
- separate stable API from internal implementation details
- publish a migration guide
- define semver discipline going forward
- verify that examples, docs, tests, and benches all tell the same story

### Must Be Stable

- typed API surface
- open/save lifecycle
- background job semantics
- huge-file support contract
- error model
- line and column semantics
- edit capability semantics
- search surface (literal and regex)
- encoding contract

### May Stay Unstable

- sidecar on-disk format
- low-level cache/layout choices
- internal storage details

### Exit Criteria

`1.0.0` is ready when:

- the public contract is frozen and labeled as stable in rustdoc
- `MIGRATION-1.0.md` lists every breaking, non-breaking, and deprecated change
  since `0.7.0`
- semver rules for the post-`1.0` line are written down
- downstream applications can build products on top of Qem without expecting
  recurring redesign of the basic contract

## Short Version

- `0.7.0`: integration release
- `0.8.0`: regex + fast search + encoding stabilization
- `0.9.0`: stability hardening, "truth after error", regression benches
- `1.0.0`: public API freeze
