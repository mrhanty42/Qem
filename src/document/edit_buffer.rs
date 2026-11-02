//! Qem-native edited buffer: the 0.9.0 replacement for `ropey::Rope`.
//!
//! This is the in-memory UTF-8 edit buffer behind small-file and untitled
//! documents. It is a B-tree of flat UTF-8 chunks whose internal nodes
//! carry per-child summaries (byte length, char count, line-break count),
//! so every accessor the document layer needs is `O(log N)` in the
//! document size: char/byte conversion, line lookup, insert, remove, and
//! chunk iteration for streaming search.
//!
//! # Semantics contract (matches the rope it replaces)
//!
//! - One text unit is one Unicode scalar value; `char`-indexed APIs count
//!   scalar values, `byte`-indexed APIs count UTF-8 bytes.
//! - The buffer stores LF-normalized text: `\n` is the only line break.
//!   This mirrors the document layer, which funnels every rope insert
//!   through `normalize_insert_text(.., LineEnding::Lf)` and re-renders
//!   the document's stored line ending only at save time. Callers must
//!   normalize before inserting; `insert` debug-asserts it.
//! - `len_lines()` is the `\n` count plus one. The `\n` char belongs to
//!   the line it ends (`char_to_line` of that char is the line's index).
//! - Chunks are an implementation detail: they are always valid UTF-8,
//!   but their segmentation is not stable. Callers may only rely on the
//!   concatenation of all chunks and each chunk's byte offset.
//! - `Clone` is a deep copy; callers that need snapshots copy the buffer.
//!
//! # Shape policy
//!
//! Leaves larger than [`LEAF_MAX_BYTES`] are split around
//! [`LEAF_TARGET_BYTES`]; adjacent leaf siblings are merged after removals
//! while their combined size stays within [`LEAF_MAX_BYTES`]; internal
//! nodes split beyond [`MAX_CHILDREN`] children and collapse when a
//! removal leaves exactly one child. Deletions do not enforce a strict
//! minimum occupancy beyond that (the same lazy-balance trade-off the
//! rope this replaces makes), so the proptest parity suite below, not a
//! shape invariant, is the correctness gate.
//!
//! Edits are implemented as consuming recursion: a path of nodes is
//! rebuilt bottom-up with summaries recomputed from the returned children
//! at every level, so no separate path-repair bookkeeping exists. Because
//! a `\n` is one byte and chunks split on char boundaries, a line break
//! can never span two chunks and the summaries need no boundary fix-ups.

use std::ops::Range;

/// A leaf chunk larger than this is split after an insert.
const LEAF_MAX_BYTES: usize = 2048;
/// Split leaves so both halves stay near this size.
const LEAF_TARGET_BYTES: usize = 1024;
/// Internal nodes split beyond this child count.
const MAX_CHILDREN: usize = 8;

/// Aggregated subtree statistics.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Summary {
    bytes: usize,
    chars: usize,
    lines: usize,
}

impl Summary {
    fn combine(left: Summary, right: Summary) -> Summary {
        Summary {
            bytes: left.bytes.saturating_add(right.bytes),
            chars: left.chars.saturating_add(right.chars),
            lines: left.lines.saturating_add(right.lines),
        }
    }
}

fn leaf_summary(text: &str) -> Summary {
    let bytes = text.as_bytes();
    let mut chars = 0usize;
    let mut lines = 0usize;
    for &b in bytes {
        if b & 0xC0 != 0x80 {
            chars += 1;
        }
        if b == b'\n' {
            lines += 1;
        }
    }
    Summary {
        bytes: bytes.len(),
        chars,
        lines,
    }
}

#[derive(Clone, Debug)]
enum Node {
    Leaf(String),
    Internal {
        children: Vec<Node>,
        summaries: Vec<Summary>,
        total: Summary,
    },
}

impl Node {
    fn summary(&self) -> Summary {
        match self {
            Node::Leaf(text) => leaf_summary(text),
            Node::Internal { total, .. } => *total,
        }
    }

    fn internal_from_children(children: Vec<Node>) -> Node {
        debug_assert!(!children.is_empty() && children.len() <= MAX_CHILDREN);
        let mut summaries = Vec::with_capacity(children.len());
        let mut total = Summary::default();
        for child in &children {
            let summary = child.summary();
            total = Summary::combine(total, summary);
            summaries.push(summary);
        }
        Node::Internal {
            children,
            summaries,
            total,
        }
    }
}

