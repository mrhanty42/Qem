# Changelog

## Unreleased (1.0.0)

### Fixed

- Class B regex windows that slide into the middle of a CJK character
  re-align before decoding. The window slide is an arbitrary byte cut
  (8 MiB chunk minus the fixed 1 MiB overlap), which is exact for
  UTF-16's even grid but lands mid-sequence for variable-length CJK
  encodings - with a 3-byte character period every slide point is
  1 (mod 3). A window opening on a trail byte decodes a leading U+FFFD,
  and the decoded-to-source mapping re-encodes it to an 8-byte numeric
  character reference in Shift_JIS / EUC-KR, drifting every match
  mapped after it by 7 bytes (wrong offsets, positions, and match
  bytes on any CJK document spanning more than one window). Each CJK
  window now drops leading bytes until a probe no longer decodes with
  a replacement as its first character; the dropped fragment belongs
  to the previous window's tail, which the overlap already covers.
  A 9.5 MiB Shift_JIS fixture whose only match sits in the provably
  mid-sequence second window pins the mapping.
- Structural piece-tree edits refuse to run through an unreadable page.
  `insert_at` / `delete_at` answered an empty child list when a
  lazily-read piece page failed to load (any IO hiccup or CRC
  mismatch), the parent recomputed its totals over the survivors, and
  the next edit silently shrank the document while `total_len` still
  advertised the full size - content loss that undo could not restore,
  since the shrunken root was recorded as history. `PieceTree::insert`
  / `delete_range` now return `io::Result`, restore the previous root
  when a page read fails, and the piece-table edit paths thread the
  error to the caller. A long session after a disk failure degrades
  into explicit edit failures instead of silent truncation.
- Hostile sidecar geometry degrades to typed errors instead of panics.
  The piece-offset arithmetic overflowed on a corrupt-huge page id
  (debug panic, release wrap) - every offset expression now saturates
  end to end. `PieceTable::read_range` sliced source bytes at bounds
  taken verbatim from page payloads, so a page advertising a piece
  past the real source length panicked the reader; segments are now
  clamped to the actual source length. Tests pin a corrupt
  `history_first_page_id` header field and a 100-byte piece over a
  7-byte source.
- Pages kept resident while editlog persistence is down are bounded.
  Once `persistence_failed` was set, `alloc_page` never wrote again
  but kept inserting every new page into `resident_pages`, and
  `flush_session` refuses while that map is non-empty - a long session
  after a disk failure accumulated one page per structural edit for
  the rest of the session. The map now keeps a bounded 4096-page
  window.
- Reverse regex keep-last scans stream their window matches instead of
  collecting them: a dense pattern over an 8 MiB decoded window used
  to transiently allocate on the order of the match count on every
  window slide.

- The engine-aware text-unit walker
  (`advance_offset_by_text_units_with_engine`) carries the walk at the
  first unconsumed byte across its 64 KiB chunk edges. The previous
  chunk carry stepped back one byte at every fully consumed edge, so
  the boundary byte was counted twice: deleting or replacing a range of
  64 KiB+ cells on a Class A piece-table document left one stray cell
  per interior boundary behind, and UTF-16 tails re-decoded from an odd
  offset, shifting every subsequent cell by one byte. The walker now
  also refuses to decode a cell from a window that truncates it (CJK
  engines fall back to a 1-byte step on truncated leads, which used to
  split the cell across the edge), while a CRLF split across the edge
  still collapses into one text unit - including when the units run out
  exactly on the CR half. Four walker tests pin Class A, UTF-16LE, a
  Shift_JIS split cell, and the CRLF straddle.
- `find_prev_regex*` on UTF-16 / CJK documents returns the last
  non-overlapping match again. The reverse decode scan documented
  keep-last semantics but each window only looked at the first regex
  hit, so a range within one 8 MiB decode window answered the leftmost
  match and "find previous" stayed stuck at the document's first
  occurrence whenever two or more matched. keep_last now walks
  `find_iter` over the decoded window (overlap-filtered) and records
  the window's last valid candidate, matching the small-span byte
  fallback. A two-occurrence UTF-16LE fixture pins it; the single-match
  pins from the previous wave pass unchanged.
