use crate::document::DocumentEncodingOrigin;
use crate::sidecar::{le_u32, le_u64};
use crate::source_identity::{sampled_content_fingerprint, sampled_file_fingerprint};
use crate::storage::FileStorage;
use crc32fast::Hasher;
use std::collections::{HashMap, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_LEAF_PIECES: usize = 64;
const MAX_INTERNAL_CHILDREN: usize = 64;
// `.qem.editlog` is an internal append-only sidecar format. The header at
// offset 0 is the authoritative commit record for the latest durable session
// snapshot. Older pages may remain in the file after later flushes, but they
// become unreachable once the header points at newer history/add page chains.
//
// Compatibility policy:
// - source identity must match `(len, mtime_ns, sampled fingerprint)` or the
//   sidecar is treated as stale and ignored
// - version or page-size mismatches are rejected as unsupported format changes
// - the format is internal and may change across Qem releases; callers should
//   treat `.qem.editlog` as rebuildable implementation detail rather than a
//   stable interchange format
const EDITLOG_MAGIC: &[u8; 8] = b"QEMEDT1\0";
const EDITLOG_VERSION: u32 = 2;
const EDITLOG_PAGE_SIZE: usize = 4096;
const EDITLOG_HEADER_BYTES: usize = 128;
const EDITLOG_PAGE_HEADER_BYTES: usize = 24;
const EDITLOG_CACHE_PAGES: usize = 1024;
/// Bound on pages kept resident while editlog persistence is down:
/// each is an Arc'd page that only this map holds (see `alloc_page`).
const RESIDENT_PAGES_FAILED_PERSISTENCE_CAP: usize = 4096;
const LEAF_ENTRY_BYTES: usize = 32;
const INTERNAL_ENTRY_BYTES: usize = 24;
const HISTORY_ENTRY_BYTES: usize = 32;
const DEFAULT_FRAGMENTATION_SMALL_PIECE_BYTES: usize = 1024;
const EDITLOG_FLAG_FULL_INDEX: u64 = 1;
const EDITLOG_FLAG_HAS_ENCODING_ORIGIN: u64 = 1 << 1;
const EDITLOG_FLAG_DECODING_HAD_ERRORS: u64 = 1 << 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PieceSource {
    Original,
    Add,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Piece {
    pub(crate) src: PieceSource,
    pub(crate) start: usize,
    pub(crate) len: usize,
    pub(crate) line_breaks: usize,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FragmentationStats {
    pub(crate) piece_count: usize,
    pub(crate) total_bytes: usize,
    pub(crate) smallest_piece_bytes: usize,
    pub(crate) largest_piece_bytes: usize,
    pub(crate) small_piece_threshold_bytes: usize,
    pub(crate) small_piece_count: usize,
    pub(crate) small_piece_bytes: usize,
}

impl FragmentationStats {
    pub fn piece_count(self) -> usize {
        self.piece_count
    }

    pub fn total_bytes(self) -> usize {
        self.total_bytes
    }

    pub fn smallest_piece_bytes(self) -> usize {
        self.smallest_piece_bytes
    }

    pub fn largest_piece_bytes(self) -> usize {
        self.largest_piece_bytes
    }

    pub fn small_piece_threshold_bytes(self) -> usize {
        self.small_piece_threshold_bytes
    }

    pub fn small_piece_count(self) -> usize {
        self.small_piece_count
    }

    pub fn small_piece_bytes(self) -> usize {
        self.small_piece_bytes
    }

    pub fn average_piece_bytes(self) -> f64 {
        if self.piece_count == 0 {
            0.0
        } else {
            self.total_bytes as f64 / self.piece_count as f64
        }
    }

    pub fn fragmentation_ratio(self) -> f64 {
        if self.piece_count == 0 {
            0.0
        } else {
            self.small_piece_count as f64 / self.piece_count as f64
        }
    }

    fn observe_piece(&mut self, piece: Piece) {
        self.piece_count = self.piece_count.saturating_add(1);
        self.total_bytes = self.total_bytes.saturating_add(piece.len);
        if self.piece_count == 1 {
            self.smallest_piece_bytes = piece.len;
            self.largest_piece_bytes = piece.len;
        } else {
            self.smallest_piece_bytes = self.smallest_piece_bytes.min(piece.len);
            self.largest_piece_bytes = self.largest_piece_bytes.max(piece.len);
        }
        if piece.len <= self.small_piece_threshold_bytes {
            self.small_piece_count = self.small_piece_count.saturating_add(1);
            self.small_piece_bytes = self.small_piece_bytes.saturating_add(piece.len);
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ChildRef {
    page_id: u64,
    total_bytes: usize,
    total_line_breaks: usize,
}

#[derive(Clone, Copy, Debug, Default)]
// Session metadata persisted alongside the piece-tree root history.
//
// These fields are recovery hints for reopened sessions:
// - `known_byte_len` is the logical document length represented by the current
//   committed root
// - `full_index` tells recovery whether the piece-table snapshot already had a
//   complete line index when it was flushed
pub(crate) struct SessionMeta {
    pub(crate) known_byte_len: usize,
    pub(crate) full_index: bool,
    pub(crate) encoding_origin: Option<DocumentEncodingOrigin>,
    pub(crate) decoding_had_errors: bool,
}

fn encode_encoding_origin(origin: DocumentEncodingOrigin) -> u32 {
    match origin {
        DocumentEncodingOrigin::NewDocument => 1,
        DocumentEncodingOrigin::Utf8FastPath => 2,
        DocumentEncodingOrigin::AutoDetected => 3,
        DocumentEncodingOrigin::AutoDetectFallbackUtf8 => 4,
        DocumentEncodingOrigin::AutoDetectFallbackOverride => 5,
        DocumentEncodingOrigin::ExplicitReinterpretation => 6,
        DocumentEncodingOrigin::SaveConversion => 7,
    }
}

fn decode_encoding_origin(raw: u32) -> Option<DocumentEncodingOrigin> {
    match raw {
        1 => Some(DocumentEncodingOrigin::NewDocument),
        2 => Some(DocumentEncodingOrigin::Utf8FastPath),
        3 => Some(DocumentEncodingOrigin::AutoDetected),
        4 => Some(DocumentEncodingOrigin::AutoDetectFallbackUtf8),
        5 => Some(DocumentEncodingOrigin::AutoDetectFallbackOverride),
        6 => Some(DocumentEncodingOrigin::ExplicitReinterpretation),
        7 => Some(DocumentEncodingOrigin::SaveConversion),
        _ => None,
    }
}

#[derive(Clone, Debug)]
enum PiecePage {
    Internal { children: Vec<ChildRef> },
    Leaf { pieces: Vec<Piece> },
}

#[derive(Debug, Default)]
struct InMemoryPageStore {
    pages: HashMap<u64, Arc<PiecePage>>,
    next_page_id: u64,
}

#[derive(Default)]
struct DiskPageStore {
    path: PathBuf,
    source: SourceMetadata,
    file: Option<File>,
    cache: Mutex<PageCache>,
    resident_pages: HashMap<u64, Arc<PiecePage>>,
    next_page_id: u64,
    persistence_failed: bool,
}

impl std::fmt::Debug for DiskPageStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiskPageStore")
            .field("path", &self.path)
            .field("next_page_id", &self.next_page_id)
            .field(
                "cache_len",
                &self
                    .cache
                    .lock()
                    .map(|cache| cache.pages.len())
                    .unwrap_or(0),
            )
            .field("resident_pages", &self.resident_pages.len())
            .field("persistence_failed", &self.persistence_failed)
            .finish()
    }
}

#[derive(Debug)]
enum PageStore {
    InMemory(InMemoryPageStore),
    Disk(DiskPageStore),
}

impl Default for PageStore {
    fn default() -> Self {
        Self::InMemory(InMemoryPageStore::default())
    }
}

#[derive(Debug, Default)]
struct PageCache {
    pages: HashMap<u64, Arc<PiecePage>>,
    order: VecDeque<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PageKind {
    Internal = 1,
    Leaf = 2,
    History = 3,
    Add = 4,
}

#[derive(Clone, Copy, Debug, Default)]
// Authoritative commit record for one durable `.qem.editlog` snapshot.
//
// Invariants:
// - `page_count` is the number of append-only pages that exist after the latest
//   committed flush
// - `history_*` describes the durable undo/redo root history chain
// - `add_*` describes the durable add-buffer bytes referenced by current pieces
// - `source_*` must match the source file identity at reopen time or recovery is
//   rejected as stale
// - `flags` is an append-only bitfield for recovery hints
struct EditLogHeader {
    page_count: u64,
    source_len: u64,
    source_mtime_ns: u64,
    source_fingerprint: u64,
    history_first_page_id: u64,
    history_page_count: u64,
    history_len: u64,
    history_index: u64,
    add_first_page_id: u64,
    add_page_count: u64,
    add_len: u64,
    known_byte_len: u64,
    flags: u64,
    encoding_origin: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
struct SourceMetadata {
    len: u64,
    mtime_ns: u64,
    fingerprint: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RootEntry {
    root: Option<ChildRef>,
}

type OpenDiskSession = Option<(DiskPageStore, Vec<RootEntry>, usize, Vec<u8>, SessionMeta)>;

#[derive(Debug)]
pub(crate) struct PieceTree {
    store: PageStore,
    root: Option<ChildRef>,
    history: Vec<RootEntry>,
    known_history: Vec<(usize, bool)>,
    history_index: usize,
    history_batch_depth: usize,
    history_batch_dirty: bool,
}

impl PieceTree {
    pub(crate) fn new() -> Self {
        Self {
            store: PageStore::default(),
            root: None,
            known_history: Vec::new(),
            history: vec![RootEntry { root: None }],
            history_index: 0,
            history_batch_depth: 0,
            history_batch_dirty: false,
        }
    }

    pub(crate) fn from_pieces(pieces: Vec<Piece>) -> Self {
        let mut this = Self::new();
        this.root = this.build_tree_from_pieces(filter_zero_len(pieces));
        this.history = vec![RootEntry { root: this.root }];
        this.known_history = vec![(0, false)];
        this.history_index = 0;
        this
    }

    pub(crate) fn from_pieces_disk(source_path: &Path, pieces: Vec<Piece>) -> io::Result<Self> {
        let mut this = Self {
            store: PageStore::Disk(DiskPageStore::create(source_path)?),
            root: None,
            history: Vec::new(),
            known_history: Vec::new(),
            history_index: 0,
            history_batch_depth: 0,
            history_batch_dirty: false,
        };
        this.root = this.build_tree_from_pieces(filter_zero_len(pieces));
        this.history = vec![RootEntry { root: this.root }];
        this.known_history = vec![(0, false)];
        this.history_index = 0;
        this.flush_session(&[], SessionMeta::default())?;
        Ok(this)
    }

    pub(crate) fn try_open_disk_session(
        source_path: &Path,
        storage: &FileStorage,
    ) -> io::Result<Option<(Self, Vec<u8>, SessionMeta)>> {
        let source = source_metadata_with_bytes(source_path, storage.bytes())?;
        let Some((store, history, history_index, add, meta)) =
            DiskPageStore::open(source_path, source)?
        else {
            return Ok(None);
        };
        let root = history
            .get(history_index)
            .copied()
            .unwrap_or(RootEntry { root: None })
            .root;
        let known_history = vec![(meta.known_byte_len, meta.full_index); history.len()];
        Ok(Some((
            Self {
                store: PageStore::Disk(store),
                root,
                history,
                known_history,
                history_index,
                history_batch_depth: 0,
                history_batch_dirty: false,
            },
            add,
            meta,
        )))
    }

    #[cfg(test)]
    pub(crate) fn open_disk(source_path: &Path) -> io::Result<Self> {
        let storage = FileStorage::open(source_path).map_err(|err| err.into_io_error())?;
        let Some((tree, _, _)) = Self::try_open_disk_session(source_path, &storage)? else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "no persisted editlog session found",
            ));
        };
        Ok(tree)
    }

    #[cfg(test)]
    pub(crate) fn poison_persistence_for_test(&mut self) {
        if let PageStore::Disk(store) = &mut self.store {
            store.file = None;
        }
    }

    pub(crate) fn total_len(&self) -> usize {
        self.root.map(|root| root.total_bytes).unwrap_or(0)
    }

    pub(crate) fn total_line_breaks(&self) -> usize {
        self.root.map(|root| root.total_line_breaks).unwrap_or(0)
    }

    pub(crate) fn to_vec(&self) -> Vec<Piece> {
        let mut out = Vec::new();
        if let Some(root) = self.root {
            self.collect_pieces(root, &mut out);
        }
        out
    }

    pub(crate) fn fragmentation_stats(&self) -> FragmentationStats {
        self.fragmentation_stats_with_threshold(DEFAULT_FRAGMENTATION_SMALL_PIECE_BYTES)
    }

    pub(crate) fn fragmentation_stats_with_threshold(
        &self,
        small_piece_threshold_bytes: usize,
    ) -> FragmentationStats {
        let mut stats = FragmentationStats {
            small_piece_threshold_bytes,
            ..FragmentationStats::default()
        };
        if let Some(root) = self.root {
            self.collect_fragmentation_stats(root, &mut stats);
        }
        stats
    }

    pub(crate) fn visit_range(
        &self,
        start: usize,
        end: usize,
        mut visit: impl FnMut(Piece, usize, usize),
    ) {
        let Some(root) = self.root else {
            return;
        };
        let total = root.total_bytes;
        let start = start.min(total);
        let end = end.min(total).max(start);
        if start >= end {
            return;
        }
        self.visit_range_ref(root, start, end, 0, &mut visit);
    }

    pub(crate) fn visit_range_while(
        &self,
        start: usize,
        end: usize,
        mut visit: impl FnMut(Piece, usize, usize) -> bool,
    ) -> bool {
        let Some(root) = self.root else {
            return true;
        };
        let total = root.total_bytes;
        let start = start.min(total);
        let end = end.min(total).max(start);
        if start >= end {
            return true;
        }
        self.visit_range_ref_while(root, start, end, 0, &mut visit)
    }

    pub(crate) fn visit_range_rev_while(
        &self,
        start: usize,
        end: usize,
        mut visit: impl FnMut(Piece, usize, usize) -> bool,
    ) -> bool {
        let Some(root) = self.root else {
            return true;
        };
        let total = root.total_bytes;
        let start = start.min(total);
        let end = end.min(total).max(start);
        if start >= end {
            return true;
        }
        self.visit_range_ref_rev_while(root, start, end, 0, &mut visit)
    }

    pub(crate) fn find_line_start(
        &self,
        line0: usize,
        mut locate_break: impl FnMut(Piece, usize) -> Option<usize>,
    ) -> Option<usize> {
        if line0 == 0 {
            return Some(0);
        }
        let root = self.root?;
        let target_break = line0.saturating_sub(1);
        if target_break >= root.total_line_breaks {
            return None;
        }
        self.find_line_start_ref(root, target_break, 0, &mut locate_break)
    }

    pub(crate) fn insert<F>(
        &mut self,
        offset: usize,
        piece: Piece,
        split_piece: &mut F,
    ) -> io::Result<()>
    where
        F: FnMut(Piece, usize) -> (Option<Piece>, Option<Piece>),
    {
        if piece.len == 0 {
            return Ok(());
        }
        let offset = offset.min(self.total_len());
        let Some(root) = self.root.take() else {
            self.root = self.build_tree_from_pieces(vec![piece]);
            self.record_history_root();
            return Ok(());
        };
        let first_new_page = self.store.next_page_id_hint();
        let children = match self.insert_at(root, offset, piece, split_piece) {
            Ok(children) => children,
            Err(err) => {
                self.root = Some(root);
                return Err(err);
            }
        };
        // While persistence is down every new page lives only in the
        // bounded resident window (see `alloc_page`). The edit must not
        // land through a page that the window refused: reads would
        // silently skip it and the document would shrink behind
        // `total_len`.
        if let Err(err) = self.store.verify_pages_resident_since(first_new_page) {
            self.root = Some(root);
            return Err(err);
        }
        self.root = self.build_tree_from_child_refs(children);
        self.record_history_root();
        Ok(())
    }

    pub(crate) fn delete_range<F>(
        &mut self,
        start: usize,
        len: usize,
        trim_piece: &mut F,
    ) -> io::Result<()>
    where
        F: FnMut(Piece, usize, usize) -> (Option<Piece>, Option<Piece>),
    {
        if len == 0 {
            return Ok(());
        }
        let Some(root) = self.root.take() else {
            return Ok(());
        };
        let total = root.total_bytes;
        let start = start.min(total);
        let end = start.saturating_add(len).min(total);
        if start >= end {
            self.root = Some(root);
            return Ok(());
        }
        let first_new_page = self.store.next_page_id_hint();
        match self.delete_at(root, start, end, trim_piece) {
            Ok(children) => {
                if let Err(err) = self.store.verify_pages_resident_since(first_new_page) {
                    self.root = Some(root);
                    return Err(err);
                }
                self.root = self.build_tree_from_child_refs(children);
                self.record_history_root();
                Ok(())
            }
            Err(err) => {
                self.root = Some(root);
                Err(err)
            }
        }
    }

    pub(crate) fn flush_session(&mut self, add_bytes: &[u8], meta: SessionMeta) -> io::Result<()> {
        self.store
            .flush_session(&self.history, self.history_index, add_bytes, meta)
    }

    // Editor-driven: the only caller is the discarded-save rebase in the
    // persistence layer.
    #[cfg(feature = "editor")]
    pub(crate) fn rebuild_history_roots_disk<F>(
        &self,
        source_path: &Path,
        mut remap_piece: F,
    ) -> io::Result<Self>
    where
        F: FnMut(Piece) -> io::Result<Vec<Piece>>,
    {
        let history_index = self.history_index.min(self.history.len().saturating_sub(1));
        let mut remapped_history = Vec::with_capacity(self.history.len().max(1));
        for entry in &self.history {
            let mut pieces = Vec::new();
            if let Some(root) = entry.root {
                self.collect_pieces(root, &mut pieces);
            }

            let mut remapped_pieces = Vec::new();
            for piece in pieces {
                remapped_pieces.extend(remap_piece(piece)?);
            }
            remapped_history.push(remapped_pieces);
        }

        if remapped_history.is_empty() {
            remapped_history.push(Vec::new());
        }

        let mut rebuilt = Self {
            store: PageStore::Disk(DiskPageStore::create(source_path)?),
            root: None,
            history: Vec::with_capacity(remapped_history.len()),
            known_history: Vec::with_capacity(remapped_history.len()),
            history_index: history_index.min(remapped_history.len().saturating_sub(1)),
            history_batch_depth: 0,
            history_batch_dirty: false,
        };

        let rebuilt_boundary = self
            .known_history
            .get(self.history_index)
            .copied()
            .unwrap_or((0, false));
        for pieces in remapped_history {
            let root = rebuilt.build_tree_from_pieces(filter_zero_len(pieces));
            rebuilt.history.push(RootEntry { root });
            rebuilt.known_history.push(rebuilt_boundary);
        }

        rebuilt.root = rebuilt
            .history
            .get(rebuilt.history_index)
            .copied()
            .unwrap_or(RootEntry { root: None })
            .root;
        Ok(rebuilt)
    }

    pub(crate) fn replace_current_root_with_pieces(&mut self, pieces: Vec<Piece>) {
        let root = self.build_tree_from_pieces(filter_zero_len(pieces));
        self.root = root;
        if let Some(entry) = self.history.get_mut(self.history_index) {
            entry.root = root;
        } else {
            self.history = vec![RootEntry { root }];
            self.history_index = 0;
        }
    }

    pub(crate) fn detach_persistence(&mut self) {
        if !matches!(self.store, PageStore::Disk(_)) {
            return;
        }

        let pieces = self.to_vec();
        *self = Self::from_pieces(pieces);
    }

    pub(crate) fn undo(&mut self, current_boundary: (usize, bool)) -> Option<(usize, bool)> {
        if self.history_index == 0 {
            return None;
        }
        self.history_index = self.history_index.saturating_sub(1);
        self.root = self.history[self.history_index].root;
        Some(
            self.known_history
                .get(self.history_index)
                .copied()
                .unwrap_or(current_boundary),
        )
    }

    pub(crate) fn redo(&mut self, current_boundary: (usize, bool)) -> Option<(usize, bool)> {
        if self.history_index + 1 >= self.history.len() {
            return None;
        }
        self.history_index += 1;
        self.root = self.history[self.history_index].root;
        Some(
            self.known_history
                .get(self.history_index)
                .copied()
                .unwrap_or(current_boundary),
        )
    }

    pub(crate) fn begin_batch_edit(&mut self) {
        self.history_batch_depth = self.history_batch_depth.saturating_add(1);
    }

    pub(crate) fn end_batch_edit(&mut self) {
        if self.history_batch_depth == 0 {
            return;
        }
        self.history_batch_depth -= 1;
        if self.history_batch_depth == 0 && self.history_batch_dirty {
            self.history_batch_dirty = false;
            self.push_history_root();
        }
    }

    fn record_history_root(&mut self) {
        if self.history_batch_depth > 0 {
            self.history_batch_dirty = true;
            return;
        }
        self.push_history_root();
    }

    fn push_history_root(&mut self) {
        let next = RootEntry { root: self.root };
        let is_same = self
            .history
            .get(self.history_index)
            .copied()
            .map(|entry| entry == next)
            .unwrap_or(false);
        // A pending boundary (written by `set_known_boundary` while a batch
        // was active) is detected by the known history carrying exactly one
        // extra entry; it must survive the truncate below and become the
        // boundary of the newly committed root.
        let pending = if self.known_history.len() == self.history.len() + 1 {
            Some(self.known_history[self.history_index + 1])
        } else {
            None
        };
        if is_same {
            // A no-op batch: the pending boundary describes the same state,
            // so fold it into the current entry and drop the pending slot.
            if let Some(pending) = pending {
                self.known_history[self.history_index] = pending;
                self.known_history.truncate(self.history_index + 1);
            }
            return;
        }
        self.history.truncate(self.history_index.saturating_add(1));
        self.known_history
            .truncate(self.history_index.saturating_add(1));
        self.history.push(next);
        let fallback = pending
            .or_else(|| self.known_history.last().copied())
            .unwrap_or((0, false));
        self.known_history.push(fallback);
        self.history_index = self.history.len().saturating_sub(1);
    }

    /// Patches the known-region boundary of the current history entry.
    /// Callers invoke this right after they adjust their own boundary, so
    /// every history entry carries the boundary that belonged to the state
    /// it represents.
    pub(crate) fn set_known_boundary(&mut self, known: usize, full_index: bool) {
        // Inside an edit batch the root push is deferred, so the boundary
        // that belongs to the batch's final state must land in the
        // *pending* slot (`history_index + 1`), not in the current one:
        // the current entry describes the pre-batch state that undo must
        // restore. `push_history_root` picks the pending value up when the
        // deferred root is committed.
        let target = if self.history_batch_depth > 0 {
            self.history_index + 1
        } else {
            self.history_index
        };
        while self.known_history.len() <= target {
            let fallback = self
                .known_history
                .last()
                .copied()
                .unwrap_or((known, full_index));
            self.known_history.push(fallback);
        }
        if let Some(entry) = self.known_history.get_mut(target) {
            *entry = (known, full_index);
        }
    }

    fn page(&self, page_id: u64) -> Option<Arc<PiecePage>> {
        self.store.get_page(page_id)
    }

    fn collect_pieces(&self, child: ChildRef, out: &mut Vec<Piece>) {
        let Some(page) = self.page(child.page_id) else {
            return;
        };
        match page.as_ref() {
            PiecePage::Internal { children } => {
                for child in children {
                    self.collect_pieces(*child, out);
                }
            }
            PiecePage::Leaf { pieces } => out.extend_from_slice(pieces),
        }
    }

    fn collect_fragmentation_stats(&self, child: ChildRef, stats: &mut FragmentationStats) {
        let Some(page) = self.page(child.page_id) else {
            return;
        };
        match page.as_ref() {
            PiecePage::Internal { children } => {
                for child in children {
                    self.collect_fragmentation_stats(*child, stats);
                }
            }
            PiecePage::Leaf { pieces } => {
                for piece in pieces {
                    stats.observe_piece(*piece);
                }
            }
        }
    }

    fn visit_range_ref(
        &self,
        child: ChildRef,
        start: usize,
        end: usize,
        base: usize,
        visit: &mut impl FnMut(Piece, usize, usize),
    ) {
        let Some(page) = self.page(child.page_id) else {
            return;
        };
        match page.as_ref() {
            PiecePage::Internal { children } => {
                let mut child_base = base;
                for child in children {
                    let child_end = child_base.saturating_add(child.total_bytes);
                    if child_end <= start {
                        child_base = child_end;
                        continue;
                    }
                    if child_base >= end {
                        break;
                    }
                    self.visit_range_ref(*child, start, end, child_base, visit);
                    child_base = child_end;
                }
            }
            PiecePage::Leaf { pieces } => {
                let mut piece_base = base;
                for piece in pieces {
                    let piece_end = piece_base.saturating_add(piece.len);
                    if piece_end <= start {
                        piece_base = piece_end;
                        continue;
                    }
                    if piece_base >= end {
                        break;
                    }
                    let overlap_start = start.saturating_sub(piece_base).min(piece.len);
                    let overlap_end = end.min(piece_end).saturating_sub(piece_base).min(piece.len);
                    if overlap_start < overlap_end {
                        visit(*piece, overlap_start, overlap_end);
                    }
                    piece_base = piece_end;
                }
            }
        }
    }

    fn visit_range_ref_while(
        &self,
        child: ChildRef,
        start: usize,
        end: usize,
        base: usize,
        visit: &mut impl FnMut(Piece, usize, usize) -> bool,
    ) -> bool {
        let Some(page) = self.page(child.page_id) else {
            return true;
        };
        match page.as_ref() {
            PiecePage::Internal { children } => {
                let mut child_base = base;
                for child in children {
                    let child_end = child_base.saturating_add(child.total_bytes);
                    if child_end <= start {
                        child_base = child_end;
                        continue;
                    }
                    if child_base >= end {
                        break;
                    }
                    if !self.visit_range_ref_while(*child, start, end, child_base, visit) {
                        return false;
                    }
                    child_base = child_end;
                }
            }
            PiecePage::Leaf { pieces } => {
                let mut piece_base = base;
                for piece in pieces {
                    let piece_end = piece_base.saturating_add(piece.len);
                    if piece_end <= start {
                        piece_base = piece_end;
                        continue;
                    }
                    if piece_base >= end {
                        break;
                    }
                    let overlap_start = start.saturating_sub(piece_base).min(piece.len);
                    let overlap_end = end.min(piece_end).saturating_sub(piece_base).min(piece.len);
                    if overlap_start < overlap_end && !visit(*piece, overlap_start, overlap_end) {
                        return false;
                    }
                    piece_base = piece_end;
                }
            }
        }
        true
    }

    fn visit_range_ref_rev_while(
        &self,
        child: ChildRef,
        start: usize,
        end: usize,
        base: usize,
        visit: &mut impl FnMut(Piece, usize, usize) -> bool,
    ) -> bool {
        let Some(page) = self.page(child.page_id) else {
            return true;
        };
        match page.as_ref() {
            PiecePage::Internal { children } => {
                let mut child_end = base.saturating_add(child.total_bytes);
                for child in children.iter().rev() {
                    let child_start = child_end.saturating_sub(child.total_bytes);
                    if child_start >= end {
                        child_end = child_start;
                        continue;
                    }
                    if child_end <= start {
                        break;
                    }
                    if !self.visit_range_ref_rev_while(*child, start, end, child_start, visit) {
                        return false;
                    }
                    child_end = child_start;
                }
            }
            PiecePage::Leaf { pieces } => {
                let mut piece_end = base.saturating_add(child.total_bytes);
                for piece in pieces.iter().rev() {
                    let piece_start = piece_end.saturating_sub(piece.len);
                    if piece_start >= end {
                        piece_end = piece_start;
                        continue;
                    }
                    if piece_end <= start {
                        break;
                    }
                    let overlap_start = start.saturating_sub(piece_start).min(piece.len);
                    let overlap_end = end
                        .min(piece_end)
                        .saturating_sub(piece_start)
                        .min(piece.len);
                    if overlap_start < overlap_end && !visit(*piece, overlap_start, overlap_end) {
                        return false;
                    }
                    piece_end = piece_start;
                }
            }
        }
        true
    }

    fn find_line_start_ref(
        &self,
        child: ChildRef,
        target_break: usize,
        base: usize,
        locate_break: &mut impl FnMut(Piece, usize) -> Option<usize>,
    ) -> Option<usize> {
        let page = self.page(child.page_id)?;
        match page.as_ref() {
            PiecePage::Internal { children } => {
                let mut child_base = base;
                let mut target_break = target_break;
                for child in children {
                    if target_break >= child.total_line_breaks {
                        target_break -= child.total_line_breaks;
                        child_base = child_base.saturating_add(child.total_bytes);
                        continue;
                    }
                    return self.find_line_start_ref(
                        *child,
                        target_break,
                        child_base,
                        locate_break,
                    );
                }
                None
            }
            PiecePage::Leaf { pieces } => {
                let mut piece_base = base;
                let mut target_break = target_break;
                for piece in pieces {
                    if target_break >= piece.line_breaks {
                        target_break -= piece.line_breaks;
                        piece_base = piece_base.saturating_add(piece.len);
                        continue;
                    }
                    let local = locate_break(*piece, target_break)?;
                    return Some(piece_base.saturating_add(local));
                }
                None
            }
        }
    }

    fn insert_at<F>(
        &mut self,
        child: ChildRef,
        offset: usize,
        piece: Piece,
        split_piece: &mut F,
    ) -> io::Result<Vec<ChildRef>>
    where
        F: FnMut(Piece, usize) -> (Option<Piece>, Option<Piece>),
    {
        // An unreadable page must fail the edit: splicing an empty
        // child list here would silently drop the whole subtree from
        // the document while the caller believes the edit landed.
        let Some(page) = self.page(child.page_id).map(|page| (*page).clone()) else {
            return Err(io::Error::other(format!(
                "piece page {} is unreadable; refusing to edit through the loss",
                child.page_id
            )));
        };
        match page {
            PiecePage::Leaf { pieces } => {
                let mut next = insert_piece_into_leaf(pieces, offset, piece, split_piece);
                coalesce_adjacent(&mut next);
                Ok(self.leaf_refs_from_pieces(next))
            }
            PiecePage::Internal { children } => {
                let (index, child_start) = child_index_for_offset(&children, offset);
                let local_offset = offset.saturating_sub(child_start);
                let mut next_children = children;
                let target = next_children[index];
                let inserted = self.insert_at(target, local_offset, piece, split_piece)?;
                next_children.splice(index..=index, inserted);
                Ok(self.internal_refs_from_children(next_children))
            }
        }
    }

    fn delete_at<F>(
        &mut self,
        child: ChildRef,
        start: usize,
        end: usize,
        trim_piece: &mut F,
    ) -> io::Result<Vec<ChildRef>>
    where
        F: FnMut(Piece, usize, usize) -> (Option<Piece>, Option<Piece>),
    {
        let Some(page) = self.page(child.page_id).map(|page| (*page).clone()) else {
            return Err(io::Error::other(format!(
                "piece page {} is unreadable; refusing to edit through the loss",
                child.page_id
            )));
        };
        match page {
            PiecePage::Leaf { pieces } => {
                let mut next = delete_range_from_leaf(pieces, start, end, trim_piece);
                coalesce_adjacent(&mut next);
                Ok(self.leaf_refs_from_pieces(next))
            }
            PiecePage::Internal { children } => {
                let mut next_children = Vec::with_capacity(children.len());
                let mut child_base = 0usize;
                for child in children {
                    let child_end = child_base.saturating_add(child.total_bytes);
                    if child_end <= start || child_base >= end {
                        next_children.push(child);
                    } else {
                        let local_start = start.saturating_sub(child_base).min(child.total_bytes);
                        let local_end = end.min(child_end).saturating_sub(child_base);
                        next_children.extend(self.delete_at(
                            child,
                            local_start,
                            local_end,
                            trim_piece,
                        )?);
                    }
                    child_base = child_end;
                }
                Ok(self.internal_refs_from_children(next_children))
            }
        }
    }

    fn build_tree_from_pieces(&mut self, pieces: Vec<Piece>) -> Option<ChildRef> {
        let leaves = self.leaf_refs_from_pieces(pieces);
        self.build_tree_from_child_refs(leaves)
    }

    fn build_tree_from_child_refs(&mut self, mut children: Vec<ChildRef>) -> Option<ChildRef> {
        if children.is_empty() {
            return None;
        }
        while children.len() > 1 {
            children = self.internal_refs_from_children(children);
        }
        children.pop()
    }

    fn leaf_refs_from_pieces(&mut self, pieces: Vec<Piece>) -> Vec<ChildRef> {
        if pieces.is_empty() {
            return Vec::new();
        }

        pieces
            .chunks(MAX_LEAF_PIECES)
            .map(|chunk| {
                let chunk = chunk.to_vec();
                // Hostile CRC-valid pages can carry totals that overflow
                // a plain sum; saturate so the next edit fails a bounds
                // check instead of panicking (debug) or wrapping (release).
                let total_bytes = chunk
                    .iter()
                    .map(|piece| piece.len)
                    .fold(0usize, usize::saturating_add);
                let total_line_breaks = chunk
                    .iter()
                    .map(|piece| piece.line_breaks)
                    .fold(0usize, usize::saturating_add);
                self.alloc_page(
                    PiecePage::Leaf { pieces: chunk },
                    total_bytes,
                    total_line_breaks,
                )
            })
            .collect()
    }

    fn internal_refs_from_children(&mut self, children: Vec<ChildRef>) -> Vec<ChildRef> {
        if children.is_empty() {
            return Vec::new();
        }

        children
            .chunks(MAX_INTERNAL_CHILDREN)
            .map(|chunk| {
                let chunk = chunk.to_vec();
                // Saturate for hostile page totals, same as the leaf
                // aggregator above.
                let total_bytes = chunk
                    .iter()
                    .map(|child| child.total_bytes)
                    .fold(0usize, usize::saturating_add);
                let total_line_breaks = chunk
                    .iter()
                    .map(|child| child.total_line_breaks)
                    .fold(0usize, usize::saturating_add);
                self.alloc_page(
                    PiecePage::Internal { children: chunk },
                    total_bytes,
                    total_line_breaks,
                )
            })
            .collect()
    }

    fn alloc_page(
        &mut self,
        page: PiecePage,
        total_bytes: usize,
        total_line_breaks: usize,
    ) -> ChildRef {
        self.store.alloc_page(page, total_bytes, total_line_breaks)
    }
}

impl PageStore {
    fn get_page(&self, page_id: u64) -> Option<Arc<PiecePage>> {
        match self {
            Self::InMemory(store) => store.pages.get(&page_id).cloned(),
            Self::Disk(store) => store.get_page(page_id),
        }
    }

    fn alloc_page(
        &mut self,
        page: PiecePage,
        total_bytes: usize,
        total_line_breaks: usize,
    ) -> ChildRef {
        match self {
            Self::InMemory(store) => store.alloc_page(page, total_bytes, total_line_breaks),
            Self::Disk(store) => store.alloc_page(page, total_bytes, total_line_breaks),
        }
    }

    fn next_page_id_hint(&self) -> u64 {
        match self {
            Self::InMemory(_) => 0,
            Self::Disk(store) => store.next_page_id,
        }
    }

    /// Verifies that every page allocated since `first_new_page` is
    /// readable for the rest of the session. While persistence is down
    /// new pages live only in the bounded resident window, so a page the
    /// window refused must fail the edit instead of being referenced by
    /// a root that reads would silently skip.
    fn verify_pages_resident_since(&self, first_new_page: u64) -> io::Result<()> {
        match self {
            Self::InMemory(_) => Ok(()),
            Self::Disk(store) => {
                if !store.persistence_failed {
                    return Ok(());
                }
                // Pages allocated by this edit are strictly past
                // `first_new_page`; the id AT the hint belongs to the
                // previous edit and may legitimately live on disk.
                for page_id in (first_new_page + 1)..=store.next_page_id {
                    if !store.resident_pages.contains_key(&page_id) {
                        return Err(io::Error::other(format!(
                            "resident window exhausted while persistence is down: \
                             piece page {page_id} could not be kept resident"
                        )));
                    }
                }
                Ok(())
            }
        }
    }

    fn flush_session(
        &mut self,
        history: &[RootEntry],
        history_index: usize,
        add_bytes: &[u8],
        meta: SessionMeta,
    ) -> io::Result<()> {
        if let Self::Disk(store) = self {
            store.flush_session(history, history_index, add_bytes, meta)
        } else {
            Ok(())
        }
    }
}

impl InMemoryPageStore {
    fn alloc_page(
        &mut self,
        page: PiecePage,
        total_bytes: usize,
        total_line_breaks: usize,
    ) -> ChildRef {
        self.next_page_id = self.next_page_id.saturating_add(1);
        let page_id = self.next_page_id;
        self.pages.insert(page_id, Arc::new(page));
        ChildRef {
            page_id,
            total_bytes,
            total_line_breaks,
        }
    }
}

impl DiskPageStore {
    fn create(source_path: &Path) -> io::Result<Self> {
        let path = editlog_path(source_path);
        let source = source_metadata(source_path)?;
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(parent)?;
        FileStorage::replace_with(&path, |file| {
            write_editlog_header(file, EditLogHeader::default())?;
            file.flush()?;
            file.sync_all()
        })?;
        let file = OpenOptions::new().read(true).write(true).open(&path)?;

        Ok(Self {
            path,
            source,
            file: Some(file),
            cache: Mutex::new(PageCache::default()),
            resident_pages: HashMap::new(),
            next_page_id: 0,
            persistence_failed: false,
        })
    }

    fn open(source_path: &Path, source: SourceMetadata) -> io::Result<OpenDiskSession> {
        let path = editlog_path(source_path);
        if !path.exists() {
            return Ok(None);
        }

        let mut file = OpenOptions::new().read(true).write(true).open(&path)?;
        let header = read_editlog_header(&mut file)?;
        if header.source_len != source.len
            || header.source_mtime_ns != source.mtime_ns
            || header.source_fingerprint != source.fingerprint
        {
            return Ok(None);
        }

        let history = read_history_entries(
            &mut file,
            header.history_first_page_id,
            header.history_page_count as usize,
            header.history_len as usize,
        )?;
        if history.is_empty() {
            return Ok(None);
        }

        let history_index = (header.history_index as usize).min(history.len().saturating_sub(1));
        let add = read_add_bytes(
            &mut file,
            header.add_first_page_id,
            header.add_page_count as usize,
            header.add_len as usize,
        )?;
        // The raw header field is u64; a value that does not fit the
        // target pointer width must fail the open instead of truncating
        // (the sidecar is dropped and rebuilt on such an error).
        let known_byte_len = usize::try_from(header.known_byte_len).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "qem editlog known_byte_len exceeds the addressable size",
            )
        })?;
        let meta = SessionMeta {
            known_byte_len,
            full_index: (header.flags & EDITLOG_FLAG_FULL_INDEX) != 0,
            encoding_origin: ((header.flags & EDITLOG_FLAG_HAS_ENCODING_ORIGIN) != 0)
                .then(|| decode_encoding_origin(header.encoding_origin))
                .flatten(),
            decoding_had_errors: (header.flags & EDITLOG_FLAG_DECODING_HAD_ERRORS) != 0,
        };

        // `page_count` is also a raw unchecksummed header field, but
        // unlike the counters above it drives the committed-size
        // truncation and `next_page_id`. A corrupt-small value would
        // truncate pages the history still references (lazy page misses
        // then read short while `total_len` stays full), and a
        // corrupt-huge one would wrap the page-offset arithmetic on the
        // next flush. Either way the session is invalid and must reject
        // here so the sidecar is dropped and rebuilt, mirroring the
        // distrust `read_history_entries` applies to its own counters.
        let file_len = file.metadata()?.len();
        let page_count = usize::try_from(header.page_count).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "qem editlog page_count exceeds the addressable size",
            )
        })?;
        if page_count > bounded_page_count(Some(file_len), usize::MAX) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "qem editlog page_count exceeds what the sidecar can physically hold",
            ));
        }
        let max_referenced_page_id = history
            .iter()
            .filter_map(|entry| entry.root.as_ref())
            .map(|child| child.page_id)
            .chain((header.history_page_count > 0).then(|| {
                header
                    .history_first_page_id
                    .saturating_add(header.history_page_count - 1)
            }))
            .chain((header.add_page_count > 0).then(|| {
                header
                    .add_first_page_id
                    .saturating_add(header.add_page_count - 1)
            }))
            .max()
            .unwrap_or(0);
        if max_referenced_page_id > page_count as u64 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "qem editlog page_count is below the pages the history references",
            ));
        }

        // The header is the commit record: pages appended after the last
        // committed `page_count` are orphans from a crash between the page
        // append and the header rewrite. Page writes go through
        // `SeekFrom::End` and assert the append offset against the page id,
        // so orphan bytes would poison every later flush into
        // `persistence_failed` and silently kill persistence for the rest
        // of the session. Truncating to the committed size restores the
        // exact pre-crash append position.
        let committed_len = EDITLOG_HEADER_BYTES as u64
            + (page_count as u64).saturating_mul(EDITLOG_PAGE_SIZE as u64);
        if file.metadata()?.len() > committed_len {
            file.set_len(committed_len)?;
        }

        Ok(Some((
            Self {
                path,
                source,
                file: Some(file),
                cache: Mutex::new(PageCache::default()),
                resident_pages: HashMap::new(),
                next_page_id: page_count as u64,
                persistence_failed: false,
            },
            history,
            history_index,
            add,
            meta,
        )))
    }

    fn get_page(&self, page_id: u64) -> Option<Arc<PiecePage>> {
        if let Some(page) = self.resident_pages.get(&page_id) {
            return Some(Arc::clone(page));
        }
        if let Ok(mut cache) = self.cache.lock() {
            if let Some(page) = cache.get(page_id) {
                return Some(page);
            }
        }

        let mut file = OpenOptions::new().read(true).open(&self.path).ok()?;
        let page = Arc::new(read_page_from_file(&mut file, page_id).ok()?);
        if let Ok(mut cache) = self.cache.lock() {
            cache.insert(page_id, Arc::clone(&page));
        }
        Some(page)
    }

    fn alloc_page(
        &mut self,
        page: PiecePage,
        total_bytes: usize,
        total_line_breaks: usize,
    ) -> ChildRef {
        self.next_page_id = self.next_page_id.saturating_add(1);
        let page_id = self.next_page_id;
        let page_ref = Arc::new(page);

        let write_result = if self.persistence_failed {
            Err(io::Error::other("editlog persistence already failed"))
        } else {
            self.write_page(page_id, Arc::clone(&page_ref))
        };

        if write_result.is_err() {
            self.persistence_failed = true;
            // While persistence is down every new page lives only in
            // resident_pages; without a cap a long session after a disk
            // failure accumulates an Arc'd page per structural edit
            // forever. Keep a bounded window of them: beyond the cap a
            // page becomes unreadable, and an edit through an unreadable
            // page fails as a typed error instead of losing the subtree
            // (see insert_at / delete_at).
            if self.resident_pages.len() < RESIDENT_PAGES_FAILED_PERSISTENCE_CAP {
                self.resident_pages.insert(page_id, Arc::clone(&page_ref));
            }
        }
        if let Ok(mut cache) = self.cache.lock() {
            cache.insert(page_id, page_ref);
        }

        ChildRef {
            page_id,
            total_bytes,
            total_line_breaks,
        }
    }

    fn write_page(&mut self, page_id: u64, page: Arc<PiecePage>) -> io::Result<()> {
        let Some(file) = self.file.as_mut() else {
            return Err(io::Error::other("missing editlog handle"));
        };
        file.seek(SeekFrom::End(0))?;
        let bytes = serialize_page(page.as_ref())?;
        debug_assert_eq!(bytes.len(), EDITLOG_PAGE_SIZE);
        file.write_all(&bytes)?;
        let expected_offset = (EDITLOG_HEADER_BYTES as u64).saturating_add(
            page_id
                .saturating_sub(1)
                .saturating_mul(EDITLOG_PAGE_SIZE as u64),
        );
        let actual_end = file.stream_position()?;
        let actual_offset = actual_end.saturating_sub(EDITLOG_PAGE_SIZE as u64);
        if actual_offset != expected_offset {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "editlog page append order became inconsistent",
            ));
        }
        Ok(())
    }

    fn flush_session(
        &mut self,
        history: &[RootEntry],
        history_index: usize,
        add_bytes: &[u8],
        meta: SessionMeta,
    ) -> io::Result<()> {
        if self.persistence_failed || !self.resident_pages.is_empty() {
            self.persistence_failed = true;
            return Err(io::Error::other("editlog persistence is unavailable"));
        }

        let (history_first_page_id, history_page_count) = self.write_history_pages(history)?;
        let (add_first_page_id, add_page_count) = self.write_add_pages(add_bytes)?;

        let Some(file) = self.file.as_mut() else {
            self.persistence_failed = true;
            return Err(io::Error::other("missing editlog handle"));
        };

        let header = EditLogHeader {
            page_count: self.next_page_id,
            source_len: self.source.len,
            source_mtime_ns: self.source.mtime_ns,
            source_fingerprint: self.source.fingerprint,
            history_first_page_id,
            history_page_count: history_page_count as u64,
            history_len: history.len() as u64,
            history_index: history_index as u64,
            add_first_page_id,
            add_page_count: add_page_count as u64,
            add_len: add_bytes.len() as u64,
            known_byte_len: meta.known_byte_len as u64,
            flags: (if meta.full_index {
                EDITLOG_FLAG_FULL_INDEX
            } else {
                0
            }) | (if meta.encoding_origin.is_some() {
                EDITLOG_FLAG_HAS_ENCODING_ORIGIN
            } else {
                0
            }) | (if meta.decoding_had_errors {
                EDITLOG_FLAG_DECODING_HAD_ERRORS
            } else {
                0
            }),
            encoding_origin: meta
                .encoding_origin
                .map(encode_encoding_origin)
                .unwrap_or(0),
        };
        // Durability protocol:
        // 1. append all newly reachable pages first
        // 2. flush/sync those pages
        // 3. rewrite the fixed header as the authoritative commit record
        // 4. flush/sync again so reopen either sees the previous committed
        //    header or the fully updated one
        let result = (|| -> io::Result<()> {
            file.flush()?;
            file.sync_all()?;
            write_editlog_header(file, header)?;
            file.flush()?;
            file.sync_all()?;
            Ok(())
        })();

        if let Err(err) = result {
            self.persistence_failed = true;
            return Err(err);
        }
        Ok(())
    }

    fn write_history_pages(&mut self, history: &[RootEntry]) -> io::Result<(u64, usize)> {
        if history.is_empty() {
            return Ok((0, 0));
        }

        let capacity = history_page_capacity();
        let first_page_id = self.next_page_id.saturating_add(1);
        let mut written = 0usize;
        for chunk in history.chunks(capacity.max(1)) {
            let bytes = serialize_history_page(chunk)?;
            self.write_raw_page(bytes)?;
            written += 1;
        }
        Ok((first_page_id, written))
    }

    fn write_add_pages(&mut self, add_bytes: &[u8]) -> io::Result<(u64, usize)> {
        if add_bytes.is_empty() {
            return Ok((0, 0));
        }

        let capacity = add_page_capacity();
        let first_page_id = self.next_page_id.saturating_add(1);
        let mut written = 0usize;
        for chunk in add_bytes.chunks(capacity.max(1)) {
            let bytes = serialize_add_page(chunk)?;
            self.write_raw_page(bytes)?;
            written += 1;
        }
        Ok((first_page_id, written))
    }

    fn write_raw_page(&mut self, bytes: Vec<u8>) -> io::Result<u64> {
        let Some(file) = self.file.as_mut() else {
            return Err(io::Error::other("missing editlog handle"));
        };
        self.next_page_id = self.next_page_id.saturating_add(1);
        let page_id = self.next_page_id;
        file.seek(SeekFrom::End(0))?;
        debug_assert_eq!(bytes.len(), EDITLOG_PAGE_SIZE);
        file.write_all(&bytes)?;
        let expected_offset = (EDITLOG_HEADER_BYTES as u64).saturating_add(
            page_id
                .saturating_sub(1)
                .saturating_mul(EDITLOG_PAGE_SIZE as u64),
        );
        let actual_end = file.stream_position()?;
        let actual_offset = actual_end.saturating_sub(EDITLOG_PAGE_SIZE as u64);
        if actual_offset != expected_offset {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "editlog page append order became inconsistent",
            ));
        }
        Ok(page_id)
    }
}

