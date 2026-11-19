use super::btree::{read_page_from_file, serialize_page, ChildRef, PiecePage};
use super::editlog_path;
use super::history::RootEntry;
use super::session::{
    add_page_capacity, bounded_page_count, decode_encoding_origin, encode_encoding_origin,
    history_page_capacity, read_add_bytes, read_editlog_header, read_history_entries,
    serialize_add_page, serialize_history_page, write_editlog_header, EditLogHeader, SessionMeta,
};
use super::{
    EDITLOG_CACHE_PAGES, EDITLOG_FLAG_DECODING_HAD_ERRORS, EDITLOG_FLAG_FULL_INDEX,
    EDITLOG_FLAG_HAS_ENCODING_ORIGIN, EDITLOG_HEADER_BYTES, EDITLOG_PAGE_SIZE,
    RESIDENT_PAGES_FAILED_PERSISTENCE_CAP,
};
use crate::source_identity::{sampled_content_fingerprint, sampled_file_fingerprint};
use crate::storage::FileStorage;
use std::collections::{HashMap, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::{self, Seek, SeekFrom, Write};
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
pub(super) enum PageStore {
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

    pub(super) fn open(source_path: &Path, source: SourceMetadata) -> io::Result<OpenDiskSession> {
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
            encoding: meta.encoding,
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
