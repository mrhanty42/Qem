use super::{
    FragmentationStats, PageKind, Piece, PieceSource, PieceTree, EDITLOG_HEADER_BYTES,
    EDITLOG_PAGE_HEADER_BYTES, EDITLOG_PAGE_SIZE, INTERNAL_ENTRY_BYTES, LEAF_ENTRY_BYTES,
    MAX_INTERNAL_CHILDREN, MAX_LEAF_PIECES,
};
use crate::sidecar::{le_u32, le_u64};
use crc32fast::Hasher;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ChildRef {
    pub(super) page_id: u64,
    pub(super) total_bytes: usize,
    pub(super) total_line_breaks: usize,
}

#[derive(Clone, Debug)]
pub(super) enum PiecePage {
    Internal { children: Vec<ChildRef> },
    Leaf { pieces: Vec<Piece> },
}

pub(super) fn serialize_page(page: &PiecePage) -> io::Result<Vec<u8>> {
    let mut buf = vec![0u8; EDITLOG_PAGE_SIZE];
    let (kind, count, entry_width) = match page {
        PiecePage::Internal { children } => {
            (PageKind::Internal, children.len(), INTERNAL_ENTRY_BYTES)
        }
        PiecePage::Leaf { pieces } => (PageKind::Leaf, pieces.len(), LEAF_ENTRY_BYTES),
    };

    let max_entries = (EDITLOG_PAGE_SIZE - EDITLOG_PAGE_HEADER_BYTES) / entry_width;
    if count > max_entries {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "page exceeds fixed editlog capacity",
        ));
    }

    buf[4] = kind as u8;
    buf[8..16].copy_from_slice(&(count as u64).to_le_bytes());
    let mut cursor = EDITLOG_PAGE_HEADER_BYTES;
    match page {
        PiecePage::Internal { children } => {
            for child in children {
                buf[cursor..cursor + 8].copy_from_slice(&child.page_id.to_le_bytes());
                buf[cursor + 8..cursor + 16]
                    .copy_from_slice(&(child.total_bytes as u64).to_le_bytes());
                buf[cursor + 16..cursor + 24]
                    .copy_from_slice(&(child.total_line_breaks as u64).to_le_bytes());
                cursor += INTERNAL_ENTRY_BYTES;
            }
        }
        PiecePage::Leaf { pieces } => {
            for piece in pieces {
                buf[cursor] = match piece.src {
                    PieceSource::Original => 0,
                    PieceSource::Add => 1,
                };
                buf[cursor + 8..cursor + 16].copy_from_slice(&(piece.start as u64).to_le_bytes());
                buf[cursor + 16..cursor + 24].copy_from_slice(&(piece.len as u64).to_le_bytes());
                buf[cursor + 24..cursor + 32]
                    .copy_from_slice(&(piece.line_breaks as u64).to_le_bytes());
                cursor += LEAF_ENTRY_BYTES;
            }
        }
    }

    let mut hasher = Hasher::new();
    hasher.update(&buf[4..]);
    buf[0..4].copy_from_slice(&hasher.finalize().to_le_bytes());
    Ok(buf)
}

pub(super) fn read_page_from_file(file: &mut File, page_id: u64) -> io::Result<PiecePage> {
    let mut buf = vec![0u8; EDITLOG_PAGE_SIZE];
    let offset = (EDITLOG_HEADER_BYTES as u64).saturating_add(
        page_id
            .saturating_sub(1)
            .saturating_mul(EDITLOG_PAGE_SIZE as u64),
    );
    file.seek(SeekFrom::Start(offset))?;
    file.read_exact(&mut buf)?;
    parse_page(&buf)
}

