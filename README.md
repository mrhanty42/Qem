# Qem

Qem is a Rust text engine for editors, viewers, and tools that need to work
with files too large to load into one `String`.

It combines memory-mapped reads with an incremental line index so a
multi-gigabyte file opens in milliseconds and the first viewport is ready
before indexing finishes. When edits arrive, Qem promotes the document to a
piece-table backing and keeps every guarantee. A separate editor-session
layer wraps the whole lifecycle — background jobs, status polling, guarded
mutations — so a frontend only has to render.

> **Project note:** Qem is designed, directed, and maintained by a human. AI
> tools have been used as development assistants for parts of implementation,
> testing, and documentation. Technical decisions, review, and project
> direction remain human-owned.

## Features

- Opens files of any size without materializing the whole document.
- Builds the line index incrementally in the background; viewports read
  during indexing.
- Bounded viewport reads for virtualized editor rendering.
- Rope backing for small files and untitled documents; piece-table backing
  when edits demand it; both transition automatically.
- Typed insert, replace, delete, selection, undo, and redo operations with
  caret reporting.
- Encoding-aware literal and regex search — forward, reverse, and bounded —
  on every backing.
- Streaming atomic saves for edited documents at any size.
- Persistent edit sessions in `.qem.editlog` sidecars, including the
  document encoding.
- Typed errors everywhere: no panics on hostile input, no silent data loss.

Qem deliberately does **not** provide a text widget or GUI toolkit
integration. It counts text units, not grapheme clusters or terminal display
cells; visual layout remains a frontend concern.

## Supported encodings

| Encoding | Route | Notes |
| --- | --- | --- |
| UTF-8 | native | primary path; BOM handled on the auto-detect policy |
| UTF-16 LE / BE | native | cell-exact line model, encoding-aware search |
| Single-byte (`windows-125x`, `KOI8-R/U`, `IBM866`, `ISO-8859-x`, `macintosh`) | native | byte-exact columns |
| Shift_JIS, GB18030, EUC-KR | native | multi-byte-cell aware; search validates character boundaries |
| Other encodings | rope decode | reopen materializes a UTF-8 rope; subject to the save/reopen size budget |

Invalid or lossy input is reported through the typed open/save APIs.
Native-route documents save and reopen at any size.

## Installation

```toml
[dependencies]
qem = "0.9.0"
```

The default features include the editor/session layer and automatic
temporary-directory selection. For the lower-level document engine only:

```toml
[dependencies]
qem = { version = "0.9.0", default-features = false }
```

### Feature flags

- `editor` — `DocumentSession`, `EditorTab`, cursor state, and async
  open/save helpers.
- `tmp-auto` — explicit marker for the default scratch policy; the flag adds
  no code path.
- `tmp-source-dir` — keeps scratch files beside the source file.
- `tmp-system-dir` — uses the OS temporary directory.
- `tmp-exe-dir` — uses the executable directory.

Scratch policy can also be overridden with the `QEM_TMP_POLICY` and
`QEM_TMP_DIR` environment variables. Atomic save-replacement files always
stay beside the destination.

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

For a GUI or long-lived frontend, start with `DocumentSession`. It wraps the
document lifecycle, background open/save work, generation tracking, and
status polling without imposing a rendering model.

## Positions and encodings

`TextPosition::col0`, `TextRange::len_chars()`, viewport columns, and edit
ranges use document text units. For UTF-8, one Unicode scalar value is one
unit. Combining marks, grapheme clusters, wide characters, tab expansion,
and visual columns are intentionally left to the frontend. Stored CRLF
counts as one unit between lines.

Use `Document::open_with_options(...)` or `Document::open_with_encoding(...)`
when the encoding is known. Preserve-save safety can be checked with
`preserve_save_error()` and `save_error_for_options(...)`; explicit
conversion is available through `DocumentSaveOptions`.

## Large-file behavior

Clean large files stay on the mmap-oriented path while Qem indexes lines in
the background. During indexing, `display_line_count()` is suitable for
scrollbar sizing and `indexing_state()` reports progress. Reads should
remain bounded through `read_viewport`, `read_text`, and `read_selection`.

Edits are accepted only when Qem can perform them within its safety limits.
Frontends can query `edit_capability_at`, `edit_capability_for_range`, or
`edit_capability_for_selection` before exposing an operation.

`.qem.lineidx` and `.qem.editlog` are internal cache/recovery sidecars. Qem
validates them against source-file identity and may rebuild or discard them;
their binary layout is not a stable interchange format.

## Editor demo

The workspace includes an `egui` editor that demonstrates virtualized
viewport rendering, line numbers, caret navigation, editing, search, native
file dialogs, background status, and save flows:

```powershell
cargo run -p qem-egui-demo -- "C:\path\to\input.txt"
```

You can also launch it without a path and choose a file from the UI. A
separate engineering-oriented large-file demo exposes explicit viewport,
jump, page, column, indexing, and backing controls:

```powershell
cargo run -p qem-egui-demo --bin large_file -- "C:\path\to\huge.log"
```

The GUI dependencies belong only to the demo crate; the `qem` library itself
stays UI-agnostic.

## Examples

```powershell
cargo run --example viewport -- "C:\path\to\huge.log" 1000000 20
cargo run --example frontend_session --features editor -- input.txt output.txt
cargo run --example typed_editing -- input.txt output.txt
cargo run --example perf_probe -- input.txt --needle ERROR --json
```

The examples cover low-level viewport reads, `EditorTab` and
`DocumentSession` background lifecycle, typed edits and search, and one-shot
performance probes.

## Quality checks

```powershell
cargo fmt --all -- --check
cargo check --workspace --all-targets
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

CI runs across Windows, macOS, and Linux. Property tests cover storage
backings, encoding boundaries, search symmetry, edit behavior, and
save/recovery invariants.

## Benchmarks

Criterion benchmarks and real-file probes cover open/indexing, viewport
reads, typed reads and edits, search, piece-tree maintenance, and saves:

```powershell
cargo bench --bench document_perf
```

Benchmark methodology, fixture guidance, giant-file caveats, and recorded
baseline numbers are documented in [`BENCHMARKS.md`](BENCHMARKS.md) and
[`benches/baselines/`](benches/baselines). Results should include the exact
command, commit, toolchain, OS, hardware, storage, and cache conditions.

## Road to 1.0

The current priorities are correctness across supported encodings and
backings, a smaller stable public surface, and clearer frontend integration
guidance. The release plan — completed work, remaining items, and the
stability contract — lives in [`ROADMAP.md`](ROADMAP.md).

## License

Licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT License ([LICENSE](LICENSE))

Repository: <https://github.com/mrhanty42/Qem>

