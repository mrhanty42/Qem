# Qem

High-performance Rust text engine for massive files, with mmap-backed reads,
incremental line indexing, and responsive editing.

Qem is built for editor-style workloads where opening huge files must stay
responsive, scrolling should avoid full materialization, and saves should
stream back to disk instead of rebuilding the entire document in memory.

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
- Background line indexing with a sparse on-disk line index
- Lazy promotion to rope or piece-table editing buffers
- Persistent edit sessions with undo/redo recovery
- Streaming atomic saves for large edited documents

## Installation

```toml
[dependencies]
qem = "0.1"
```

To disable the editor/session wrapper and use only the document/storage layer:

```toml
[dependencies]
qem = { version = "0.1", default-features = false }
```

## Cargo features

- `editor` (default): enables `EditorTab`, `CursorPosition`, and async save helpers.
- `tmp-auto` (default): dynamic scratch-temp policy. On Windows Qem prefers the executable directory, then the edited file directory, then the system temp directory. On Unix-like systems Qem prefers the edited file directory, then the system temp directory, then the executable directory.
- `tmp-source-dir`: keeps snapshot/scratch temp files next to the edited file.
- `tmp-system-dir`: uses the OS temp directory for snapshot/scratch temp files.
- `tmp-exe-dir`: uses the executable directory for snapshot/scratch temp files.

Only scratch files such as `.qem.snap.*` follow this policy. The temp file used for atomic save replacement still stays next to the destination file so `save_to()` remains atomic.

Example:

```toml
[dependencies]
qem = { version = "0.1", default-features = false, features = ["editor", "tmp-exe-dir"] }
```

Runtime override is also available:

- `QEM_TMP_POLICY=auto|source-dir|system-dir|exe-dir`
- `QEM_TMP_DIR=/absolute/path/to/custom/tmp/root`

## Core components

- `FileStorage`: mmap-backed file access.
- `Document`: background line indexing, fast line metrics, viewport reads, rope or piece-table editing, and persistent edit-session recovery.
- `EditorTab`: lightweight document session wrapper with cursor and save helpers.

## Examples

Inspect a viewport from a huge file:

```powershell
cargo run --example viewport -- "C:\path\to\huge.log" 1000000 20
```

Open a file through the editor wrapper and save a modified copy:

```powershell
cargo run --example editor_session --features editor -- input.txt output.txt
```

## Example

```rust
use qem::Document;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut doc = Document::open("huge.log")?;

    let visible = doc.line_slice(10, 0, 160);
    println!("exact: {}", visible.is_exact());
    println!("{}", visible.text());

    doc.insert_text_at(0, 0, "[Qem]\\n");
    doc.save_to("huge.log")?;

    Ok(())
}
```

## Benchmarks

Qem ships with Criterion benchmarks for:

- large-file open and indexing
- viewport/scroll reads through `line_slices`
- streaming saves of edited large files

Run them with:

```powershell
cargo bench --bench document_perf
```
"# Qem" 
