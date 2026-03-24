use crate::source_identity::sampled_content_fingerprint;
use crate::storage::FileStorage;
use memchr::memchr2_iter;
use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::UNIX_EPOCH;

const INDEX_MAGIC: &[u8; 8] = b"QEMIDX1\0";
// `.qem.lineidx` is an internal cache sidecar rather than a stable external
// format. Qem may bump this version across releases and rebuild the cache
// instead of preserving compatibility with older index files.
const INDEX_VERSION: u32 = 2;
const INDEX_PAGE_SIZE: usize = 4096;
const INDEX_HEADER_BYTES: usize = 72;
const INDEX_PAGE_HEADER_BYTES: usize = 16;
const INDEX_ENTRY_BYTES: usize = 24;
const INDEX_CACHE_PAGES: usize = 32;
const INDEX_BUILD_MIN_FILE_BYTES: usize = 64 * 1024 * 1024; // 64 MiB
const INDEX_CHECKPOINT_STRIDE_LINES: u64 = 8_192;
const INDEX_CHECKPOINT_STRIDE_BYTES: u64 = 1_048_576; // 1 MiB

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LineCheckpoint {
    pub(crate) line0: usize,
    pub(crate) byte0: usize,
}

#[derive(Clone)]
pub(crate) struct DiskLineIndex {
    state: Arc<RwLock<DiskLineIndexState>>,
}

impl std::fmt::Debug for DiskLineIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self.state.read().ok();
        let label = match state.as_deref() {
            Some(DiskLineIndexState::Building) => "Building",
            Some(DiskLineIndexState::Ready(_)) => "Ready",
            Some(DiskLineIndexState::Failed) => "Failed",
            None => "Poisoned",
        };
        f.debug_struct("DiskLineIndex")
            .field("state", &label)
            .finish()
    }
}

#[derive(Debug)]
enum DiskLineIndexState {
    Building,
    Ready(Arc<ReadyDiskLineIndex>),
    Failed,
}

#[derive(Debug)]
struct ReadyDiskLineIndex {
    path: PathBuf,
    total_lines: u64,
    root_page: u64,
    page_count: u64,
    cache: Mutex<PageCache>,
}

