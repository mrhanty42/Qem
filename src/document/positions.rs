use super::*;

#[derive(Clone, Copy, Debug)]
struct PieceTableScanRange {
    range: (usize, usize),
    exact: bool,
}

fn next_piece_table_scan_line_range(
    bytes: &[u8],
    start0: usize,
    buffer_reaches_eof: bool,
) -> Option<PieceTableScanRange> {
    if start0 >= bytes.len() {
        return None;
    }

    let slice = &bytes[start0..];
    let end0 = if let Some(rel) = memchr::memchr2(b'\n', b'\r', slice) {
        let idx = start0 + rel;
        if bytes[idx] == b'\r' && idx + 1 < bytes.len() && bytes[idx + 1] == b'\n' {
            idx + 2
        } else {
            idx + 1
        }
    } else {
        bytes.len()
    };

    Some(PieceTableScanRange {
        range: (start0, end0.max(start0)),
        exact: end0 < bytes.len() || buffer_reaches_eof,
    })
}

fn count_text_units_in_bytes(bytes: &[u8]) -> usize {
    let mut units = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'\r' => {
                units = units.saturating_add(1);
                i += 1;
                if i < bytes.len() && bytes[i] == b'\n' {
                    i += 1;
                }
            }
            b'\n' => {
                units = units.saturating_add(1);
                i += 1;
            }
            _ => {
                units = units.saturating_add(1);
                i += utf8_step(bytes, i, bytes.len());
            }
        }
    }
    units
}

fn safe_piece_table_offset_for_position(piece_table: &PieceTable, position: TextPosition) -> usize {
    Document::scanned_piece_table_offset_for_position(piece_table, position)
        .map(|(offset, _)| offset)
        .unwrap_or_else(|| piece_table.known_byte_len.min(piece_table.total_len()))
}

impl Document {
    pub(crate) fn piece_table_position_is_representable(
        piece_table: &PieceTable,
        position: TextPosition,
    ) -> bool {
        if piece_table.full_index() || piece_table.has_line(position.line0()) {
            return true;
        }

        let Some(scanned) = Self::scanned_piece_table_line_range(piece_table, position.line0())
        else {
            return false;
        };
        let bytes = piece_table.read_range(scanned.range.0, scanned.range.1);
        let mut end = bytes.len();
        while end > 0 {
            let b = bytes[end - 1];
            if b == b'\n' || b == b'\r' {
                end -= 1;
            } else {
                break;
            }
        }

        let line_len = if scanned.exact {
            count_text_columns_exact(&bytes[..end])
        } else {
            count_text_columns(&bytes[..end], MAX_LINE_SCAN_CHARS)
        };
        position.col0() <= line_len
    }

    pub(super) fn selection_requires_piece_table_promotion(
        &self,
        selection: TextSelection,
    ) -> bool {
        let Some(piece_table) = &self.piece_table else {
            return false;
        };
        if piece_table.full_index() {
            return false;
        }

        !Self::piece_table_position_is_representable(piece_table, selection.anchor())
            || !Self::piece_table_position_is_representable(piece_table, selection.head())
    }

    fn scanned_piece_table_line_range(
        piece_table: &PieceTable,
        line0: usize,
    ) -> Option<PieceTableScanRange> {
        if piece_table.full_index() || piece_table.has_line(line0) {
            return Some(PieceTableScanRange {
                range: piece_table.line_range(line0),
                exact: true,
            });
        }

        let scan_start = piece_table.known_byte_len.min(piece_table.total_len());
        let scan_end = scan_start
            .saturating_add(PARTIAL_PIECE_TABLE_SCAN_BYTES)
            .min(piece_table.total_len());
        if scan_start >= scan_end {
            return None;
        }

        let bytes = piece_table.read_range(scan_start, scan_end);
        let buffer_reaches_eof = scan_end == piece_table.total_len();
        let mut rel_start = 0usize;
        let mut skip_lines = line0.saturating_sub(piece_table.line_count());
        while skip_lines > 0 {
            let scanned = next_piece_table_scan_line_range(&bytes, rel_start, buffer_reaches_eof)?;
            if scanned.range.1 <= rel_start || !scanned.exact {
                return None;
            }
            rel_start = scanned.range.1;
            skip_lines -= 1;
        }

        let scanned = next_piece_table_scan_line_range(&bytes, rel_start, buffer_reaches_eof)?;
        Some(PieceTableScanRange {
            range: (
                scan_start.saturating_add(scanned.range.0),
                scan_start.saturating_add(scanned.range.1),
            ),
            exact: scanned.exact,
        })
    }