/// Resolved position inside a leaf during a descent. The `*_before`
/// fields accumulate the summaries of every fully skipped sibling, so a
/// leaf-local answer plus its prefix is a document answer.
#[derive(Clone, Copy, Debug)]
struct LeafLoc<'a> {
    leaf: &'a str,
    bytes_before: usize,
    chars_before: usize,
    lines_before: usize,
    local_byte: usize,
}

enum Axis {
    Chars(usize),
    Bytes(usize),
    Lines(usize),
}

/// Qem-native edited buffer: a B-tree of UTF-8 chunks with `O(log N)`
/// char/byte/line navigation. See the [module documentation](self) for
/// the semantics contract.
#[derive(Clone, Debug)]
pub(crate) struct EditBuffer {
    root: Node,
}

impl EditBuffer {
    pub(crate) fn new() -> Self {
        Self {
            root: Node::Leaf(String::new()),
        }
    }

    /// Builds a buffer from LF-normalized text.
    pub(crate) fn from_str(text: &str) -> Self {
        debug_assert!(!text.contains('\r'), "expected LF-normalized text");
        let mut builder = EditBufferBuilder::new();
        builder.append(text);
        builder.finish()
    }

    pub(crate) fn len_chars(&self) -> usize {
        self.root.summary().chars
    }

    pub(crate) fn len_bytes(&self) -> usize {
        self.root.summary().bytes
    }

    /// `\n` count plus one.
    pub(crate) fn len_lines(&self) -> usize {
        self.root.summary().lines + 1
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.len_bytes() == 0
    }

    /// Converts a char index to a byte offset.
    ///
    /// # Panics
    /// Panics when `char_idx` is past the end of the buffer.
    pub(crate) fn char_to_byte(&self, char_idx: usize) -> usize {
        assert!(char_idx <= self.len_chars(), "char index out of bounds");
        let loc = self.locate(Axis::Chars(char_idx));
        loc.bytes_before + loc.local_byte
    }

    /// Converts a byte offset to a char index.
    ///
    /// # Panics
    /// Panics when `byte_idx` is not on a UTF-8 char boundary or is past
    /// the end of the buffer.
    pub(crate) fn byte_to_char(&self, byte_idx: usize) -> usize {
        assert!(byte_idx <= self.len_bytes(), "byte offset out of bounds");
        let loc = self.locate(Axis::Bytes(byte_idx));
        assert!(
            loc.local_byte == loc.leaf.len() || loc.leaf.as_bytes()[loc.local_byte] & 0xC0 != 0x80,
            "byte offset not on a char boundary"
        );
        loc.chars_before + count_prefix_chars(&loc.leaf.as_bytes()[..loc.local_byte])
    }

    /// Converts a char index to the index of the line containing it. The
    /// `\n` char belongs to the line it ends.
    ///
    /// # Panics
    /// Panics when `char_idx` is past the end of the buffer.
    pub(crate) fn char_to_line(&self, char_idx: usize) -> usize {
        assert!(char_idx <= self.len_chars(), "char index out of bounds");
        let loc = self.locate(Axis::Chars(char_idx));
        loc.lines_before + count_line_breaks(&loc.leaf.as_bytes()[..loc.local_byte])
    }

    /// Converts a line index to the char index of the line's first char.
    ///
    /// # Panics
    /// Panics when `line_idx` is past `len_lines()`.
    pub(crate) fn line_to_char(&self, line_idx: usize) -> usize {
        assert!(line_idx < self.len_lines(), "line index out of bounds");
        let loc = self.locate(Axis::Lines(line_idx));
        loc.chars_before + count_prefix_chars(&loc.leaf.as_bytes()[..loc.local_byte])
    }

    /// Char range of the line at `line_idx`, excluding its break.
    ///
    /// # Panics
    /// Panics when `line_idx` is past `len_lines()`.
    pub(crate) fn line_char_range(&self, line_idx: usize) -> Range<usize> {
        assert!(line_idx < self.len_lines(), "line index out of bounds");
        let start = self.line_to_char(line_idx);
        if line_idx + 1 == self.len_lines() {
            return start..self.len_chars();
        }
        // The break is exactly one `\n` char directly before the next
        // line's first char.
        start..(self.line_to_char(line_idx + 1) - 1)
    }

