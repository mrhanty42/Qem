# Qem Roadmap

This document is the release plan, not a wish list. It states what has been
shipped, what the 1.0.0 freeze delivered, what moves along the post-1.0
line, and which limitations are knowingly accepted. Per-version change
details live in [`CHANGELOG.md`](CHANGELOG.md); performance methodology and
recorded numbers live in [`BENCHMARKS.md`](BENCHMARKS.md).

## Release status

| Release | State | Theme |
| --- | --- | --- |
| 0.8.0 | shipped | native per-encoding byte-movement engine, typed regex search, UTF-8 BOM auto-detect |
| 0.9.0 | shipped 2026-11-08 | ropey dependency replaced by the Qem-native edit buffer, rope-path undo/redo history, far-edit support, performance baselines |
| 1.0.0 | shipped 2026-11-19 | cell-exact line model for multi-byte encodings, hardening of the persistence and search paths, public API freeze |
| 1.0.1 | shipped 2026-11-19 | documentation corrections for the release page |
| 1.1.0 | shipped 2026-11-22 | additive typed reads on the frozen surface, CJK regex character-phase tracking, decoder-cell port corrections, internal module splits |
| 1.1.1 | shipped 2026-11-28 | performance fixes: append-only edit log, balanced page splits, checksummed line index, clone-on-write snapshots, CI regression bench |

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
  error" coverage (failed saves, failed opens, discarded stale workers,
  deferred closes, mutations through busy handles) is added incrementally
  as each surface stabilizes, not bundled into one later release.

## Completed work in the 1.0 track

The following landed on `master` after the 0.9.0 release (2026-11-08)
through 2026-11-19. Each item carries its own regression test, and every
fix was verified by temporarily reverting it and watching that test fail.

- **Encoding-aware literal search.** The search needle is encoded into the
  document's encoding before the byte search. UTF-16 needles are looked up
  on the 2-byte cell grid, and odd-offset collision candidates are
  rejected. Single-byte needles go through the encoding encoder. A needle
  that cannot be represented in the document's encoding answers a typed
  dead query instead of a silent miss. Reverse search validates candidates
  the same way. A randomized property suite drives eight encodings end to
  end.
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
  the failed-persistence resident window produce typed errors without
  corrupting the document; a half-landed replace batch reports the document
  as modified; a no-op backspace no longer marks a clean rope document
  dirty.
- **The cell-exact line model for UTF-16 piece tables.** The line model
  used to scan raw `0x0A` / `0x0D` bytes, which is byte-exact for UTF-8
  and the single-byte encodings but wrong for UTF-16, where a `0x0A` byte
  can be the low or high half of a non-line-ending code unit. Every walker
  now classifies whole encoding cells: line-start resolution; the
  line-break counter behind inserts, splits, trims, rebases, persistence
  snapshots, and compaction; the column-to-byte and byte-to-column mappers;
  the viewport line segment; the text-unit counter; the scanned line
  ranges; the line-length scanner; and the cursor position scanner. CJK
  trail bytes can never alias a break byte (the Shift_JIS, GB18030, and
  EUC-KR trail ranges exclude `0x0A` / `0x0D`), so the raw scans are exact
  there by construction.
- **Phantom-origin snapping in search.** The position-to-byte mapping
  saturates at the end of a line for columns past the line's width; a
  phantom origin could echo itself through the anchored position mapping
  and stall the search iterators. Forward and reverse search now snap
  their origins onto the true position of the saturated byte, and the
  zero-width nudge walks the byte model instead of trusting drifted line
  metadata.
- **Bounded search windows.** A literal-search window used to hold the
  entire remaining bytes of the piece segment it entered, which on edited
  giant documents copied tens of gibibytes onto the heap per search step (a
  measured 27-minute step and a 43.3 GB commit peak on a 16 GB machine
  against a 50 GiB fixture). Windows now enter a segment in capped slices
  with the same overlap hand-off; the measured find-all on the 50 GiB
  edited fixture dropped to pure disk-read time with a 0.24 GB peak.
- **The disk line index counts UTF-16 lines on whole cells** (sidecar
  format version 3). The index builder previously registered a phantom
  line for every aliased break byte, shifting all later line numbers and
  anchoring checkpoints at mid-cell offsets. The builder validates every
  hit as a whole cell, and the sidecar header records which cell width the
  line numbers were counted in: a sidecar built for another width, or by
  an older build, is stale and rebuilds.
- **The trailing line break is classified as a whole cell**, so the
  reverse-search window on a document ending in a UTF-16 line break no
  longer includes the break.
