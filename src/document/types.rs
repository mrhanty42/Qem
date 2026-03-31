use super::{CompactionRecommendation, CompactionUrgency, FragmentationStats, LineEnding};
use encoding_rs::{Encoding, UTF_16BE, UTF_16LE, UTF_8};
use std::fmt;
use std::io;
use std::ops::Deref;
use std::path::{Path, PathBuf};

/// Named text encoding used for explicit open/save operations.
#[derive(Clone, Copy)]
pub struct DocumentEncoding(&'static Encoding);

impl DocumentEncoding {
    /// Returns the stable UTF-8 encoding used by Qem's default fast path.
    pub const fn utf8() -> Self {
        Self(UTF_8)
    }

    /// Returns UTF-16LE for BOM-backed reinterpret/open flows.
    pub const fn utf16le() -> Self {
        Self(UTF_16LE)
    }

    /// Returns UTF-16BE for BOM-backed reinterpret/open flows.
    pub const fn utf16be() -> Self {
        Self(UTF_16BE)
    }

    /// Looks up an encoding by label accepted by `encoding_rs`.
    pub fn from_label(label: &str) -> Option<Self> {
        Encoding::for_label(label.as_bytes()).map(Self)
    }

    /// Returns the canonical label for this encoding.
    pub fn name(self) -> &'static str {
        self.0.name()
    }

    /// Returns `true` when this is UTF-8.
    pub fn is_utf8(self) -> bool {
        self.0 == UTF_8
    }

    /// Returns `true` when `encoding_rs` can round-trip saves using this encoding.
    pub fn can_roundtrip_save(self) -> bool {
        self.0.output_encoding() == self.0
    }

    pub(crate) const fn as_encoding(self) -> &'static Encoding {
        self.0
    }

    pub(crate) const fn from_encoding_rs(encoding: &'static Encoding) -> Self {
        Self(encoding)
    }
}

impl Default for DocumentEncoding {
    fn default() -> Self {
        Self::utf8()
    }
}

impl PartialEq for DocumentEncoding {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self.0, other.0)
    }
}

impl Eq for DocumentEncoding {}

impl std::hash::Hash for DocumentEncoding {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.name().hash(state);
    }
}

impl fmt::Debug for DocumentEncoding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("DocumentEncoding")
            .field(&self.name())
            .finish()
    }
}

impl fmt::Display for DocumentEncoding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Describes how the current document encoding contract was chosen.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum DocumentEncodingOrigin {
    /// A new in-memory document starts with the default UTF-8 contract.
    #[default]
    NewDocument,
    /// The file was opened through Qem's default UTF-8 / ASCII fast path.
    Utf8FastPath,
    /// Lightweight auto-detection identified the current encoding from source bytes.
    AutoDetected,
    /// Auto-detection was requested but fell back to the UTF-8 fast path.
    AutoDetectFallbackUtf8,
    /// Auto-detection was requested and then fell back to an explicit caller override.
    AutoDetectFallbackOverride,
    /// The caller explicitly reinterpreted the source bytes through a chosen encoding.
    ExplicitReinterpretation,
    /// The current encoding contract came from an explicit save conversion.
    SaveConversion,
}

impl DocumentEncodingOrigin {
    /// Returns a stable lowercase identifier for logs, UI state, or JSON glue.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NewDocument => "new-document",
            Self::Utf8FastPath => "utf8-fast-path",
            Self::AutoDetected => "auto-detected",
            Self::AutoDetectFallbackUtf8 => "auto-detect-fallback-utf8",
            Self::AutoDetectFallbackOverride => "auto-detect-fallback-override",
            Self::ExplicitReinterpretation => "explicit-reinterpretation",
            Self::SaveConversion => "save-conversion",
        }
    }

    /// Returns `true` when auto-detection participated in the current contract.
    pub const fn used_auto_detection(self) -> bool {
        matches!(
            self,
            Self::AutoDetected | Self::AutoDetectFallbackUtf8 | Self::AutoDetectFallbackOverride
        )
    }

    /// Returns `true` when the contract came from an explicit caller choice.
    pub const fn is_explicit(self) -> bool {
        matches!(
            self,
            Self::AutoDetectFallbackOverride
                | Self::ExplicitReinterpretation
                | Self::SaveConversion
        )
    }
}