- Class B regex match positions resolve in the byte space the match was
  found in. `build_class_b_search_match` mapped match offsets through
  the original mmap snapshot and the open-time line offsets, but an
  edited Class B document promotes to the piece table on its first edit
  while the snapshot stays in place, so every regex position drifted
  after that first insert or delete (and the "currently unused for
  piece-table backings" comment beside the mapper falsified what the
  same series had just wired up). Piece-table-backed matches now go
  through the piece table's engine-aware position scan - the same model
  every other piece-table position surface uses - while mmap-backed
  documents keep the snapshot mapper. The piece-table UTF-16 column
  model keeps its documented 1.0 line-model caveat.
- Editlog session open rejects an invalid `page_count` instead of
  trusting it. The unchecksummed header field fed the committed-size
  truncation and `next_page_id` directly, while every other raw header
  counter is bounded by what the sidecar can physically hold: a
  corrupt-small value truncated pages the history still references
  (lazy page misses then read short while `total_len` stays full, and
  the drop-and-rebuild recovery never fired), and a corrupt-huge one
  wrapped the page-offset arithmetic on the next flush. Open now
  surfaces either direction as a typed `InvalidData` so the session
  sidecar is dropped and rebuilt. Both corruption directions are pinned
  by tests, and the orphan-page truncation test passes unchanged.
- Class B regex window hardening: the chunked decode scan now actually
  rounds the window start up to the UTF-16 cell grid as its contract
  documents (guards a truncated odd-length file tail), and a
  decoded-to-source mapping failure can no longer abort the scan and
  discard a match already accumulated on an earlier window by the
  reverse keep-last path. Piece-split backward alignment no longer snaps
  a slice at the very end of an odd-length piece, which would have
  carved a one-byte mid-cell right piece. None of these paths is
  reachable through the public API today; the hardening pins the
  documented contracts instead of relying on that.
- Bounded literal search on non-UTF-8 piece tables is engine-aware.
  `find_next_in_range` / `find_prev_in_range` / `find_all_in_range` (and
  the `_between` variants) used to walk the range end and its fallback
  typed positions with plain UTF-8 stepping, so on a windows-1251 piece
  table a "collision pair" (two adjacent bytes that also parse as one
  UTF-8 sequence) was swallowed into a single unit and the search range
  overshot into the next line, surfacing matches outside the requested
  range. The range bounds now advance through
  `advance_offset_by_text_units_with_engine`, and
  `position_for_byte_offset` / `position_for_byte_offset_from` / the
  reverse fast-path scan take the document's engine instead of a
  hard-coded `Utf8Engine`. Regression test pins both directions with a
  five-pair fixture.
- Regex match lengths are engine-aware. `finalize_byte_match` counted
  `SearchMatch::len_chars` with `std::str::from_utf8(..).unwrap_or("")`,
  which answered `0` units for a windows-1251 match whose bytes are not
  valid UTF-8 and grouped a cp1251 collision pair into one unit, while
  the match's start/end positions were already engine-aware. The unit
  count now walks the matched bytes through the document's engine, so
  the reported range stays self-consistent for `replace`-style
  consumers. Two regression tests pin the invalid-UTF-8 and
  collision-pair fixtures.
- `find_prev_regex*` (including `_in_range` / `_between`) on UTF-16 and
  CJK multibyte documents routes through the same chunked decode path as
  the forward direction instead of running the byte-engine regex over
  raw storage bytes. The old routing silently answered `None` for ASCII
  needles on UTF-16 (`0x00` filler bytes sit between the characters),
  mis-anchored on filler bytes, and never matched non-ASCII needles on
  Shift_JIS / GB18030 / EUC-KR, while forward `find_next_regex` on the
  same document found them. The reverse decode scan keeps the
  last-non-overlapping match, matching the small-span byte fallback's
  semantics. Regression tests pin UTF-16LE and Shift_JIS documents.
- Class B native open records the trailing empty line when the file ends
  with a line break. The UTF-16 / CJK inline line scanner stopped at
  `next == file_len` without pushing the offset, so `"AB\n"` in UTF-16LE
  reported one line where the identical content as UTF-8 (and through
  the piece-tree break counters) reports two. The scanner now
  distinguishes a line break ending at EOF from plain end-of-text via
  the engine's `trim_trailing_line_break` and keeps the line model
  consistent across encoding classes. Regression test pins UTF-16LE,
  Shift_JIS, and the UTF-8 reference.
