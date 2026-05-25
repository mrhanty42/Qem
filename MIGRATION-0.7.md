# Qem 0.7.0 Migration Notes

This file describes how to upgrade from `0.6.x` to `0.7.0`.

## Who Should Read This

Read this if you are upgrading from `0.6.x` and you integrate Qem into a GUI,
editor, or other frontend-facing text workflow.

## Summary

`0.7.0` is the integration release.

The goal of this release is not new subsystems. The goal is to make the
existing frontend-facing path around `Document`, `DocumentSession`, and
`EditorTab` coherent enough that a new integrator can succeed from docs and
examples.

There are no breaking API changes between `0.6.x` and `0.7.0`. Existing code
that compiles against `0.6.3` is expected to compile against `0.7.0` without
source changes. Public additions are listed below for visibility.

## Breaking Changes

None.

No public types, methods, traits, modules, or feature flags were removed,
renamed, or changed in signature between `0.6.3` and `0.7.0`.

If you are still using deprecated compatibility wrappers, they remain
available and `#[doc(hidden)]`. They are tracked for eventual removal in a
later release line and replaced by the typed `try_*` and typed-progress
helpers, but `0.7.0` does not delete them.

## Non-Breaking Additions

These are public additions in `0.7.0`. Your existing code does not need to
use them, but they are the recommended frontend surface going forward.

- `DocumentSession::is_line_count_pending()`,
  `EditorTab::is_line_count_pending()`,
  `DocumentSessionStatus::is_line_count_pending()`,
  `EditorTabStatus::is_line_count_pending()`. These pass through the existing
  `Document::is_line_count_pending()` value so frontends can distinguish
  "still estimating" from "done" without peeling back into raw
  `DocumentStatus`.
- An expanded set of session/tab scenario tests covering CRLF-preserving
  open/edit/save flows and very long line viewport/edit/save flows, plus
  more "truth after error" coverage around `document_mut` and `set_path`
  while busy.
- A workspace `qem-egui-demo` member with two binaries: a minimal
  viewer/editor and a large-file-oriented viewport demo. These are demo
  crates, not part of the published `qem` crate, and they keep GUI
  dependencies out of the core library.
- `ROADMAP.md`, `MIGRATION-0.7.md`, and `PERF-BASELINE-0.7.md` release
  documents committed in-tree.
- Example smoke coverage in CI for the official CLI/frontend examples.

## Recommended Upgrade Steps

For most frontend integrations:

- Bump the dependency: `qem = "0.7.0"`.
- Prefer `DocumentSession` as the default entry path.
- Use `Document` directly only when your application already owns its own
  session/job lifecycle.
- Use `EditorTab` only when you specifically want built-in cursor
  convenience on top of the same session machinery.

### Prefer the typed session/status surface

If your application still relies on older tuple-based progress helpers or
compatibility wrappers, move toward:

- `loading_state()`
- `loading_phase()`
- `save_state()`
- `background_issue()`
- `take_background_issue()`
- `close_pending()`
- `is_line_count_pending()`
- the typed `try_*` edit helpers

These remain the intended frontend-facing contract for the `0.7.x` line and
will stay supported through the `0.8.0` and `0.9.0` releases.

### Treat full-text helpers as escape hatches

For large-file UI loops, prefer bounded reads such as:

- `read_viewport(...)`
- `read_text(...)`
- `read_selection(...)`

Avoid `text_lossy()` / `DocumentSession::text()` / `EditorTab::text()` in
hot render paths unless you intentionally want full materialization.

### Expect explicit huge-file edit boundaries

Qem stays truthful when an edit would cross built-in safety limits.

For frontend code, that means:

- surface `EditUnsupported` instead of guessing
- use `edit_capability_at(...)`, `edit_capability_for_range(...)`, and
  `edit_capability_for_selection(...)` to preflight UI actions
- do not assume every visible huge-file position is editable

### Keep GUI dependencies out of `qem` core

The repository now includes official GUI demo code through the workspace
member `qem-egui-demo`, including:

- a minimal viewer/editor demo
- a large-file-oriented viewport demo

This is the intended direction for frontend examples: real integration demos
without pulling GUI dependencies into the library crate itself.

### Follow the support contract, not sidecar format

Sidecar recovery behavior is public.

Sidecar binary layout is not.

If your application inspects `.qem.lineidx` or `.qem.editlog` directly, treat
that as unsupported. Sidecar layout will continue to evolve internally
through the `0.8.0` and `0.9.0` releases without going through semver.

## What Comes Next

`0.8.0` is the search and encoding release: typed regex search, faster
literal and regex search on real huge files, and a wider stable encoding
contract.

`0.9.0` is the stability release: explicit backing transitions, predictable
huge-file rejection, sidecar recovery outcomes, full "truth after error"
coverage, and regression benches in CI.

`1.0.0` freezes the public API.