    /// Returns the char at `char_idx`.
    ///
    /// # Panics
    /// Panics when `char_idx` is past the end of the buffer.
    pub(crate) fn char(&self, char_idx: usize) -> char {
        assert!(char_idx < self.len_chars(), "char index out of bounds");
        let loc = self.locate(Axis::Chars(char_idx));
        loc.leaf[loc.local_byte..]
            .chars()
            .next()
            .expect("char axis always resolves to a char boundary")
    }

    pub(crate) fn is_char_boundary(&self, byte_idx: usize) -> bool {
        let len = self.len_bytes();
        if byte_idx == 0 || byte_idx == len {
            return byte_idx <= len;
        }
        if byte_idx > len {
            return false;
        }
        let loc = self.locate(Axis::Bytes(byte_idx));
        loc.leaf.as_bytes()[loc.local_byte] & 0xC0 != 0x80
    }

    /// Inserts LF-normalized `text` at `char_idx`.
    ///
    /// # Panics
    /// Panics when `char_idx` is past the end of the buffer, or (in debug
    /// builds) when `text` still contains `\r`.
    pub(crate) fn insert(&mut self, char_idx: usize, text: &str) {
        assert!(char_idx <= self.len_chars(), "char index out of bounds");
        debug_assert!(!text.contains('\r'), "expected LF-normalized text");
        if text.is_empty() {
            return;
        }
        let old_root = std::mem::replace(&mut self.root, Node::Leaf(String::new()));
        let nodes = insert_rec(old_root, char_idx, text);
        self.root = join_roots(nodes);
    }

    /// Removes the char range `range`.
    ///
    /// # Panics
    /// Panics when the range is out of bounds.
    pub(crate) fn remove(&mut self, range: Range<usize>) {
        let len = self.len_chars();
        assert!(
            range.start <= range.end && range.end <= len,
            "char range out of bounds"
        );
        if range.start == range.end {
            return;
        }
        let old_root = std::mem::replace(&mut self.root, Node::Leaf(String::new()));
        self.root = match remove_rec(old_root, range.start, range.end) {
            Some(node) => node,
            None => Node::Leaf(String::new()),
        };
    }

    /// Copies the char range out of the buffer.
    ///
    /// # Panics
    /// Panics when the range is out of bounds.
    pub(crate) fn slice_to_string(&self, range: Range<usize>) -> String {
        assert!(range.start <= range.end && range.end <= self.len_chars());
        let start_byte = self.char_to_byte(range.start);
        let end_byte = self.char_to_byte(range.end);
        let mut out = String::with_capacity(end_byte - start_byte);
        for (base, text) in self.chunks_with_byte_offsets() {
            let chunk_end = base + text.len();
            if chunk_end <= start_byte {
                continue;
            }
            if base >= end_byte {
                break;
            }
            let from = start_byte.saturating_sub(base);
            let to = (end_byte - base).min(text.len());
            out.push_str(&text[from..to]);
        }
        out
    }

    /// Leaf chunks in document order. Chunks are an implementation
    /// detail: only the concatenation is contractual.
    pub(crate) fn chunks(&self) -> Chunks<'_> {
        Chunks {
            stack: vec![(&self.root, 0)],
        }
    }

    /// Chunk byte offsets paired with chunk text, in order. Streaming
    /// search walks these forward or in reverse and needs each chunk's
    /// absolute byte offset.
    pub(crate) fn chunks_with_byte_offsets(&self) -> Vec<(usize, &str)> {
        let mut out = Vec::new();
        let mut base = 0usize;
        for text in self.chunks() {
            out.push((base, text));
            base += text.len();
        }
        out
    }

    pub(crate) fn bytes(&self) -> impl Iterator<Item = u8> + '_ {
        self.chunks().flat_map(str::as_bytes).copied()
    }

    /// Walks to the leaf resolving `axis` and returns the leaf, the
    /// combined summaries of every skipped sibling, and the leaf-local
    /// byte offset for the axis target.
    ///
    /// Descent rule: enter the child whose cumulative range strictly
    /// contains the remaining target, subtracting skipped children from
    /// the running target and accumulating their summaries; the last
    /// child is the fallback and its leaf handler clamps the remainder.
    fn locate(&self, axis: Axis) -> LeafLoc<'_> {
        let mut acc = Summary::default();
        let mut node = &self.root;
        let mut remaining = match axis {
            Axis::Chars(n) | Axis::Bytes(n) | Axis::Lines(n) => n,
        };
        loop {
            match node {
                Node::Leaf(text) => {
                    let local_byte = match axis {
                        Axis::Chars(_) => local_char_to_byte(text, remaining),
                        Axis::Bytes(_) => remaining.min(text.len()),
                        Axis::Lines(_) => line_start_byte_in_leaf(text, remaining),
                    };
                    return LeafLoc {
                        leaf: text,
                        bytes_before: acc.bytes,
                        chars_before: acc.chars,
                        lines_before: acc.lines,
                        local_byte,
                    };
                }
                Node::Internal {
                    children,
                    summaries,
                    ..
                } => {
                    let last = children.len() - 1;
                    let mut chosen = last;
                    for (index, summary) in summaries.iter().enumerate() {
                        if index == last {
                            break;
                        }
                        let field = match axis {
                            Axis::Chars(_) => summary.chars,
                            Axis::Bytes(_) => summary.bytes,
                            Axis::Lines(_) => summary.lines,
                        };
                        // For the line axis a target equal to the child's
                        // break count resolves at the child's end / the
                        // next child's start, so `<=` enters; char and
                        // byte targets at a boundary resolve in the next
                        // child just as well, so a strict `<` suffices.
                        let enters = match axis {
                            Axis::Lines(_) => remaining <= field,
                            Axis::Chars(_) | Axis::Bytes(_) => remaining < field,
                        };
                        if enters {
                            chosen = index;
                            break;
                        }
                        remaining -= field;
                        acc = Summary::combine(acc, *summary);
                    }
                    node = &children[chosen];
                }
            }
        }
    }
}

