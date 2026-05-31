# Qem Roadmap

This roadmap is a release plan, not a wish list.

The path is: ship a fast and complete search/encoding contract, then replace
the rope dependency with our own structure, then freeze the public API at
`1.0.0` after a small round of clean-up.

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

Current target: `0.8.0`

## 0.8.0

`0.8.0` is the search and encoding release. Encoding handling is **native**
instead of a transcode-to-UTF-8 pipeline: every encoding moves through its
own byte movement engine.

### Focus

#### Regex search

- Typed `RegexSearchQuery` / `RegexCompileError` / `RegexSearchIter` mirroring
  the literal-search surface.
- `find_next_regex`, `find_prev_regex`, `find_all_regex` plus reusable
  `_query`, bounded `_in_range`, and `_between` variants.
- `DocumentSession` and `EditorTab` expose the same regex surface.
- Mmap zero-copy regex path (no 8 MiB cap, finds matches at 64 MiB+
  offsets), piece-table chunked streaming with 1 MiB overlap, rope chunked
  streaming via `Rope::chunks()` over the byte engine — no `String`
  materialization on the rope hot path.
- Reverse regex via reverse-DFA built from `regex_automata::dfa::dense`,
  routed across all three backings (rope, mmap slice, piece-tree chunked
  walker) and bounded by a typed size-limit error rather than panic on
  pathological patterns.
- Dense-vs-sparse ratio is gated by a deterministic perf test.

#### UTF-8 BOM auto-detect

- `auto_detect_open_encoding` recognizes the UTF-8 BOM. Files opened with
  the auto-detect policy and a leading `EF BB BF` go through the rope
  decode path (which strips the BOM via `decode_with_bom_removal`).
- Default `Document::open()` does not strip BOM; only the explicit
  auto-detect path does.

#### Encoding-aware byte movement engine

The "transcode every non-UTF-8 file into a UTF-8 rope" approach is treated
as a workaround, not a target. `0.8.0` introduces a dedicated byte-movement
engine per encoding so every encoding can run natively over its own bytes:

- `Utf8Engine` for UTF-8 (the default).
- `SingleByteEngine` for ASCII supersets: `windows-1250`..`-1258`,
  `windows-874`, `ISO-8859-2`..`-16`, `KOI8-R`, `KOI8-U`, `IBM866`,
  `macintosh`, `x-mac-cyrillic`. Step is always one byte; line scanning
  reuses memchr; CRLF collapses identically to UTF-8.
- `Utf16Engine<Endian>` for UTF-16 LE / BE. Newline finder looks for
  2-byte aligned `0x0A 0x00` / `0x0D 0x00`. Char step is 2 bytes (4 for
  surrogate pairs). Edit boundaries align on 2 bytes.
- `MultiByteEngine` for `Shift_JIS`, `GB18030`, `EUC-KR`. Each kind has
  its own leading-byte detector and a false-positive-aware newline finder
  for `0x0A` / `0x0D` (some trailing bytes can be these values).

Engines are cached per encoding via `OnceLock<Mutex<HashMap>>`, so the
dispatch cost is one pointer compare on the hot path.

Non-UTF-8 documents never build a UTF-8 rope. Open dispatches to the
mmap fast path with the appropriate engine. Viewport reads decode only
the requested window via `encoding_rs`. Edit goes through the piece tree
directly: incoming `&str` is encoded to the target encoding via
`encoding_rs`; non-representable code points return a typed error
without mutating state.

#### Encoding-aware alignment

`Document::align_byte_offset` lands every byte offset on a character
boundary of the document's current encoding before it reaches the edit /
regex paths:

- UTF-8 walks back to the nearest UTF-8 char boundary.
- Class A is a no-op clamp.
- UTF-16 rounds to a 2-byte cell.
- Class B (CJK multibyte) walks forward from the nearest line anchor via
  the engine's `step` and picks the closest reachable boundary.

This is the surface that fixes the historic piece-boundary truncation
bug where reverse search across an internal piece break could land on
the trail byte of a multi-byte character and corrupt downstream byte
offsets.

#### Search performance

- Literal and regex search are the fastest path on mmap, piece-table, and
  rope backings.
- Real numbers on 1 GiB / 10 GiB / 50 GiB files, recorded next to the
  existing perf baseline.

### Non-Goals

- Removing ropey. That is `0.9.0`.
- Refactoring `piece_tree`, splitting history/persistence, or eliminating
  every `unwrap` / `expect`. That is `1.0.0`.
