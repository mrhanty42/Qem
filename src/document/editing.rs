use super::*;

#[derive(Debug)]
enum EditBufferPlan {
    KeepCurrent,
    CreatePieceTable {
        storage: FileStorage,
        line_lengths: Vec<usize>,
        full_index: bool,
    },
    /// Seed the piece table from disk-index checkpoints: the pieces carry
    /// exact break counts for everything below `known_byte_len`, so edits
    /// work at the target line without materializing per-line lengths for
    /// the whole span.
    CreatePieceTableWindowed {
        storage: FileStorage,
        pieces: Vec<Piece>,
        known_byte_len: usize,
        known_line_count: usize,
        exact_total_lines: Option<usize>,
    },
    CreateRope,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ActiveEditPlan {
    Ready,
    PromotePieceTableToRope,
}

/// Reported when an edit lands beyond the bounded piece-table window of a
/// storage-backed document that can never be materialized into a rope.
pub(super) static EDIT_WINDOW_LIMIT_REASON: LazyLock<String> = LazyLock::new(|| {
    format!(
        "editing is limited to the first {LINE_LENGTHS_MAX_SYNC_LINES} lines of this file (bounded edit window)"
    )
});

pub(super) const EDIT_WINDOW_LIMIT_LINES: usize = LINE_LENGTHS_MAX_SYNC_LINES;

pub(super) fn edit_window_limit_reason() -> &'static str {
    &EDIT_WINDOW_LIMIT_REASON
}

/// Reported when a far-line edit cannot be resolved because the disk line
/// index is missing or cannot cover the target line.
pub(super) static EDIT_SPAN_UNRESOLVABLE_REASON: LazyLock<String> = LazyLock::new(|| {
    "this line cannot be resolved for editing: the on-disk line index is missing or does not cover it yet".to_owned()
});

pub(super) fn edit_span_unresolvable_reason() -> &'static str {
    &EDIT_SPAN_UNRESOLVABLE_REASON
}

impl Document {
    pub(super) fn precise_piece_table_line_lengths(
        &self,
        indexed_complete: bool,
    ) -> Option<Vec<usize>> {
        if !indexed_complete {
            return None;
        }

        let Ok(guard) = self.line_offsets.try_read() else {
            return None;
        };
        if guard.len() > LINE_LENGTHS_MAX_SYNC_LINES {
            return None;
        }

        Some(line_lengths_from_offsets(&guard, self.file_len))
    }

    pub(super) fn piece_table_line_lengths_for_edit(
        &self,
        line0: usize,
    ) -> Option<(Vec<usize>, bool)> {
        let indexed_complete = self.indexed_bytes() >= self.file_len;
        if let Some(line_lengths) = self.precise_piece_table_line_lengths(indexed_complete) {
            return Some((line_lengths, true));
        }

        let storage = self.storage.as_ref()?;
        if !indexed_complete && self.file_len <= FULL_SYNC_PIECE_TABLE_MAX_FILE_BYTES {
            if let Some(line_lengths) =
                line_lengths_from_bytes(storage.bytes(), LINE_LENGTHS_MAX_SYNC_LINES)
            {
                return Some((line_lengths, true));
            }
        }
        // A window reaching beyond the sync budget can never be seeded; bail
        // before materializing a doomed prefix vector.
        if line0 >= LINE_LENGTHS_MAX_SYNC_LINES {
            return None;
        }
        let required_lines = line0
            .saturating_add(1)
            .max(PARTIAL_PIECE_TABLE_TARGET_LINES);
        let guard = self.line_offsets.read().ok()?;

        let mut line_lengths = prefix_line_lengths_from_offsets(&guard, required_lines);
        if line_lengths.len() < required_lines {
            let scan_start = guard.get_usize(line_lengths.len()).unwrap_or(0);
            let scanned = scan_line_lengths_from(
                storage.bytes(),
                scan_start,
                required_lines.saturating_sub(line_lengths.len()),
                PARTIAL_PIECE_TABLE_SCAN_BYTES,
            );
            line_lengths.extend(scanned);
        }

        if line_lengths.len() <= line0 {
            return None;
        }

        Some((line_lengths, false))
    }

