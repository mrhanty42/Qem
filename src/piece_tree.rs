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
/// Test-only mirror of `EDITLOG_HEADER_BYTES` so document-level tests can
/// target the first piece page of a session sidecar by offset (see the
/// unreadable-page read regression).
#[cfg(test)]
pub(crate) const EDITLOG_HEADER_BYTES_FOR_TEST: usize = EDITLOG_HEADER_BYTES;
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
        // A lazily-read piece page that fails its CRC answers `None`;
        // reads through such a page now fail as a typed error instead of
        // silently skipping the subtree, but a session whose live root
        // reaches an unreadable page still advertises `total_len` over
        // content it can never return. Verify every page reachable from
        // the committed root up front: an invalid sidecar must surface as
        // a typed error so the recovery layer drops it and rebuilds from
        // the source file.
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

    /// Materializes every piece reachable from the live root. Fails typed
    /// when any piece page is unreadable instead of silently dropping the
    /// subtree from the snapshot.
    pub(crate) fn to_vec_result(&self) -> io::Result<Vec<Piece>> {
        let mut out = Vec::new();
        if let Some(root) = self.root {
            self.collect_pieces_result(root, &mut out)?;
        }
        Ok(out)
    }

    /// Legacy silent snapshot used only where a missing piece page is
    /// already impossible or is handled by the caller; prefer
    /// [`PieceTree::to_vec_result`].
    #[cfg(test)]
    pub(crate) fn to_vec(&self) -> Vec<Piece> {
        self.to_vec_result()
            .expect("piece tree snapshot must stay readable in tests")
    }

    pub(crate) fn fragmentation_stats(&self) -> FragmentationStats {
        self.fragmentation_stats_with_threshold(DEFAULT_FRAGMENTATION_SMALL_PIECE_BYTES)
    }

    /// Advisory fragmentation statistics.
    ///
    /// Deliberately tolerant of an unreadable piece page: the stats feed
    /// maintenance advice (`DocumentMaintenanceStatus`), not content, and
    /// the only mutating consumer (`compact_current_state`) re-validates
    /// every byte it materializes through the honest
    /// `read_range_result` path and skips compaction on a read error.
    /// A silently undercounted snapshot can therefore only mis-rank
    /// advice, never lose content.
    pub(crate) fn fragmentation_stats_with_threshold(
        &self,
        small_piece_threshold_bytes: usize,
    ) -> FragmentationStats {
        let mut stats = FragmentationStats {
            small_piece_threshold_bytes,
            ..FragmentationStats::default()
        };
        if let Some(root) = self.root {
            let _ = self.collect_fragmentation_stats_result(root, &mut stats);
        }
        stats
    }

    /// Honest range visit: fails typed when a lazily read page overlapping
    /// `[start, end)` does not load, instead of silently skipping the
    /// subtree and shortening the read behind `total_len`.
    pub(crate) fn visit_range_result(
        &self,
        start: usize,
        end: usize,
        mut visit: impl FnMut(Piece, usize, usize),
    ) -> io::Result<()> {
        let Some(root) = self.root else {
            return Ok(());
        };
        let total = root.total_bytes;
        let start = start.min(total);
        let end = end.min(total).max(start);
        if start >= end {
            return Ok(());
        }
        self.visit_range_ref_result(root, start, end, 0, &mut visit)
    }

    /// Honest readability pre-check for a byte range: descends exactly
    /// like the visiting walkers without invoking a callback, so a caller
    /// can verify every page overlapping `[start, end)` is loadable before
    /// byte arithmetic that runs through helpers with silent internal
    /// reads.
    pub(crate) fn verify_range_readable(&self, start: usize, end: usize) -> io::Result<()> {
        let Some(root) = self.root else {
            return Ok(());
        };
        let total = root.total_bytes;
        let start = start.min(total);
        let end = end.min(total).max(start);
        if start >= end {
            return Ok(());
        }
        self.verify_range_ref_readable(root, start, end, 0)
    }

    pub(crate) fn visit_range_while_result(
        &self,
        start: usize,
        end: usize,
        mut visit: impl FnMut(Piece, usize, usize) -> bool,
    ) -> io::Result<bool> {
        let Some(root) = self.root else {
            return Ok(true);
        };
        let total = root.total_bytes;
        let start = start.min(total);
        let end = end.min(total).max(start);
        if start >= end {
            return Ok(true);
        }
        self.visit_range_ref_while_result(root, start, end, 0, &mut visit)
    }

    pub(crate) fn visit_range_rev_while_result(
        &self,
        start: usize,
        end: usize,
        mut visit: impl FnMut(Piece, usize, usize) -> bool,
    ) -> io::Result<bool> {
        let Some(root) = self.root else {
            return Ok(true);
        };
        let total = root.total_bytes;
        let start = start.min(total);
        let end = end.min(total).max(start);
        if start >= end {
            return Ok(true);
        }
        self.visit_range_ref_rev_while_result(root, start, end, 0, &mut visit)
    }

    pub(crate) fn find_line_start_result(
        &self,
        line0: usize,
        mut locate_break: impl FnMut(Piece, usize) -> Option<usize>,
    ) -> io::Result<Option<usize>> {
        if line0 == 0 {
            return Ok(Some(0));
        }
        let Some(root) = self.root else {
            return Ok(None);
        };
        let target_break = line0.saturating_sub(1);
        if target_break >= root.total_line_breaks {
            return Ok(None);
        }
        self.find_line_start_ref_result(root, target_break, 0, &mut locate_break)
    }

    /// Legacy silent range visit.
    ///
    /// Kept only for callers where a missing piece page degrades to a
    /// weaker-but-safe answer instead of a wrong one (search miss,
    /// advisory position estimates, legacy best-effort line slices). Every
    /// content-producing read goes through [`PieceTree::visit_range_result`];
    /// do not add new callers here.
    pub(crate) fn visit_range(
        &self,
        start: usize,
        end: usize,
        visit: impl FnMut(Piece, usize, usize),
    ) {
        let _ = self.visit_range_result(start, end, visit);
    }

    /// Legacy silent bounded visit: `false` means the callback stopped the
    /// walk; a load failure degrades to "completed" exactly as before the
    /// honest walkers existed. See [`PieceTree::visit_range`] for the
    /// caller contract.
    pub(crate) fn visit_range_while(
        &self,
        start: usize,
        end: usize,
        visit: impl FnMut(Piece, usize, usize) -> bool,
    ) -> bool {
        self.visit_range_while_result(start, end, visit)
            .unwrap_or(true)
    }

    /// Legacy silent reverse visit; see [`PieceTree::visit_range_while`].
    pub(crate) fn visit_range_rev_while(
        &self,
        start: usize,
        end: usize,
        visit: impl FnMut(Piece, usize, usize) -> bool,
    ) -> bool {
        self.visit_range_rev_while_result(start, end, visit)
            .unwrap_or(true)
    }

    /// Legacy silent line-start lookup; see [`PieceTree::visit_range`].
    pub(crate) fn find_line_start(
        &self,
        line0: usize,
        locate_break: impl FnMut(Piece, usize) -> Option<usize>,
    ) -> Option<usize> {
        self.find_line_start_result(line0, locate_break)
            .ok()
            .flatten()
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
        // land through a page that the window refused: reads through it
        // would fail typed (and the document would shrink behind
        // `total_len` if the loss were tolerated). The verification runs
        // AFTER the root rebuild, which itself allocates pages on an
        // overflowing edit.
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
                // Honest collect: a rebuilt sidecar that silently dropped an
                // unreadable subtree would persist the loss as the new
                // committed state.
                self.collect_pieces_result(root, &mut pieces)?;
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

        // Materializing through the honest collector: if any piece page is
        // unreadable, replacing the disk store with an in-memory tree built
        // from the readable subset would silently shrink the document
        // (from_pieces recomputes total_len over the survivors). Keep the
        // disk store instead — reads through it keep failing typed on the
        // unreadable page, and the flush failure that triggered the detach
        // has already been reported to the caller.
        let Ok(pieces) = self.to_vec_result() else {
            return;
        };
        *self = Self::from_pieces(pieces);
    }

    fn page(&self, page_id: u64) -> Option<Arc<PiecePage>> {
        self.store.get_page(page_id)
    }

    /// Open-time readability check: every piece page reachable from the
    /// live root must be readable, because an unreadable page (CRC
    /// mismatch, IO failure) answers `None` to every lazy reader — reads
    /// through it fail typed today, but the open would otherwise
    /// advertise a full `total_len` over a document that cannot be read
    /// back completely, and every read would surface that inconsistency
    /// as an error only when the page is touched. Rejecting the open as
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