impl PageCache {
    fn get(&mut self, page_id: u64) -> Option<Arc<PiecePage>> {
        let page = self.pages.get(&page_id).cloned()?;
        self.touch(page_id);
        Some(page)
    }

    fn insert(&mut self, page_id: u64, page: Arc<PiecePage>) {
        self.pages.insert(page_id, page);
        self.touch(page_id);
        while self.order.len() > EDITLOG_CACHE_PAGES {
            if let Some(evicted) = self.order.pop_back() {
                self.pages.remove(&evicted);
            }
        }
    }

    fn touch(&mut self, page_id: u64) {
        if let Some(idx) = self.order.iter().position(|id| *id == page_id) {
            self.order.remove(idx);
        }
        self.order.push_front(page_id);
    }
}

fn write_editlog_header(file: &mut File, header: EditLogHeader) -> io::Result<()> {
    let mut buf = [0u8; EDITLOG_HEADER_BYTES];
    buf[0..8].copy_from_slice(EDITLOG_MAGIC);
    buf[8..12].copy_from_slice(&EDITLOG_VERSION.to_le_bytes());
    buf[12..16].copy_from_slice(&(EDITLOG_PAGE_SIZE as u32).to_le_bytes());
    buf[16..24].copy_from_slice(&header.page_count.to_le_bytes());
    buf[24..32].copy_from_slice(&header.source_len.to_le_bytes());
    buf[32..40].copy_from_slice(&header.source_mtime_ns.to_le_bytes());
    buf[40..48].copy_from_slice(&header.source_fingerprint.to_le_bytes());
    buf[48..56].copy_from_slice(&header.history_first_page_id.to_le_bytes());
    buf[56..64].copy_from_slice(&header.history_page_count.to_le_bytes());
    buf[64..72].copy_from_slice(&header.history_len.to_le_bytes());
    buf[72..80].copy_from_slice(&header.history_index.to_le_bytes());
    buf[80..88].copy_from_slice(&header.add_first_page_id.to_le_bytes());
    buf[88..96].copy_from_slice(&header.add_page_count.to_le_bytes());
    buf[96..104].copy_from_slice(&header.add_len.to_le_bytes());
    buf[104..112].copy_from_slice(&header.known_byte_len.to_le_bytes());
    buf[112..120].copy_from_slice(&header.flags.to_le_bytes());
    buf[120..124].copy_from_slice(&header.encoding_origin.to_le_bytes());
    let committed_at_s = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .min(u32::MAX as u64) as u32;
    buf[124..128].copy_from_slice(&committed_at_s.to_le_bytes());
    file.seek(SeekFrom::Start(0))?;
    file.write_all(&buf)
}

