use super::btree::ChildRef;
use super::filter_zero_len;
use super::{Piece, PieceTree};
use std::io;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct RootEntry {
    pub(super) root: Option<ChildRef>,
}

impl PieceTree {
    /// Replaces the live root with a tree rebuilt from `pieces` (same
    /// history entry, no new undo step). The rebuild allocates fresh
    /// pages, so it must satisfy the same residency contract as
    /// `insert` / `delete_range`: while persistence is down the new root
    /// may only reference pages the resident window actually holds. A
    /// root referencing a refused page reads back silently truncated
    /// (`visit_range_ref` skips unreadable pages) and would stream that
    /// truncation into every save. On refusal the previous root stays in
    /// place and the error is reported to the caller.
    pub(crate) fn replace_current_root_with_pieces(
        &mut self,
        pieces: Vec<Piece>,
    ) -> io::Result<()> {
        let previous_root = self.root;
        let first_new_page = self.store.next_page_id_hint();
        let root = self.build_tree_from_pieces(filter_zero_len(pieces));
        if let Err(err) = self.store.verify_pages_resident_since(first_new_page) {
            self.root = previous_root;
            return Err(err);
        }
        self.root = root;
        if let Some(entry) = self.history.get_mut(self.history_index) {
            entry.root = root;
        } else {
            self.history = vec![RootEntry { root }];
            self.history_index = 0;
        }
        Ok(())
    }

    pub(crate) fn undo(&mut self, current_boundary: (usize, bool)) -> Option<(usize, bool)> {
        if self.history_index == 0 {
            return None;
        }
        self.history_index = self.history_index.saturating_sub(1);
        self.root = self.history[self.history_index].root;
        Some(
            self.known_history
                .get(self.history_index)
                .copied()
                .unwrap_or(current_boundary),
        )
    }

    pub(crate) fn redo(&mut self, current_boundary: (usize, bool)) -> Option<(usize, bool)> {
        if self.history_index + 1 >= self.history.len() {
            return None;
        }
        self.history_index += 1;
        self.root = self.history[self.history_index].root;
        Some(
            self.known_history
                .get(self.history_index)
                .copied()
                .unwrap_or(current_boundary),
        )
    }

    pub(crate) fn begin_batch_edit(&mut self) {
        self.history_batch_depth = self.history_batch_depth.saturating_add(1);
    }

    pub(crate) fn end_batch_edit(&mut self) {
        if self.history_batch_depth == 0 {
            return;
        }
        self.history_batch_depth -= 1;
        if self.history_batch_depth == 0 && self.history_batch_dirty {
            self.history_batch_dirty = false;
            self.push_history_root();
        }
    }

    pub(super) fn record_history_root(&mut self) {
        if self.history_batch_depth > 0 {
            self.history_batch_dirty = true;
            return;
        }
        self.push_history_root();
    }

    fn push_history_root(&mut self) {
        let next = RootEntry { root: self.root };
        let is_same = self
            .history
            .get(self.history_index)
            .copied()
            .map(|entry| entry == next)
            .unwrap_or(false);
        // A pending boundary (written by `set_known_boundary` while a batch
        // was active) is detected by the known history carrying exactly one
        // extra entry; it must survive the truncate below and become the
        // boundary of the newly committed root.
        let pending = if self.known_history.len() == self.history.len() + 1 {
            Some(self.known_history[self.history_index + 1])
        } else {
            None
        };
        if is_same {
            // A no-op batch: the pending boundary describes the same state,
            // so fold it into the current entry and drop the pending slot.
            if let Some(pending) = pending {
                self.known_history[self.history_index] = pending;
                self.known_history.truncate(self.history_index + 1);
            }
            return;
        }
        self.history.truncate(self.history_index.saturating_add(1));
        self.known_history
            .truncate(self.history_index.saturating_add(1));
        self.history.push(next);
        let fallback = pending
            .or_else(|| self.known_history.last().copied())
            .unwrap_or((0, false));
        self.known_history.push(fallback);
        self.history_index = self.history.len().saturating_sub(1);
    }

    /// Patches the known-region boundary of the current history entry.
    /// Callers invoke this right after they adjust their own boundary, so
    /// every history entry carries the boundary that belonged to the state
    /// it represents.
    pub(crate) fn set_known_boundary(&mut self, known: usize, full_index: bool) {
        // Inside an edit batch the root push is deferred, so the boundary
        // that belongs to the batch's final state must land in the
        // *pending* slot (`history_index + 1`), not in the current one:
        // the current entry describes the pre-batch state that undo must
        // restore. `push_history_root` picks the pending value up when the
        // deferred root is committed.
        let target = if self.history_batch_depth > 0 {
            self.history_index + 1
        } else {
            self.history_index
        };
        while self.known_history.len() <= target {
            let fallback = self
                .known_history
                .last()
                .copied()
                .unwrap_or((known, full_index));
            self.known_history.push(fallback);
        }
        if let Some(entry) = self.known_history.get_mut(target) {
            *entry = (known, full_index);
        }
    }
}