#[derive(Debug, Default)]
struct PageCache {
    pages: HashMap<u64, Arc<Page>>,
    order: VecDeque<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PageKind {
    Internal = 1,
    Leaf = 2,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PageEntry {
    line0: u64,
    byte0: u64,
    child_page: u64,
}

#[derive(Debug)]
struct Page {
    kind: PageKind,
    entries: Vec<PageEntry>,
}

#[derive(Clone, Copy, Debug)]
struct IndexMetadata {
    source_len: u64,
    source_mtime_ns: u64,
    source_fingerprint: u64,
}

impl DiskLineIndex {
    pub(crate) fn open_or_build(path: &Path, storage: &FileStorage) -> Option<Self> {
        if storage.len() < INDEX_BUILD_MIN_FILE_BYTES {
            return None;
        }

        let metadata = source_metadata(path, storage.bytes()).ok()?;
        let sidecar = sidecar_path(path);
        let state = if let Ok(ready) = ReadyDiskLineIndex::open_existing(&sidecar, metadata) {
            DiskLineIndexState::Ready(Arc::new(ready))
        } else {
            DiskLineIndexState::Building
        };
        let this = Self {
            state: Arc::new(RwLock::new(state)),
        };

        let already_ready = this
            .state
            .read()
            .ok()
            .map(|state| matches!(&*state, DiskLineIndexState::Ready(_)))
            .unwrap_or(false);
        if already_ready {
            return Some(this);
        }

        let state = Arc::clone(&this.state);
        let storage = storage.clone();
        let path = path.to_path_buf();
        thread::spawn(move || {
            let result = build_or_open_index(&path, &storage, metadata);
            if let Ok(mut guard) = state.write() {
                *guard = match result {
                    Ok(ready) => DiskLineIndexState::Ready(Arc::new(ready)),
                    Err(_) => DiskLineIndexState::Failed,
                };
            }
        });

        Some(this)
    }

    pub(crate) fn checkpoint_for_line(&self, line0: usize) -> Option<LineCheckpoint> {
        let ready = self.ready()?;
        let checkpoint = ready.checkpoint_for_line(line0 as u64).ok()?;
        Some(LineCheckpoint {
            line0: checkpoint.line0 as usize,
            byte0: checkpoint.byte0 as usize,
        })
    }

    pub(crate) fn total_lines(&self) -> Option<usize> {
        let ready = self.ready()?;
        usize::try_from(ready.total_lines).ok()
    }

    fn ready(&self) -> Option<Arc<ReadyDiskLineIndex>> {
        let state = self.state.read().ok()?;
        match &*state {
            DiskLineIndexState::Ready(ready) => Some(Arc::clone(ready)),
            DiskLineIndexState::Building | DiskLineIndexState::Failed => None,
        }
    }
}

impl ReadyDiskLineIndex {
    fn open_existing(path: &Path, metadata: IndexMetadata) -> io::Result<Self> {
        let mut file = File::open(path)?;
        let header = read_header(&mut file)?;
        validate_header(header, metadata)?;

        let ready = Self {
            path: path.to_path_buf(),
            total_lines: header.total_lines,
            root_page: header.root_page,
            page_count: header.page_count,
            cache: Mutex::new(PageCache::default()),
        };
        ready.validate_root_page()?;
        Ok(ready)
    }

    fn checkpoint_for_line(&self, line0: u64) -> io::Result<PageEntry> {
        if self.page_count == 0 {
            return Ok(PageEntry {
                line0: 0,
                byte0: 0,
                child_page: 0,
            });
        }

        let mut current_page = self.root_page;
        loop {
            let page = self.read_page(current_page)?;
            let Some(entry) = lookup_entry(&page.entries, line0) else {
                return Ok(PageEntry {
                    line0: 0,
                    byte0: 0,
                    child_page: 0,
                });
            };
            if page.kind == PageKind::Leaf {
                return Ok(entry);
            }
            current_page = entry.child_page;
        }
    }

    fn read_page(&self, page_id: u64) -> io::Result<Arc<Page>> {
        if let Ok(mut cache) = self.cache.lock() {
            if let Some(page) = cache.get(page_id) {
                return Ok(page);
            }
        }

        let mut file = File::open(&self.path)?;
        let mut buf = vec![0u8; INDEX_PAGE_SIZE];
        let page_offset =
            INDEX_HEADER_BYTES as u64 + page_id.saturating_sub(1) * INDEX_PAGE_SIZE as u64;
        file.seek(SeekFrom::Start(page_offset))?;
        file.read_exact(&mut buf)?;
        let page = Arc::new(parse_page(&buf)?);

        if let Ok(mut cache) = self.cache.lock() {
            cache.insert(page_id, Arc::clone(&page));
        }

        Ok(page)
    }

    fn validate_root_page(&self) -> io::Result<()> {
        if self.page_count == 0 {
            return Ok(());
        }
        let _ = self.read_page(self.root_page)?;
        Ok(())
    }
}

impl PageCache {
    fn get(&mut self, page_id: u64) -> Option<Arc<Page>> {
        let page = self.pages.get(&page_id).cloned()?;
        self.touch(page_id);
        Some(page)
    }

    fn insert(&mut self, page_id: u64, page: Arc<Page>) {
        self.pages.insert(page_id, page);
        self.touch(page_id);
        while self.order.len() > INDEX_CACHE_PAGES {
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

#[derive(Clone, Copy, Debug)]
struct FileHeader {
    root_page: u64,
    page_count: u64,
    total_lines: u64,
    total_bytes: u64,
    source_len: u64,
    source_mtime_ns: u64,
    source_fingerprint: u64,
}

fn validate_header(header: FileHeader, metadata: IndexMetadata) -> io::Result<()> {
    if header.source_len != metadata.source_len
        || header.source_mtime_ns != metadata.source_mtime_ns
        || header.source_fingerprint != metadata.source_fingerprint
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "stale line index metadata",
        ));
    }

    if header.total_bytes != metadata.source_len || header.total_lines == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid qem line index header",
        ));
    }

    if header.page_count == 0 {
        if header.root_page != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid qem line index root page",
            ));
        }
    } else if header.root_page == 0 || header.root_page > header.page_count {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid qem line index root page",
        ));
    }

    Ok(())
}

fn build_or_open_index(
    source_path: &Path,
    storage: &FileStorage,
    metadata: IndexMetadata,
) -> io::Result<ReadyDiskLineIndex> {
    let sidecar = sidecar_path(source_path);
    if let Ok(ready) = ReadyDiskLineIndex::open_existing(&sidecar, metadata) {
        return Ok(ready);
    }

    build_index_file(&sidecar, storage, metadata)?;
    ReadyDiskLineIndex::open_existing(&sidecar, metadata)
}