fn read_editlog_header(file: &mut File) -> io::Result<EditLogHeader> {
    let mut buf = [0u8; EDITLOG_HEADER_BYTES];
    file.seek(SeekFrom::Start(0))?;
    file.read_exact(&mut buf)?;
    if &buf[0..8] != EDITLOG_MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid qem editlog magic",
        ));
    }
    let version = le_u32(&buf, 8, "qem editlog header")?;
    let page_size = le_u32(&buf, 12, "qem editlog header")?;
    if version != EDITLOG_VERSION || page_size as usize != EDITLOG_PAGE_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unsupported qem editlog format",
        ));
    }
    Ok(EditLogHeader {
        page_count: le_u64(&buf, 16, "qem editlog header")?,
        source_len: le_u64(&buf, 24, "qem editlog header")?,
        source_mtime_ns: le_u64(&buf, 32, "qem editlog header")?,
        source_fingerprint: le_u64(&buf, 40, "qem editlog header")?,
        history_first_page_id: le_u64(&buf, 48, "qem editlog header")?,
        history_page_count: le_u64(&buf, 56, "qem editlog header")?,
        history_len: le_u64(&buf, 64, "qem editlog header")?,
        history_index: le_u64(&buf, 72, "qem editlog header")?,
        add_first_page_id: le_u64(&buf, 80, "qem editlog header")?,
        add_page_count: le_u64(&buf, 88, "qem editlog header")?,
        add_len: le_u64(&buf, 96, "qem editlog header")?,
        known_byte_len: le_u64(&buf, 104, "qem editlog header")?,
        flags: le_u64(&buf, 112, "qem editlog header")?,
        encoding_origin: le_u32(&buf, 120, "qem editlog header")?,
    })
}

