# Changelog

## 0.6.3

### Fixed

- Ran `rustfmt` on the recent huge-file line-count and piece-tree changes so
  `cargo fmt --all --check` passes again in CI.
- Scoped sparse-index helper code correctly for non-Windows builds so
  `cargo clippy --all-targets --all-features -- -D warnings` passes on Linux
  and other non-Windows CI runners.

## 0.6.2

### Added

- Added explicit `is_line_count_pending()` / `wait_for_exact_line_count(...)`
  flows so callers can distinguish "still estimating" from "done" and
  intentionally block only when they truly need an exact total line count.
- Added sparse-aware `.qem.lineidx` build behavior plus honest `perf_probe`
  reporting for pending exact line counts and exact-count wait time.

### Fixed

- Removed several hidden huge-file `O(file_size)` edit/read paths that caused
  the first prefix edit or first exact viewport after edit to degrade from
  interactive latency into tens of seconds or minutes on multi-gigabyte files.
- Preserved original line-break boundaries in the piece tree so exact line
  navigation after giant-line edits no longer has to rescan gigabytes to find
  the next line start.
- Kept exact total line counts available after partial large-file
  piece-table promotion whenever the sidecar line index already knows the full
  source-line total.

## 0.6.1

### Changed

- Tightened the session/save contract around `set_path()` and explicit clean
  saves so sync/async save paths no longer silently no-op when callers have
  changed the destination path or when the live file has diverged from the
  current clean backing.
- Expanded CI coverage into a clearer cross-platform matrix for Windows, Linux,
  and macOS, plus a dedicated Linux stress job for the heaviest
  recovery/save/history property tests.

### Fixed

- Hardened same-path async save discard recovery so piece-table sessions keep
  their dirty state, immediate recoverability, and undo/redo history even when
  older history roots reference pre-save `Add` text that no longer exists in
  the current saved snapshot.
- Fixed multiple quiet lifecycle mismatches in the editor/session layer,
  including deferred clean-mark handling after open, truthful sync/async
  save-in-progress guards, and same-path clean-save semantics.
- Eliminated a wide set of partial `piece_table` and incomplete `mmap`
  exactness/position/search/read bugs that could previously return stale text,
  phantom rows, incorrect boundary matches, or misleading `is_exact` flags.
- Added deeper regression coverage for discarded background jobs, recovery
  survival after reopen failures, clean-save path overrides, long-line
  edge cases, and partial/inexact read/search invariants.

## 0.6.0

### Added

- Added typed encoding provenance through `DocumentEncodingOrigin`, including
  `auto-detected`, `auto-detect-fallback`, explicit reinterpretation, and
  save-conversion paths on `Document`, `DocumentSession`, `EditorTab`, and
  their status snapshots.
- Added preserve-save and explicit-conversion preflight helpers through
  `preserve_save_error()`, `can_preserve_save()`,
  `save_error_for_options()`, and `save_error_for_encoding()` across the
  document/session/tab surfaces.
- Added recovery metadata for UTF-8 session sidecars so reopened `.qem.editlog`
  sessions preserve encoding origin and lossy-decode hints instead of silently
  collapsing back to the default fast-path origin.

### Changed

- Tightened the open/save encoding contract around `detect vs explicit
  override`, `reinterpret vs convert`, and explicit `SaveConversion`
  semantics, including same-path async conversion flows that must not degrade
  into no-op preserve saves.
- `decoding_had_errors()` is now reported more truthfully for fully inspected
  small UTF-8 opens, while preserve-save stays allowed when Qem can still write
  raw source bytes without materializing lossy-decoded text.
- Public docs now describe the sharper distinction between `decoding_had_errors`
  and `LossyDecodedPreserve`, so frontends can treat "source was malformed" and
  "this preserve-save would lose data" as separate states.

### Fixed

- Preserve-save now returns structured typed errors for lossy-decoded legacy
  opens, unsupported preserve targets, unrepresentable edited text, and other
  save-contract failures instead of relying on implicit or silent behavior.
- Explicit `convert to UTF-8` now performs a real conversion even when the
  current document contract already reports UTF-8, preventing raw-byte copies
  from leaking through invalid UTF-8 sanitize flows.
- UTF-8 materialization paths now mark lossy decode correctly when invalid bytes
  are pulled into rope-backed editing, preventing later preserve-save from
  silently committing replacement-character text back to disk.
- Expanded regression coverage across `windows-1251`, `Shift_JIS`, `GB18030`,
  `UTF-16LE`, `UTF-16BE`, piece-table recovery/save flows, and async
  session/tab save behavior.

## 0.5.4

### Changed

- Tightened the public README roadmap into a short release-plan summary instead
  of a larger planning dump.
- README installation examples now point at the current `0.5.4` release.

## 0.5.3

### Added

- Added a more public huge-file benchmark workflow around `examples/perf_probe.rs`, including head/middle/tail viewport anchors and giant-file metadata such as `file_len_bytes`, `indexed_bytes`, and exact-vs-estimated line-count reporting.
- Added helper scripts for collecting and summarizing repeatable JSONL probe matrices and for generating a sparse `1TB` structural stress fixture.
- Added benchmark documentation for public `1TB` methodology and explicit caveats around current giant-file editing and search limits.