- Piece-tree splits stay cell-aligned for non-UTF-8 documents. The split
  / trim aligner ran a UTF-8 boundary walk gated on `from_utf8` *byte
  validity* instead of on the document encoding, so a UTF-16LE stream
  that is also valid UTF-8 (any pure-ASCII file, or cells like
  `41 C2 80 C2`) had its insert/delete splits pulled into the middle of
  a code unit, breaking every later piece-local decode in that window.
  Piece-level alignment now dispatches on the document's engine:
  UTF-16 keeps the 2-byte cell grid anchored at the piece start, Class A
  and CJK multibyte keep the already cell-aligned edit offsets verbatim,
  and the UTF-8 walk applies to UTF-8 only. Regression test pins a
  UTF-16LE insert whose byte stream is simultaneously valid UTF-8.
- `edit_capability_at` mirrors the real edit plan for non-UTF-8
  documents. It used to promise `RequiresPromotion(Rope)` for small
  storage-backed non-UTF-8 files, while the first edit actually creates
  a piece tree (`edit_buffer_plan_for_line` drops the size gate for
  them). The capability fork now follows the same encoding dispatch.
  Regression test pins the windows-1251 and UTF-8 twins.
- The cursor position scan never stalls on a zero-step engine cell.
  `scan_cursor_position_bytes` (piece-table `position_for_char_index`)
  lacked the `.max(1)` guard its twin `scan_position_bytes` got in the
  0.8.1 zero-step fix, so an odd-length UTF-16 segment (truncated file
  or a piece boundary inside a code unit) re-counted the same tail byte
  once per remaining unit and inflated the column. Regression test pins
  a one-byte truncated UTF-16 cell.
- `file_len` for rope-backed non-UTF-8 documents is cached behind the
  existing content-cache invalidation discipline. That combination has
  no O(chunks) length formula - the exact answer required a full render
  plus re-encode on every status poll, edit-capability check, and
  edit-offset alignment of a promoted document. The encoded length is
  now computed once per edit generation; a regression test tracks it
  across successive edits on a big5 document.

### Changed

- README documents `tmp-auto` as the explicit marker for the default
  scratch policy: the flag itself has no code hook, the automatic
  writable-location fallback already applies when none of the other
  `tmp-*` features (and no `QEM_TMP_POLICY` override) are set.
