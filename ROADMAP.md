# Qem Roadmap

This document is the release plan, not a wish list. It states what has been
shipped, what is being built now, what remains before the `1.0.0` API
freeze, and which limitations are knowingly accepted. Per-version change
details live in [`CHANGELOG.md`](CHANGELOG.md); performance methodology and
recorded numbers live in [`BENCHMARKS.md`](BENCHMARKS.md).

## Release status

| Release | State | Theme |
| --- | --- | --- |
| 0.8.0 | shipped | native per-encoding byte-movement engine, typed regex search, UTF-8 BOM auto-detect |
| 0.9.0 | shipped 2026-11-08 | ropey dependency replaced by the Qem-native edit buffer, rope-path undo/redo history, far-edit support, performance baselines |
| 1.0.0 | in progress | cell-exact line model for multi-byte encodings, hardening of the persistence and search paths, public API freeze |

## Planning rules

- Integration beats expansion. Examples and real frontend workflows decide
  which API changes are justified.
- GUI dependencies stay out of the `qem` core. Frontend demos live in a
  separate workspace member when they need `egui` or another UI stack.
- Prefer a typed public surface over private-detail escape hatches and glue
  code.
- Performance is a measured contract on real files, not only synthetic
  microbenchmarks.
- Sidecars and internal storage layout are implementation details unless
  they are explicitly promoted into the public contract.
- Every release ships with the regression tests it needs. "Truth after
  error" coverage — failed saves, failed opens, discarded stale workers,
  deferred closes, mutations through busy handles — is added incrementally
  as each surface stabilizes, not bundled into one later release.

## Completed work in the 1.0 track

The following landed on `master` after the 0.9.0 release (2026-11-08)
through 2026-11-19. Each item carries its own regression test, and every
fix was verified by temporarily reverting it and watching that test fail.

- **Encoding-aware literal search.** The search needle is encoded into the
  document's encoding before the byte search: UTF-16 needles are looked up
  on the 2-byte cell grid with odd-offset collision candidates rejected,
  single-byte needles go through the encoding encoder, needles that cannot
  be represented in the document's encoding answer a typed dead query
  instead of a silent miss. Reverse search validates candidates the same
  way. A randomized property suite drives eight encodings end to end.
- **Character-boundary proof for CJK candidates.** Shift_JIS, GB18030, and
  EUC-KR byte grids are not self-synchronizing, so a plain byte search can
  splice the halves of neighboring characters into a false match. Literal
  and regex candidates are walked from the nearest known character
  boundary; non-boundary hits are rejected.
- **Open-time readability verification.** A session sidecar whose piece
  pages fail their CRC is rejected as a typed error, and recovery drops the
  sidecar and rebuilds from the source file instead of opening a document
  whose content reads could silently shorten.
- **Coherence around refused persistence.** Inserts and deletes refused by
  the failed-persistence resident window surface typed errors without
  corrupting the document; a half-landed replace batch reports the document
  as modified; a no-op backspace no longer marks a clean rope document
  dirty.
- **The cell-exact line model for UTF-16 piece tables.** The line model
  used to scan raw `0x0A` / `0x0D` bytes, which is byte-exact for UTF-8
  and the single-byte encodings but wrong for UTF-16, where a `0x0A` byte
  can be the low or high half of a non-line-ending code unit. Every walker
  now classifies whole encoding cells: line-start resolution, the
  line-break counter behind inserts, splits, trims, rebases, persistence
  snapshots, and compaction, the column-to-byte and byte-to-column mappers,
  the viewport line segment, the text-unit counter, the scanned line
  ranges, the line-length scanner, and the cursor position scanner. CJK
  trail bytes can never alias a break byte (the Shift_JIS, GB18030, and
  EUC-KR trail ranges exclude `0x0A` / `0x0D`), so the raw scans are exact
  there by construction.
- **Phantom-origin snapping in search.** The position-to-byte mapping
  saturates at the end of a line for columns past the line's width, which
  let a phantom origin echo itself through the anchored position mapping
  and stall the search iterators. Forward and reverse search now snap
  their origins onto the true position of the saturated byte, and the
  zero-width nudge walks the byte model instead of trusting drifted line
  metadata.
- **Bounded search windows.** A literal-search window used to hold the
  entire remaining bytes of the piece segment it entered, which on edited
  giant documents copied tens of gibibytes into heap per search step (a
  measured 27-minute step and a 43.3 GB commit peak on a 16 GB machine
  against a 50 GiB fixture). Windows now enter a segment in capped slices
  with the same overlap hand-off; the measured find-all on the 50 GiB
  edited fixture dropped to pure disk-read time with a 0.24 GB peak.
- **The disk line index counts UTF-16 lines on whole cells** (sidecar
  format version 3). The index builder previously registered a phantom
  line for every aliased break byte, shifting all later line numbers and
  anchoring checkpoints at mid-cell offsets. The builder validates every
  hit as a whole cell, and the sidecar header records which cell width the
  line numbers were counted in: a sidecar built for another width — or by
  an older build — is stale and rebuilds.
