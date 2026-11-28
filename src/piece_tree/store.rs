use super::btree::{read_page_from_file, serialize_page, ChildRef, PiecePage};
use super::editlog_path;
use super::history::RootEntry;
use super::session::{
    add_page_capacity_v4, bounded_page_count, decode_encoding_origin, encode_encoding_origin,
    log_page_capacity_v4, read_add_bytes, read_add_bytes_v4, read_editlog_header,
    read_editlog_header_v4, read_history_entries, read_index_chain_v4, read_log_entries_v4,
    serialize_add_chain_page_v4, serialize_history_log_page_v4, serialize_index_page_v4,
    write_editlog_header_v4, EditLogHeaderV4, HistoryLogEntryV4, SessionMeta,
};
use super::{
    PageKind, EDITLOG_CACHE_PAGES, EDITLOG_FLAG_DECODING_HAD_ERRORS, EDITLOG_FLAG_FULL_INDEX,
    EDITLOG_FLAG_HAS_ENCODING_ORIGIN, EDITLOG_HEADER_BYTES, EDITLOG_PAGE_SIZE,
    EDITLOG_V3_HEADER_BYTES, RESIDENT_PAGES_FAILED_PERSISTENCE_CAP,
};
use crate::source_identity::{sampled_content_fingerprint, sampled_file_fingerprint};
use crate::storage::FileStorage;
use std::collections::{HashMap, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::UNIX_EPOCH;

#[derive(Debug, Default)]
pub(super) struct InMemoryPageStore {
    pages: HashMap<u64, Arc<PiecePage>>,
    next_page_id: u64,
}

#[derive(Default)]
pub(super) struct DiskPageStore {
    path: PathBuf,
    source: SourceMetadata,
    pub(super) file: Option<File>,
    cache: Mutex<PageCache>,
    pub(super) resident_pages: HashMap<u64, Arc<PiecePage>>,
    next_page_id: u64,
    persistence_failed: bool,
    /// v4: the history vector already committed to the append-only log,
    /// paired with each entry's flat log id. The next flush diffs the live
    /// vector against this snapshot and appends only the new entries.
    flushed: Vec<(Option<ChildRef>, u64)>,
    /// v4: the flat id the next appended log entry receives.
    next_flat: u64,
    /// v4: committed epoch; bumped whenever the flushed vector prefix
    /// diverges (a new edit after undo), so recovery can tell the live redo
    /// tail from dead entries of abandoned branches.
    epoch: u64,
    /// v4: page ids of the append-only history log, in chain order.
    log_page_ids: Vec<u64>,
    /// v4: page ids of the append-only add chain, in chain order.
    add_page_ids: Vec<u64>,
    /// v4: add-buffer bytes already committed to the chain; the next flush
    /// appends only the tail past this length.
    committed_add_len: usize,
    /// The store reopened a legacy v3 sidecar: its pages sit behind the
    /// 192-byte v3 header, so the first flush rewrites the file into the v4
    /// layout before any append-only bookkeeping can start.
    legacy: bool,
    /// Byte offset the sidecar's pages start at (192 for an unconverted v3
    /// sidecar, 256 for v4), used by every page read.
    page_read_header_bytes: u64,
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
            .field("flushed_len", &self.flushed.len())
            .field("next_flat", &self.next_flat)
            .field("epoch", &self.epoch)
            .field("log_pages", &self.log_page_ids.len())
            .field("add_pages", &self.add_page_ids.len())
            .field("committed_add_len", &self.committed_add_len)
            .field("legacy", &self.legacy)
            .finish()
    }
}

#[derive(Debug)]
pub(super) enum PageStore {
    InMemory(InMemoryPageStore),
    Disk(Box<DiskPageStore>),
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub(super) struct SourceMetadata {
    pub(super) len: u64,
    pub(super) mtime_ns: u64,
    pub(super) fingerprint: u64,
}

pub(super) type OpenDiskSession =
    Option<(DiskPageStore, Vec<RootEntry>, usize, Vec<u8>, SessionMeta)>;

impl PageStore {
    pub(super) fn get_page(&self, page_id: u64) -> Option<Arc<PiecePage>> {
        match self {
            Self::InMemory(store) => store.pages.get(&page_id).cloned(),
            Self::Disk(store) => store.get_page(page_id),
        }
    }

