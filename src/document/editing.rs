use super::*;

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
        let required_lines = line0
            .saturating_add(1)
            .clamp(
                PARTIAL_PIECE_TABLE_TARGET_LINES,
                PARTIAL_PIECE_TABLE_MAX_LINES,
            )
            .min(LINE_LENGTHS_MAX_SYNC_LINES);
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

    pub(super) fn ensure_edit_buffer_for_line(
        &mut self,
        line0: usize,
    ) -> Result<(), DocumentError> {
        if self.rope.is_some() || self.piece_table.is_some() {
            return Ok(());
        }
        // Editing should stay responsive: stop the background indexer once we switch to a mutable buffer.
        self.indexing.store(false, Ordering::Relaxed);
        let use_piece_table = self.storage.is_some() && self.file_len >= PIECE_TABLE_MIN_BYTES;
        if use_piece_table {
            if let Some((line_lengths, full_index)) = self.piece_table_line_lengths_for_edit(line0)
            {
                if let Some(storage) = self.storage.as_ref().cloned() {
                    self.piece_table = Some(PieceTable::new(storage, line_lengths, full_index));
                    return Ok(());
                }
            }
        }

        // On huge mmap-backed files we must never fall back to a full Rope materialization.
        self.ensure_rope()
    }

    pub(super) fn prepare_edit_at(&mut self, line0: usize) -> Result<(), DocumentError> {
        self.ensure_edit_buffer_for_line(line0)?;
        let piece_table_supports_line = self
            .piece_table
            .as_ref()
            .map(|piece_table| piece_table.full_index() || piece_table.has_line(line0))
            .unwrap_or(false);
        if self.piece_table.is_some() && !piece_table_supports_line {
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
        let bytes = self.mmap_bytes();
        self.rope = Some(build_rope_from_bytes(bytes));
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
        self.rope = Some(build_rope_from_bytes(&bytes));
        Ok(())
    }

    pub(super) fn rope_mut(&mut self) -> Result<&mut Rope, DocumentError> {
        let path = self.path.clone();
        self.ensure_rope()?;
        self.dirty = true;
        let Some(rope) = self.rope.as_mut() else {
            return Err(DocumentError::EditUnsupported {
                path,
                reason: "internal error: rope buffer is unavailable after materialization",
            });
        };
        Ok(rope)
    }

    pub(super) fn rope_line_len_chars_without_newline(rope: &Rope, line0: usize) -> usize {
        let line = rope.line(line0);
        let mut len = line.len_chars();
        if len > 0 && line.char(len - 1) == '\n' {
            len = len.saturating_sub(1);
        }
        len
    }

    pub(super) fn rope_replace_noop_cursor(
        rope: &Rope,
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

        let existing = rope.slice(start..end).to_string();
        (existing == normalized).then_some(cursor)
    }
}