/// Open policy for choosing between the UTF-8 mmap fast path, initial
/// BOM-backed detection, or an explicit reinterpretation encoding.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum OpenEncodingPolicy {
    /// Keep the existing UTF-8/ASCII mmap fast path and its current semantics.
    #[default]
    Utf8FastPath,
    /// Detect BOM-backed encodings on open and otherwise fall back to the
    /// normal UTF-8/ASCII fast path.
    ///
    /// This first detection slice intentionally avoids heavyweight legacy
    /// charset guessing so open-time cost stays predictable.
    AutoDetect,
    /// Detect BOM-backed encodings first and otherwise reinterpret the source
    /// through the requested fallback encoding.
    ///
    /// This keeps the cheap BOM-backed detection path while still letting a
    /// caller say "if you do not detect anything stronger, use this explicit
    /// encoding instead of plain UTF-8 fast-path behavior".
    AutoDetectOrReinterpret(DocumentEncoding),
    /// Reinterpret the source bytes through the requested encoding.
    ///
    /// This is the option to use for legacy encodings such as
    /// `windows-1251`, `Shift_JIS`, or `GB18030` when the caller already knows
    /// the intended source encoding.
    Reinterpret(DocumentEncoding),
}

/// Explicit document-open options for choosing between the default UTF-8 path
/// and encoding-aware reinterpretation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct DocumentOpenOptions {
    encoding_policy: OpenEncodingPolicy,
}

impl DocumentOpenOptions {
    /// Creates open options that use Qem's default UTF-8 fast path.
    pub const fn new() -> Self {
        Self {
            encoding_policy: OpenEncodingPolicy::Utf8FastPath,
        }
    }

    /// Returns options that enable the initial BOM-backed auto-detect path.
    pub const fn with_auto_encoding_detection(mut self) -> Self {
        self.encoding_policy = OpenEncodingPolicy::AutoDetect;
        self
    }

    /// Returns options that try auto-detection first and otherwise reinterpret
    /// the source through `encoding`.
    pub const fn with_auto_encoding_detection_and_fallback(
        mut self,
        encoding: DocumentEncoding,
    ) -> Self {
        self.encoding_policy = OpenEncodingPolicy::AutoDetectOrReinterpret(encoding);
        self
    }

    /// Returns options that reinterpret the source through the given encoding.
    pub const fn with_reinterpretation(mut self, encoding: DocumentEncoding) -> Self {
        self.encoding_policy = OpenEncodingPolicy::Reinterpret(encoding);
        self
    }

    /// Returns options that force decoding the source through the given encoding.
    ///
    /// This is an alias for [`Self::with_reinterpretation`] kept for ergonomic
    /// compatibility with the first encoding-support release.
    pub const fn with_encoding(mut self, encoding: DocumentEncoding) -> Self {
        self.encoding_policy = OpenEncodingPolicy::Reinterpret(encoding);
        self
    }

    /// Returns the current open encoding policy.
    pub const fn encoding_policy(self) -> OpenEncodingPolicy {
        self.encoding_policy
    }

    /// Returns the explicit reinterpretation or fallback encoding, if one was requested.
    ///
    /// This compatibility helper returns `None` for the default fast path and
    /// for auto-detect mode.
    pub const fn encoding_override(self) -> Option<DocumentEncoding> {
        match self.encoding_policy {
            OpenEncodingPolicy::Reinterpret(encoding)
            | OpenEncodingPolicy::AutoDetectOrReinterpret(encoding) => Some(encoding),
            OpenEncodingPolicy::Utf8FastPath | OpenEncodingPolicy::AutoDetect => None,
        }
    }
}

/// Save policy for preserving the current document encoding or converting to a
/// different target encoding on write.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum SaveEncodingPolicy {
    /// Save using the document's current encoding contract.
    #[default]
    Preserve,
    /// Convert the current document text into the requested target encoding.
    Convert(DocumentEncoding),
}

/// Explicit document-save options.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct DocumentSaveOptions {
    encoding_policy: SaveEncodingPolicy,
}

impl DocumentSaveOptions {
    /// Creates save options that preserve the current document encoding.
    pub const fn new() -> Self {
        Self {
            encoding_policy: SaveEncodingPolicy::Preserve,
        }
    }

