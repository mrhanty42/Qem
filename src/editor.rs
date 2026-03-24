use crate::document::{OpenProgressPhase, SaveCompletion};
use crate::{
    ByteProgress, CutResult, Document, DocumentBacking, DocumentError, DocumentStatus,
    EditCapability, EditResult, LineCount, LineEnding, LiteralSearchQuery, SearchMatch,
    TextPosition, TextRange, TextSelection, TextSlice, Viewport, ViewportRequest,
};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;

mod core;
mod session;
mod tab;
mod types;

pub use session::DocumentSession;
pub use tab::EditorTab;
pub use types::{
    BackgroundActivity, BackgroundIssue, BackgroundIssueKind, CursorPosition,
    DocumentSessionStatus, EditorTabStatus, FileProgress, LoadPhase, SaveError,
};

pub(crate) use core::SessionCore;

#[cfg(test)]
mod tests;
