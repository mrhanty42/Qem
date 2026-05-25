# Qem

High-performance Rust text engine for massive files, with mmap-backed reads,
incremental line indexing, and responsive editing.

Qem is built for editor-style workloads where opening huge files must stay
responsive, scrolling should avoid full materialization, and saves should
stream back to disk instead of rebuilding the entire document in memory.

Qem is intentionally a backend-first text engine, not a GUI widget toolkit.
Applications are expected to own their cursor, scrollbar, selection, and
rendering model while Qem owns the hard part: huge-file access, viewport reads,
text mutation, save pipelines, and persistent edit sessions.

`Qem` is the project name, not an expanded acronym.

Small files take a fast inline path: Qem fully indexes them during `open()` so
line counts, viewport reads, and first edits are exact immediately without
paying background-thread overhead.

For large file-backed documents, Qem builds a sparse on-disk B-tree line index
with an LRU page cache. This keeps global line navigation bounded without
holding billions of line offsets in RAM.

When editing is enabled for large files, Qem can promote the document into a
piece tree and persist its state to a sibling `.qem.editlog` sidecar using
append-only copy-on-write commits and a bounded in-memory page cache.

The same sidecar also stores persistent edit sessions: the current piece-tree
state, add buffer, and undo/redo roots can be recovered automatically on the
next `Document::open`, as long as the source file identity still matches.

## Support Qem

If Qem is useful to you and you want to support the project:

- `USDT (0x address)`: `0x62c602d819dde8be07f1744b4e3b740ac0593982`
- `BTC`: `bc1qhgft34j89vpsyggyljvg7rkzc7lh2yyk5w45k5`

## Highlights

- Fast open and viewport reads for very large files
- Exact inline indexing for small files
- Background line indexing with a sparse on-disk line index
- Detected line-ending style exposed and preserved on save
- Lazy promotion to rope or piece-table editing buffers
- Persistent edit sessions with undo/redo recovery
- Async document-session open/save flows with progress polling
- Streaming atomic saves for large edited documents

## Installation

```toml
[dependencies]
qem = "0.7.0"
```

To disable the editor/session wrapper and use only the document/storage layer:

```toml
[dependencies]
qem = { version = "0.7.0", default-features = false }
```

## Cargo features

- `editor` (default): enables `DocumentSession`, `EditorTab`, `CursorPosition`, and async open/save helpers.
- `tmp-auto` (default): dynamic scratch-temp policy. On Windows Qem prefers the executable directory, then the edited file directory, then the system temp directory. On Unix-like systems Qem prefers the edited file directory, then the system temp directory, then the executable directory.
- `tmp-source-dir`: keeps snapshot/scratch temp files next to the edited file.
- `tmp-system-dir`: uses the OS temp directory for snapshot/scratch temp files.
- `tmp-exe-dir`: uses the executable directory for snapshot/scratch temp files.

Only scratch files such as `.qem.snap.*` follow this policy. The temp file used for atomic save replacement still stays next to the destination file so `save_to()` remains atomic.

Example:

```toml
[dependencies]
qem = { version = "0.7.0", default-features = false, features = ["editor", "tmp-exe-dir"] }
```

Runtime override is also available:

- `QEM_TMP_POLICY=auto|source-dir|system-dir|exe-dir`
- `QEM_TMP_DIR=/absolute/path/to/custom/tmp/root`

`QEM_TMP_DIR` is only honored when it points to an absolute, writable
directory. Invalid or unusable custom roots are ignored and Qem falls back to
the selected temp policy.

## Core components

- `FileStorage`: mmap-backed file access.
- `Document`: background line indexing, fast line metrics, viewport reads, rope or piece-table editing, and persistent edit-session recovery.
- `DocumentSession`: backend-first document session wrapper with async open/save helpers, progress polling, status snapshots, forwarded viewport/edit helpers, and no GUI cursor assumptions.
- `EditorTab`: lightweight document session wrapper with cursor, async open/save helpers, and progress polling.