    fn edit_buffer_plan_for_line(&self, line0: usize) -> EditBufferPlan {
        if self.rope.is_some() || self.piece_table.is_some() {
            return EditBufferPlan::KeepCurrent;
        }

        // Promote non-UTF-8 storage-backed documents to the
        // piece-tree edit buffer so the new `try_insert_text_at_encoded`
        // path can append target-encoding bytes verbatim into
        // `add` without ever transcoding the document into UTF-8.
        // UTF-8 documents keep the existing size gate so small
        // files continue to use the cheaper rope edit buffer; non-UTF-8
        // documents drop the size gate because the rope-decode bridge
        // is no longer the default fallback for them.
        let use_piece_table = if self.encoding.is_utf8() {
            self.storage.is_some() && self.file_len >= PIECE_TABLE_MIN_BYTES
        } else {
            self.storage.is_some()
        };
        if use_piece_table {
            if let Some((line_lengths, full_index)) = self.piece_table_line_lengths_for_edit(line0)
            {
                if let Some(storage) = self.storage.as_ref().cloned() {
                    return EditBufferPlan::CreatePieceTable {
                        storage,
                        line_lengths,
                        full_index,
                    };
                }
            }
            // The target sits beyond every in-memory anchor. Seed the known
            // region from disk-index checkpoints instead: the piece tree gets
            // exact break counts down to the target without materializing a
            // per-line vector for the span.
            if let Some(plan) = self.windowed_piece_table_plan(line0) {
                return plan;
            }
        }

        EditBufferPlan::CreateRope
    }

    /// Builds the checkpoint-window piece-table plan for a first edit at
    /// `line0` when no in-memory anchor reaches that far. Returns `None`
    /// when there is no disk index (or it does not cover the line), leaving
    /// the caller to report its own error.
    fn windowed_piece_table_plan(&self, line0: usize) -> Option<EditBufferPlan> {
        if line0 == 0 {
            return None;
        }
        let disk = self.disk_index.as_ref()?;
        let storage = self.storage.as_ref().cloned()?;
        if let Some(total_lines) = disk.total_lines() {
            if line0 >= total_lines {
                return None;
            }
        }
        let engine = self.encoding_engine();
        let bytes = storage.read_range(0, storage.len());
        let file_len = bytes.len();
        let (mut pieces, span_breaks, window_end_orig) =
            checkpoint_pieces_up_to(bytes, file_len, engine, disk, 0, 0, line0)?;
        if window_end_orig < file_len {
            pieces.push(Piece {
                src: PieceSource::Original,
                start: window_end_orig,
                len: file_len - window_end_orig,
                line_breaks: 0,
            });
        }
        Some(EditBufferPlan::CreatePieceTableWindowed {
            storage,
            pieces,
            known_byte_len: window_end_orig,
            known_line_count: span_breaks,
            exact_total_lines: disk.total_lines(),
        })
    }

    /// Extends the existing piece table's annotated region so `line0` becomes
    /// navigable, using the disk line index. A no-op when the table already
    /// reaches the line or when the document is rope-backed.
    fn annotate_piece_table_line(&mut self, line0: usize) -> Result<(), DocumentError> {
        if self
            .piece_table
            .as_ref()
            .is_none_or(|piece_table| piece_table.has_line(line0))
        {
            return Ok(());
        }
        let Some(disk) = self.disk_index.as_ref() else {
            return Err(edit_span_unresolvable_error());
        };
        let engine = self.encoding_engine();
        let flush = {
            let piece_table = self
                .piece_table
                .as_mut()
                .expect("piece table checked above");
            piece_table.splice_annotated_pieces(engine, disk, line0)?
        };
        if flush > 0 {
            // Persist the annotated pieces so crash recovery keeps them.
            let path = self.path.clone();
            if let Some(piece_table) = self.piece_table.as_mut() {
                let sidecar = session_sidecar_path(path.as_deref(), piece_table.original.path());
                piece_table
                    .schedule_session_flush()
                    .map_err(|source| DocumentError::Write {
                        path: sidecar,
                        source,
                    })?;
            }
        }
        Ok(())
    }

