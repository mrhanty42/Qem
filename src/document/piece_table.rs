//! `impl PieceTable`: constructors, session persistence, the piece-tree
//! line model, byte reads, text edits, and undo/redo.

use super::*;

/// Reported when a far-line edit cannot be resolved because the disk line
/// index is missing or does not cover the requested line.
pub(super) fn edit_span_unresolvable_error() -> DocumentError {
    DocumentError::EditUnsupported {
        path: None,
        reason: editing::edit_span_unresolvable_reason(),
    }
}

impl PieceTable {
    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) fn new(original: FileStorage, line_lengths: Vec<usize>, full_index: bool) -> Self {
        Self::new_with_encoding_state(
            original,
            line_lengths,
            full_index,
            None,
            DocumentEncodingOrigin::Utf8FastPath,
            false,
        )
    }

    pub(super) fn new_with_encoding_state(
        original: FileStorage,
        line_lengths: Vec<usize>,
        full_index: bool,
        exact_base_total_lines: Option<usize>,
        encoding_origin: DocumentEncodingOrigin,
        decoding_had_errors: bool,
    ) -> Self {
        Self::new_with_encoding_state_for_engine(
            original,
            line_lengths,
            full_index,
            exact_base_total_lines,
            encoding_origin,
            decoding_had_errors,
            DocumentEncoding::utf8(),
        )
    }

    /// Builds the piece table with the document's engine wired in, so
    /// column / text-unit arithmetic and line decoding stay encoding-aware
    /// for non-UTF-8 documents. `encoding` is the document's current
    /// encoding contract; it is persisted through the session sidecar so
    /// reopened sessions restore the same encoding.
    #[allow(clippy::too_many_arguments)] // mirrors the encoding-state constructor shape
    pub(super) fn new_with_encoding_state_for_engine(
        original: FileStorage,
        mut line_lengths: Vec<usize>,
        full_index: bool,
        exact_base_total_lines: Option<usize>,
        encoding_origin: DocumentEncodingOrigin,
        decoding_had_errors: bool,
        encoding: DocumentEncoding,
    ) -> Self {
        let total_len = original.len();
        if line_lengths.is_empty() {
            line_lengths.push(total_len);
        }
        let known_byte_len = line_lengths.iter().copied().sum::<usize>().min(total_len);
        let known_line_count = line_lengths.len().max(1);
        let pieces = Self::build_initial_piece_tree(
            &original,
            total_len,
            &line_lengths,
            known_byte_len,
            full_index,
        );
        let exact_base_line_breaks = pieces.total_line_breaks();

        let mut this = Self {
            original,
            add: Vec::new(),
            pieces,
            engine: encoding_engine::engine_for_encoding(encoding),
            known_line_count,
            exact_base_line_breaks: Some(exact_base_line_breaks),
            exact_base_total_lines: exact_base_total_lines
                .or(full_index.then_some(known_line_count)),
            known_byte_len,
            total_len,
            full_index,
            encoding,
            encoding_origin,
            decoding_had_errors,
            pending_session_flush: false,
            pending_session_edits: 0,
            last_session_flush: None,
            edit_batch_depth: 0,
            edit_batch_dirty: false,
        };
        this.pieces
            .set_known_boundary(this.known_byte_len, this.full_index);
        this
    }

    /// Rebuilds a piece table from a persisted session. The engine is
    /// derived from the encoding stored in the session metadata; sessions
    /// flushed before the v3 sidecar format carried no encoding and are
    /// treated as UTF-8, matching the historical recovery behavior.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) fn from_recovered_session(
        original: FileStorage,
        add: Vec<u8>,
        pieces: PieceTree,
        meta: SessionMeta,
    ) -> Self {
        let encoding = meta.encoding.unwrap_or_default();
        Self::from_recovered_session_with_engine(
            original,
            add,
            pieces,
            meta,
            encoding_engine::engine_for_encoding(encoding),
        )
    }

    pub(super) fn from_recovered_session_with_engine(
        original: FileStorage,
        add: Vec<u8>,
        pieces: PieceTree,
        meta: SessionMeta,
        engine: &'static dyn encoding_engine::EncodingEngine,
    ) -> Self {
        let total_len = pieces.total_len();
        let known_line_count = pieces
            .total_line_breaks()
            .saturating_add(usize::from(meta.full_index))
            .max(1);
        let known_byte_len = meta.known_byte_len.min(total_len);

        let mut this = Self {
            original,
            add,
            pieces,
            engine,
            known_line_count,
            exact_base_line_breaks: meta
                .full_index
                .then_some(known_line_count.saturating_sub(1)),
            exact_base_total_lines: meta.full_index.then_some(known_line_count),
            known_byte_len,
            total_len,
            full_index: meta.full_index,
            encoding: meta.encoding.unwrap_or_default(),
            encoding_origin: meta
                .encoding_origin
                .unwrap_or(DocumentEncodingOrigin::Utf8FastPath),
            decoding_had_errors: meta.decoding_had_errors,
            pending_session_flush: false,
            pending_session_edits: 0,
            last_session_flush: None,
            edit_batch_depth: 0,
            edit_batch_dirty: false,
        };
        this.pieces
            .set_known_boundary(this.known_byte_len, this.full_index);
        this
    }

    #[allow(clippy::too_many_arguments)] // mirrors the encoding-state constructor shape
    pub(super) fn from_annotated_pieces_with_engine(
        original: FileStorage,
        pieces: Vec<Piece>,
        known_byte_len: usize,
        known_line_count: usize,
        exact_base_total_lines: Option<usize>,
        encoding_origin: DocumentEncodingOrigin,
        decoding_had_errors: bool,
        encoding: DocumentEncoding,
        engine: &'static dyn encoding_engine::EncodingEngine,
    ) -> Self {
        let total_len: usize = pieces.iter().map(|piece| piece.len).sum();
        let pieces = if total_len >= PIECE_TREE_DISK_MIN_BYTES {
            PieceTree::from_pieces_disk(original.path(), pieces.clone())
                .unwrap_or_else(|_| PieceTree::from_pieces(pieces))
        } else {
            PieceTree::from_pieces(pieces)
        };
        Self {
            original,
            add: Vec::new(),
            pieces,
            engine,
            known_line_count,
            exact_base_line_breaks: Some(known_line_count),
            exact_base_total_lines,
            known_byte_len: known_byte_len.min(total_len),
            total_len,
            full_index: false,
            encoding,
            encoding_origin,
            decoding_had_errors,
            pending_session_flush: false,
            pending_session_edits: 0,
            last_session_flush: None,
            edit_batch_depth: 0,
            edit_batch_dirty: false,
        }
    }

    /// Appends checkpoint-annotated pieces covering the document span from
    /// the end of the known region through the interval that contains
    /// `line0`, and splices them into the piece tree. The span must be
    /// pristine original content (edits only ever happen inside the known
    /// region, which `has_line` enforces at the edit gates). Returns the
    /// number of line breaks the annotation added.
    pub(super) fn splice_annotated_pieces(
        &mut self,
        engine: &dyn encoding_engine::EncodingEngine,
        disk: &crate::index::DiskLineIndex,
        line0: usize,
    ) -> Result<usize, DocumentError> {
        // The byte walks below operate on the original storage bytes, so the
        // walk bound is the original length, not the (post-edit) doc length.
        let file_len = self.original.len().min(self.total_len);
        let bytes = self.original.read_range(0, file_len);
        let known_byte_len = self.known_byte_len;
        let known_line_count = self.line_count();

        // The first piece at or after the known-region boundary gives the
        // constant doc→original offset of the pristine span.
        let pieces = self.pieces.to_vec();
        let mut consumed = 0usize;
        let mut span_delta: i128 = 0;
        let mut span_found = false;
        for piece in &pieces {
            let piece_end = consumed.saturating_add(piece.len);
            if piece_end > known_byte_len {
                span_delta = piece.start as i128 - consumed as i128;
                span_found = true;
                break;
            }
            consumed = piece_end;
        }
        if !span_found || known_byte_len as i128 + span_delta < 0 {
            return Err(edit_span_unresolvable_error());
        }
        let orig_from = (known_byte_len as i128 + span_delta) as usize;
        if orig_from >= file_len {
            return Err(edit_span_unresolvable_error());
        }

        // The original line containing `orig_from` anchors the walk; a
        // pristine span maps document lines onto original lines 1:1.
        let Some(anchor) = disk.checkpoint_for_byte(orig_from) else {
            return Err(edit_span_unresolvable_error());
        };
        let mut line = anchor.line0;
        let mut byte = anchor.byte0;
        while byte < file_len && byte < orig_from {
            let next = engine.next_line_start(bytes, file_len, byte);
            if next <= byte {
                return Err(edit_span_unresolvable_error());
            }
            byte = next;
            line += 1;
        }
        let anchor_line = line;
        // Document lines inside the known region include the lines added or
        // removed by earlier edits (`known_line_count` is refreshed from the
        // piece tree after every edit), so the document→original line delta
        // at the boundary is `known_line_count - 1 - anchor_line`: the doc
        // line starting at the boundary has index `known_line_count - 1` and
        // corresponds to the original line starting at (or containing)
        // `orig_from`.
        let target_orig_line = anchor_line + (line0 - known_line_count) + 1;
        if let Some(total_lines) = disk.total_lines() {
            if target_orig_line >= total_lines {
                return Err(edit_span_unresolvable_error());
            }
        }
        let Some((new_pieces, span_breaks, window_end_orig)) = checkpoint_pieces_up_to(
            bytes,
            file_len,
            engine,
            disk,
            orig_from,
            anchor_line,
            target_orig_line,
        ) else {
            return Err(edit_span_unresolvable_error());
        };

        // Pristine-span check: every piece overlapping the doc range
        // [known_byte_len .. target_doc_byte] must be original content with
        // a contiguous original mapping, so the constant `span_delta` holds
        // across the whole span.
        let target_doc_byte = (window_end_orig as i128 - span_delta) as usize;
        let mut expected_orig: Option<usize> = None;
        let mut pristine = true;
        self.pieces.visit_range_while(
            known_byte_len,
            target_doc_byte,
            |piece, local_start, local_end| {
                if !pristine {
                    return false;
                }
                if piece.src != PieceSource::Original {
                    pristine = false;
                    return false;
                }
                let orig_start = piece.start + local_start;
                match expected_orig {
                    None => {
                        if orig_start as i128 - (known_byte_len as i128 + local_start as i128)
                            != span_delta
                        {
                            pristine = false;
                            return false;
                        }
                    }
                    Some(expected) => {
                        if expected != orig_start {
                            pristine = false;
                            return false;
                        }
                    }
                }
                expected_orig = Some(piece.start + local_end);
                true
            },
        );
        if !pristine {
            return Err(edit_span_unresolvable_error());
        }

        // The last piece is the pristine tail covering
        // [known_byte_len .. total_len): the annotated pieces replace it, and
        // a new pristine tail continues after the annotated region. Appending
        // after the tail would place the pieces out of document order.
        let mut all = self.pieces.to_vec();
        let tail = all.pop();
        match tail {
            Some(piece) if piece.src == PieceSource::Original => {
                let tail_doc_start = self.total_len.saturating_sub(piece.len);
                if tail_doc_start != known_byte_len {
                    return Err(edit_span_unresolvable_error());
                }
            }
            _ => return Err(edit_span_unresolvable_error()),
        }
        all.extend(new_pieces);
        let window_end_doc = (window_end_orig as i128 - span_delta) as usize;
        if window_end_doc < self.total_len {
            all.push(Piece {
                src: PieceSource::Original,
                start: window_end_orig,
                len: self.total_len - window_end_doc,
                line_breaks: 0,
            });
        }
        // The splice rebuilds the root from fresh pages, so it can be
        // refused while persistence is down and the resident window is
        // exhausted. Surface the refusal as a write error and leave the
        // piece tree untouched: landing a root that references pages
        // reads would silently skip would truncate the document behind
        // `total_len`.
        let sidecar = session_sidecar_path(None, self.original.path());
        self.pieces
            .replace_current_root_with_pieces(all)
            .map_err(|source| DocumentError::Write {
                path: sidecar,
                source,
            })?;
        self.known_byte_len = window_end_doc;
        self.known_line_count += span_breaks;
        self.exact_base_line_breaks = self
            .exact_base_line_breaks
            .map(|base| base.saturating_add(span_breaks));
        Ok(span_breaks)
    }

    pub(crate) fn line_count(&self) -> usize {
        self.known_line_count.max(1)
    }

    #[cfg(test)]
    pub(super) fn pieces_snapshot(&self) -> Vec<Piece> {
        self.pieces.to_vec()
    }

    pub(crate) fn total_len(&self) -> usize {
        self.total_len
    }

    /// The document's byte-movement engine, for sibling-module callers
    /// (search / reads) that must step cells or decode windows in the
    /// document's own encoding.
    pub(crate) fn engine(&self) -> &'static dyn encoding_engine::EncodingEngine {
        self.engine
    }

    /// Returns the byte length covered by the known line boundaries.
    ///
    /// A partial piece table indexes only a bounded window of lines, so this
    /// can be far smaller than [`Self::total_len`]; the tail of the document
    /// has no per-line bookkeeping until the table is widened.
    pub(crate) fn known_byte_len(&self) -> usize {
        self.known_byte_len
    }

    pub(crate) fn full_index(&self) -> bool {
        self.full_index
    }

    pub(crate) fn exact_line_count_with_fallback(
        &self,
        fallback_total_lines: Option<usize>,
    ) -> Option<usize> {
        if self.full_index {
            return Some(self.line_count().max(1));
        }

        let base_breaks = self.exact_base_line_breaks?;
        let base_total_lines = self.exact_base_total_lines.or(fallback_total_lines)?.max(1);
        let current_breaks = self.pieces.total_line_breaks();
        if current_breaks >= base_breaks {
            Some(
                base_total_lines
                    .saturating_add(current_breaks - base_breaks)
                    .max(1),
            )
        } else {
            Some(
                base_total_lines
                    .saturating_sub(base_breaks - current_breaks)
                    .max(1),
            )
        }
    }

    pub(crate) fn fragmentation_stats(&self) -> FragmentationStats {
        self.pieces.fragmentation_stats()
    }

    pub(crate) fn fragmentation_stats_with_threshold(
        &self,
        small_piece_threshold_bytes: usize,
    ) -> FragmentationStats {
        self.pieces
            .fragmentation_stats_with_threshold(small_piece_threshold_bytes)
    }

    pub(super) fn session_meta(&self) -> SessionMeta {
        SessionMeta {
            known_byte_len: self.known_byte_len,
            full_index: self.full_index,
            encoding: Some(self.encoding),
            encoding_origin: Some(self.encoding_origin),
            decoding_had_errors: self.decoding_had_errors,
        }
    }

    pub(super) fn flush_session(&mut self) -> io::Result<()> {
        self.flush_session_inner(true)
    }

    pub(super) fn schedule_session_flush(&mut self) -> io::Result<()> {
        self.pending_session_flush = true;
        self.pending_session_edits = self.pending_session_edits.saturating_add(1);
        if self.edit_batch_depth > 0 {
            self.edit_batch_dirty = true;
            return Ok(());
        }
        self.flush_session_inner(false)
    }

    fn flush_session_inner(&mut self, force: bool) -> io::Result<()> {
        if !force && !self.pending_session_flush {
            return Ok(());
        }
        if !force {
            let debounce_elapsed = self
                .last_session_flush
                .map(|instant| instant.elapsed() >= PIECE_SESSION_FLUSH_DEBOUNCE)
                .unwrap_or(true);
            if !debounce_elapsed && self.pending_session_edits < PIECE_SESSION_FORCE_AFTER_EDITS {
                return Ok(());
            }
        }

        match self.pieces.flush_session(&self.add, self.session_meta()) {
            Ok(()) => {
                self.pending_session_flush = false;
                self.pending_session_edits = 0;
                self.last_session_flush = Some(Instant::now());
                Ok(())
            }
            Err(err) => {
                self.pending_session_flush = false;
                self.pending_session_edits = 0;
                self.last_session_flush = None;
                self.pieces.detach_persistence();
                if force {
                    Err(err)
                } else {
                    Ok(())
                }
            }
        }
    }

    pub(super) fn begin_edit_batch(&mut self) {
        self.edit_batch_depth = self.edit_batch_depth.saturating_add(1);
        self.pieces.begin_batch_edit();
    }

    pub(super) fn end_edit_batch(&mut self) -> io::Result<()> {
        if self.edit_batch_depth == 0 {
            return Ok(());
        }

        self.edit_batch_depth -= 1;
        self.pieces.end_batch_edit();
        if self.edit_batch_depth == 0 && self.edit_batch_dirty {
            self.edit_batch_dirty = false;
            self.flush_session_inner(false)?;
        }
        Ok(())
    }

    pub(crate) fn has_line(&self, line0: usize) -> bool {
        line0 < self.line_count()
    }

    pub(crate) fn line_len_chars(&self, line0: usize) -> usize {
        self.line_len_chars_with_engine(line0, self.engine)
    }

    /// Encoding-aware variant of [`PieceTable::line_len_chars`].
    ///
    /// Counts text-unit columns of `line0` by walking the line's contiguous
    /// byte range through the encoding engine instead of `utf8_step`. The
    /// piece-tree may split a single line across multiple `Piece`s, so we
    /// materialize the line bytes into a contiguous buffer once and then
    /// drive the engine's `count_columns_exact` (which already stops at
    /// the first line-ending cell of the engine's encoding).
    ///
    /// Used by the non-UTF-8 edit paths (`try_replace_range`,
    /// `try_delete_range_at_encoded`, `try_backspace_at`) so column /
    /// text-unit arithmetic over piece-tree storage matches the
    /// document's target encoding.
    pub(crate) fn line_len_chars_with_engine(
        &self,
        line0: usize,
        engine: &dyn encoding_engine::EncodingEngine,
    ) -> usize {
        let (start, end) = self.line_range(line0);
        if start >= end {
            return 0;
        }
        let bytes = self.read_range(start, end);
        engine.count_columns_exact(&bytes)
    }

    pub(crate) fn line_visible_segment(
        &self,
        line0: usize,
        start_col: usize,
        max_cols: usize,
    ) -> String {
        if max_cols == 0 || line0 >= self.line_count() {
            return String::new();
        }
        let Some(line_start) = self.line_start_byte(line0) else {
            return String::new();
        };
        let line_end = self.line_range(line0).1;
        if line_start >= line_end {
            return String::new();
        }
        let start = self.byte_offset_for_col(line0, start_col);
        if start >= line_end {
            return String::new();
        }

        // Collect the requested columns as raw bytes and let the
        // document's engine decode them. Decoding the bytes as UTF-8
        // here corrupted every non-UTF-8 document: a windows-1251 or
        // UTF-16 cell is not valid UTF-8, so the whole line came back
        // as U+FFFD replacement characters.
        let utf16_cells = utf16_break_cells(self.engine.encoding().name());
        let mut out = Vec::with_capacity(max_cols.min(4096).saturating_mul(4));
        let mut cols = 0usize;
        self.pieces
            .visit_range_while(start, line_end, |piece, local_start, local_end| {
                if cols >= max_cols {
                    return false;
                }

                let src = self.source_bytes(piece.src);
                let (seg_start, seg_end) =
                    self.clamped_piece_segment(piece, local_start, local_end, src.len());
                let mut i = seg_start;

                while i < seg_end {
                    let step = self.engine.step(src, i, seg_end);
                    if step == 0 || i.saturating_add(step) > seg_end {
                        // Known limitation: a piece boundary can split an encoded
                        // cell (e.g. the halves of a UTF-16 unit), and this
                        // guard stops the walk there instead of spinning on
                        // a zero step or decoding a partial cell. For Class
                        // A the guard never fires (a cell is one byte), so
                        // single-byte documents are exact; for UTF-16/CJK
                        // this can end a line segment early - the full fix
                        // is the engine-aware line model, not this guard.
                        return false;
                    }
                    // The line break is a WHOLE encoding cell: a raw byte
                    // check fires on the first byte of a non-break UTF-16LE
                    // cell like U+4E0A (`[0x0A, 0x4E]`) and truncates the
                    // visible segment mid-content.
                    let cell = &src[i..i + step];
                    let is_break = match utf16_cells {
                        Some((lf, cr)) => cell == lf.as_slice() || cell == cr.as_slice(),
                        None => cell == b"\n" || cell == b"\r",
                    };
                    if is_break {
                        return false;
                    }
                    out.extend_from_slice(cell);
                    cols += 1;
                    if cols >= max_cols {
                        return false;
                    }
                    i += step;
                }

                true
            });

        self.decode_bytes(&out)
    }

    /// Decodes a byte buffer with the document's engine. UTF-8 documents
    /// keep the strict UTF-8 path (lossy only on genuinely invalid
    /// bytes); every other encoding goes through the encoding crate's
    /// decoder so the text is interpreted in the document's own charset.
    fn decode_bytes(&self, bytes: &[u8]) -> String {
        if self.engine.encoding().is_utf8() {
            return String::from_utf8_lossy(bytes).into_owned();
        }
        let (decoded, _had_errors) = self
            .engine
            .encoding()
            .as_encoding()
            .decode_with_bom_removal(bytes);
        decoded.into_owned()
    }

    pub(crate) fn line_slices_exact(
        &self,
        first_line0: usize,
        line_count: usize,
        start_col: usize,
        max_cols: usize,
    ) -> Vec<LineSlice> {
        if line_count == 0 {
            return Vec::new();
        }
        if first_line0 >= self.line_count() {
            return vec![LineSlice::new(String::new(), true); line_count];
        }

        let available = self
            .line_count()
            .saturating_sub(first_line0)
            .min(line_count);
        let mut slices = Vec::with_capacity(line_count);
        for offset in 0..available {
            let line0 = first_line0.saturating_add(offset);
            slices.push(LineSlice::new(
                self.line_visible_segment(line0, start_col, max_cols),
                true,
            ));
        }
        slices.resize(line_count, LineSlice::new(String::new(), true));
        slices
    }

    pub(crate) fn insert_text_at(
        &mut self,
        line_ending: LineEnding,
        line0: usize,
        col0: usize,
        text: &str,
    ) -> io::Result<EditOutcome> {
        let actual_col0 = self.line_len_chars(line0);
        let insert_col0 = col0.min(actual_col0);
        let virtual_padding_cols = col0.saturating_sub(actual_col0);
        let insert_at = self.byte_offset_for_col(line0, insert_col0);
        let (normalized, added_lines, last_col) =
            normalize_insert_text(text, virtual_padding_cols, line_ending);

        let bytes = normalized.as_bytes();
        if !bytes.is_empty() {
            self.insert_bytes(insert_at, bytes)?;
        }

        let cursor = if added_lines == 0 {
            (line0, col0.saturating_add(last_col))
        } else {
            (line0.saturating_add(added_lines), last_col)
        };
        Ok(EditOutcome::new(!bytes.is_empty(), cursor))
    }

    /// Inserts raw `encoded` bytes into the piece-tree add buffer at
    /// `byte_offset` without any UTF-8 validation or normalization.
    ///
    /// This is the non-UTF-8 edit path. The caller is expected to have
    /// already encoded the user's `&str` into the document's target
    /// encoding (Class A bytes, UTF-16 code units, or CJK multibyte
    /// sequences) and to have aligned `byte_offset` to a character
    /// boundary of that encoding — this method itself performs no
    /// alignment and no encoding checks.
    ///
    /// Line breaks inside `encoded` are counted by raw `0x0A` / `0x0D`
    /// scans. For ASCII-superset encodings (Class A, UTF-8) this is exact;
    /// for UTF-16 LE/BE the count is approximate (a `0x0A` byte landing at
    /// an odd position within a 2-byte code unit is not a real line break),
    /// but the engine-aware read paths use the document's `LineOffsets`
    /// cache rather than this internal counter to compute logical
    /// line/column positions.
    ///
    /// Returns an [`EditOutcome`] whose `cursor` is currently a stub
    /// `(0, 0)`; computing the post-edit cursor in the document's target
    /// encoding is the caller's responsibility.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn insert_encoded_bytes_at(
        &mut self,
        byte_offset: usize,
        encoded: &[u8],
    ) -> io::Result<EditOutcome> {
        if encoded.is_empty() {
            return Ok(EditOutcome::new(false, (0, 0)));
        }

        let insert_at = byte_offset.min(self.total_len);
        self.insert_bytes(insert_at, encoded)?;

        Ok(EditOutcome::new(true, (0, 0)))
    }

    /// Single-transaction replace: deletes the byte range
    /// `[start, end)` and inserts `encoded` at `start`, batching both
    /// edits so the session flush sees one transaction.
    ///
    /// Returns `true` when the document state changed (the existing
    /// bytes differed from `encoded` or the range was non-empty).
    /// Like [`PieceTable::insert_encoded_bytes_at`], no UTF-8
    /// validation or normalization is performed; the caller is
    /// responsible for aligning `start` / `end` onto character
    /// boundaries of the document's target encoding and for encoding
    /// `encoded` in that encoding.
    pub(crate) fn replace_encoded_bytes_at(
        &mut self,
        start: usize,
        end: usize,
        encoded: &[u8],
    ) -> Result<bool, ReplaceBatchError> {
        let start = start.min(self.total_len);
        let end = end.min(self.total_len).max(start);
        if start == end && encoded.is_empty() {
            return Ok(false);
        }
        // Short-circuit no-op replace: the byte range is already equal
        // to `encoded`. This keeps `dirty` clean for round-trip edits
        // that do not actually change content.
        if end > start {
            let existing = self.read_range(start, end);
            if existing == encoded {
                return Ok(false);
            }
        }

        self.begin_edit_batch();
        let mut deleted = false;
        let result = (|| -> io::Result<()> {
            if end > start {
                self.delete_range(start, end - start)?;
                deleted = true;
            }
            if !encoded.is_empty() {
                let insert_at = start.min(self.total_len);
                self.insert_bytes(insert_at, encoded)?;
            }
            Ok(())
        })();
        let end_batch = self.end_edit_batch();
        if let Err(source) = result {
            // A refused insert after a landed delete still changed the
            // document: the deletion is committed to the tree and stays
            // undoable, so the caller must keep the document dirty
            // instead of treating the batch as a no-op.
            return Err(ReplaceBatchError {
                source,
                mutated: deleted,
            });
        }
        // Both halves landed. The end-of-batch session flush never rolls
        // the tree edits back (a failed flush detaches persistence and
        // keeps the in-memory content), so a flush error still means the
        // content changed.
        end_batch.map_err(|source| ReplaceBatchError {
            source,
            mutated: true,
        })?;
        Ok(true)
    }

    pub(crate) fn replace_range_at(
        &mut self,
        line_ending: LineEnding,
        line0: usize,
        col0: usize,
        len_chars: usize,
        text: &str,
    ) -> Result<EditOutcome, ReplaceBatchError> {
        if len_chars == 0 {
            return self
                .insert_text_at(line_ending, line0, col0, text)
                .map_err(|source| ReplaceBatchError {
                    source,
                    mutated: false,
                });
        }

        let actual_col0 = self.line_len_chars(line0);
        let start_col0 = col0.min(actual_col0);
        let start = self.byte_offset_for_col(line0, start_col0);
        let end = self.advance_offset_by_text_units(start, len_chars);
        let (normalized, added_lines, last_col) = normalize_insert_text(text, 0, line_ending);
        let existing = if end > start {
            self.read_range(start, end)
        } else {
            Vec::new()
        };
        let cursor = if added_lines == 0 {
            (line0, start_col0.saturating_add(last_col))
        } else {
            (line0.saturating_add(added_lines), last_col)
        };
        if existing == normalized.as_bytes() {
            return Ok(EditOutcome::new(false, cursor));
        }

        self.begin_edit_batch();
        let mut deleted = false;
        let result = (|| -> io::Result<EditOutcome> {
            let mut edited = false;
            if end > start {
                self.delete_range(start, end - start)?;
                deleted = true;
                edited = true;
            }
            let outcome = self.insert_text_at(line_ending, line0, start_col0, text)?;
            Ok(EditOutcome::new(edited || outcome.edited, outcome.cursor))
        })();
        let end_batch = self.end_edit_batch();
        match result {
            Ok(outcome) => {
                // Both halves landed. The end-of-batch session flush never
                // rolls the tree edits back (a failed flush detaches
                // persistence and keeps the in-memory content), so a flush
                // error still means the content changed.
                end_batch.map_err(|source| ReplaceBatchError {
                    source,
                    mutated: true,
                })?;
                Ok(EditOutcome::new(true, outcome.cursor))
            }
            Err(source) => {
                // A refused insert after a landed delete still changed the
                // document: the deletion is committed to the tree and stays
                // undoable, so the caller must keep the document dirty
                // instead of treating the batch as a no-op.
                Err(ReplaceBatchError {
                    source,
                    mutated: deleted,
                })
            }
        }
    }

    pub(crate) fn backspace_at(
        &mut self,
        line0: usize,
        col0: usize,
    ) -> io::Result<(bool, usize, usize)> {
        if self.total_len == 0 {
            return Ok((false, line0, col0));
        }
        if col0 > 0 {
            let actual_col0 = self.line_len_chars(line0);
            if col0 > actual_col0 {
                return Ok((false, line0, col0.saturating_sub(1)));
            }
            let cur_byte = self.byte_offset_for_col(line0, col0);
            let prev_byte = self.byte_offset_for_col(line0, col0.saturating_sub(1));
            let len = cur_byte.saturating_sub(prev_byte);
            if len == 0 {
                return Ok((false, line0, col0));
            }
            self.delete_range(prev_byte, len)?;
            return Ok((true, line0, col0.saturating_sub(1)));
        }

        if line0 == 0 {
            return Ok((false, line0, col0));
        }
        let line_start = self.line_range(line0).0;
        let newline_len = self.newline_len_before(line_start);
        if newline_len == 0 {
            return Ok((false, line0, col0));
        }
        let del_start = line_start.saturating_sub(newline_len);
        let new_line0 = line0.saturating_sub(1);
        let new_col0 = self.line_len_chars(new_line0);
        self.delete_range(del_start, newline_len)?;
        Ok((true, new_line0, new_col0))
    }

    pub(crate) fn position_for_char_index(&self, char_index: usize) -> (usize, usize) {
        let mut state = CursorScanState::new(char_index);
        if self.total_len == 0 || state.is_done() {
            return state.position();
        }

        self.pieces
            .visit_range(0, self.total_len, |piece, local_start, local_end| {
                if state.is_done() {
                    return;
                }
                let src = self.source_bytes(piece.src);
                let (seg_start, seg_end) =
                    self.clamped_piece_segment(piece, local_start, local_end, src.len());
                scan_cursor_position_bytes(&src[seg_start..seg_end], &mut state, self.engine);
            });

        state.position()
    }

    pub(crate) fn to_string_lossy(&self) -> String {
        let bytes = self.read_range(0, self.total_len);
        String::from_utf8_lossy(&bytes).to_string()
    }

    pub(super) fn source_bytes(&self, src: PieceSource) -> &[u8] {
        match src {
            PieceSource::Original => self.original.read_range(0, self.original.len()),
            PieceSource::Add => &self.add,
        }
    }

    pub(super) fn line_range(&self, line0: usize) -> (usize, usize) {
        if line0 >= self.line_count() {
            return (self.total_len, self.total_len);
        }
        let start = self.line_start_byte(line0).unwrap_or(self.known_byte_len);
        let end = if line0 + 1 < self.line_count() {
            self.line_start_byte(line0 + 1)
                .unwrap_or(self.known_byte_len)
        } else {
            self.known_byte_len
        };
        (
            start.min(self.total_len),
            end.max(start).min(self.total_len),
        )
    }

    /// Returns the clamped source-byte range of the piece segment
    /// covered by `[local_start, local_end)`.
    ///
    /// Piece bounds come from the sidecar's piece pages; a page whose
    /// CRC still holds can advertise bounds past the real source
    /// length, so every consumer slices through this clamp instead of
    /// raw adds and raw slices (the public API must not panic on
    /// hostile sidecar bytes).
    pub(crate) fn clamped_piece_segment(
        &self,
        piece: Piece,
        local_start: usize,
        local_end: usize,
        src_len: usize,
    ) -> (usize, usize) {
        let seg_start = piece.start.saturating_add(local_start).min(src_len);
        let seg_end = piece
            .start
            .saturating_add(local_end)
            .max(seg_start)
            .min(src_len);
        (seg_start, seg_end)
    }

    pub(super) fn read_range(&self, start: usize, end: usize) -> Vec<u8> {
        if start >= end || start >= self.total_len {
            return Vec::new();
        }
        let end = end.min(self.total_len);
        // Piece bounds come from the sidecar's piece pages; a hostile
        // page whose CRC still holds can advertise start/len past the
        // real source length, and byte_at's defensive `get` must not
        // be the only reader that survives it.
        let mut out = Vec::with_capacity(end.saturating_sub(start));
        self.pieces
            .visit_range(start, end, |piece, local_start, local_end| {
                let src = self.source_bytes(piece.src);
                let (seg_start, seg_end) =
                    self.clamped_piece_segment(piece, local_start, local_end, src.len());
                out.extend_from_slice(&src[seg_start..seg_end]);
            });
        out
    }

    fn byte_at(&self, offset: usize) -> Option<u8> {
        if offset >= self.total_len {
            return None;
        }
        let mut found = None;
        self.pieces
            .visit_range(offset, offset.saturating_add(1), |piece, local_start, _| {
                if found.is_some() {
                    return;
                }
                let src = self.source_bytes(piece.src);
                found = src.get(piece.start + local_start).copied();
            });
        found
    }

    pub(super) fn byte_offset_for_col(&self, line0: usize, col0: usize) -> usize {
        self.byte_offset_for_col_with_engine(line0, col0, self.engine)
    }

    /// Encoding-aware variant of [`PieceTable::byte_offset_for_col`].
    ///
    /// Walks the requested line's bytes through the engine's `step`
    /// instead of `utf8_step`, so multi-byte cells of the document's
    /// target encoding (UTF-16 code units, CJK multi-byte sequences) are
    /// counted as single text units. The line bytes are read into a
    /// contiguous buffer once because the piece-tree may store a single
    /// logical line across several `Piece`s and the encoding engine's
    /// step contract assumes a flat byte slice.
    ///
    /// Returns a byte offset within `self.total_len()`. The returned
    /// offset sits on a character boundary of the engine's encoding by
    /// construction (every `step` advance lands on one).
    pub(crate) fn byte_offset_for_col_with_engine(
        &self,
        line0: usize,
        col0: usize,
        engine: &dyn encoding_engine::EncodingEngine,
    ) -> usize {
        let (start, end) = self.line_range(line0);
        if col0 == 0 || start >= end {
            return start;
        }
        let bytes = self.read_range(start, end);
        let bytes_len = bytes.len();
        // Line breaks are recognised on WHOLE encoding cells (the same
        // discipline as `local_offset_after_break`): a raw byte check
        // fires on the first byte of a non-break UTF-16LE cell like
        // U+4E0A (`[0x0A, 0x4E]`), stopping the column walk mid-line and
        // mapping every later column of the line onto the wrong byte —
        // an edit at that position would land inside the wrong text.
        let utf16_cells = utf16_break_cells(engine.encoding().name());
        let mut col = 0usize;
        let mut local = 0usize;
        while local < bytes_len && col < col0 {
            let step = engine
                .step(&bytes, local, bytes_len)
                .max(1)
                .min(bytes_len - local);
            let cell = &bytes[local..local + step];
            let is_break = match utf16_cells {
                Some((lf, cr)) => cell == lf.as_slice() || cell == cr.as_slice(),
                None => cell == b"\n" || cell == b"\r",
            };
            if is_break {
                break;
            }
            local += step;
            col += 1;
        }
        start.saturating_add(local).min(end)
    }

    pub(super) fn advance_offset_by_text_units(&self, start: usize, text_units: usize) -> usize {
        let start = start.min(self.total_len);
        if text_units == 0 || start >= self.total_len {
            return start;
        }

        let mut remaining = text_units;
        let mut offset = start;
        let mut pending_cr = false;
        self.pieces
            .visit_range(start, self.total_len, |piece, local_start, local_end| {
                if remaining == 0 && !pending_cr {
                    return;
                }
                let src = self.source_bytes(piece.src);
                let (seg_start, seg_end) =
                    self.clamped_piece_segment(piece, local_start, local_end, src.len());
                let mut i = seg_start;
                while i < seg_end && (remaining > 0 || pending_cr) {
                    if pending_cr {
                        pending_cr = false;
                        if src[i] == b'\n' {
                            i += 1;
                            offset = offset.saturating_add(1);
                            continue;
                        }
                    }
                    if remaining == 0 {
                        break;
                    }

                    match src[i] {
                        b'\r' => {
                            remaining -= 1;
                            i += 1;
                            offset = offset.saturating_add(1);
                            pending_cr = true;
                        }
                        b'\n' => {
                            remaining -= 1;
                            i += 1;
                            offset = offset.saturating_add(1);
                        }
                        _ => {
                            let step = utf8_step(src, i, seg_end);
                            remaining -= 1;
                            i += step;
                            offset = offset.saturating_add(step);
                        }
                    }
                }
            });
        offset.min(self.total_len)
    }

    /// Encoding-aware variant of
    /// [`PieceTable::advance_offset_by_text_units`].
    ///
    /// Walks `text_units` text-unit cells forward from `start`, counting
    /// each `engine.step` advance as one cell and treating `\r\n` as a
    /// single CRLF cell when the engine reports `0x0D` and `0x0A` as
    /// 1-byte cells. The piece-tree range is read in bounded 64 KiB
    /// chunks (plus a small decode-context tail) instead of one
    /// contiguous buffer, so a replace near the start of a
    /// multi-gigabyte document never materializes the whole tail.
    ///
    /// Used by the non-UTF-8 edit paths so `try_replace_range` /
    /// `try_delete_range_at_encoded` compute deletion endpoints on
    /// character boundaries of the document's target encoding instead
    /// of on UTF-8 stepping that would mis-count multi-byte cells of
    /// UTF-16 / Class B encodings.
    pub(crate) fn advance_offset_by_text_units_with_engine(
        &self,
        start: usize,
        text_units: usize,
        engine: &dyn encoding_engine::EncodingEngine,
    ) -> usize {
        let start = start.min(self.total_len);
        if text_units == 0 || start >= self.total_len {
            return start;
        }
        const CHUNK_BYTES: usize = 64 * 1024;
        // Wideest cell any engine reports: 4 bytes (UTF-16 supplementary
        // pair, GB18030 four-byte sequence).
        const MAX_CELL_BYTES: usize = 4;
        let mut local = 0usize;
        let mut remaining = text_units;
        let mut pending_cr = false;
        loop {
            let chunk_start = start.saturating_add(local);
            if chunk_start >= self.total_len {
                break;
            }
            let chunk_end = self
                .total_len
                .min(chunk_start.saturating_add(CHUNK_BYTES + 1));
            let bytes = self.read_range(chunk_start, chunk_end);
            if bytes.is_empty() {
                break;
            }
            let bytes_len = bytes.len();
            let at_doc_end = chunk_end == self.total_len;
            let mut chunk_local = 0usize;
            while chunk_local < bytes_len && (remaining > 0 || pending_cr) {
                // Never decode a cell from a window that truncates it:
                // engines fall back to a 1-byte step on truncated
                // multi-byte leads, which would desync the walk. Cells
                // are at most MAX_CELL_BYTES wide, so consume only while
                // the full cell is visible; a cell starting nearer the
                // edge is decoded by the next chunk, which re-reads from
                // the unconsumed byte with fresh context.
                if !at_doc_end && bytes_len - chunk_local < MAX_CELL_BYTES {
                    break;
                }
                let next_step = engine.step(&bytes, chunk_local, bytes_len);
                if pending_cr {
                    pending_cr = false;
                    // CRLF collapses to a single text unit when the engine
                    // recognises both halves as 1-byte ASCII cells.
                    if next_step == 1 && bytes[chunk_local] == b'\n' {
                        chunk_local += 1;
                        continue;
                    }
                }
                if remaining == 0 {
                    break;
                }
                if next_step == 0 {
                    break;
                }
                if next_step == 1 && bytes[chunk_local] == b'\r' {
                    pending_cr = true;
                }
                chunk_local += next_step;
                remaining -= 1;
            }
            local += chunk_local;
            if remaining == 0 && !pending_cr {
                break;
            }
            if chunk_end == self.total_len {
                // Reached the end of the document: no further progress
                // is possible.
                break;
            }
            if chunk_local == 0 {
                // No cell was consumed and no pending CRLF is resolvable
                // here; further chunks cannot make progress.
                break;
            }
            // The pending_cr flag (and the first unconsumed byte at
            // `start + local`) carry into the next chunk: a CRLF split
            // across the boundary still collapses into one text unit,
            // and no cell is ever counted on both sides of an edge.
        }
        start.saturating_add(local).min(self.total_len)
    }

    /// Returns the byte range `[start, end)` covering line `line0` in the
    /// piece-tree backing.
    ///
    /// Line offsets are tracked through raw `0x0A` / `0x0D` byte scans.
    /// These scans are byte-exact for UTF-8 and Class A encodings, where
    /// line-ending bytes can never appear inside a multi-byte cell. For
    /// UTF-16 a raw `0x0A` / `0x0D` byte can be the low or high half of a
    /// non-line-ending code unit (e.g. `U+0A0A`), and for CJK a trail byte
    /// can coincide with them, so multi-byte-cell documents must not rely
    /// on this byte model for line indexing - tracked as a 1.0 design
    /// item in ROADMAP.
    pub(crate) fn line_byte_range(&self, line0: usize) -> (usize, usize) {
        self.line_range(line0)
    }

    /// Reads `total_len` bytes (or a sub-range) from the piece-tree.
    ///
    /// Marked `pub(crate)` so the encoded edit path in
    /// `commands.rs` can drive `step_backward` over a contiguous byte
    /// slice for backspace alignment.
    pub(crate) fn read_byte_range(&self, start: usize, end: usize) -> Vec<u8> {
        self.read_range(start, end)
    }

    pub(super) fn insert_bytes(&mut self, pos: usize, bytes: &[u8]) -> io::Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        let add_start = self.add.len();
        self.add.extend_from_slice(bytes);
        let new_piece = Piece {
            src: PieceSource::Add,
            start: add_start,
            len: bytes.len(),
            line_breaks: count_line_breaks_in_bytes(
                bytes,
                utf16_break_cells(self.engine().encoding().name()),
            ),
        };
        let original = &self.original;
        let add = &self.add;
        let engine = self.engine();
        let mut split_piece = |piece: Piece, left_len: usize| {
            split_piece_with_sources(original, add, piece, left_len, engine)
        };
        self.pieces
            .insert(pos.min(self.total_len), new_piece, &mut split_piece)
            .inspect_err(|_| {
                // The tree edit did not land: the add-buffer range the
                // piece would have referenced must not leak into every
                // future session flush as unreferenced dead weight.
                self.add.truncate(add_start);
            })?;
        self.total_len = self.pieces.total_len();
        // Maintain the known-region boundary here, before the session flush
        // can run: the journal must never serialize pieces from a newer
        // document length with an older known_byte_len.
        if self.full_index {
            self.known_byte_len = self.total_len;
        } else if pos <= self.known_byte_len {
            self.known_byte_len = self.known_byte_len.saturating_add(bytes.len());
        }
        self.pieces
            .set_known_boundary(self.known_byte_len, self.full_index);
        self.refresh_known_line_count();
        self.schedule_session_flush()
    }

    pub(super) fn delete_range(&mut self, start: usize, len: usize) -> io::Result<()> {
        if len == 0 || start >= self.total_len {
            return Ok(());
        }
        let end = start.saturating_add(len).min(self.total_len);
        let known_overlap = end
            .min(self.known_byte_len)
            .saturating_sub(start.min(self.known_byte_len));
        let original = &self.original;
        let add = &self.add;
        let engine = self.engine();
        let mut trim_piece = |piece: Piece, local_start: usize, local_end: usize| {
            trim_piece_with_sources(original, add, piece, local_start, local_end, engine)
        };
        self.pieces.delete_range(start, len, &mut trim_piece)?;
        self.total_len = self.pieces.total_len();
        if self.full_index {
            self.known_byte_len = self.total_len;
        } else if known_overlap > 0 {
            self.known_byte_len = self.known_byte_len.saturating_sub(known_overlap);
        }
        self.pieces
            .set_known_boundary(self.known_byte_len, self.full_index);
        self.refresh_known_line_count();
        self.schedule_session_flush()
    }

    pub(super) fn newline_len_before(&self, line_start: usize) -> usize {
        if line_start == 0 {
            return 0;
        }
        let utf16_cells = utf16_break_cells(self.engine.encoding().name());
        if let Some((lf, cr)) = utf16_cells {
            // The trailing break is a WHOLE cell: in UTF-16LE the LF cell is
            // [0x0A, 0x00], whose last byte is 0x00 — a raw single-byte
            // check never recognises it, and the reverse-search window
            // would include the trailing line break. Line starts are
            // cell-aligned; an odd line_start means a truncated tail and
            // stays unclassified.
            if line_start % 2 != 0 || line_start < 2 {
                return 0;
            }
            let cell = [
                self.byte_at(line_start - 2).unwrap_or(0),
                self.byte_at(line_start - 1).unwrap_or(0),
            ];
            if cell == lf {
                if line_start >= 4 {
                    let prev = [
                        self.byte_at(line_start - 4).unwrap_or(0),
                        self.byte_at(line_start - 3).unwrap_or(0),
                    ];
                    if prev == cr {
                        return 4; // CRLF: two cells
                    }
                }
                return 2;
            }
            if cell == cr {
                return 2;
            }
            return 0;
        }
        let b1 = self.byte_at(line_start - 1);
        if b1 == Some(b'\n') {
            if line_start >= 2 && self.byte_at(line_start - 2) == Some(b'\r') {
                return 2;
            }
            return 1;
        }
        if b1 == Some(b'\r') {
            return 1;
        }
        0
    }

    pub(super) fn undo(&mut self) -> io::Result<bool> {
        let Some(restored) = self.pieces.undo((self.known_byte_len, self.full_index)) else {
            return Ok(false);
        };
        self.total_len = self.pieces.total_len();
        // Every history entry carries the known-region boundary that
        // belonged to the state it represents (both full-index and partial
        // windows), so undo restores the exact boundary instead of
        // clamping the stale one down.
        self.known_byte_len = restored.0.min(self.total_len);
        self.full_index = restored.1 && self.known_byte_len == self.total_len;
        self.pieces
            .set_known_boundary(self.known_byte_len, self.full_index);
        self.refresh_known_line_count();
        self.schedule_session_flush()?;
        Ok(true)
    }

    pub(super) fn redo(&mut self) -> io::Result<bool> {
        let Some(restored) = self.pieces.redo((self.known_byte_len, self.full_index)) else {
            return Ok(false);
        };
        self.total_len = self.pieces.total_len();
        self.known_byte_len = restored.0.min(self.total_len);
        self.full_index = restored.1 && self.known_byte_len == self.total_len;
        self.pieces
            .set_known_boundary(self.known_byte_len, self.full_index);
        self.refresh_known_line_count();
        self.schedule_session_flush()?;
        Ok(true)
    }

    pub(super) fn refresh_known_line_count(&mut self) {
        // A full index counts the virtual trailing line after the final
        // newline; a partial region ends at a complete line whose break is
        // attributed, and the line starting at the boundary is not yet fully
        // known, so it stays out of the count.
        let trailing = usize::from(self.full_index);
        self.known_line_count = self
            .pieces
            .total_line_breaks()
            .saturating_add(trailing)
            .max(1);
    }

    fn line_start_byte(&self, line0: usize) -> Option<usize> {
        self.pieces
            .find_line_start(line0, |piece, local_break_idx| {
                self.local_offset_after_break(piece, local_break_idx)
            })
            .filter(|offset| *offset <= self.total_len)
    }

    fn local_offset_after_break(&self, piece: Piece, local_break_idx: usize) -> Option<usize> {
        let bytes = self.source_bytes(piece.src);
        let start = piece.start.min(bytes.len());
        let end = piece.start.saturating_add(piece.len).min(bytes.len());
        // Line breaks are recognised on WHOLE encoding cells. For every
        // encoding except UTF-16 the raw byte scan is already cell-exact:
        // single-byte cells are one byte, and the CJK multibyte trail
        // ranges (Shift_JIS 0x40-0xFC, GB18030 0x30-0xFE, EUC-KR 0xA1-0xFE)
        // exclude 0x0A / 0x0D, so a trail byte can never alias a break
        // byte. UTF-16 is the one exception: in BE the LF cell is
        // `[0x00, 0x0A]` and a raw byte scan fires on its second byte,
        // handing back a mid-cell (+1 byte) line start that leaves every
        // later reader of the line decoding mid-cell garbage — the
        // `prop_alignment` crawl, where `[a-z]+` degraded to one-letter
        // matches and walked a long line one column per scan. There the
        // walk advances in fixed 2-byte cells and compares each cell
        // whole, keeping the same cost class as the raw scan.
        let utf16_lf_cr = utf16_break_cells(self.engine.encoding().name());
        let mut seen = 0usize;
        let mut i = start;
        if let Some((lf, cr)) = utf16_lf_cr {
            while i + 1 < end {
                let cell = [bytes[i], bytes[i + 1]];
                if cell == lf {
                    if seen == local_break_idx {
                        return Some(i + 2 - start);
                    }
                    seen += 1;
                    i += 2;
                } else if cell == cr {
                    // CRLF collapses into one break: a CR cell immediately
                    // followed by an LF cell advances past both.
                    let crlf = i + 3 < end && [bytes[i + 2], bytes[i + 3]] == lf;
                    let after = if crlf { i + 4 } else { i + 2 };
                    if seen == local_break_idx {
                        return Some(after - start);
                    }
                    seen += 1;
                    i = after;
                } else {
                    i += 2;
                }
            }
            // A truncated one-byte tail is text and carries no break.
            return None;
        }
        while i < end {
            match bytes[i] {
                b'\n' => {
                    if seen == local_break_idx {
                        return Some(i + 1 - start);
                    }
                    seen += 1;
                    i += 1;
                }
                b'\r' => {
                    if i + 1 < end && bytes[i + 1] == b'\n' {
                        if seen == local_break_idx {
                            return Some(i + 2 - start);
                        }
                        seen += 1;
                        i += 2;
                    } else {
                        if seen == local_break_idx {
                            return Some(i + 1 - start);
                        }
                        seen += 1;
                        i += 1;
                    }
                }
                _ => i += 1,
            }
        }
        None
    }
}
