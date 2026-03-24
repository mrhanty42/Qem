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
const EDITLOG_MAGIC: &[u8; 8] = b"QEMEDT1\0";
const EDITLOG_VERSION: u32 = 2;
const EDITLOG_PAGE_SIZE: usize = 4096;
const EDITLOG_HEADER_BYTES: usize = 128;
const EDITLOG_PAGE_HEADER_BYTES: usize = 24;
const EDITLOG_CACHE_PAGES: usize = 1024;
const LEAF_ENTRY_BYTES: usize = 32;
const INTERNAL_ENTRY_BYTES: usize = 24;
const HISTORY_ENTRY_BYTES: usize = 32;

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ChildRef {
    page_id: u64,
    total_bytes: usize,
    total_line_breaks: usize,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct SessionMeta {
    pub(crate) known_byte_len: usize,
    pub(crate) full_index: bool,
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
    history_index: usize,
    history_batch_depth: usize,
    history_batch_dirty: bool,
}

impl PieceTree {
    pub(crate) fn new() -> Self {
        Self {
            store: PageStore::default(),
            root: None,
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
        this.history_index = 0;
        this
    }

    pub(crate) fn from_pieces_disk(source_path: &Path, pieces: Vec<Piece>) -> io::Result<Self> {
        let mut this = Self {
            store: PageStore::Disk(DiskPageStore::create(source_path)?),
            root: None,
            history: Vec::new(),
            history_index: 0,
            history_batch_depth: 0,
            history_batch_dirty: false,
        };
        this.root = this.build_tree_from_pieces(filter_zero_len(pieces));
        this.history = vec![RootEntry { root: this.root }];
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
        Ok(Some((
            Self {
                store: PageStore::Disk(store),
                root,
                history,
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

    pub(crate) fn insert<F>(&mut self, offset: usize, piece: Piece, split_piece: &mut F)
    where
        F: FnMut(Piece, usize) -> (Option<Piece>, Option<Piece>),
    {
        if piece.len == 0 {
            return;
        }
        let offset = offset.min(self.total_len());
        let Some(root) = self.root.take() else {
            self.root = self.build_tree_from_pieces(vec![piece]);
            self.record_history_root();
            return;
        };
        let children = self.insert_at(root, offset, piece, split_piece);
        self.root = self.build_tree_from_child_refs(children);
        self.record_history_root();
    }

    pub(crate) fn delete_range<F>(&mut self, start: usize, len: usize, trim_piece: &mut F)
    where
        F: FnMut(Piece, usize, usize) -> (Option<Piece>, Option<Piece>),
    {
        if len == 0 {
            return;
        }
        let Some(root) = self.root.take() else {
            return;
        };
        let total = root.total_bytes;
        let start = start.min(total);
        let end = start.saturating_add(len).min(total);
        if start >= end {
            self.root = Some(root);
            return;
        }
        let children = self.delete_at(root, start, end, trim_piece);
        self.root = self.build_tree_from_child_refs(children);
        self.record_history_root();
    }

    pub(crate) fn flush_session(&mut self, add_bytes: &[u8], meta: SessionMeta) -> io::Result<()> {
        self.store
            .flush_session(&self.history, self.history_index, add_bytes, meta)
    }

    pub(crate) fn detach_persistence(&mut self) {
        if !matches!(self.store, PageStore::Disk(_)) {
            return;
        }

        let pieces = self.to_vec();
        *self = Self::from_pieces(pieces);
    }

    pub(crate) fn undo(&mut self) -> bool {
        if self.history_index == 0 {
            return false;
        }
        self.history_index = self.history_index.saturating_sub(1);
        self.root = self.history[self.history_index].root;
        true
    }

    pub(crate) fn redo(&mut self) -> bool {
        if self.history_index + 1 >= self.history.len() {
            return false;
        }
        self.history_index += 1;
        self.root = self.history[self.history_index].root;
        true
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
        if is_same {
            return;
        }
        self.history.truncate(self.history_index.saturating_add(1));
        self.history.push(next);
        self.history_index = self.history.len().saturating_sub(1);
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
    ) -> Vec<ChildRef>
    where
        F: FnMut(Piece, usize) -> (Option<Piece>, Option<Piece>),
    {
        let Some(page) = self.page(child.page_id).map(|page| (*page).clone()) else {
            return Vec::new();
        };
        match page {
            PiecePage::Leaf { pieces } => {
                let mut next = insert_piece_into_leaf(pieces, offset, piece, split_piece);
                coalesce_adjacent(&mut next);
                self.leaf_refs_from_pieces(next)
            }
            PiecePage::Internal { children } => {
                let (index, child_start) = child_index_for_offset(&children, offset);
                let local_offset = offset.saturating_sub(child_start);
                let mut next_children = children;
                let target = next_children[index];
                let inserted = self.insert_at(target, local_offset, piece, split_piece);
                next_children.splice(index..=index, inserted);
                self.internal_refs_from_children(next_children)
            }
        }
    }

    fn delete_at<F>(
        &mut self,
        child: ChildRef,
        start: usize,
        end: usize,
        trim_piece: &mut F,
    ) -> Vec<ChildRef>
    where
        F: FnMut(Piece, usize, usize) -> (Option<Piece>, Option<Piece>),
    {
        let Some(page) = self.page(child.page_id).map(|page| (*page).clone()) else {
            return Vec::new();
        };
        match page {
            PiecePage::Leaf { pieces } => {
                let mut next = delete_range_from_leaf(pieces, start, end, trim_piece);
                coalesce_adjacent(&mut next);
                self.leaf_refs_from_pieces(next)
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
                        ));
                    }
                    child_base = child_end;
                }
                self.internal_refs_from_children(next_children)
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
                let total_bytes = chunk.iter().map(|piece| piece.len).sum();
                let total_line_breaks = chunk.iter().map(|piece| piece.line_breaks).sum();
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
                let total_bytes = chunk.iter().map(|child| child.total_bytes).sum();
                let total_line_breaks = chunk.iter().map(|child| child.total_line_breaks).sum();
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
        let meta = SessionMeta {
            known_byte_len: header.known_byte_len as usize,
            full_index: (header.flags & 1) != 0,
        };

        Ok(Some((
            Self {
                path,
                source,
                file: Some(file),
                cache: Mutex::new(PageCache::default()),
                resident_pages: HashMap::new(),
                next_page_id: header.page_count,
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
            self.resident_pages.insert(page_id, Arc::clone(&page_ref));
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
        let expected_offset =
            EDITLOG_HEADER_BYTES as u64 + page_id.saturating_sub(1) * EDITLOG_PAGE_SIZE as u64;
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
            flags: u64::from(meta.full_index),
        };
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
        let expected_offset =
            EDITLOG_HEADER_BYTES as u64 + page_id.saturating_sub(1) * EDITLOG_PAGE_SIZE as u64;
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
    let committed_at_ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .min(u64::MAX as u128) as u64;
    buf[120..128].copy_from_slice(&committed_at_ns.to_le_bytes());
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
    let version = u32::from_le_bytes(buf[8..12].try_into().unwrap_or([0; 4]));
    let page_size = u32::from_le_bytes(buf[12..16].try_into().unwrap_or([0; 4]));
    if version != EDITLOG_VERSION || page_size as usize != EDITLOG_PAGE_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unsupported qem editlog format",
        ));
    }
    Ok(EditLogHeader {
        page_count: u64::from_le_bytes(buf[16..24].try_into().unwrap_or([0; 8])),
        source_len: u64::from_le_bytes(buf[24..32].try_into().unwrap_or([0; 8])),
        source_mtime_ns: u64::from_le_bytes(buf[32..40].try_into().unwrap_or([0; 8])),
        source_fingerprint: u64::from_le_bytes(buf[40..48].try_into().unwrap_or([0; 8])),
        history_first_page_id: u64::from_le_bytes(buf[48..56].try_into().unwrap_or([0; 8])),
        history_page_count: u64::from_le_bytes(buf[56..64].try_into().unwrap_or([0; 8])),
        history_len: u64::from_le_bytes(buf[64..72].try_into().unwrap_or([0; 8])),
        history_index: u64::from_le_bytes(buf[72..80].try_into().unwrap_or([0; 8])),
        add_first_page_id: u64::from_le_bytes(buf[80..88].try_into().unwrap_or([0; 8])),
        add_page_count: u64::from_le_bytes(buf[88..96].try_into().unwrap_or([0; 8])),
        add_len: u64::from_le_bytes(buf[96..104].try_into().unwrap_or([0; 8])),
        known_byte_len: u64::from_le_bytes(buf[104..112].try_into().unwrap_or([0; 8])),
        flags: u64::from_le_bytes(buf[112..120].try_into().unwrap_or([0; 8])),
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
    let offset = EDITLOG_HEADER_BYTES as u64 + page_id.saturating_sub(1) * EDITLOG_PAGE_SIZE as u64;
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

    let expected_crc = u32::from_le_bytes(buf[0..4].try_into().unwrap_or([0; 4]));
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
    let count = u64::from_le_bytes(buf[8..16].try_into().unwrap_or([0; 8])) as usize;
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

            let mut children = Vec::with_capacity(count);
            for _ in 0..count {
                children.push(ChildRef {
                    page_id: u64::from_le_bytes(
                        buf[cursor..cursor + 8].try_into().unwrap_or([0; 8]),
                    ),
                    total_bytes: u64::from_le_bytes(
                        buf[cursor + 8..cursor + 16].try_into().unwrap_or([0; 8]),
                    ) as usize,
                    total_line_breaks: u64::from_le_bytes(
                        buf[cursor + 16..cursor + 24].try_into().unwrap_or([0; 8]),
                    ) as usize,
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
                    start: u64::from_le_bytes(
                        buf[cursor + 8..cursor + 16].try_into().unwrap_or([0; 8]),
                    ) as usize,
                    len: u64::from_le_bytes(
                        buf[cursor + 16..cursor + 24].try_into().unwrap_or([0; 8]),
                    ) as usize,
                    line_breaks: u64::from_le_bytes(
                        buf[cursor + 24..cursor + 32].try_into().unwrap_or([0; 8]),
                    ) as usize,
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

    let mut out = Vec::with_capacity(history_len);
    for rel in 0..page_count {
        let page_id = first_page_id.saturating_add(rel as u64);
        let buf = read_raw_page_from_file(file, page_id)?;
        let count = parse_page_header(&buf, PageKind::History)?;
        let mut cursor = EDITLOG_PAGE_HEADER_BYTES;
        for _ in 0..count {
            if out.len() >= history_len {
                break;
            }
            let present = u64::from_le_bytes(buf[cursor..cursor + 8].try_into().unwrap_or([0; 8]));
            let root = if present == 0 {
                None
            } else {
                Some(ChildRef {
                    page_id: u64::from_le_bytes(
                        buf[cursor + 8..cursor + 16].try_into().unwrap_or([0; 8]),
                    ),
                    total_bytes: u64::from_le_bytes(
                        buf[cursor + 16..cursor + 24].try_into().unwrap_or([0; 8]),
                    ) as usize,
                    total_line_breaks: u64::from_le_bytes(
                        buf[cursor + 24..cursor + 32].try_into().unwrap_or([0; 8]),
                    ) as usize,
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

fn read_raw_page_from_file(file: &mut File, page_id: u64) -> io::Result<Vec<u8>> {
    let mut buf = vec![0u8; EDITLOG_PAGE_SIZE];
    let offset = EDITLOG_HEADER_BYTES as u64 + page_id.saturating_sub(1) * EDITLOG_PAGE_SIZE as u64;
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
    Ok(u64::from_le_bytes(buf[8..16].try_into().unwrap_or([0; 8])) as usize)
}

fn validate_page_crc(buf: &[u8]) -> io::Result<()> {
    if buf.len() != EDITLOG_PAGE_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid editlog page size",
        ));
    }

    let expected_crc = u32::from_le_bytes(buf[0..4].try_into().unwrap_or([0; 4]));
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
            if last.src == piece.src && last.start + last.len == piece.start {
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
        editlog_path, source_metadata, DiskPageStore, Piece, PieceSource, PieceTree, SessionMeta,
        SourceMetadata,
    };
    use std::fs;

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
        );
        tree.insert(
            3,
            Piece {
                src: PieceSource::Add,
                start: 3,
                len: 2,
                line_breaks: 0,
            },
            &mut split,
        );

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

        tree.delete_range(2, 6, &mut trim);

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
        );
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
}
