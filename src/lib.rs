//! Qem is a cross-platform text engine for Rust applications that need fast
//! file-backed reads, incremental line indexing, and responsive editing for
//! very large documents.
//!
//! At its core, Qem combines mmap-backed access, sparse on-disk line indexes,
//! and mutable rope or piece-table edit buffers so large-file workflows remain
//! responsive without requiring full materialization up front.
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
//!   raw progress tuples remain available for now, but they are deprecated in
//!   favor of the typed/session-first APIs.
//!
//! # Frontend Integration Recipe
//!
//! A typical GUI or TUI loop looks like this:
//!
//! 1. Open a file with [`Document::open`] or [`DocumentSession::open_file_async`].
//! 2. Poll [`DocumentSession::poll_background_job`] and cache
//!    [`DocumentSession::status`] or the more focused
//!    [`DocumentSession::loading_state`], [`DocumentSession::save_state`], and
//!    [`Document::indexing_state`] values from the app loop.
//! 3. Size scrollbars with [`Document::display_line_count`] while indexing is
//!    still in progress.
//! 4. Render only the visible rows with [`Document::read_viewport`].
//! 5. Query [`Document::edit_capability_at`] when you want to disable editing
//!    for positions that would exceed huge-file safety limits.
//! 6. Keep GUI selections as [`TextSelection`] values, read them through
//!    [`Document::read_selection`], convert them through
//!    [`Document::text_range_for_selection`], or edit them directly with
//!    [`Document::try_replace_selection`], [`Document::try_delete_selection`],
//!    [`Document::try_cut_selection`], [`Document::try_backspace_selection`],
//!    or [`Document::try_delete_forward_selection`], then save through
//!    [`Document::save_to`], [`DocumentSession::save_async`], or
//!    [`DocumentSession::save_as_async`].
//!
//! ```no_run
//! use qem::{DocumentSession, ViewportRequest};
//! use std::path::PathBuf;
//!
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
//! ```
//!
//! # Cargo Features
//!
//! - `editor` (default): enables the backend-first session wrapper
//!   [`DocumentSession`], the convenience cursor wrapper [`EditorTab`], and
//!   the related progress/save helper types.

pub mod document;
#[cfg(feature = "editor")]
pub mod editor;
pub mod index;
pub(crate) mod piece_tree;
pub mod storage;

pub use document::{
    ByteProgress, CutResult, Document, DocumentBacking, DocumentError, DocumentStatus,
    EditCapability, EditResult, LineCount, LineEnding, LineSlice, TextPosition, TextRange,
    TextSelection, TextSlice, Viewport, ViewportRequest, ViewportRow,
};
#[cfg(feature = "editor")]
pub use editor::{
    BackgroundActivity, CursorPosition, DocumentSession, DocumentSessionStatus, EditorTab,
    EditorTabStatus, FileProgress, SaveError,
};
pub use storage::{FileStorage, StorageOpenError};