fn serialize_page(page: &PiecePage) -> io::Result<Vec<u8>> {
    let mut buf = vec![0u8; EDITLOG_PAGE_SIZE];
    let (kind, count, entry_width) = match page {
        PiecePage::Internal { children } => {
            (PageKind::Internal, children.len(), INTERNAL_ENTRY_BYTES)
        }
        PiecePage::Leaf { pieces } => (PageKind::Leaf, pieces.len(), LEAF_ENTRY_BYTES),
    };

    let max_entries = (EDITLOG_PAGE_SIZE - EDITLOG_PAGE_HEADER_BYTES) / entry_width;
    if count > max_entries {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "page exceeds fixed editlog capacity",
        ));
    }

    buf[4] = kind as u8;
    buf[8..16].copy_from_slice(&(count as u64).to_le_bytes());
    let mut cursor = EDITLOG_PAGE_HEADER_BYTES;
    match page {
        PiecePage::Internal { children } => {
            for child in children {
                buf[cursor..cursor + 8].copy_from_slice(&child.page_id.to_le_bytes());
                buf[cursor + 8..cursor + 16]
                    .copy_from_slice(&(child.total_bytes as u64).to_le_bytes());
                buf[cursor + 16..cursor + 24]
                    .copy_from_slice(&(child.total_line_breaks as u64).to_le_bytes());
                cursor += INTERNAL_ENTRY_BYTES;
            }
        }
        PiecePage::Leaf { pieces } => {
            for piece in pieces {
                buf[cursor] = match piece.src {
                    PieceSource::Original => 0,
                    PieceSource::Add => 1,
                };
                buf[cursor + 8..cursor + 16].copy_from_slice(&(piece.start as u64).to_le_bytes());
                buf[cursor + 16..cursor + 24].copy_from_slice(&(piece.len as u64).to_le_bytes());
                buf[cursor + 24..cursor + 32]
                    .copy_from_slice(&(piece.line_breaks as u64).to_le_bytes());
                cursor += LEAF_ENTRY_BYTES;
            }
        }
    }

    let mut hasher = Hasher::new();
    hasher.update(&buf[4..]);
    buf[0..4].copy_from_slice(&hasher.finalize().to_le_bytes());
    Ok(buf)
}

