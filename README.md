# Qem

Qem is a Rust text engine for editors, viewers, and tools that need to work with files too large to load into one `String`.

It provides mmap-backed reads, incremental line indexing, bounded viewport access, typed text positions and ranges, editing, search, atomic saves, and persistent edit sessions. Qem is a backend library: the application still owns rendering, visual cursor movement, tabs, and other UI decisions.

> **Project note:** Qem is designed, directed, and maintained by a human. AI tools have been used as development assistants for parts of implementation, testing, and documentation. Technical decisions, review, and project direction remain human-owned.

## Status

Qem is currently pre-1.0. The public API is usable, but it may still receive focused changes before the 1.0 stability freeze.

The primary path is UTF-8/ASCII. Explicit native support also exists for UTF-16 LE/BE, common single-byte encodings, Shift_JIS, GB18030, and EUC-KR. Invalid or lossy input is reported through the typed open/save APIs.

## What it does

- Opens large files without materializing the whole document
- Builds line indexes incrementally in the background
- Reads bounded viewports for virtualized editor rendering
- Uses rope or piece-tree backings when edits require them
- Supports typed insert, replace, delete, selection, undo, and redo operations
- Provides literal and regex search, including bounded and reverse search
- Saves edited documents through streaming atomic replacement
- Persists eligible edit sessions in `.qem.editlog` sidecars
- Exposes background loading, indexing, saving, and error state to frontends

Qem deliberately does **not** provide a text widget or GUI toolkit integration. It counts text units, not grapheme clusters or terminal display cells; visual layout remains a frontend concern.

## Installation

```toml
[dependencies]
qem = "0.8.1"
```

The default features include the editor/session layer and automatic temporary-directory selection. For the lower-level document engine only:

```toml
[dependencies]
qem = { version = "0.8.1", default-features = false }
```

### Features

- `editor` — `DocumentSession`, `EditorTab`, cursor state, and async open/save helpers
- `tmp-auto` — chooses a writable scratch location automatically
- `tmp-source-dir` — keeps scratch files beside the source file
- `tmp-system-dir` — uses the OS temporary directory
- `tmp-exe-dir` — uses the executable directory

Scratch policy can also be overridden with `QEM_TMP_POLICY` and `QEM_TMP_DIR`. Atomic save-replacement files always stay beside the destination.

## Quick start

```rust
use qem::{Document, TextPosition, ViewportRequest};
use std::path::Path;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = Path::new("huge.log");
    let mut document = Document::open(path)?;

    let viewport = document.read_viewport(
        ViewportRequest::new(100_000, 30).with_columns(0, 160),
    );

    for row in viewport.rows() {
        println!("{:>8}  {}", row.line_number(), row.text());
    }

    document.try_insert(TextPosition::new(0, 0), "[Qem]\n")?;
    document.save_to(path)?;
    Ok(())
}
```

For a GUI or long-lived frontend, start with `DocumentSession`. It wraps the document lifecycle, background open/save work, generation tracking, and status polling without imposing a rendering model.

## Editor demo

The workspace includes an `egui` editor that demonstrates virtualized viewport rendering, line numbers, caret navigation, editing, search, native file dialogs, background status, and save flows:

```powershell
cargo run -p qem-egui-demo -- "C:\path\to\input.txt"
```

You can also launch it without a path and choose a file from the UI.

A separate engineering-oriented large-file demo exposes explicit viewport, jump, page, column, indexing, and backing controls:

```powershell
cargo run -p qem-egui-demo --bin large_file -- "C:\path\to\huge.log"
```

The GUI dependencies belong only to the demo crate; the `qem` library itself stays UI-agnostic.

## Encodings and positions

`TextPosition::col0`, `TextRange::len_chars()`, viewport columns, and edit ranges use document text units. For UTF-8, one Unicode scalar value is one unit. Combining marks, grapheme clusters, wide characters, tab expansion, and visual columns are intentionally left to the frontend. Stored CRLF counts as one unit between lines.

Use `Document::open_with_options(...)` or `Document::open_with_encoding(...)` when the encoding is known. Preserve-save safety can be checked with `preserve_save_error()` and `save_error_for_options(...)`; explicit conversion is available through `DocumentSaveOptions`.

## Large-file behavior

Clean large files stay on the mmap-oriented path while Qem indexes lines in the background. During indexing, `display_line_count()` is suitable for scrollbar sizing and `indexing_state()` reports progress. Reads should remain bounded through `read_viewport`, `read_text`, and `read_selection`.

Edits are accepted only when Qem can perform them within its safety limits. Frontends can query `edit_capability_at`, `edit_capability_for_range`, or `edit_capability_for_selection` before exposing an operation.

`.qem.lineidx` and `.qem.editlog` are internal cache/recovery sidecars. Qem validates them against source-file identity and may rebuild or discard them; their binary layout is not a stable interchange format.

## Examples

```powershell
cargo run --example viewport -- "C:\path\to\huge.log" 1000000 20
cargo run --example frontend_session --features editor -- input.txt output.txt
cargo run --example typed_editing -- input.txt output.txt
cargo run --example perf_probe -- input.txt --needle ERROR --json
```

The examples cover low-level viewport reads, frontend session lifecycle, typed edits and search, and one-shot performance probes.

## Quality checks

```powershell
cargo fmt --all -- --check
cargo check --workspace --all-targets
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

CI runs across Windows, macOS, and Linux. Property tests cover storage backings, encoding boundaries, search symmetry, edit behavior, and save/recovery invariants.

## Benchmarks

Criterion benchmarks and real-file probes cover open/indexing, viewport reads, typed reads and edits, search, piece-tree maintenance, and saves:

```powershell
cargo bench --bench document_perf
```

Benchmark methodology, fixture guidance, and giant-file caveats are documented in [`BENCHMARKS.md`](BENCHMARKS.md). Results should include the exact command, commit, toolchain, OS, hardware, storage, and cache conditions.

## Road to 1.0

The current priorities are correctness across supported encodings and backings, a smaller stable public surface, removal of remaining compatibility APIs, and clearer frontend integration guidance. The detailed release gates and non-goals live in [`ROADMAP.md`](ROADMAP.md).

## License

Licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT License ([LICENSE](LICENSE))

Repository: <https://github.com/mrhanty42/Qem>
