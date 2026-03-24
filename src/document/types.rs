use super::LineEnding;
use std::fmt;
use std::io;
use std::ops::Deref;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LineSlice {
    text: String,
    exact: bool,
}

impl LineSlice {
    /// Creates a new line slice and marks whether it is exact.
    pub fn new(text: String, exact: bool) -> Self {
        Self { text, exact }
    }

    /// Returns the slice text.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Consumes the slice and returns the owned text.
    pub fn into_text(self) -> String {
        self.text
    }

    /// Returns `true` if the slice was produced from exact indexes rather than heuristics.
    pub fn is_exact(&self) -> bool {
        self.exact
    }

    /// Returns `true` if the slice is empty.
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }
}

impl AsRef<str> for LineSlice {
    fn as_ref(&self) -> &str {
        self.text()
    }
}

impl Deref for LineSlice {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.text()
    }
}

impl fmt::Display for LineSlice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.text())
    }
}

impl From<LineSlice> for String {
    fn from(value: LineSlice) -> Self {
        value.into_text()
    }
}

/// Text slice returned by typed range/selection reads.
///
/// The slice applies lossy UTF-8 decoding and tracks whether the underlying
/// range was anchored by exact document indexes or a heuristic mmap guess.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TextSlice {
    text: String,
    exact: bool,
}

impl TextSlice {
    /// Creates a new text slice and marks whether it is exact.
    pub fn new(text: String, exact: bool) -> Self {
        Self { text, exact }
    }

    /// Returns the slice text.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Consumes the slice and returns the owned text.
    pub fn into_text(self) -> String {
        self.text
    }

    /// Returns `true` if the slice was produced from exact indexes rather than heuristics.
    pub fn is_exact(&self) -> bool {
        self.exact
    }

    /// Returns `true` if the slice is empty.
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }
}

impl AsRef<str> for TextSlice {
    fn as_ref(&self) -> &str {
        self.text()
    }
}

impl Deref for TextSlice {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.text()
    }
}

impl fmt::Display for TextSlice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.text())
    }
}

impl From<TextSlice> for String {
    fn from(value: TextSlice) -> Self {
        value.into_text()
    }
}

/// Zero-based document position used by frontend integrations.
///
/// Qem keeps positions in document coordinates instead of screen coordinates so
/// applications remain free to implement their own cursor, scrollbar, and
/// selection rendering. `col0` uses document text columns: for UTF-8 text this
/// means Unicode scalar values, not grapheme clusters and not terminal/display
/// cells.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TextPosition {
    line0: usize,
    col0: usize,
}

impl TextPosition {
    /// Creates a zero-based text position.
    pub const fn new(line0: usize, col0: usize) -> Self {
        Self { line0, col0 }
    }

    /// Returns the zero-based line index.
    pub const fn line0(self) -> usize {
        self.line0
    }

    /// Returns the zero-based document column index in text units.
    pub const fn col0(self) -> usize {
        self.col0
    }
}

impl From<(usize, usize)> for TextPosition {
    fn from(value: (usize, usize)) -> Self {
        Self::new(value.0, value.1)
    }
}

impl From<TextPosition> for (usize, usize) {
    fn from(value: TextPosition) -> Self {
        (value.line0, value.col0)
    }
}

/// Typed text range used by edit operations.
///
/// The range is expressed as a starting position together with a text-unit
/// length, matching the semantics of
/// [`crate::document::Document::try_replace_range`]. For UTF-8 text, line-local
/// units are Unicode scalar values rather than grapheme clusters or display
/// cells. Between lines, a stored CRLF sequence still counts as one text unit.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct TextRange {
    start: TextPosition,
    len_chars: usize,
}

impl TextRange {
    /// Creates a text range from a starting position and text-unit length.
    pub const fn new(start: TextPosition, len_chars: usize) -> Self {
        Self { start, len_chars }
    }

    /// Creates an empty text range at the given position.
    pub const fn empty(start: TextPosition) -> Self {
        Self::new(start, 0)
    }

    /// Returns the starting position of the range.
    pub const fn start(self) -> TextPosition {
        self.start
    }

    /// Returns the number of text units in the range.
    pub const fn len_chars(self) -> usize {
        self.len_chars
    }

    /// Returns `true` when the range is empty.
    pub const fn is_empty(self) -> bool {
        self.len_chars == 0
    }
}

/// Typed literal-search match within the current document contents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SearchMatch {
    range: TextRange,
    end: TextPosition,
}

impl SearchMatch {
    /// Creates a search match from its range and end position.
    pub const fn new(range: TextRange, end: TextPosition) -> Self {
        Self { range, end }
    }