pub(super) fn parse_page(buf: &[u8]) -> io::Result<PiecePage> {
    if buf.len() != EDITLOG_PAGE_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid editlog page size",
        ));
    }

    let expected_crc = le_u32(buf, 0, "qem editlog page")?;
    let mut hasher = Hasher::new();
    hasher.update(&buf[4..]);
    if hasher.finalize() != expected_crc {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "qem editlog page checksum mismatch",
        ));
    }

    let kind = match buf[4] {
        1 => PageKind::Internal,
        2 => PageKind::Leaf,
        3 => PageKind::History,
        4 => PageKind::Add,
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid qem editlog page kind",
            ));
        }
    };
    let count = le_u64(buf, 8, "qem editlog page")? as usize;
    let mut cursor = EDITLOG_PAGE_HEADER_BYTES;

    match kind {
        PageKind::Internal => {
            let max_entries =
                (EDITLOG_PAGE_SIZE - EDITLOG_PAGE_HEADER_BYTES) / INTERNAL_ENTRY_BYTES;
            if count > max_entries {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "internal editlog page is overfull",
                ));
            }
            // The writer never emits an empty internal page (a root for
            // an empty document is `None`, and
            // `build_tree_from_child_refs` allocates only for non-empty
            // children). A CRC-valid but empty internal page would panic
            // `child_index_for_offset` consumers on insert and silently
            // empty the document on delete, so it is invalid by
            // construction.
            if count == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "internal editlog page is empty",
                ));
            }

            let mut children = Vec::with_capacity(count);
            for _ in 0..count {
                children.push(ChildRef {
                    page_id: le_u64(buf, cursor, "qem editlog page")?,
                    total_bytes: le_u64(buf, cursor + 8, "qem editlog page")? as usize,
                    total_line_breaks: le_u64(buf, cursor + 16, "qem editlog page")? as usize,
                });
                cursor += INTERNAL_ENTRY_BYTES;
            }
            Ok(PiecePage::Internal { children })
        }
        PageKind::Leaf => {
            let max_entries = (EDITLOG_PAGE_SIZE - EDITLOG_PAGE_HEADER_BYTES) / LEAF_ENTRY_BYTES;
            if count > max_entries {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "leaf editlog page is overfull",
                ));
            }
            // Same distrust as the internal branch above: the writer
            // never emits an empty leaf (`leaf_refs_from_pieces` returns
            // before allocating on empty input), and a forged empty leaf
            // would parse into a silently-short read under a root that
            // claims a nonzero span. Unlike the empty internal page this
            // cannot panic today - the reject keeps the parse contract
            // symmetric and the read hole closed.
            if count == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "leaf editlog page is empty",
                ));
            }

            let mut pieces = Vec::with_capacity(count);
            for _ in 0..count {
                let src = match buf[cursor] {
                    0 => PieceSource::Original,
                    1 => PieceSource::Add,
                    _ => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "invalid piece source in editlog page",
                        ));
                    }
                };
                pieces.push(Piece {
                    src,
                    start: le_u64(buf, cursor + 8, "qem editlog page")? as usize,
                    len: le_u64(buf, cursor + 16, "qem editlog page")? as usize,
                    line_breaks: le_u64(buf, cursor + 24, "qem editlog page")? as usize,
                });
                cursor += LEAF_ENTRY_BYTES;
            }
            Ok(PiecePage::Leaf { pieces })
        }
        PageKind::History | PageKind::Add => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "non-piece editlog page used as piece page",
        )),
    }
}

fn child_index_for_offset(children: &[ChildRef], offset: usize) -> (usize, usize) {
    if children.is_empty() {
        return (0, 0);
    }

    let mut child_start = 0usize;
    for (index, child) in children.iter().enumerate() {
        let child_end = child_start.saturating_add(child.total_bytes);
        if offset < child_end || index + 1 == children.len() {
            return (index, child_start);
        }
        child_start = child_end;
    }
    (children.len().saturating_sub(1), child_start)
}

fn insert_piece_into_leaf<F>(
    mut pieces: Vec<Piece>,
    offset: usize,
    piece: Piece,
    split_piece: &mut F,
) -> Vec<Piece>
where
    F: FnMut(Piece, usize) -> (Option<Piece>, Option<Piece>),
{
    let mut acc = 0usize;
    for index in 0..pieces.len() {
        let cur = pieces[index];
        let piece_end = acc.saturating_add(cur.len);
        if offset <= piece_end {
            let inner = offset.saturating_sub(acc).min(cur.len);
            if inner == 0 {
                pieces.insert(index, piece);
            } else if inner >= cur.len {
                pieces.insert(index + 1, piece);
            } else {
                let (left, right) = split_piece(cur, inner);
                let replacement = [left, Some(piece), right]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>();
                pieces.splice(index..=index, replacement);
            }
            return pieces;
        }
        acc = piece_end;
    }

    pieces.push(piece);
    pieces
}

