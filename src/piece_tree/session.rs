use super::btree::ChildRef;
use super::history::RootEntry;
use super::{
    PageKind, EDITLOG_ENCODING_LABEL_BYTES, EDITLOG_ENCODING_LABEL_OFFSET, EDITLOG_HEADER_BYTES,
    EDITLOG_MAGIC, EDITLOG_PAGE_HEADER_BYTES, EDITLOG_PAGE_SIZE, EDITLOG_V3_HEADER_BYTES,
    EDITLOG_V4_EXT_ADD_INDEX_OFFSET, EDITLOG_V4_EXT_EPOCH_OFFSET, EDITLOG_V4_EXT_LOG_INDEX_OFFSET,
    EDITLOG_V4_EXT_VECTOR_LEN_OFFSET, EDITLOG_VERSION, HISTORY_ENTRY_BYTES,
    HISTORY_LOG_ENTRY_BYTES_V4, PAGE_CHAIN_LINK_OFFSET,
};
use crate::document::{DocumentEncoding, DocumentEncodingOrigin};
use crate::sidecar::{le_u32, le_u64};
use crc32fast::Hasher;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy, Debug, Default)]
// Session metadata persisted alongside the piece-tree root history.
//
// These fields are recovery hints for reopened sessions:
// - `known_byte_len` is the logical document length represented by the current
//   committed root
// - `full_index` tells recovery whether the piece-table snapshot already had a
//   complete line index when it was flushed
// - `encoding` is the document encoding contract at flush time (v3 header
//   label); `None` restores the pre-v3 behavior where reopened sessions were
//   always interpreted as UTF-8
pub(crate) struct SessionMeta {
    pub(crate) known_byte_len: usize,
    pub(crate) full_index: bool,
    pub(crate) encoding: Option<DocumentEncoding>,
    pub(crate) encoding_origin: Option<DocumentEncodingOrigin>,
    pub(crate) decoding_had_errors: bool,
}

pub(super) fn encode_encoding_origin(origin: DocumentEncodingOrigin) -> u32 {
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

pub(super) fn decode_encoding_origin(raw: u32) -> Option<DocumentEncodingOrigin> {
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

/// Encodes a canonical `encoding_rs` label into the fixed NUL-padded header
/// buffer. Canonical encoding names are short ASCII strings, so truncation is
/// a defensive bound rather than an expected path.
fn encode_encoding_label(label: &str) -> [u8; EDITLOG_ENCODING_LABEL_BYTES] {
    let mut buf = [0u8; EDITLOG_ENCODING_LABEL_BYTES];
    let bytes = label.as_bytes();
    let len = bytes.len().min(buf.len());
    buf[..len].copy_from_slice(&bytes[..len]);
    buf
}

/// Decodes the NUL-padded header label back into a document encoding. An
/// all-NUL buffer means the sidecar carries no encoding hint (v2 headers and
/// freshly created sidecars) and restores `None`. Any non-empty label must be
/// valid UTF-8 naming an encoding known to `encoding_rs`; anything else is a
/// corrupt header and must reject the session so recovery drops and rebuilds
/// the sidecar.
fn decode_encoding_label(
    raw: &[u8; EDITLOG_ENCODING_LABEL_BYTES],
) -> io::Result<Option<DocumentEncoding>> {
    let len = raw
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(EDITLOG_ENCODING_LABEL_BYTES);
    let bytes = &raw[..len];
    if bytes.is_empty() {
        return Ok(None);
    }
    let Ok(label) = std::str::from_utf8(bytes) else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "qem editlog encoding label is not valid UTF-8",
        ));
    };
    DocumentEncoding::from_label(label)
        .map(Some)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "qem editlog encoding label does not name a known encoding",
            )
        })
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
// - `encoding` is the NUL-padded document encoding label (v3 header tail);
//   `None` encodes as an all-NUL buffer
pub(super) struct EditLogHeader {
    pub(super) page_count: u64,
    pub(super) source_len: u64,
    pub(super) source_mtime_ns: u64,
    pub(super) source_fingerprint: u64,
    pub(super) history_first_page_id: u64,
    pub(super) history_page_count: u64,
    pub(super) history_len: u64,
    pub(super) history_index: u64,
    pub(super) add_first_page_id: u64,
    pub(super) add_page_count: u64,
    pub(super) add_len: u64,
    pub(super) known_byte_len: u64,
    pub(super) flags: u64,
    pub(super) encoding_origin: u32,
    pub(super) encoding: Option<DocumentEncoding>,
}