fn build_index_file(path: &Path, storage: &FileStorage, metadata: IndexMetadata) -> io::Result<()> {
    FileStorage::replace_with(path, |file| {
        file.write_all(&[0u8; INDEX_HEADER_BYTES])?;

        let page_capacity =
            ((INDEX_PAGE_SIZE - INDEX_PAGE_HEADER_BYTES) / INDEX_ENTRY_BYTES).max(1);
        let bytes = storage.bytes();
        let mut page_count = 0u64;
        let mut leaf_entries = Vec::with_capacity(page_capacity);
        let mut summaries = Vec::new();
        let mut total_lines = 1u64;
        let mut next_line_checkpoint = INDEX_CHECKPOINT_STRIDE_LINES;
        let mut next_byte_checkpoint = INDEX_CHECKPOINT_STRIDE_BYTES;

        leaf_entries.push(PageEntry {
            line0: 0,
            byte0: 0,
            child_page: 0,
        });

        for pos in memchr2_iter(b'\n', b'\r', bytes) {
            if bytes[pos] == b'\r' && pos + 1 < bytes.len() && bytes[pos + 1] == b'\n' {
                continue;
            }

            let line_start = (pos + 1) as u64;
            let line0 = total_lines;
            total_lines = total_lines.saturating_add(1);
            if line0 < next_line_checkpoint && line_start < next_byte_checkpoint {
                continue;
            }

            leaf_entries.push(PageEntry {
                line0,
                byte0: line_start,
                child_page: 0,
            });
            next_line_checkpoint = line0.saturating_add(INDEX_CHECKPOINT_STRIDE_LINES);
            next_byte_checkpoint = line_start.saturating_add(INDEX_CHECKPOINT_STRIDE_BYTES);

            if leaf_entries.len() >= page_capacity {
                let page_id = write_page(file, PageKind::Leaf, &leaf_entries, &mut page_count)?;
                let first = leaf_entries[0];
                summaries.push(PageEntry {
                    line0: first.line0,
                    byte0: first.byte0,
                    child_page: page_id,
                });
                leaf_entries.clear();
            }
        }

        if !leaf_entries.is_empty() {
            let page_id = write_page(file, PageKind::Leaf, &leaf_entries, &mut page_count)?;
            let first = leaf_entries[0];
            summaries.push(PageEntry {
                line0: first.line0,
                byte0: first.byte0,
                child_page: page_id,
            });
        }

        let root_page = build_internal_levels(file, page_capacity, &mut page_count, summaries)?;
        let header = FileHeader {
            root_page,
            page_count,
            total_lines,
            total_bytes: bytes.len() as u64,
            source_len: metadata.source_len,
            source_mtime_ns: metadata.source_mtime_ns,
            source_fingerprint: metadata.source_fingerprint,
        };
        file.seek(SeekFrom::Start(0))?;
        write_header(file, header)
    })
}

fn build_internal_levels(
    file: &mut File,
    page_capacity: usize,
    page_count: &mut u64,
    mut summaries: Vec<PageEntry>,
) -> io::Result<u64> {
    if summaries.is_empty() {
        return Ok(0);
    }
    if summaries.len() == 1 {
        return Ok(summaries[0].child_page);
    }

    while summaries.len() > 1 {
        let mut next = Vec::new();
        for chunk in summaries.chunks(page_capacity.max(1)) {
            let page_id = write_page(file, PageKind::Internal, chunk, page_count)?;
            let first = chunk[0];
            next.push(PageEntry {
                line0: first.line0,
                byte0: first.byte0,
                child_page: page_id,
            });
        }
        summaries = next;
    }

    Ok(summaries[0].child_page)
}

fn write_page(
    file: &mut File,
    kind: PageKind,
    entries: &[PageEntry],
    page_count: &mut u64,
) -> io::Result<u64> {
    let page_id = page_count.saturating_add(1);
    let mut buf = vec![0u8; INDEX_PAGE_SIZE];
    buf[0] = kind as u8;
    buf[8..16].copy_from_slice(&(entries.len() as u64).to_le_bytes());
    let mut cursor = INDEX_PAGE_HEADER_BYTES;
    for entry in entries {
        buf[cursor..cursor + 8].copy_from_slice(&entry.line0.to_le_bytes());
        buf[cursor + 8..cursor + 16].copy_from_slice(&entry.byte0.to_le_bytes());
        buf[cursor + 16..cursor + 24].copy_from_slice(&entry.child_page.to_le_bytes());
        cursor += INDEX_ENTRY_BYTES;
    }
    file.seek(SeekFrom::End(0))?;
    file.write_all(&buf)?;
    *page_count = page_id;
    Ok(page_id)
}