fn read_page_from_file(file: &mut File, page_id: u64) -> io::Result<PiecePage> {
    let mut buf = vec![0u8; EDITLOG_PAGE_SIZE];
    let offset = (EDITLOG_HEADER_BYTES as u64).saturating_add(
        page_id
            .saturating_sub(1)
            .saturating_mul(EDITLOG_PAGE_SIZE as u64),
    );
    file.seek(SeekFrom::Start(offset))?;
    file.read_exact(&mut buf)?;
    parse_page(&buf)
}

fn parse_page(buf: &[u8]) -> io::Result<PiecePage> {
    if buf.len() != EDITLOG_PAGE_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid editlog page size",
        ));
    }

    let expected_crc = le_u32(buf, 0, "qem editlog page")?;
    let mut hasher = Hasher::new();
    hasher.update(&buf[4..]);
    if hasher.finalize() != expected_crc {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "qem editlog page checksum mismatch",
        ));
    }

    let kind = match buf[4] {
        1 => PageKind::Internal,
        2 => PageKind::Leaf,
        3 => PageKind::History,
        4 => PageKind::Add,
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid qem editlog page kind",
            ));
        }
    };
    let count = le_u64(buf, 8, "qem editlog page")? as usize;
    let mut cursor = EDITLOG_PAGE_HEADER_BYTES;

    match kind {
        PageKind::Internal => {
            let max_entries =
                (EDITLOG_PAGE_SIZE - EDITLOG_PAGE_HEADER_BYTES) / INTERNAL_ENTRY_BYTES;
            if count > max_entries {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "internal editlog page is overfull",
                ));
            }
            // The writer never emits an empty internal page (a root for
            // an empty document is `None`, and
            // `build_tree_from_child_refs` allocates only for non-empty
            // children). A CRC-valid but empty internal page would panic
            // `child_index_for_offset` consumers on insert and silently
            // empty the document on delete, so it is invalid by
            // construction.
            if count == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "internal editlog page is empty",
                ));
            }

            let mut children = Vec::with_capacity(count);
            for _ in 0..count {
                children.push(ChildRef {
                    page_id: le_u64(buf, cursor, "qem editlog page")?,
                    total_bytes: le_u64(buf, cursor + 8, "qem editlog page")? as usize,
                    total_line_breaks: le_u64(buf, cursor + 16, "qem editlog page")? as usize,
                });
                cursor += INTERNAL_ENTRY_BYTES;
            }
            Ok(PiecePage::Internal { children })
        }
        PageKind::Leaf => {
            let max_entries = (EDITLOG_PAGE_SIZE - EDITLOG_PAGE_HEADER_BYTES) / LEAF_ENTRY_BYTES;
            if count > max_entries {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "leaf editlog page is overfull",
                ));
            }

            let mut pieces = Vec::with_capacity(count);
            for _ in 0..count {
                let src = match buf[cursor] {
                    0 => PieceSource::Original,
                    1 => PieceSource::Add,
                    _ => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "invalid piece source in editlog page",
                        ));
                    }
                };
                pieces.push(Piece {
                    src,
                    start: le_u64(buf, cursor + 8, "qem editlog page")? as usize,
                    len: le_u64(buf, cursor + 16, "qem editlog page")? as usize,
                    line_breaks: le_u64(buf, cursor + 24, "qem editlog page")? as usize,
                });
                cursor += LEAF_ENTRY_BYTES;
            }
            Ok(PiecePage::Leaf { pieces })
        }
        PageKind::History | PageKind::Add => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "non-piece editlog page used as piece page",
        )),
    }
}

fn history_page_capacity() -> usize {
    (EDITLOG_PAGE_SIZE - EDITLOG_PAGE_HEADER_BYTES) / HISTORY_ENTRY_BYTES
}

fn add_page_capacity() -> usize {
    EDITLOG_PAGE_SIZE - EDITLOG_PAGE_HEADER_BYTES
}

fn serialize_history_page(entries: &[RootEntry]) -> io::Result<Vec<u8>> {
    let mut buf = vec![0u8; EDITLOG_PAGE_SIZE];
    let max_entries = history_page_capacity().max(1);
    if entries.len() > max_entries {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "history page exceeds fixed editlog capacity",
        ));
    }

    buf[4] = PageKind::History as u8;
    buf[8..16].copy_from_slice(&(entries.len() as u64).to_le_bytes());
    let mut cursor = EDITLOG_PAGE_HEADER_BYTES;
    for entry in entries {
        let (present, page_id, total_bytes, total_line_breaks) = match entry.root {
            Some(root) => (
                1u64,
                root.page_id,
                root.total_bytes as u64,
                root.total_line_breaks as u64,
            ),
            None => (0u64, 0, 0, 0),
        };
        buf[cursor..cursor + 8].copy_from_slice(&present.to_le_bytes());
        buf[cursor + 8..cursor + 16].copy_from_slice(&page_id.to_le_bytes());
        buf[cursor + 16..cursor + 24].copy_from_slice(&total_bytes.to_le_bytes());
        buf[cursor + 24..cursor + 32].copy_from_slice(&total_line_breaks.to_le_bytes());
        cursor += HISTORY_ENTRY_BYTES;
    }

    let mut hasher = Hasher::new();
    hasher.update(&buf[4..]);
    buf[0..4].copy_from_slice(&hasher.finalize().to_le_bytes());
    Ok(buf)
}

fn serialize_add_page(chunk: &[u8]) -> io::Result<Vec<u8>> {
    let mut buf = vec![0u8; EDITLOG_PAGE_SIZE];
    let max_len = add_page_capacity().max(1);
    if chunk.len() > max_len {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "add-data page exceeds fixed editlog capacity",
        ));
    }

    buf[4] = PageKind::Add as u8;
    buf[8..16].copy_from_slice(&(chunk.len() as u64).to_le_bytes());
    buf[EDITLOG_PAGE_HEADER_BYTES..EDITLOG_PAGE_HEADER_BYTES + chunk.len()].copy_from_slice(chunk);

    let mut hasher = Hasher::new();
    hasher.update(&buf[4..]);
    buf[0..4].copy_from_slice(&hasher.finalize().to_le_bytes());
    Ok(buf)
}

fn read_history_entries(
    file: &mut File,
    first_page_id: u64,
    page_count: usize,
    history_len: usize,
) -> io::Result<Vec<RootEntry>> {
    if first_page_id == 0 || page_count == 0 || history_len == 0 {
        return Ok(Vec::new());
    }

    // The raw header is not checksummed, so its counters are bounded by what
    // the sidecar file can physically hold before they reach the allocation
    // and the per-page cursor below.
    let page_capacity = history_page_capacity();
    let page_count = bounded_page_count(file.metadata().map(|meta| meta.len()).ok(), page_count);
    let history_len = history_len.min(page_count.saturating_mul(page_capacity));

    let mut out = Vec::with_capacity(history_len);
    for rel in 0..page_count {
        let page_id = first_page_id.saturating_add(rel as u64);
        let buf = read_raw_page_from_file(file, page_id)?;
        let count = parse_page_header(&buf, PageKind::History)?;
        if count > page_capacity {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "history editlog page is overfull",
            ));
        }
        let mut cursor = EDITLOG_PAGE_HEADER_BYTES;
        for _ in 0..count {
            if out.len() >= history_len {
                break;
            }
            let present = le_u64(&buf, cursor, "qem editlog history page")?;
            let root = if present == 0 {
                None
            } else {
                Some(ChildRef {
                    page_id: le_u64(&buf, cursor + 8, "qem editlog history page")?,
                    total_bytes: le_u64(&buf, cursor + 16, "qem editlog history page")? as usize,
                    total_line_breaks: le_u64(&buf, cursor + 24, "qem editlog history page")?
                        as usize,
                })
            };
            out.push(RootEntry { root });
            cursor += HISTORY_ENTRY_BYTES;
        }
    }
    Ok(out)
}

fn read_add_bytes(
    file: &mut File,
    first_page_id: u64,
    page_count: usize,
    add_len: usize,
) -> io::Result<Vec<u8>> {
    if first_page_id == 0 || page_count == 0 || add_len == 0 {
        return Ok(Vec::new());
    }

    // Same raw-header bound as `read_history_entries`: the declared counters
    // cannot exceed what the sidecar file can physically hold.
    let page_count = bounded_page_count(file.metadata().map(|meta| meta.len()).ok(), page_count);
    let add_len = add_len.min(page_count.saturating_mul(add_page_capacity().max(1)));

    let mut out = Vec::with_capacity(add_len);
    for rel in 0..page_count {
        let page_id = first_page_id.saturating_add(rel as u64);
        let buf = read_raw_page_from_file(file, page_id)?;
        let payload_len = parse_page_header(&buf, PageKind::Add)?;
        let start = EDITLOG_PAGE_HEADER_BYTES;
        let end = start.saturating_add(payload_len).min(buf.len());
        out.extend_from_slice(&buf[start..end]);
        if out.len() >= add_len {
            break;
        }
    }
    out.truncate(add_len);
    Ok(out)
}

