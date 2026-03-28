//! Qem is a cross-platform text engine for Rust applications that need fast
//! file-backed reads, incremental line indexing, and responsive editing for
//! very large documents.
//!
//! At its core, Qem combines mmap-backed access, sparse on-disk line indexes,
//! and mutable rope or piece-table edit buffers so large-file workflows remain
//! responsive without requiring full materialization up front.
//!
//! `Qem` is the project name, not an expanded acronym.
//!
//! # Picking the Right Layer
//!
//! - Use [`Document`] when your application already owns tab state, session
//!   state, and background-job orchestration.
//! - Use [`DocumentSession`] when you want a backend-first session wrapper with
//!   generation tracking, async open/save helpers, forwarded viewport/edit
//!   helpers, status snapshots, and progress polling while still owning cursor
//!   and GUI behavior in your app.
//! - Use [`EditorTab`] when you additionally want convenience cursor state on
//!   top of the same session machinery.
//! - Most GUI frontends render visible rows through [`Document::read_viewport`]
//!   or [`DocumentSession::read_viewport`].
//! - Legacy compatibility wrappers that silently swallow edit errors or return
//!   raw progress tuples remain available for migration only, but they are
//!   deprecated and hidden from the main rustdoc surface in favor of the
//!   typed/session-first APIs.
//!
//! # Recommended Entry Path
//!
//! For most frontend integrations, start with [`DocumentSession`].
//!
//! - Use [`ViewportRequest`], [`TextSelection`], [`TextRange`], and
//!   [`SearchMatch`] as the main typed values passed between your app state and
//!   Qem.
//! - Prefer bounded reads such as [`Document::read_viewport`],
//!   [`Document::read_text`], and [`Document::read_selection`] over
//!   full-document materialization through [`Document::text_lossy`],
//!   [`DocumentSession::text`], or [`EditorTab::text`] in normal UI loops.
//! - Prefer the typed session-facing surface:
//!   [`DocumentSession::loading_state`], [`DocumentSession::loading_phase`],
//!   [`DocumentSession::save_state`], [`DocumentSession::background_issue`],
//!   [`DocumentSession::take_background_issue`], [`DocumentSession::close_pending`],
//!   and the typed `try_*` edit helpers.
//! - Treat [`DocumentSession::document_mut`], [`DocumentSession::set_path`],
//!   unconditional [`Document::compact_piece_table`], and the full-text helpers
//!   as advanced escape hatches for callers that intentionally manage those
//!   trade-offs themselves.
//! - Reach for raw [`Document`] when your application deliberately owns tab
//!   state, background-job orchestration, and save lifecycle itself.
//!
//! # Frontend Integration Recipe
//!
//! A typical GUI or TUI loop looks like this:
//!
//! 1. Open a file with [`Document::open`] or [`DocumentSession::open_file_async`].
//! 2. Poll [`DocumentSession::poll_background_job`] and cache
//!    [`DocumentSession::status`] or the more focused
//!    [`DocumentSession::loading_state`], [`DocumentSession::loading_phase`],
//!    [`DocumentSession::save_state`], [`DocumentSession::background_issue`],
//!    [`DocumentSession::take_background_issue`], [`DocumentSession::close_pending`], and
//!    [`Document::indexing_state`] values from the app loop. Load progress
//!    covers the asynchronous open path itself; once the document is ready,
//!    continued line indexing is reported separately through
//!    [`Document::indexing_state`]. If a background job fails or is
//!    intentionally discarded as stale, [`DocumentSession::background_issue`]
//!    keeps the last typed problem available even after the current
//!    [`BackgroundActivity`] returns to idle. If [`DocumentSession::close_file`]
//!    was requested while the session was busy, [`DocumentSession::close_pending`]
//!    exposes that deferred-close state until the active worker finishes.
//!    Call [`DocumentSession::take_background_issue`] after surfacing that
//!    problem to clear the retained issue explicitly.
//! 3. Size scrollbars with [`Document::display_line_count`] while indexing is
//!    still in progress.
//! 4. Render only the visible rows with [`Document::read_viewport`].
//! 5. Query [`Document::edit_capability_at`] when you want to disable editing
//!    for positions that would exceed huge-file safety limits.
//!    Avoid full-text materialization in hot paths: [`Document::text_lossy`],
//!    [`DocumentSession::text`], and [`EditorTab::text`] build a fresh
//!    `String` for the entire current document.
//! 6. Wait for [`DocumentSession::poll_background_job`] to finish before
//!    applying session/tab edit helpers. While a background open/save is
//!    active, those helpers return [`DocumentError::EditUnsupported`];
//!    [`DocumentSession::document_mut`] is an escape hatch for callers that
//!    coordinate that synchronization themselves. If it is used while busy,
//!    the in-flight worker result is discarded on the next poll instead of
//!    being applied over newer raw document changes. The same stale-result
//!    rule applies to [`DocumentSession::set_path`] while busy. If a deferred
//!    close was pending at the time, that new session state change also
//!    cancels the deferred close.
//! 7. If the user closes a session/tab while it is still busy, keep polling:
//!    [`DocumentSession::close_file`] defers the actual close until the active
//!    background open/save completes instead of silently dropping that result.
//!    Failed background saves cancel that deferred close so the dirty document
//!    stays available for retry or explicit discard.
//! 8. Treat the active [`DocumentSession::loading_state`] or
//!    [`DocumentSession::save_state`] path as authoritative while busy. Later
//!    async open/save requests are rejected until that first worker result is
//!    polled and applied. The actual file write runs in the background, but
//!    `save_async` still snapshots the current document before the worker
//!    starts, so very large edited buffers may make the call itself noticeable.
//! 9. Keep GUI selections as [`TextSelection`] values, read them through
//!    [`Document::read_selection`], convert them through
//!    [`Document::text_range_for_selection`], or edit them directly with
//!    [`Document::try_replace_selection`], [`Document::try_delete_selection`],
//!    [`Document::try_cut_selection`], [`Document::try_backspace_selection`],
//!    or [`Document::try_delete_forward_selection`]. Literal search is exposed
//!    through [`Document::find_next`], [`Document::find_prev`],
//!    [`Document::find_all`], the compiled-query variants such as
//!    [`Document::find_all_query`], the bounded range/position helpers, and
//!    the session/tab wrappers as typed [`SearchMatch`] values.
//! 10. For long-lived edited piece-table documents, prefer
//!     [`Document::maintenance_status`] or
//!     [`Document::maintenance_status_with_policy`] (or the session/tab
//!     wrappers) when the caller wants one explicit maintenance snapshot.
//!     [`Document::maintenance_action`] and
//!     [`DocumentMaintenanceStatus::recommended_action`] provide a lighter
//!     high-level decision when the frontend only needs to know whether to do
//!     idle maintenance now or wait for an explicit boundary.
//!     Run [`Document::run_idle_compaction`] or
//!     [`Document::run_idle_compaction_with_policy`] during idle time for
//!     deferred maintenance. Keep
//!     [`Document::compact_piece_table`] for explicit maintenance actions.
//! 11. Then save through [`Document::save_to`],
//!     [`DocumentSession::save_async`], or [`DocumentSession::save_as_async`].
//!
//! ```no_run
//! # #[cfg(not(feature = "editor"))]
//! # fn main() {}
//! # #[cfg(feature = "editor")]
//! use qem::{DocumentSession, ViewportRequest};
//! use std::path::PathBuf;
//!
//! # #[cfg(feature = "editor")]
//! fn pump_frame(session: &mut DocumentSession, path: PathBuf) -> Result<(), Box<dyn std::error::Error>> {
//!     if session.current_path().is_none() && !session.is_busy() {
//!         session.open_file_async(path)?;
//!     }
//!
//!     if let Some(result) = session.poll_background_job() {
//!         result?;
//!     }
//!
//!     let status = session.status();
//!
//!     if let Some(progress) = status.indexing_state() {
//!         println!(
//!             "indexing: {}/{} bytes",
//!             progress.completed_bytes(),
//!             progress.total_bytes()
//!         );
//!     }
//!
//!     let viewport = session.read_viewport(ViewportRequest::new(0, 40).with_columns(0, 160));
//!     println!("scroll rows: {}", status.display_line_count());
//!     println!("visible rows this frame: {}", viewport.len());
//!
//!     Ok(())
//! }
//! #
//! # #[cfg(feature = "editor")]
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! #     let mut session = DocumentSession::new();
//! #     let path = PathBuf::from("huge.log");
//! #     pump_frame(&mut session, path)?;
//! #     Ok(())
//! # }
//! ```
//!
//! # Cargo Features
//!
//! - `editor` (default): enables the backend-first session wrapper
//!   [`DocumentSession`], the convenience cursor wrapper [`EditorTab`], and
//!   the related progress/save helper types.
//!
//! # Current Contract
//!
//! - UTF-8 and ASCII text are the primary stable fast path: open, viewport
//!   reads, edits, undo/redo, and saves are supported without transcoding.
//! - Explicit encoding open/save is available through
//!   [`Document::open_with_encoding`] and [`Document::save_to_with_encoding`]
//!   plus the session/tab wrappers. For convenience, BOM-backed UTF-16 files
//!   can also use [`Document::open_with_auto_encoding_detection`]. For a more
//!   extensible contract, the same flows are also exposed through
//!   [`DocumentOpenOptions`], [`OpenEncodingPolicy`], and
//!   [`DocumentSaveOptions`].
//! - Auto-detect open currently recognizes BOM-backed UTF-16 files and
//!   otherwise keeps the normal UTF-8/ASCII fast path. Callers that already
//!   know a likely legacy fallback can opt into "detect first, otherwise
//!   reinterpret as X" through [`DocumentOpenOptions`] and the session/tab
//!   convenience wrappers.
//! - Non-UTF8 opens currently materialize into a rope-backed document instead
//!   of using the mmap fast path. Very large legacy-encoded files may therefore
//!   still be rejected until the broader encoding contract lands in a later
//!   release.
//! - Preserve-save for some decoded encodings can still return a typed
//!   [`DocumentError::Encoding`] with a structured
//!   [`DocumentEncodingErrorKind`] until a broader persistence contract lands.
//!   [`Document::decoding_had_errors`] means Qem has already seen malformed
//!   source bytes, but preserve-save is only rejected when the write would
//!   materialize lossy-decoded text. Raw mmap/piece-table preserve can still
//!   remain valid, while rope-backed legacy opens and UTF-8 after lossy
//!   materialization/edit correctly fail with
//!   [`DocumentEncodingErrorKind::LossyDecodedPreserve`]. Frontends can
//!   preflight both preserve and explicit conversion paths through
//!   [`Document::preserve_save_error`], [`Document::save_error_for_options`],
//!   and the matching session/tab wrappers before attempting the write.
//!   Callers can already convert to a supported target through
//!   [`DocumentSaveOptions`] or [`Document::save_to_with_encoding`].
//! - Huge files are supported for mmap-backed reads, viewport rendering, line
//!   counting, and background indexing without full materialization. Editing
//!   may be rejected when it would require rope materialization beyond the
//!   built-in safety limits.
//! - Typed positions, ranges, and viewport columns use document text units.
//!   For UTF-8 text, line-local columns count Unicode scalar values rather
//!   than grapheme clusters or display cells. Stored CRLF still counts as one
//!   text unit between lines.
//! - Internal `.qem.lineidx` and `.qem.editlog` sidecars are validated against
//!   source file length, modification time, and a sampled content fingerprint.
//!   Their formats are internal cache/durability details rather than stable
//!   interchange formats, so Qem may rebuild, discard, or version-bump them
//!   across releases.
//! - Session-facing async-open state is reported through byte progress plus an
//!   explicit [`LoadPhase`] so frontends can distinguish "open is still being
//!   prepared" from "the document is ready but background indexing continues".
//! - Session-facing background failures and stale-result discards are retained
//!   as typed [`BackgroundIssue`] values so frontends can keep showing the most
//!   recent async-open/save problem after background activity has gone idle.
//!   Call [`DocumentSession::take_background_issue`] or
//!   [`EditorTab::take_background_issue`] when your app wants to acknowledge
//!   and clear that retained issue explicitly.
//! - Deferred closes are part of the public session contract:
//!   [`DocumentSession::close_pending`] and the corresponding status snapshot
//!   expose when `close_file()` is waiting for an in-flight background job to
//!   finish before the document can actually disappear.