- **`qem::index` is no longer part of the public API.** Every item inside
  was already crate-private; closing the empty re-export keeps the disk
  line index internal, so the freeze does not have to promise compatibility
  for it.
- **Verification on real hardware.** The full fix stack was re-verified on
  a 50 GiB edited fixture: session recovery with the rebuilt index, then a
  rare-needle find-all answered in pure disk-read time with the correct
  single match.

## Post-1.0 work

- **Bounded session history.** The piece-tree edit history and its sidecar
  rewrite are unbounded: every flush re-serializes all history entries and
  appends fresh history pages, so long editing sessions on huge files pay
  quadratic sidecar writes and monotonically growing flush latency until
  the next save and reopen. A cap or sliding window changes the
  user-visible undo depth, so it needs an explicit product decision before
  implementation.
- **Share the edit buffer through `Arc` in snapshot `save_async`** instead
  of cloning it on the caller thread (the snapshot backing itself now
  clones copy-on-write on supported filesystems).
- **Backing transitions surface as events.** The `mmap → piece_table`
  promotion is observable today only through typed status polling;
  surfacing transitions as typed events lets frontends react instead of
  guessing.


## Completed after the 1.0 release

- **The frozen read surface stays infallible; unreadable pages surface
  through additive typed methods.** `read_text`, `read_selection`,
  `read_viewport`, and `text_lossy` keep their 1.0.0 signatures and their
  best-effort degradation, documented on each method. The additive
  `read_text_result`, `read_selection_result`, `read_viewport_result`,
  `text_lossy_result`, and `line_slices_result` return the typed
  `DocumentReadError` when a session sidecar page no longer passes its
  checksum. Save and cut fail typed (`DocumentError::Write`) through the
  same contour; compaction, detach, and the discarded-save history rebase
  refuse their pass and keep the live piece tree untouched; the
  piece-table-to-rope promotion fails typed instead of opening a silently
  emptied rope.
- **The UTF-16 text-unit advance collapses stored CRLF on whole cells.**
  The pending context carries the entire CR cell across window edges and
  at EOF, so a CRLF pair spanning two UTF-16 cells counts as one unit
  behind inserts, splits, and caret movement.
- **The Shift_JIS / EUC-KR / gb18030 decoder-cell port matches
  encoding_rs on rejected sequences.** Valid-shape trails below 0x80
  (Shift_JIS unmapped pointers, EUC-KR letter trails outside the hangul
  extension) report Malformed(1) with the trail reprocessed, so the lead
  stays a one-byte cell; a gb18030 truncated four-byte head decomposes
  exactly like the decoder, including the pending three-byte end flush;
  and the euro sign maps through gb18030's `0x80`. The Shift_JIS
  low-trail mapped table covers the full lead range (the 3780-entry table
  panicked on leads above 0x9F and is resized to 11280 entries). The port
  was verified by a differential sweep against encoding_rs on all three
  encodings.
- **Regression benchmarks run in CI.** A short `regression` benchmark
  (open + first viewport, first edit + save, literal and regex search on a
  20k-line fixture with tightened criterion settings) runs on every CI
  push; the numbers land in the job log and a criterion-results artifact,
  ready to be compared against the recorded baselines in
  `benches/baselines/`. Threshold gating stays out of CI on purpose:
  shared-runner timing noise would flake.
- **The edit-log sidecar is an append-only log (format v4).** The v3
  sidecar rewrote the whole history vector and the whole add buffer on
  every flush and kept every dead page, so a session of 10,000 small edits
  inflated the sidecar to 6.6 GB and refused further edits once the disk
  filled. Each history entry now carries its parent link, history-vector
  position, and epoch; recovery walks the parents and rebuilds the redo
  tail from the committed epoch; two small index pages are rewritten on
  each commit while data pages never are. Legacy v3 sidecars recover and
  convert to the v4 layout at reopen with the recovered state committed
  immediately. A degenerate piece-tree page split (a full page plus a
  1-item tail on every overflow) grew a "ladder" tree whose page path
  every edit rewrote; page splits now divide evenly, keeping the tree
  shallow. Measured on a 2 MiB fixture with 20,000 inserts: 5.9 s total
  with a 203 MB sidecar (previously the run refused further edits at
  ~13,900 edits with a 6.6 GB sidecar).
- **`.qem.lineidx` pages carry a CRC32 (sidecar format version 4).** A
  flipped byte inside a checkpoint value now fails typed and the stale
  sidecar rebuilds instead of parsing into silently wrong line starts.
  Sidecars written by older builds are rejected as stale and rebuilt
  automatically.