/// Bounds a page-chain length taken from the raw editlog header by the
/// number of pages the sidecar file can actually hold, so corrupt counters
/// fail as typed errors instead of driving huge allocations or out-of-bounds
/// cursors. When the file length is unavailable the chain is treated as
/// empty: an unbounded counter must never reach an allocation.
fn bounded_page_count(file_len: Option<u64>, page_count: usize) -> usize {
    let Some(file_len) = file_len else {
        return 0;
    };
    let usable = file_len
        .saturating_sub(EDITLOG_HEADER_BYTES as u64)
        .saturating_div(EDITLOG_PAGE_SIZE as u64);
    let usable = usize::try_from(usable).unwrap_or(usize::MAX);
    page_count.min(usable)
}

fn read_raw_page_from_file(file: &mut File, page_id: u64) -> io::Result<Vec<u8>> {
    let mut buf = vec![0u8; EDITLOG_PAGE_SIZE];
    let offset = (EDITLOG_HEADER_BYTES as u64).saturating_add(
        page_id
            .saturating_sub(1)
            .saturating_mul(EDITLOG_PAGE_SIZE as u64),
    );
    file.seek(SeekFrom::Start(offset))?;
    file.read_exact(&mut buf)?;
    validate_page_crc(&buf)?;
    Ok(buf)
}

fn parse_page_header(buf: &[u8], expected_kind: PageKind) -> io::Result<usize> {
    validate_page_crc(buf)?;
    let kind = match buf[4] {
        1 => PageKind::Internal,
        2 => PageKind::Leaf,
        3 => PageKind::History,
        4 => PageKind::Add,
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid qem editlog page kind",
            ));
        }
    };
    if kind != expected_kind {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unexpected qem editlog page kind",
        ));
    }
    Ok(le_u64(buf, 8, "qem editlog page")? as usize)
}

fn validate_page_crc(buf: &[u8]) -> io::Result<()> {
    if buf.len() != EDITLOG_PAGE_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid editlog page size",
        ));
    }

    let expected_crc = le_u32(buf, 0, "qem editlog page")?;
    let mut hasher = Hasher::new();
    hasher.update(&buf[4..]);
    if hasher.finalize() != expected_crc {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "qem editlog page checksum mismatch",
        ));
    }
    Ok(())
}

fn source_metadata(path: &Path) -> io::Result<SourceMetadata> {
    let metadata = std::fs::metadata(path)?;
    let len = metadata.len();
    let mtime_ns = metadata
        .modified()?
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .min(u64::MAX as u128) as u64;
    let fingerprint = sampled_file_fingerprint(path)?;
    Ok(SourceMetadata {
        len,
        mtime_ns,
        fingerprint,
    })
}

fn source_metadata_with_bytes(path: &Path, bytes: &[u8]) -> io::Result<SourceMetadata> {
    let metadata = std::fs::metadata(path)?;
    let len = metadata.len();
    let mtime_ns = metadata
        .modified()?
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .min(u64::MAX as u128) as u64;
    Ok(SourceMetadata {
        len,
        mtime_ns,
        fingerprint: sampled_content_fingerprint(bytes),
    })
}

pub(crate) fn editlog_path(source_path: &Path) -> PathBuf {
    let parent = source_path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = source_path
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("qem");
    parent.join(format!(".{file_name}.qem.editlog"))
}

fn filter_zero_len(mut pieces: Vec<Piece>) -> Vec<Piece> {
    pieces.retain(|piece| piece.len > 0);
    pieces
}

fn child_index_for_offset(children: &[ChildRef], offset: usize) -> (usize, usize) {
    if children.is_empty() {
        return (0, 0);
    }

    let mut child_start = 0usize;
    for (index, child) in children.iter().enumerate() {
        let child_end = child_start.saturating_add(child.total_bytes);
        if offset < child_end || index + 1 == children.len() {
            return (index, child_start);
        }
        child_start = child_end;
    }
    (children.len().saturating_sub(1), child_start)
}

fn insert_piece_into_leaf<F>(
    mut pieces: Vec<Piece>,
    offset: usize,
    piece: Piece,
    split_piece: &mut F,
) -> Vec<Piece>
where
    F: FnMut(Piece, usize) -> (Option<Piece>, Option<Piece>),
{
    let mut acc = 0usize;
    for index in 0..pieces.len() {
        let cur = pieces[index];
        let piece_end = acc.saturating_add(cur.len);
        if offset <= piece_end {
            let inner = offset.saturating_sub(acc).min(cur.len);
            if inner == 0 {
                pieces.insert(index, piece);
            } else if inner >= cur.len {
                pieces.insert(index + 1, piece);
            } else {
                let (left, right) = split_piece(cur, inner);
                let replacement = [left, Some(piece), right]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>();
                pieces.splice(index..=index, replacement);
            }
            return pieces;
        }
        acc = piece_end;
    }

    pieces.push(piece);
    pieces
}

fn delete_range_from_leaf<F>(
    pieces: Vec<Piece>,
    start: usize,
    end: usize,
    trim_piece: &mut F,
) -> Vec<Piece>
where
    F: FnMut(Piece, usize, usize) -> (Option<Piece>, Option<Piece>),
{
    let mut out = Vec::with_capacity(pieces.len());
    let mut acc = 0usize;
    for piece in pieces {
        let piece_end = acc.saturating_add(piece.len);
        if piece_end <= start || acc >= end {
            out.push(piece);
            acc = piece_end;
            continue;
        }

        let local_start = start.saturating_sub(acc).min(piece.len);
        let local_end = end.min(piece_end).saturating_sub(acc).min(piece.len);
        let (left, right) = trim_piece(piece, local_start, local_end);
        if let Some(left) = left {
            out.push(left);
        }
        if let Some(right) = right {
            out.push(right);
        }

        acc = piece_end;
    }
    out
}

fn coalesce_adjacent(pieces: &mut Vec<Piece>) {
    if pieces.len() < 2 {
        return;
    }
    let mut out: Vec<Piece> = Vec::with_capacity(pieces.len());
    for piece in pieces.drain(..) {
        if let Some(last) = out.last_mut() {
            let merge_original = last.src == PieceSource::Original
                && last.line_breaks == 0
                && piece.line_breaks == 0;
            let merge_add = last.src == PieceSource::Add;
            if last.src == piece.src
                && last.start + last.len == piece.start
                && (merge_original || merge_add)
            {
                last.len = last.len.saturating_add(piece.len);
                last.line_breaks = last.line_breaks.saturating_add(piece.line_breaks);
                continue;
            }
        }
        out.push(piece);
    }
    *pieces = out;
}

#[cfg(test)]
mod tests {
    use super::{
        editlog_path, parse_page, serialize_page, source_metadata, DiskPageStore,
        FragmentationStats, Piece, PiecePage, PieceSource, PieceTree, SessionMeta, SourceMetadata,
        EDITLOG_HEADER_BYTES, RESIDENT_PAGES_FAILED_PERSISTENCE_CAP,
    };
    use std::fs;
    use std::io::{Seek, SeekFrom, Write};

    fn collect(tree: &PieceTree) -> Vec<(PieceSource, usize, usize, usize)> {
        tree.to_vec()
            .into_iter()
            .map(|piece| (piece.src, piece.start, piece.len, piece.line_breaks))
            .collect()
    }

    #[test]
    fn insert_at_head_is_logical_and_coalesces_adjacent_add_pieces() {
        let mut tree = PieceTree::from_pieces(vec![Piece {
            src: PieceSource::Original,
            start: 0,
            len: 10,
            line_breaks: 1,
        }]);

        let mut split = |piece: Piece, left_len: usize| {
            let left = Piece {
                src: piece.src,
                start: piece.start,
                len: left_len,
                line_breaks: 0,
            };
            let right = Piece {
                src: piece.src,
                start: piece.start + left_len,
                len: piece.len - left_len,
                line_breaks: piece.line_breaks,
            };
            (Some(left), Some(right))
        };

        tree.insert(
            0,
            Piece {
                src: PieceSource::Add,
                start: 0,
                len: 3,
                line_breaks: 1,
            },
            &mut split,
        )
        .unwrap();
        tree.insert(
            3,
            Piece {
                src: PieceSource::Add,
                start: 3,
                len: 2,
                line_breaks: 0,
            },
            &mut split,
        )
        .unwrap();

        assert_eq!(
            collect(&tree),
            vec![
                (PieceSource::Add, 0, 5, 1),
                (PieceSource::Original, 0, 10, 1),
            ]
        );
        assert_eq!(tree.total_len(), 15);
        assert_eq!(tree.total_line_breaks(), 2);
    }

    #[test]
    fn delete_range_splices_across_piece_boundaries() {
        let mut tree = PieceTree::from_pieces(vec![
            Piece {
                src: PieceSource::Original,
                start: 0,
                len: 4,
                line_breaks: 1,
            },
            Piece {
                src: PieceSource::Add,
                start: 0,
                len: 3,
                line_breaks: 1,
            },
            Piece {
                src: PieceSource::Original,
                start: 4,
                len: 5,
                line_breaks: 0,
            },
        ]);

        let mut trim = |piece: Piece, local_start: usize, local_end: usize| {
            let left_len = local_start.min(piece.len);
            let right_len = piece.len.saturating_sub(local_end.min(piece.len));
            let left = (left_len > 0).then_some(Piece {
                src: piece.src,
                start: piece.start,
                len: left_len,
                line_breaks: usize::from(piece.line_breaks > 0 && left_len == piece.len),
            });
            let right = (right_len > 0).then_some(Piece {
                src: piece.src,
                start: piece.start + (piece.len - right_len),
                len: right_len,
                line_breaks: 0,
            });
            (left, right)
        };

        tree.delete_range(2, 6, &mut trim).unwrap();

        assert_eq!(
            collect(&tree),
            vec![
                (PieceSource::Original, 0, 2, 0),
                (PieceSource::Original, 5, 4, 0),
            ]
        );
        assert_eq!(tree.total_len(), 6);
    }

    #[test]
    fn coalesce_adjacent_preserves_original_break_boundaries() {
        let mut pieces = vec![
            Piece {
                src: PieceSource::Original,
                start: 0,
                len: 64,
                line_breaks: 0,
            },
            Piece {
                src: PieceSource::Original,
                start: 64,
                len: 8,
                line_breaks: 1,
            },
            Piece {
                src: PieceSource::Original,
                start: 72,
                len: 64,
                line_breaks: 0,
            },
            Piece {
                src: PieceSource::Add,
                start: 0,
                len: 2,
                line_breaks: 1,
            },
            Piece {
                src: PieceSource::Add,
                start: 2,
                len: 3,
                line_breaks: 0,
            },
        ];

        super::coalesce_adjacent(&mut pieces);

        assert_eq!(
            pieces,
            vec![
                Piece {
                    src: PieceSource::Original,
                    start: 0,
                    len: 64,
                    line_breaks: 0,
                },
                Piece {
                    src: PieceSource::Original,
                    start: 64,
                    len: 8,
                    line_breaks: 1,
                },
                Piece {
                    src: PieceSource::Original,
                    start: 72,
                    len: 64,
                    line_breaks: 0,
                },
                Piece {
                    src: PieceSource::Add,
                    start: 0,
                    len: 5,
                    line_breaks: 1,
                },
            ]
        );
    }

    #[test]
    fn visit_range_only_emits_overlapping_piece_segments() {
        let tree = PieceTree::from_pieces(vec![
            Piece {
                src: PieceSource::Original,
                start: 0,
                len: 4,
                line_breaks: 1,
            },
            Piece {
                src: PieceSource::Add,
                start: 10,
                len: 4,
                line_breaks: 0,
            },
        ]);

        let mut seen = Vec::new();
        tree.visit_range(2, 6, |piece, local_start, local_end| {
            seen.push((
                piece.src,
                piece.start + local_start,
                local_end - local_start,
            ));
        });

        assert_eq!(
            seen,
            vec![(PieceSource::Original, 2, 2), (PieceSource::Add, 10, 2)]
        );
    }

    #[test]
    fn visit_range_rev_emits_overlapping_piece_segments_in_reverse_order() {
        let tree = PieceTree::from_pieces(vec![
            Piece {
                src: PieceSource::Original,
                start: 0,
                len: 4,
                line_breaks: 0,
            },
            Piece {
                src: PieceSource::Add,
                start: 10,
                len: 3,
                line_breaks: 0,
            },
            Piece {
                src: PieceSource::Original,
                start: 4,
                len: 2,
                line_breaks: 0,
            },
        ]);

        let mut seen = Vec::new();
        tree.visit_range_rev_while(2, 8, |piece, local_start, local_end| {
            seen.push((
                piece.src,
                piece.start + local_start,
                local_end - local_start,
            ));
            true
        });

        assert_eq!(
            seen,
            vec![
                (PieceSource::Original, 4, 1),
                (PieceSource::Add, 10, 3),
                (PieceSource::Original, 2, 2),
            ]
        );
    }

