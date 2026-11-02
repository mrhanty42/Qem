//! In-memory undo/redo history for rope-backed edit buffers.
//!
//! The piece-tree edit buffer keeps its own snapshot history (see
//! `PieceTree::undo`/`redo` and the persisted editlog sidecar), so every
//! edit that lands in the piece table is already reversible. The rope
//! edit buffer had no history at all, which made `try_undo`/`try_redo`
//! silently report "nothing to undo" for small UTF-8 documents (below
//! [`super::PIECE_TABLE_MIN_BYTES`]) and for untitled documents, because
//! those never create a piece table.
//!
//! This module closes that gap. Every rope-path mutation records one
//! [`RopeEditRecord`] holding the exact pre-edit slice (`before`) and the
//! applied replacement (`after`) at a rope char index. Undo and redo are
//! pure range replacements, so they stay correct for any mix of inserts,
//! deletes, and replaces without re-deriving cursor arithmetic.
//!
//! Records are grouped into entries: one entry per user-visible operation.
//! Composite operations (a range replace runs as delete + insert) open a
//! batch so the whole composite undoes in a single step.
//!
//! Limits, documented for frontends:
//!
//! - History lives in memory only. It is never persisted, and it does not
//!   survive a document reopen (including the reload-after-save path used
//!   by encoding-conversion saves).
//! - History survives same-encoding saves. Undoing after a save marks the
//!   document dirty again, matching the conservative piece-tree behavior.
//! - When an mmap-backed document is promoted to the rope buffer
//!   (`promote_piece_table_to_rope`), the piece-tree snapshot history is
//!   not carried over. Rope history only covers edits made after the
//!   promotion; undoing pre-promotion edits reports "nothing to undo".
//! - A cap bounds memory: oldest entries are dropped once [`MAX_ENTRIES`]
//!   is exceeded.

use super::edit_buffer::EditBuffer;

/// Maximum number of undoable operations retained per rope-backed document.
const MAX_ENTRIES: usize = 500;

/// One inverted-able rope mutation: the char range `[start_char,
/// start_char + after.chars()]` currently holds `after`; undoing puts
/// `before` back into `[start_char, start_char + before.chars()]`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RopeEditRecord {
    start_char: usize,
    before: String,
    after: String,
}

impl RopeEditRecord {
    fn before_len_chars(&self) -> usize {
        self.before.chars().count()
    }

    fn after_len_chars(&self) -> usize {
        self.after.chars().count()
    }
}

/// Undo/redo history for one rope-backed [`super::Document`].
#[derive(Debug, Default)]
pub(crate) struct RopeHistory {
    /// Applied entries are `entries[..index]`; `entries[index..]` are redoable.
    entries: Vec<Vec<RopeEditRecord>>,
    index: usize,
    /// Records collected inside an open batch; flushed as one entry.
    batch: Option<Vec<RopeEditRecord>>,
    batch_depth: usize,
}

impl RopeHistory {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Opens a composite operation. Records pushed until the matching
    /// [`RopeHistory::end_batch`] are flushed as a single undoable entry.
    pub(crate) fn begin_batch(&mut self) {
        self.batch_depth = self.batch_depth.saturating_add(1);
        if self.batch.is_none() {
            self.batch = Some(Vec::new());
        }
    }

    /// Closes the innermost open batch, flushing accumulated records as one
    /// entry. Callers must flush even when the composite operation failed
    /// partway: records describe mutations that actually reached the rope,
    /// so a successful delete followed by a failed insert stays undoable.
    pub(crate) fn end_batch(&mut self) {
        self.batch_depth = self.batch_depth.saturating_sub(1);
        if self.batch_depth > 0 {
            return;
        }
        self.batch_depth = 0;
        if let Some(records) = self.batch.take() {
            self.push_entry(records);
        }
    }

    /// Records one applied rope mutation. `start_char`, `before`, and
    /// `after` must describe the mutation in the coordinates of the state
    /// the mutation was applied to. No-op mutations (identical before/after,
    /// or both empty) are ignored.
    pub(crate) fn record(&mut self, start_char: usize, before: String, after: String) {
        if before == after {
            return;
        }
        let record = RopeEditRecord {
            start_char,
            before,
            after,
        };
        if let Some(batch) = self.batch.as_mut() {
            batch.push(record);
            return;
        }
        self.push_entry(vec![record]);
    }

    fn push_entry(&mut self, records: Vec<RopeEditRecord>) {
        if records.is_empty() {
            return;
        }
        // A fresh edit invalidates every redoable entry past the cursor.
        self.entries.truncate(self.index);
        self.entries.push(records);
        if self.entries.len() > MAX_ENTRIES {
            let drop_count = self.entries.len() - MAX_ENTRIES;
            self.entries.drain(0..drop_count);
            self.index = self.index.saturating_sub(drop_count);
        } else {
            self.index += 1;
        }
    }

    /// Returns `true` when there is nothing to undo.
    pub(crate) fn can_undo(&self) -> bool {
        self.index > 0
    }

    /// Returns `true` when there is something to redo.
    pub(crate) fn can_redo(&self) -> bool {
        self.index < self.entries.len()
    }