fn join_roots(nodes: Vec<Node>) -> Node {
    if nodes.len() == 1 {
        nodes.into_iter().next().expect("non-empty node list")
    } else {
        // A large insert can split one leaf into many replacement nodes;
        // group them bottom-up so every internal node stays within the
        // child-count invariant.
        build_bottom_up(nodes)
    }
}

impl std::fmt::Display for EditBuffer {
    /// Writes every leaf chunk in order, so `to_string()` (via the trait)
    /// yields exactly the buffer text, matching the rope this replaces.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for text in self.chunks() {
            f.write_str(text)?;
        }
        Ok(())
    }
}

/// Inserts `text` at `char_idx` (child-local) and returns the one or more
/// nodes replacing `node` after leaf/internal splits.
fn insert_rec(node: Node, char_idx: usize, text: &str) -> Vec<Node> {
    match node {
        Node::Leaf(mut s) => {
            let at = local_char_to_byte(&s, char_idx);
            s.insert_str(at, text);
            split_leaf(s)
        }
        Node::Internal { children, .. } => {
            let mut remaining = char_idx;
            let mut chosen = children.len() - 1;
            for (index, child) in children.iter().enumerate() {
                if index + 1 == children.len() {
                    break;
                }
                let chars = child.summary().chars;
                if remaining < chars {
                    chosen = index;
                    break;
                }
                remaining -= chars;
            }
            let mut children = children;
            let replacement = insert_rec(children.remove(chosen), remaining, text);
            // `chosen` indexes the pre-removal child list; after `remove`
            // the list is one shorter, so the replacement goes back at the
            // same position through an empty splice range.
            children.splice(chosen..chosen, replacement);
            split_internal(children)
        }
    }
}

/// Removes the child-local char range `start..end`. Returns `None` when
/// the subtree became empty, and a bare child when only one child is
/// left (collapsing the internal node into its parent).
fn remove_rec(node: Node, start: usize, end: usize) -> Option<Node> {
    match node {
        Node::Leaf(mut s) => {
            let from = local_char_to_byte(&s, start.min(s.chars().count()));
            let to = local_char_to_byte(&s, end.min(s.chars().count()));
            s.replace_range(from..to, "");
            Some(Node::Leaf(s))
        }
        Node::Internal { children, .. } => {
            let mut base = 0usize;
            let mut kept: Vec<Node> = Vec::with_capacity(children.len());
            for child in children {
                let summary = child.summary();
                let from = start.saturating_sub(base).min(summary.chars);
                let to = end.saturating_sub(base).min(summary.chars);
                base += summary.chars;
                if from == 0 && to == summary.chars {
                    continue; // fully inside the removed range
                }
                if from == to {
                    kept.push(child);
                    continue;
                }
                if let Some(replacement) = remove_rec(child, from, to) {
                    kept.push(replacement);
                }
            }
            if kept.is_empty() {
                return None;
            }
            let kept = merge_adjacent_leaves(kept);
            if kept.len() == 1 {
                return kept.into_iter().next();
            }
            Some(Node::internal_from_children(kept))
        }
    }
}