    /// Returns options that convert the current document text to `encoding` on save.
    pub const fn with_encoding(mut self, encoding: DocumentEncoding) -> Self {
        self.encoding_policy = SaveEncodingPolicy::Convert(encoding);
        self
    }

    /// Returns the encoding policy that will be used when saving.
    pub const fn encoding_policy(self) -> SaveEncodingPolicy {
        self.encoding_policy
    }
}

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
            1.0
        } else {
            self.completed_bytes.min(self.total_bytes) as f32 / self.total_bytes as f32
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
    exact_line_count_pending: bool,
    line_ending: LineEnding,
    encoding: DocumentEncoding,
    preserve_save_error: Option<DocumentEncodingErrorKind>,
    encoding_origin: DocumentEncodingOrigin,
    decoding_had_errors: bool,
    indexing: Option<ByteProgress>,
    backing: DocumentBacking,
}

impl DocumentStatus {
    /// Creates a document status snapshot.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        path: Option<PathBuf>,
        dirty: bool,
        file_len: usize,
        line_count: LineCount,
        exact_line_count_pending: bool,
        line_ending: LineEnding,
        encoding: DocumentEncoding,
        preserve_save_error: Option<DocumentEncodingErrorKind>,
        encoding_origin: DocumentEncodingOrigin,
        decoding_had_errors: bool,
        indexing: Option<ByteProgress>,
        backing: DocumentBacking,
    ) -> Self {
        Self {
            path,
            dirty,
            file_len,
            line_count,
            exact_line_count_pending,
            line_ending,
            encoding,
            preserve_save_error,
            encoding_origin,
            decoding_had_errors,
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

    /// Returns `true` when background work may still upgrade the total line
    /// count from an estimate to an exact value.
    pub fn is_line_count_pending(&self) -> bool {
        self.exact_line_count_pending
    }

    /// Returns the currently detected line ending style.
    pub fn line_ending(&self) -> LineEnding {
        self.line_ending
    }

    /// Returns the current document encoding contract.
    pub fn encoding(&self) -> DocumentEncoding {
        self.encoding
    }

    /// Returns the typed reason why preserve-save would currently fail, if any.
    pub fn preserve_save_error(&self) -> Option<DocumentEncodingErrorKind> {
        self.preserve_save_error
    }

    /// Returns `true` when preserve-save is currently allowed for this document snapshot.
    pub fn can_preserve_save(&self) -> bool {
        self.preserve_save_error().is_none()
    }

    /// Returns how the current encoding contract was chosen.
    pub fn encoding_origin(&self) -> DocumentEncodingOrigin {
        self.encoding_origin
    }

    /// Returns `true` when opening the source required lossy decode replacement.
    pub fn decoding_had_errors(&self) -> bool {
        self.decoding_had_errors
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

/// Snapshot of maintenance-oriented document state such as fragmentation and
/// compaction advice.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DocumentMaintenanceStatus {
    backing: DocumentBacking,
    fragmentation: Option<FragmentationStats>,
    compaction: Option<CompactionRecommendation>,
}

/// High-level maintenance action suggested by the current compaction policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaintenanceAction {
    /// No maintenance work is currently recommended.
    None,
    /// The frontend can run idle compaction now.
    IdleCompaction,
    /// Heavier maintenance should wait for an explicit operator/save boundary.
    ExplicitCompaction,
}

impl MaintenanceAction {
    /// Returns a stable lowercase identifier for logs, JSON output, or UI glue.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::IdleCompaction => "idle-compaction",
            Self::ExplicitCompaction => "explicit-compaction",
        }
    }
}

impl DocumentMaintenanceStatus {
    /// Creates a maintenance status snapshot.
    pub const fn new(
        backing: DocumentBacking,
        fragmentation: Option<FragmentationStats>,
        compaction: Option<CompactionRecommendation>,
    ) -> Self {
        Self {
            backing,
            fragmentation,
            compaction,
        }
    }

    /// Returns the current document backing mode.
    pub const fn backing(self) -> DocumentBacking {
        self.backing
    }

    /// Returns `true` when the document currently uses a piece table.
    pub const fn has_piece_table(self) -> bool {
        matches!(self.backing, DocumentBacking::PieceTable)
    }

