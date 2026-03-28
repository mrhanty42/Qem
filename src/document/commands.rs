use super::*;

impl Document {
    fn clamp_raw_edit_line0_after_prepare(&self, line0: usize) -> usize {
        line0.min(self.bounded_line_count().saturating_sub(1))
    }

    fn prepare_selection_for_edit(
        &mut self,
        selection: TextSelection,
    ) -> Result<TextSelection, DocumentError> {
        if self.selection_requires_piece_table_promotion(selection) {
            self.promote_piece_table_to_rope()?;
        }
        Ok(self.clamp_selection(selection))
    }

    /// Attempts to insert text at the given position and returns the new cursor coordinates.
    ///
    /// Passing an empty string is a no-op and keeps the document clean.
    ///
    /// # Errors
    /// Returns [`DocumentError::EditUnsupported`] if editing would require
    /// fully materializing an excessively large file in memory.
    pub fn try_insert_text_at(
        &mut self,
        line0: usize,
        col0: usize,
        text: &str,
    ) -> Result<(usize, usize), DocumentError> {
        if text.is_empty() {
            return Ok((line0, col0));
        }

        self.prepare_edit_at(line0)?;
        let line0 = self.clamp_raw_edit_line0_after_prepare(line0);
        let doc_path = self.path.clone();
        if let Some(piece_table) = self.piece_table.as_mut() {
            let path = session_sidecar_path(doc_path.as_deref(), piece_table.original.path());
            let outcome = piece_table
                .insert_text_at(self.line_ending, line0, col0, text)
                .map_err(|source| DocumentError::Write { path, source })?;
            if outcome.edited {
                self.mark_dirty();
            }
            return Ok(outcome.cursor);
        }

        let rope = self.rope_mut()?;

        let actual_col0 = Self::rope_line_len_chars_without_newline(rope, line0);
        let insert_at = Self::line_col_to_char_index(rope, line0, col0.min(actual_col0));
        let virtual_padding_cols = col0.saturating_sub(actual_col0);
        let mut added_lines = 0usize;
        let mut last_col = 0usize;
        let needs_normalization =
            text.contains('\r') || text.contains('\n') || virtual_padding_cols > 0;
        if needs_normalization {
            let (normalized, normalized_lines, normalized_last_col) =
                normalize_insert_text(text, virtual_padding_cols, LineEnding::Lf);
            added_lines = normalized_lines;
            last_col = normalized_last_col;
            rope.insert(insert_at, &normalized);
        } else {
            for ch in text.chars() {
                if ch == '\n' {
                    added_lines += 1;
                    last_col = 0;
                } else {
                    last_col += 1;
                }
            }
            rope.insert(insert_at, text);
        }
        if added_lines == 0 {
            Ok((line0, col0.saturating_add(last_col)))
        } else {
            Ok((line0.saturating_add(added_lines), last_col))
        }
    }

    /// Attempts to insert text at a typed position and returns the resulting cursor.
    pub fn try_insert(
        &mut self,
        position: TextPosition,
        text: &str,
    ) -> Result<TextPosition, DocumentError> {
        let position = self.clamp_position(position);
        self.try_insert_text_at(position.line0(), position.col0(), text)
            .map(|(line0, col0)| TextPosition::new(line0, col0))
    }

    /// Deprecated compatibility wrapper for [`Document::try_insert`].
    #[doc(hidden)]
    #[deprecated(since = "0.3.0", note = "use try_insert() for explicit error handling")]
    pub fn insert(&mut self, position: TextPosition, text: &str) -> TextPosition {
        self.try_insert(position, text).unwrap_or(position)
    }