    fn active_edit_plan_for_line(&self, line0: usize) -> ActiveEditPlan {
        let piece_table_supports_line = self
            .piece_table
            .as_ref()
            .map(|piece_table| piece_table.full_index() || piece_table.has_line(line0))
            .unwrap_or(false);
        if self.piece_table.is_some() && !piece_table_supports_line {
            return ActiveEditPlan::PromotePieceTableToRope;
        }
        ActiveEditPlan::Ready
    }

    pub(super) fn ensure_edit_buffer_for_line(
        &mut self,
        line0: usize,
    ) -> Result<(), DocumentError> {
        match self.edit_buffer_plan_for_line(line0) {
            EditBufferPlan::KeepCurrent => {
                // A piece table already exists; extend its annotated region
                // when the edit targets a line the tree cannot navigate to
                // yet. When annotation is impossible but the table is still
                // small enough to promote to a rope, fall through to the
                // promotion path (the pre-window behavior).
                if let Err(err) = self.annotate_piece_table_line(line0) {
                    let promotable = self.piece_table.as_ref().is_some_and(|piece_table| {
                        self.can_materialize_rope(piece_table.total_len())
                    });
                    if !promotable {
                        return Err(err);
                    }
                }
                Ok(())
            }
            EditBufferPlan::CreatePieceTable {
                storage,
                line_lengths,
                full_index,
            } => {
                // Editing should stay responsive: stop the background indexer once
                // we switch to a mutable buffer.
                self.indexing.store(false, Ordering::Relaxed);
                self.piece_table = Some(PieceTable::new_with_encoding_state(
                    storage,
                    line_lengths,
                    full_index,
                    self.raw_disk_index_total_lines(),
                    self.encoding_origin,
                    self.decoding_had_errors,
                ));
                Ok(())
            }
            EditBufferPlan::CreatePieceTableWindowed {
                storage,
                pieces,
                known_byte_len,
                known_line_count,
                exact_total_lines,
            } => {
                self.indexing.store(false, Ordering::Relaxed);
                self.piece_table = Some(PieceTable::from_annotated_pieces(
                    storage,
                    pieces,
                    known_byte_len,
                    known_line_count,
                    exact_total_lines,
                    self.encoding_origin,
                    self.decoding_had_errors,
                ));
                Ok(())
            }
            EditBufferPlan::CreateRope => {
                // On huge mmap-backed files we must never fall back to a full Rope
                // materialization unless policy explicitly allows it.
                self.indexing.store(false, Ordering::Relaxed);
                if self.storage.is_some() && !self.can_materialize_rope(self.file_len) {
                    let reason: &'static str = if line0 >= LINE_LENGTHS_MAX_SYNC_LINES {
                        &EDIT_WINDOW_LIMIT_REASON
                    } else {
                        "the edit buffer for this line could not be prepared"
                    };
                    return Err(self.edit_unsupported(reason));
                }
                self.ensure_rope()
            }
        }
    }

    pub(super) fn prepare_edit_at(&mut self, line0: usize) -> Result<(), DocumentError> {
        self.ensure_edit_buffer_for_line(line0)?;
        if self.active_edit_plan_for_line(line0) == ActiveEditPlan::PromotePieceTableToRope {
            self.promote_piece_table_to_rope()?;
        }
        Ok(())
    }

    pub(super) fn ensure_rope(&mut self) -> Result<(), DocumentError> {
        if self.rope.is_some() {
            return Ok(());
        }
        if !self.can_materialize_rope(self.file_len) {
            return Err(self.edit_unsupported(
                "document is too large to materialize into a rope; editing this region is disabled",
            ));
        }
        let (rope, had_errors) = if self.encoding.is_utf8() {
            let bytes = self.mmap_bytes();
            build_rope_from_bytes(bytes)
        } else {
            // Class A native open path leaves the document with `rope: None`
            // and `piece_table: None`, so the first edit lands here against
            // the raw mmap bytes. Decoding through `encoding_rs` mirrors the
            // rope-decoding branch in `from_storage_with_encoding`: it keeps
            // the rope text in canonical UTF-8 instead of treating the legacy
            // bytes as garbage UTF-8.
            let bytes = self.mmap_bytes();
            let (decoded, decoding_had_errors) = decode_text_with_encoding(bytes, self.encoding);
            (build_rope_from_decoded_text(&decoded), decoding_had_errors)
        };
        if had_errors && !self.decoding_had_errors {
            self.decoding_had_errors = true;
            self.invalidate_preserve_save_error_cache();
        }
        self.rope = Some(rope);
        Ok(())
    }

    pub(super) fn promote_piece_table_to_rope(&mut self) -> Result<(), DocumentError> {
        if self.rope.is_some() {
            return Ok(());
        }

        let Some(piece_table) = self.piece_table.take() else {
            return self.ensure_rope();
        };

        if !self.can_materialize_rope(piece_table.total_len()) {
            self.piece_table = Some(piece_table);
            return Err(self.edit_unsupported(
                "document is too large to widen partial piece-table editing beyond the indexed prefix",
            ));
        }
        let bytes = piece_table.read_range(0, piece_table.total_len());
        let (rope, had_errors) = if self.encoding.is_utf8() {
            build_rope_from_bytes(&bytes)
        } else {
            // Mirror `ensure_rope`: when the document is in a legacy encoding
            // the piece-tree bytes are still in that encoding, so they must
            // be decoded through `encoding_rs` before being placed into a
            // UTF-8 rope.
            let (decoded, decoding_had_errors) = decode_text_with_encoding(&bytes, self.encoding);
            (build_rope_from_decoded_text(&decoded), decoding_had_errors)
        };
        if had_errors && !self.decoding_had_errors {
            self.decoding_had_errors = true;
            self.invalidate_preserve_save_error_cache();
        }
        self.rope = Some(rope);
        Ok(())
    }

    pub(super) fn rope_mut(&mut self) -> Result<&mut EditBuffer, DocumentError> {
        let path = self.path.clone();
        self.ensure_rope()?;
        self.mark_dirty();
        let Some(rope) = self.rope.as_mut() else {
            return Err(DocumentError::EditUnsupported {
                path,
                reason: "internal error: rope buffer is unavailable after materialization",
            });
        };
        Ok(rope)
    }

    pub(super) fn rope_line_len_chars_without_newline(rope: &EditBuffer, line0: usize) -> usize {
        let line0 = line0.min(rope.len_lines().saturating_sub(1));
        // The edit buffer is LF-normalized and `line_char_range` already
        // excludes the break, which is exactly what the previous rope
        // slice length minus its trailing-`\n` check produced.
        let range = rope.line_char_range(line0);
        range.end - range.start
    }

    pub(super) fn rope_replace_noop_cursor(
        rope: &EditBuffer,
        line0: usize,
        col0: usize,
        len_chars: usize,
        text: &str,
    ) -> Option<(usize, usize)> {
        let actual_col0 = Self::rope_line_len_chars_without_newline(rope, line0);
        let start_col0 = col0.min(actual_col0);
        let start = Self::line_col_to_char_index(rope, line0, start_col0);
        let end = start.saturating_add(len_chars).min(rope.len_chars());
        let (normalized, added_lines, last_col) = normalize_insert_text(text, 0, LineEnding::Lf);
        let cursor = if added_lines == 0 {
            (line0, start_col0.saturating_add(last_col))
        } else {
            (line0.saturating_add(added_lines), last_col)
        };

        let existing = rope.slice_to_string(start..end);
        (existing == normalized).then_some(cursor)
    }
}