pub(super) fn read_editlog_header(file: &mut File) -> io::Result<EditLogHeader> {
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
    // The v4 header is consumed by `read_editlog_header_v4` before this
    // legacy reader runs, so only the v3 version (192-byte layout) and a
    // genuinely unknown format may reach here.
    if version != 3 || page_size as usize != EDITLOG_PAGE_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unsupported qem editlog format",
        ));
    }
    let mut encoding_label = [0u8; EDITLOG_ENCODING_LABEL_BYTES];
    encoding_label.copy_from_slice(
        &buf[EDITLOG_ENCODING_LABEL_OFFSET
            ..EDITLOG_ENCODING_LABEL_OFFSET + EDITLOG_ENCODING_LABEL_BYTES],
    );
    let encoding = decode_encoding_label(&encoding_label)?;
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
        encoding,
    })
}

pub(super) fn history_page_capacity() -> usize {
    (EDITLOG_PAGE_SIZE - EDITLOG_PAGE_HEADER_BYTES) / HISTORY_ENTRY_BYTES
}

// ---------------------------------------------------------------------------
// v4 append-only format
//
// v3 rewrote the whole history vector and the whole add buffer on every
// flush, so a long session paid quadratic sidecar writes (measured: 6.7 GB
// of sidecar after 10 000 small edits). v4 makes the sidecar append-only:
//
// - the history is an append-only *log*; each entry records its parent
//   (flat sequence number), its history-vector position, and the epoch.
//   Recovery rebuilds the live vector by walking parents from the current
//   entry and appends the redo tail (entries of the committed epoch with a
//   higher vector index).
// - the add buffer is an append-only page chain.
// - two index pages (log, add) enumerate the chain page ids and are
//   rewritten on every commit; data pages are never rewritten.
// - dead pages of abandoned branches stay until the next full rewrite
//   (which happens on every save), so growth stays linear in the number of
//   new edits instead of quadratic in the session length.
// ---------------------------------------------------------------------------

/// One append-only history-log entry (v4). `flat` is the 1-based sequence
/// number of the entry inside the log and is derived from the read order,
/// not stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct HistoryLogEntryV4 {
    pub(super) root: Option<ChildRef>,
    pub(super) parent_flat: u64,
    pub(super) vector_index: u64,
    pub(super) epoch: u64,
}

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct EditLogHeaderV4 {
    pub(super) page_count: u64,
    pub(super) source_len: u64,
    pub(super) source_mtime_ns: u64,
    pub(super) source_fingerprint: u64,
    /// First page of the history log chain.
    pub(super) log_head_page_id: u64,
    /// Number of pages in the history log chain (dead branches included).
    pub(super) log_page_count: u64,
    /// Total log entries appended so far (flat sequence length).
    pub(super) log_entry_count: u64,
    /// Flat id of the entry at the committed `history_index` position.
    pub(super) current_flat: u64,
    /// Committed epoch: bumped whenever the history vector is truncated
    /// (a new edit after undo), so the redo tail is identifiable.
    pub(super) epoch: u64,
    /// Page id of the committed history-log index page (0 = no log).
    pub(super) log_index_page_id: u64,
    /// Page id of the committed add-chain index page (0 = no add data).
    pub(super) add_index_page_id: u64,
    pub(super) add_head_page_id: u64,
    pub(super) add_page_count: u64,
    pub(super) add_len: u64,
    pub(super) known_byte_len: u64,
    pub(super) flags: u64,
    pub(super) encoding_origin: u32,
    pub(super) encoding: Option<DocumentEncoding>,
    /// Full history-vector length at commit time (redo tail included).
    pub(super) vector_len: u64,
}

pub(super) fn log_page_capacity_v4() -> usize {
    (EDITLOG_PAGE_SIZE - EDITLOG_PAGE_HEADER_BYTES) / HISTORY_LOG_ENTRY_BYTES_V4
}