    #[test]
    fn visit_range_while_stops_after_callback_returns_false() {
        let tree = PieceTree::from_pieces(vec![
            Piece {
                src: PieceSource::Original,
                start: 0,
                len: 4,
                line_breaks: 0,
            },
            Piece {
                src: PieceSource::Add,
                start: 10,
                len: 3,
                line_breaks: 0,
            },
            Piece {
                src: PieceSource::Original,
                start: 20,
                len: 2,
                line_breaks: 0,
            },
        ]);

        let mut seen = Vec::new();
        let completed = tree.visit_range_while(0, 9, |piece, local_start, local_end| {
            seen.push((
                piece.src,
                piece.start + local_start,
                local_end.saturating_sub(local_start),
            ));
            seen.len() < 2
        });

        assert!(!completed);
        assert_eq!(
            seen,
            vec![(PieceSource::Original, 0, 4), (PieceSource::Add, 10, 3)]
        );
    }

    #[test]
    fn visit_range_rev_while_stops_after_callback_returns_false() {
        let tree = PieceTree::from_pieces(vec![
            Piece {
                src: PieceSource::Original,
                start: 0,
                len: 4,
                line_breaks: 0,
            },
            Piece {
                src: PieceSource::Add,
                start: 10,
                len: 3,
                line_breaks: 0,
            },
            Piece {
                src: PieceSource::Original,
                start: 20,
                len: 2,
                line_breaks: 0,
            },
        ]);

        let mut seen = Vec::new();
        let completed = tree.visit_range_rev_while(0, 9, |piece, local_start, local_end| {
            seen.push((
                piece.src,
                piece.start + local_start,
                local_end.saturating_sub(local_start),
            ));
            seen.len() < 2
        });

        assert!(!completed);
        assert_eq!(
            seen,
            vec![(PieceSource::Original, 20, 2), (PieceSource::Add, 10, 3)]
        );
    }

    #[test]
    fn find_line_start_uses_weighted_breaks() {
        let tree = PieceTree::from_pieces(vec![
            Piece {
                src: PieceSource::Original,
                start: 0,
                len: 4,
                line_breaks: 1,
            },
            Piece {
                src: PieceSource::Original,
                start: 4,
                len: 5,
                line_breaks: 1,
            },
        ]);

        let found = tree.find_line_start(2, |piece, local_break| {
            if piece.start == 4 && local_break == 0 {
                Some(3)
            } else {
                None
            }
        });

        assert_eq!(found, Some(7));
    }

