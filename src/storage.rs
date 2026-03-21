use memmap2::Mmap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[cfg(not(windows))]
use std::io::{Seek, SeekFrom};

static TEMP_PATH_COUNTER: AtomicU64 = AtomicU64::new(0);
const STALE_QEM_TEMP_MAX_AGE: Duration = Duration::from_secs(60 * 60);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TempDirPolicy {
    Auto,
    SourceDir,
    SystemDir,
    ExeDir,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TempArtifactKind {
    AtomicRewrite,
    #[cfg(not(windows))]
    Snapshot,
}

#[derive(Debug)]
pub enum StorageOpenError {
    Open(io::Error),
    Map(io::Error),
}

impl StorageOpenError {
    pub fn into_io_error(self) -> io::Error {
        match self {
            Self::Open(err) | Self::Map(err) => err,
        }
    }
}

impl std::fmt::Display for StorageOpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Open(err) => write!(f, "{err}"),
            Self::Map(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for StorageOpenError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Open(err) | Self::Map(err) => Some(err),
        }
    }
}

#[derive(Clone, Debug)]
pub struct FileStorage {
    path: PathBuf,
    inner: Arc<StorageInner>,
}

#[derive(Debug)]
struct StorageInner {
    backing: StorageBacking,
}

#[derive(Debug)]
enum StorageBacking {
    Empty,
    Mapped {
        _file: File,
        mmap: Mmap,
    },
    #[cfg(not(windows))]
    Snapshot {
        mapping: SnapshotMapping,
    },
}

#[cfg(not(windows))]
#[derive(Debug)]
struct SnapshotMapping {
    path: PathBuf,
    file: Option<File>,
    mmap: Option<Mmap>,
}

#[cfg(not(windows))]
impl SnapshotMapping {
    fn bytes(&self) -> &[u8] {
        self.mmap.as_ref().map(|mmap| &mmap[..]).unwrap_or_default()
    }
}

#[cfg(not(windows))]
impl Drop for SnapshotMapping {
    fn drop(&mut self) {
        let _ = self.mmap.take();
        let _ = self.file.take();
        if !self.path.as_os_str().is_empty() {
            let _ = fs::remove_file(&self.path);
        }
    }
}

impl StorageInner {
    fn bytes(&self) -> &[u8] {
        match &self.backing {
            StorageBacking::Empty => &[],
            StorageBacking::Mapped { mmap, .. } => &mmap[..],
            #[cfg(not(windows))]
            StorageBacking::Snapshot { mapping } => mapping.bytes(),
        }
    }
}

impl FileStorage {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StorageOpenError> {
        let path = path.as_ref().to_path_buf();
        cleanup_stale_qem_artifacts(&path);
        let backing = open_storage_backing(&path)?;

        Ok(Self {
            path,
            inner: Arc::new(StorageInner { backing }),
        })
    }