pub(super) fn index_page_capacity_v4() -> usize {
    (EDITLOG_PAGE_SIZE - EDITLOG_PAGE_HEADER_BYTES) / 8
}

pub(super) fn add_page_capacity_v4() -> usize {
    EDITLOG_PAGE_SIZE - EDITLOG_PAGE_HEADER_BYTES
}

fn page_link(buf: &mut [u8], next_page_id: u64) {
    buf[PAGE_CHAIN_LINK_OFFSET..PAGE_CHAIN_LINK_OFFSET + 8]
        .copy_from_slice(&next_page_id.to_le_bytes());
}

fn finish_page(mut buf: Vec<u8>, kind: PageKind) -> Vec<u8> {
    buf[4] = kind as u8;
    let mut hasher = Hasher::new();
    hasher.update(&buf[4..]);
    let crc = hasher.finalize().to_le_bytes();
    buf[0..4].copy_from_slice(&crc);
    buf
}

pub(super) fn serialize_history_log_page_v4(
    entries: &[HistoryLogEntryV4],
    next_page_id: u64,
) -> io::Result<Vec<u8>> {
    let mut buf = vec![0u8; EDITLOG_PAGE_SIZE];
    let max_entries = log_page_capacity_v4().max(1);
    if entries.len() > max_entries {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "history log page exceeds fixed editlog capacity",
        ));
    }
    page_link(&mut buf, next_page_id);
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
        buf[cursor + 32..cursor + 40].copy_from_slice(&entry.parent_flat.to_le_bytes());
        buf[cursor + 40..cursor + 48].copy_from_slice(&entry.vector_index.to_le_bytes());
        buf[cursor + 48..cursor + 56].copy_from_slice(&entry.epoch.to_le_bytes());
        cursor += HISTORY_LOG_ENTRY_BYTES_V4;
    }
    Ok(finish_page(buf, PageKind::HistoryLog))
}

pub(super) fn serialize_add_chain_page_v4(chunk: &[u8], next_page_id: u64) -> io::Result<Vec<u8>> {
    let mut buf = vec![0u8; EDITLOG_PAGE_SIZE];
    let max_len = add_page_capacity_v4().max(1);
    if chunk.len() > max_len {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "add-data page exceeds fixed editlog capacity",
        ));
    }
    page_link(&mut buf, next_page_id);
    buf[8..16].copy_from_slice(&(chunk.len() as u64).to_le_bytes());
    buf[EDITLOG_PAGE_HEADER_BYTES..EDITLOG_PAGE_HEADER_BYTES + chunk.len()].copy_from_slice(chunk);
    Ok(finish_page(buf, PageKind::AddChain))
}

/// Serializes one index page: an enumeration of data-page ids of one chain.
/// `next_index_page_id` links the index pages themselves (an index chain is
/// rewritten on every commit and is short, so the single-page form is the
/// normal case).
pub(super) fn serialize_index_page_v4(
    kind: PageKind,
    page_ids: &[u64],
    next_index_page_id: u64,
) -> io::Result<Vec<u8>> {
    let mut buf = vec![0u8; EDITLOG_PAGE_SIZE];
    let max_ids = index_page_capacity_v4().max(1);
    if page_ids.len() > max_ids {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "editlog index page exceeds fixed editlog capacity",
        ));
    }
    page_link(&mut buf, next_index_page_id);
    buf[8..16].copy_from_slice(&(page_ids.len() as u64).to_le_bytes());
    let mut cursor = EDITLOG_PAGE_HEADER_BYTES;
    for &page_id in page_ids {
        buf[cursor..cursor + 8].copy_from_slice(&page_id.to_le_bytes());
        cursor += 8;
    }
    Ok(finish_page(buf, kind))
}

