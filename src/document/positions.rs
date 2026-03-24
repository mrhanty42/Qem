use super::*;

impl Document {
    /// Returns the line length in document text columns, excluding any trailing
    /// line ending.
    ///
    /// For UTF-8 text this counts Unicode scalar values rather than grapheme
    /// clusters or display cells.
    pub fn line_len_chars(&self, line0: usize) -> usize {
        if let Some(piece_table) = &self.piece_table {
            if piece_table.full_index() || piece_table.has_line(line0) {
                return piece_table.line_len_chars(line0);
            }
        }
        if let Some(rope) = &self.rope {
            return Self::rope_line_len_chars_without_newline(rope, line0);
        }

        let bytes = self.mmap_bytes();
        let exact_range = self
            .line_offsets
            .read()
            .ok()
            .and_then(|offsets| {
                let start0 = offsets.get_usize(line0)?;
                let end0 = offsets
                    .get_usize(line0 + 1)
                    .or_else(|| (self.indexed_bytes() >= self.file_len).then_some(self.file_len))?;
                Some((start0, end0))
            })
            .or_else(|| self.estimated_mmap_line_byte_range(line0));
        let Some((start0, end0)) = exact_range else {
            return 0;
        };
        let start = start0.min(bytes.len());
        let mut end = end0.min(bytes.len());
        while end > start {
            let b = bytes[end - 1];
            if b == b'\n' || b == b'\r' {
                end -= 1;
            } else {
                break;
            }
        }
        count_text_columns(&bytes[start..end], MAX_LINE_SCAN_CHARS)
    }

    /// Clamps a typed position into the currently known document bounds.
    pub fn clamp_position(&self, position: TextPosition) -> TextPosition {
        let total_lines = self.display_line_count().max(1);
        let line0 = position.line0().min(total_lines.saturating_sub(1));
        let col0 = position.col0().min(self.line_len_chars(line0));
        TextPosition::new(line0, col0)
    }

    /// Returns the ordered pair of two clamped positions.
    ///
    /// This is useful for frontend code that keeps anchor/head selection state
    /// and needs a stable document-ordered range before applying edits.
    pub fn ordered_positions(
        &self,
        first: TextPosition,
        second: TextPosition,
    ) -> (TextPosition, TextPosition) {
        let first = self.clamp_position(first);
        let second = self.clamp_position(second);
        if first <= second {
            (first, second)
        } else {
            (second, first)
        }
    }

    /// Clamps a selection into the currently known document bounds.
    pub fn clamp_selection(&self, selection: TextSelection) -> TextSelection {
        TextSelection::new(
            self.clamp_position(selection.anchor()),
            self.clamp_position(selection.head()),
        )
    }

    /// Returns the typed document position for a full-text character index.
    ///
    /// For UTF-8 text, this maps indices to Unicode scalar-value columns
    /// instead of grapheme clusters or display cells.
    pub fn position_for_char_index(&self, char_index: usize) -> TextPosition {
        let (line0, col0) = self.cursor_position_for_char_index(char_index);
        TextPosition::new(line0, col0)
    }

    /// Returns the full-text character index for a typed document position.
    ///
    /// This uses the same text-unit semantics as [`Document::try_replace`]:
    /// line-local columns count Unicode scalar values, and line breaks count as
    /// one text unit even when they are stored as CRLF.
    pub fn char_index_for_position(&self, position: TextPosition) -> usize {
        let position = self.clamp_position(position);
        if let Some(rope) = &self.rope {
            return Self::line_col_to_char_index(rope, position.line0(), position.col0());
        }
        self.text_units_between(TextPosition::new(0, 0), position)
    }

    /// Returns the number of edit text units between two typed positions.
    ///
    /// This follows the same semantics as [`Document::try_replace`]: line-local
    /// columns count Unicode scalar values, and line breaks count as one text
    /// unit even when they are stored as CRLF.
    pub fn text_units_between(&self, start: TextPosition, end: TextPosition) -> usize {
        let (start, end) = self.ordered_positions(start, end);
        if start == end {
            return 0;
        }

        if let Some(rope) = &self.rope {
            let start_idx = Self::line_col_to_char_index(rope, start.line0(), start.col0());
            let end_idx = Self::line_col_to_char_index(rope, end.line0(), end.col0());
            return end_idx.saturating_sub(start_idx);
        }

        if start.line0() == end.line0() {
            return end.col0().saturating_sub(start.col0());
        }

        let mut units = self
            .line_len_chars(start.line0())
            .saturating_sub(start.col0());
        for line0 in start.line0()..end.line0() {
            units = units.saturating_add(1);
            if line0 + 1 == end.line0() {
                break;
            }
            units = units.saturating_add(self.line_len_chars(line0 + 1));
        }
        units.saturating_add(end.col0())
    }