fn delete_range_from_leaf<F>(
    pieces: Vec<Piece>,
    start: usize,
    end: usize,
    trim_piece: &mut F,
) -> Vec<Piece>
where
    F: FnMut(Piece, usize, usize) -> (Option<Piece>, Option<Piece>),
{
    let mut out = Vec::with_capacity(pieces.len());
    let mut acc = 0usize;
    for piece in pieces {
        let piece_end = acc.saturating_add(piece.len);
        if piece_end <= start || acc >= end {
            out.push(piece);
            acc = piece_end;
            continue;
        }

        let local_start = start.saturating_sub(acc).min(piece.len);
        let local_end = end.min(piece_end).saturating_sub(acc).min(piece.len);
        let (left, right) = trim_piece(piece, local_start, local_end);
        if let Some(left) = left {
            out.push(left);
        }
        if let Some(right) = right {
            out.push(right);
        }

        acc = piece_end;
    }
    out
}

pub(super) fn coalesce_adjacent(pieces: &mut Vec<Piece>) {
    if pieces.len() < 2 {
        return;
    }
    let mut out: Vec<Piece> = Vec::with_capacity(pieces.len());
    for piece in pieces.drain(..) {
        if let Some(last) = out.last_mut() {
            let merge_original = last.src == PieceSource::Original
                && last.line_breaks == 0
                && piece.line_breaks == 0;
            let merge_add = last.src == PieceSource::Add;
            if last.src == piece.src
                // Saturate: hostile page payloads can carry
                // start/len whose plain add overflows (debug panic,
                // release wrap merging unrelated pieces).
                && last.start.saturating_add(last.len) == piece.start
                && (merge_original || merge_add)
            {
                last.len = last.len.saturating_add(piece.len);
                last.line_breaks = last.line_breaks.saturating_add(piece.line_breaks);
                continue;
            }
        }
        out.push(piece);
    }
    *pieces = out;
}