    /// Returns fragmentation metrics when they are meaningful for the current backing.
    pub const fn fragmentation_stats(self) -> Option<FragmentationStats> {
        self.fragmentation
    }

    /// Returns `true` when fragmentation metrics are available.
    pub const fn has_fragmentation_stats(self) -> bool {
        self.fragmentation.is_some()
    }

    /// Returns the current compaction recommendation, if any.
    pub const fn compaction_recommendation(self) -> Option<CompactionRecommendation> {
        self.compaction
    }

    /// Returns `true` when the current maintenance policy recommends compaction.
    pub const fn is_compaction_recommended(self) -> bool {
        self.compaction.is_some()
    }

    /// Returns the current compaction urgency, if a recommendation exists.
    pub fn compaction_urgency(self) -> Option<CompactionUrgency> {
        self.compaction
            .map(|recommendation| recommendation.urgency())
    }

    /// Returns the high-level maintenance action implied by this snapshot.
    pub fn recommended_action(self) -> MaintenanceAction {
        match self.compaction_urgency() {
            Some(CompactionUrgency::Deferred) => MaintenanceAction::IdleCompaction,
            Some(CompactionUrgency::Forced) => MaintenanceAction::ExplicitCompaction,
            None => MaintenanceAction::None,
        }
    }

    /// Returns `true` when idle compaction is currently recommended.
    pub fn should_run_idle_compaction(self) -> bool {
        self.recommended_action() == MaintenanceAction::IdleCompaction
    }

    /// Returns `true` when heavier maintenance should be deferred to an
    /// explicit operator/save boundary.
    pub fn should_wait_for_explicit_compaction(self) -> bool {
        self.recommended_action() == MaintenanceAction::ExplicitCompaction
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DocumentEncodingErrorKind {
    /// Opening this encoding would require a full transcode beyond the current safety limit.
    OpenTranscodeTooLarge { max_bytes: usize },
    /// Saving to this encoding would succeed, but reopening the saved document
    /// would require a full transcode beyond the current safety limit.
    SaveReopenTooLarge { max_bytes: usize },
    /// Preserving the current decoded encoding contract is not supported on save yet.
    PreserveSaveUnsupported,
    /// Preserving the current decoded encoding would cement a lossy open.
    LossyDecodedPreserve,
    /// The requested save target is not yet supported as a direct output encoding.
    UnsupportedSaveTarget,
    /// `encoding_rs` redirected the save target to a different output encoding.
    RedirectedSaveTarget { actual: DocumentEncoding },
    /// The current document text cannot be represented in the requested encoding.
    UnrepresentableText,
}

impl std::fmt::Display for DocumentEncodingErrorKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OpenTranscodeTooLarge { max_bytes } => write!(
                f,
                "non-UTF8 open currently requires full transcoding and is limited to {max_bytes} bytes"
            ),
            Self::SaveReopenTooLarge { max_bytes } => write!(
                f,
                "saving to this non-UTF8 target would require reopening a full transcoded buffer and is limited to {max_bytes} bytes"
            ),
            Self::PreserveSaveUnsupported => f.write_str(
                "preserve-save is not yet supported for this encoding; use DocumentSaveOptions::with_encoding(...) to convert to a supported target",
            ),
            Self::LossyDecodedPreserve => f.write_str(
                "preserve-save is rejected because opening this document already required lossy decoding; convert explicitly if you want to keep the repaired text",
            ),
            Self::UnsupportedSaveTarget => {
                f.write_str("this encoding is not yet supported as a save target")
            }
            Self::RedirectedSaveTarget { actual } => write!(
                f,
                "encoding_rs redirected this save target to `{actual}`"
            ),
            Self::UnrepresentableText => f.write_str(
                "the current document contains characters that are not representable in the target encoding",
            ),
        }
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
    /// Encoding negotiation, decode, or save conversion failed.
    Encoding {
        path: PathBuf,
        operation: &'static str,
        encoding: DocumentEncoding,
        reason: DocumentEncodingErrorKind,
    },
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
            Self::Encoding {
                path,
                operation,
                encoding,
                reason,
            } => write!(
                f,
                "{operation} `{}` with encoding `{encoding}`: {reason}",
                path.display()
            ),
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
            Self::Encoding { .. } | Self::EditUnsupported { .. } => None,
        }
    }
}
