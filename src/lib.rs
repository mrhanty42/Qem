//! Qem is a cross-platform text engine for Rust applications that need fast
//! file-backed reads, incremental line indexing, and responsive editing for
//! very large documents.
//!
//! At its core, Qem combines mmap-backed access, sparse on-disk line indexes,
//! and mutable rope or piece-table edit buffers so large-file workflows remain
//! responsive without requiring full materialization up front.
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

pub use document::{Document, DocumentError, LineSlice};
#[cfg(feature = "editor")]
pub use editor::{CursorPosition, EditorTab, SaveError};
pub use storage::{FileStorage, StorageOpenError};
