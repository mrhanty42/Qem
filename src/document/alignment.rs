//! Encoding-aware byte-offset alignment.
//!
//! [`Document::align_byte_offset`] is the single surface that clamps a raw
//! byte offset to a character boundary of the document's current encoding
//! before it reaches `try_insert_text_at_encoded`, `try_replace_range`, or
//! the post-filter for regex-match endpoints.
//!
//! The alignment strategy is dispatched by encoding family:
//!
//! - **UTF-8** delegates to the existing `align_utf8_boundary_backward` /
//!   `align_utf8_boundary_forward` helpers in `src/document.rs`. Backward
//!   walks back to the nearest UTF-8 char boundary; forward walks forward
//!   to the next one.
//! - **Class A** (single-byte ASCII supersets driven by
//!   [`super::encoding_engine::SingleByteEngine`]) is a no-op: every byte
//!   position is already a character boundary, so the function clamps to
//!   the document length and returns the offset unchanged.
//! - **UTF-16 LE / BE** reduces to a 2-byte alignment. Backward rounds
//!   down (`offset & !1`); forward rounds up (`(offset + 1) & !1`). Both
//!   are clamped to the document length so callers can never cross EOF.
//! - **Class B** (variable-length CJK encodings driven by
//!   [`super::encoding_engine::multibyte::MultiByteEngine`]) uses
//!   scan-from-anchor: the engine's `step` walks forward from the nearest
//!   line-start (or document start) until it lands on or steps past
//!   `offset`. The previous step-boundary is the backward answer; the
//!   next step's start is the forward answer.
//!
//! This is the surface that prevents reverse search and multibyte edits
//! from landing on the trail byte of a multi-byte character at an
//! internal piece boundary.

// Some alignment surfaces are reachable only from unit tests until the
// last edit / regex caller is wired in. Keep clippy quiet under
// `-D warnings`.
#![allow(dead_code)]

use super::encoding_engine::{EncodingEngine, SingleByteEngine};
use super::{align_utf8_boundary_backward, align_utf8_boundary_forward, Document};

/// Direction in which a byte offset is rounded to the nearest character
/// boundary of the document's current encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AlignDirection {
    /// Round down: return the largest character-boundary offset
    /// `<= offset`.
    Backward,
    /// Round up: return the smallest character-boundary offset
    /// `>= offset`.
    Forward,
}

impl Document {
    /// Aligns a raw byte offset onto a character boundary of the document's
    /// current encoding.
    ///
    /// `offset` is first clamped to the document length, so callers may
    /// pass the raw output of regex / piece-tree arithmetic without having
    /// to pre-clamp. The returned value is always within
    /// `[0, self.file_len()]`.
    ///
    /// Dispatch by encoding family:
    ///
    /// - UTF-8 reuses the existing `align_utf8_boundary_*` walkers.
    /// - Class A (single-byte ASCII supersets) is a no-op clamp because
    ///   every byte already sits on a character boundary.
    /// - UTF-16 LE / BE rounds to a 2-byte cell: `offset & !1` for
    ///   `Backward`, `(offset + 1) & !1` for `Forward`. The 2-byte
    ///   alignment matches the chunk-window post-filter contract used
    ///   by UTF-16 regex search.
    /// - Class B (variable-length CJK) walks forward from the nearest
    ///   line anchor (or document start) via the engine's `step` until
    ///   the cursor reaches or passes `offset`. If the cursor lands on
    ///   `offset`, that offset is already aligned; otherwise the
    ///   previous cursor is the `Backward` answer and the cursor that
    ///   stepped past `offset` is the `Forward` answer.
    ///
    /// Bytes for the alignment scan come from whichever backing the
    /// document currently holds (mmap, piece-tree, or rope). The function
    /// is `pub(crate)` because it is an internal building block for the
    /// edit / regex paths — it is not part of the stable public surface.
    pub(crate) fn align_byte_offset(&self, offset: usize, dir: AlignDirection) -> usize {
        let file_len = self.file_len();
        let offset = offset.min(file_len);
        if offset == 0 || offset == file_len {
            return offset;
        }

        let encoding = self.encoding();
        let name = encoding.name();

        // UTF-8: delegate to the existing UTF-8 boundary walkers. They
        // consult `is_char_boundary` on the decoded text, so they
        // already cover the multi-byte continuation rules.
        if encoding.is_utf8() {
            let bytes = self.bytes_for_alignment();
            return match dir {
                AlignDirection::Backward => align_utf8_boundary_backward(&bytes, offset),
                AlignDirection::Forward => align_utf8_boundary_forward(&bytes, offset),
            };
        }

        // Class A: every byte is a character boundary in any
        // single-byte ASCII superset, so the offset is returned as-is
        // (already clamped to `file_len` above). CRLF still splits as
        // two text units on the byte level; the caller is expected to
        // resolve that at the text-unit layer, not at the byte layer.
        if SingleByteEngine::supports(encoding) {
            return offset;
        }

        // UTF-16 LE / BE: round to a 2-byte aligned cell. Both endianness
        // markers share the same alignment contract: a valid character
        // boundary always sits on an even byte offset.
        if matches!(name, "UTF-16LE" | "UTF-16BE") {
            return match dir {
                AlignDirection::Backward => offset & !1usize,
                AlignDirection::Forward => ((offset + 1) & !1usize).min(file_len),
            };
        }

        // Class B: scan-from-anchor through the engine's step. Class B
        // is the catch-all for the remaining engines installed by
        // `engine_for_encoding` (Shift_JIS, gb18030, EUC-KR). UTF-8 /
        // Class A / UTF-16 have already been handled above, and the
        // unknown-encoding fallback in `engine_for_encoding` returns
        // `Utf8Engine`, which is also captured by the UTF-8 branch.
        let bytes = self.bytes_for_alignment();
        let bytes_len = bytes.len();
        let target = offset.min(bytes_len);
        if target == 0 || target == bytes_len {
            return target;
        }

        let engine = self.encoding_engine();
        align_class_b(
            engine,
            &bytes,
            &self.line_anchor_before(target, &bytes),
            target,
            dir,
        )
    }