    pub fn open_or_create(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref();
        if !path.exists() {
            OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(false)
                .open(path)?;
        }

        Self::open(path).map_err(StorageOpenError::into_io_error)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn len(&self) -> usize {
        self.inner.bytes().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn bytes(&self) -> &[u8] {
        self.inner.bytes()
    }

    pub fn read_range(&self, start: usize, end: usize) -> &[u8] {
        let bytes = self.bytes();
        let start = start.min(bytes.len());
        let end = end.min(bytes.len()).max(start);
        &bytes[start..end]
    }

    pub fn write_all(&self, data: &[u8]) -> io::Result<Self> {
        replace_file_contents(self.path(), data)?;
        Self::open(self.path()).map_err(StorageOpenError::into_io_error)
    }

    pub(crate) fn replace_with(
        path: impl AsRef<Path>,
        write: impl FnOnce(&mut File) -> io::Result<()>,
    ) -> io::Result<()> {
        replace_file(path.as_ref(), write)
    }
}

fn open_storage_backing(path: &Path) -> Result<StorageBacking, StorageOpenError> {
    let file = open_source_file(path).map_err(StorageOpenError::Open)?;
    let len = file.metadata().map_err(StorageOpenError::Open)?.len() as usize;
    if len == 0 {
        return Ok(StorageBacking::Empty);
    }

    #[cfg(windows)]
    {
        let mmap = unsafe { Mmap::map(&file) }.map_err(StorageOpenError::Map)?;
        Ok(StorageBacking::Mapped { _file: file, mmap })
    }

    #[cfg(not(windows))]
    {
        let _ = file;
        open_snapshot_backing(path)
    }
}

#[cfg(windows)]
fn open_source_file(path: &Path) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;

    const FILE_SHARE_READ: u32 = 0x00000001;
    const FILE_SHARE_DELETE: u32 = 0x00000004;

    let mut options = OpenOptions::new();
    options.read(true);
    options.share_mode(FILE_SHARE_READ | FILE_SHARE_DELETE);
    options.open(path)
}

#[cfg(not(windows))]
fn open_source_file(path: &Path) -> io::Result<File> {
    File::open(path)
}

#[cfg(not(windows))]
fn open_snapshot_backing(path: &Path) -> Result<StorageBacking, StorageOpenError> {
    const SNAPSHOT_RETRIES: usize = 3;

    for _attempt in 0..SNAPSHOT_RETRIES {
        let before = source_fingerprint(path).map_err(StorageOpenError::Open)?;
        let mut source = File::open(path).map_err(StorageOpenError::Open)?;
        let snapshot_path = unique_qem_temp_path(path, "snap", TempArtifactKind::Snapshot);
        let copy_result = copy_into_snapshot(&mut source, &snapshot_path);

        match copy_result {
            Ok(file) => {
                let after = source_fingerprint(path).map_err(StorageOpenError::Open)?;
                if before != after {
                    let _ = fs::remove_file(&snapshot_path);
                    continue;
                }
                let mmap = unsafe { Mmap::map(&file) }.map_err(StorageOpenError::Map)?;
                return Ok(StorageBacking::Snapshot {
                    mapping: SnapshotMapping {
                        path: snapshot_path,
                        file: Some(file),
                        mmap: Some(mmap),
                    },
                });
            }
            Err(err) => {
                let _ = fs::remove_file(&snapshot_path);
                return Err(StorageOpenError::Open(err));
            }
        }
    }

    Err(StorageOpenError::Open(io::Error::other(
        "file changed while creating a safe snapshot",
    )))
}

#[cfg(not(windows))]
fn copy_into_snapshot(source: &mut File, snapshot_path: &Path) -> io::Result<File> {
    source.seek(SeekFrom::Start(0))?;
    let mut snapshot = OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(snapshot_path)?;
    io::copy(source, &mut snapshot)?;
    snapshot.flush()?;
    snapshot.sync_all()?;
    snapshot.seek(SeekFrom::Start(0))?;
    Ok(snapshot)
}

#[cfg(not(windows))]
fn source_fingerprint(path: &Path) -> io::Result<(u64, Option<SystemTime>)> {
    let metadata = fs::metadata(path)?;
    Ok((metadata.len(), metadata.modified().ok()))
}

fn replace_file_contents(path: &Path, data: &[u8]) -> io::Result<()> {
    replace_file(path, |temp_file| temp_file.write_all(data))
}

fn replace_file(path: &Path, write: impl FnOnce(&mut File) -> io::Result<()>) -> io::Result<()> {
    cleanup_stale_qem_artifacts(path);
    let temp_path = unique_qem_temp_path(path, "tmp", TempArtifactKind::AtomicRewrite);

    let mut temp_file = OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(&temp_path)?;
    write(&mut temp_file)?;
    temp_file.flush()?;
    temp_file.sync_all()?;
    drop(temp_file);

    if let Err(err) = replace_temp_file(&temp_path, path) {
        let _ = fs::remove_file(&temp_path);
        return Err(err);
    }

    Ok(())
}

fn cleanup_stale_qem_artifacts(path: &Path) {
    let prefix = qem_temp_prefix(path);
    let now_nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let stale_after = STALE_QEM_TEMP_MAX_AGE.as_nanos();

    for dir in temp_artifact_candidate_dirs(path) {
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if !name.starts_with(&prefix) {
                continue;
            }
            let Some((suffix, timestamp, _nonce)) = parse_qem_temp_name(name, &prefix) else {
                continue;
            };
            if !matches!(suffix, "snap" | "tmp" | "bak" | "probe") {
                continue;
            }
            if now_nanos.saturating_sub(timestamp) < stale_after {
                continue;
            }
            let _ = fs::remove_file(entry.path());
        }
    }
}

fn parse_qem_temp_name<'a>(name: &'a str, prefix: &str) -> Option<(&'a str, u128, &'a str)> {
    let rest = name.strip_prefix(prefix)?;
    let mut parts = rest.rsplitn(3, '.');
    let nonce = parts.next()?;
    let timestamp = parts.next()?.parse().ok()?;
    let suffix = parts.next()?;
    Some((suffix, timestamp, nonce))
}