    /// Returns the typed range covered by the match.
    pub const fn range(self) -> TextRange {
        self.range
    }

    /// Returns the typed start position of the match.
    pub const fn start(self) -> TextPosition {
        self.range.start()
    }

    /// Returns the typed end position of the match.
    pub const fn end(self) -> TextPosition {
        self.end
    }

    /// Returns the match length in document text units.
    pub const fn len_chars(self) -> usize {
        self.range.len_chars()
    }

    /// Returns `true` when the match is empty.
    pub const fn is_empty(self) -> bool {
        self.range.is_empty()
    }

    /// Returns the match as an anchor/head selection.
    pub const fn selection(self) -> TextSelection {
        TextSelection::new(self.start(), self.end())
    }
}

/// Anchor/head text selection used by frontend integrations.
///
/// Qem keeps this selection in document coordinates so applications remain
/// free to own their own painting, cursor visuals, and interaction model.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct TextSelection {
    anchor: TextPosition,
    head: TextPosition,
}

impl TextSelection {
    /// Creates a selection from an anchor and active head position.
    pub const fn new(anchor: TextPosition, head: TextPosition) -> Self {
        Self { anchor, head }
    }

    /// Creates a caret selection at a single position.
    pub const fn caret(position: TextPosition) -> Self {
        Self::new(position, position)
    }

    /// Returns the anchor position.
    pub const fn anchor(self) -> TextPosition {
        self.anchor
    }

    /// Returns the active head position.
    pub const fn head(self) -> TextPosition {
        self.head
    }

    /// Returns `true` when the selection is only a caret.
    pub fn is_caret(self) -> bool {
        self.anchor == self.head
    }
}

/// Viewport request used by frontend code to read only visible rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViewportRequest {
    first_line0: usize,
    line_count: usize,
    start_col: usize,
    max_cols: usize,
}

impl Default for ViewportRequest {
    fn default() -> Self {
        Self::new(0, 0)
    }
}

impl ViewportRequest {
    /// Creates a viewport request for a contiguous line range.
    ///
    /// Columns default to the full visible width of the line. Horizontal
    /// columns use the same document text-unit semantics as [`TextPosition`].
    pub const fn new(first_line0: usize, line_count: usize) -> Self {
        Self {
            first_line0,
            line_count,
            start_col: 0,
            max_cols: usize::MAX,
        }
    }

    /// Sets the horizontal slice within each requested row.
    ///
    /// `start_col` and `max_cols` count document text columns, not grapheme
    /// clusters and not display cells.
    pub const fn with_columns(mut self, start_col: usize, max_cols: usize) -> Self {
        self.start_col = start_col;
        self.max_cols = max_cols;
        self
    }

    /// Returns the first requested zero-based line index.
    pub const fn first_line0(self) -> usize {
        self.first_line0
    }

    /// Returns the requested number of lines.
    pub const fn line_count(self) -> usize {
        self.line_count
    }

    /// Returns the requested starting column.
    pub const fn start_col(self) -> usize {
        self.start_col
    }

    /// Returns the requested maximum number of columns.
    pub const fn max_cols(self) -> usize {
        self.max_cols
    }
}

/// One row returned by a viewport read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewportRow {
    line0: usize,
    slice: LineSlice,
}

impl ViewportRow {
    /// Creates a viewport row.
    pub fn new(line0: usize, slice: LineSlice) -> Self {
        Self { line0, slice }
    }

    /// Returns the zero-based line index for this row.
    pub fn line0(&self) -> usize {
        self.line0
    }

    /// Returns the 1-based line number for display.
    pub fn line_number(&self) -> usize {
        self.line0.saturating_add(1)
    }

    /// Returns the rendered line slice.
    pub fn slice(&self) -> &LineSlice {
        &self.slice
    }

    /// Consumes the row and returns the line slice.
    pub fn into_slice(self) -> LineSlice {
        self.slice
    }

    /// Returns the row text.
    pub fn text(&self) -> &str {
        self.slice.text()
    }

    /// Returns `true` when the row is backed by exact line indexes.
    pub fn is_exact(&self) -> bool {
        self.slice.is_exact()
    }
}

/// Viewport read response returned by [`crate::document::Document::read_viewport`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Viewport {
    request: ViewportRequest,
    total_lines: LineCount,
    rows: Vec<ViewportRow>,
}

impl Viewport {
    /// Creates a viewport response.
    pub fn new(request: ViewportRequest, total_lines: LineCount, rows: Vec<ViewportRow>) -> Self {
        Self {
            request,
            total_lines,
            rows,
        }
    }