    /// Reads the document's current bytes into an owned `Vec<u8>` for
    /// alignment scans.
    ///
    /// The alignment surface is rare-path (one call per edit / regex
    /// match endpoint), so the simplicity of an owned buffer outweighs
    /// the cost of avoiding the copy. mmap-only documents short-circuit
    /// through `mmap_bytes()` without copying via `Cow` — but the
    /// implementation deliberately keeps the return type owned so the
    /// piece-tree / rope branches do not need a different signature.
    pub(crate) fn bytes_for_alignment(&self) -> Vec<u8> {
        if let Some(piece_table) = &self.piece_table {
            return piece_table.read_range(0, piece_table.total_len());
        }
        if let Some(rope) = &self.rope {
            // UTF-8 rope — the rope holds canonical UTF-8 bytes regardless
            // of `self.encoding`, but `align_byte_offset` only consults
            // the rope branch on UTF-8 documents (Class A / UTF-16 / Class
            // B never build a rope), so handing back the rope
            // bytes here is safe for the UTF-8 branch.
            return rope.bytes().collect();
        }
        self.mmap_bytes().to_vec()
    }

    /// Finds a known character-boundary anchor at or before `target` for
    /// the Class B scan-from-anchor walk.
    ///
    /// The cheapest anchors are the bytes immediately after `\n` / `\r`:
    /// in every Class B encoding we support, line-ending bytes are
    /// 1-byte ASCII and cannot appear as a trail byte of a multi-byte
    /// sequence, so the byte right after them sits on a character
    /// boundary. The walk back is bounded by
    /// [`super::APPROX_LINE_BACKTRACK_BYTES`] (64 KiB) so the alignment
    /// stays O(line) in the worst case.
    fn line_anchor_before(&self, target: usize, bytes: &[u8]) -> usize {
        let scan_floor = target.saturating_sub(super::APPROX_LINE_BACKTRACK_BYTES);
        let window = &bytes[scan_floor..target];
        match window.iter().rposition(|b| matches!(*b, b'\n' | b'\r')) {
            Some(rel) => {
                let idx = scan_floor + rel;
                if bytes[idx] == b'\r' && idx + 1 < target && bytes[idx + 1] == b'\n' {
                    idx + 2
                } else {
                    idx + 1
                }
            }
            // Deg-fallback: if no line break is reachable inside the
            // window, fall back to `scan_floor`. The forward walk may
            // overshoot `target` when the heuristic anchor is not on a
            // character boundary; the alignment then collapses to the
            // closest reachable boundary as documented in
            // `align_class_b`.
            None => scan_floor,
        }
    }
}