fn split_leaf(s: String) -> Vec<Node> {
    if s.len() <= LEAF_MAX_BYTES {
        return vec![Node::Leaf(s)];
    }
    let mut out = Vec::new();
    let mut rest = s;
    while rest.len() > LEAF_MAX_BYTES {
        let cut = floor_char_boundary(&rest, LEAF_TARGET_BYTES);
        let head: String = rest.drain(..cut).collect();
        out.push(Node::Leaf(head));
    }
    out.push(Node::Leaf(rest));
    out
}

fn split_internal(children: Vec<Node>) -> Vec<Node> {
    if children.len() <= MAX_CHILDREN {
        return vec![Node::internal_from_children(children)];
    }
    let mut out = Vec::new();
    let mut rest = children;
    while rest.len() > MAX_CHILDREN {
        let tail = rest.split_off(MAX_CHILDREN / 2);
        out.push(Node::internal_from_children(rest));
        rest = tail;
    }
    out.push(Node::internal_from_children(rest));
    out
}

/// Merges adjacent leaf siblings while the combined size stays within
/// one leaf split threshold, so removals shrink the tree back toward the
/// target chunk size.
fn merge_adjacent_leaves(children: Vec<Node>) -> Vec<Node> {
    let mut out: Vec<Node> = Vec::with_capacity(children.len());
    for child in children {
        let merge = match (out.last_mut(), &child) {
            (Some(Node::Leaf(prev)), Node::Leaf(next))
                if prev.len() + next.len() <= LEAF_MAX_BYTES =>
            {
                prev.push_str(next);
                true
            }
            _ => false,
        };
        if !merge {
            out.push(child);
        }
    }
    out
}

/// Lazy forward iterator over leaf chunks.
pub(crate) struct Chunks<'a> {
    stack: Vec<(&'a Node, usize)>,
}

impl<'a> Iterator for Chunks<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<&'a str> {
        loop {
            let (node, index) = self.stack.last_mut()?;
            match node {
                Node::Leaf(text) => {
                    self.stack.pop();
                    if !text.is_empty() {
                        return Some(text);
                    }
                }
                Node::Internal { children, .. } => {
                    if *index < children.len() {
                        let child = &children[*index];
                        *index += 1;
                        self.stack.push((child, 0));
                    } else {
                        self.stack.pop();
                    }
                }
            }
        }
    }
}

fn local_char_to_byte(text: &str, chars: usize) -> usize {
    text.char_indices()
        .nth(chars)
        .map_or(text.len(), |(byte, _)| byte)
}

fn count_prefix_chars(bytes: &[u8]) -> usize {
    bytes.iter().filter(|&&b| b & 0xC0 != 0x80).count()
}

fn count_line_breaks(bytes: &[u8]) -> usize {
    bytes.iter().filter(|&&b| b == b'\n').count()
}

/// Byte offset of the start of `line_index` within a leaf: just after
/// the `line_index`-th `\n`. When the leaf does not contain that many
/// breaks the leaf length is returned, which is the correct clamped
/// answer for a line that starts in a later sibling (or at the document
/// end).
fn line_start_byte_in_leaf(text: &str, line_index: usize) -> usize {
    if line_index == 0 {
        return 0;
    }
    let bytes = text.as_bytes();
    let mut seen = 0usize;
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'\n' {
            seen += 1;
            if seen == line_index {
                return i + 1;
            }
        }
    }
    bytes.len()
}