    /// Applies the inverse of the most recent applied entry to `buffer`.
    ///
    /// Returns the restored cursor as a full-text char index: the end of the
    /// re-applied `before` text of the entry's first record, which for the
    /// recorded operation shapes equals the caret position the edit started
    /// from.
    pub(crate) fn undo(&mut self, buffer: &mut EditBuffer) -> Option<usize> {
        if !self.can_undo() {
            return None;
        }
        self.index -= 1;
        let entry = &self.entries[self.index];
        for record in entry.iter().rev() {
            let after_end = record.start_char + record.after_len_chars();
            if record.start_char < after_end {
                buffer.remove(record.start_char..after_end);
            }
            if !record.before.is_empty() {
                buffer.insert(record.start_char, &record.before);
            }
        }
        let first = entry.first()?;
        Some(first.start_char + first.before_len_chars())
    }

    /// Re-applies the next undone entry to `buffer`.
    ///
    /// Returns the resulting cursor as a full-text char index: the end of
    /// the re-applied `after` text of the entry's last record.
    pub(crate) fn redo(&mut self, buffer: &mut EditBuffer) -> Option<usize> {
        if !self.can_redo() {
            return None;
        }
        let entry = &self.entries[self.index];
        for record in entry.iter() {
            let before_end = record.start_char + record.before_len_chars();
            if record.start_char < before_end {
                buffer.remove(record.start_char..before_end);
            }
            if !record.after.is_empty() {
                buffer.insert(record.start_char, &record.after);
            }
        }
        self.index += 1;
        // Later records' coordinates already account for earlier records of
        // the same batch, so the resulting caret is the end of the last
        // record's re-applied text.
        let last = entry.last()?;
        Some(last.start_char + last.after_len_chars())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buffer(text: &str) -> EditBuffer {
        EditBuffer::from_str(text)
    }

    #[test]
    fn insert_undo_redo_round_trip() {
        let mut history = RopeHistory::new();
        let mut b = buffer("hello");
        b.insert(5, " world");
        history.record(5, String::new(), " world".to_owned());
        assert_eq!(b.to_string(), "hello world");

        let cursor = history.undo(&mut b).expect("undo");
        assert_eq!(b.to_string(), "hello");
        assert_eq!(cursor, 5);

        let cursor = history.redo(&mut b).expect("redo");
        assert_eq!(b.to_string(), "hello world");
        assert_eq!(cursor, 11);
    }

    #[test]
    fn delete_undo_restores_text_and_cursor() {
        let mut history = RopeHistory::new();
        let mut b = buffer("hello world");
        b.remove(5..11);
        history.record(5, " world".to_owned(), String::new());
        assert_eq!(b.to_string(), "hello");

        let cursor = history.undo(&mut b).expect("undo");
        assert_eq!(b.to_string(), "hello world");
        assert_eq!(cursor, 11);
    }

    #[test]
    fn replace_batch_undoes_in_one_step() {
        let mut history = RopeHistory::new();
        let mut b = buffer("hello world");
        history.begin_batch();
        b.remove(6..11);
        history.record(6, "world".to_owned(), String::new());
        b.insert(6, "qem");
        history.record(6, String::new(), "qem".to_owned());
        history.end_batch();
        assert_eq!(b.to_string(), "hello qem");

        let cursor = history.undo(&mut b).expect("undo");
        assert_eq!(b.to_string(), "hello world");
        assert_eq!(cursor, 11);

        let cursor = history.redo(&mut b).expect("redo");
        assert_eq!(b.to_string(), "hello qem");
        assert_eq!(cursor, 9);
    }

    #[test]
    fn fresh_edit_clears_redo_stack() {
        let mut history = RopeHistory::new();
        let mut b = buffer("a");
        b.insert(1, "b");
        history.record(1, String::new(), "b".to_owned());
        b.insert(2, "c");
        history.record(2, String::new(), "c".to_owned());
        assert!(history.undo(&mut b).is_some());

        b.insert(1, "x");
        history.record(1, String::new(), "x".to_owned());
        assert!(!history.can_redo(), "redo must be dropped after a new edit");
        assert_eq!(b.to_string(), "axb");
    }

    #[test]
    fn noop_records_are_ignored() {
        let mut history = RopeHistory::new();
        history.record(0, "same".to_owned(), "same".to_owned());
        history.record(0, String::new(), String::new());
        assert!(!history.can_undo());
    }

    #[test]
    fn history_cap_drops_oldest_entries() {
        let mut history = RopeHistory::new();
        let mut b = buffer("");
        for i in 0..(MAX_ENTRIES + 50) {
            let ch = (b'a' + (i % 26) as u8) as char;
            b.insert(b.len_chars(), &ch.to_string());
            history.record(b.len_chars() - 1, String::new(), ch.to_string());
        }
        assert_eq!(history.entries.len(), MAX_ENTRIES);
        // Every retained undo must still restore a consistent shorter buffer.
        let mut undos = 0;
        while history.undo(&mut b).is_some() {
            undos += 1;
        }
        assert_eq!(undos, MAX_ENTRIES);
        assert_eq!(b.len_chars(), 50);
    }
}