/// Class B scan-from-anchor alignment.
///
/// Walks the engine's [`EncodingEngine::step`] forward from `anchor` until
/// the cursor lands on or moves past `target`:
///
/// - If the cursor lands exactly on `target`, that offset is already on a
///   character boundary and is returned as both the backward and forward
///   answer.
/// - Otherwise the cursor that stepped past `target` is the smallest
///   boundary `>= target` (the `Forward` answer), and the cursor's
///   previous position is the largest boundary `<= target` (the
///   `Backward` answer).
///
/// `step` returning `0` before reaching `target` means the slice is
/// truncated mid-character; in that case the function returns the cursor
/// (it is the closest reachable boundary).
fn align_class_b(
    engine: &dyn EncodingEngine,
    bytes: &[u8],
    anchor: &usize,
    target: usize,
    dir: AlignDirection,
) -> usize {
    let bytes_len = bytes.len();
    let target = target.min(bytes_len);
    let mut cursor = (*anchor).min(target);

    while cursor < target {
        let step = engine.step(bytes, cursor, bytes_len);
        if step == 0 {
            // Mid-character truncation; closest reachable boundary is
            // the current cursor.
            return cursor;
        }
        let next = cursor.saturating_add(step).min(bytes_len);
        if next == target {
            return target;
        }
        if next > target {
            return match dir {
                // Largest boundary <= target.
                AlignDirection::Backward => cursor,
                // Smallest boundary >= target. Clamp to file length so
                // callers never cross EOF.
                AlignDirection::Forward => next.min(bytes_len),
            };
        }
        cursor = next;
    }

    // Cursor reached `target` exactly through a sequence of steps —
    // already aligned regardless of direction.
    cursor
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::DocumentEncoding;
    use std::io::Write;
    use tempfile::tempdir;

    fn write_fixture(name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let dir = tempdir().expect("tempdir");
        // Keep the directory alive for the duration of the test by
        // leaking the handle: tests are short-lived processes and the
        // OS reclaims the temp directory anyway.
        let dir = Box::leak(Box::new(dir));
        let path = dir.path().join(name);
        let mut file = std::fs::File::create(&path).expect("create fixture");
        file.write_all(bytes).expect("write fixture");
        path
    }

    fn open_doc(bytes: &[u8], encoding: DocumentEncoding, name: &str) -> Document {
        let path = write_fixture(name, bytes);
        Document::open_with_encoding(path, encoding).expect("open fixture")
    }

    // ---------------------------------------------------------------
    // UTF-8: align via existing align_utf8_boundary_* helpers.
    // ---------------------------------------------------------------

    #[test]
    fn align_utf8_backward_walks_to_char_boundary() {
        // Cyrillic "Привет" — every letter takes 2 bytes in UTF-8.
        // Offset 3 lands on a continuation byte (mid-character).
        let bytes = "Привет".as_bytes();
        let doc = open_doc(bytes, DocumentEncoding::utf8(), "utf8_back.txt");

        let aligned = doc.align_byte_offset(3, AlignDirection::Backward);
        assert_eq!(aligned, 2, "backward must land on the start of П's pair");
    }

    #[test]
    fn align_utf8_forward_walks_to_next_char_boundary() {
        let bytes = "Привет".as_bytes();
        let doc = open_doc(bytes, DocumentEncoding::utf8(), "utf8_forward.txt");

        let aligned = doc.align_byte_offset(3, AlignDirection::Forward);
        assert_eq!(aligned, 4, "forward must land at the end of П's pair");
    }

    #[test]
    fn align_utf8_clamps_to_file_len() {
        let bytes = b"hello\n";
        let doc = open_doc(bytes, DocumentEncoding::utf8(), "utf8_clamp.txt");

        let aligned = doc.align_byte_offset(usize::MAX, AlignDirection::Backward);
        assert_eq!(aligned, doc.file_len());
    }

    // ---------------------------------------------------------------
    // Class A: every byte is already a character boundary.
    // ---------------------------------------------------------------

    #[test]
    fn align_class_a_is_noop_clamp() {
        // Windows-1251 fixture: ASCII + a single high byte for "Я".
        let bytes = b"abc\xDF\n"; // 0xDF == "Я" in windows-1251
        let encoding = DocumentEncoding::from_label("windows-1251").expect("known");
        let doc = open_doc(bytes, encoding, "win1251_class_a.txt");

        for offset in [0usize, 1, 2, 3, 4, 5] {
            assert_eq!(
                doc.align_byte_offset(offset, AlignDirection::Backward),
                offset.min(doc.file_len()),
                "Class A backward must be a no-op clamp at offset {offset}",
            );
            assert_eq!(
                doc.align_byte_offset(offset, AlignDirection::Forward),
                offset.min(doc.file_len()),
                "Class A forward must be a no-op clamp at offset {offset}",
            );
        }
    }

    // ---------------------------------------------------------------
    // UTF-16: round to a 2-byte aligned cell.
    // ---------------------------------------------------------------

    #[test]
    fn align_utf16_le_rounds_to_even_byte() {
        // "AB\n" in UTF-16LE: 41 00 42 00 0A 00 (6 bytes total).
        let bytes: Vec<u8> = "AB\n"
            .encode_utf16()
            .flat_map(|u| u.to_le_bytes())
            .collect();
        let doc = open_doc(&bytes, DocumentEncoding::utf16le(), "utf16_le.txt");

        // Odd offset: backward rounds down, forward rounds up.
        assert_eq!(doc.align_byte_offset(3, AlignDirection::Backward), 2);
        assert_eq!(doc.align_byte_offset(3, AlignDirection::Forward), 4);
        // Even offset stays put in both directions.
        assert_eq!(doc.align_byte_offset(2, AlignDirection::Backward), 2);
        assert_eq!(doc.align_byte_offset(2, AlignDirection::Forward), 2);
    }

    #[test]
    fn align_utf16_be_rounds_to_even_byte() {
        let bytes: Vec<u8> = "AB\n"
            .encode_utf16()
            .flat_map(|u| u.to_be_bytes())
            .collect();
        let doc = open_doc(&bytes, DocumentEncoding::utf16be(), "utf16_be.txt");

        assert_eq!(doc.align_byte_offset(1, AlignDirection::Backward), 0);
        assert_eq!(doc.align_byte_offset(1, AlignDirection::Forward), 2);
        assert_eq!(doc.align_byte_offset(4, AlignDirection::Forward), 4);
    }

    #[test]
    fn align_utf16_forward_clamps_to_file_len() {
        // 4-byte UTF-16LE document: forward alignment of an offset past
        // the last byte must clamp to file_len, never report 6.
        let bytes: Vec<u8> = "A\n".encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
        let doc = open_doc(&bytes, DocumentEncoding::utf16le(), "utf16_clamp.txt");

        let file_len = doc.file_len();
        assert_eq!(file_len, 4);
        assert_eq!(
            doc.align_byte_offset(file_len, AlignDirection::Forward),
            file_len,
        );
    }

    // ---------------------------------------------------------------
    // Class B: scan-from-anchor over MultiByteEngine::step.
    // ---------------------------------------------------------------

    #[test]
    fn align_class_b_shift_jis_backward_walks_to_char_start() {
        // Two Shift_JIS 2-byte sequences (Hiragana あ = 0x82 0xA0 each)
        // followed by ASCII LF: bytes are [82 A0 82 A0 0A].
        // Offset 1 sits inside the trailing byte of the first character;
        // backward must land on offset 0 (the start of that character).
        let bytes = [0x82u8, 0xA0, 0x82, 0xA0, 0x0A];
        let encoding = DocumentEncoding::from_label("Shift_JIS").expect("known");
        let doc = open_doc(&bytes, encoding, "shift_jis_back.txt");

        assert_eq!(doc.align_byte_offset(1, AlignDirection::Backward), 0);
        // Offset 3 is inside the trailing byte of the second character;
        // backward must land on offset 2.
        assert_eq!(doc.align_byte_offset(3, AlignDirection::Backward), 2);
    }

    #[test]
    fn align_class_b_shift_jis_forward_walks_to_next_char_start() {
        let bytes = [0x82u8, 0xA0, 0x82, 0xA0, 0x0A];
        let encoding = DocumentEncoding::from_label("Shift_JIS").expect("known");
        let doc = open_doc(&bytes, encoding, "shift_jis_forward.txt");

        // Offset 1 → next boundary is offset 2.
        assert_eq!(doc.align_byte_offset(1, AlignDirection::Forward), 2);
        // Offset 3 → next boundary is offset 4 (start of the LF).
        assert_eq!(doc.align_byte_offset(3, AlignDirection::Forward), 4);
        // Offset 4 (start of LF) is already aligned.
        assert_eq!(doc.align_byte_offset(4, AlignDirection::Forward), 4);
    }

    #[test]
    fn align_class_b_class_a_byte_already_aligned() {
        // Class B fixture but the offset lands on an ASCII boundary — the
        // walk must recognise that and return the offset unchanged in
        // either direction.
        let bytes = [b'A', 0x82u8, 0xA0, b'\n'];
        let encoding = DocumentEncoding::from_label("Shift_JIS").expect("known");
        let doc = open_doc(&bytes, encoding, "shift_jis_aligned.txt");

        assert_eq!(doc.align_byte_offset(1, AlignDirection::Backward), 1);
        assert_eq!(doc.align_byte_offset(1, AlignDirection::Forward), 1);
        assert_eq!(doc.align_byte_offset(3, AlignDirection::Backward), 3);
        assert_eq!(doc.align_byte_offset(3, AlignDirection::Forward), 3);
    }
}