fn floor_char_boundary(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

/// Bulk loader for building an [`EditBuffer`] from large LF-normalized
/// text without paying per-insert tree churn.
#[derive(Debug, Default)]
pub(crate) struct EditBufferBuilder {
    chunks: Vec<String>,
    pending: String,
}

impl EditBufferBuilder {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn append(&mut self, text: &str) {
        debug_assert!(!text.contains('\r'), "expected LF-normalized text");
        self.pending.push_str(text);
        while self.pending.len() >= LEAF_TARGET_BYTES * 2 {
            let split_at = floor_char_boundary(&self.pending, LEAF_TARGET_BYTES);
            let head: String = self.pending.drain(..split_at).collect();
            self.chunks.push(head);
        }
    }

    pub(crate) fn finish(self) -> EditBuffer {
        let mut chunks = self.chunks;
        if !self.pending.is_empty() || chunks.is_empty() {
            chunks.push(self.pending);
        }
        if chunks.len() == 1 {
            return EditBuffer {
                root: Node::Leaf(chunks.pop().expect("non-empty chunk list")),
            };
        }
        let leaves: Vec<Node> = chunks.into_iter().map(Node::Leaf).collect();
        EditBuffer {
            root: build_bottom_up(leaves),
        }
    }
}

fn build_bottom_up(mut level: Vec<Node>) -> Node {
    while level.len() > MAX_CHILDREN {
        let mut next = Vec::new();
        for group in level.chunks(MAX_CHILDREN) {
            next.push(Node::internal_from_children(group.to_vec()));
        }
        level = next;
    }
    Node::internal_from_children(level)
}

#[cfg(test)]
mod tests {
    use super::{EditBuffer, EditBufferBuilder};

    fn model_lines(model: &str) -> usize {
        model.matches('\n').count() + 1
    }

    fn model_byte_of_char(model: &str, char_idx: usize) -> usize {
        model
            .char_indices()
            .nth(char_idx)
            .map_or(model.len(), |(b, _)| b)
    }

    fn model_char_to_line(model: &str, char_idx: usize) -> usize {
        let byte = model_byte_of_char(model, char_idx);
        model.as_bytes()[..byte]
            .iter()
            .filter(|&&b| b == b'\n')
            .count()
    }

    fn model_line_to_char(model: &str, line: usize) -> usize {
        if line == 0 {
            return 0;
        }
        let bytes = model.as_bytes();
        let mut seen = 0usize;
        for (i, &b) in bytes.iter().enumerate() {
            if b == b'\n' {
                seen += 1;
                if seen == line {
                    // Char index of the byte right after the `\n`.
                    let mut chars = 0usize;
                    for byte in &bytes[..i + 1] {
                        if byte & 0xC0 != 0x80 {
                            chars += 1;
                        }
                    }
                    return chars;
                }
            }
        }
        model.chars().count()
    }

    fn model_line_end_char(model: &str, line: usize) -> usize {
        if line + 1 == model_lines(model) {
            return model.chars().count();
        }
        model_line_to_char(model, line + 1) - 1
    }

    fn assert_matches_model(buffer: &EditBuffer, model: &str) {
        assert_eq!(buffer.to_string(), model);
        assert_eq!(buffer.len_chars(), model.chars().count());
        assert_eq!(buffer.len_bytes(), model.len());
        assert_eq!(buffer.len_lines(), model_lines(model));

        let concat: String = buffer.chunks().collect();
        assert_eq!(concat, model);
        for (base, chunk) in buffer.chunks_with_byte_offsets() {
            assert!(chunk.is_empty() || model.is_char_boundary(base));
        }

        let chars = model.chars().count();
        for i in (0..=chars).filter(|i| i % 7 == 0 || *i == chars) {
            let byte = model_byte_of_char(model, i);
            assert_eq!(buffer.char_to_byte(i), byte, "char_to_byte({i})");
            assert_eq!(buffer.byte_to_char(byte), i, "byte_to_char({byte})");
            assert_eq!(
                buffer.char_to_line(i),
                model_char_to_line(model, i),
                "char_to_line({i})"
            );
        }

        for line in 0..model_lines(model) {
            let start = model_line_to_char(model, line);
            let end = model_line_end_char(model, line);
            assert_eq!(buffer.line_to_char(line), start, "line_to_char({line})");
            let range = buffer.line_char_range(line);
            assert_eq!(range.start, start, "line_char_range({line}).start");
            assert_eq!(range.end, end, "line_char_range({line}).end");
            assert_eq!(
                buffer.slice_to_string(range.clone()),
                model
                    .chars()
                    .skip(start)
                    .take(end - start)
                    .collect::<String>(),
                "slice of line {line}"
            );
        }

        for (byte, ch) in model.char_indices() {
            if byte % 5 != 0 && byte + 1 != model.len() {
                continue;
            }
            assert_eq!(
                buffer.char(buffer.byte_to_char(byte)),
                ch,
                "char at byte {byte}"
            );
            assert!(buffer.is_char_boundary(byte), "boundary at {byte}");
        }
    }

    #[test]
    fn build_and_navigate_multichunk() {
        let text = "ж".repeat(900) + &"中".repeat(900) + "ascii-tail\nsecond\n";
        let buffer = EditBuffer::from_str(&text);
        assert!(buffer.chunks().count() > 2, "expected multiple leaves");
        assert_matches_model(&buffer, &text);
    }

    #[test]
    fn line_break_at_chunk_start_and_end() {
        // Chunk 1 ends right before a `\n`; the next chunk opens with it.
        let mut builder = EditBufferBuilder::new();
        builder.append(&"x".repeat(1024));
        builder.append(&format!("\n{}", "y".repeat(1024)));
        let buffer = builder.finish();
        assert_eq!(buffer.chunks().count(), 2, "expected two leaves");
        assert_eq!(buffer.len_lines(), 2);
        assert_eq!(buffer.line_to_char(1), 1025);
        assert_eq!(buffer.char_to_line(1024), 0);
        assert_eq!(buffer.char_to_line(1025), 1);

        let text = format!("{}\n{}", "x".repeat(1024), "y".repeat(1024));
        assert_matches_model(&buffer, &text);
    }

    #[test]
    fn insert_and_remove_round_trip_against_string() {
        let mut buffer = EditBuffer::new();
        let mut model = String::new();
        buffer.insert(0, "hello\nworld");
        model.push_str("hello\nworld");
        buffer.insert(5, "\n");
        model.insert(5, '\n');
        assert_matches_model(&buffer, &model);

        buffer.remove(5..6);
        model.replace_range(5..6, "");
        assert_matches_model(&buffer, &model);

        // Cross-chunk removal.
        buffer.insert(buffer.len_chars(), &"n".repeat(4000));
        model.push_str(&"n".repeat(4000));
        buffer.remove(3..3990);
        model.replace_range(3..3990, "");
        assert_matches_model(&buffer, &model);

        buffer.remove(0..buffer.len_chars());
        model.clear();
        assert_matches_model(&buffer, &model);
    }

    #[test]
    fn typing_pattern_stays_correct() {
        let mut buffer = EditBuffer::new();
        let mut model = String::new();
        for step in 0..600 {
            let text = if step % 3 == 0 {
                "ж"
            } else if step % 3 == 1 {
                "a"
            } else {
                "\n"
            };
            buffer.insert(buffer.len_chars(), text);
            model.push_str(text);
        }
        for step in 0..200 {
            let at = step * 2 % (model.chars().count() + 1);
            buffer.insert(at, "Z");
            let byte = model_byte_of_char(&model, at);
            model.insert(byte, 'Z');
        }
        for step in 0..200 {
            let len = model.chars().count();
            let start = step * 3 % (len + 1);
            let end = (start + 2).min(len);
            buffer.remove(start..end);
            let sb = model_byte_of_char(&model, start);
            let eb = model_byte_of_char(&model, end);
            model.replace_range(sb..eb, "");
        }
        assert_matches_model(&buffer, &model);
    }

    #[test]
    fn empty_and_singleton_edges() {
        let mut buffer = EditBuffer::new();
        assert_eq!(buffer.len_lines(), 1);
        assert_matches_model(&buffer, "");

        buffer.remove(0..0);
        buffer.insert(0, "a");
        buffer.remove(0..1);
        assert_matches_model(&buffer, "");

        let single = EditBuffer::from_str("x");
        assert_matches_model(&single, "x");

        let only_breaks = EditBuffer::from_str("\n\n\n");
        assert_matches_model(&only_breaks, "\n\n\n");
    }
}

#[cfg(test)]
mod parity_tests {
    use super::EditBuffer;
    use proptest::prelude::*;

    /// Model oracles over a plain `String`, mirroring the LF-only
    /// semantics contract (see the module docs).
    mod oracle {
        pub(super) fn len_lines(model: &str) -> usize {
            model.matches('\n').count() + 1
        }

        pub(super) fn char_to_line(model: &str, char_idx: usize) -> usize {
            let byte = model
                .char_indices()
                .nth(char_idx)
                .map_or(model.len(), |(b, _)| b);
            model.as_bytes()[..byte]
                .iter()
                .filter(|&&b| b == b'\n')
                .count()
        }

        pub(super) fn line_to_char(model: &str, line: usize) -> usize {
            if line == 0 {
                return 0;
            }
            let bytes = model.as_bytes();
            let mut seen = 0usize;
            for (i, &b) in bytes.iter().enumerate() {
                if b == b'\n' {
                    seen += 1;
                    if seen == line {
                        // Char index of the byte right after the `\n`.
                        let mut chars = 0usize;
                        for byte in &bytes[..i + 1] {
                            if byte & 0xC0 != 0x80 {
                                chars += 1;
                            }
                        }
                        return chars;
                    }
                }
            }
            model.chars().count()
        }
    }

    use oracle::*;

    #[derive(Debug, Clone)]
    enum Op {
        Insert(usize, String),
        Remove(usize, usize),
    }

    fn op_strategy() -> impl Strategy<Value = Op> {
        prop::char::range('\u{20}', '\u{4E00}')
            .prop_flat_map(|base| {
                prop::strategy::Union::new_weighted(vec![
                    (
                        3,
                        prop::collection::vec(
                            prop::strategy::Union::new_weighted(vec![
                                (6, Just(base)),
                                (1, Just('\n')),
                                (1, Just('😀')),
                                (1, Just('ж')),
                            ]),
                            0..40,
                        )
                        .prop_map(String::from_iter)
                        .boxed(),
                    ),
                    (2, Just(String::new()).boxed()),
                ])
            })
            .prop_flat_map(|text| {
                prop::strategy::Union::new_weighted(vec![
                    (
                        5,
                        (0usize..8192, Just(text))
                            .prop_map(|(at, text)| Op::Insert(at, text))
                            .boxed(),
                    ),
                    (
                        3,
                        (0usize..8192, 0usize..8192)
                            .prop_map(|(a, b)| {
                                if a <= b {
                                    Op::Remove(a, b)
                                } else {
                                    Op::Remove(b, a)
                                }
                            })
                            .boxed(),
                    ),
                ])
            })
    }

    proptest! {
        #![proptest_config(proptest::test_runner::Config {
            cases: 256,
            max_shrink_iters: 4096,
            ..proptest::test_runner::Config::default()
        })]

        #[test]
        fn buffer_matches_string_oracle(ops in prop::collection::vec(op_strategy(), 0..300)) {
            let mut buffer = EditBuffer::new();
            let mut model = String::new();
            for step in ops {
                match step {
                    Op::Insert(at, text) => {
                        let clamped = at.min(model.chars().count());
                        buffer.insert(clamped, &text);
                        let byte = model
                            .char_indices()
                            .nth(clamped)
                            .map_or(model.len(), |(b, _)| b);
                        model.insert_str(byte, &text);
                    }
                    Op::Remove(a, b) => {
                        let len = model.chars().count();
                        let start = a.min(len);
                        let end = b.clamp(start, len);
                        buffer.remove(start..end);
                        let sb = model
                            .char_indices()
                            .nth(start)
                            .map_or(model.len(), |(byte, _)| byte);
                        let eb = model
                            .char_indices()
                            .nth(end)
                            .map_or(model.len(), |(byte, _)| byte);
                        model.replace_range(sb..eb, "");
                    }
                }

                prop_assert_eq!(buffer.to_string(), model.as_str());
                prop_assert_eq!(buffer.len_chars(), model.chars().count());
                prop_assert_eq!(buffer.len_bytes(), model.len());
                prop_assert_eq!(buffer.len_lines(), len_lines(&model));

                let concat: String = buffer.chunks().collect();
                prop_assert_eq!(concat, model.as_str());
                for (base, chunk) in buffer.chunks_with_byte_offsets() {
                    prop_assert!(chunk.is_empty() || model.is_char_boundary(base));
                }

                let chars = model.chars().count();
                for probe in [0usize, chars / 3, chars / 2, chars.saturating_sub(1), chars] {
                    let byte = model
                        .char_indices()
                        .nth(probe)
                        .map_or(model.len(), |(b, _)| b);
                    prop_assert_eq!(buffer.char_to_byte(probe), byte);
                    prop_assert_eq!(buffer.byte_to_char(byte), probe);
                    prop_assert_eq!(buffer.char_to_line(probe), char_to_line(&model, probe));
                }

                let lines = len_lines(&model);
                for line in [0usize, lines / 2, lines.saturating_sub(1)] {
                    prop_assert_eq!(buffer.line_to_char(line), line_to_char(&model, line));
                }

                if chars >= 2 {
                    let mid = chars / 2;
                    let expected: String = model.chars().skip(1).take(mid - 1).collect();
                    prop_assert_eq!(buffer.slice_to_string(1..mid), expected);
                }
            }
        }
    }
}