fn temp_artifact_candidate_dirs(path: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    push_dir_unique(&mut dirs, path.parent().map(Path::to_path_buf));
    push_dir_unique(&mut dirs, configured_tmp_root(path));
    push_dir_unique(&mut dirs, Some(std::env::temp_dir()));
    push_dir_unique(&mut dirs, executable_dir());
    dirs
}

fn configured_tmp_root(path: &Path) -> Option<PathBuf> {
    if let Some(custom) = std::env::var_os("QEM_TMP_DIR").filter(|v| !v.is_empty()) {
        let custom = PathBuf::from(custom);
        if is_usable_tmp_dir(&custom, path) {
            return Some(custom);
        }
    }

    let mut candidates = Vec::new();
    match temp_dir_policy() {
        TempDirPolicy::Auto => {
            #[cfg(windows)]
            {
                push_dir_unique(&mut candidates, executable_dir());
                push_dir_unique(&mut candidates, path.parent().map(Path::to_path_buf));
                push_dir_unique(&mut candidates, Some(std::env::temp_dir()));
            }
            #[cfg(not(windows))]
            {
                push_dir_unique(&mut candidates, path.parent().map(Path::to_path_buf));
                push_dir_unique(&mut candidates, Some(std::env::temp_dir()));
                push_dir_unique(&mut candidates, executable_dir());
            }
        }
        TempDirPolicy::SourceDir => {
            push_dir_unique(&mut candidates, path.parent().map(Path::to_path_buf));
            push_dir_unique(&mut candidates, Some(std::env::temp_dir()));
            push_dir_unique(&mut candidates, executable_dir());
        }
        TempDirPolicy::SystemDir => {
            push_dir_unique(&mut candidates, Some(std::env::temp_dir()));
            push_dir_unique(&mut candidates, path.parent().map(Path::to_path_buf));
            push_dir_unique(&mut candidates, executable_dir());
        }
        TempDirPolicy::ExeDir => {
            push_dir_unique(&mut candidates, executable_dir());
            push_dir_unique(&mut candidates, path.parent().map(Path::to_path_buf));
            push_dir_unique(&mut candidates, Some(std::env::temp_dir()));
        }
    }

    candidates
        .into_iter()
        .find(|candidate| is_usable_tmp_dir(candidate, path))
}

fn temp_dir_policy() -> TempDirPolicy {
    if let Ok(value) = std::env::var("QEM_TMP_POLICY") {
        match value.trim().to_ascii_lowercase().as_str() {
            "file" | "source" | "source-dir" | "near-file" => return TempDirPolicy::SourceDir,
            "temp" | "system" | "system-dir" | "system-temp" => return TempDirPolicy::SystemDir,
            "exe" | "exe-dir" | "near-exe" => return TempDirPolicy::ExeDir,
            "auto" => return TempDirPolicy::Auto,
            _ => {}
        }
    }

    if cfg!(feature = "tmp-source-dir") {
        TempDirPolicy::SourceDir
    } else if cfg!(feature = "tmp-system-dir") {
        TempDirPolicy::SystemDir
    } else if cfg!(feature = "tmp-exe-dir") {
        TempDirPolicy::ExeDir
    } else {
        TempDirPolicy::Auto
    }
}

fn executable_dir() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf))
}

