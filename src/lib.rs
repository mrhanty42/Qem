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
//! - Use [`Document`] when your application already owns tab state, cursor
//!   state, and background-job orchestration.
//! - Use [`EditorTab`] when you want a lightweight session wrapper with cursor
//!   state, generation tracking, async open/save helpers, and progress polling.
//! - Most GUI frontends keep [`EditorTab`] at the tab/session layer and render
//!   visible rows through [`Document::line_slices`].
//!
//! # Frontend Integration Recipe
//!
//! A typical GUI or TUI loop looks like this:
//!
//! 1. Open a file with [`Document::open`] or [`EditorTab::open_file_async`].
//! 2. Poll [`EditorTab::poll_background_job`], [`EditorTab::loading_progress`],
//!    [`EditorTab::save_progress`], and [`Document::indexing_progress`] from
//!    the app loop.
//! 3. Size scrollbars with [`Document::display_line_count`] while indexing is
//!    still in progress.
//! 4. Render only the visible rows with [`Document::line_slices`].
//! 5. Apply edits through [`Document`] mutation APIs and save through
//!    [`Document::save_to`], [`EditorTab::save_async`], or
//!    [`EditorTab::save_as_async`].
//!
//! ```no_run
//! use qem::EditorTab;
//! use std::path::PathBuf;
//!
//! fn pump_frame(tab: &mut EditorTab, path: PathBuf) -> Result<(), Box<dyn std::error::Error>> {
//!     if tab.current_path().is_none() && !tab.is_busy() {
//!         tab.open_file_async(path)?;
//!     }
//!
//!     if let Some(result) = tab.poll_background_job() {
//!         result?;
//!     }
//!
//!     if let Some((indexed_bytes, total_bytes)) = tab.indexing_progress() {
//!         println!("indexing: {indexed_bytes}/{total_bytes} bytes");
//!     }
//!
//!     let total_rows = tab.document().display_line_count();
//!     let visible_rows = tab.document().line_slices(0, 40, 0, 160);
//!     println!("scroll rows: {total_rows}");
//!     println!("visible rows this frame: {}", visible_rows.len());
//!
//!     Ok(())
//! }
//! ```
//!
//! # Cargo Features
//!
//! - `editor` (default): enables the lightweight editor/session wrapper
//!   [`EditorTab`] together with cursor and save helper types.

pub mod document;
#[cfg(feature = "editor")]
pub mod editor;
pub mod index;
pub(crate) mod piece_tree;
pub mod storage;

pub use document::{Document, DocumentError, LineCount, LineEnding, LineSlice};
#[cfg(feature = "editor")]
pub use editor::{CursorPosition, EditorTab, SaveError};
pub use storage::{FileStorage, StorageOpenError};