- **The trailing line break is classified as a whole cell**, so the
  reverse-search window on a document ending in a UTF-16 line break no
  longer includes the break.
- **`qem::index` is no longer part of the public API.** Every item inside
  was already crate-private; closing the empty re-export keeps the disk
  line index an internal detail the freeze does not have to promise
  compatibility for.
- **Verification on real hardware.** The full fix stack was re-verified on
  a 50 GiB edited fixture: session recovery with the rebuilt index, then a
  rare-needle find-all answered in pure disk-read time with the correct
  single match.

## Remaining before the 1.0.0 freeze

- **Bounded session history.** The piece-tree edit history and its sidecar
  rewrite are unbounded: every flush re-serializes all history entries and
  appends fresh history pages, so long editing sessions on huge files pay
  quadratic sidecar writes and monotonically growing flush latency until
  the next save and reopen. A cap or sliding window changes the
  user-visible undo depth, so it needs an explicit product decision before
  implementation.
- **Typed reads on unreadable pages.** Structural edits already refuse to
  reference a page that the resident window dropped, but the read walkers
  still silently skip a lazily read page that fails its CRC, shortening
  reads behind the documented length instead of surfacing a typed error.
  The fix converts the read-walker family to a `Result` contour; it is
  deferred so the hot read path is not churned unreviewed.
- **Checksums for `.qem.lineidx` pages.** Index pages validate their
  structural invariants but carry no CRC, so a flipped byte inside a
  checkpoint value parses cleanly and yields silently wrong line starts.
  The fix belongs with the next sidecar format bump.
- **Engine-aware bounded-read context.** The text-unit advance walk reads
  the document in bounded windows and carries a single CRLF-context byte
  across window edges; for UTF-16 the CRLF pair spans two cells, so the
  context must widen to the encoding's maximum cell width.
- **Cheaper snapshot backing off Windows.** Non-Windows snapshot backing
  still copies the whole source file at open; a cheaper backing needs
  design work. Snapshot `save_async` should also share the edit buffer
  through `Arc` instead of cloning it on the caller thread.
- **Focused refactors, not a rewrite.** Split the overgrown modules into
  focused units (history, persistence, fragmentation, and the core
  piece-tree structure; named lifecycle helpers around inspection,
  indexing, sidecar recovery, and final state construction), and reduce the
  duplication between `DocumentSession` and `EditorTab` through composition
  without breaking the public surface.
- **Drop non-critical `unsafe` blocks** where a safe equivalent exists at
  the same performance tier, and document in place why each remaining
  `unsafe` site is load-bearing.
- **Remove `unwrap` and `expect` from recoverable paths**, replacing the
  remaining default-value patterns with typed error propagation.
- **Backing transitions surface as events.** The `mmap → piece_table`
  promotion is observable today only through typed status polling;
  surfacing transitions as typed events lets frontends react instead of
  guessing.
- **Regression benchmarks in CI.** Open, first viewport, first edit on huge
  files, edited save, literal search, and regex search each get a short
  regression bench tied to the recorded baselines.
- **Public API freeze.** Label the stable surface against internal
  implementation details in rustdoc, and write down the semver policy for
  the post-1.0 line.

## Known limitations accepted for 1.0

These are deliberate, documented trade-offs rather than bugs. Each is
visible in the public documentation.

- Search mapping stays approximate for **genuinely corrupt bytes**: a
  U+FFFD decoded from ill-formed data mid-window re-encodes to a numeric
  character reference in Shift_JIS / EUC-KR, drifting the mapping for
  matches after it inside the same window.
- The leading-byte window probe is one-sided for CJK: a window opening on
  a trail byte that happens to splice into a valid pair with the next byte
  decodes clean and is accepted mid-sequence, so occurrences absorbed into
  spliced pairs are missed. A full fix needs absolute character-phase
  tracking instead of probing; it is planned for the post-1.0 line.
- Pre-edit viewport estimates on the mmap path stay approximate until the
  exact line mapping reaches the requested line; this is the documented
  estimate contract.
- UTF-16 and CJK edit sessions are not recoverable through the
  byte-indexed session path yet; such sidecars fall back to a fresh open,
  and the session history cap (see above) is pending the same product
  decision.

## Stability contract

Must be stable at 1.0.0:

- the typed API surface;
- the open and save lifecycle;
- background job semantics;
- the huge-file support contract;
- the error model;
- line and column semantics;
- edit capability semantics;
- the search surface, literal and regex;
- the encoding contract.

May stay unstable:

- the sidecar on-disk formats;
- low-level cache and layout choices;
- internal storage details.

## Exit criteria for 1.0.0

- The remaining items above are either implemented or explicitly moved to
  the post-1.0 line by decision.
- The focused refactors leave the public surface unchanged while the
  internals become smaller and easier to reason about.
- Non-critical `unsafe` blocks are gone, and each remaining site documents
  why it exists.
- `unwrap` and `expect` no longer appear on hot or recoverable paths.
- The public contract is frozen and labeled as stable in rustdoc.
- The semver rules for the post-1.0 line are written down.
- Downstream applications can build products on top of Qem without
  expecting recurring redesign of the basic contract.