## Positioning

- Qem is a text engine / editor backend for huge files.
- Qem is not a ready-made text widget.
- GUI code should keep ownership of visual cursor movement, custom scrollbars, line-number gutters, and selection painting.
- Qem should own viewport reads, typed range/selection reads, text positions/ranges, char-index conversions, mutation commands, save orchestration, and large-file storage behavior.
- Qem should also tell the frontend when an edit is safe, when it would require promotion, and when a huge-file safety limit makes it unavailable.

## Choosing an integration layer

- Use `Document` when your application already owns tabs, session state, background jobs, and save orchestration.
- Use `DocumentSession` when you want a backend-first session wrapper with generation tracking, async open/save helpers, progress polling, and no built-in cursor state.
- Use `EditorTab` when you want the same session helpers plus convenience cursor state.
- A GUI typically renders visible rows through `Document::read_viewport(...)` or `DocumentSession::read_viewport(...)`.
- Older compatibility helpers that silently swallow edit errors or expose raw tuple progress are still present for migration only, but they are deprecated and hidden from the main rustdoc surface in favor of the typed/session-first API.

## Recommended path for most applications

- Start with `DocumentSession` for a frontend/backend integration unless you already own your own tab lifecycle and background-job orchestration.
- Use `ViewportRequest`, `TextSelection`, `TextRange`, and `SearchMatch` as the main typed values that move between your UI state and Qem.
- Prefer bounded reads like `read_viewport(...)`, `read_text(...)`, and `read_selection(...)` over `text_lossy()` / `text()` in normal UI flows.
- Prefer typed session helpers such as `loading_state()`, `loading_phase()`, `save_state()`, `background_issue()`, `take_background_issue()`, `close_pending()`, and the `try_*` edit methods.
- Treat `document_mut()`, `set_path()`, unconditional `compact_piece_table()`, and full-document `text_lossy()` / `text()` as advanced surface for callers that intentionally manage those trade-offs themselves.
- Reach for raw `Document` only when your application is deliberately building its own session/job layer on top of the lower-level engine.

## Current Support Contract

### UTF-8 and ASCII

- UTF-8 / ASCII text is the primary stable fast path. Open, viewport reads, edits, undo/redo, and save are supported without transcoding.
- Huge-file reads use the mmap-oriented path when possible. Frontends should treat this as the main scalable contract for text viewing.
- Typed positions, ranges, selections, and viewport columns use document text units. For UTF-8 text, line-local columns count Unicode scalar values, not grapheme clusters and not display cells.
- Stored CRLF still counts as one text unit between lines for typed range/edit/navigation semantics.

### Invalid UTF-8 and Other Encodings

- Explicit legacy-encoding open/save is supported through `Document::open_with_encoding(...)`, `Document::save_to_with_encoding(...)`, and the matching `DocumentSession` / `EditorTab` wrappers.
- Auto-detect open currently recognizes BOM-backed UTF-16 files. Otherwise Qem stays on the normal UTF-8 / ASCII path unless the caller provides an explicit fallback through `DocumentOpenOptions`.
- Non-UTF8 opens currently materialize into a rope-backed document instead of using the mmap fast path. Very large legacy-encoded files may still be rejected until the wider encoding contract expands.
- `decoding_had_errors()` means the source required lossy decode replacement at open time. That does not automatically mean preserve-save is forbidden.
- Preserve-save is rejected only when the write would materialize lossy-decoded text. Callers can preflight this through `preserve_save_error()` / `save_error_for_options(...)` and explicitly convert through `DocumentSaveOptions::with_encoding(...)` or `Document::save_to_with_encoding(...)`.

### Large Files and Edit Limits