pub(super) fn write_editlog_header_v4(file: &mut File, header: EditLogHeaderV4) -> io::Result<()> {
    let mut buf = [0u8; EDITLOG_HEADER_BYTES];
    buf[0..8].copy_from_slice(EDITLOG_MAGIC);
    buf[8..12].copy_from_slice(&EDITLOG_VERSION.to_le_bytes());
    buf[12..16].copy_from_slice(&(EDITLOG_PAGE_SIZE as u32).to_le_bytes());
    buf[16..24].copy_from_slice(&header.page_count.to_le_bytes());
    buf[24..32].copy_from_slice(&header.source_len.to_le_bytes());
    buf[32..40].copy_from_slice(&header.source_mtime_ns.to_le_bytes());
    buf[40..48].copy_from_slice(&header.source_fingerprint.to_le_bytes());
    buf[48..56].copy_from_slice(&header.log_head_page_id.to_le_bytes());
    buf[56..64].copy_from_slice(&header.log_page_count.to_le_bytes());
    buf[64..72].copy_from_slice(&header.log_entry_count.to_le_bytes());
    buf[72..80].copy_from_slice(&header.current_flat.to_le_bytes());
    buf[80..88].copy_from_slice(&header.add_head_page_id.to_le_bytes());
    buf[88..96].copy_from_slice(&header.add_page_count.to_le_bytes());
    buf[96..104].copy_from_slice(&header.add_len.to_le_bytes());
    buf[104..112].copy_from_slice(&header.known_byte_len.to_le_bytes());
    buf[112..120].copy_from_slice(&header.flags.to_le_bytes());
    buf[120..124].copy_from_slice(&header.encoding_origin.to_le_bytes());
    let label = header
        .encoding
        .map(|encoding| encode_encoding_label(encoding.name()))
        .unwrap_or([0u8; EDITLOG_ENCODING_LABEL_BYTES]);
    buf[EDITLOG_ENCODING_LABEL_OFFSET
        ..EDITLOG_ENCODING_LABEL_OFFSET + EDITLOG_ENCODING_LABEL_BYTES]
        .copy_from_slice(&label);
    buf[124..128].copy_from_slice(
        &(SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            .min(u32::MAX as u64) as u32)
            .to_le_bytes(),
    );
    buf[EDITLOG_V4_EXT_VECTOR_LEN_OFFSET..EDITLOG_V4_EXT_VECTOR_LEN_OFFSET + 8]
        .copy_from_slice(&header.vector_len.to_le_bytes());
    buf[EDITLOG_V4_EXT_EPOCH_OFFSET..EDITLOG_V4_EXT_EPOCH_OFFSET + 8]
        .copy_from_slice(&header.epoch.to_le_bytes());
    buf[EDITLOG_V4_EXT_LOG_INDEX_OFFSET..EDITLOG_V4_EXT_LOG_INDEX_OFFSET + 8]
        .copy_from_slice(&header.log_index_page_id.to_le_bytes());
    buf[EDITLOG_V4_EXT_ADD_INDEX_OFFSET..EDITLOG_V4_EXT_ADD_INDEX_OFFSET + 8]
        .copy_from_slice(&header.add_index_page_id.to_le_bytes());
    file.seek(SeekFrom::Start(0))?;
    file.write_all(&buf)
}