    fn scanned_piece_table_offset_for_position(
        piece_table: &PieceTable,
        position: TextPosition,
    ) -> Option<(usize, bool)> {
        let scanned = Self::scanned_piece_table_line_range(piece_table, position.line0())?;
        let bytes = piece_table.read_range(scanned.range.0, scanned.range.1);
        let offset = byte_offset_for_text_col_in_bytes(&bytes, (0, bytes.len()), position.col0());
        Some((
            scanned.range.0.saturating_add(offset),
            Self::piece_table_position_is_representable(piece_table, position),
        ))
    }

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
            if let Some(scanned) = Self::scanned_piece_table_line_range(piece_table, line0) {
                let bytes = piece_table.read_range(scanned.range.0, scanned.range.1);
                let mut end = bytes.len();
                while end > 0 {
                    let b = bytes[end - 1];
                    if b == b'\n' || b == b'\r' {
                        end -= 1;
                    } else {
                        break;
                    }
                }
                return if scanned.exact {
                    count_text_columns_exact(&bytes[..end])
                } else {
                    count_text_columns(&bytes[..end], MAX_LINE_SCAN_CHARS)
                };
            }
            return 0;
        }
        if let Some(rope) = &self.rope {
            return Self::rope_line_len_chars_without_newline(rope, line0);
        }

        let bytes = self.mmap_bytes();
        let file_len = self.file_len.min(bytes.len());
        if file_len == 0 {
            return 0;
        }

        let Some(start) = self.mmap_line_start_offset_exact(line0) else {
            return 0;
        };
        let mut end = super::search::next_line_start_exact(bytes, file_len, start).min(file_len);
        while end > start {
            let b = bytes[end - 1];
            if b == b'\n' || b == b'\r' {
                end -= 1;
            } else {
                break;
            }
        }
        count_text_columns_exact(&bytes[start..end])
    }

    /// Clamps a typed position into the currently known document bounds.
    pub fn clamp_position(&self, position: TextPosition) -> TextPosition {
        let line0 = if self.rope.is_some() {
            position
                .line0()
                .min(self.display_line_count().max(1).saturating_sub(1))
        } else if let Some(piece_table) = &self.piece_table {
            if piece_table.full_index()
                || piece_table.has_line(position.line0())
                || Self::scanned_piece_table_line_range(piece_table, position.line0()).is_some()
            {
                position.line0()
            } else {
                position
                    .line0()
                    .min(self.display_line_count().max(1).saturating_sub(1))
            }
        } else {
            let bytes = self.mmap_bytes();
            let file_len = self.file_len.min(bytes.len());
            let eof_position = self.mmap_position_for_byte_offset(file_len);
            if self
                .mmap_line_start_offset_exact(position.line0())
                .is_some()
            {
                position.line0()
            } else {
                eof_position.line0()
            }
        };
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
        if let Some(piece_table) = &self.piece_table {
            let offset = safe_piece_table_offset_for_position(piece_table, position);
            return count_text_units_in_bytes(&piece_table.read_range(0, offset));
        }
        let bytes = self.mmap_bytes();
        let file_len = self.file_len.min(bytes.len());
        let offset = self.mmap_byte_offset_for_position(position).min(file_len);
        count_text_units_in_bytes(&bytes[..offset])
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
        if let Some(piece_table) = &self.piece_table {
            let start_offset = safe_piece_table_offset_for_position(piece_table, start);
            let end_offset = safe_piece_table_offset_for_position(piece_table, end);
            return count_text_units_in_bytes(
                &piece_table.read_range(start_offset.min(end_offset), end_offset.max(start_offset)),
            );
        }

        let bytes = self.mmap_bytes();
        let file_len = self.file_len.min(bytes.len());
        let start_offset = self.mmap_byte_offset_for_position(start).min(file_len);
        let end_offset = self.mmap_byte_offset_for_position(end).min(file_len);
        count_text_units_in_bytes(
            &bytes[start_offset.min(end_offset)..end_offset.max(start_offset)],
        )
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
        if self.selection_requires_piece_table_promotion(selection) {
            let total_len = self
                .piece_table
                .as_ref()
                .map(|piece_table| piece_table.total_len())
                .unwrap_or(self.file_len);
            return if self.can_materialize_rope(total_len) {
                EditCapability::RequiresPromotion {
                    from: DocumentBacking::PieceTable,
                    to: DocumentBacking::Rope,
                }
            } else {
                EditCapability::Unsupported {
                    backing: DocumentBacking::PieceTable,
                    reason:
                        "document is too large to widen partial piece-table editing beyond the indexed prefix",
                }
            };
        }
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