    #[test]
    fn disk_backed_tree_round_trips_through_editlog() {
        let dir = std::env::temp_dir().join(format!("qem-piece-tree-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let source = dir.join("sample.txt");
        fs::write(&source, b"abc").unwrap();
        let tree = PieceTree::from_pieces_disk(
            &source,
            vec![
                Piece {
                    src: PieceSource::Original,
                    start: 0,
                    len: 4,
                    line_breaks: 1,
                },
                Piece {
                    src: PieceSource::Add,
                    start: 2,
                    len: 3,
                    line_breaks: 0,
                },
            ],
        )
        .unwrap();
        assert_eq!(tree.total_len(), 7);
        drop(tree);

        let reopened = PieceTree::open_disk(&source).unwrap();
        let sidecar = editlog_path(&source);
        assert_eq!(
            collect(&reopened),
            vec![
                (PieceSource::Original, 0, 4, 1),
                (PieceSource::Add, 2, 3, 0),
            ]
        );

        let _ = fs::remove_file(&sidecar);
        let _ = fs::remove_file(&source);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn disk_backed_tree_persists_latest_root_after_edit() {
        let dir = std::env::temp_dir().join(format!("qem-piece-tree-edit-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let source = dir.join("sample.txt");
        fs::write(&source, b"abc").unwrap();
        let mut tree = PieceTree::from_pieces_disk(
            &source,
            vec![Piece {
                src: PieceSource::Original,
                start: 0,
                len: 6,
                line_breaks: 1,
            }],
        )
        .unwrap();

        let mut split = |piece: Piece, left_len: usize| {
            let left = Piece {
                src: piece.src,
                start: piece.start,
                len: left_len,
                line_breaks: 0,
            };
            let right = Piece {
                src: piece.src,
                start: piece.start + left_len,
                len: piece.len - left_len,
                line_breaks: piece.line_breaks,
            };
            (Some(left), Some(right))
        };

        tree.insert(
            3,
            Piece {
                src: PieceSource::Add,
                start: 10,
                len: 2,
                line_breaks: 1,
            },
            &mut split,
        )
        .unwrap();
        tree.flush_session(b"ignored-by-test", SessionMeta::default())
            .unwrap();
        drop(tree);

        let reopened = PieceTree::open_disk(&source).unwrap();
        let sidecar = editlog_path(&source);
        assert_eq!(
            collect(&reopened),
            vec![
                (PieceSource::Original, 0, 3, 0),
                (PieceSource::Add, 10, 2, 1),
                (PieceSource::Original, 3, 3, 1),
            ]
        );

        let _ = fs::remove_file(&sidecar);
        let _ = fs::remove_file(&source);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn disk_open_truncates_orphan_pages_from_an_interrupted_flush() {
        // A crash between the page append and the header rewrite leaves
        // orphan pages past the committed size. Page writes append at
        // `SeekFrom::End` and assert the offset against the page id, so the
        // orphans would poison every later flush into `persistence_failed`
        // and silently disable persistence for the session. Open must
        // truncate back to the committed size (the header is the commit
        // record; the orphans are unreachable by design).
        let dir = std::env::temp_dir().join(format!(
            "qem-piece-tree-orphan-pages-{}",
            std::process::id()
        ));
        let _ = fs::create_dir_all(&dir);
        let source = dir.join("sample.txt");
        fs::write(&source, b"abcdef\n").unwrap();
        let mut tree = PieceTree::from_pieces_disk(
            &source,
            vec![Piece {
                src: PieceSource::Original,
                start: 0,
                len: 7,
                line_breaks: 1,
            }],
        )
        .unwrap();
        let mut split = |piece: Piece, left_len: usize| {
            let left = Piece {
                src: piece.src,
                start: piece.start,
                len: left_len,
                line_breaks: 0,
            };
            let right = Piece {
                src: piece.src,
                start: piece.start + left_len,
                len: piece.len - left_len,
                line_breaks: piece.line_breaks,
            };
            (Some(left), Some(right))
        };
        tree.insert(
            3,
            Piece {
                src: PieceSource::Add,
                start: 10,
                len: 2,
                line_breaks: 1,
            },
            &mut split,
        )
        .unwrap();
        tree.flush_session(b"ignored-by-test", SessionMeta::default())
            .unwrap();
        drop(tree);

        let sidecar = editlog_path(&source);
        {
            let mut file = fs::OpenOptions::new().append(true).open(&sidecar).unwrap();
            file.write_all(&[0u8; 2 * 4096]).unwrap();
        }

        let mut reopened = PieceTree::open_disk(&source).unwrap();
        reopened
            .insert(
                1,
                Piece {
                    src: PieceSource::Add,
                    start: 12,
                    len: 1,
                    line_breaks: 0,
                },
                &mut split,
            )
            .unwrap();
        reopened
            .flush_session(b"ignored-by-test", SessionMeta::default())
            .unwrap();
        drop(reopened);

        let again = PieceTree::open_disk(&source).unwrap();
        assert_eq!(
            again.total_len(),
            10,
            "the post-orphan edit must persist; a poisoned editlog would have \
             recovered the pre-edit session instead"
        );

        let _ = fs::remove_file(&sidecar);
        let _ = fs::remove_file(&source);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn disk_open_surfaces_corrupt_history_counters_as_typed_errors() {
        let dir = std::env::temp_dir().join(format!(
            "qem-piece-tree-corrupt-history-{}",
            std::process::id()
        ));
        let _ = fs::create_dir_all(&dir);
        let source = dir.join("sample.txt");
        fs::write(&source, b"abcdef\n").unwrap();
        let mut tree = PieceTree::from_pieces_disk(
            &source,
            vec![Piece {
                src: PieceSource::Original,
                start: 0,
                len: 7,
                line_breaks: 1,
            }],
        )
        .unwrap();
        let mut split = |piece: Piece, left_len: usize| {
            let left = Piece {
                src: piece.src,
                start: piece.start,
                len: left_len,
                line_breaks: 0,
            };
            let right = Piece {
                src: piece.src,
                start: piece.start + left_len,
                len: piece.len - left_len,
                line_breaks: piece.line_breaks,
            };
            (Some(left), Some(right))
        };
        tree.insert(
            3,
            Piece {
                src: PieceSource::Add,
                start: 10,
                len: 2,
                line_breaks: 1,
            },
            &mut split,
        )
        .unwrap();
        tree.flush_session(b"ignored-by-test", SessionMeta::default())
            .unwrap();
        drop(tree);

        let sidecar = editlog_path(&source);
        {
            let mut file = fs::OpenOptions::new().write(true).open(&sidecar).unwrap();
            file.seek(SeekFrom::Start(56)).unwrap();
            // history_page_count and history_len are raw header fields with
            // no checksum. A corrupt sidecar must surface as a typed error
            // instead of driving huge allocations or out-of-bounds reads.
            file.write_all(&(u64::MAX / 2).to_le_bytes()).unwrap();
            file.write_all(&(u64::MAX / 2).to_le_bytes()).unwrap();
            file.flush().unwrap();
        }

        let err = PieceTree::open_disk(&source).unwrap_err();
        assert!(matches!(
            err.kind(),
            std::io::ErrorKind::InvalidData | std::io::ErrorKind::UnexpectedEof
        ));

        let _ = fs::remove_file(&sidecar);
        let _ = fs::remove_file(&source);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn disk_open_rejects_page_count_below_referenced_pages() {
        // `page_count` is a raw unchecksummed header field that drives
        // the committed-size truncation. A corrupt-small value must not
        // silently truncate pages the history still references (lazy
        // page misses then read short while total_len stays full); it
        // must surface as a typed error so the session sidecar is
        // dropped and rebuilt instead.
        let dir = std::env::temp_dir().join(format!(
            "qem-piece-tree-page-count-small-{}",
            std::process::id()
        ));
        let _ = fs::create_dir_all(&dir);
        let source = dir.join("sample.txt");
        fs::write(&source, b"abcdef\n").unwrap();
        let mut tree = PieceTree::from_pieces_disk(
            &source,
            vec![Piece {
                src: PieceSource::Original,
                start: 0,
                len: 7,
                line_breaks: 1,
            }],
        )
        .unwrap();
        let mut split = |piece: Piece, left_len: usize| {
            let left = Piece {
                src: piece.src,
                start: piece.start,
                len: left_len,
                line_breaks: 0,
            };
            let right = Piece {
                src: piece.src,
                start: piece.start + left_len,
                len: piece.len - left_len,
                line_breaks: piece.line_breaks,
            };
            (Some(left), Some(right))
        };
        tree.insert(
            3,
            Piece {
                src: PieceSource::Add,
                start: 10,
                len: 2,
                line_breaks: 1,
            },
            &mut split,
        )
        .unwrap();
        tree.flush_session(b"ignored-by-test", SessionMeta::default())
            .unwrap();
        drop(tree);

        let sidecar = editlog_path(&source);
        {
            let mut file = fs::OpenOptions::new().write(true).open(&sidecar).unwrap();
            file.seek(SeekFrom::Start(16)).unwrap();
            // Real sessions span several pages (piece page, history page,
            // add page); 1 is below every referenced page id.
            file.write_all(&1u64.to_le_bytes()).unwrap();
            file.flush().unwrap();
        }

        let err = PieceTree::open_disk(&source).unwrap_err();
        assert_eq!(
            err.kind(),
            std::io::ErrorKind::InvalidData,
            "a page_count below the referenced pages must reject the session"
        );

        let _ = fs::remove_file(&sidecar);
        let _ = fs::remove_file(&source);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn disk_open_rejects_page_count_beyond_the_sidecar_size() {
        // A corrupt-huge `page_count` must not become `next_page_id` (the
        // next flush computes page offsets with a non-saturating
        // multiply) nor extend the sidecar at open. It is an invalid
        // session, not a truncation hint.
        let dir = std::env::temp_dir().join(format!(
            "qem-piece-tree-page-count-huge-{}",
            std::process::id()
        ));
        let _ = fs::create_dir_all(&dir);
        let source = dir.join("sample.txt");
        fs::write(&source, b"abcdef\n").unwrap();
        let mut tree = PieceTree::from_pieces_disk(
            &source,
            vec![Piece {
                src: PieceSource::Original,
                start: 0,
                len: 7,
                line_breaks: 1,
            }],
        )
        .unwrap();
        let mut split = |piece: Piece, left_len: usize| {
            let left = Piece {
                src: piece.src,
                start: piece.start,
                len: left_len,
                line_breaks: 0,
            };
            let right = Piece {
                src: piece.src,
                start: piece.start + left_len,
                len: piece.len - left_len,
                line_breaks: piece.line_breaks,
            };
            (Some(left), Some(right))
        };
        tree.insert(
            3,
            Piece {
                src: PieceSource::Add,
                start: 10,
                len: 2,
                line_breaks: 1,
            },
            &mut split,
        )
        .unwrap();
        tree.flush_session(b"ignored-by-test", SessionMeta::default())
            .unwrap();
        drop(tree);

        let sidecar = editlog_path(&source);
        {
            let mut file = fs::OpenOptions::new().write(true).open(&sidecar).unwrap();
            file.seek(SeekFrom::Start(16)).unwrap();
            file.write_all(&u64::MAX.to_le_bytes()).unwrap();
            file.flush().unwrap();
        }

        let err = PieceTree::open_disk(&source).unwrap_err();
        assert_eq!(
            err.kind(),
            std::io::ErrorKind::InvalidData,
            "a page_count beyond the sidecar size must reject the session"
        );

        let _ = fs::remove_file(&sidecar);
        let _ = fs::remove_file(&source);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn disk_edit_fails_typed_when_a_piece_page_is_unreadable() {
        // A lazily-read piece page whose bytes no longer pass the page
        // CRC must not let a structural edit silently splice the
        // subtree out: an unreadable page used to answer an empty child
        // list, the parent recomputed its totals over the survivors,
        // and the next edit shrank the document while `total_len` still
        // advertised the full size. The edit must fail as a typed error
        // so the caller keeps the tree intact.
        let dir = std::env::temp_dir().join(format!(
            "qem-piece-tree-unreadable-piece-page-{}",
            std::process::id()
        ));
        let _ = fs::create_dir_all(&dir);
        let source = dir.join("sample.txt");
        fs::write(&source, b"abcdef\n").unwrap();
        let mut tree = PieceTree::from_pieces_disk(
            &source,
            vec![Piece {
                src: PieceSource::Original,
                start: 0,
                len: 7,
                line_breaks: 1,
            }],
        )
        .unwrap();
        let mut split = |piece: Piece, left_len: usize| {
            let left = Piece {
                src: piece.src,
                start: piece.start,
                len: left_len,
                line_breaks: 0,
            };
            let right = Piece {
                src: piece.src,
                start: piece.start + left_len,
                len: piece.len - left_len,
                line_breaks: piece.line_breaks,
            };
            (Some(left), Some(right))
        };
        tree.flush_session(b"ignored-by-test", SessionMeta::default())
            .unwrap();
        drop(tree);

        let sidecar = editlog_path(&source);
        {
            // Piece page 1 holds the original pieces; scramble its
            // payload so the page CRC fails on the next lazy read.
            let mut file = fs::OpenOptions::new().write(true).open(&sidecar).unwrap();
            file.seek(SeekFrom::Start((EDITLOG_HEADER_BYTES + 8) as u64))
                .unwrap();
            file.write_all(&[0xFFu8; 16]).unwrap();
            file.flush().unwrap();
        }

        let mut reopened = PieceTree::open_disk(&source).unwrap();
        let insert_result = reopened.insert(
            3,
            Piece {
                src: PieceSource::Add,
                start: 10,
                len: 2,
                line_breaks: 1,
            },
            &mut split,
        );
        assert!(
            insert_result.is_err(),
            "editing through an unreadable piece page must fail, not lose the subtree"
        );
        assert_eq!(
            reopened.total_len(),
            7,
            "the refused edit must leave the tree content intact"
        );

        let _ = fs::remove_file(&sidecar);
        let _ = fs::remove_file(&source);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn disk_open_survives_a_corrupt_history_page_id_as_typed_error() {
        // `history_first_page_id` is another raw unchecksummed header
        // field. A corrupt-huge value must not overflow the page-offset
        // arithmetic (debug builds panic on the multiply), it must
        // surface as a typed IO error like every other corrupt counter.
        let dir = std::env::temp_dir().join(format!(
            "qem-piece-tree-corrupt-history-page-id-{}",
            std::process::id()
        ));
        let _ = fs::create_dir_all(&dir);
        let source = dir.join("sample.txt");
        fs::write(&source, b"abcdef\n").unwrap();
        let mut tree = PieceTree::from_pieces_disk(
            &source,
            vec![Piece {
                src: PieceSource::Original,
                start: 0,
                len: 7,
                line_breaks: 1,
            }],
        )
        .unwrap();
        let mut split = |piece: Piece, left_len: usize| {
            let left = Piece {
                src: piece.src,
                start: piece.start,
                len: left_len,
                line_breaks: 0,
            };
            let right = Piece {
                src: piece.src,
                start: piece.start + left_len,
                len: piece.len - left_len,
                line_breaks: piece.line_breaks,
            };
            (Some(left), Some(right))
        };
        tree.insert(
            3,
            Piece {
                src: PieceSource::Add,
                start: 10,
                len: 2,
                line_breaks: 1,
            },
            &mut split,
        )
        .unwrap();
        tree.flush_session(b"ignored-by-test", SessionMeta::default())
            .unwrap();
        drop(tree);

        let sidecar = editlog_path(&source);
        {
            let mut file = fs::OpenOptions::new().write(true).open(&sidecar).unwrap();
            file.seek(SeekFrom::Start(48)).unwrap();
            file.write_all(&u64::MAX.to_le_bytes()).unwrap();
            file.flush().unwrap();
        }

        let err = PieceTree::open_disk(&source).unwrap_err();
        assert!(
            matches!(err.kind(), std::io::ErrorKind::InvalidData | std::io::ErrorKind::UnexpectedEof)
                // Windows maps the out-of-range page-id seek to raw OS
                // errors instead of UnexpectedEof: 131 =
                // ERROR_NEGATIVE_SEEK, 38 = ERROR_HANDLE_EOF.
                || matches!(err.raw_os_error(), Some(38) | Some(131)),
            "actual kind: {:?}, raw: {:?}",
            err.kind(),
            err.raw_os_error()
        );

        let _ = fs::remove_file(&sidecar);
        let _ = fs::remove_file(&source);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_page_rejects_empty_internal_pages_as_invalid() {
        // The writer never emits an empty internal page; a CRC-valid
        // forged one would panic the insert path and silently empty the
        // document on delete, so parse must reject it up front.
        let page = PiecePage::Internal {
            children: Vec::new(),
        };
        let buf = serialize_page(&page).unwrap();
        let err = parse_page(&buf).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn disk_resident_window_refuses_edits_before_losing_pages() {
        // While persistence is down every new piece page lives only in
        // the bounded resident window (4096). An edit whose page the
        // window refused must fail typed instead of landing: otherwise
        // the recorded root would reference unreadable pages and reads
        // would silently shorten behind `total_len` once the page cache
        // (1024) evicted them too.
        let dir = std::env::temp_dir().join(format!(
            "qem-piece-tree-resident-window-{}",
            std::process::id()
        ));
        let _ = fs::create_dir_all(&dir);
        let source = dir.join("sample.txt");
        fs::write(&source, b"abcdef\n").unwrap();
        let mut tree = PieceTree::from_pieces_disk(
            &source,
            vec![Piece {
                src: PieceSource::Original,
                start: 0,
                len: 7,
                line_breaks: 1,
            }],
        )
        .unwrap();
        let mut split = |piece: Piece, left_len: usize| {
            let left = Piece {
                src: piece.src,
                start: piece.start,
                len: left_len,
                line_breaks: 0,
            };
            let right = Piece {
                src: piece.src,
                start: piece.start + left_len,
                len: piece.len - left_len,
                line_breaks: piece.line_breaks,
            };
            (Some(left), Some(right))
        };
        tree.flush_session(b"ignored-by-test", SessionMeta::default())
            .unwrap();
        tree.poison_persistence_for_test();

        // Single-byte inserts at the tail; every copy-on-write rebuild
        // allocates a fresh page, so the refusal must arrive around the
        // window size instead of never.
        let mut accepted = 0usize;
        let mut refused = false;
        for i in 0..(RESIDENT_PAGES_FAILED_PERSISTENCE_CAP * 2) {
            let insert_result = tree.insert(
                7 + accepted,
                Piece {
                    src: PieceSource::Add,
                    start: i,
                    len: 1,
                    line_breaks: 0,
                },
                &mut split,
            );
            match insert_result {
                Ok(()) => accepted += 1,
                Err(_) => {
                    refused = true;
                    break;
                }
            }
        }
        assert!(refused, "the resident window must eventually refuse edits");
        assert!(
            accepted >= RESIDENT_PAGES_FAILED_PERSISTENCE_CAP - 64,
            "the window must still give edits room before refusing (accepted {accepted})"
        );
        assert_eq!(
            tree.total_len(),
            7 + accepted,
            "the refused edit must not change the tree size"
        );

        // Reads must stay complete: walking the live root collects every
        // piece, so a silent unreadable-page skip would shrink the sum
        // below total_len.
        let pieces = tree.to_vec();
        let collected_bytes: usize = pieces.iter().map(|piece| piece.len).sum();
        assert_eq!(
            collected_bytes,
            7 + accepted,
            "every referenced page must stay readable past the refusal"
        );

        let sidecar = editlog_path(&source);
        let _ = fs::remove_file(&sidecar);
        let _ = fs::remove_file(&source);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn disk_open_rejects_stale_content_fingerprint_even_with_matching_file_metadata() {
        let dir = std::env::temp_dir().join(format!(
            "qem-piece-tree-stale-fingerprint-{}",
            std::process::id()
        ));
        let _ = fs::create_dir_all(&dir);
        let source = dir.join("sample.txt");
        fs::write(&source, b"abcdef\n").unwrap();
        let tree = PieceTree::from_pieces_disk(
            &source,
            vec![Piece {
                src: PieceSource::Original,
                start: 0,
                len: 7,
                line_breaks: 1,
            }],
        )
        .unwrap();
        drop(tree);

        let source_meta = source_metadata(&source).unwrap();
        let stale = SourceMetadata {
            len: source_meta.len,
            mtime_ns: source_meta.mtime_ns,
            fingerprint: source_meta.fingerprint ^ 1,
        };
        let reopened = DiskPageStore::open(&source, stale).unwrap();
        assert!(reopened.is_none());

        let sidecar = editlog_path(&source);
        let _ = fs::remove_file(&sidecar);
        let _ = fs::remove_file(&source);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn fragmentation_stats_report_piece_size_distribution() {
        let tree = PieceTree::from_pieces(vec![
            Piece {
                src: PieceSource::Original,
                start: 0,
                len: 4096,
                line_breaks: 1,
            },
            Piece {
                src: PieceSource::Add,
                start: 0,
                len: 16,
                line_breaks: 0,
            },
            Piece {
                src: PieceSource::Add,
                start: 64,
                len: 12,
                line_breaks: 0,
            },
            Piece {
                src: PieceSource::Original,
                start: 4096,
                len: 8192,
                line_breaks: 2,
            },
        ]);

        let stats = tree.fragmentation_stats_with_threshold(64);

        assert_eq!(
            stats,
            FragmentationStats {
                piece_count: 4,
                total_bytes: 12_316,
                smallest_piece_bytes: 12,
                largest_piece_bytes: 8192,
                small_piece_threshold_bytes: 64,
                small_piece_count: 2,
                small_piece_bytes: 28,
            }
        );
        assert!((stats.average_piece_bytes() - 3079.0).abs() < f64::EPSILON);
        assert!((stats.fragmentation_ratio() - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn fragmentation_stats_for_empty_tree_are_zeroed() {
        let stats = PieceTree::new().fragmentation_stats();

        assert_eq!(
            stats,
            FragmentationStats {
                small_piece_threshold_bytes: super::DEFAULT_FRAGMENTATION_SMALL_PIECE_BYTES,
                ..FragmentationStats::default()
            }
        );
        assert_eq!(stats.average_piece_bytes(), 0.0);
        assert_eq!(stats.fragmentation_ratio(), 0.0);
    }

    #[test]
    fn bounded_page_count_caps_counters_by_physical_file_size() {
        use super::{bounded_page_count, EDITLOG_HEADER_BYTES, EDITLOG_PAGE_SIZE};

        let header_only = EDITLOG_HEADER_BYTES as u64;
        // A sidecar that only holds the header cannot hold any pages.
        assert_eq!(bounded_page_count(Some(header_only), 5), 0);
        // Without a readable file length the raw counter must never reach an
        // allocation: the chain is treated as empty instead.
        assert_eq!(bounded_page_count(None, usize::MAX), 0);

        let three_pages = header_only + (EDITLOG_PAGE_SIZE as u64) * 3;
        assert_eq!(bounded_page_count(Some(three_pages), 100), 3);
        assert_eq!(bounded_page_count(Some(three_pages), 2), 2);
        // Huge files clamp to the caller-provided counter, not the other way.
        assert_eq!(bounded_page_count(Some(u64::MAX), 7), 7);
    }
}