    /// Returns the request that produced this viewport.
    pub fn request(&self) -> ViewportRequest {
        self.request
    }

    /// Returns the current total document line count.
    pub fn total_lines(&self) -> LineCount {
        self.total_lines
    }

    /// Returns the visible rows.
    pub fn rows(&self) -> &[ViewportRow] {
        &self.rows
    }

    /// Consumes the viewport and returns the visible rows.
    pub fn into_rows(self) -> Vec<ViewportRow> {
        self.rows
    }

    /// Returns the number of visible rows.
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// Returns `true` when the viewport contains no rows.
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}

/// Result of an edit command together with the resulting cursor position.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EditResult {
    changed: bool,
    cursor: TextPosition,
}

impl EditResult {
    /// Creates an edit result.
    pub const fn new(changed: bool, cursor: TextPosition) -> Self {
        Self { changed, cursor }
    }

    /// Returns `true` when the document changed.
    pub const fn changed(self) -> bool {
        self.changed
    }

    /// Returns the resulting cursor position.
    pub const fn cursor(self) -> TextPosition {
        self.cursor
    }
}

/// Result of cutting a selection together with the resulting edit outcome.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CutResult {
    text: String,
    edit: EditResult,
}

impl CutResult {
    /// Creates a cut result.
    pub fn new(text: String, edit: EditResult) -> Self {
        Self { text, edit }
    }

    /// Returns the cut text.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Consumes the result and returns the owned cut text.
    pub fn into_text(self) -> String {
        self.text
    }

    /// Returns the underlying edit result.
    pub const fn edit(&self) -> EditResult {
        self.edit
    }

    /// Returns `true` when the document changed.
    pub const fn changed(&self) -> bool {
        self.edit.changed()
    }

    /// Returns the resulting cursor position.
    pub const fn cursor(&self) -> TextPosition {
        self.edit.cursor()
    }
}

/// Typed byte progress used by indexing and other document-local background work.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ByteProgress {
    completed_bytes: usize,
    total_bytes: usize,
}

impl ByteProgress {
    /// Creates a byte progress value.
    pub const fn new(completed_bytes: usize, total_bytes: usize) -> Self {
        Self {
            completed_bytes,
            total_bytes,
        }
    }

    /// Returns the completed byte count.
    pub const fn completed_bytes(self) -> usize {
        self.completed_bytes
    }

    /// Returns the total byte count.
    pub const fn total_bytes(self) -> usize {
        self.total_bytes
    }

    /// Returns completion as a `0.0..=1.0` fraction.
    pub fn fraction(self) -> f32 {
        if self.total_bytes == 0 {
            0.0
        } else {
            self.completed_bytes as f32 / self.total_bytes as f32
        }
    }
}

/// Total document line count, represented either as an exact value or as a
/// scrolling estimate while background indexing is still incomplete.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub enum LineCount {
    Exact(usize),
    Estimated(usize),
}

impl LineCount {
    /// Returns the exact line count when it is known.
    pub fn exact(self) -> Option<usize> {
        match self {
            Self::Exact(lines) => Some(lines),
            Self::Estimated(_) => None,
        }
    }

    /// Returns the value that should be used for viewport sizing and scrolling.
    pub fn display_rows(self) -> usize {
        match self {
            Self::Exact(lines) | Self::Estimated(lines) => lines.max(1),
        }
    }

    /// Returns `true` when the total line count is exact.
    pub fn is_exact(self) -> bool {
        matches!(self, Self::Exact(_))
    }
}

/// Current backing mode of the document text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DocumentBacking {
    Mmap,
    PieceTable,
    Rope,
}

impl DocumentBacking {
    /// Returns a short display label for the current backing mode.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Mmap => "mmap",
            Self::PieceTable => "piece-table",
            Self::Rope => "rope",
        }
    }
}

/// Typed editability state for a document position or range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EditCapability {
    Editable {
        backing: DocumentBacking,
    },
    RequiresPromotion {
        from: DocumentBacking,
        to: DocumentBacking,
    },
    Unsupported {
        backing: DocumentBacking,
        reason: &'static str,
    },
}

impl EditCapability {
    /// Returns `true` when an edit can proceed, possibly after a backend promotion.
    pub const fn is_editable(self) -> bool {
        !matches!(self, Self::Unsupported { .. })
    }

    /// Returns `true` when the edit would require promoting to another backing.
    pub const fn requires_promotion(self) -> bool {
        matches!(self, Self::RequiresPromotion { .. })
    }