    /// Inserts text at the given position and returns the new cursor coordinates.
    ///
    /// On edit failure, this compatibility helper preserves the previous
    /// behavior and returns the original coordinates unchanged. Use
    /// [`Document::try_insert_text_at`] for explicit error handling.
    #[doc(hidden)]
    #[deprecated(
        since = "0.3.0",
        note = "use try_insert_text_at() for explicit error handling"
    )]
    pub fn insert_text_at(&mut self, line0: usize, col0: usize, text: &str) -> (usize, usize) {
        self.try_insert_text_at(line0, col0, text)
            .unwrap_or((line0, col0))
    }

    fn try_delete_text_range_at_internal(
        &mut self,
        line0: usize,
        col0: usize,
        len_chars: usize,
    ) -> Result<(usize, usize), DocumentError> {
        if len_chars == 0 {
            return Ok((line0, col0));
        }

        self.prepare_edit_at(line0)?;
        let line0 = self.clamp_raw_edit_line0_after_prepare(line0);

        let doc_path = self.path.clone();
        if let Some(piece_table) = self.piece_table.as_mut() {
            let (start_col0, edited, delete_result) = {
                let actual_col0 = piece_table.line_len_chars(line0);
                let start_col0 = col0.min(actual_col0);
                let start = piece_table.byte_offset_for_col(line0, start_col0);
                let end = piece_table.advance_offset_by_text_units(start, len_chars);
                let delete_result = if end > start {
                    let path =
                        session_sidecar_path(doc_path.as_deref(), piece_table.original.path());
                    Some(
                        piece_table
                            .delete_range(start, end - start)
                            .map_err(|source| DocumentError::Write { path, source }),
                    )
                } else {
                    None
                };
                (start_col0, end > start, delete_result)
            };
            if edited {
                self.mark_dirty();
            }
            if let Some(delete_result) = delete_result {
                delete_result?;
            }
            return Ok((line0, start_col0));
        }

        self.ensure_rope()?;
        let Some(rope) = self.rope.as_mut() else {
            return Err(self.missing_rope_error());
        };
        let actual_col0 = Self::rope_line_len_chars_without_newline(rope, line0);
        let start_col0 = col0.min(actual_col0);
        let start = Self::line_col_to_char_index(rope, line0, start_col0);
        let end = start.saturating_add(len_chars).min(rope.len_chars());
        if end > start {
            rope.remove(start..end);
            self.mark_dirty();
        }
        Ok((line0, start_col0))
    }

    /// Replaces `len_chars` text units starting at the given line/column.
    ///
    /// Newline sequences are treated as a single text unit for piece-table backed
    /// documents, so replacing across CRLF text behaves like a normal editor
    /// operation instead of deleting only half of the line break. Replacing a
    /// range with text that normalizes to the current contents is a no-op and
    /// keeps the document clean.
    pub fn try_replace_range(
        &mut self,
        line0: usize,
        col0: usize,
        len_chars: usize,
        text: &str,
    ) -> Result<(usize, usize), DocumentError> {
        if len_chars == 0 && text.is_empty() {
            return Ok((line0, col0));
        }

        self.prepare_edit_at(line0)?;
        let line0 = self.clamp_raw_edit_line0_after_prepare(line0);

        let doc_path = self.path.clone();
        if let Some(piece_table) = self.piece_table.as_mut() {
            let path = session_sidecar_path(doc_path.as_deref(), piece_table.original.path());
            let outcome = piece_table
                .replace_range_at(self.line_ending, line0, col0, len_chars, text)
                .map_err(|source| DocumentError::Write { path, source })?;
            if outcome.edited {
                self.mark_dirty();
            }
            return Ok(outcome.cursor);
        }

        if let Some(rope) = self.rope.as_ref() {
            if let Some(cursor) = Self::rope_replace_noop_cursor(rope, line0, col0, len_chars, text)
            {
                return Ok(cursor);
            }
        }

        let (line0, col0) = self.try_delete_text_range_at_internal(line0, col0, len_chars)?;
        self.try_insert_text_at(line0, col0, text)
    }

    /// Attempts to replace a typed text range and returns the resulting cursor.
    pub fn try_replace(
        &mut self,
        range: TextRange,
        text: &str,
    ) -> Result<TextPosition, DocumentError> {
        let start = self.clamp_position(range.start());
        self.try_replace_range(start.line0(), start.col0(), range.len_chars(), text)
            .map(|(line0, col0)| TextPosition::new(line0, col0))
    }

    /// Replaces the current selection and returns the resulting caret position.
    pub fn try_replace_selection(
        &mut self,
        selection: TextSelection,
        text: &str,
    ) -> Result<TextPosition, DocumentError> {
        let selection = self.prepare_selection_for_edit(selection)?;
        self.try_replace(self.text_range_for_selection(selection), text)
    }

    /// Deprecated compatibility wrapper for [`Document::try_replace`].
    #[doc(hidden)]
    #[deprecated(
        since = "0.3.0",
        note = "use try_replace() for explicit error handling"
    )]
    pub fn replace(&mut self, range: TextRange, text: &str) -> TextPosition {
        let fallback = self.clamp_position(range.start());
        self.try_replace(range, text).unwrap_or(fallback)
    }

    /// Deprecated compatibility wrapper for [`Document::try_replace_range`].
    ///
    /// On edit failure the original coordinates are returned unchanged.
    #[doc(hidden)]
    #[deprecated(
        since = "0.3.0",
        note = "use try_replace_range() for explicit error handling"
    )]
    pub fn replace_range(
        &mut self,
        line0: usize,
        col0: usize,
        len_chars: usize,
        text: &str,
    ) -> (usize, usize) {
        self.try_replace_range(line0, col0, len_chars, text)
            .unwrap_or((line0, col0))
    }

    /// Attempts to delete the character before the cursor and returns the edit
    /// result together with the new position.
    ///
    /// # Errors
    /// Returns [`DocumentError::EditUnsupported`] if editing would require
    /// fully materializing an excessively large file in memory.
    pub fn try_backspace_at(
        &mut self,
        line0: usize,
        col0: usize,
    ) -> Result<(bool, usize, usize), DocumentError> {
        self.prepare_edit_at(line0)?;
        let line0 = self.clamp_raw_edit_line0_after_prepare(line0);
        let doc_path = self.path.clone();
        if let Some(piece_table) = self.piece_table.as_mut() {
            let path = session_sidecar_path(doc_path.as_deref(), piece_table.original.path());
            match piece_table.backspace_at(line0, col0) {
                Ok((edited, new_line0, new_col0)) => {
                    if edited {
                        self.mark_dirty();
                    }
                    return Ok((edited, new_line0, new_col0));
                }
                Err(source) => {
                    self.mark_dirty();
                    return Err(DocumentError::Write { path, source });
                }
            }
        }

        let rope = self.rope_mut()?;
        if rope.len_chars() == 0 {
            return Ok((false, line0, col0));
        }

        let actual_col0 = Self::rope_line_len_chars_without_newline(rope, line0);
        if col0 > actual_col0 {
            return Ok((false, line0, col0.saturating_sub(1)));
        }

        let cur = Self::line_col_to_char_index(rope, line0, col0);
        if cur == 0 {
            return Ok((false, line0, col0));
        }

        let prev_ch = rope.char(cur - 1);
        rope.remove((cur - 1)..cur);

        if prev_ch == '\n' {
            let new_line0 = line0.saturating_sub(1);
            let new_col0 = Self::rope_line_len_chars_without_newline(rope, new_line0);
            Ok((true, new_line0, new_col0))
        } else {
            Ok((true, line0, col0.saturating_sub(1)))
        }
    }

    /// Attempts to delete the text unit before a typed cursor position.
    pub fn try_backspace(&mut self, position: TextPosition) -> Result<EditResult, DocumentError> {
        let position = self.clamp_position(position);
        self.try_backspace_at(position.line0(), position.col0())
            .map(|(changed, line0, col0)| EditResult::new(changed, TextPosition::new(line0, col0)))
    }

    /// Applies a backspace command to an anchor/head selection.
    ///
    /// When the selection is a caret, this behaves like [`Document::try_backspace`].
    /// When the selection is non-empty, it deletes the selected range instead.
    pub fn try_backspace_selection(
        &mut self,
        selection: TextSelection,
    ) -> Result<EditResult, DocumentError> {
        let selection = self.prepare_selection_for_edit(selection)?;
        if selection.is_caret() {
            self.try_backspace(selection.head())
        } else {
            self.try_delete_selection(selection)
        }
    }

    /// Attempts to delete the text unit at the cursor and returns the edit
    /// result together with the resulting position.
    ///
    /// The cursor stays anchored at the same typed document position when the
    /// deletion succeeds.
    ///
    /// # Errors
    /// Returns [`DocumentError::EditUnsupported`] if editing would require
    /// fully materializing an excessively large file in memory.
    pub fn try_delete_forward_at(
        &mut self,
        line0: usize,
        col0: usize,
    ) -> Result<(bool, usize, usize), DocumentError> {
        let position = self.clamp_position(TextPosition::new(line0, col0));
        let line0 = position.line0();
        let col0 = position.col0();

        self.prepare_edit_at(line0)?;

        let doc_path = self.path.clone();
        if let Some(piece_table) = self.piece_table.as_mut() {
            let (start_col0, edited, delete_result) = {
                let actual_col0 = piece_table.line_len_chars(line0);
                let start_col0 = col0.min(actual_col0);
                let start = piece_table.byte_offset_for_col(line0, start_col0);
                let end = piece_table.advance_offset_by_text_units(start, 1);
                let delete_result = if end > start {
                    let path =
                        session_sidecar_path(doc_path.as_deref(), piece_table.original.path());
                    Some(
                        piece_table
                            .delete_range(start, end - start)
                            .map_err(|source| DocumentError::Write { path, source }),
                    )
                } else {
                    None
                };
                (start_col0, end > start, delete_result)
            };
            if edited {
                self.mark_dirty();
            }
            if let Some(delete_result) = delete_result {
                delete_result?;
                return Ok((true, line0, start_col0));
            }
            return Ok((false, line0, start_col0));
        }

        self.ensure_rope()?;
        let Some(rope) = self.rope.as_mut() else {
            return Err(self.missing_rope_error());
        };
        let actual_col0 = Self::rope_line_len_chars_without_newline(rope, line0);
        let start_col0 = col0.min(actual_col0);
        let start = Self::line_col_to_char_index(rope, line0, start_col0);
        if start >= rope.len_chars() {
            return Ok((false, line0, start_col0));
        }
        rope.remove(start..(start + 1));
        self.mark_dirty();
        Ok((true, line0, start_col0))
    }

    /// Attempts to delete the text unit at a typed cursor position.
    pub fn try_delete_forward(
        &mut self,
        position: TextPosition,
    ) -> Result<EditResult, DocumentError> {
        let position = self.clamp_position(position);
        self.try_delete_forward_at(position.line0(), position.col0())
            .map(|(changed, line0, col0)| EditResult::new(changed, TextPosition::new(line0, col0)))
    }

    /// Applies a forward-delete command to an anchor/head selection.
    ///
    /// When the selection is a caret, this behaves like
    /// [`Document::try_delete_forward`]. When the selection is non-empty, it
    /// deletes the selected range instead.
    pub fn try_delete_forward_selection(
        &mut self,
        selection: TextSelection,
    ) -> Result<EditResult, DocumentError> {
        let selection = self.prepare_selection_for_edit(selection)?;
        if selection.is_caret() {
            self.try_delete_forward(selection.head())
        } else {
            self.try_delete_selection(selection)
        }
    }

    /// Deletes the current selection and returns the resulting caret position.
    ///
    /// Empty caret selections are a no-op.
    pub fn try_delete_selection(
        &mut self,
        selection: TextSelection,
    ) -> Result<EditResult, DocumentError> {
        let selection = self.prepare_selection_for_edit(selection)?;
        if selection.is_caret() {
            return Ok(EditResult::new(false, selection.head()));
        }
        let cursor = self.try_replace_selection(selection, "")?;
        Ok(EditResult::new(true, cursor))
    }

    /// Cuts the current selection, returning the removed text and resulting edit outcome.
    ///
    /// Empty caret selections are a no-op and return an empty string.
    pub fn try_cut_selection(
        &mut self,
        selection: TextSelection,
    ) -> Result<CutResult, DocumentError> {
        let selection = self.prepare_selection_for_edit(selection)?;
        if selection.is_caret() {
            return Ok(CutResult::new(
                String::new(),
                EditResult::new(false, selection.head()),
            ));
        }

        let range = self.text_range_for_selection(selection);
        self.prepare_edit_at(range.start().line0())?;
        let text = self.read_text(range).into_text();
        let edit = self.try_delete_selection(selection)?;
        Ok(CutResult::new(text, edit))
    }

    /// Deprecated compatibility wrapper for [`Document::try_backspace`].
    #[doc(hidden)]
    #[deprecated(
        since = "0.3.0",
        note = "use try_backspace() for explicit error handling"
    )]
    pub fn backspace(&mut self, position: TextPosition) -> EditResult {
        self.try_backspace(position)
            .unwrap_or_else(|_| EditResult::new(false, self.clamp_position(position)))
    }

    /// Deletes the character before the cursor and returns the edit result and
    /// new position.
    ///
    /// On edit failure, this compatibility helper preserves the previous
    /// behavior and reports no change. Use [`Document::try_backspace_at`] for
    /// explicit error handling.
    #[doc(hidden)]
    #[deprecated(
        since = "0.3.0",
        note = "use try_backspace_at() for explicit error handling"
    )]
    pub fn backspace_at(&mut self, line0: usize, col0: usize) -> (bool, usize, usize) {
        self.try_backspace_at(line0, col0)
            .unwrap_or((false, line0, col0))
    }

    /// Deprecated compatibility wrapper for [`Document::try_delete_forward`].
    #[doc(hidden)]
    #[deprecated(
        since = "0.3.0",
        note = "use try_delete_forward() for explicit error handling"
    )]
    pub fn delete_forward(&mut self, position: TextPosition) -> EditResult {
        self.try_delete_forward(position)
            .unwrap_or_else(|_| EditResult::new(false, self.clamp_position(position)))
    }

    /// Deletes the text unit at the cursor and returns the edit result and
    /// resulting position.
    ///
    /// On edit failure, this compatibility helper preserves the previous
    /// behavior and reports no change. Use [`Document::try_delete_forward_at`]
    /// for explicit error handling.
    #[doc(hidden)]
    #[deprecated(
        since = "0.3.0",
        note = "use try_delete_forward_at() for explicit error handling"
    )]
    pub fn delete_forward_at(&mut self, line0: usize, col0: usize) -> (bool, usize, usize) {
        self.try_delete_forward_at(line0, col0)
            .unwrap_or((false, line0, col0))
    }
}