fn write_header(file: &mut File, header: FileHeader) -> io::Result<()> {
    let mut buf = [0u8; INDEX_HEADER_BYTES];
    buf[0..8].copy_from_slice(INDEX_MAGIC);
    buf[8..12].copy_from_slice(&INDEX_VERSION.to_le_bytes());
    buf[12..16].copy_from_slice(&(INDEX_PAGE_SIZE as u32).to_le_bytes());
    buf[16..24].copy_from_slice(&header.root_page.to_le_bytes());
    buf[24..32].copy_from_slice(&header.page_count.to_le_bytes());
    buf[32..40].copy_from_slice(&header.total_lines.to_le_bytes());
    buf[40..48].copy_from_slice(&header.total_bytes.to_le_bytes());
    buf[48..56].copy_from_slice(&header.source_len.to_le_bytes());
    buf[56..64].copy_from_slice(&header.source_mtime_ns.to_le_bytes());
    buf[64..72].copy_from_slice(&header.source_fingerprint.to_le_bytes());
    file.write_all(&buf)
}

fn read_header(file: &mut File) -> io::Result<FileHeader> {
    let mut buf = [0u8; INDEX_HEADER_BYTES];
    file.seek(SeekFrom::Start(0))?;
    file.read_exact(&mut buf)?;
    if &buf[0..8] != INDEX_MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid qem line index magic",
        ));
    }

    let version = u32::from_le_bytes(buf[8..12].try_into().unwrap_or([0; 4]));
    let page_size = u32::from_le_bytes(buf[12..16].try_into().unwrap_or([0; 4]));
    if version != INDEX_VERSION || page_size as usize != INDEX_PAGE_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unsupported qem line index version",
        ));
    }

    Ok(FileHeader {
        root_page: u64::from_le_bytes(buf[16..24].try_into().unwrap_or([0; 8])),
        page_count: u64::from_le_bytes(buf[24..32].try_into().unwrap_or([0; 8])),
        total_lines: u64::from_le_bytes(buf[32..40].try_into().unwrap_or([0; 8])),
        total_bytes: u64::from_le_bytes(buf[40..48].try_into().unwrap_or([0; 8])),
        source_len: u64::from_le_bytes(buf[48..56].try_into().unwrap_or([0; 8])),
        source_mtime_ns: u64::from_le_bytes(buf[56..64].try_into().unwrap_or([0; 8])),
        source_fingerprint: u64::from_le_bytes(buf[64..72].try_into().unwrap_or([0; 8])),
    })
}

fn parse_page(buf: &[u8]) -> io::Result<Page> {
    let kind = match buf.first().copied() {
        Some(1) => PageKind::Internal,
        Some(2) => PageKind::Leaf,
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid qem line index page kind",
            ));
        }
    };
    let count = u64::from_le_bytes(buf[8..16].try_into().unwrap_or([0; 8])) as usize;
    let mut entries = Vec::with_capacity(count);
    let mut cursor = INDEX_PAGE_HEADER_BYTES;
    for _ in 0..count {
        if cursor + INDEX_ENTRY_BYTES > buf.len() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "truncated qem line index page",
            ));
        }
        entries.push(PageEntry {
            line0: u64::from_le_bytes(buf[cursor..cursor + 8].try_into().unwrap_or([0; 8])),
            byte0: u64::from_le_bytes(buf[cursor + 8..cursor + 16].try_into().unwrap_or([0; 8])),
            child_page: u64::from_le_bytes(
                buf[cursor + 16..cursor + 24].try_into().unwrap_or([0; 8]),
            ),
        });
        cursor += INDEX_ENTRY_BYTES;
    }
    Ok(Page { kind, entries })
}

fn lookup_entry(entries: &[PageEntry], target_line0: u64) -> Option<PageEntry> {
    if entries.is_empty() {
        return None;
    }
    let idx = entries.partition_point(|entry| entry.line0 <= target_line0);
    Some(entries[idx.saturating_sub(1)])
}

fn sidecar_path(source_path: &Path) -> PathBuf {
    let parent = source_path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = source_path
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("qem");
    parent.join(format!(".{file_name}.qem.lineidx"))
}

fn source_metadata(path: &Path, bytes: &[u8]) -> io::Result<IndexMetadata> {
    let metadata = std::fs::metadata(path)?;
    let source_len = metadata.len();
    let source_mtime_ns = metadata
        .modified()?
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .min(u64::MAX as u128) as u64;
    Ok(IndexMetadata {
        source_len,
        source_mtime_ns,
        source_fingerprint: sampled_content_fingerprint(bytes),
    })
}

#[cfg(test)]
mod tests {
    use super::{
        build_index_file, build_or_open_index, sidecar_path, source_metadata, FileHeader,
        ReadyDiskLineIndex,
    };
    use crate::storage::FileStorage;
    use std::fs;
    use std::io::{Seek, SeekFrom, Write};