- Piece-table documents now carry their encoding engine: column
  arithmetic (`line_len_chars`, `byte_offset_for_col`), line-segment
  decoding, and `read_text` from line/col positions all go through the
  document's engine instead of UTF-8 stepping and UTF-8 lossy decoding.
  Before this, a windows-1251 (or any Class A) document edited into a
  piece table rendered every viewport line as U+FFFD garbage after the
  first edit. Regression tests cover the cp1251 viewport/edit path end
  to end, including the UTF-8 "collision pair" case where two adjacent
  cp1251 bytes form a valid UTF-8 sequence and the old column scan
  silently swallowed one. Search and `position_for_char_index` on
  Class A piece tables are engine-aware too (`advance_position_by_bytes`
  and the cursor scan now take the document's engine); line-indexed
  reads for multi-byte-cell documents (UTF-16/CJK) remain a documented
  1.0 item - see the line-model entry in ROADMAP.
- `advance_offset_by_text_units_with_engine` no longer materializes the
  document tail: the engine-aware walker reads bounded 64 KiB chunks
  with one byte of CRLF context, so a replace near the start of a
  multi-gigabyte non-UTF-8 document no longer allocates the entire
  remaining document per edit.
- Composite edits (replace = delete + insert inside one edit batch) no
  longer corrupt the known-region boundary history: the batch-aware
  `set_known_boundary` writes to the pending history slot and the
  deferred root commit picks it up, so undoing a composite replace on a
  partial-window document restores the pre-batch boundary instead of
  the post-batch one. The regression test fails against the old
  behavior (verified: 199945 vs 200000) and passes with the fix.
- The `.qem.editlog` open path truncates orphan pages past the committed
  header size. A crash between the page append and the header rewrite
  used to leave unreachable bytes at the tail of the sidecar; page
  writes append at end-of-file and verify the offset against the page
  id, so the first edit after reopening poisoned every later flush into
  `persistence_failed` and the session silently stopped persisting for
  the rest of its life. The header is the commit record, so open now
  restores the exact pre-crash append position; a regression test
  simulates the interrupted flush end to end.
- Piece-table undo/redo restores the known-region boundary for partial
  windows, not only for full-index documents. Every history entry now
  carries the `(known_byte_len, full_index)` pair that belonged to the
  state it represents; undoing a delete inside a partial window used to
  clamp the boundary down and truncate reads of the last known lines
  until the next edit. The regression test fails against the old
  min-clamp behavior and passes with the history restore.
- Line-offset scanning pairs a CRLF whose CR sits exactly on the
  1 GiB scan-budget edge with its LF: the lookahead is bounded by the
  buffer length instead of the budget, so the pair is no longer
  miscounted as a lone CR plus a line.
- `align_byte_offset` no longer returns mid-character offsets for
  rope-backed UTF-8 documents. The alignment window snapped its base
  outward onto a char boundary while the caller rebased its answer on
  the *requested* offset, shifting every result by the snap delta; the
  backward direction could land on a continuation byte. The window now
  reports its actual base and both call sites rebase on it. A unit test
  pins the boundary property on a multi-byte rope fixture.
- Piece-table compaction is capped by the full-materialization budget
  (128 MiB, like the rope path). Compaction copies the whole document
  into the edit buffer, so `run_idle_compaction` - which the crate docs
  advertise as an idle-time helper - used to attempt a full-document
  allocation on partial-window huge documents and could abort the
  process. The recommendation path never proposes a rewrite the
  executor would refuse, and `compact_piece_table` documents the
  refusal.
- The save skip paths (nothing to write: the document is already saved
  and byte-identical at the target) now clear the retained background
  issue like every other successful save completion, matching the
  crate-docs contract instead of silently keeping a stale problem.
- The `large_file` demo binary no longer allocates a console window in
  release builds, matching the main demo (the 0.9.0 changelog claim now
  holds for both binaries).

### Changed

- Removed the stale module-level `#![allow(dead_code)]` from the
  alignment and encoding-engine modules: every item under them is
  reachable again, so the blanket waivers are gone.
- The no-editor build (`--no-default-features`) is warning-clean. The
  session-driven pieces of the save/rebase machinery (the discarded-save
  rebase chain, its saved-source span helpers, the disk-history rebuild
  hook, and the already-saved fast path) are now explicitly
  `editor`-feature-gated, the dead `snapshot_add_len` field is removed,
  and CI runs clippy with `-D warnings` on the no-editor configuration
  so this class of drift cannot land silently.
- Documentation swept against the code: auto-detect recognizes the
  UTF-8 BOM (not only UTF-16), the Class A CRLF line-offset comment now
  describes the actual skip/push split, `EditorTab::generation` documents
  that successful saves also move it, `EditorTab::open_file` #Errors
  covers the load-in-progress rejection, the literal-search crate-doc
  sentence acknowledges the coexisting regex API, the Class B alignment
  doc block is attached to `align_class_b` instead of the UTF-8 window
  walker, BENCHMARKS.md names the real
  `session_layer_maintenance_status` group and the
  `small_document_open` / session-layer families and notes that
  `--seed-edit` inserts its text verbatim, and CI gained a
  `cargo check --workspace --all-targets` gate next to the README's
  quality-check list.
- The reverse-regex rightmost-match semantics are documented as a known
  inconsistency with the small-span forward path for self-overlapping
  patterns, with a pinned reproduction behind `#[ignore]` until the
  semantics are unified.

## Unreleased (0.9.0)

### Added

- The Qem-native edit buffer replacing `ropey::Rope` (the 0.9.0 rope
  replacement). It is a B-tree of flat UTF-8 chunks whose internal nodes
  carry per-child byte, char, and line-break summaries, so char/byte
  conversion, line lookup, insert, remove, and chunk iteration for
  streaming search are all O(log N) in the document size. It stores
  LF-normalized text - the same contract the document layer already
  enforced on the rope path - and takes over the small-file and
  untitled-document paths with unchanged public behavior. `ropey` is
  removed from `Cargo.toml`.
- A proptest parity suite drives the new buffer against a plain `String`
  oracle, re-checking every indexed accessor after each randomized
  insert/remove step; the saved regression case covers a last-child
  splice bug the suite caught before any unit test reached it.

- A documented threading model in the crate root: `FileStorage` is
  `Send + Sync`, while `Document`, `DocumentSession`, and `EditorTab` are
  `Send` single-thread-owner types - movable between threads but not
  shareable across threads, because their preserve-save error cache and
  background-job receivers carry no internal locking. Background open,
  save, and indexing work stays on Qem-owned worker threads and reports
  through the polling APIs. The auto-trait bounds are pinned by a
  compile-time test so a change to them cannot land silently.

- Undo/redo history for the rope edit buffer. Small UTF-8 documents (below
  the piece-table size gate) and untitled documents edit through the rope
  buffer and previously reported "nothing to undo" even right after edits;
  they now keep a bounded in-memory history (500 operations) covering
  insert, delete, backspace, delete-forward, and replace, with composite
  replaces undoing in a single step. The history survives same-encoding
  saves and is reset on reopen and on piece-table-to-rope promotion.
- `Document::try_undo_with_cursor` / `try_redo_with_cursor` returning a
  typed `UndoResult` that carries the restored caret position. The
  piece-tree snapshot path reports `cursor: None` (its snapshots carry no
  caret); rope history returns the position the edit started from.
- `DocumentSession::try_undo` / `try_redo` and `EditorTab::try_undo` /
  `try_redo`: guarded undo entry points that refuse while a background
  open/save is in flight instead of mutating the document through the
  `document_mut()` escape hatch underneath a running job.
- Mouse text selection in the egui demo editor: drag with the left button,
  Shift+click and Shift+arrow keys extend the selection, Escape collapses
  it, and Ctrl+C / Ctrl+X / Delete / Backspace / typing operate on the
  selection (falling back to the current search match when there is no
  selection). The selection renders as a highlight under the text and its
  geometry stays in integer line space, so it works at any depth of a
  huge file.
- Editing at any line of a huge storage-backed document. When an edit
  targets a line beyond every in-memory anchor, the piece table extends its
  annotated region from the on-disk line index: checkpoint-aligned pieces
  carry exact per-piece line-break counts, so the piece tree can navigate
  to the edit point without materializing per-line lengths for the span
  (a line-132,213,510 insert into the 50 GiB bench file completes in well
  under a second). The annotated region grows monotonically, edits in
  several distant regions coexist, reads outside it keep using the disk
  index, and annotated pieces persist through the session journal and
  streaming saves. Documents that can still be promoted to a rope keep the
  promotion fallback.
- The piece-table edit window is budget-based instead of a fixed
  4,000,000-line cap: the materialized line-length table is bounded by a
  128 MiB byte budget (16,777,216 rows on 64-bit targets), and
  storage-backed documents whose whole line table fits the budget get a
  fully indexed edit buffer, making every line editable (a
  7,043,298-line 1 GiB log file used to refuse edits past line
  4,000,000).

### Fixed

- Rope undo/redo history no longer produces a phantom redo at the
  history cap: after the 501st consecutive edit the just-applied entry
  was counted as redoable, so the first redo re-applied the last edit
  (duplicating text). The cap drain now derives the cursor from the
  drained list length, and the cap test asserts the invariant on every
  step instead of relying on the step-count parity that used to hide
  the flip.
- Piece-table undo→redo restores the known-region boundary on
  fully-indexed documents. Undo clamped `known_byte_len` down and redo
  never restored it, so a full-index piece table carried
  `known < total_len` until the next edit and tail reads through the
  boundary-clamped lookup paths could truncate the document tail. A
  regression test pins the state invariant (known == total after
  undo→redo) and the tail read.
- Corrupt `.qem.lineidx` sidecars no longer hang or mis-shape the disk
  index: checkpoint descent is bounded by a tree-depth limit, internal
  pages reject zero child page references (page id 0 aliases page 1
  through the on-disk offset math, so a zero reference would make the
  descent revisit the same page forever), page entry counters are
  bounded by the fixed page capacity, and allocated-range queries on
  Windows fail loudly instead of silently shrinking the scanned region
  when a range cannot be represented losslessly.
- The crate-root support contract no longer claims that non-UTF-8 opens
  materialize a rope-backed document: since 0.8.0 they run through the
  native per-encoding engines on the mmap fast path, and only the UTF-8
  BOM-strip and explicit transcode paths still decode through the rope
  pipeline. ROADMAP's current-target labels now point at the 0.9.0 line.
- A corrupt `.qem.editlog` sidecar whose raw (unchecksummed) header declares
  garbage history or add counters could drive huge allocations or
  out-of-bounds page reads while recovering a session. Page-chain lengths
  are now bounded by the sidecar file's actual capacity and per-page entry
  counts by the fixed page size, so recovery fails with a typed
  `InvalidData` / `UnexpectedEof` error and the existing recovery path
  drops the sidecar and reopens cleanly.
- Sidecar field decoding (`.qem.lineidx` and `.qem.editlog` headers and
  pages) no longer silently decodes truncated buffers as zeroed fields.
  Every fixed-layout field read goes through explicit typed readers that
  report truncation as an error; behavior for well-formed sidecars is
  unchanged.
- The egui demo rendered every panel and the editor twice per frame after a
  debugging leftover duplicated the whole update pass, doubling all widget
  IDs (egui painted "First use / Second use of widget ID" warnings over the
  window) and dropping the frame rate to single digits. The frame runs one
  pass again; per-frame stderr diagnostics were removed and remain
  available behind `QEM_DEBUG_FRAMES=1`. Release builds no longer allocate
  a console window at all, so a QuickEdit text selection in a console can
  never block the UI thread inside a stderr write again.
- Typing after undo/redo, a search jump, or a background job completing no
  longer deletes the text between a stale selection anchor and the caret:
  every programmatic caret move now collapses the selection, and only the
  selection-specific interactions (drag, Shift+click, Shift+arrows) manage
  the anchor.
- Editing beyond the bounded piece-table window reports the real limit
  ("editing is limited to the first N lines of this file (bounded edit
  window)") through `edit_capability_at` and the edit buffer paths, instead
  of claiming the document is "too large to materialize into a rope".
- The egui demo's Undo/Redo menu entries now actually undo and redo edits
  (previously a silent no-op on rope-backed documents), restore the caret,
  and route through the session guard.
- The Edit-menu shortcuts that never existed (Ctrl+C / Ctrl+X for the
  current search match) are wired up. Ctrl+Z / Ctrl+Y and the new clipboard
  chords are ignored while the search or rename text field owns the
  keyboard, so document undo no longer fires behind a text field.
- Keyboard input no longer reaches the document while the "Unsaved changes"
  or rename modal is open, and both modals close on Escape and backdrop
  clicks via `Modal::should_close()`.
- The demo Encoding menu entry "Save Copy As {encoding}" is renamed to
  "Save As {encoding}" to match what it does (the document rebinds to the
  new path when the save completes), same-encoding entries are no longer
  disabled, and the pre-save rejection now renders the typed error through
  its `Display` implementation.
- Clicking the editor while the search bar is open closes the search bar
  and drops the highlight instead of moving the caret while keystrokes keep
  landing in the search field; the search-bar Close button now clears the
  highlight like Escape does.
- large_file demo: "Close" asks for confirmation when the document has
  unsaved edits (including save-then-close), the caret column mapping
  measures the real monospace glyph width instead of a hardcoded 8.4 px,
  and the "Go to line" field is no longer reset while it is being edited.
- Demo PageUp/PageDown use the actual viewport height instead of a fixed
  30-line page, and background saves report "Saved" without yanking the
  viewport back to the caret.
- The demo editor viewport requests only the columns that fit the window
  and clips every row to its own rect, so long lines no longer paint past
  the window edge on huge files.
- Opening a file whose first line break sits far from the start (log files
  with long binary headers, zero-filled images) no longer scans the whole
  document synchronously: the line-ending probe is bounded at 16 MiB and
  falls back to LF beyond it. A 50 GiB no-newline file used to block the
  background load for minutes.
- Clamping a caret at column 0 no longer walks to the end of the line; on
  a file stored as one giant line that walk crossed the whole document on
  the UI thread.
- Editing a huge file no longer collapses the reported line count to the
  piece-table window. After the first edit the document moves into a
  bounded piece-table window (4096 lines on the 50 GiB bench file), and
  both the line count and the scroll range snapped to that window because
  the window count was reported as the exact document total. A window that
  does not cover the whole document now reports an honest estimate
  extrapolated from the window's own average line length, and the full
  scroll range survives the edit.

### Changed

- The `small_document_open` benchmark's raw-load baseline is now the
  Qem-native edit buffer bulk loader (through the hidden test-support
  surface) instead of the removed ropey bulk loader.
- The encoding-engine lookup cache recovers its mutex after poisoning
  instead of panicking on every later open, matching the storage-side
  stale-cleanup cache behavior.
- The `frontend_session` example only reports "saved copy" when the save
  actually ran; a clean document already bound to the target path reports
  that nothing was written.


## 0.8.1

### Fixed

- Encoding-aware text-column conversion. Column-based movement and
  search anchoring in non-UTF-8 documents now map text columns to byte
  offsets through the document's `EncodingEngine`
  (`byte_offset_for_text_col_with_engine`) instead of assuming UTF-8,
  keeping line/column navigation correct for UTF-16, single-byte, and
  multi-byte encodings.
- Open-ended search ranges over UTF-16 storage no longer loop forever
  when the file ends on a truncated (odd) final byte. The scanner now
  always advances even when the engine reports a zero-byte step at the
  tail.
- Reverse regex search no longer re-yields the same zero-width match
  at EOF after the start offset is clamped back by one text unit.

### Changed

- Reverse regex search over small byte-backed and rope ranges runs a
  single forward scan instead of determinizing a reverse DFA. The
  threshold is deliberately small, so larger ranges keep the cached
  DFA path.
- Search position scanning routes through the document's encoding
  engine instead of assuming UTF-8.
- README rewritten in a shorter form limited to documented behavior;
  architecture claims that did not match the implementation (B-tree
  line index, LRU page cache) were removed.
- Expanded encoding-engine test coverage: alignment, step, newline,
  endianness, reverse-regex property suites, and per-encoding cases.

### Added

- The egui demo opens documents through native file dialogs (`rfd`),
  with a reworked main window and an updated `large_file` viewer.
- Internal background-completion notifier so demo UI state refreshes
  when async open, save, and indexing work finishes.

## 0.8.0

### Added

- Encoding-aware byte movement engine that drives line and character
  navigation per encoding instead of assuming UTF-8 across the document
  layer. The `EncodingEngine` trait is implemented by `Utf8Engine`,
  `SingleByteEngine`, `Utf16Engine<Endian>`, and `MultiByteEngine`, each
  cached per encoding via a process-wide `OnceLock<Mutex<HashMap>>` so
  dispatch is one pointer compare on the hot path.
- `Document::encoding_engine()` accessor and a stored
  `encoding_engine` field that always reflects the document's current
  encoding. The hot mmap navigation methods
  (`mmap_line_start_offset_exact`, `mmap_byte_offset_for_position`,
  `mmap_advance_offset_by_text_units`, `read_text_from_mmap_backing`,
  and the mmap fallback in `line_len_chars`) route through the engine.
- `SingleByteEngine` for ASCII-superset single-byte encodings
  (`windows-1250`..`-1258`, `windows-874`, `ISO-8859-2`..`-16`,
  `KOI8-R`, `KOI8-U`, `IBM866`, `macintosh`, `x-mac-cyrillic`). Each
  character is one byte, line scanning reuses memchr, and CRLF collapses
  identically to UTF-8.
- `Utf16Engine<LittleEndian>` and `Utf16Engine<BigEndian>` for
  UTF-16 LE / BE. Newline finding looks for 2-byte aligned `0x0A 0x00` /
  `0x0D 0x00`. Char step is 2 bytes (4 for surrogate pairs). Edit
  boundaries always align on 2 bytes.
- `MultiByteEngine` for `Shift_JIS`, `GB18030`, and `EUC-KR`. Each kind
  has its own leading-byte detector and a false-positive-aware newline
  finder for `0x0A` / `0x0D`, since some trailing bytes can take those
  values.
- Native open dispatch for non-UTF-8 documents (`from_storage_class_a_native`
  and `from_storage_class_b_native`). Class A and Class B encodings now
  open mmap-backed without ever building a UTF-8 rope; viewport reads
  decode only the requested window via `encoding_rs`.
- Encoded edit path. `try_insert_text_at`, `try_replace_range`, and
  `try_delete_range` for non-UTF-8 documents go through the piece tree
  directly: the incoming `&str` is encoded via `encoding_rs`; characters
  not representable in the target encoding return a typed
  `UnrepresentableText` error before any state mutation.
- `Document::align_byte_offset(offset, AlignDirection)` and the
  supporting `bytes_for_alignment` helper. Every byte offset that
  reaches the edit / regex paths is clamped to a character boundary of
  the document's current encoding: UTF-8 walks back to the nearest char
  boundary, Class A is a no-op clamp, UTF-16 rounds to a 2-byte cell,
  and Class B walks forward from the nearest line anchor through the
  engine's `step`. This is the surface that prevents reverse search and
  multibyte edits from landing on the trail byte of a multi-byte
  character at an internal piece boundary.
- Reverse-DFA regex via `regex_automata::dfa::dense::Builder`. All
  `find_prev_regex*` paths (rope, mmap slice, piece-tree chunked) route
  through the reverse DFA and are bounded by a typed
  `RegexCompileError` on a 32 MiB size limit instead of panicking on
  pathological patterns.
- Updated `ROADMAP.md`: `0.8.0` ships the encoding-aware engine, native
  non-UTF-8 mmap path, and reverse-DFA regex; `0.9.0` replaces the
  `ropey` dependency with a Qem-native edited-buffer structure; `1.0.0`
  is the small-cleanup + public-API-freeze release.

### Changed

- `Document::encoding_engine` is now a stored field initialised by every
  constructor. Encoding mutations go through the internal
  `set_encoding_contract` helper which atomically updates encoding,
  origin, and engine, and invalidates the preserve-save cache.
- Non-UTF-8 documents never build a UTF-8 rope. The previous
  rope-decode fallback for legacy encodings is removed in favour of the
  native mmap + engine path.
- `find_prev_regex*` now uses the reverse-DFA backend instead of the
  chunked-from-end forward-scan fallback. Match coordinates and
  ordering follow leftmost-first reverse semantics.

### Removed

- The `find_prev_regex_via_forward_scan` chunked-from-end fallback and
  its supporting helpers (`find_prev_regex_in_bytes_bounded`,
  `find_prev_regex_in_byte_slice`, `find_prev_regex_in_rope_bounded`).
- The historical 8 MiB cap on the mmap regex slice path.

## 0.7.1

### Fixed

- Made `cargo clippy --all-targets --all-features -- -D warnings` pass on the
  newer clippy lint set used in CI by collapsing a nested `if` inside a
  `match` arm in `trailing_mmap_line_ranges`.
- Stabilized `document_session_large_async_open_reports_pending_exact_line_count_before_index_finishes`
  against background-indexing races on fast CI runners. The test no longer
  fails when the background line index finishes before the assertion runs;
  it now accepts both the still-pending and already-exact paths and verifies
  the matching status invariants in either case.

## 0.7.0

`0.7.0` is the integration release. There are no breaking API changes
between `0.6.x` and `0.7.0`.

### Fixed

- Eliminated a degenerate `find_prev` / `find_prev_query` slow path on huge
  mmap-backed files. Calling reverse literal search with an out-of-range
  before-position such as `TextPosition::new(usize::MAX, usize::MAX)` (the
  natural way to mean "from the end of the document") used to walk hundreds
  of millions of memchr-anchored line starts before answering. On a real
  50 GiB file that path now returns in microseconds instead of timing out
  past the 10-minute mark. Out-of-range typed positions on huge mmap files
  now short-circuit through the highest indexed line anchor instead of doing
  a multi-gigabyte byte-by-byte rescan.

### Added

- Added a workspace `qem-egui-demo` integration target with separate minimal
  and large-file-oriented `egui` frontends so real GUI integration can evolve
  without pulling GUI dependencies into the core crate.
- Added example smoke coverage in CI for the current official CLI/frontend
  examples.
- Added `ROADMAP.md` and `MIGRATION-0.7.md` release-planning documents that
  describe the path from `0.7.0` integration through `0.8.0` (regex + fast
  search + encoding stabilization), `0.9.0` (stability hardening), and
  `1.0.0` (public API freeze).
- Added `PERF-BASELINE-0.7.md` plus expanded perf-matrix tooling so the
  repository now records a seed real-file baseline with edit/save and coarse
  working-set numbers.
- Added `is_line_count_pending()` passthrough helpers on the session/tab-facing
  status surface so frontends no longer need to peel back into raw
  `DocumentStatus` just to check whether exact line count is still pending.
- Added explicit `DocumentSession` scenario coverage for CRLF-preserving
  open/edit/save flows and very long line viewport/edit/save flows.

### Changed

- Repositioned the public roadmap around `0.7.0` as an integration release,
  followed by `0.8.0` (regex + fast search + encoding stabilization),
  `0.9.0` (stability hardening), and `1.0.0` (public API freeze).
- Rewrote the public README and crate-level docs toward a clearer integration
  contract around `Document`, `DocumentSession`, `EditorTab`, huge-file edit
  limits, sidecars, and frontend-visible async semantics.

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
