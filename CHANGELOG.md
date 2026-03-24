# Changelog

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