- Large files are supported for mmap-backed reads, viewport rendering, line-count estimation, and background indexing without full materialization.
- Editing is allowed only when Qem can do it without violating built-in safety limits. If an edit would require an unsafe promotion or full materialization, Qem returns `EditUnsupported`.
- Frontends should use `edit_capability_at(...)`, `edit_capability_for_range(...)`, or `edit_capability_for_selection(...)` when they need to surface that boundary before the user commits the action.
- `display_line_count()` is the supported scroll-sizing value while indexing is still in progress. Exact total line count may arrive later through `indexing_state()` and the line-count status helpers.

### Session and Background Job Guarantees

- Typed session/status APIs such as `indexing_state()`, `loading_state()`, `loading_phase()`, `save_state()`, `background_issue()`, `take_background_issue()`, and `close_pending()` are the supported frontend-facing async surface.
- `DocumentSession` and `EditorTab` typed edit helpers are idle-only. While a background open/save is active they return `EditUnsupported` instead of mutating state under an in-flight worker result.
- `close_file()` is truthful. If a background open/save is still running, close is deferred until that job completes instead of silently dropping the worker result.
- Repeated async open/save requests use first-job-wins semantics. While a load/save is active, later requests are rejected until `poll_background_job()` consumes the active result.
- Raw `document_mut()` and `set_path()` are escape hatches. Using them while busy invalidates the in-flight worker result and turns the next poll into a discard/error path instead of applying stale state.

### Search and Typed Reads

- Literal search is part of the current public contract through `find_next(...)`, `find_prev(...)`, `find_all(...)`, `LiteralSearchQuery`, and the bounded query/range helpers.
- This is a typed, case-sensitive literal search surface. It is not a regex subsystem.
- Bounded search returns only matches fully contained within the requested typed range or boundary positions.

### Sidecars and Recovery

- `.qem.lineidx` and `.qem.editlog` are internal sidecars used for cache/recovery behavior.
- Qem validates them against file length, modification time, and sampled content fingerprint. When they do not match, Qem may rebuild them, discard them, or reopen cleanly instead of trusting stale state.
- Sidecar recovery behavior is public. Sidecar on-disk format is not.

### Public Behavior vs Internal Format

- Stable public behavior in this release line includes the typed API surface, open/save lifecycle, async progress semantics, huge-file read contract, edit rejection semantics, and typed line/column rules.
- Internal implementation details include sidecar binary layout, cache structure, exact storage layout, and backing/layout decisions that are not explicitly promised by the typed API.

## Column Semantics

- `TextPosition::col0`, `TextRange::len_chars()`, `line_len_chars()`, `text_units_between()`, and `ViewportRequest::with_columns(...)` all use document text units.
- For UTF-8 text, a column is one Unicode scalar value. Combining marks therefore occupy their own columns, and wide CJK characters still count as one column even if they render as two terminal cells.
- Qem intentionally does not try to own grapheme clustering, display-cell width, tab expansion, or visual cursor movement. Those are frontend concerns.
- CRLF is treated as one text unit for typed range/edit/navigation semantics even though it is stored as two bytes in mmap-backed files.

## Frontend integration pattern

Qem stays UI-agnostic on purpose, but the intended editor/frontend flow is:

1. Open a file with `Document::open(...)` for a synchronous viewer or `DocumentSession::open_file_async(...)` for a responsive frontend.
2. Poll `poll_background_job()` and cache `status()` or the focused `loading_state()`, `loading_phase()`, `save_state()`, `background_issue()`, `close_pending()`, and `indexing_state()` values from your event loop. `loading_state()` covers the asynchronous open path itself; once the document is ready, continued line indexing is reported through `indexing_state()`. If a background job fails or is intentionally discarded as stale, `background_issue()` keeps the last typed problem available even after `background_activity()` returns to `Idle`. If `close_file()` was requested while the engine was busy, `close_pending()` tells the frontend that the document is now scheduled to disappear once the active worker finishes. Once you have surfaced that async problem to the user, call `take_background_issue()` to acknowledge and clear it explicitly.
3. Size scrollbars with `display_line_count()` while the line count is still estimated.
4. Render only the visible rows with `read_viewport(ViewportRequest::new(...).with_columns(...))`.
5. Use `edit_capability_at(...)` when the UI wants to disable or annotate edits before the user hits a huge-file safety limit.
6. Avoid using `text_lossy()` / `DocumentSession::text()` / `EditorTab::text()` in hot paths for large files. They materialize the entire current document into a new `String`. Prefer `read_viewport(...)` or `read_text(...)` for bounded reads.
7. Treat session/tab edit helpers as idle-only. If `is_busy()` is true, keep polling `poll_background_job()` instead of mutating through `try_insert(...)`, `try_replace(...)`, or the selection helpers. If you intentionally use raw `document_mut()` or `set_path()` while busy, expect the active load/save result to be discarded on the next poll.
8. If the user closes a session/tab while `is_busy()` is true, keep polling until the current job completes; `close_file()` defers the actual close so the engine can finish and account for the in-flight worker result. If a background save fails, surface the error and keep the document open for retry or explicit discard.
9. While `is_busy()` is true, treat the current `loading_state()` or `save_state()` path as authoritative. New async open/save requests will be rejected until you poll and finish that active job. The file write runs on a worker thread, but `save_async()` and `save_as_async()` still snapshot the current document before that worker starts, so very large edited buffers can make the call itself noticeable.
10. Keep GUI selection state as `TextSelection { anchor, head }`, read it through `read_selection(...)` for copy flows, convert it into a `TextRange` with `text_range_for_selection(...)` when needed, or use `try_replace_selection(...)` / `try_delete_selection(...)` / `try_cut_selection(...)` directly. For key handling, `try_backspace_selection(...)` and `try_delete_forward_selection(...)` handle the usual "delete selection or fall back to caret command" path for you.
11. For literal search, use `find_next(...)`, `find_prev(...)`, or `find_all(...)` and keep the returned `SearchMatch` values as typed `TextRange`/selection sources for highlight or replace flows. If you are repeating the same search many times from UI state, build one `LiteralSearchQuery` and call `find_next_query(...)` / `find_prev_query(...)` or the iterator forms `find_all_query(...)` / `find_all_query_from(...)` instead. If the search must stay inside a selection or other local region, use `find_next_in_range(...)` / `find_prev_in_range(...)` / `find_all_in_range(...)` or the query-based bounded variants. When you already have explicit selection endpoints, prefer the position-bounded `find_next_between(...)`, `find_prev_between(...)`, `find_next_query_between(...)`, `find_prev_query_between(...)`, `find_all_between(...)`, and `find_all_query_between(...)` helpers so callers do not have to precompute a typed `TextRange`. `find_prev(...)` returns the last match whose end is at or before the boundary position you pass, and bounded search only returns matches fully contained within the requested range. On clean mmap and piece-table backings this search follows stored bytes, including stored CRLF; rope-backed documents search the current in-memory `\n` representation.
12. For long-lived edited piece-table documents, treat compaction as an idle-time maintenance step. Prefer `maintenance_status()` or `maintenance_status_with_policy(...)` when the UI wants one explicit maintenance snapshot; use `recommended_action()` or the direct `maintenance_action()` helpers when the frontend only wants a high-level decision. Then run `run_idle_compaction()` or `run_idle_compaction_with_policy(...)` while the UI is idle. If the returned outcome says `ForcedPending`, or the snapshot says `explicit-compaction`, reserve the heavier rewrite for save-boundary or explicit operator maintenance. Keep unconditional `compact_piece_table()` for explicit maintenance flows.
13. Then save with `save_to(...)`, `DocumentSession::save_async(...)`, or `DocumentSession::save_as_async(...)`.

The new `frontend_session` example demonstrates that engine-facing lifecycle without pulling any GUI toolkit into the crate:

```powershell
cargo run --example frontend_session --features editor -- input.txt output.txt
```

## Stability Notes

- The Rust API is intended to be stable within the current release line except
  for clearly documented fixes to incorrect behavior.
