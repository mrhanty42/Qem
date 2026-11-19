use crate::storage::FileStorage;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use btree::{ChildRef, PiecePage};
use history::RootEntry;
use store::{source_metadata_with_bytes, DiskPageStore, PageStore};

mod btree;
mod history;
mod session;
mod store;

#[cfg(test)]
use btree::{coalesce_adjacent, parse_page, serialize_page};
#[cfg(test)]
use session::bounded_page_count;
pub(crate) use session::SessionMeta;
#[cfg(test)]
use store::{source_metadata, SourceMetadata};

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
//
// v3 stores the document encoding as a NUL-padded `encoding_rs` label in the
// header tail (offset 128..192) so a reopened session can restore the exact
// encoding contract instead of forcing the caller to guess it again. v2
// sidecars carry no label and are rejected by the version check; recovery
// drops them and rebuilds from the source file.
const EDITLOG_MAGIC: &[u8; 8] = b"QEMEDT1\0";
const EDITLOG_VERSION: u32 = 3;
const EDITLOG_PAGE_SIZE: usize = 4096;
const EDITLOG_HEADER_BYTES: usize = 192;
/// v3 header tail: NUL-padded `encoding_rs` label of the document encoding.
pub(crate) const EDITLOG_ENCODING_LABEL_OFFSET: usize = 128;
pub(crate) const EDITLOG_ENCODING_LABEL_BYTES: usize = 64;
const EDITLOG_PAGE_HEADER_BYTES: usize = 24;
const EDITLOG_CACHE_PAGES: usize = 1024;
/// Test-only mirror of `EDITLOG_CACHE_PAGES` so document-level tests can
/// churn the read cache past its bound (see the failed-persistence save
/// regression).
#[cfg(test)]
pub(crate) const EDITLOG_READ_CACHE_PAGES_FOR_TEST: usize = EDITLOG_CACHE_PAGES;
/// Bound on pages kept resident while editlog persistence is down:
/// each is an Arc'd page that only this map holds (see `alloc_page`).
pub(crate) const RESIDENT_PAGES_FAILED_PERSISTENCE_CAP: usize = 4096;
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
enum PageKind {
    Internal = 1,
    Leaf = 2,
    History = 3,
    Add = 4,
}

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
        let tree = Self {
            store: PageStore::Disk(store),
            root,
            history,
            known_history,
            history_index,
            history_batch_depth: 0,
            history_batch_dirty: false,
        };
        // Cross-validate the raw (unchecksummed) header totals against the
        // committed root they claim to describe. The header is the only
        // checksummed-free place a divergent total can live: history and
        // piece pages carry per-page CRCs, so a flipped header byte is the
        // one corruption that changes a total while every page stays
        // readable. The known boundary is a prefix of the document, so it
        // can never exceed the committed root's byte count, and a full-index
        // session must cover the whole document, so its boundary equals it.
        // Without this check recovery silently clamped the disagreement
        // (`meta.known_byte_len.min(total_len)`), reopening with a line
        // index that claimed exactness over content it did not cover.
        let root_bytes = root.map(|child| child.total_bytes).unwrap_or(0);
        if meta.known_byte_len > root_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "qem editlog known_byte_len {} exceeds the committed root size {root_bytes}",
                    meta.known_byte_len
                ),
            ));
        }
        if meta.full_index && meta.known_byte_len != root_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "qem editlog full-index known_byte_len {} does not match the committed root size {root_bytes}",
                    meta.known_byte_len
                ),
            ));
        }
        // A lazily-read piece page that fails its CRC answers `None` and
        // the read paths silently skip the subtree, so a session whose
        // live root reaches an unreadable page would open and advertise
        // `total_len` over content it can never return. Verify every
        // page reachable from the committed root up front: an invalid
        // sidecar must surface as a typed error so the recovery layer
        // drops it and rebuilds from the source file.
        tree.verify_root_pages_readable()?;
        Ok(Some((tree, add, meta)))
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

    /// Test support: fills the resident window with dummy pages up to
    /// `target` entries so a test can pin the next edit against the very
    /// last free residency slot (the real pages already there stay).
    #[cfg(test)]
    pub(crate) fn fill_resident_window_for_test(&mut self, target: usize) {
        if let PageStore::Disk(store) = &mut self.store {
            let mut id = 1_000_000_000u64;
            while store.resident_pages.len() < target {
                store
                    .resident_pages
                    .insert(id, Arc::new(PiecePage::Leaf { pieces: Vec::new() }));
                id += 1;
            }
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
            // Even the empty-root rebuild allocates pages, so the
            // residency check covers it the same way.
            let first_new_page = self.store.next_page_id_hint();
            let new_root = self.build_tree_from_pieces(vec![piece]);
            if let Err(err) = self.store.verify_pages_resident_since(first_new_page) {
                self.root = None;
                return Err(err);
            }
            self.root = new_root;
            self.record_history_root();
            return Ok(());
        };
        let first_new_page = self.store.next_page_id_hint();
        // While persistence is down every new page lives only in the
        // bounded resident window (see `alloc_page`). The edit must not
        // land through a page that the window refused: reads would
        // silently skip it and the document would shrink behind
        // `total_len`. The verification runs AFTER the root rebuild,
        // which itself allocates pages on an overflowing edit.
        let new_root = match self.insert_at(root, offset, piece, split_piece) {
            Ok(children) => self.build_tree_from_child_refs(children),
            Err(err) => {
                self.root = Some(root);
                return Err(err);
            }
        };
        if let Err(err) = self.store.verify_pages_resident_since(first_new_page) {
            self.root = Some(root);
            return Err(err);
        }
        self.root = new_root;
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
        let new_root = match self.delete_at(root, start, end, trim_piece) {
            Ok(children) => self.build_tree_from_child_refs(children),
            Err(err) => {
                self.root = Some(root);
                return Err(err);
            }
        };
        // Same ordering as the insert path: the root rebuild allocates
        // pages on an overflowing edit, so the verification must see
        // them too.
        if let Err(err) = self.store.verify_pages_resident_since(first_new_page) {
            self.root = Some(root);
            return Err(err);
        }
        self.root = new_root;
        self.record_history_root();
        Ok(())
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

    pub(crate) fn detach_persistence(&mut self) {
        if !matches!(self.store, PageStore::Disk(_)) {
            return;
        }

        let pieces = self.to_vec();
        *self = Self::from_pieces(pieces);
    }

    fn page(&self, page_id: u64) -> Option<Arc<PiecePage>> {
        self.store.get_page(page_id)
    }

    /// Open-time readability check: every piece page reachable from the
    /// live root must be readable, because an unreadable page (CRC
    /// mismatch, IO failure) otherwise answers `None` and the read paths
    /// silently skip the subtree — a short read masquerading as a
    /// complete document under a full `total_len`. Rejecting the open as
    /// `InvalidData` lets the recovery layer drop the sidecar and
    /// rebuild from the source file instead.
    fn verify_root_pages_readable(&self) -> io::Result<()> {
        let Some(root) = self.root else {
            return Ok(());
        };
        self.verify_child_pages_readable(root)
    }

    fn verify_child_pages_readable(&self, child: ChildRef) -> io::Result<()> {
        let Some(page) = self.page(child.page_id) else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("qem session piece page {} is unreadable", child.page_id),
            ));
        };
        if let PiecePage::Internal { children } = page.as_ref() {
            for child in children {
                self.verify_child_pages_readable(*child)?;
            }
        }
        Ok(())
    }
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

#[cfg(test)]
mod tests;