/// Reads the v4 header. `Ok(None)` means the sidecar uses a different
/// format version (a v3 sidecar falls through to the legacy recovery path);
/// any `Err` is genuine corruption and drops the sidecar.
pub(super) fn read_editlog_header_v4(file: &mut File) -> io::Result<Option<EditLogHeaderV4>> {
    // The format decision needs only the 16-byte prefix; a v3 sidecar header
    // is 192 bytes, so reading the full 256 up front would misreport a page-less
    // v3 sidecar as truncated.
    let mut prefix = [0u8; 16];
    file.seek(SeekFrom::Start(0))?;
    file.read_exact(&mut prefix)?;
    if &prefix[0..8] != EDITLOG_MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid qem editlog magic",
        ));
    }
    let version = le_u32(&prefix, 8, "qem editlog header")?;
    let page_size = le_u32(&prefix, 12, "qem editlog header")?;
    if version != EDITLOG_VERSION || page_size as usize != EDITLOG_PAGE_SIZE {
        return Ok(None);
    }
    let mut buf = [0u8; EDITLOG_HEADER_BYTES];
    file.seek(SeekFrom::Start(0))?;
    file.read_exact(&mut buf)?;
    let mut encoding_label = [0u8; EDITLOG_ENCODING_LABEL_BYTES];
    encoding_label.copy_from_slice(
        &buf[EDITLOG_ENCODING_LABEL_OFFSET
            ..EDITLOG_ENCODING_LABEL_OFFSET + EDITLOG_ENCODING_LABEL_BYTES],
    );
    let encoding = decode_encoding_label(&encoding_label)?;
    Ok(Some(EditLogHeaderV4 {
        page_count: le_u64(&buf, 16, "qem editlog header")?,
        source_len: le_u64(&buf, 24, "qem editlog header")?,
        source_mtime_ns: le_u64(&buf, 32, "qem editlog header")?,
        source_fingerprint: le_u64(&buf, 40, "qem editlog header")?,
        log_head_page_id: le_u64(&buf, 48, "qem editlog header")?,
        log_page_count: le_u64(&buf, 56, "qem editlog header")?,
        log_entry_count: le_u64(&buf, 64, "qem editlog header")?,
        current_flat: le_u64(&buf, 72, "qem editlog header")?,
        add_head_page_id: le_u64(&buf, 80, "qem editlog header")?,
        add_page_count: le_u64(&buf, 88, "qem editlog header")?,
        add_len: le_u64(&buf, 96, "qem editlog header")?,
        known_byte_len: le_u64(&buf, 104, "qem editlog header")?,
        flags: le_u64(&buf, 112, "qem editlog header")?,
        encoding_origin: le_u32(&buf, 120, "qem editlog header")?,
        encoding,
        vector_len: le_u64(&buf, EDITLOG_V4_EXT_VECTOR_LEN_OFFSET, "qem editlog header")?,
        epoch: le_u64(&buf, EDITLOG_V4_EXT_EPOCH_OFFSET, "qem editlog header")?,
        log_index_page_id: le_u64(&buf, EDITLOG_V4_EXT_LOG_INDEX_OFFSET, "qem editlog header")?,
        add_index_page_id: le_u64(&buf, EDITLOG_V4_EXT_ADD_INDEX_OFFSET, "qem editlog header")?,
    }))
}

/// Reads an index chain (LogIndex or AddIndex) and returns the enumerated
/// data-page ids in chain order. Counters taken from the raw header are
/// bounded by what the sidecar can physically hold before they reach an
/// allocation.
pub(super) fn read_index_chain_v4(
    file: &mut File,
    index_page_id: u64,
    expected_kind: PageKind,
    max_pages: usize,
) -> io::Result<Vec<u64>> {
    let header_bytes = EDITLOG_HEADER_BYTES as u64;
    if index_page_id == 0 {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    let mut page_id = index_page_id;
    let mut visited = 0usize;
    loop {
        visited += 1;
        if visited > max_pages || out.len() > max_pages {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "qem editlog index chain exceeds the sidecar size",
            ));
        }
        let buf = read_raw_page_from_file(file, page_id, header_bytes)?;
        let count = parse_page_header(&buf, expected_kind)?;
        if count > index_page_capacity_v4() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "qem editlog index page is overfull",
            ));
        }
        let mut cursor = EDITLOG_PAGE_HEADER_BYTES;
        for _ in 0..count {
            out.push(le_u64(&buf, cursor, "qem editlog index page")?);
            cursor += 8;
        }
        let next = le_u64(&buf, PAGE_CHAIN_LINK_OFFSET, "qem editlog index page")?;
        if next == 0 {
            return Ok(out);
        }
        page_id = next;
    }
}