### Changed

- README installation examples now point at the current `0.5.3` release instead of the broader `0.5` range so the crates.io page matches the latest published patch.

### Fixed

- Clarified in the public docs that `Qem` is the project name rather than an expanded acronym, reducing accidental third-party mislabeling.

## 0.5.2

### Added

- Added explicit encoding support through `DocumentEncoding`, `Document::open_with_encoding(...)`, `Document::save_to_with_encoding(...)`, and matching `DocumentSession` / `EditorTab` wrappers.
- Added frontend-visible encoding metadata through `encoding()` and `decoding_had_errors()` on document/session/tab-facing status paths.
- Added `OpenEncodingPolicy` plus richer `DocumentOpenOptions` flows for BOM-backed auto-detect and explicit reinterpretation.
- Added convenience auto-detect wrappers for BOM-backed UTF-16 opens so callers do not always need to construct `DocumentOpenOptions` by hand.

### Changed

- Legacy-encoded opens now use an explicit rope-backed transcode path so the existing UTF-8 mmap fast path remains unchanged.
- Documentation now describes the first stable encoding contract more explicitly, including the current non-UTF8 limitations for very large files, BOM-backed auto-detect, and unsupported preserve-save targets.

### Fixed

- Save-path validation now returns typed encoding errors for unrepresentable output instead of silently producing lossy legacy-encoded writes.
- Added regression coverage for explicit legacy reinterpret/open flows, UTF-16 BOM auto-detect, UTF-8 conversion saves, async session open with encoding, and save failures that must preserve dirty state.

## 0.5.1

### Changed

- Refactored `reads.rs` so exact and estimated mmap read paths are separated more explicitly, with clearer internal helpers for mmap, piece-table, and rope-backed reads.
- Refactored edit-buffer promotion policy around explicit internal planning types so `mmap -> piece_table -> rope` transitions are easier to reason about and maintain.
- Reworked `SessionCore` async lifecycle bookkeeping around named internal states instead of scattered boolean flags, making stale-result handling and deferred-close flow easier to follow.
- Expanded rustdoc and README guidance around the recommended frontend entry path, advanced escape hatches, and lower-level `Document` usage.

### Fixed

- Restored exact empty-document line semantics in the mmap read path after the internal read-layer split.
- Preserved a proptest regression seed for the empty-edit edge case so future runs replay it before novel cases.

## 0.5.0

### Added

- Piece-table fragmentation metrics and compaction policy types.
- Explicit piece-table maintenance APIs including idle compaction and maintenance snapshots.
- `MaintenanceAction` and high-level maintenance helpers for `Document`, `DocumentSession`, and `EditorTab`.
- Literal-search iterators, compiled query iterator variants, and bounded `..._between(...)` search helpers.
- `perf_probe` support for capped `find_all` probing plus maintenance-action reporting.
- Benchmark tooling scripts for collecting JSONL probe matrices and summarizing them into markdown.

### Changed

- Search iteration now uses buffering and offset-hint paths more aggressively for practical `find_all(...)` workloads on `mmap` and `piece_table` backings.
- Search benchmarks now cover reusable query iterator paths in addition to one-shot literal queries.
- README and benchmark methodology docs now describe the current maintenance and probe workflow more explicitly.

## 0.4.0

### Added

- Typed literal search across all current backings through `find_next(...)` and `find_prev(...)`.
- Reusable compiled literal queries through `LiteralSearchQuery` plus `find_next_query(...)` / `find_prev_query(...)`.
- Bounded literal search through `find_next_in_range(...)` / `find_prev_in_range(...)` and the query-based bounded variants.
- Explicit async load phases and retained background issues for `DocumentSession` / `EditorTab`.
- Typed frontend/session visibility for deferred closes through `close_pending()`.
- Search benchmarks covering clean `mmap`, edited `piece_table`, and reusable query paths.
- New runnable `typed_editing` example for the typed document API.

### Changed

- `document` and `editor` were split into internal modules to reduce facade size and isolate lifecycle, persistence, reads, commands, and session logic.
- Sidecar validation now uses sampled content fingerprints in addition to file length and modification time.
- Corrupt `.qem.editlog` and `.qem.lineidx` sidecars now degrade or rebuild more predictably instead of being trusted too easily.
- Async session/job orchestration is stricter: stale background results are discarded after raw state changes, deferred close is truthful, and idle-only typed edit helpers are enforced while busy.
- Temp-file and atomic-rewrite handling is stricter, including absolute custom tmp-root validation and cleanup on write failure.
- Documentation, examples, CI matrix, and benchmark coverage were expanded to match the current public contract.

### Fixed

- Multiple failure paths around `save`, `save_async`, `open_file_async`, deferred close, and stale background worker results now preserve truthful session state.
- Several search and session edge cases that previously required frontend guesswork now have typed APIs and explicit tests.