    /// Returns the current backing mode before any edit is attempted.
    pub const fn current_backing(self) -> DocumentBacking {
        match self {
            Self::Editable { backing } | Self::Unsupported { backing, .. } => backing,
            Self::RequiresPromotion { from, .. } => from,
        }
    }

    /// Returns the resulting backing mode after promotion, if one is required.
    pub const fn target_backing(self) -> Option<DocumentBacking> {
        match self {
            Self::RequiresPromotion { to, .. } => Some(to),
            _ => None,
        }
    }

    /// Returns an unsupported-edit reason when one is available.
    pub const fn reason(self) -> Option<&'static str> {
        match self {
            Self::Unsupported { reason, .. } => Some(reason),
            _ => None,
        }
    }
}

/// Snapshot of the current document state for frontend polling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentStatus {
    path: Option<PathBuf>,
    dirty: bool,
    file_len: usize,
    line_count: LineCount,
    line_ending: LineEnding,
    indexing: Option<ByteProgress>,
    backing: DocumentBacking,
}

impl DocumentStatus {
    /// Creates a document status snapshot.
    pub fn new(
        path: Option<PathBuf>,
        dirty: bool,
        file_len: usize,
        line_count: LineCount,
        line_ending: LineEnding,
        indexing: Option<ByteProgress>,
        backing: DocumentBacking,
    ) -> Self {
        Self {
            path,
            dirty,
            file_len,
            line_count,
            line_ending,
            indexing,
            backing,
        }
    }

    /// Returns the current document path, if one is set.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Returns `true` when the document has unsaved changes.
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Returns the current document length in bytes.
    pub fn file_len(&self) -> usize {
        self.file_len
    }

    /// Returns the current document line count.
    pub fn line_count(&self) -> LineCount {
        self.line_count
    }

    /// Returns the exact line count when it is known.
    pub fn exact_line_count(&self) -> Option<usize> {
        self.line_count.exact()
    }

    /// Returns the best-effort line count for viewport sizing and scrolling.
    pub fn display_line_count(&self) -> usize {
        self.line_count.display_rows()
    }

    /// Returns `true` when the current line count is exact.
    pub fn is_line_count_exact(&self) -> bool {
        self.line_count.is_exact()
    }

    /// Returns the currently detected line ending style.
    pub fn line_ending(&self) -> LineEnding {
        self.line_ending
    }

    /// Returns typed indexing progress while background indexing is active.
    pub fn indexing_state(&self) -> Option<ByteProgress> {
        self.indexing
    }

    /// Returns `true` when document-local indexing is still running.
    pub fn is_indexing(&self) -> bool {
        self.indexing.is_some()
    }

    /// Returns the current document backing mode.
    pub fn backing(&self) -> DocumentBacking {
        self.backing
    }

    /// Returns `true` when the document currently uses any edit buffer.
    pub fn has_edit_buffer(&self) -> bool {
        !matches!(self.backing, DocumentBacking::Mmap)
    }

    /// Returns `true` when the document is currently backed by a rope.
    pub fn has_rope(&self) -> bool {
        matches!(self.backing, DocumentBacking::Rope)
    }

    /// Returns `true` when the document is currently backed by a piece table.
    pub fn has_piece_table(&self) -> bool {
        matches!(self.backing, DocumentBacking::PieceTable)
    }
}

/// File-system, mapping, and edit-capability errors produced by [`crate::document::Document`].
#[derive(Debug)]
pub enum DocumentError {
    /// The source file could not be opened.
    Open { path: PathBuf, source: io::Error },
    /// The source file could not be memory-mapped.
    Map { path: PathBuf, source: io::Error },
    /// A write, rename, or reload step failed.
    Write { path: PathBuf, source: io::Error },
    /// The requested edit operation is unsupported for the current document state.
    EditUnsupported {
        path: Option<PathBuf>,
        reason: &'static str,
    },
}

impl std::fmt::Display for DocumentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Open { path, source } => write!(f, "open `{}`: {source}", path.display()),
            Self::Map { path, source } => write!(f, "mmap `{}`: {source}", path.display()),
            Self::Write { path, source } => write!(f, "write `{}`: {source}", path.display()),
            Self::EditUnsupported { path, reason } => {
                if let Some(path) = path {
                    write!(f, "edit `{}`: {reason}", path.display())
                } else {
                    write!(f, "edit: {reason}")
                }
            }
        }
    }
}

impl std::error::Error for DocumentError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Open { source, .. } | Self::Map { source, .. } | Self::Write { source, .. } => {
                Some(source)
            }
            Self::EditUnsupported { .. } => None,
        }
    }
}