/// Reads the append-only history log described by its page-id enumeration
/// and returns the entries with their flat sequence numbers (1-based, in
/// log order).
pub(super) fn read_log_entries_v4(
    file: &mut File,
    log_page_ids: &[u64],
    entry_count: usize,
) -> io::Result<Vec<HistoryLogEntryV4>> {
    if log_page_ids.is_empty() || entry_count == 0 {
        return Ok(Vec::new());
    }
    let page_capacity = log_page_capacity_v4().max(1);
    let entry_count = entry_count.min(log_page_ids.len().saturating_mul(page_capacity));
    let mut out = Vec::with_capacity(entry_count);
    'pages: for &page_id in log_page_ids {
        let buf = read_raw_page_from_file(file, page_id, EDITLOG_HEADER_BYTES as u64)?;
        let count = parse_page_header(&buf, PageKind::HistoryLog)?;
        if count > page_capacity {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "qem editlog history log page is overfull",
            ));
        }
        let mut cursor = EDITLOG_PAGE_HEADER_BYTES;
        for _ in 0..count {
            if out.len() >= entry_count {
                break 'pages;
            }
            let present = le_u64(&buf, cursor, "qem editlog history log")?;
            let root = if present == 0 {
                None
            } else {
                Some(ChildRef {
                    page_id: le_u64(&buf, cursor + 8, "qem editlog history log")?,
                    total_bytes: le_u64(&buf, cursor + 16, "qem editlog history log")? as usize,
                    total_line_breaks: le_u64(&buf, cursor + 24, "qem editlog history log")?
                        as usize,
                })
            };
            out.push(HistoryLogEntryV4 {
                root,
                parent_flat: le_u64(&buf, cursor + 32, "qem editlog history log")?,
                vector_index: le_u64(&buf, cursor + 40, "qem editlog history log")?,
                epoch: le_u64(&buf, cursor + 48, "qem editlog history log")?,
            });
            cursor += HISTORY_LOG_ENTRY_BYTES_V4;
        }
    }
    Ok(out)
}

pub(super) fn read_add_bytes_v4(
    file: &mut File,
    add_page_ids: &[u64],
    add_len: usize,
) -> io::Result<Vec<u8>> {
    if add_page_ids.is_empty() || add_len == 0 {
        return Ok(Vec::new());
    }
    let page_capacity = add_page_capacity_v4().max(1);
    let add_len = add_len.min(add_page_ids.len().saturating_mul(page_capacity));
    let mut out = Vec::with_capacity(add_len);
    for &page_id in add_page_ids {
        let buf = read_raw_page_from_file(file, page_id, EDITLOG_HEADER_BYTES as u64)?;
        let payload_len = parse_page_header(&buf, PageKind::AddChain)?;
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

pub(super) fn add_page_capacity() -> usize {
    EDITLOG_PAGE_SIZE - EDITLOG_PAGE_HEADER_BYTES
}

#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn serialize_history_page(entries: &[RootEntry]) -> io::Result<Vec<u8>> {
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

#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn serialize_add_page(chunk: &[u8]) -> io::Result<Vec<u8>> {
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

pub(super) fn read_history_entries(
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
    let page_count = bounded_page_count(
        file.metadata().map(|meta| meta.len()).ok(),
        page_count,
        EDITLOG_V3_HEADER_BYTES as u64,
    );
    let history_len = history_len.min(page_count.saturating_mul(page_capacity));

    let mut out = Vec::with_capacity(history_len);
    for rel in 0..page_count {
        let page_id = first_page_id.saturating_add(rel as u64);
        let buf = read_raw_page_from_file(file, page_id, EDITLOG_V3_HEADER_BYTES as u64)?;
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

pub(super) fn read_add_bytes(
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
    let page_count = bounded_page_count(
        file.metadata().map(|meta| meta.len()).ok(),
        page_count,
        EDITLOG_V3_HEADER_BYTES as u64,
    );
    let add_len = add_len.min(page_count.saturating_mul(add_page_capacity().max(1)));

    let mut out = Vec::with_capacity(add_len);
    for rel in 0..page_count {
        let page_id = first_page_id.saturating_add(rel as u64);
        let buf = read_raw_page_from_file(file, page_id, EDITLOG_V3_HEADER_BYTES as u64)?;
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
pub(super) fn bounded_page_count(
    file_len: Option<u64>,
    page_count: usize,
    header_bytes: u64,
) -> usize {
    let Some(file_len) = file_len else {
        return 0;
    };
    let usable = file_len
        .saturating_sub(header_bytes)
        .saturating_div(EDITLOG_PAGE_SIZE as u64);
    let usable = usize::try_from(usable).unwrap_or(usize::MAX);
    page_count.min(usable)
}

fn read_raw_page_from_file(
    file: &mut File,
    page_id: u64,
    header_bytes: u64,
) -> io::Result<Vec<u8>> {
    let mut buf = vec![0u8; EDITLOG_PAGE_SIZE];
    let offset = header_bytes.saturating_add(
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
        5 => PageKind::HistoryLog,
        6 => PageKind::AddChain,
        7 => PageKind::LogIndex,
        8 => PageKind::AddIndex,
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