    /// Builds a typed edit range between two positions.
    ///
    /// Frontends that keep anchor/head selection state can use this helper to
    /// convert it directly into a [`TextRange`] for [`Document::try_replace`].
    pub fn text_range_between(&self, start: TextPosition, end: TextPosition) -> TextRange {
        let (start, end) = self.ordered_positions(start, end);
        TextRange::new(start, self.text_units_between(start, end))
    }

    /// Builds a typed edit range from an anchor/head selection.
    pub fn text_range_for_selection(&self, selection: TextSelection) -> TextRange {
        self.text_range_between(selection.anchor(), selection.head())
    }

    /// Returns whether the requested position is currently editable.
    ///
    /// This lets frontends distinguish between positions that are already
    /// editable, positions that would trigger a backend promotion, and positions
    /// that would fail with [`DocumentError::EditUnsupported`].
    pub fn edit_capability_at(&self, position: TextPosition) -> EditCapability {
        let position = self.clamp_position(position);
        let backing = self.backing();

        if self.rope.is_some() {
            return EditCapability::Editable { backing };
        }

        if let Some(piece_table) = &self.piece_table {
            if piece_table.full_index() || piece_table.has_line(position.line0()) {
                return EditCapability::Editable { backing };
            }

            return if self.can_materialize_rope(piece_table.total_len()) {
                EditCapability::RequiresPromotion {
                    from: DocumentBacking::PieceTable,
                    to: DocumentBacking::Rope,
                }
            } else {
                EditCapability::Unsupported {
                backing: DocumentBacking::PieceTable,
                reason: "document is too large to widen partial piece-table editing beyond the indexed prefix",
            }
            };
        }

        let use_piece_table = self.storage.is_some() && self.file_len >= PIECE_TABLE_MIN_BYTES;
        if use_piece_table
            && self
                .piece_table_line_lengths_for_edit(position.line0())
                .is_some()
        {
            return EditCapability::RequiresPromotion {
                from: DocumentBacking::Mmap,
                to: DocumentBacking::PieceTable,
            };
        }

        if self.can_materialize_rope(self.file_len) {
            return EditCapability::RequiresPromotion {
                from: backing,
                to: DocumentBacking::Rope,
            };
        }

        EditCapability::Unsupported {
            backing,
            reason:
                "document is too large to materialize into a rope; editing this region is disabled",
        }
    }

    /// Returns the editability for a typed edit range.
    pub fn edit_capability_for_range(&self, range: TextRange) -> EditCapability {
        self.edit_capability_at(range.start())
    }

    /// Returns the editability for an anchor/head selection.
    pub fn edit_capability_for_selection(&self, selection: TextSelection) -> EditCapability {
        let range = self.text_range_for_selection(selection);
        self.edit_capability_for_range(range)
    }

    pub(crate) fn cursor_position_for_char_index(&self, char_index: usize) -> (usize, usize) {
        if let Some(rope) = &self.rope {
            let char_index = char_index.min(rope.len_chars());
            let line0 = rope.char_to_line(char_index);
            let line_start = rope.line_to_char(line0);
            let line_len = Self::rope_line_len_chars_without_newline(rope, line0);
            let col0 = char_index.saturating_sub(line_start).min(line_len);
            return (line0, col0);
        }

        if let Some(piece_table) = &self.piece_table {
            return piece_table.position_for_char_index(char_index);
        }

        let mut state = CursorScanState::new(char_index);
        scan_cursor_position_bytes(self.mmap_bytes(), &mut state);
        state.position()
    }

    pub(super) fn line_col_to_char_index(rope: &Rope, line0: usize, col0: usize) -> usize {
        let line0 = line0.min(rope.len_lines().saturating_sub(1));
        let line_start = rope.line_to_char(line0);
        let line_len = Self::rope_line_len_chars_without_newline(rope, line0);
        line_start + col0.min(line_len)
    }
}
