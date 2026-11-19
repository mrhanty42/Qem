//! Initial piece-tree construction and checkpoint-annotated piece windows
//! backing the partial piece table.

use super::*;

impl PieceTable {
    pub(super) fn build_initial_piece_tree(
        original: &FileStorage,
        total_len: usize,
        line_lengths: &[usize],
        known_byte_len: usize,
        full_index: bool,
    ) -> PieceTree {
        if total_len == 0 {
            return PieceTree::new();
        }

        let mut pieces = Vec::new();
        let mut start = 0usize;
        let mut chunk_len = 0usize;
        let mut chunk_breaks = 0usize;
        let mut chunk_lines = 0usize;
        let known_line_count = line_lengths.len().max(1);
        let flush_chunk = |pieces: &mut Vec<Piece>,
                           start: &mut usize,
                           chunk_len: &mut usize,
                           chunk_breaks: &mut usize,
                           chunk_lines: &mut usize| {
            if *chunk_len == 0 {
                return;
            }
            pieces.push(Piece {
                src: PieceSource::Original,
                start: *start,
                len: *chunk_len,
                line_breaks: *chunk_breaks,
            });
            *start = start.saturating_add(*chunk_len);
            *chunk_len = 0;
            *chunk_breaks = 0;
            *chunk_lines = 0;
        };

        for (idx, len) in line_lengths.iter().copied().enumerate() {
            // A partial prefix ends at a complete line, so every prefix line's
            // newline lies inside the known bytes and must be attributed. A
            // full index ends with the virtual trailing line, which owns no
            // break.
            let line_has_break = if full_index {
                idx + 1 < known_line_count
            } else {
                true
            };
            if line_has_break && len > PIECE_TREE_TARGET_BYTES {
                flush_chunk(
                    &mut pieces,
                    &mut start,
                    &mut chunk_len,
                    &mut chunk_breaks,
                    &mut chunk_lines,
                );

                let tail_len = PIECE_TREE_TARGET_BYTES.min(len);
                let body_len = len.saturating_sub(tail_len);
                if body_len > 0 {
                    pieces.push(Piece {
                        src: PieceSource::Original,
                        start,
                        len: body_len,
                        line_breaks: 0,
                    });
                    start = start.saturating_add(body_len);
                }
                pieces.push(Piece {
                    src: PieceSource::Original,
                    start,
                    len: tail_len,
                    line_breaks: 1,
                });
                start = start.saturating_add(tail_len);
                continue;
            }

            chunk_len = chunk_len.saturating_add(len);
            if line_has_break {
                chunk_breaks = chunk_breaks.saturating_add(1);
            }
            chunk_lines = chunk_lines.saturating_add(1);

            let should_flush =
                chunk_len >= PIECE_TREE_TARGET_BYTES || chunk_lines >= PIECE_TREE_TARGET_LINES;
            if should_flush {
                flush_chunk(
                    &mut pieces,
                    &mut start,
                    &mut chunk_len,
                    &mut chunk_breaks,
                    &mut chunk_lines,
                );
            }
        }

        flush_chunk(
            &mut pieces,
            &mut start,
            &mut chunk_len,
            &mut chunk_breaks,
            &mut chunk_lines,
        );

        if known_byte_len < total_len {
            pieces.push(Piece {
                src: PieceSource::Original,
                start: known_byte_len,
                len: total_len - known_byte_len,
                line_breaks: 0,
            });
        }

        if pieces.is_empty() {
            pieces.push(Piece {
                src: PieceSource::Original,
                start: 0,
                len: total_len,
                line_breaks: 0,
            });
        }

        if total_len >= PIECE_TREE_DISK_MIN_BYTES {
            if let Ok(tree) = PieceTree::from_pieces_disk(original.path(), pieces.clone()) {
                return tree;
            }
        }

        PieceTree::from_pieces(pieces)
    }
}