- Sidecar artifacts such as `.qem.editlog` and `.qem.lineidx` are internal
  cache/session formats. Qem may regenerate them across releases, and callers
  should not treat them as long-term interchange formats.
- Sidecar recovery trusts the source file identity, currently defined as file
  length, modification time, and a sampled content fingerprint.
- The minimum supported Rust version for this release line is `1.85`.

## Roadmap

Current focus is `0.7.0` as an integration release, not a feature-expansion
release.

- `0.7.0`: integration release. Ship official frontend entry examples, tighten
  only the API surface that real integrations actually need, and make the
  README/support contract usable without reading half the source.
- `0.8.0`: regex + fast search + encoding stabilization. Add a typed regex
  search surface alongside literal search, push literal and regex search to
  practical real-file speed on `mmap`, `piece_table`, and `rope` backings,
  and widen the encoding contract with full `LossyDecodedPreserve` semantics.
- `0.9.0`: stability hardening. Make backing transitions, sidecar recovery,
  and huge-file edit rejection explicit and predictable, complete the
  "truth after error" test matrix, and land regression benches.
- `1.0.0`: public API freeze. Stabilize the typed surface, publish migration
  guidance, and keep internal storage/layout details explicitly out of the
  stable API promise.

The detailed release gates, non-goals, and exit criteria live in
[`ROADMAP.md`](ROADMAP.md).

Draft migration notes for the upcoming integration release live in
[`MIGRATION-0.7.md`](MIGRATION-0.7.md).

## Examples

Inspect a viewport from a huge file:

```powershell
cargo run --example viewport -- "C:\path\to\huge.log" 1000000 20
```

Open a file through the editor wrapper and save a modified copy:

```powershell
cargo run --example editor_session --features editor -- input.txt output.txt
```

The `editor_session` example now uses `EditorTab::open_file_async()`,
`loading_phase()`, `save_as_async()`, and `poll_background_job()` to show the
non-blocking tab workflow end to end.

Model the data flow that a GUI frontend would use:

```powershell
cargo run --example frontend_session --features editor -- input.txt output.txt
```

The `frontend_session` example shows how a frontend can poll load/save/index
state, distinguish open phases from continued indexing, compute a viewport,
read one maintenance snapshot through `maintenance_status()`, and render
visible rows through `DocumentSession::read_viewport(...)` while keeping UI
concerns out of the engine.

Run the minimal workspace `egui` demo for a small viewer/editor integration:

```powershell
cargo run -p qem-egui-demo -- "C:\path\to\input.txt"
```

Run the large-file-oriented `egui` demo when you want an explicit viewport,
gutter, caret, jump/page/tail navigation, open/save, and background status:

```powershell
cargo run -p qem-egui-demo --bin large_file -- "C:\path\to\huge.log"
```

The `qem-egui-demo` workspace member keeps GUI dependencies out of the core
crate while exercising two actual frontend paths around `DocumentSession`:
one minimal viewer/editor and one more explicit large-file viewport workflow.

Exercise the typed `Document` edit/read API directly:

```powershell
cargo run --example typed_editing -- input.txt output.txt
```

The `typed_editing` example shows a minimal non-session flow built around
`read_selection(...)`, `try_replace_selection(...)`, `try_cut_selection(...)`,
`read_text(...)`, `LiteralSearchQuery`, `find_next_query(...)`,
`find_prev_query(...)`, `maintenance_status()`, and `save_to(...)`.

Probe real files outside the synthetic Criterion fixtures:

```powershell
cargo run --example perf_probe -- input.txt --needle ERROR --seed-edit "[probe]\n" --save output.txt
```

The `perf_probe` example prints one-shot timings for `open`, indexing wait,
viewport reads, literal search, idle-maintenance state, and optional `save_to`
so you can build a cold/warm matrix on real `1GB / 10GB / 50GB` files.
Use `--find-all-limit` and optional `--find-all-range-lines` when you want a
fast capped probe of dense iterator throughput without waiting on full
Criterion dense-match runs.
Pass `--json` when you want machine-readable output for scripts or spreadsheet
pipelines.

