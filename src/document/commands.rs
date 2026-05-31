use super::alignment::AlignDirection;
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

        // UTF-8 documents continue to use the rope/piece-tree path
        // unchanged. Every other encoding (Class A and Class B)
        // routes through the encoded path so add-buffer bytes are written
        // directly in the document's target encoding without ever
        // transcoding the document into UTF-8.
        if !self.encoding.is_utf8() {
            return self.try_insert_text_at_encoded(line0, col0, text);
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

    /// Encoded insert path for non-UTF-8 documents.
    ///
    /// This is the native edit-buffer dispatch for every Class A
    /// and Class B encoding. It encodes `text` into the document's target
    /// encoding via `encoding_rs::Encoding::encode`, validates the encode
    /// outcome, and then writes the resulting bytes verbatim into the
    /// piece-tree add buffer through
    /// [`PieceTable::insert_encoded_bytes_at`]. The document is never
    /// transcoded into UTF-8, and an unrepresentable input string
    /// is rejected before the add buffer is touched.
    ///
    /// Critical contracts:
    ///
    /// - **No mutation on unrepresentable input.** When
    ///   `had_unmappable == true`, the function returns
    ///   `DocumentError::Encoding { reason: UnrepresentableText }` before
    ///   any byte lands in `piece_table.add`. Likewise, if
    ///   `encoding_rs::Encoding::encode` redirects the output to a
    ///   different encoding (e.g. UTF-8 for `replacement` /
    ///   `x-user-defined`), the caller's intent could not be honoured and
    ///   `RedirectedSaveTarget` is returned with the actual output
    ///   encoding.
    /// - **Char-boundary alignment.** The byte offset of the
    ///   insertion point is rounded backward to the nearest character
    ///   boundary of the document's current encoding through
    ///   [`Document::align_byte_offset`].
    /// - **No UTF-8 transcode of document content.** The function
    ///   only reads `&str` (the user's input, which is UTF-8 by Rust's
    ///   contract) and writes the encoded bytes into piece-tree storage.
    ///   It never touches `self.rope`.
    /// - **Piece-tree add buffer.** [`Document::prepare_edit_at`]
    ///   promotes a storage-backed non-UTF-8 document to a piece-tree
    ///   edit buffer; the resulting add buffer holds raw target-encoding
    ///   bytes.
    /// - **Explicit conversions still work.** Save-time
    ///   conversion via `DocumentSaveOptions::with_encoding(...)` and
    ///   reinterpret-open into UTF-8 are not affected by this path: they
    ///   own their own decode / encode steps and run outside this
    ///   function.
    fn try_insert_text_at_encoded(
        &mut self,
        line0: usize,
        col0: usize,
        text: &str,
    ) -> Result<(usize, usize), DocumentError> {
        // Encode the user's text first and validate the outcome
        // before doing anything that could mutate document state. If the
        // input contains an unmappable scalar, or `encoding_rs` decided
        // to redirect the output to a different encoding, we must return
        // an error without touching the piece-tree add buffer.
        let target = self.encoding;
        let target_encoding = target.as_encoding();
        let target_name = target.name();
        let encoded: Vec<u8> = if target_name == "UTF-16LE" {
            // UTF-16 LE / BE: `encoding_rs` deliberately refuses to emit
            // UTF-16 (the WHATWG spec only labels it as a decoder and
            // redirects the encoder to UTF-8). Every Unicode scalar is
            // representable in UTF-16 — including supplementary code
            // points via surrogate pairs — so encoding through
            // `str::encode_utf16` cannot fail and matches the bytes the
            // open path expects. The UTF-16 path therefore
            // never reaches the encoding-rs redirect / unmappable
            // checks below.
            text.encode_utf16().flat_map(|u| u.to_le_bytes()).collect()
        } else if target_name == "UTF-16BE" {
            text.encode_utf16().flat_map(|u| u.to_be_bytes()).collect()
        } else {
            let (bytes, output_encoding, had_unmappable) = target_encoding.encode(text);
            if output_encoding != target_encoding {
                // Explicit conversions still flow through the
                // save / reopen surface; this function only handles the
                // implicit-target encoded edit path, so an encoding-rs
                // redirect is reported as a typed error rather than
                // silently succeeding under a different output contract.
                return Err(self.encoding_insert_error(
                    DocumentEncodingErrorKind::RedirectedSaveTarget {
                        actual: DocumentEncoding::from_encoding_rs(output_encoding),
                    },
                ));
            }
            if had_unmappable {
                return Err(
                    self.encoding_insert_error(DocumentEncodingErrorKind::UnrepresentableText)
                );
            }
            bytes.into_owned()
        };

        // Promote the document to a piece-tree edit buffer if
        // it is still mmap-only. The edit_buffer_plan_for_line gate in
        // editing.rs was relaxed so non-UTF-8 documents
        // route through the piece-tree branch instead of the legacy
        // rope-decode bridge.
        self.prepare_edit_at(line0)?;
        let line0 = self.clamp_raw_edit_line0_after_prepare(line0);

        // Compute the byte offset of (line0, col0) in the current
        // backing. The encoded edit path only ever runs against either
        // the piece-tree (after `prepare_edit_at` promoted us) or, in
        // degenerate fallback cases where the rope path took over (e.g.
        // very small files where `piece_table_line_lengths_for_edit`
        // returned None), against the rope itself. We delegate to the
        // piece-table walker when one exists; otherwise we fall back to
        // the existing UTF-8 rope path, which is allowed because a rope
        // backing for a non-UTF-8 document is the explicit fallback
        // permitted in editing.rs (the default keeps us off it, but it
        // remains as a documented escape hatch).
        let raw_byte_offset = if let Some(piece_table) = self.piece_table.as_ref() {
            let actual_col0 = piece_table.line_len_chars(line0);
            let insert_col0 = col0.min(actual_col0);
            piece_table.byte_offset_for_col(line0, insert_col0)
        } else if self.rope.is_some() {
            // Document genuinely fell back to the rope bridge. We have
            // no native byte-offset surface here, so use the rope path
            // for the insert cursor and rely on the rope to carry the
            // edit through. The rope decoder already converted the
            // document into canonical UTF-8 in this fallback branch, so
            // inserting `text` directly is consistent with the bridge's
            // contract.
            return self.try_insert_text_at_encoded_rope_fallback(line0, col0, text);
        } else if self.storage.is_some() {
            self.mmap_byte_offset_for_position(TextPosition::new(line0, col0))
        } else {
            // No backing at all — empty new document with non-UTF-8
            // contract. Treat this as inserting at offset 0; cursor
            // bookkeeping below still produces a sensible position.
            0
        };

        // Skip a leading byte-order mark for UTF-16 LE / BE. The BOM is
        // preserved in the piece-tree bytes so preserve-save round-trips
        // it byte-for-byte, but conceptually "line 0 column 0" is
        // *after* the BOM — `text_lossy` and `decode_with_bom_removal`
        // both strip it from user-visible text. Inserting before the
        // BOM would push it into the middle of the document on
        // conversion-save, which is observable through
        // `decode_with_bom_removal` (it only strips a leading BOM).
        let bom_len = self.leading_bom_len_for_encoded_insert();
        let raw_byte_offset = raw_byte_offset.max(bom_len);

        // Align the insertion offset onto a character boundary of
        // the document's current encoding. Backward rounding keeps the
        // insertion before the start of any partial multi-byte cell.
        let byte_offset = self.align_byte_offset(raw_byte_offset, AlignDirection::Backward);
        let byte_offset = byte_offset.max(bom_len);

        // Append the encoded bytes verbatim into the piece-tree add
        // buffer. If the document fell back to the
        // rope bridge above, the early return already handled it.
        let doc_path = self.path.clone();
        let piece_table = self
            .piece_table
            .as_mut()
            .expect("prepare_edit_at must install a piece-tree for non-UTF-8 storage-backed docs");
        let path = session_sidecar_path(doc_path.as_deref(), piece_table.original.path());
        let outcome = piece_table
            .insert_encoded_bytes_at(byte_offset, &encoded)
            .map_err(|source| DocumentError::Write { path, source })?;
        if outcome.edited {
            self.mark_dirty();
        }

        // Cursor bookkeeping: count code points and embedded line breaks
        // in the user's `&str`. Encoding boundaries do not matter for
        // the (line0, col0) result because we count text units, not
        // bytes — every `\r\n` collapses to one logical line break per
        // the engine's CRLF semantics.
        let mut added_lines = 0usize;
        let mut last_col = 0usize;
        let mut chars = text.chars().peekable();
        while let Some(ch) = chars.next() {
            match ch {
                '\r' => {
                    if chars.peek() == Some(&'\n') {
                        let _ = chars.next();
                    }
                    added_lines += 1;
                    last_col = 0;
                }
                '\n' => {
                    added_lines += 1;
                    last_col = 0;
                }
                _ => last_col += 1,
            }
        }
        if added_lines == 0 {
            Ok((line0, col0.saturating_add(last_col)))
        } else {
            Ok((line0.saturating_add(added_lines), last_col))
        }
    }

    /// Degenerate fallback: a non-UTF-8 document landed on the rope
    /// edit-buffer plan (e.g. through `ensure_rope` for multi-line
    /// virtual padding) and we still need to honour an `&str` insert.
    /// The rope already holds canonical UTF-8 in that branch, so the
    /// insert is byte-for-byte the same as the UTF-8 fast path.
    ///
    /// This branch is intentionally narrow: by default the insert
    /// path keeps the document in its native encoding, but the rope
    /// bridge documented in `editing.rs` still exists as an escape
    /// hatch.
    fn try_insert_text_at_encoded_rope_fallback(
        &mut self,
        line0: usize,
        col0: usize,
        text: &str,
    ) -> Result<(usize, usize), DocumentError> {
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

    fn encoding_insert_error(&self, reason: DocumentEncodingErrorKind) -> DocumentError {
        DocumentError::Encoding {
            path: self.path.clone().unwrap_or_default(),
            operation: "insert",
            encoding: self.encoding,
            reason,
        }
    }

    /// Returns the byte length of a leading byte-order mark in the
    /// document's encoded byte stream, or `0` when no BOM is present.
    ///
    /// The native open paths preserve the BOM in the underlying
    /// storage so preserve-save round-trips it byte-for-byte. The user-
    /// visible text obtained through `decode_with_bom_removal` strips
    /// the BOM, so the encoded edit path treats "line 0 column 0" as
    /// the position immediately after the BOM — otherwise an insert at
    /// (0, 0) would be placed before the BOM, leaving the BOM as a
    /// stray U+FEFF in the middle of the document on conversion-save.
    fn leading_bom_len_for_encoded_insert(&self) -> usize {
        let bytes = if let Some(piece_table) = &self.piece_table {
            // The piece-tree fronts the original storage; the BOM lives
            // at offset 0 of `piece_table.read_range(0, 2)`.
            piece_table.read_range(0, 2.min(piece_table.total_len()))
        } else if let Some(storage) = &self.storage {
            let head_len = 2.min(storage.len());
            storage.read_range(0, head_len).to_vec()
        } else {
            return 0;
        };
        match self.encoding.name() {
            "UTF-16LE" if bytes.starts_with(&[0xFF, 0xFE]) => 2,
            "UTF-16BE" if bytes.starts_with(&[0xFE, 0xFF]) => 2,
            _ => 0,
        }
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

        // Non-UTF-8 documents go through the encoded
        // delete path so the byte range handed to `PieceTable::delete_range`
        // is rounded onto character boundaries of the document's current
        // encoding before any bytes are removed. UTF-8 documents keep the
        // existing rope / piece-table arithmetic that treats byte offsets
        // as already-aligned UTF-8 char boundaries.
        if !self.encoding.is_utf8() {
            return self.try_delete_range_at_encoded(line0, col0, len_chars);
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

    /// Encoded delete path for non-UTF-8 documents.
    ///
    /// Mirrors `try_insert_text_at_encoded`: deletion happens directly on
    /// the piece-tree's raw target-encoding bytes. The byte range is
    /// computed from the piece-tree's engine-aware text-unit walker
    /// (`*_with_engine` variants) so a single text-unit advance lands on
    /// a character boundary of the current encoding, and then both
    /// endpoints are clamped through [`Document::align_byte_offset`]:
    ///
    /// - `start_byte` is rounded **backward** so the deletion never
    ///   starts in the middle of a multi-byte cell.
    /// - `end_byte` is rounded **forward** so the deletion never ends
    ///   inside a partial multi-byte cell either.
    ///
    /// Documents that fell back to the rope edit-buffer bridge
    /// (canonical UTF-8) reuse the rope path — the alignment surface is
    /// a no-op there because the rope already holds UTF-8.
    ///
    /// Document content is never transcoded into UTF-8; the
    /// piece-tree continues to hold raw target-encoding bytes throughout.
    fn try_delete_range_at_encoded(
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

        // Non-UTF-8 documents on the rope-decode bridge keep the existing
        // UTF-8 rope path: the bridge already transcodes the document
        // into canonical UTF-8 so `rope.remove` sees a UTF-8 char index.
        if self.piece_table.is_none() {
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
            return Ok((line0, start_col0));
        }

        // Compute the deletion byte range from the
        // piece-tree's text-unit walker and then realign each endpoint
        // onto a character boundary of the current encoding. The walker
        // here is the engine-aware `*_with_engine` variant so multi-byte
        // cells of UTF-16 / Class B encodings are counted as a single
        // text unit; alignment then post-clamps offsets onto a
        // character boundary.
        let engine = self.encoding_engine();
        let (start_col0, raw_start, raw_end) = {
            let piece_table = self
                .piece_table
                .as_ref()
                .expect("piece_table presence checked above");
            let actual_col0 = piece_table.line_len_chars_with_engine(line0, engine);
            let start_col0 = col0.min(actual_col0);
            let raw_start = piece_table.byte_offset_for_col_with_engine(line0, start_col0, engine);
            let raw_end =
                piece_table.advance_offset_by_text_units_with_engine(raw_start, len_chars, engine);
            (start_col0, raw_start, raw_end)
        };

        let aligned_start = self.align_byte_offset(raw_start, AlignDirection::Backward);
        let aligned_end = self.align_byte_offset(raw_end, AlignDirection::Forward);
        if aligned_end <= aligned_start {
            return Ok((line0, start_col0));
        }

        let doc_path = self.path.clone();
        let piece_table = self
            .piece_table
            .as_mut()
            .expect("piece_table presence checked above");
        let path = session_sidecar_path(doc_path.as_deref(), piece_table.original.path());
        piece_table
            .delete_range(aligned_start, aligned_end - aligned_start)
            .map_err(|source| DocumentError::Write { path, source })?;
        self.mark_dirty();
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

        // Non-UTF-8 documents replace by
        // composing the encoded delete and encoded insert paths. Both
        // operations use byte offsets aligned via `align_byte_offset` and
        // never transcode the document content into UTF-8. The dispatch
        // is a sibling of `try_insert_text_at`'s encoding fork.
        if !self.encoding.is_utf8() {
            return self.try_replace_range_encoded(line0, col0, len_chars, text);
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

    /// Encoded replace path for non-UTF-8 documents.
    ///
    /// Performs a single delete + insert transaction directly on the
    /// piece-tree's raw target-encoding bytes:
    ///
    /// 1. Encode `text` first via `encoding_rs` and bail on
    ///    `had_unmappable` / `RedirectedSaveTarget` **before any byte
    ///    leaves the document**. UTF-16 LE / BE bypass
    ///    encoding-rs and use `str::encode_utf16` because every Unicode
    ///    scalar is representable in UTF-16 and `encoding_rs` refuses
    ///    to emit it.
    /// 2. Resolve the deletion byte range from the engine-aware
    ///    text-unit walker (`*_with_engine` variants) and align both
    ///    endpoints onto character boundaries of the current encoding
    ///    through [`Document::align_byte_offset`] — backward for the
    ///    start, forward for the end.
    /// 3. `begin_edit_batch` → `delete_range` → `insert_encoded_bytes_at`
    ///    → `end_edit_batch`. The session flush is debounced through
    ///    the batch so the on-disk session log sees a single edit
    ///    rather than a delete-then-insert pair.
    ///
    /// Document content is never transcoded into UTF-8; the
    /// piece-tree carries raw target-encoding bytes throughout. The
    /// rope edit-buffer bridge (canonical UTF-8 fallback) takes the
    /// existing UTF-8 rope replace path because the rope already holds
    /// UTF-8.
    fn try_replace_range_encoded(
        &mut self,
        line0: usize,
        col0: usize,
        len_chars: usize,
        text: &str,
    ) -> Result<(usize, usize), DocumentError> {
        // Empty range + empty text is a no-op handled by the caller.
        // Empty range with non-empty text reduces to insert.
        if len_chars == 0 {
            return self.try_insert_text_at(line0, col0, text);
        }
        // Empty replacement reduces to delete.
        if text.is_empty() {
            return self.try_delete_range_at_encoded(line0, col0, len_chars);
        }

        // Encode first so an unmappable input never mutates the
        // document. The encode contract here matches
        // `try_insert_text_at_encoded` byte-for-byte.
        let target = self.encoding;
        let target_encoding = target.as_encoding();
        let target_name = target.name();
        let encoded: Vec<u8> = if target_name == "UTF-16LE" {
            text.encode_utf16().flat_map(|u| u.to_le_bytes()).collect()
        } else if target_name == "UTF-16BE" {
            text.encode_utf16().flat_map(|u| u.to_be_bytes()).collect()
        } else {
            let (bytes, output_encoding, had_unmappable) = target_encoding.encode(text);
            if output_encoding != target_encoding {
                return Err(self.encoding_insert_error(
                    DocumentEncodingErrorKind::RedirectedSaveTarget {
                        actual: DocumentEncoding::from_encoding_rs(output_encoding),
                    },
                ));
            }
            if had_unmappable {
                return Err(
                    self.encoding_insert_error(DocumentEncodingErrorKind::UnrepresentableText)
                );
            }
            bytes.into_owned()
        };

        // Promote the document to a piece-tree edit buffer if it is
        // still mmap-only.
        self.prepare_edit_at(line0)?;
        let line0 = self.clamp_raw_edit_line0_after_prepare(line0);

        // Documents that landed on the rope bridge fall back to the
        // existing two-step path: the rope already holds canonical
        // UTF-8 so the implicit decode/encode round-trip cannot lose
        // information.
        if self.piece_table.is_none() {
            let (line0, col0) = self.try_delete_range_at_encoded(line0, col0, len_chars)?;
            return self.try_insert_text_at(line0, col0, text);
        }

        // Compute the deletion byte range from the
        // engine-aware text-unit walker, then align both endpoints onto
        // a character boundary of the current encoding.
        let engine = self.encoding_engine();
        let (start_col0, raw_start, raw_end) = {
            let piece_table = self
                .piece_table
                .as_ref()
                .expect("piece_table presence checked above");
            let actual_col0 = piece_table.line_len_chars_with_engine(line0, engine);
            let start_col0 = col0.min(actual_col0);
            let raw_start = piece_table.byte_offset_for_col_with_engine(line0, start_col0, engine);
            let raw_end =
                piece_table.advance_offset_by_text_units_with_engine(raw_start, len_chars, engine);
            (start_col0, raw_start, raw_end)
        };

        let aligned_start = self.align_byte_offset(raw_start, AlignDirection::Backward);
        let aligned_end = self.align_byte_offset(raw_end, AlignDirection::Forward);
        // BOM cannot be deleted by an insert/replace at (line 0, col 0):
        // the leading BOM is preserved in the piece-tree bytes for
        // round-trip save and is conceptually outside the user-
        // visible text.
        let bom_len = self.leading_bom_len_for_encoded_insert();
        let aligned_start = aligned_start.max(bom_len);
        let aligned_end = aligned_end.max(aligned_start);

        // Write directly into piece-tree storage. The
        // delete + insert pair runs as a single edit batch so the
        // session flush sees one transaction instead of two.
        let doc_path = self.path.clone();
        let piece_table = self
            .piece_table
            .as_mut()
            .expect("piece_table presence checked above");
        let path = session_sidecar_path(doc_path.as_deref(), piece_table.original.path());
        let outcome = piece_table
            .replace_encoded_bytes_at(aligned_start, aligned_end, &encoded)
            .map_err(|source| DocumentError::Write { path, source })?;
        if outcome {
            self.mark_dirty();
        }

        // Cursor bookkeeping: mirrors `try_insert_text_at_encoded`.
        let mut added_lines = 0usize;
        let mut last_col = 0usize;
        let mut chars = text.chars().peekable();
        while let Some(ch) = chars.next() {
            match ch {
                '\r' => {
                    if chars.peek() == Some(&'\n') {
                        let _ = chars.next();
                    }
                    added_lines += 1;
                    last_col = 0;
                }
                '\n' => {
                    added_lines += 1;
                    last_col = 0;
                }
                _ => last_col += 1,
            }
        }
        if added_lines == 0 {
            Ok((line0, start_col0.saturating_add(last_col)))
        } else {
            Ok((line0.saturating_add(added_lines), last_col))
        }
    }

    /// Encoded backspace path for non-UTF-8 documents.
    ///
    /// Walks one text-unit backward from `(line0, col0)` through the
    /// document's encoding engine, then deletes the corresponding byte
    /// range from the piece-tree directly. The byte step is computed
    /// from a contiguous slice of the current line so the engine's
    /// `step_backward` contract (flat byte slice with `start <= offset
    /// <= bytes.len()`) holds for every encoding family:
    ///
    /// - UTF-8: walks back over continuation bytes.
    /// - Class A: 1 byte (or 2 for CRLF).
    /// - UTF-16: 2 bytes for a BMP cell, 4 for a surrogate pair.
    /// - Class B: scan-from-anchor through the line's bytes.
    ///
    /// At column 0 the path collapses onto line-merge: the previous
    /// line's terminating LF / CR / CRLF is removed verbatim. Line
    /// endings are 1-byte ASCII in every supported encoding, so the
    /// existing `PieceTable::backspace_at` line-merge path is reused
    /// as-is in that case.
    ///
    /// Document content is never transcoded into UTF-8.
    fn try_backspace_at_encoded(
        &mut self,
        line0: usize,
        col0: usize,
    ) -> Result<(bool, usize, usize), DocumentError> {
        self.prepare_edit_at(line0)?;
        let line0 = self.clamp_raw_edit_line0_after_prepare(line0);

        // Documents that fell back to the rope edit-buffer bridge
        // (canonical UTF-8) reuse the rope path, like the encoded
        // delete path.
        if self.piece_table.is_none() {
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
            let result = if prev_ch == '\n' {
                let new_line0 = line0.saturating_sub(1);
                let new_col0 = Self::rope_line_len_chars_without_newline(rope, new_line0);
                (true, new_line0, new_col0)
            } else {
                (true, line0, col0.saturating_sub(1))
            };
            self.mark_dirty();
            return Ok(result);
        }

        // Column 0 + line 0 → nothing to delete.
        if col0 == 0 && line0 == 0 {
            return Ok((false, line0, col0));
        }

        // Column 0 on line > 0: line-merge. Line endings are 1-byte
        // ASCII (LF, CR, or CRLF) in every supported encoding, so the
        // existing piece-tree line-merge path is encoding-correct.
        let engine = self.encoding_engine();
        if col0 == 0 {
            let doc_path = self.path.clone();
            let piece_table = self
                .piece_table
                .as_mut()
                .expect("piece_table presence checked above");
            let path = session_sidecar_path(doc_path.as_deref(), piece_table.original.path());
            return match piece_table.backspace_at(line0, col0) {
                Ok((edited, new_line0, new_col0)) => {
                    if edited {
                        self.mark_dirty();
                    }
                    // The cursor coming back from `backspace_at` reuses
                    // the UTF-8-naive `line_len_chars`; recompute the
                    // column count via the engine-aware walker so
                    // multi-byte cells of the previous line are
                    // counted as one text unit each.
                    if edited {
                        let new_col0 = self.piece_table.as_ref().map_or(new_col0, |pt| {
                            pt.line_len_chars_with_engine(new_line0, engine)
                        });
                        Ok((true, new_line0, new_col0))
                    } else {
                        Ok((false, new_line0, new_col0))
                    }
                }
                Err(source) => Err(DocumentError::Write { path, source }),
            };
        }

        // col0 > 0: walk one text-unit backward via the engine.
        let (line_start, cur_byte, step_back) = {
            let piece_table = self
                .piece_table
                .as_ref()
                .expect("piece_table presence checked above");
            let actual_col0 = piece_table.line_len_chars_with_engine(line0, engine);
            let effective_col0 = col0.min(actual_col0);
            let (line_start, _line_end) = piece_table.line_byte_range(line0);
            let cur_byte =
                piece_table.byte_offset_for_col_with_engine(line0, effective_col0, engine);
            // Read the byte slice from `line_start` to `cur_byte` and
            // ask the engine how many bytes the previous text unit
            // occupies. The slice is bounded by the current line's
            // length, which is the same cost as `byte_offset_for_col`
            // already paid above.
            let window = piece_table.read_byte_range(line_start, cur_byte);
            let window_len = window.len();
            let step = engine.step_backward(&window, window_len, 0);
            (line_start, cur_byte, step)
        };

        // BOM guard: never delete bytes that belong to a leading BOM.
        let bom_len = self.leading_bom_len_for_encoded_insert();
        if cur_byte <= bom_len {
            return Ok((false, line0, col0));
        }
        if step_back == 0 {
            // Engine reported no boundary — clamp the cursor without
            // mutating the document. Treats the position as already at
            // the line start.
            return Ok((false, line0, col0));
        }
        let delete_start = cur_byte
            .saturating_sub(step_back)
            .max(line_start)
            .max(bom_len);
        let delete_len = cur_byte.saturating_sub(delete_start);
        if delete_len == 0 {
            return Ok((false, line0, col0));
        }

        let doc_path = self.path.clone();
        let piece_table = self
            .piece_table
            .as_mut()
            .expect("piece_table presence checked above");
        let path = session_sidecar_path(doc_path.as_deref(), piece_table.original.path());
        piece_table
            .delete_range(delete_start, delete_len)
            .map_err(|source| DocumentError::Write { path, source })?;
        self.mark_dirty();
        Ok((true, line0, col0.saturating_sub(1)))
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
        // Non-UTF-8 documents go through the encoded
        // backspace path so the deletion byte range is computed via the
        // engine-aware text-unit walker, not via UTF-8 stepping. UTF-8
        // documents keep the existing rope / piece-tree path.
        if !self.encoding.is_utf8() {
            return self.try_backspace_at_encoded(line0, col0);
        }

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

        // Non-UTF-8 documents reuse the encoded delete
        // path so the byte range is character-aligned through
        // `align_byte_offset` before `PieceTable::delete_range` runs.
        // `try_delete_range_at_encoded` returns the post-edit cursor
        // (line0, col0); we recover the `edited` flag by comparing
        // against the pre-edit `(line0, col0)` plus a length probe of
        // the line — a deletion at the end of a line where there is
        // nothing to remove returns the same cursor and reports `false`.
        if !self.encoding.is_utf8() {
            let prev_total = self.file_len();
            let (new_line0, new_col0) = self.try_delete_range_at_encoded(line0, col0, 1)?;
            let edited = self.file_len() != prev_total;
            return Ok((edited, new_line0, new_col0));
        }

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
