//! Little-endian field readers for Qem's internal sidecar files.
//!
//! `.qem.lineidx` and `.qem.editlog` use fixed-layout pages whose counters
//! and offsets are decoded with these helpers. A short or malformed buffer
//! must surface as a typed error instead of silently decoding zeroed
//! fields: both sidecars validate and rebuild from scratch when opening
//! returns such an error, and silently zeroed fields would corrupt that
//! decision (or, for page entry counts, read out of bounds).

use std::io;

fn truncated(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::UnexpectedEof, format!("truncated {what}"))
}

fn field<const N: usize>(buf: &[u8], offset: usize, what: &str) -> io::Result<[u8; N]> {
    let end = offset.checked_add(N).ok_or_else(|| truncated(what))?;
    let bytes = buf.get(offset..end).ok_or_else(|| truncated(what))?;
    bytes.try_into().map_err(|_| truncated(what))
}

/// Reads a little-endian `u32` field at `offset` from a fixed-layout
/// sidecar buffer, or fails with a typed truncation error.
pub(crate) fn le_u32(buf: &[u8], offset: usize, what: &str) -> io::Result<u32> {
    Ok(u32::from_le_bytes(field::<4>(buf, offset, what)?))
}

/// Reads a little-endian `u64` field at `offset` from a fixed-layout
/// sidecar buffer, or fails with a typed truncation error.
pub(crate) fn le_u64(buf: &[u8], offset: usize, what: &str) -> io::Result<u64> {
    Ok(u64::from_le_bytes(field::<8>(buf, offset, what)?))
}