- **Snapshot backing clones copy-on-write on macOS and Linux.** APFS
  (fclonefileat) and reflink-capable Linux filesystems (FICLONE) hand
  back an instant snapshot; filesystems without cloning fall back to the
  full-copy path unchanged, and a 50 GB file opens in seconds instead of
  minutes on supported filesystems.
- **Recoverable paths carry no `unwrap`/`expect`.** A full sweep of the
  non-test code found no recoverable `unwrap`/`expect` calls: the remaining
  ones are either test fixtures or static invariants whose inputs are
  compile-time constants or checked earlier in the same function ("this
  encoding is a known label", "piece-table presence checked above"), each
  with a message describing the invariant. New code inherits the typed
  error propagation conventions of the frozen surface.
- **The overgrown modules are split into focused units.** `src/document.rs`
  (piece_table, scan, fixtures, pieces) and `src/piece_tree.rs` (store,
  btree, session, history) are now module directories. The public surface
  is unchanged; the stability labels in the rustdoc keep marking the
  frozen API, and the semver rules for the 1.x line are written down in
  the stability contract below.
- **Splice absorption in CJK regex windows is fixed with absolute
  character-phase tracking.** The Class B regex scan used to re-align a
  window that opened mid-sequence by probing the decode: as long as the
  window head did not decode with a replacement character first, the head
  was accepted even when it sat on a trail byte, and a trail byte that
  spliced into a valid pair with the next byte (the Shift_JIS, GB18030,
  and EUC-KR trail ranges overlap the lead ranges) shifted the whole
  window's decode off the document's character phase, so needle
  occurrences absorbed into the spliced pairs were never reported. On
  GB18030 the window edges could sit mid-cell on the decoder's grid
  outright, because the leading-byte detector answers four bytes for
  every lead + digit pair while the decoder decomposes a rejected
  four-byte sequence into a one-byte replacement plus reprocessed bytes,
  and the re-pairing persists through a whole run of two-byte characters.
  The scan now walks its window edges with the encoding_rs decoder's own
  per-cell consumption instead of the plain detector, the probe is gone,
  and the decoded-to-source mapping walks the decoded characters and the
  source window in lockstep, so corrupt cells in front of a match no
  longer drift its offsets. Deterministic Shift_JIS and GB18030
  absorption fixtures pin the behavior, and each fix component was
  verified by re-introducing it and watching its test fail.

## Known limitations

These are documented trade-offs, not bugs. Each is visible in the public
documentation.

- Reverse regex over large spans returns the **rightmost** match by start
  offset, which for self-overlapping patterns (e.g. `aa`) may overlap the
  previous match, while the small-span forward fallback returns the last
  non-overlapping match. The two agree for patterns that cannot
  self-overlap; the inconsistency is pinned by an ignored test in
  `regex_tests.rs`.

- Search mapping stays approximate for **genuinely corrupt bytes**: a
  U+FFFD decoded from ill-formed data mid-window re-encodes to a numeric
  character reference in Shift_JIS / EUC-KR, drifting positions computed
  after it. The regex scan's candidate-to-source mapping walks around
  replacements exactly now (see the splice-absorption fix above); the
  residual approximation lives in the piece-table position mapping, the
  amortized window-cache cursor, and the overlap threshold, which still
  re-encode replacement characters.
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

### Semver policy for the post-1.0 line

The 1.x line is additive: new types, methods, fields, and feature flags,
plus fixes, ship in minor releases (1.1.0, 1.2.0, …). A change that alters
a frozen signature, removes or renames a public item, changes documented
behavior, or breaks compatibility for a released crate requires the next
major release (2.0.0). Patch releases (1.x.y with no new API) carry fixes
and documentation corrections only.

## 1.0.0 freeze criteria — met at release (2026-11-19)

- The remaining items were either implemented or explicitly moved to the
  post-1.0 line by decision.
- The focused refactors left the public surface unchanged while the
  internals became smaller and easier to reason about.
- Non-critical `unsafe` blocks are gone, and each remaining site documents
  why it exists.
- `unwrap` and `expect` no longer appear on hot or recoverable paths.
- The public contract is frozen and labeled as stable in rustdoc.
- The semver rules for the post-1.0 line are written down.
- Downstream applications can build products on top of Qem without
  expecting recurring redesign of the basic contract.

## After 1.0

The post-1.0 line starts with the items above plus the decisions parked
during the 1.0 cycle: the session history cap (it changes the user-visible
undo depth) and the crates.io publication policy for patch releases.