- LSP, syntax highlighting, grapheme clustering, display-cell width.

### Exit Criteria

`0.8.0` is ready when:

- The encoding-engine work is shipped end to end (open, viewport, search,
  edit, save) for UTF-8, every Class A encoding, UTF-16 LE / BE, and the
  three Class B CJK encodings.
- Regex and literal search share one typed surface and one set of
  guarantees on every backing.
- Reverse regex is reverse-DFA based and bounded by a typed size limit.
- Search performance numbers on real huge files are recorded next to the
  existing perf baseline.
- The encoding contract is widened with new tests instead of new prose.
- `cargo test --all-features --workspace` is green on all CI targets.

## 0.9.0

`0.9.0` is the rope replacement release.

### Focus

- Replace the `ropey` dependency with a Qem-native edited-buffer structure.
  The new structure must:
  - support large-file editing with predictable memory.
  - keep `O(log N)` line / column lookup.
  - integrate cleanly with the existing piece tree so a single backing can
    own both unedited mmap-anchored pieces and edited add-buffer pieces.
- Keep the public contract behavior-stable. The replacement is internal:
  open / save / edit / viewport behavior must not change for users of the
  library.
- Add property-based tests comparing the new structure against a reference
  implementation across randomized edit sequences.

### Non-Goals

- New API surface that was deferred to the rope-replacement work
  specifically for that work.
- Domain expansion beyond text.

### Exit Criteria

`0.9.0` is ready when:

- `ropey` is removed from `Cargo.toml`.
- The new edited-buffer structure passes property-based parity tests
  against a reference oracle.
- All existing scenario tests stay green without behavior change.
- Real-file perf numbers on edited buffers stay within the established
  baseline (no regression).

## 1.0.0

`1.0.0` is the small-cleanup + freeze release.

### Focus

- **Small refactors for clarity**, not a rewrite. Split overgrown modules
  into focused units (history, persistence, fragmentation, core data
  structure for `piece_tree`; named lifecycle helpers around inspection,
  indexing, sidecar recovery, and final state construction). Reduce
  duplication between `DocumentSession` and `EditorTab` through
  composition without breaking the public surface.
- **Drop non-critical `unsafe` blocks** where a safe equivalent exists at
  the same performance tier. Keep `unsafe` only on hot paths where it is
  load-bearing for the documented performance contract, and document why
  in-place at each remaining site.
- **Reduce `unwrap` / `expect` on recoverable paths**. Replace remaining
  patterns like `unwrap_or([0; 8])` with explicit `?`-driven error
  propagation backed by typed errors.
- **Sidecar recovery is typed.** `.qem.lineidx` and `.qem.editlog` failure
  modes resolve through typed outcomes (`Rebuild`, `Discard`,
  `ReopenClean`) instead of best-effort heuristics. Version mismatch,
  corrupt sidecar, and stale identity are explicit.
- **Backing transitions surface as events.** `mmap → piece_table → rope-
  replacement` promotions are observable through typed status so frontends
  can react instead of guessing.
- **Regression benches in CI.** Open, first viewport, first edit on huge
  files, edited save, literal search, and regex search each get a short
  regression bench tied to the perf baseline.
- **Public API freeze.** Stable surface vs internal implementation details
  is labeled in rustdoc; semver discipline is documented.

### Must Be Stable

- typed API surface
- open / save lifecycle
- background job semantics
- huge-file support contract
- error model
- line and column semantics
- edit capability semantics
- search surface (literal and regex)
- encoding contract

### May Stay Unstable

- sidecar on-disk format
- low-level cache / layout choices
- internal storage details

### Exit Criteria

`1.0.0` is ready when:

- the small refactors leave the public surface unchanged but the
  internals are smaller and easier to reason about.
- non-critical `unsafe` blocks are gone; remaining `unsafe` is documented
  in-place.
- `unwrap` / `expect` no longer appears on hot or recoverable paths.
- the public contract is frozen and labeled as stable in rustdoc.
- semver rules for the post-`1.0` line are written down.
- downstream applications can build products on top of Qem without
  expecting recurring redesign of the basic contract.

## Short Version

- `0.8.0` (current): regex + fast search + encoding-aware native byte
  engine + UTF-8 BOM
- `0.9.0`: replace `ropey` with a Qem-native edited-buffer structure
- `1.0.0`: small cleanup (module splits, drop non-critical `unsafe`,
  remove `unwrap` / `expect` on recoverable paths, sidecar typed
  outcomes, backing transition events, regression benches in CI) +
  public API freeze
