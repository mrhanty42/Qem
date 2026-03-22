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
next `Document::open`, as long as the source file metadata still matches.

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
qem = "0.3"
```

To disable the editor/session wrapper and use only the document/storage layer:

```toml
[dependencies]
qem = { version = "0.3", default-features = false }
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
qem = { version = "0.3", default-features = false, features = ["editor", "tmp-exe-dir"] }
```

Runtime override is also available:

- `QEM_TMP_POLICY=auto|source-dir|system-dir|exe-dir`
- `QEM_TMP_DIR=/absolute/path/to/custom/tmp/root`

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
- Older compatibility helpers that silently swallow edit errors or expose raw tuple progress are still present for now, but they are deprecated in favor of the typed/session-first API.

## Frontend integration pattern

Qem stays UI-agnostic on purpose, but the intended editor/frontend flow is:

1. Open a file with `Document::open(...)` for a synchronous viewer or `DocumentSession::open_file_async(...)` for a responsive frontend.
2. Poll `poll_background_job()` and cache `status()` or the focused `loading_state()`, `save_state()`, and `indexing_state()` values from your event loop.
3. Size scrollbars with `display_line_count()` while the line count is still estimated.
4. Render only the visible rows with `read_viewport(ViewportRequest::new(...).with_columns(...))`.
5. Use `edit_capability_at(...)` when the UI wants to disable or annotate edits before the user hits a huge-file safety limit.
6. Keep GUI selection state as `TextSelection { anchor, head }`, read it through `read_selection(...)` for copy flows, convert it into a `TextRange` with `text_range_for_selection(...)` when needed, or use `try_replace_selection(...)` / `try_delete_selection(...)` / `try_cut_selection(...)` directly. For key handling, `try_backspace_selection(...)` and `try_delete_forward_selection(...)` handle the usual "delete selection or fall back to caret command" path for you. Then save with `save_to(...)`, `DocumentSession::save_async(...)`, or `DocumentSession::save_as_async(...)`.

The new `frontend_session` example demonstrates that engine-facing lifecycle without pulling any GUI toolkit into the crate:

```powershell
cargo run --example frontend_session --features editor -- input.txt output.txt
```

## Stability Notes

- The Rust API is intended to be stable within the `0.3.x` release line except
  for clearly documented fixes to incorrect behavior.
- Sidecar artifacts such as `.qem.editlog` and `.qem.lineidx` are internal
  cache/session formats. Qem may regenerate them across releases, and callers
  should not treat them as long-term interchange formats.
- The minimum supported Rust version for this release line is `1.85`.

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
`save_as_async()`, and `poll_background_job()` to show the non-blocking tab
workflow end to end.

Model the data flow that a GUI frontend would use:

```powershell
cargo run --example frontend_session --features editor -- input.txt output.txt
```

The `frontend_session` example shows how a frontend can poll load/save/index
progress, compute a viewport, and render visible rows through
`DocumentSession::read_viewport(...)` while keeping UI concerns out of the engine.

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

## Benchmarks

Qem ships with Criterion benchmarks for:

- large-file open and indexing
- viewport/scroll reads through `read_viewport`
- viewport reads after large-file edits through the piece table
- streaming saves of edited large files

The built-in fixture sizes currently include:

- `1_000_000` lines for large-file open/indexing
- `400_000` lines for viewport read benchmarks
- `250_000` lines for save benchmarks

Run them with:

```powershell
cargo bench --bench document_perf
```

## Project Links

- CI lives in [`.github/workflows/ci.yml`](.github/workflows/ci.yml).
- Repository: <https://github.com/MrHanty1488/Qem>