impl PieceTree {
    /// The typed error a read walker surfaces when a lazily read piece
    /// page does not load (CRC mismatch, missing sidecar, IO failure).
    /// Read walkers must never answer `None` quietly here: a skipped
    /// subtree would shorten the read behind the root's advertised
    /// `total_len`, so every read-time page miss becomes this error and
    /// honest callers surface it all the way to `DocumentError::Read`.
    pub(super) fn unreadable_page_error(page_id: u64) -> io::Error {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "qem editlog piece page {page_id} is unreadable; \
                 refusing to shorten the read below the advertised range"
            ),
        )
    }

    pub(super) fn collect_pieces_result(
        &self,
        child: ChildRef,
        out: &mut Vec<Piece>,
    ) -> io::Result<()> {
        let page = self
            .page(child.page_id)
            .ok_or_else(|| Self::unreadable_page_error(child.page_id))?;
        match page.as_ref() {
            PiecePage::Internal { children } => {
                for child in children {
                    self.collect_pieces_result(*child, out)?;
                }
            }
            PiecePage::Leaf { pieces } => out.extend_from_slice(pieces),
        }
        Ok(())
    }

    pub(super) fn collect_fragmentation_stats_result(
        &self,
        child: ChildRef,
        stats: &mut FragmentationStats,
    ) -> io::Result<()> {
        let page = self
            .page(child.page_id)
            .ok_or_else(|| Self::unreadable_page_error(child.page_id))?;
        match page.as_ref() {
            PiecePage::Internal { children } => {
                for child in children {
                    self.collect_fragmentation_stats_result(*child, stats)?;
                }
            }
            PiecePage::Leaf { pieces } => {
                for piece in pieces {
                    stats.observe_piece(*piece);
                }
            }
        }
        Ok(())
    }

    pub(super) fn visit_range_ref_result(
        &self,
        child: ChildRef,
        start: usize,
        end: usize,
        base: usize,
        visit: &mut impl FnMut(Piece, usize, usize),
    ) -> io::Result<()> {
        let page = self
            .page(child.page_id)
            .ok_or_else(|| Self::unreadable_page_error(child.page_id))?;
        match page.as_ref() {
            PiecePage::Internal { children } => {
                let mut child_base = base;
                for child in children {
                    let child_end = child_base.saturating_add(child.total_bytes);
                    if child_end <= start {
                        child_base = child_end;
                        continue;
                    }
                    if child_base >= end {
                        break;
                    }
                    self.visit_range_ref_result(*child, start, end, child_base, visit)?;
                    child_base = child_end;
                }
            }
            PiecePage::Leaf { pieces } => {
                let mut piece_base = base;
                for piece in pieces {
                    let piece_end = piece_base.saturating_add(piece.len);
                    if piece_end <= start {
                        piece_base = piece_end;
                        continue;
                    }
                    if piece_base >= end {
                        break;
                    }
                    let overlap_start = start.saturating_sub(piece_base).min(piece.len);
                    let overlap_end = end.min(piece_end).saturating_sub(piece_base).min(piece.len);
                    if overlap_start < overlap_end {
                        visit(*piece, overlap_start, overlap_end);
                    }
                    piece_base = piece_end;
                }
            }
        }
        Ok(())
    }

    /// Readability-only walk over `[start, end)`: descends exactly like the
    /// visiting walkers but touches no callback, so an honest read can
    /// pre-verify every page overlapping the bytes it is about to decode.
    pub(super) fn verify_range_ref_readable(
        &self,
        child: ChildRef,
        start: usize,
        end: usize,
        base: usize,
    ) -> io::Result<()> {
        let page = self
            .page(child.page_id)
            .ok_or_else(|| Self::unreadable_page_error(child.page_id))?;
        match page.as_ref() {
            PiecePage::Internal { children } => {
                let mut child_base = base;
                for child in children {
                    let child_end = child_base.saturating_add(child.total_bytes);
                    if child_end <= start {
                        child_base = child_end;
                        continue;
                    }
                    if child_base >= end {
                        break;
                    }
                    self.verify_range_ref_readable(*child, start, end, child_base)?;
                    child_base = child_end;
                }
            }
            PiecePage::Leaf { .. } => {}
        }
        Ok(())
    }

    pub(super) fn visit_range_ref_while_result(
        &self,
        child: ChildRef,
        start: usize,
        end: usize,
        base: usize,
        visit: &mut impl FnMut(Piece, usize, usize) -> bool,
    ) -> io::Result<bool> {
        let page = self
            .page(child.page_id)
            .ok_or_else(|| Self::unreadable_page_error(child.page_id))?;
        match page.as_ref() {
            PiecePage::Internal { children } => {
                let mut child_base = base;
                for child in children {
                    let child_end = child_base.saturating_add(child.total_bytes);
                    if child_end <= start {
                        child_base = child_end;
                        continue;
                    }
                    if child_base >= end {
                        break;
                    }
                    if !self.visit_range_ref_while_result(*child, start, end, child_base, visit)? {
                        return Ok(false);
                    }
                    child_base = child_end;
                }
            }
            PiecePage::Leaf { pieces } => {
                let mut piece_base = base;
                for piece in pieces {
                    let piece_end = piece_base.saturating_add(piece.len);
                    if piece_end <= start {
                        piece_base = piece_end;
                        continue;
                    }
                    if piece_base >= end {
                        break;
                    }
                    let overlap_start = start.saturating_sub(piece_base).min(piece.len);
                    let overlap_end = end.min(piece_end).saturating_sub(piece_base).min(piece.len);
                    if overlap_start < overlap_end && !visit(*piece, overlap_start, overlap_end) {
                        return Ok(false);
                    }
                    piece_base = piece_end;
                }
            }
        }
        Ok(true)
    }

    pub(super) fn visit_range_ref_rev_while_result(
        &self,
        child: ChildRef,
        start: usize,
        end: usize,
        base: usize,
        visit: &mut impl FnMut(Piece, usize, usize) -> bool,
    ) -> io::Result<bool> {
        let page = self
            .page(child.page_id)
            .ok_or_else(|| Self::unreadable_page_error(child.page_id))?;
        match page.as_ref() {
            PiecePage::Internal { children } => {
                let mut child_end = base.saturating_add(child.total_bytes);
                for child in children.iter().rev() {
                    let child_start = child_end.saturating_sub(child.total_bytes);
                    if child_start >= end {
                        child_end = child_start;
                        continue;
                    }
                    if child_end <= start {
                        break;
                    }
                    if !self.visit_range_ref_rev_while_result(
                        *child,
                        start,
                        end,
                        child_start,
                        visit,
                    )? {
                        return Ok(false);
                    }
                    child_end = child_start;
                }
            }
            PiecePage::Leaf { pieces } => {
                let mut piece_end = base.saturating_add(child.total_bytes);
                for piece in pieces.iter().rev() {
                    let piece_start = piece_end.saturating_sub(piece.len);
                    if piece_start >= end {
                        piece_end = piece_start;
                        continue;
                    }
                    if piece_end <= start {
                        break;
                    }
                    let overlap_start = start.saturating_sub(piece_start).min(piece.len);
                    let overlap_end = end
                        .min(piece_end)
                        .saturating_sub(piece_start)
                        .min(piece.len);
                    if overlap_start < overlap_end && !visit(*piece, overlap_start, overlap_end) {
                        return Ok(false);
                    }
                    piece_end = piece_start;
                }
            }
        }
        Ok(true)
    }

    pub(super) fn find_line_start_ref_result(
        &self,
        child: ChildRef,
        target_break: usize,
        base: usize,
        locate_break: &mut impl FnMut(Piece, usize) -> Option<usize>,
    ) -> io::Result<Option<usize>> {
        let page = self
            .page(child.page_id)
            .ok_or_else(|| Self::unreadable_page_error(child.page_id))?;
        match page.as_ref() {
            PiecePage::Internal { children } => {
                let mut child_base = base;
                let mut target_break = target_break;
                for child in children {
                    if target_break >= child.total_line_breaks {
                        target_break -= child.total_line_breaks;
                        child_base = child_base.saturating_add(child.total_bytes);
                        continue;
                    }
                    return self.find_line_start_ref_result(
                        *child,
                        target_break,
                        child_base,
                        locate_break,
                    );
                }
                Ok(None)
            }
            PiecePage::Leaf { pieces } => {
                let mut piece_base = base;
                let mut target_break = target_break;
                for piece in pieces {
                    if target_break >= piece.line_breaks {
                        target_break -= piece.line_breaks;
                        piece_base = piece_base.saturating_add(piece.len);
                        continue;
                    }
                    let Some(local) = locate_break(*piece, target_break) else {
                        return Ok(None);
                    };
                    return Ok(Some(piece_base.saturating_add(local)));
                }
                Ok(None)
            }
        }
    }

    pub(super) fn insert_at<F>(
        &mut self,
        child: ChildRef,
        offset: usize,
        piece: Piece,
        split_piece: &mut F,
    ) -> io::Result<Vec<ChildRef>>
    where
        F: FnMut(Piece, usize) -> (Option<Piece>, Option<Piece>),
    {
        // An unreadable page must fail the edit: splicing an empty
        // child list here would silently drop the whole subtree from
        // the document while the caller believes the edit landed.
        let Some(page) = self.page(child.page_id).map(|page| (*page).clone()) else {
            return Err(io::Error::other(format!(
                "piece page {} is unreadable; refusing to edit through the loss",
                child.page_id
            )));
        };
        match page {
            PiecePage::Leaf { pieces } => {
                let mut next = insert_piece_into_leaf(pieces, offset, piece, split_piece);
                coalesce_adjacent(&mut next);
                Ok(self.leaf_refs_from_pieces(next))
            }
            PiecePage::Internal { children } => {
                let (index, child_start) = child_index_for_offset(&children, offset);
                let local_offset = offset.saturating_sub(child_start);
                let mut next_children = children;
                let target = next_children[index];
                let inserted = self.insert_at(target, local_offset, piece, split_piece)?;
                next_children.splice(index..=index, inserted);
                Ok(self.internal_refs_from_children(next_children))
            }
        }
    }

    pub(super) fn delete_at<F>(
        &mut self,
        child: ChildRef,
        start: usize,
        end: usize,
        trim_piece: &mut F,
    ) -> io::Result<Vec<ChildRef>>
    where
        F: FnMut(Piece, usize, usize) -> (Option<Piece>, Option<Piece>),
    {
        let Some(page) = self.page(child.page_id).map(|page| (*page).clone()) else {
            return Err(io::Error::other(format!(
                "piece page {} is unreadable; refusing to edit through the loss",
                child.page_id
            )));
        };
        match page {
            PiecePage::Leaf { pieces } => {
                let mut next = delete_range_from_leaf(pieces, start, end, trim_piece);
                coalesce_adjacent(&mut next);
                Ok(self.leaf_refs_from_pieces(next))
            }
            PiecePage::Internal { children } => {
                let mut next_children = Vec::with_capacity(children.len());
                let mut child_base = 0usize;
                for child in children {
                    let child_end = child_base.saturating_add(child.total_bytes);
                    if child_end <= start || child_base >= end {
                        next_children.push(child);
                    } else {
                        let local_start = start.saturating_sub(child_base).min(child.total_bytes);
                        let local_end = end.min(child_end).saturating_sub(child_base);
                        next_children.extend(self.delete_at(
                            child,
                            local_start,
                            local_end,
                            trim_piece,
                        )?);
                    }
                    child_base = child_end;
                }
                Ok(self.internal_refs_from_children(next_children))
            }
        }
    }

    pub(super) fn build_tree_from_pieces(&mut self, pieces: Vec<Piece>) -> Option<ChildRef> {
        let leaves = self.leaf_refs_from_pieces(pieces);
        self.build_tree_from_child_refs(leaves)
    }

    pub(super) fn build_tree_from_child_refs(
        &mut self,
        mut children: Vec<ChildRef>,
    ) -> Option<ChildRef> {
        if children.is_empty() {
            return None;
        }
        while children.len() > 1 {
            children = self.internal_refs_from_children(children);
        }
        children.pop()
    }

    fn leaf_refs_from_pieces(&mut self, pieces: Vec<Piece>) -> Vec<ChildRef> {
        if pieces.is_empty() {
            return Vec::new();
        }

        pieces
            .chunks(MAX_LEAF_PIECES)
            .map(|chunk| {
                let chunk = chunk.to_vec();
                // Hostile CRC-valid pages can carry totals that overflow
                // a plain sum; saturate so the next edit fails a bounds
                // check instead of panicking (debug) or wrapping (release).
                let total_bytes = chunk
                    .iter()
                    .map(|piece| piece.len)
                    .fold(0usize, usize::saturating_add);
                let total_line_breaks = chunk
                    .iter()
                    .map(|piece| piece.line_breaks)
                    .fold(0usize, usize::saturating_add);
                self.alloc_page(
                    PiecePage::Leaf { pieces: chunk },
                    total_bytes,
                    total_line_breaks,
                )
            })
            .collect()
    }

    fn internal_refs_from_children(&mut self, children: Vec<ChildRef>) -> Vec<ChildRef> {
        if children.is_empty() {
            return Vec::new();
        }

        children
            .chunks(MAX_INTERNAL_CHILDREN)
            .map(|chunk| {
                let chunk = chunk.to_vec();
                // Saturate for hostile page totals, same as the leaf
                // aggregator above.
                let total_bytes = chunk
                    .iter()
                    .map(|child| child.total_bytes)
                    .fold(0usize, usize::saturating_add);
                let total_line_breaks = chunk
                    .iter()
                    .map(|child| child.total_line_breaks)
                    .fold(0usize, usize::saturating_add);
                self.alloc_page(
                    PiecePage::Internal { children: chunk },
                    total_bytes,
                    total_line_breaks,
                )
            })
            .collect()
    }

    fn alloc_page(
        &mut self,
        page: PiecePage,
        total_bytes: usize,
        total_line_breaks: usize,
    ) -> ChildRef {
        self.store.alloc_page(page, total_bytes, total_line_breaks)
    }
}