/// Builds checkpoint-aligned original pieces covering the document span from
/// `from_byte` (whose containing original line is `from_line0`) until the
/// checkpoint interval containing `target_orig_line` has been split at that
/// line's exact start byte. Break counts come from the disk index, so the
/// only byte scanning is the final ≤8192-line walk inside the target's
/// interval. Returns the pieces, the line breaks they carry, and the byte
/// offset that ends the annotated region (the end of the target interval).
///
/// Returns `None` when the disk index cannot resolve the span (missing
/// checkpoints for the region, target beyond the indexed lines, or a
/// malformed walk).
pub(super) fn checkpoint_pieces_up_to(
    bytes: &[u8],
    file_len: usize,
    engine: &dyn encoding_engine::EncodingEngine,
    disk: &crate::index::DiskLineIndex,
    from_byte: usize,
    from_line0: usize,
    target_orig_line: usize,
) -> Option<(Vec<Piece>, usize, usize)> {
    if file_len == 0 || from_byte >= file_len || target_orig_line < from_line0 {
        return None;
    }
    let stride = crate::index::INDEX_CHECKPOINT_STRIDE_LINES as usize;
    let mut pieces: Vec<Piece> = Vec::new();
    let mut span_breaks = 0usize;
    let mut prev_line = from_line0;
    let mut prev_byte = from_byte;
    let mut guard = 0usize;
    while prev_byte < file_len {
        guard += 1;
        if guard > 64 * 1024 * 1024 {
            return None;
        }
        // The next stored checkpoint after `prev_line`. Stored checkpoints
        // are at most `stride` lines apart, so querying `prev_line + stride`
        // always makes progress while any checkpoint remains; past the last
        // one the target sits within the final stride-sized stretch and the
        // interval end is the end of the file.
        let (cp_line, cp_byte) = match disk.checkpoint_for_line(prev_line + stride) {
            Some(cp) if cp.line0 > prev_line => (cp.line0, cp.byte0.min(file_len)),
            _ => {
                // No stored checkpoint covers the stretch up to the target: it
                // is within the final stride-sized stretch, so walk it and
                // attribute every line break exactly.
                let mut walk_line = prev_line;
                let mut walk_byte = prev_byte;
                while walk_line < target_orig_line && walk_byte < file_len {
                    let next = engine.next_line_start(bytes, file_len, walk_byte);
                    if next <= walk_byte {
                        return None;
                    }
                    walk_byte = next;
                    walk_line += 1;
                }
                if walk_line != target_orig_line {
                    return None;
                }
                if walk_byte > prev_byte {
                    pieces.push(Piece {
                        src: PieceSource::Original,
                        start: prev_byte,
                        len: walk_byte - prev_byte,
                        line_breaks: walk_line - prev_line,
                    });
                    span_breaks += walk_line - prev_line;
                }
                // Cover the target line itself through its terminator so the
                // annotated region includes its content and its break.
                if walk_byte < file_len {
                    let line_end = engine.next_line_start(bytes, file_len, walk_byte);
                    if line_end > walk_byte {
                        pieces.push(Piece {
                            src: PieceSource::Original,
                            start: walk_byte,
                            len: line_end - walk_byte,
                            line_breaks: 1,
                        });
                        span_breaks += 1;
                        walk_byte = line_end;
                    }
                }
                return Some((pieces, span_breaks, walk_byte));
            }
        };
        if cp_line < target_orig_line {
            pieces.push(Piece {
                src: PieceSource::Original,
                start: prev_byte,
                len: cp_byte.saturating_sub(prev_byte),
                line_breaks: cp_line - prev_line,
            });
            span_breaks += cp_line - prev_line;
            prev_line = cp_line;
            prev_byte = cp_byte;
            continue;
        }
        // The interval containing the target: split it at the exact start
        // byte of `target_orig_line`.
        let mut split_byte = prev_byte;
        let mut split_line = prev_line;
        while split_line < target_orig_line && split_byte < file_len {
            let next = engine.next_line_start(bytes, file_len, split_byte);
            if next <= split_byte {
                return None;
            }
            split_byte = next;
            split_line += 1;
        }
        if split_line != target_orig_line {
            return None;
        }
        // The target line itself must land inside the annotated region with
        // its terminator, so its content and break are fully known.
        let mut target_end = split_byte;
        if target_end < file_len {
            let line_end = engine.next_line_start(bytes, file_len, target_end);
            if line_end > target_end {
                target_end = line_end;
            }
        }
        let left_breaks = target_orig_line - prev_line;
        let target_breaks = usize::from(target_end > split_byte);
        let right_breaks = cp_line.saturating_sub(target_orig_line + 1);
        if split_byte > prev_byte {
            pieces.push(Piece {
                src: PieceSource::Original,
                start: prev_byte,
                len: split_byte - prev_byte,
                line_breaks: left_breaks,
            });
        }
        if target_end > split_byte {
            pieces.push(Piece {
                src: PieceSource::Original,
                start: split_byte,
                len: target_end - split_byte,
                line_breaks: target_breaks,
            });
        }
        if cp_byte > target_end {
            pieces.push(Piece {
                src: PieceSource::Original,
                start: target_end,
                len: cp_byte - target_end,
                line_breaks: right_breaks,
            });
        }
        span_breaks += left_breaks + target_breaks + right_breaks;
        return Some((pieces, span_breaks, cp_byte));
    }
    None
}