    #[test]
    fn disk_line_index_builds_and_resolves_checkpoints() {
        let dir = std::env::temp_dir().join(format!("qem-index-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("sample.txt");
        let text = "aa\n".repeat(20_000);
        fs::write(&path, text).unwrap();

        let storage = FileStorage::open(&path).unwrap();
        let metadata = source_metadata(&path, storage.bytes()).unwrap();
        let sidecar = sidecar_path(&path);
        build_index_file(&sidecar, &storage, metadata).unwrap();
        let ready = ReadyDiskLineIndex::open_existing(&sidecar, metadata).unwrap();

        assert_eq!(ready.total_lines, 20_001);
        assert!(ready.page_count >= 1);

        let checkpoint = ready.checkpoint_for_line(16_384).unwrap();
        assert!(checkpoint.line0 <= 16_384);
        assert!(checkpoint.byte0 <= storage.len() as u64);

        let _ = fs::remove_file(&sidecar);
        let _ = fs::remove_file(&path);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn index_header_round_trips() {
        let header = FileHeader {
            root_page: 7,
            page_count: 9,
            total_lines: 123,
            total_bytes: 456,
            source_len: 456,
            source_mtime_ns: 789,
            source_fingerprint: 987,
        };
        let dir = std::env::temp_dir().join(format!("qem-index-header-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("header.bin");
        let mut file = fs::File::create(&path).unwrap();
        super::write_header(&mut file, header).unwrap();
        drop(file);

        let mut file = fs::File::open(&path).unwrap();
        let parsed = super::read_header(&mut file).unwrap();
        assert_eq!(parsed.root_page, header.root_page);
        assert_eq!(parsed.page_count, header.page_count);
        assert_eq!(parsed.total_lines, header.total_lines);
        assert_eq!(parsed.total_bytes, header.total_bytes);
        assert_eq!(parsed.source_len, header.source_len);
        assert_eq!(parsed.source_mtime_ns, header.source_mtime_ns);
        assert_eq!(parsed.source_fingerprint, header.source_fingerprint);

        let _ = fs::remove_file(&path);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn line_index_rejects_stale_content_fingerprint_even_with_matching_file_metadata() {
        let dir = std::env::temp_dir().join(format!(
            "qem-index-stale-fingerprint-{}",
            std::process::id()
        ));
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("sample.txt");
        let text = "aa\n".repeat(10_000);
        fs::write(&path, text).unwrap();

        let storage = FileStorage::open(&path).unwrap();
        let metadata = source_metadata(&path, storage.bytes()).unwrap();
        let sidecar = sidecar_path(&path);
        build_index_file(&sidecar, &storage, metadata).unwrap();

        let stale = super::IndexMetadata {
            source_len: metadata.source_len,
            source_mtime_ns: metadata.source_mtime_ns,
            source_fingerprint: metadata.source_fingerprint ^ 1,
        };
        let err = ReadyDiskLineIndex::open_existing(&sidecar, stale).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);

        let _ = fs::remove_file(&sidecar);
        let _ = fs::remove_file(&path);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn line_index_rebuilds_when_root_page_is_corrupt() {
        let dir =
            std::env::temp_dir().join(format!("qem-index-corrupt-root-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("sample.txt");
        let text = "aa\n".repeat(40_000);
        fs::write(&path, text).unwrap();

        let storage = FileStorage::open(&path).unwrap();
        let metadata = source_metadata(&path, storage.bytes()).unwrap();
        let sidecar = sidecar_path(&path);
        build_index_file(&sidecar, &storage, metadata).unwrap();

        {
            let mut file = fs::OpenOptions::new().write(true).open(&sidecar).unwrap();
            file.seek(SeekFrom::Start(super::INDEX_HEADER_BYTES as u64))
                .unwrap();
            file.write_all(&[0xFF]).unwrap();
            file.flush().unwrap();
        }

        let err = ReadyDiskLineIndex::open_existing(&sidecar, metadata).unwrap_err();
        assert!(matches!(
            err.kind(),
            std::io::ErrorKind::InvalidData | std::io::ErrorKind::UnexpectedEof
        ));

        let rebuilt = build_or_open_index(&path, &storage, metadata).unwrap();
        assert_eq!(rebuilt.total_lines, 40_001);
        let checkpoint = rebuilt.checkpoint_for_line(16_384).unwrap();
        assert!(checkpoint.line0 <= 16_384);
        assert!(checkpoint.byte0 <= storage.len() as u64);

        let _ = fs::remove_file(&sidecar);
        let _ = fs::remove_file(&path);
        let _ = fs::remove_dir_all(&dir);
    }
}
