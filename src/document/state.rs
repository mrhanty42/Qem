use super::*;

impl Document {
    /// Returns `true` if the document has already been materialized as a `Rope`.
    pub fn has_rope(&self) -> bool {
        self.rope.is_some()
    }

    /// Returns `true` if the document has been promoted to a mutable editing buffer.
    pub fn has_edit_buffer(&self) -> bool {
        self.rope.is_some() || self.piece_table.is_some()
    }

    /// Returns `true` if the document is currently backed by a piece table.
    pub fn has_piece_table(&self) -> bool {
        self.piece_table.is_some()
    }

    /// Returns piece-table fragmentation metrics for edited large-file documents.
    ///
    /// Returns `None` unless the document is currently backed by a piece table.
    pub fn fragmentation_stats(&self) -> Option<FragmentationStats> {
        self.piece_table
            .as_ref()
            .map(PieceTable::fragmentation_stats)
    }

    /// Returns piece-table fragmentation metrics using a caller-provided small-piece threshold.
    ///
    /// The threshold is applied in bytes and controls which pieces contribute to
    /// [`FragmentationStats::fragmentation_ratio`].
    pub fn fragmentation_stats_with_threshold(
        &self,
        small_piece_threshold_bytes: usize,
    ) -> Option<FragmentationStats> {
        self.piece_table.as_ref().map(|piece_table| {
            piece_table.fragmentation_stats_with_threshold(small_piece_threshold_bytes)
        })
    }

    /// Returns `true` if the engine knows the exact length of every line.
    pub fn has_precise_line_lengths(&self) -> bool {
        if self.rope.is_some() {
            return true;
        }
        if let Some(piece_table) = &self.piece_table {
            return piece_table.full_index();
        }
        self.is_fully_indexed()
    }

    /// Returns `true` while background indexing of the mmap-backed file is still running.
    pub fn is_indexing(&self) -> bool {
        if self.has_edit_buffer() {
            return false;
        }
        self.indexing.load(Ordering::Relaxed)
    }

    /// Returns `true` if the file has been indexed completely.
    pub fn is_fully_indexed(&self) -> bool {
        self.indexed_bytes() >= self.file_len
    }

    /// Returns `true` while the exact total line count can still improve from
    /// background indexing or a disk-backed line index sidecar.
    pub fn is_line_count_pending(&self) -> bool {
        self.exact_line_count_value().is_none()
            && (self.is_indexing() || self.raw_disk_index_is_building())
    }

    /// Blocks until the exact line count is known or the timeout expires.
    ///
    /// This is a convenience helper for tools, tests, and explicit workflows
    /// that intentionally trade latency for a stable exact total. Frontends
    /// should generally prefer polling [`Document::is_line_count_pending`]
    /// instead of blocking the UI thread.
    pub fn wait_for_exact_line_count(&self, timeout: Duration) -> Option<usize> {
        if let Some(lines) = self.exact_line_count_value() {
            return Some(lines);
        }

        let deadline = Instant::now() + timeout;
        while self.is_line_count_pending() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }

        self.exact_line_count_value()
    }

    /// Returns the elapsed time since indexing started.
    pub fn indexing_elapsed(&self) -> Option<Duration> {
        let started = self.indexing_started?;
        Some(started.elapsed())
    }

    /// Returns the number of source-file bytes that have already been indexed.
    pub fn indexed_bytes(&self) -> usize {
        self.indexed_bytes.load(Ordering::Relaxed)
    }

    /// Returns `(indexed_bytes, total_bytes)` while background indexing is active.
    ///
    /// Prefer [`Document::indexing_state`] in new code when you want a typed
    /// progress value instead of a raw tuple.
    #[doc(hidden)]
    #[deprecated(
        since = "0.3.0",
        note = "use indexing_state() for typed progress instead"
    )]
    pub fn indexing_progress(&self) -> Option<(usize, usize)> {
        if !self.is_indexing() {
            return None;
        }
        Some((self.indexed_bytes(), self.file_len()))
    }

    /// Returns typed indexing progress while background indexing is active.
    pub fn indexing_state(&self) -> Option<ByteProgress> {
        if !self.is_indexing() {
            return None;
        }
        Some(ByteProgress::new(self.indexed_bytes(), self.file_len()))
    }

    /// Returns the current estimate of the average line length in bytes.
    pub fn avg_line_len(&self) -> usize {
        self.avg_line_len.load(Ordering::Relaxed).max(1)
    }

    fn disk_index_total_lines(&self) -> Option<usize> {
        if self.rope.is_some() || self.piece_table.is_some() {
            return None;
        }
        self.raw_disk_index_total_lines()
    }

    pub(super) fn raw_disk_index_total_lines(&self) -> Option<usize> {
        self.disk_index.as_ref()?.total_lines()
    }

    fn raw_disk_index_is_building(&self) -> bool {
        self.disk_index
            .as_ref()
            .map(DiskLineIndex::is_building)
            .unwrap_or(false)
    }

    fn disk_index_checkpoint_for_line(&self, line0: usize) -> Option<(usize, usize)> {
        if self.rope.is_some() || self.piece_table.is_some() {
            return None;
        }
        let checkpoint = self.disk_index.as_ref()?.checkpoint_for_line(line0)?;
        Some((checkpoint.line0, checkpoint.byte0))
    }

    pub(super) fn estimated_mmap_line_byte_range(&self, line0: usize) -> Option<(usize, usize)> {
        let bytes = self.mmap_bytes();
        let file_len = self.file_len.min(bytes.len());
        if bytes.is_empty() || file_len == 0 {
            return None;
        }
        if let Some(total_lines) = self.disk_index_total_lines() {
            if line0 >= total_lines {
                return None;
            }
        }

        let avg_line_len = self.avg_line_len();
        let offsets = self.line_offsets.read().ok();
        let approx = if let Some(offsets) = offsets.as_deref() {
            if let Some(start0) = offsets.get_usize(line0) {
                start0
            } else if let Some((anchor_line0, anchor_byte0)) =
                self.disk_index_checkpoint_for_line(line0)
            {
                anchor_byte0.saturating_add(
                    line0
                        .saturating_sub(anchor_line0)
                        .saturating_mul(avg_line_len.max(1)),
                )
            } else {
                let anchor_line0 = offsets.len().saturating_sub(1);
                let anchor_byte0 = offsets.get_usize(anchor_line0).unwrap_or(0);
                anchor_byte0.saturating_add(
                    line0
                        .saturating_sub(anchor_line0)
                        .saturating_mul(avg_line_len.max(1)),
                )
            }
        } else if let Some((anchor_line0, anchor_byte0)) =
            self.disk_index_checkpoint_for_line(line0)
        {
            anchor_byte0.saturating_add(
                line0
                    .saturating_sub(anchor_line0)
                    .saturating_mul(avg_line_len.max(1)),
            )
        } else {
            line0.saturating_mul(avg_line_len.max(1))
        }
        .min(file_len.saturating_sub(1));

        let back_limit = approx.saturating_sub(APPROX_LINE_BACKTRACK_BYTES);
        let start0 = if approx == 0 {
            0
        } else {
            let back_slice = &bytes[back_limit..approx];
            if let Some(rel) = back_slice.iter().rposition(|b| matches!(*b, b'\n' | b'\r')) {
                let idx = back_limit + rel;
                if bytes[idx] == b'\r' && idx + 1 < file_len && bytes[idx + 1] == b'\n' {
                    idx + 2
                } else {
                    idx + 1
                }
            } else {
                back_limit
            }
        };

        let forward_limit = approx
            .saturating_add(APPROX_LINE_FORWARD_BYTES)
            .min(file_len);
        let start0 = start0.min(forward_limit);
        let forward_slice = &bytes[start0..forward_limit];
        let end0 = if let Some(rel) = memchr::memchr2(b'\n', b'\r', forward_slice) {
            let idx = start0 + rel;
            if bytes[idx] == b'\r' && idx + 1 < file_len && bytes[idx + 1] == b'\n' {
                idx + 2
            } else {
                idx + 1
            }
        } else {
            forward_limit
        };

        Some((start0.min(end0), end0.max(start0)))
    }

    /// Returns the memory-mapped bytes of the original backing file.
    ///
    /// For edited documents, this may still expose the original file contents
    /// rather than the post-edit text.
    pub fn mmap_bytes(&self) -> &[u8] {
        let Some(storage) = &self.storage else {
            return &[];
        };
        storage.read_range(0, storage.len())
    }

    /// Returns the line count without heuristic extrapolation from average line length.
    pub(super) fn bounded_line_count(&self) -> usize {
        if let Some(piece_table) = &self.piece_table {
            return piece_table.line_count().max(1);
        }
        if let Some(rope) = &self.rope {
            return rope.len_lines().max(1);
        }
        if let Some(total_lines) = self.disk_index_total_lines() {
            return total_lines.max(1);
        }
        if let Ok(guard) = self.line_offsets.read() {
            guard.len().max(1)
        } else {
            1
        }
    }

    /// Returns an estimated line count that is useful while background indexing is in progress.
    pub(super) fn estimated_line_count_value(&self) -> usize {
        if self.has_precise_line_lengths() {
            return self.exact_line_count().unwrap_or(1);
        }
        if let Some(total_lines) = self.disk_index_total_lines() {
            return total_lines.max(1);
        }

        let estimate = if self.file_len() == 0 {
            1
        } else {
            self.file_len().div_ceil(self.avg_line_len().max(1)).max(1)
        };
        let offsets_rows = if let Ok(guard) = self.line_offsets.read() {
            guard.len().max(1)
        } else {
            1
        };
        let piece_rows = self
            .piece_table
            .as_ref()
            .map(|piece_table| piece_table.line_count().max(1))
            .unwrap_or(1);

        estimate.max(offsets_rows).max(piece_rows)
    }

    fn exact_line_count_value(&self) -> Option<usize> {
        if let Some(piece_table) = &self.piece_table {
            if let Some(lines) =
                piece_table.exact_line_count_with_fallback(self.raw_disk_index_total_lines())
            {
                return Some(lines.max(1));
            }
        }
        if let Some(rope) = &self.rope {
            return Some(rope.len_lines().max(1));
        }
        if let Some(total_lines) = self.disk_index_total_lines() {
            return Some(total_lines.max(1));
        }
        if self.is_fully_indexed() {
            return Some(self.bounded_line_count().max(1));
        }
        None
    }

    /// Returns the exact document line count when it is known.
    pub fn exact_line_count(&self) -> Option<usize> {
        self.exact_line_count_value()
    }

    /// Returns the current document line count, explicitly distinguishing exact
    /// values from scrolling estimates.
    pub fn line_count(&self) -> LineCount {
        if let Some(lines) = self.exact_line_count_value() {
            LineCount::Exact(lines)
        } else {
            LineCount::Estimated(self.estimated_line_count_value())
        }
    }

    /// Returns the current best-effort line count for viewport sizing and scrolling.
    pub fn display_line_count(&self) -> usize {
        self.line_count().display_rows()
    }

    /// Returns `true` when [`Document::line_count`] is already exact.
    pub fn is_line_count_exact(&self) -> bool {
        self.line_count().is_exact()
    }

    /// Returns the current document backing mode.
    pub fn backing(&self) -> DocumentBacking {
        if self.has_rope() {
            DocumentBacking::Rope
        } else if self.has_piece_table() {
            DocumentBacking::PieceTable
        } else {
            DocumentBacking::Mmap
        }
    }

    /// Returns a frontend-friendly snapshot of the current document state.
    pub fn status(&self) -> DocumentStatus {
        DocumentStatus::new(
            self.path.clone(),
            self.is_dirty(),
            self.file_len(),
            self.line_count(),
            self.is_line_count_pending(),
            self.line_ending(),
            self.encoding(),
            self.preserve_save_error(),
            self.encoding_origin(),
            self.decoding_had_errors(),
            self.indexing_state(),
            self.backing(),
        )
    }

    pub(super) fn invalidate_preserve_save_error_cache(&self) {
        self.preserve_save_error_cache.set(None);
    }

    pub(super) fn mark_dirty(&mut self) {
        self.invalidate_preserve_save_error_cache();
        self.dirty = true;
    }

    /// Returns a maintenance-focused snapshot using the default compaction policy.
    ///
    /// This is intentionally separate from [`Document::status`] because
    /// fragmentation and compaction advice may require traversing piece-table
    /// state and are heavier than ordinary frontend polling fields.
    pub fn maintenance_status(&self) -> DocumentMaintenanceStatus {
        self.maintenance_status_with_policy(CompactionPolicy::default())
    }

    /// Returns the high-level maintenance action suggested by the default policy.
    pub fn maintenance_action(&self) -> MaintenanceAction {
        self.maintenance_status().recommended_action()
    }

    /// Returns a maintenance-focused snapshot using a caller-provided compaction policy.
    pub fn maintenance_status_with_policy(
        &self,
        policy: CompactionPolicy,
    ) -> DocumentMaintenanceStatus {
        DocumentMaintenanceStatus::new(
            self.backing(),
            self.fragmentation_stats_with_threshold(policy.small_piece_threshold_bytes),
            self.compaction_recommendation_with_policy(policy),
        )
    }

    /// Returns the high-level maintenance action suggested by a caller-provided policy.
    pub fn maintenance_action_with_policy(&self, policy: CompactionPolicy) -> MaintenanceAction {
        self.maintenance_status_with_policy(policy)
            .recommended_action()
    }

    /// Returns the current document length in bytes.
    pub fn file_len(&self) -> usize {
        if let Some(piece_table) = &self.piece_table {
            return piece_table.total_len();
        }
        if let Some(rope) = &self.rope {
            if self.encoding.is_utf8() {
                return rope_save_len_bytes(rope, self.line_ending);
            }
            let rendered = rope_text_with_line_endings(rope, self.line_ending);
            return encode_text_with_encoding(&rendered, self.encoding)
                .map(|bytes| bytes.len())
                .unwrap_or_else(|_| rendered.len());
        }
        self.file_len
    }

    /// Returns the currently detected line ending style for the document.
    pub fn line_ending(&self) -> LineEnding {
        self.line_ending
    }

    /// Returns the current explicit or inherited encoding contract for the document.
    pub fn encoding(&self) -> DocumentEncoding {
        self.encoding
    }

    pub(super) fn preserve_save_materializes_lossy_decoded_text(&self) -> bool {
        self.decoding_had_errors && !(self.encoding.is_utf8() && self.rope.is_none())
    }

    fn save_reopen_size_error(
        &self,
        encoding: DocumentEncoding,
        encoded_len: usize,
    ) -> Option<DocumentEncodingErrorKind> {
        let reload_after_save = !self.has_edit_buffer() || self.has_piece_table();
        (reload_after_save && !encoding.is_utf8() && encoded_len > MAX_ROPE_EDIT_FILE_BYTES)
            .then_some(DocumentEncodingErrorKind::SaveReopenTooLarge {
                max_bytes: MAX_ROPE_EDIT_FILE_BYTES,
            })
    }

    fn rendered_text_for_save_validation(&self) -> String {
        if let Some(rope) = &self.rope {
            return rope_text_with_line_endings(rope, self.line_ending);
        }
        if let Some(piece_table) = &self.piece_table {
            return piece_table.to_string_lossy();
        }
        String::from_utf8_lossy(self.mmap_bytes()).to_string()
    }

    /// Returns the typed reason why preserve-save would currently fail, if any.
    pub fn preserve_save_error(&self) -> Option<DocumentEncodingErrorKind> {
        if let Some(cached) = self.preserve_save_error_cache.get() {
            return cached;
        }

        let computed = if !self.encoding.can_roundtrip_save() {
            Some(DocumentEncodingErrorKind::PreserveSaveUnsupported)
        } else if self.preserve_save_materializes_lossy_decoded_text() {
            Some(DocumentEncodingErrorKind::LossyDecodedPreserve)
        } else if self.encoding.is_utf8() {
            None
        } else {
            let rendered = self.rendered_text_for_save_validation();
            match encode_text_with_encoding(&rendered, self.encoding) {
                Ok(bytes) => self.save_reopen_size_error(self.encoding, bytes.len()),
                Err(err) => Some(err),
            }
        };

        self.preserve_save_error_cache.set(Some(computed));
        computed
    }

    /// Returns `true` when preserve-save is currently allowed for this document.
    pub fn can_preserve_save(&self) -> bool {
        self.preserve_save_error().is_none()
    }

    /// Returns the typed reason why the requested save options would currently fail, if any.
    ///
    /// For explicit save conversions this may materialize the current document
    /// text to validate representability in the target encoding.
    pub fn save_error_for_options(
        &self,
        options: DocumentSaveOptions,
    ) -> Option<DocumentEncodingErrorKind> {
        match options.encoding_policy() {
            SaveEncodingPolicy::Preserve => self.preserve_save_error(),
            SaveEncodingPolicy::Convert(encoding) => {
                let rendered = self.rendered_text_for_save_validation();
                match encode_text_with_encoding(&rendered, encoding) {
                    Ok(bytes) => self.save_reopen_size_error(encoding, bytes.len()),
                    Err(err) => Some(err),
                }
            }
        }
    }

    /// Returns `true` when the requested save options are currently valid.
    pub fn can_save_with_options(&self, options: DocumentSaveOptions) -> bool {
        self.save_error_for_options(options).is_none()
    }

    /// Returns the typed reason why an explicit save conversion would currently fail, if any.
    pub fn save_error_for_encoding(
        &self,
        encoding: DocumentEncoding,
    ) -> Option<DocumentEncodingErrorKind> {
        self.save_error_for_options(DocumentSaveOptions::new().with_encoding(encoding))
    }

    /// Returns `true` when saving through the given explicit encoding is currently valid.
    pub fn can_save_with_encoding(&self, encoding: DocumentEncoding) -> bool {
        self.save_error_for_encoding(encoding).is_none()
    }

    /// Returns how the current encoding contract was chosen.
    pub fn encoding_origin(&self) -> DocumentEncodingOrigin {
        self.encoding_origin
    }

    /// Returns `true` when opening the source required replacement characters.
    pub fn decoding_had_errors(&self) -> bool {
        self.decoding_had_errors
    }

    /// Returns the full document text, applying lossy UTF-8 decoding when needed.
    ///
    /// This materializes the entire current document into a fresh `String`.
    /// It is an advanced convenience helper rather than the recommended
    /// frontend path. Prefer viewport or typed range reads for large-file
    /// frontends that only need a visible window or a bounded selection.
    pub fn text_lossy(&self) -> String {
        if let Some(rope) = &self.rope {
            return rope.to_string();
        }
        if let Some(piece_table) = &self.piece_table {
            return piece_table.to_string_lossy();
        }
        String::from_utf8_lossy(self.mmap_bytes()).to_string()
    }
}
