//! Piece splitting and trimming helpers over the original + add-buffer
//! sources, with encoding-aligned split offsets.

use super::*;

/// Aligns a piece-split offset backward to a character boundary of the
/// document's encoding.
///
/// UTF-8 keeps the whole-slice UTF-8 boundary walk. Non-UTF-8 backings
/// must not run that walk at all: offsets reaching a piece split are
/// already cell-aligned by the encoded edit paths, and a UTF-8 *validity*
/// gate is not an encoding gate — a pure-ASCII UTF-16LE stream (or the
/// `\u{C241}\u{C280}` cell pair `41 C2 80 C2`) is also valid UTF-8, and
/// re-aligning by `is_char_boundary` would pull the split into the middle
/// of a code unit. UTF-16 keeps the 2-byte cell grid anchored at the
/// piece start (source offsets and document offsets share parity because
/// every piece is cell-aligned); Class A and CJK multibyte keep the
/// offset verbatim, exactly like the previous `from_utf8` rejection path.
fn align_piece_split_offset_backward(
    bytes: &[u8],
    source_start: usize,
    offset: usize,
    engine: &dyn encoding_engine::EncodingEngine,
) -> usize {
    let encoding = engine.encoding();
    if !encoding.is_utf8() {
        if matches!(encoding.name(), "UTF-16LE" | "UTF-16BE") {
            let mut aligned = offset.min(bytes.len());
            // Snap only a split that creates a right piece: a slice at the
            // very end of an odd-length piece (truncated file tail) has no
            // boundary to align, and snapping it backward would carve a
            // one-byte, mid-cell right piece - the exact invariant break
            // this grid exists to prevent. Mirrors the forward twin's
            // `.min(bytes.len())` clamp.
            if (aligned ^ source_start) & 1 == 1 && aligned > 0 && aligned < bytes.len() {
                aligned -= 1;
            }
            return aligned;
        }
        return offset.min(bytes.len());
    }
    align_utf8_boundary_backward(bytes, offset)
}

/// Forward twin of [`align_piece_split_offset_backward`].
fn align_piece_split_offset_forward(
    bytes: &[u8],
    source_start: usize,
    offset: usize,
    engine: &dyn encoding_engine::EncodingEngine,
) -> usize {
    let encoding = engine.encoding();
    if !encoding.is_utf8() {
        if matches!(encoding.name(), "UTF-16LE" | "UTF-16BE") {
            let mut aligned = offset.min(bytes.len());
            if (aligned ^ source_start) & 1 == 1 {
                aligned = aligned.saturating_add(1).min(bytes.len());
            }
            return aligned;
        }
        return offset.min(bytes.len());
    }
    align_utf8_boundary_forward(bytes, offset)
}

fn align_utf8_boundary_backward(bytes: &[u8], offset: usize) -> usize {
    let offset = offset.min(bytes.len());
    if offset == 0 || offset == bytes.len() {
        return offset;
    }

    let Ok(text) = std::str::from_utf8(bytes) else {
        return offset;
    };
    let mut aligned = offset;
    while aligned > 0 && !text.is_char_boundary(aligned) {
        aligned -= 1;
    }
    aligned
}

fn align_utf8_boundary_forward(bytes: &[u8], offset: usize) -> usize {
    let offset = offset.min(bytes.len());
    if offset == 0 || offset == bytes.len() {
        return offset;
    }

    let Ok(text) = std::str::from_utf8(bytes) else {
        return offset;
    };
    let mut aligned = offset;
    while aligned < bytes.len() && !text.is_char_boundary(aligned) {
        aligned += 1;
    }
    aligned
}

fn piece_source_bytes<'a>(original: &'a FileStorage, add: &'a [u8], src: PieceSource) -> &'a [u8] {
    match src {
        PieceSource::Original => original.read_range(0, original.len()),
        PieceSource::Add => add,
    }
}

fn count_piece_line_breaks_with_sources(
    original: &FileStorage,
    add: &[u8],
    piece: Piece,
    local_start: usize,
    local_end: usize,
    engine: &dyn encoding_engine::EncodingEngine,
) -> usize {
    let bytes = piece_source_bytes(original, add, piece.src);
    let start = piece.start.saturating_add(local_start).min(bytes.len());
    let end = piece
        .start
        .saturating_add(local_end)
        .min(bytes.len())
        .max(start);
    count_line_breaks_in_bytes(
        &bytes[start..end],
        utf16_break_cells(engine.encoding().name()),
    )
}

pub(super) fn split_piece_with_sources(
    original: &FileStorage,
    add: &[u8],
    piece: Piece,
    left_len: usize,
    engine: &dyn encoding_engine::EncodingEngine,
) -> (Option<Piece>, Option<Piece>) {
    let bytes = piece_source_bytes(original, add, piece.src);
    let start = piece.start.min(bytes.len());
    let end = piece.start.saturating_add(piece.len).min(bytes.len());
    let left_len = align_piece_split_offset_backward(
        &bytes[start..end],
        piece.start,
        left_len.min(piece.len),
        engine,
    );
    let right_len = piece.len.saturating_sub(left_len);
    let left = (left_len > 0).then_some(Piece {
        src: piece.src,
        start: piece.start,
        len: left_len,
        line_breaks: count_piece_line_breaks_with_sources(
            original, add, piece, 0, left_len, engine,
        ),
    });
    let right = (right_len > 0).then_some(Piece {
        src: piece.src,
        start: piece.start + left_len,
        len: right_len,
        line_breaks: count_piece_line_breaks_with_sources(
            original, add, piece, left_len, piece.len, engine,
        ),
    });
    (left, right)
}

pub(super) fn trim_piece_with_sources(
    original: &FileStorage,
    add: &[u8],
    piece: Piece,
    local_start: usize,
    local_end: usize,
    engine: &dyn encoding_engine::EncodingEngine,
) -> (Option<Piece>, Option<Piece>) {
    let bytes = piece_source_bytes(original, add, piece.src);
    let start = piece.start.min(bytes.len());
    let end = piece.start.saturating_add(piece.len).min(bytes.len());
    let piece_bytes = &bytes[start..end];
    let left_len = align_piece_split_offset_backward(
        piece_bytes,
        piece.start,
        local_start.min(piece.len),
        engine,
    );
    let right_start = align_piece_split_offset_forward(
        piece_bytes,
        piece.start,
        local_end.min(piece.len),
        engine,
    );
    let right_len = piece.len.saturating_sub(right_start);
    let left = (left_len > 0).then_some(Piece {
        src: piece.src,
        start: piece.start,
        len: left_len,
        line_breaks: count_piece_line_breaks_with_sources(
            original, add, piece, 0, left_len, engine,
        ),
    });
    let right = (right_len > 0).then_some(Piece {
        src: piece.src,
        start: piece.start + right_start,
        len: right_len,
        line_breaks: count_piece_line_breaks_with_sources(
            original,
            add,
            piece,
            right_start,
            piece.len,
            engine,
        ),
    });
    (left, right)
}