For repeated `clean/edited` probe runs across several files, use
`.\scripts\collect_perf_matrix.ps1` to produce one JSONL matrix instead of
hand-running each `perf_probe` command.

## API Example

```rust
use qem::{Document, TextPosition, TextRange, ViewportRequest};
use std::path::Path;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = Path::new("huge.log");
    let mut doc = Document::open(path)?;

    let viewport = doc.read_viewport(ViewportRequest::new(10, 20).with_columns(0, 160));
    println!("scroll rows: {}", viewport.total_lines().display_rows());
    for row in viewport.rows() {
        println!(
            "{:>8}: [{}] {}",
            row.line_number(),
            if row.is_exact() { "=" } else { "~" },
            row.text()
        );
    }

    let _ = doc.try_insert(TextPosition::new(0, 0), "[Qem]\n")?;
    let _ = doc.try_replace(TextRange::new(TextPosition::new(1, 0), 4), "HEAD")?;
    doc.save_to(path)?;

    Ok(())
}
```

## Encoding Example

```rust
use qem::{Document, DocumentEncoding, DocumentOpenOptions, DocumentSaveOptions};
use std::path::Path;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = Path::new("legacy-cp1251.txt");
    let target = Path::new("legacy-cp1251-copy.txt");
    let encoding = DocumentEncoding::from_label("windows-1251").unwrap();

    let mut doc = Document::open_with_options(
        path,
        DocumentOpenOptions::new().with_reinterpretation(encoding),
    )?;
    if doc.decoding_had_errors() {
        eprintln!("source contained malformed byte sequences for {}", doc.encoding());
    }
    if let Some(reason) = doc.preserve_save_error() {
        eprintln!("preserve-save would not be safe yet: {reason}");
    }

    let _ = doc.try_insert_text_at(0, 0, "header: ")?;
    if let Some(reason) = doc.save_error_for_encoding(DocumentEncoding::utf8()) {
        eprintln!("cannot convert this document to utf-8 yet: {reason}");
        return Ok(());
    }
    doc.save_to_with_options(
        target,
        DocumentSaveOptions::new().with_encoding(DocumentEncoding::utf8()),
    )?;

    Ok(())
}
```

## Benchmarks

Qem ships with Criterion benchmarks for:

- large-file open and indexing
- viewport/scroll reads through `read_viewport`
- session-layer viewport reads through `DocumentSession` and `EditorTab`
- session-layer `text()` and `status()` overhead relative to raw `Document`
- viewport reads after large-file edits through the piece table
- typed text reads through `read_text` and `read_selection`
- typed literal search through `find_next` and `find_prev`, including bounded-range, no-match, and dense-match scenarios
- full-text materialization through `text_lossy`
- typed edit commands such as `try_insert`, `try_replace_selection`, and `try_delete_selection`
- piece-table compaction on fragmented edited documents
- streaming saves of edited large files

The built-in fixture sizes currently include:

- `1_000_000` lines for large-file open/indexing
- `400_000` lines for viewport read benchmarks
- `64_000` lines for steady-state `piece_table` edit benchmarks
- `4_096` lines for typed edit command benchmarks
- `250_000` lines for save benchmarks

Run them with:

```powershell
cargo bench --bench document_perf
```

Run only the session-layer overhead benches with:

```powershell
cargo bench --bench document_perf -- session_layer
```

## Project Links

- Benchmark methodology lives in [`BENCHMARKS.md`](BENCHMARKS.md).
- The current in-repo `0.7.0` seed perf matrix lives in [`PERF-BASELINE-0.7.md`](PERF-BASELINE-0.7.md).
- Cross-platform CI and Linux stress coverage live in [`.github/workflows/ci.yml`](.github/workflows/ci.yml).
- Repository: <https://github.com/mrhanty42/Qem>