    pub(super) fn alloc_page(
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

    pub(super) fn next_page_id_hint(&self) -> u64 {
        match self {
            Self::InMemory(_) => 0,
            Self::Disk(store) => store.next_page_id,
        }
    }

    /// Verifies that every page allocated since `first_new_page` is
    /// readable for the rest of the session. While persistence is down
    /// new pages live only in the bounded resident window, so a page the
    /// window refused must fail the edit instead of being referenced by
    /// a root that reads would fail typed on.
    pub(super) fn verify_pages_resident_since(&self, first_new_page: u64) -> io::Result<()> {
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

    pub(super) fn flush_session(
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
    pub(super) fn create(source_path: &Path) -> io::Result<Self> {
        let path = editlog_path(source_path);
        let source = source_metadata(source_path)?;
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(parent)?;
        FileStorage::replace_with(&path, |file| {
            write_editlog_header_v4(file, EditLogHeaderV4::default())?;
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
            flushed: Vec::new(),
            next_flat: 1,
            epoch: 0,
            log_page_ids: Vec::new(),
            add_page_ids: Vec::new(),
            committed_add_len: 0,
            legacy: false,
            page_read_header_bytes: EDITLOG_HEADER_BYTES as u64,
        })
    }

    pub(super) fn open(source_path: &Path, source: SourceMetadata) -> io::Result<OpenDiskSession> {
        let path = editlog_path(source_path);
        if !path.exists() {
            return Ok(None);
        }

        let mut file = OpenOptions::new().read(true).write(true).open(&path)?;
        match read_editlog_header_v4(&mut file)? {
            Some(header) => Self::open_v4(path, file, header, source),
            None => Self::open_v3_legacy(path, file, source),
        }
    }

    /// v4 recovery: rebuilds the history vector by walking the append-only
    /// log's parent links from the committed entry, then appends the redo
    /// tail (entries of the committed epoch with a higher vector index).
    fn open_v4(
        path: PathBuf,
        mut file: File,
        header: EditLogHeaderV4,
        source: SourceMetadata,
    ) -> io::Result<OpenDiskSession> {
        if header.source_len != source.len
            || header.source_mtime_ns != source.mtime_ns
            || header.source_fingerprint != source.fingerprint
        {
            return Ok(None);
        }
        let known_byte_len = usize::try_from(header.known_byte_len).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "qem editlog known_byte_len exceeds the addressable size",
            )
        })?;
        let meta = SessionMeta {
            known_byte_len,
            full_index: (header.flags & EDITLOG_FLAG_FULL_INDEX) != 0,
            encoding: header.encoding,
            encoding_origin: ((header.flags & EDITLOG_FLAG_HAS_ENCODING_ORIGIN) != 0)
                .then(|| decode_encoding_origin(header.encoding_origin))
                .flatten(),
            decoding_had_errors: (header.flags & EDITLOG_FLAG_DECODING_HAD_ERRORS) != 0,
        };

        let file_len = file.metadata()?.len();
        let page_count = usize::try_from(header.page_count).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "qem editlog page_count exceeds the addressable size",
            )
        })?;
        if page_count > bounded_page_count(Some(file_len), usize::MAX, EDITLOG_HEADER_BYTES as u64)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "qem editlog page_count exceeds what the sidecar can physically hold",
            ));
        }

        let max_chain_pages =
            bounded_page_count(Some(file_len), usize::MAX, EDITLOG_HEADER_BYTES as u64);
        let log_page_ids = read_index_chain_v4(
            &mut file,
            header.log_index_page_id,
            PageKind::LogIndex,
            max_chain_pages,
        )?;
        if log_page_ids.len() as u64 != header.log_page_count {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "qem editlog log index does not enumerate the committed page count",
            ));
        }
        let add_page_ids = read_index_chain_v4(
            &mut file,
            header.add_index_page_id,
            PageKind::AddIndex,
            max_chain_pages,
        )?;
        if add_page_ids.len() as u64 != header.add_page_count {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "qem editlog add index does not enumerate the committed page count",
            ));
        }

        let mut max_referenced_page_id = log_page_ids
            .iter()
            .chain(add_page_ids.iter())
            .copied()
            .max()
            .unwrap_or(0);
        max_referenced_page_id = max_referenced_page_id
            .max(header.log_head_page_id)
            .max(header.add_head_page_id);
        if max_referenced_page_id > page_count as u64 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "qem editlog page_count is below the pages the chains reference",
            ));
        }

        // The header is the commit record: pages appended after the last
        // committed `page_count` are orphans from a crash between the page
        // append and the header rewrite. Truncating to the committed size
        // restores the exact pre-crash append position (same protocol as v3).
        let committed_len = EDITLOG_HEADER_BYTES as u64
            + (page_count as u64).saturating_mul(EDITLOG_PAGE_SIZE as u64);
        if file_len > committed_len {
            file.set_len(committed_len)?;
        }

        let log_entry_count = usize::try_from(header.log_entry_count).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "qem editlog log_entry_count exceeds the addressable size",
            )
        })?;
        if log_entry_count == 0 && header.vector_len == 0 {
            return Ok(None);
        }
        let entries = read_log_entries_v4(&mut file, &log_page_ids, log_entry_count)?;
        if entries.len() != log_entry_count {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "qem editlog history log is shorter than the committed entry count",
            ));
        }
        if header.current_flat == 0 || header.current_flat > header.log_entry_count {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "qem editlog current entry points outside the history log",
            ));
        }

        // Rebuild the committed history vector: walk parent links from the
        // current entry to the root, then append the redo tail (entries of
        // the committed epoch with a higher vector index). `flat` is the
        // 1-based position of an entry inside the log.
        let mut branch: Vec<(Option<ChildRef>, u64, u64)> = Vec::new();
        let mut flat = header.current_flat;
        loop {
            let entry = entries.get((flat - 1) as usize).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "qem editlog history parent link points outside the log",
                )
            })?;
            branch.push((entry.root, flat, entry.vector_index));
            flat = entry.parent_flat;
            if flat == 0 {
                break;
            }
            if branch.len() > log_entry_count {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "qem editlog history parent links form a cycle",
                ));
            }
        }
        branch.reverse();
        let current_vector_index = branch.last().map(|(_, _, v)| *v).unwrap_or(0);
        for (position, (_, _, vector_index)) in branch.iter().enumerate() {
            if *vector_index != position as u64 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "qem editlog history vector positions are inconsistent",
                ));
            }
        }

        let mut redo: Vec<(u64, Option<ChildRef>, u64)> = entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| {
                entry.epoch == header.epoch && entry.vector_index > current_vector_index
            })
            .map(|(idx, entry)| (entry.vector_index, entry.root, idx as u64 + 1))
            .collect();
        redo.sort_by_key(|(vector_index, _, _)| *vector_index);
        let expected_redo_len = header.vector_len.saturating_sub(current_vector_index + 1);
        if redo.len() as u64 != expected_redo_len {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "qem editlog redo tail does not match the committed vector length",
            ));
        }
        for (offset, (vector_index, _, _)) in redo.iter().enumerate() {
            if *vector_index != current_vector_index + 1 + offset as u64 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "qem editlog redo tail positions are inconsistent",
                ));
            }
        }

        let flushed: Vec<(Option<ChildRef>, u64)> = branch
            .iter()
            .map(|(root, flat, _)| (*root, *flat))
            .chain(redo.iter().map(|(_, root, flat)| (*root, *flat)))
            .collect();
        let history: Vec<RootEntry> = flushed
            .iter()
            .map(|(root, _)| RootEntry { root: *root })
            .collect();
        let history_index = (current_vector_index as usize).min(history.len().saturating_sub(1));

        let add_len = usize::try_from(header.add_len).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "qem editlog add_len exceeds the addressable size",
            )
        })?;
        let add = read_add_bytes_v4(&mut file, &add_page_ids, add_len)?;

        Ok(Some((
            Self {
                path,
                source,
                file: Some(file),
                cache: Mutex::new(PageCache::default()),
                resident_pages: HashMap::new(),
                next_page_id: page_count as u64,
                persistence_failed: false,
                flushed,
                next_flat: header.log_entry_count + 1,
                epoch: header.epoch,
                log_page_ids,
                add_page_ids,
                committed_add_len: add_len,
                legacy: false,
                page_read_header_bytes: EDITLOG_HEADER_BYTES as u64,
            },
            history,
            history_index,
            add,
            meta,
        )))
    }

    /// Legacy (v3) recovery: contiguous history/add blocks rewritten whole
    /// on every flush. Read-only support; the first flush of a reopened v3
    /// session converts it to the append-only v4 format without data loss.
    fn open_v3_legacy(
        path: PathBuf,
        mut file: File,
        source: SourceMetadata,
    ) -> io::Result<OpenDiskSession> {
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
            encoding: header.encoding,
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
        if page_count
            > bounded_page_count(Some(file_len), usize::MAX, EDITLOG_V3_HEADER_BYTES as u64)
        {
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
        // exact pre-crash append position. Legacy v3 sidecars keep their
        // pages behind the 192-byte v3 header.
        let committed_len = EDITLOG_V3_HEADER_BYTES as u64
            + (page_count as u64).saturating_mul(EDITLOG_PAGE_SIZE as u64);
        if file.metadata()?.len() > committed_len {
            file.set_len(committed_len)?;
        }

        let mut store = Self {
            path,
            source,
            file: Some(file),
            cache: Mutex::new(PageCache::default()),
            resident_pages: HashMap::new(),
            next_page_id: page_count as u64,
            persistence_failed: false,
            // Legacy v3 state: nothing is in the append-only log yet, so
            // the first flush rewrites every entry and the add buffer
            // into the v4 format (one-time conversion, no data loss).
            flushed: Vec::new(),
            next_flat: 1,
            epoch: 0,
            log_page_ids: Vec::new(),
            add_page_ids: Vec::new(),
            committed_add_len: 0,
            legacy: false,
            page_read_header_bytes: EDITLOG_V3_HEADER_BYTES as u64,
        };

        // Convert the legacy layout right at reopen: the new pages an edit
        // allocates must land at the v4 offsets, and deferring the rewrite
        // to the first flush would fail that append-order check. The
        // conversion alone would leave a crash window where the unsaved
        // edits are still only in memory, so the recovered state is
        // committed to the fresh log immediately.
        store.convert_legacy_file_to_v4()?;
        store.flush_session(&history, history_index, &add, meta)?;

        Ok(Some((store, history, history_index, add, meta)))
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
        let page =
            Arc::new(read_page_from_file(&mut file, page_id, self.page_read_header_bytes).ok()?);
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

    /// Rewrites a legacy v3 sidecar file into the v4 page layout. The v3
    /// pages sit behind the 192-byte v3 header; the append-only format needs
    /// them behind the 256-byte v4 header, and an in-place append would
    /// desync every page offset. The rewrite shifts each existing page to
    /// its v4 offset (last page first, so a page is never read after its
    /// source bytes were overwritten), resets the append-only bookkeeping,
    /// and stamps a fresh v4 header; the following flush appends the whole
    /// in-memory history as new log entries. Piece-page ids are preserved,
    /// so the recovered tree keeps referencing the same pages.
    fn convert_legacy_file_to_v4(&mut self) -> io::Result<()> {
        let Some(file) = self.file.as_mut() else {
            return Err(io::Error::other("missing editlog handle"));
        };
        let page_count = self.next_page_id;
        let mut buf = vec![0u8; EDITLOG_PAGE_SIZE];
        for page_id in (1..=page_count).rev() {
            let source_offset = (EDITLOG_V3_HEADER_BYTES as u64)
                .saturating_add((page_id - 1).saturating_mul(EDITLOG_PAGE_SIZE as u64));
            let target_offset = (EDITLOG_HEADER_BYTES as u64)
                .saturating_add((page_id - 1).saturating_mul(EDITLOG_PAGE_SIZE as u64));
            file.seek(SeekFrom::Start(source_offset))?;
            file.read_exact(&mut buf)?;
            file.seek(SeekFrom::Start(target_offset))?;
            file.write_all(&buf)?;
        }
        file.set_len(
            (EDITLOG_HEADER_BYTES as u64)
                .saturating_add(page_count.saturating_mul(EDITLOG_PAGE_SIZE as u64)),
        )?;
        let header = EditLogHeaderV4 {
            page_count,
            source_len: self.source.len,
            source_mtime_ns: self.source.mtime_ns,
            source_fingerprint: self.source.fingerprint,
            log_head_page_id: 0,
            log_page_count: 0,
            log_entry_count: 0,
            current_flat: 0,
            epoch: 0,
            add_head_page_id: 0,
            add_page_count: 0,
            add_len: 0,
            known_byte_len: 0,
            flags: 0,
            encoding_origin: 0,
            encoding: None,
            vector_len: 0,
            log_index_page_id: 0,
            add_index_page_id: 0,
        };
        write_editlog_header_v4(file, header)?;
        file.flush()?;
        file.sync_all()?;
        self.flushed.clear();
        self.next_flat = 1;
        self.epoch = 0;
        self.log_page_ids.clear();
        self.add_page_ids.clear();
        self.committed_add_len = 0;
        self.page_read_header_bytes = EDITLOG_HEADER_BYTES as u64;
        Ok(())
    }

    /// v4 flush: appends only the history entries the log does not hold yet
    /// (diffed against the flushed snapshot), appends only the add-buffer
    /// tail past the committed length, rewrites the two small index pages,
    /// and commits by rewriting the fixed header. Data pages are never
    /// rewritten, so a long session pays linear sidecar growth instead of
    /// the v3 quadratic rewrite of the whole history on every flush.
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

        if self.legacy {
            self.convert_legacy_file_to_v4()?;
            self.legacy = false;
        }

        // History diff: find the first position where the live vector
        // diverges from the flushed snapshot. Entries before it are already
        // on disk; entries past it are appended now.
        let common = self.flushed.len().min(history.len());
        let mut prefix = 0usize;
        while prefix < common && self.flushed[prefix].0 == history[prefix].root {
            prefix += 1;
        }
        // A divergence inside the shared prefix (a new edit after undo)
        // abandons the committed redo tail: entries of the new branch carry
        // the next epoch, so recovery keeps only the live tail.
        if prefix < self.flushed.len().min(history.len()) {
            self.epoch += 1;
        }

        let page_capacity = log_page_capacity_v4().max(1);
        let mut pending_page: Vec<HistoryLogEntryV4> = Vec::new();
        let mut new_flushed: Vec<(Option<ChildRef>, u64)> = Vec::new();
        let mut parent_flat = if prefix > 0 {
            self.flushed[prefix - 1].1
        } else {
            0
        };
        for (i, entry) in history.iter().enumerate().skip(prefix) {
            if pending_page.len() == page_capacity {
                let bytes = serialize_history_log_page_v4(&pending_page, 0)?;
                let page_id = self.write_raw_page(bytes)?;
                self.log_page_ids.push(page_id);
                pending_page.clear();
            }
            let flat = self.next_flat;
            self.next_flat += 1;
            pending_page.push(HistoryLogEntryV4 {
                root: entry.root,
                parent_flat,
                vector_index: i as u64,
                epoch: self.epoch,
            });
            new_flushed.push((entry.root, flat));
            parent_flat = flat;
        }
        if !pending_page.is_empty() {
            let bytes = serialize_history_log_page_v4(&pending_page, 0)?;
            let page_id = self.write_raw_page(bytes)?;
            self.log_page_ids.push(page_id);
        }
        self.flushed.truncate(prefix);
        self.flushed.append(&mut new_flushed);

        // Add-buffer diff: append only the bytes past the committed length.
        // A shrinking add buffer rewrites the chain from scratch (defensive:
        // the buffer grows through inserts, and durability must never
        // silently shrink).
        if add_bytes.len() < self.committed_add_len {
            self.add_page_ids.clear();
            self.committed_add_len = 0;
        }
        let add_capacity = add_page_capacity_v4().max(1);
        if add_bytes.len() > self.committed_add_len {
            let mut start = self.committed_add_len;
            while start < add_bytes.len() {
                let end = (start + add_capacity).min(add_bytes.len());
                let bytes = serialize_add_chain_page_v4(&add_bytes[start..end], 0)?;
                let page_id = self.write_raw_page(bytes)?;
                self.add_page_ids.push(page_id);
                start = end;
            }
            self.committed_add_len = add_bytes.len();
        }
        let add_head = self.add_page_ids.first().copied().unwrap_or(0);

        // Index pages are rewritten on every commit: they enumerate the
        // chain page ids, which is what keeps every data page append-only.
        let mut log_index_page_id = 0u64;
        if !self.log_page_ids.is_empty() {
            let bytes = serialize_index_page_v4(PageKind::LogIndex, &self.log_page_ids, 0)?;
            log_index_page_id = self.write_raw_page(bytes)?;
        }
        let mut add_index_page_id = 0u64;
        if !self.add_page_ids.is_empty() {
            let bytes = serialize_index_page_v4(PageKind::AddIndex, &self.add_page_ids, 0)?;
            add_index_page_id = self.write_raw_page(bytes)?;
        }

        let history_index = history_index.min(history.len().saturating_sub(1));
        let current_flat = self
            .flushed
            .get(history_index)
            .map(|(_, flat)| *flat)
            .unwrap_or(0);
        let header = EditLogHeaderV4 {
            page_count: self.next_page_id,
            source_len: self.source.len,
            source_mtime_ns: self.source.mtime_ns,
            source_fingerprint: self.source.fingerprint,
            log_head_page_id: self.log_page_ids.first().copied().unwrap_or(0),
            log_page_count: self.log_page_ids.len() as u64,
            log_entry_count: self.next_flat.saturating_sub(1),
            current_flat,
            epoch: self.epoch,
            add_head_page_id: add_head,
            add_page_count: self.add_page_ids.len() as u64,
            add_len: self.committed_add_len as u64,
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
            encoding: meta.encoding,
            vector_len: history.len() as u64,
            log_index_page_id,
            add_index_page_id,
        };
        // Durability protocol (same as v3):
        // 1. append all newly reachable pages first
        // 2. flush/sync those pages
        // 3. rewrite the fixed header as the authoritative commit record
        // 4. flush/sync again so reopen either sees the previous committed
        //    header or the fully updated one
        let result = (|| -> io::Result<()> {
            if let Some(file) = self.file.as_mut() {
                file.flush()?;
                file.sync_all()?;
            }
            if let Some(file) = self.file.as_mut() {
                write_editlog_header_v4(file, header)?;
                file.flush()?;
                file.sync_all()?;
            }
            Ok(())
        })();

        if let Err(err) = result {
            self.persistence_failed = true;
            return Err(err);
        }
        Ok(())
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

pub(super) fn source_metadata(path: &Path) -> io::Result<SourceMetadata> {
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

pub(super) fn source_metadata_with_bytes(path: &Path, bytes: &[u8]) -> io::Result<SourceMetadata> {
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
