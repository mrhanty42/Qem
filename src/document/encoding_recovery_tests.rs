//! Session-recovery tests for the `.qem.editlog` v3 encoding label (B5).
//!
//! The sidecar stores the document encoding in the header tail so reopened
//! sessions restore the encoding contract instead of forcing the caller to
//! guess it again: an implicit open trusts the stored label, an explicit
//! open must agree with it, and legacy v2 sidecars (no label) keep the
//! historical drop-and-rebuild behavior.

use super::*;
use encoding_rs::WINDOWS_1251;
use std::io::{Seek, Write};
use tempfile::tempdir;

/// Writes a fixture large enough for the piece-table edit buffer to become
/// disk-backed (`PIECE_TREE_DISK_MIN_BYTES`) with windows-1251 content:
/// `0xDF` is "Я" in windows-1251 and an invalid UTF-8 byte, so decoding the
/// bytes under the wrong contract is observable.
fn write_windows_1251_fixture(path: &Path) {
    let chunk = b"abc\xDF\n";
    let target_len = PIECE_TREE_DISK_MIN_BYTES + chunk.len();
    let mut file = std::fs::File::create(path).unwrap();
    let mut written = 0usize;
    while written < target_len {
        file.write_all(chunk).unwrap();
        written += chunk.len();
    }
    file.flush().unwrap();
}

fn windows_1251() -> DocumentEncoding {
    DocumentEncoding::from_encoding_rs(WINDOWS_1251)
}

/// Creates a disk-backed windows-1251 session with a leading insert so the
/// sidecar holds edits worth recovering.
fn flush_windows_1251_session(path: &Path) {
    let mut doc = Document::open_with_encoding(path, windows_1251()).unwrap();
    assert_eq!(doc.encoding(), windows_1251());
    let _ = doc.try_insert_text_at(0, 0, "123").unwrap();
    doc.flush_session().unwrap();
}

#[test]
fn recovered_session_restores_windows1251_encoding_without_explicit_request() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("session-cp1251.txt");
    write_windows_1251_fixture(&path);
    flush_windows_1251_session(&path);

    // Reopen without any explicit encoding: the stored label restores the
    // windows-1251 contract and the edited piece-table session.
    let recovered = Document::open(&path).unwrap();

    assert!(recovered.is_dirty());
    assert!(recovered.has_piece_table());
    assert_eq!(recovered.encoding(), windows_1251());
    assert_eq!(
        recovered.encoding_origin(),
        DocumentEncodingOrigin::ExplicitReinterpretation
    );
    assert!(!recovered.decoding_had_errors());
    assert!(recovered.text_lossy().starts_with("123abc\u{042F}\n"));

    // An explicit request that agrees with the stored label recovers the
    // session too.
    let reopened_explicit = Document::open_with_encoding(&path, windows_1251()).unwrap();
    assert!(reopened_explicit.has_piece_table());
    assert_eq!(reopened_explicit.encoding(), windows_1251());
    assert!(reopened_explicit
        .text_lossy()
        .starts_with("123abc\u{042F}\n"));
}

#[test]
fn legacy_v2_sidecar_keeps_the_historical_open_behavior() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("session-v2-cp1251.txt");
    write_windows_1251_fixture(&path);
    flush_windows_1251_session(&path);

    // Patch the sidecar version field back to v2: the historical format
    // carried no encoding label and is rejected by the version check, so
    // recovery drops and rebuilds from the source file like before.
    let sidecar = editlog_path(&path);
    assert!(sidecar.exists(), "expected persisted editlog sidecar");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .open(&sidecar)
        .unwrap();
    file.seek(std::io::SeekFrom::Start(8)).unwrap();
    file.write_all(&2u32.to_le_bytes()).unwrap();
    drop(file);

    // Implicit open: the unlabeled session is not recoverable, the document
    // falls back to a clean UTF-8 open of the pristine source bytes (the
    // windows-1251 byte shows up lossy under the UTF-8 contract).
    let reopened = Document::open(&path).unwrap();
    assert!(!reopened.has_edit_buffer());
    assert_eq!(reopened.encoding(), DocumentEncoding::utf8());
    assert!(reopened.text_lossy().starts_with("abc\u{FFFD}\n"));

    // Explicit open: the caller's encoding wins and no session is recovered
    // from a sidecar without a label.
    let reopened_explicit = Document::open_with_encoding(&path, windows_1251()).unwrap();
    assert!(!reopened_explicit.has_piece_table());
    assert_eq!(reopened_explicit.encoding(), windows_1251());
    assert!(reopened_explicit.text_lossy().starts_with("abc\u{042F}\n"));

    // The invalid sidecar was dropped so the next session rebuilds cleanly.
    assert!(!sidecar.exists());
}

#[test]
fn corrupt_encoding_label_rejects_the_session_and_rebuilds_from_source() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("session-corrupt-label.txt");
    write_windows_1251_fixture(&path);
    flush_windows_1251_session(&path);

    // Corrupt the v3 label with bytes that are neither valid UTF-8 nor a
    // known encoding label: the header must reject the session so recovery
    // drops and rebuilds the sidecar from the source file.
    let sidecar = editlog_path(&path);
    assert!(sidecar.exists(), "expected persisted editlog sidecar");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .open(&sidecar)
        .unwrap();
    file.seek(std::io::SeekFrom::Start(
        crate::piece_tree::EDITLOG_ENCODING_LABEL_OFFSET as u64,
    ))
    .unwrap();
    file.write_all(&[0xFF; crate::piece_tree::EDITLOG_ENCODING_LABEL_BYTES])
        .unwrap();
    drop(file);

    let reopened = Document::open(&path).unwrap();

    assert!(!reopened.has_edit_buffer());
    assert!(!reopened.has_piece_table());
    assert!(reopened.text_lossy().starts_with("abc\u{FFFD}\n"));
    assert!(!sidecar.exists(), "invalid sidecar must be dropped");
}

#[test]
fn mismatched_explicit_encoding_does_not_recover_the_session() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("session-mismatch.txt");
    write_windows_1251_fixture(&path);
    flush_windows_1251_session(&path);

    // The caller's explicit contract disagrees with the stored label: the
    // caller wins and the document opens fresh instead of recovering a
    // session stored under a different encoding.
    let reopened = Document::open_with_encoding(&path, DocumentEncoding::utf8()).unwrap();

    assert!(!reopened.has_edit_buffer());
    assert!(!reopened.has_piece_table());
    assert_eq!(reopened.encoding(), DocumentEncoding::utf8());

    // The sidecar stays on disk: a later open that agrees with the stored
    // label can still recover the session.
    assert!(editlog_path(&path).exists());
}
