# Qem

High-performance Rust text engine for massive files, with mmap-backed reads,
incremental line indexing, and responsive editing.

Qem is built for editor-style workloads where opening huge files must stay
responsive, scrolling should avoid full materialization, and saves should
stream back to disk instead of rebuilding the entire document in memory.

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
- Async editor-tab open/save flows with progress polling
- Streaming atomic saves for large edited documents

## Installation

```toml
[dependencies]
qem = "0.2"
```

To disable the editor/session wrapper and use only the document/storage layer:

```toml
[dependencies]
qem = { version = "0.2", default-features = false }
```

## Cargo features

- `editor` (default): enables `EditorTab`, `CursorPosition`, and async open/save helpers.
- `tmp-auto` (default): dynamic scratch-temp policy. On Windows Qem prefers the executable directory, then the edited file directory, then the system temp directory. On Unix-like systems Qem prefers the edited file directory, then the system temp directory, then the executable directory.
- `tmp-source-dir`: keeps snapshot/scratch temp files next to the edited file.
- `tmp-system-dir`: uses the OS temp directory for snapshot/scratch temp files.
- `tmp-exe-dir`: uses the executable directory for snapshot/scratch temp files.

Only scratch files such as `.qem.snap.*` follow this policy. The temp file used for atomic save replacement still stays next to the destination file so `save_to()` remains atomic.

Example:

```toml
[dependencies]
qem = { version = "0.2", default-features = false, features = ["editor", "tmp-exe-dir"] }
```

Runtime override is also available:

- `QEM_TMP_POLICY=auto|source-dir|system-dir|exe-dir`
- `QEM_TMP_DIR=/absolute/path/to/custom/tmp/root`

## Core components

- `FileStorage`: mmap-backed file access.
- `Document`: background line indexing, fast line metrics, viewport reads, rope or piece-table editing, and persistent edit-session recovery.
- `EditorTab`: lightweight document session wrapper with cursor, async open/save helpers, and progress polling.

## Stability Notes

- The Rust API is intended to be stable within the `0.2.x` release line except
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

## API Example

```rust
use qem::Document;
use std::path::Path;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = Path::new("huge.log");
    let mut doc = Document::open(path)?;

    let visible = doc.line_slice(10, 0, 160);
    println!("exact: {}", visible.is_exact());
    println!("{}", visible.text());

    doc.try_insert_text_at(0, 0, "[Qem]\n")?;
    doc.save_to(path)?;

    Ok(())
}
```

## Benchmarks

Qem ships with Criterion benchmarks for:

- large-file open and indexing
- viewport/scroll reads through `line_slices`
- viewport reads after large-file edits through the piece table
- streaming saves of edited large files

Run them with:

```powershell
cargo bench --bench document_perf
```

## Project Links

- CI lives in [`.github/workflows/ci.yml`](.github/workflows/ci.yml).
- Repository: <https://github.com/MrHanty1488/Qem>