pub mod document;
#[cfg(feature = "editor")]
pub mod editor;
pub mod index;
pub(crate) mod piece_tree;
pub(crate) mod source_identity;
pub mod storage;

pub use document::{
    ByteProgress, CompactionPolicy, CompactionRecommendation, CompactionUrgency, CutResult,
    Document, DocumentBacking, DocumentEncoding, DocumentEncodingErrorKind, DocumentEncodingOrigin,
    DocumentError, DocumentMaintenanceStatus, DocumentOpenOptions, DocumentSaveOptions,
    DocumentStatus, EditCapability, EditResult, FragmentationStats, IdleCompactionOutcome,
    LineCount, LineEnding, LineSlice, LiteralSearchIter, LiteralSearchQuery, MaintenanceAction,
    OpenEncodingPolicy, SaveEncodingPolicy, SearchMatch, TextPosition, TextRange, TextSelection,
    TextSlice, Viewport, ViewportRequest, ViewportRow,
};
#[cfg(feature = "editor")]
pub use editor::{
    BackgroundActivity, BackgroundIssue, BackgroundIssueKind, CursorPosition, DocumentSession,
    DocumentSessionStatus, EditorTab, EditorTabStatus, FileProgress, LoadPhase, SaveError,
};
pub use storage::{FileStorage, StorageOpenError};