fn is_usable_tmp_dir(dir: &Path, source_path: &Path) -> bool {
    if fs::create_dir_all(dir).is_err() {
        return false;
    }
    let probe = temp_path_in_dir(dir, source_path, "probe");
    match OpenOptions::new().create_new(true).write(true).open(&probe) {
        Ok(file) => {
            drop(file);
            let _ = fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

fn push_dir_unique(dirs: &mut Vec<PathBuf>, dir: Option<PathBuf>) {
    let Some(dir) = dir else {
        return;
    };
    if !dirs.iter().any(|candidate| candidate == &dir) {
        dirs.push(dir);
    }
}

#[cfg(not(windows))]
fn replace_temp_file(temp_path: &Path, path: &Path) -> io::Result<()> {
    fs::rename(temp_path, path)?;
    sync_parent_directory(path)
}

#[cfg(not(windows))]
fn sync_parent_directory(path: &Path) -> io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let dir = File::open(parent)?;
    dir.sync_all()
}

#[cfg(windows)]
fn replace_temp_file(temp_path: &Path, path: &Path) -> io::Result<()> {
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;

    const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x8;
    const REPLACEFILE_WRITE_THROUGH: u32 = 0x00000002;

    unsafe extern "system" {
        fn MoveFileExW(existing: *const u16, new: *const u16, flags: u32) -> i32;
        fn ReplaceFileW(
            replaced: *const u16,
            replacement: *const u16,
            backup: *const u16,
            flags: u32,
            exclude: *mut c_void,
            reserved: *mut c_void,
        ) -> i32;
    }

    fn wide(path: &Path) -> Vec<u16> {
        path.as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    let from = wide(temp_path);
    let to = wide(path);
    let ok = if path.exists() {
        unsafe {
            ReplaceFileW(
                to.as_ptr(),
                from.as_ptr(),
                std::ptr::null(),
                REPLACEFILE_WRITE_THROUGH,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        }
    } else {
        unsafe {
            MoveFileExW(
                from.as_ptr(),
                to.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        }
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn unique_qem_temp_path(path: &Path, suffix: &str, kind: TempArtifactKind) -> PathBuf {
    let parent = temp_root_for_artifact(path, kind);
    temp_path_in_dir(&parent, path, suffix)
}

fn temp_root_for_artifact(path: &Path, kind: TempArtifactKind) -> PathBuf {
    match kind {
        TempArtifactKind::AtomicRewrite => path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from(".")),
        #[cfg(not(windows))]
        TempArtifactKind::Snapshot => configured_tmp_root(path)
            .or_else(|| path.parent().map(Path::to_path_buf))
            .unwrap_or_else(std::env::temp_dir),
    }
}

fn temp_path_in_dir(dir: &Path, source_path: &Path, suffix: &str) -> PathBuf {
    let file_name = source_path
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("qem");
    let tag = qem_temp_tag(source_path);
    let nonce = TEMP_PATH_COUNTER.fetch_add(1, Ordering::Relaxed);
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();

    dir.join(format!(
        ".{file_name}.qem.{tag}.{suffix}.{timestamp}.{nonce}"
    ))
}

fn qem_temp_prefix(path: &Path) -> String {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("qem");
    let tag = qem_temp_tag(path);
    format!(".{file_name}.qem.{tag}.")
}

fn qem_temp_tag(path: &Path) -> String {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in path.to_string_lossy().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::{FileStorage, qem_temp_prefix};
    use std::fs::{self, OpenOptions};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn read_range_returns_requested_slice() {
        let dir = test_dir("range");
        let path = dir.join("bytes.bin");
        fs::write(&path, b"0123456789").unwrap();

        let storage = FileStorage::open(&path).unwrap();
        assert_eq!(storage.read_range(2, 6), b"2345");
        assert_eq!(storage.read_range(7, 99), b"789");

        drop(storage);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_all_replaces_file_even_with_existing_mapping() {
        let dir = test_dir("replace");
        let path = dir.join("mapped.bin");
        fs::write(&path, b"abcdef").unwrap();

        let original = FileStorage::open(&path).unwrap();
        let writer = FileStorage::open(&path).unwrap();
        let updated = writer.write_all(b"xy").unwrap();

        assert_eq!(original.bytes(), b"abcdef");
        assert_eq!(updated.bytes(), b"xy");
        assert_eq!(fs::read(&path).unwrap(), b"xy");

        drop(updated);
        drop(original);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn open_cleans_stale_qem_temp_artifacts() {
        let dir = test_dir("cleanup");
        let path = dir.join("artifact.bin");
        fs::write(&path, b"abcdef").unwrap();

        let stale = dir.join(format!("{}tmp.0.0", qem_temp_prefix(&path)));
        fs::write(&stale, b"stale").unwrap();
        assert!(stale.exists());

        let storage = FileStorage::open(&path).unwrap();

        assert_eq!(storage.bytes(), b"abcdef");
        assert!(!stale.exists(), "stale qem temp artifact should be removed");

        drop(storage);
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn open_prevents_concurrent_writes_on_windows() {
        let dir = test_dir("share");
        let path = dir.join("locked.bin");
        fs::write(&path, b"abcdef").unwrap();

        let storage = FileStorage::open(&path).unwrap();
        let writer = OpenOptions::new().write(true).open(&path);

        assert!(
            writer.is_err(),
            "writer should be blocked while storage is open"
        );

        drop(storage);
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(not(windows))]
    #[test]
    fn snapshot_storage_survives_external_truncate() {
        let dir = test_dir("snapshot");
        let path = dir.join("safe.bin");
        fs::write(&path, b"abcdef").unwrap();

        let storage = FileStorage::open(&path).unwrap();
        fs::write(&path, b"x").unwrap();

        assert_eq!(storage.bytes(), b"abcdef");

        drop(storage);
        let _ = fs::remove_dir_all(&dir);
    }

    fn test_dir(name: &str) -> PathBuf {
        let id = TEST_DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("qem-storage-{name}-{}-{id}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }
}
