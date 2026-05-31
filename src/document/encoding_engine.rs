//! Encoding-aware byte movement engine.
//!
//! Instead of assuming UTF-8 throughout the document layer, every
//! byte-level helper (line navigation, column scanning, character
//! stepping) is expressed as a method on `EncodingEngine`. The default
//! implementation is `Utf8Engine`, which preserves the original UTF-8
//! behavior. Other implementations:
//!
//! - `SingleByteEngine` for ASCII-superset single-byte encodings
//!   (`windows-1251`, `latin1`, `koi8-r`, `cp866`, ...). These run natively
//!   over the same mmap path UTF-8 uses.
//! - `Utf16Engine<Endian>` for UTF-16 LE/BE.
//! - `MultiByteEngine` for variable-length CJK encodings such as
//!   Shift_JIS, GB18030 and EUC-KR.

#![allow(dead_code)]

use super::DocumentEncoding;

/// Byte-level operations the document layer needs from a text encoding.
///
/// All offsets are byte offsets into the raw stored bytes (mmap or piece
/// table buffer). Implementations are expected to be cheap and allocation
/// free for the hot helpers (`step`, `next_line_start`, `count_text_columns`),
/// so they can be called millions of times per search / viewport pass.
///
/// This trait is not part of the stable public API surface; stability is
/// reserved for `1.0.0`. It is exposed only through the hidden
/// `qem::__test_support` module so the integration property tests under
/// `tests/encoding_engine/` can drive engines directly.
#[doc(hidden)]
pub trait EncodingEngine: Send + Sync + std::fmt::Debug {
    /// Returns the encoding this engine is built for.
    fn encoding(&self) -> DocumentEncoding;

    /// Returns the number of bytes occupied by one character starting at
    /// `offset` within `bytes`.
    ///
    /// `end` is the upper bound of the byte slice the caller considers
    /// valid; implementations must not read past it. The returned step is
    /// always at least `1` so iteration cannot stall.
    fn step(&self, bytes: &[u8], offset: usize, end: usize) -> usize;

    /// Returns the number of bytes occupied by the character that ends at
    /// `offset` within `bytes`, walking *backward* toward `start`.
    ///
    /// Pre:  `start <= offset <= bytes.len()`.
    /// Post: returns `0` when `offset == start`; otherwise `1..=4`, with
    ///       `offset - n >= start`. The symmetry contract is
    ///       `step_forward(p - step_backward(p)) == step_backward(p)` for
    ///       any `p` reachable by forward stepping from `start`.
    ///
    /// The trait provides a conservative single-byte default
    /// (`1` whenever `offset > start`, otherwise `0`) so engines that only
    /// implement forward stepping for now keep compiling. Real backward
    /// stepping is overridden per-engine: `Utf8Engine` walks back to a
    /// UTF-8 char boundary, `SingleByteEngine` always answers `1`,
    /// `Utf16Engine<E>` is surrogate-aware, and `MultiByteEngine` scans
    /// forward from a known anchor.
    fn step_backward(&self, _bytes: &[u8], offset: usize, start: usize) -> usize {
        if offset > start {
            1
        } else {
            0
        }
    }

    /// Returns the byte offset where the next line starts after
    /// `line_start` within `bytes[..file_len]`. If no further line break
    /// exists within the slice, returns `file_len`.
    ///
    /// CRLF is treated as one line ending; the returned offset is past the
    /// `\n` byte.
    fn next_line_start(&self, bytes: &[u8], file_len: usize, line_start: usize) -> usize;

    /// Counts the number of text-unit columns in `bytes`, stopping at the
    /// first line-ending byte. Used for measuring exact line widths.
    fn count_columns_exact(&self, bytes: &[u8]) -> usize;

    /// Same as [`count_columns_exact`] but bounded to `max_cols` columns.
    fn count_columns_bounded(&self, bytes: &[u8], max_cols: usize) -> usize;

    /// Advances `start` by `text_units` characters within `bytes[..file_len]`,
    /// returning the resulting byte offset. CRLF counts as one text unit.
    fn advance_offset_by_text_units(
        &self,
        bytes: &[u8],
        file_len: usize,
        start: usize,
        text_units: usize,
    ) -> usize;

    /// Trims a trailing line-ending sequence ending at `end` from `bytes`,
    /// returning the new end offset.
    ///
    /// The contract is encoding-aware: implementations remove a single
    /// trailing line-ending cell (LF, CR, or CRLF) when the bytes preceding
    /// `end` form one. Single-byte engines (`Utf8Engine`, `SingleByteEngine`)
    /// trim 1-byte `\n` / `\r` cells; `Utf16Engine<E>` trims 2-byte cells in
    /// the engine's endianness so the trimmed prefix stays on a code-unit
    /// boundary and the decoder is never handed an odd-length window.
    ///
    /// `start <= end <= bytes.len()` must hold; the returned offset is
    /// always in `[start, end]`.
    fn trim_trailing_line_break(&self, bytes: &[u8], start: usize, end: usize) -> usize {
        let mut end = end.min(bytes.len()).max(start);
        if end > start && bytes[end - 1] == b'\n' {
            end -= 1;
        }
        if end > start && bytes[end - 1] == b'\r' {
            end -= 1;
        }
        end
    }
}

// ---------------------------------------------------------------------------
// UTF-8 engine
// ---------------------------------------------------------------------------

/// UTF-8 encoding engine. Delegates to the existing free helpers so the
/// engine surface is a thin layer over the original UTF-8 byte movement.
#[doc(hidden)]
#[derive(Debug, Clone, Copy)]
pub struct Utf8Engine;

impl EncodingEngine for Utf8Engine {
    fn encoding(&self) -> DocumentEncoding {
        DocumentEncoding::utf8()
    }

    fn step(&self, bytes: &[u8], offset: usize, end: usize) -> usize {
        super::utf8_step(bytes, offset, end)
    }

    fn step_backward(&self, bytes: &[u8], offset: usize, start: usize) -> usize {
        // Forward/backward symmetry. UTF-8 char boundaries are exactly
        // the positions where `is_utf8_continuation` is false, so we
        // walk back over continuation bytes from `offset - 1` until we
        // hit a leading byte (or `start`).
        let len = bytes.len();
        let offset = offset.min(len);
        if offset <= start {
            return 0;
        }
        // CRLF collapses to a single text unit (matches the forward
        // walker's CRLF semantics in `advance_offset_by_text_units`). Only
        // returns 2 when both bytes are still inside `[start, offset)`.
        if offset >= start + 2 && bytes[offset - 1] == b'\n' && bytes[offset - 2] == b'\r' {
            return 2;
        }
        let mut p = offset - 1;
        while p > start && super::is_utf8_continuation(bytes[p]) {
            p -= 1;
        }
        offset - p
    }

    fn next_line_start(&self, bytes: &[u8], file_len: usize, line_start: usize) -> usize {
        super::search::next_line_start_exact(bytes, file_len, line_start)
    }

    fn count_columns_exact(&self, bytes: &[u8]) -> usize {
        super::count_text_columns_exact(bytes)
    }

    fn count_columns_bounded(&self, bytes: &[u8], max_cols: usize) -> usize {
        super::count_text_columns(bytes, max_cols)
    }

    fn advance_offset_by_text_units(
        &self,
        bytes: &[u8],
        file_len: usize,
        start: usize,
        text_units: usize,
    ) -> usize {
        super::advance_offset_by_text_units_in_bytes(bytes, file_len, start, text_units)
    }
}

/// The single shared UTF-8 engine instance.
#[doc(hidden)]
pub const UTF8_ENGINE: &Utf8Engine = &Utf8Engine;

// ---------------------------------------------------------------------------
// Single-byte engine
// ---------------------------------------------------------------------------

/// Byte-level engine for ASCII-superset single-byte encodings such as
/// `windows-1251`, `windows-1252`, `latin1` (ISO-8859-1), `koi8-r`,
/// `cp866`, etc.
///
/// Every code point is exactly one byte, so character navigation reduces to
/// byte navigation. The bytes `0x0A` and `0x0D` always represent line feed
/// and carriage return because no single-byte legacy encoding remaps them.
/// CRLF therefore collapses the same way as in UTF-8.
///
/// This engine is intentionally encoding-parameterized so a single
/// implementation covers every Class A encoding without duplicating logic.
#[doc(hidden)]
#[derive(Debug, Clone, Copy)]
pub struct SingleByteEngine {
    encoding: DocumentEncoding,
}

impl SingleByteEngine {
    /// Constructs a single-byte engine for the given encoding.
    ///
    /// The caller is expected to verify that the encoding is in fact a
    /// single-byte ASCII superset; the engine itself does not re-validate
    /// that contract on the hot path.
    pub(crate) const fn new(encoding: DocumentEncoding) -> Self {
        Self { encoding }
    }

    /// Returns `true` when `encoding` is a single-byte ASCII superset that
    /// `SingleByteEngine` can drive natively.
    ///
    /// This filters by the well-known Class A set used by phases 4-5. Other
    /// encodings (UTF-16, multibyte CJK) need their own engine and are
    /// handled in later phases.
    pub fn supports(encoding: DocumentEncoding) -> bool {
        matches!(
            encoding.name(),
            "windows-1250"
                | "windows-1251"
                | "windows-1252"
                | "windows-1253"
                | "windows-1254"
                | "windows-1255"
                | "windows-1256"
                | "windows-1257"
                | "windows-1258"
                | "windows-874"
                | "ISO-8859-2"
                | "ISO-8859-3"
                | "ISO-8859-4"
                | "ISO-8859-5"
                | "ISO-8859-6"
                | "ISO-8859-7"
                | "ISO-8859-8"
                | "ISO-8859-8-I"
                | "ISO-8859-10"
                | "ISO-8859-13"
                | "ISO-8859-14"
                | "ISO-8859-15"
                | "ISO-8859-16"
                | "KOI8-R"
                | "KOI8-U"
                | "IBM866"
                | "macintosh"
                | "x-mac-cyrillic"
        )
    }
}

impl EncodingEngine for SingleByteEngine {
    fn encoding(&self) -> DocumentEncoding {
        self.encoding
    }

    #[inline]
    fn step(&self, _bytes: &[u8], offset: usize, end: usize) -> usize {
        if offset >= end {
            0
        } else {
            1
        }
    }

    #[inline]
    fn step_backward(&self, bytes: &[u8], offset: usize, start: usize) -> usize {
        // Every code point is exactly one byte in any single-byte
        // ASCII superset, so the backward step is symmetrically `1` —
        // except for CRLF, which collapses to a single text unit on the
        // forward walker (`advance_offset_by_text_units`) and therefore
        // must collapse here too.
        if offset <= start {
            return 0;
        }
        let len = bytes.len();
        let offset = offset.min(len);
        if offset <= start {
            return 0;
        }
        if offset >= start + 2 && bytes[offset - 1] == b'\n' && bytes[offset - 2] == b'\r' {
            return 2;
        }
        1
    }

    fn next_line_start(&self, bytes: &[u8], file_len: usize, line_start: usize) -> usize {
        // CR / LF / CRLF detection is byte-identical to UTF-8 for every
        // ASCII-superset single-byte encoding because none of them remap
        // 0x0A or 0x0D.
        super::search::next_line_start_exact(bytes, file_len, line_start)
    }

    fn count_columns_exact(&self, bytes: &[u8]) -> usize {
        // One byte per column, stop at the first line-ending byte.
        match memchr::memchr2(b'\n', b'\r', bytes) {
            Some(idx) => idx,
            None => bytes.len(),
        }
    }

    fn count_columns_bounded(&self, bytes: &[u8], max_cols: usize) -> usize {
        let scan_end = max_cols.min(bytes.len());
        match memchr::memchr2(b'\n', b'\r', &bytes[..scan_end]) {
            Some(idx) => idx,
            None => scan_end,
        }
    }

    fn advance_offset_by_text_units(
        &self,
        bytes: &[u8],
        file_len: usize,
        start: usize,
        text_units: usize,
    ) -> usize {
        let start = start.min(file_len);
        if text_units == 0 || start >= file_len {
            return start;
        }

        let mut remaining = text_units;
        let mut offset = start;
        let mut pending_cr = false;
        while offset < file_len && (remaining > 0 || pending_cr) {
            if pending_cr {
                pending_cr = false;
                if bytes[offset] == b'\n' {
                    offset += 1;
                    continue;
                }
            }
            if remaining == 0 {
                break;
            }

            match bytes[offset] {
                b'\r' => {
                    remaining -= 1;
                    offset += 1;
                    pending_cr = true;
                }
                _ => {
                    // Both `\n` and any high-byte single-byte glyph are
                    // exactly one byte and one text unit.
                    remaining -= 1;
                    offset += 1;
                }
            }
        }
        offset.min(file_len)
    }
}

/// Returns the static engine instance that drives byte-level operations for
/// the given encoding.
///
/// Single-byte ASCII supersets (windows-1251, latin1, KOI8-R, ...) route
/// through a shared static `SingleByteEngine` instance keyed by encoding
/// name. The surrogate-aware `Utf16Engine<E>` handles `UTF-16LE` /
/// `UTF-16BE`, also cached as static singletons. `MultiByteEngine` covers
/// `Shift_JIS`, `gb18030` and `EUC-KR`, each cached independently. Other
/// encodings still fall back to the UTF-8 engine
/// here.
#[doc(hidden)]
pub fn engine_for_encoding(encoding: DocumentEncoding) -> &'static dyn EncodingEngine {
    if SingleByteEngine::supports(encoding) {
        return single_byte_engine_for(encoding);
    }
    if let Some(engine) = utf16::utf16_engine_for(encoding) {
        return engine;
    }
    if let Some(engine) = multibyte::multibyte_engine_for(encoding) {
        return engine;
    }
    UTF8_ENGINE
}

fn single_byte_engine_for(encoding: DocumentEncoding) -> &'static dyn EncodingEngine {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};

    static CACHE: OnceLock<Mutex<HashMap<&'static str, &'static SingleByteEngine>>> =
        OnceLock::new();
    let map = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = map.lock().expect("encoding-engine cache poisoned");
    let name: &'static str = encoding.name();
    *guard
        .entry(name)
        .or_insert_with(|| Box::leak(Box::new(SingleByteEngine::new(encoding))))
}

// ---------------------------------------------------------------------------
// UTF-16 engine
// ---------------------------------------------------------------------------

/// UTF-16 engine plumbing: the `Endian` marker trait, `LittleEndian` and
/// `BigEndian` markers, and the `Utf16Engine<E>` struct itself.
///
/// The module ships the dispatch wiring through `engine_for_encoding`
/// plus surrogate-aware forward and backward stepping, the
/// 2-byte-aligned `next_line_start` scan, and the surrogate-aware
/// `count_columns_exact`, `count_columns_bounded`, and
/// `advance_offset_by_text_units` walkers.
pub(crate) mod utf16 {
    use super::{DocumentEncoding, EncodingEngine};
    use std::marker::PhantomData;
    use std::sync::OnceLock;

    /// Endianness marker for `Utf16Engine`.
    ///
    /// Carries the byte patterns of LF / CR for the chosen endianness and a
    /// `read_u16` helper so the engine implementation can stay
    /// endian-agnostic.
    pub(crate) trait Endian: Send + Sync + std::fmt::Debug + Copy + 'static {
        /// Canonical encoding label as known by `encoding_rs`
        /// (`"UTF-16LE"` or `"UTF-16BE"`).
        const NAME: &'static str;
        /// Byte sequence for the `\n` (`U+000A`) code unit on this
        /// endianness (`[0x0A, 0x00]` for LE, `[0x00, 0x0A]` for BE).
        const LF: [u8; 2];
        /// Byte sequence for the `\r` (`U+000D`) code unit on this
        /// endianness.
        const CR: [u8; 2];
        /// Decodes a 2-byte aligned UTF-16 code unit at the given byte
        /// pair into its `u16` value.
        fn read_u16(bytes: &[u8; 2]) -> u16;
    }

    /// Little-endian UTF-16 marker.
    #[derive(Debug, Clone, Copy)]
    pub(crate) struct LittleEndian;

    /// Big-endian UTF-16 marker.
    #[derive(Debug, Clone, Copy)]
    pub(crate) struct BigEndian;

    impl Endian for LittleEndian {
        const NAME: &'static str = "UTF-16LE";
        const LF: [u8; 2] = [0x0A, 0x00];
        const CR: [u8; 2] = [0x0D, 0x00];
        #[inline]
        fn read_u16(bytes: &[u8; 2]) -> u16 {
            u16::from_le_bytes(*bytes)
        }
    }

    impl Endian for BigEndian {
        const NAME: &'static str = "UTF-16BE";
        const LF: [u8; 2] = [0x00, 0x0A];
        const CR: [u8; 2] = [0x00, 0x0D];
        #[inline]
        fn read_u16(bytes: &[u8; 2]) -> u16 {
            u16::from_be_bytes(*bytes)
        }
    }

    /// Byte-level engine for UTF-16, parameterised by endianness.
    ///
    /// The `PhantomData<E>` carries the endianness purely at the type
    /// level; `Utf16Engine<LittleEndian>` and `Utf16Engine<BigEndian>` are
    /// distinct zero-sized types so the dispatcher can hand out a
    /// `&'static dyn EncodingEngine` for each one independently and each
    /// instance can compile-time bake in its own LF/CR byte pattern
    /// without runtime branching.
    #[derive(Debug, Clone, Copy)]
    pub(crate) struct Utf16Engine<E: Endian>(PhantomData<E>);

    impl<E: Endian> Utf16Engine<E> {
        /// Constructs the (zero-sized) engine for endianness `E`.
        ///
        /// Marked `const` so the `OnceLock` initialisers in
        /// `utf16_engine_for` can avoid synchronisation overhead beyond
        /// the lock itself.
        pub(crate) const fn new() -> Self {
            Self(PhantomData)
        }
    }

    impl<E: Endian> EncodingEngine for Utf16Engine<E> {
        fn encoding(&self) -> DocumentEncoding {
            DocumentEncoding::from_label(E::NAME)
                .unwrap_or_else(|| panic!("encoding_rs should know {}", E::NAME))
        }

        /// Surrogate-aware forward step.
        ///
        /// Returns the byte size of the character at `offset` within
        /// `bytes[..end]`:
        ///
        /// - BMP code unit (not in surrogate range) → `2`.
        /// - High surrogate (`0xD800..=0xDBFF`) followed by a low
        ///   surrogate (`0xDC00..=0xDFFF`) at `offset + 2` → `4`
        ///   (supplementary character).
        /// - Lone high surrogate, or insufficient bytes for a low
        ///   surrogate → `2` (treat the unpaired unit as malformed and
        ///   advance by one code unit so iteration cannot stall).
        /// - Insufficient bytes for a single code unit (`remaining < 2`)
        ///   → `0`.
        ///
        /// Endian-aware: `E::read_u16` decodes the 2-byte cell into its
        /// `u16` value, so LE/BE share one implementation and only
        /// differ in the byte order they consume.
        fn step(&self, bytes: &[u8], offset: usize, end: usize) -> usize {
            let end = end.min(bytes.len());
            if offset >= end {
                return 0;
            }
            let remaining = end - offset;
            if remaining < 2 {
                return 0;
            }
            // Read the first 2-byte code unit at `offset`.
            let unit0 = E::read_u16(&[bytes[offset], bytes[offset + 1]]);
            // High surrogate + low surrogate is one supplementary
            // character (4 bytes total).
            if (0xD800..=0xDBFF).contains(&unit0) && remaining >= 4 {
                let unit1 = E::read_u16(&[bytes[offset + 2], bytes[offset + 3]]);
                if (0xDC00..=0xDFFF).contains(&unit1) {
                    return 4;
                }
            }
            // BMP code unit, lone high surrogate, or low surrogate at the
            // start of the slice — advance by one code unit.
            2
        }

        /// Surrogate-aware backward step.
        ///
        /// Symmetric to [`Self::step`]: returns `2` for a single BMP code
        /// unit ending at `offset`, and `4` if the previous 2 bytes form
        /// a low surrogate AND the 2 bytes before that form a high
        /// surrogate. Returns `0` when there are not enough bytes left
        /// between `start` and `offset` for a code unit.
        ///
        /// The round-trip property
        /// `step_forward(p - step_backward(p)) == step_backward(p)` holds
        /// for every offset reachable by forward stepping from `start`.
        fn step_backward(&self, bytes: &[u8], offset: usize, start: usize) -> usize {
            let len = bytes.len();
            let offset = offset.min(len);
            if offset.saturating_sub(start) < 2 {
                return 0;
            }
            // Try to read the trailing 2-byte code unit. If it is a
            // low surrogate AND the cell before it is a high surrogate
            // (and both pairs fit inside `[start, offset)`), the
            // character is supplementary (4 bytes).
            let p_last = offset - 2;
            if p_last + 1 < len {
                let unit_last = E::read_u16(&[bytes[p_last], bytes[p_last + 1]]);
                if (0xDC00..=0xDFFF).contains(&unit_last) && offset.saturating_sub(start) >= 4 {
                    let p_prev = offset - 4;
                    let unit_prev = E::read_u16(&[bytes[p_prev], bytes[p_prev + 1]]);
                    if (0xD800..=0xDBFF).contains(&unit_prev) {
                        return 4;
                    }
                }
            }
            // Lone or BMP unit — single code unit.
            2
        }

        /// 2-byte aligned newline scan.
        ///
        /// Walks the slice in 2-byte aligned cells from `line_start`,
        /// looking for `[E::LF]` (or `[E::CR]` optionally followed by
        /// `[E::LF]` for CRLF — both forms collapse to a single line
        /// boundary). The starting offset is rounded up to the
        /// next even boundary so each read inspects exactly one code
        /// unit cell. Misaligned `0x0A` / `0x0D` bytes —
        /// i.e. the trailing byte of a UTF-16 code unit — are
        /// automatically rejected because the loop only inspects pairs
        /// at even offsets, never reading a byte at an odd index as the
        /// low half of a candidate line-ending cell.
        fn next_line_start(&self, bytes: &[u8], file_len: usize, line_start: usize) -> usize {
            let file_len = file_len.min(bytes.len());
            // Round up to the next 2-byte aligned offset.
            let mut p = if line_start & 1 == 1 {
                line_start.saturating_add(1)
            } else {
                line_start
            };
            while p + 1 < file_len {
                let unit = [bytes[p], bytes[p + 1]];
                if unit == E::LF {
                    return (p + 2).min(file_len);
                }
                if unit == E::CR {
                    let q = p + 2;
                    if q + 1 < file_len {
                        let next = [bytes[q], bytes[q + 1]];
                        if next == E::LF {
                            return (q + 2).min(file_len);
                        }
                    }
                    return (p + 2).min(file_len);
                }
                p += 2;
            }
            file_len
        }

        /// Surrogate-aware exact column count.
        ///
        /// Walks `bytes` via [`Self::step`] from offset `0`, peeking at
        /// the next 2-byte aligned cell before each step. Stops when the
        /// peeked cell is `[E::LF]` or `[E::CR]` (line-ending cell, not
        /// counted), or when the slice ends. Each step (`2` for a BMP
        /// code unit or `4` for a supplementary surrogate pair) counts
        /// as exactly one column, so a supplementary character is one
        /// column rather than two. Misaligned `0x0A` / `0x0D` bytes
        /// inside the trailing half of a code unit are never inspected
        /// because peek+step iterate strictly on even-aligned cells.
        fn count_columns_exact(&self, bytes: &[u8]) -> usize {
            self.count_columns_bounded(bytes, usize::MAX)
        }

        /// Surrogate-aware bounded column count.
        ///
        /// Same walker as [`Self::count_columns_exact`], additionally
        /// stopping after `max_cols` columns have been counted.
        fn count_columns_bounded(&self, bytes: &[u8], max_cols: usize) -> usize {
            let len = bytes.len();
            let mut p = 0usize;
            let mut cols = 0usize;
            while cols < max_cols && p + 1 < len {
                // Peek at the 2-byte cell at `p` to detect a line-ending
                // before consuming a step (line-ending cells are
                // not counted as columns).
                let unit = [bytes[p], bytes[p + 1]];
                if unit == E::LF || unit == E::CR {
                    return cols;
                }
                let step = self.step(bytes, p, len);
                if step == 0 {
                    // Insufficient bytes for another code unit; the
                    // trailing odd byte (if any) cannot be a line-ending
                    // candidate at an aligned position.
                    break;
                }
                p += step;
                cols += 1;
            }
            cols
        }

        /// Surrogate-aware advance by text units with CRLF collapse.
        ///
        /// Aligns `start` up to a 2-byte boundary first (so subsequent
        /// cell reads stay inside a code-unit cell), then walks
        /// `text_units` units forward via [`Self::step`]. A supplementary
        /// surrogate pair counts as a single text unit (one 4-byte
        /// step). When the consumed cell at the previous position is
        /// `[E::CR]` and the next aligned cell is `[E::LF]`, the LF is
        /// consumed as part of the same text unit (CRLF collapse).
        fn advance_offset_by_text_units(
            &self,
            bytes: &[u8],
            file_len: usize,
            start: usize,
            text_units: usize,
        ) -> usize {
            let file_len = file_len.min(bytes.len());
            // Round up to the next 2-byte aligned offset so
            // each cell read inspects a full UTF-16 code unit.
            let mut p = if start & 1 == 1 {
                start.saturating_add(1)
            } else {
                start
            };
            if p >= file_len || text_units == 0 {
                return p.min(file_len);
            }
            let mut remaining = text_units;
            while remaining > 0 && p + 1 < file_len {
                // Snapshot the leading cell so we can recognise CR and
                // collapse a following LF into the same text unit.
                let unit = [bytes[p], bytes[p + 1]];
                let step = self.step(bytes, p, file_len);
                if step == 0 {
                    break;
                }
                p += step;
                remaining -= 1;
                // CRLF collapses to one text unit. The previous cell
                // was CR, so if the next aligned cell is LF, swallow it
                // without consuming a new text unit.
                if unit == E::CR && p + 1 < file_len {
                    let next = [bytes[p], bytes[p + 1]];
                    if next == E::LF {
                        p += 2;
                    }
                }
            }
            p.min(file_len)
        }

        /// Endian-aware line-ending trim.
        ///
        /// Trims a trailing UTF-16 line-ending cell — `[E::LF]` or
        /// `[E::CR]` (with optional preceding CRLF when both halves
        /// are present) — from the byte window ending at `end`. The
        /// returned offset is always 2-byte aligned within the trimmed
        /// region so the downstream `encoding_rs` decoder is never
        /// handed an odd-length window that would split a code unit
        /// in half.
        ///
        /// Misaligned `0x0A` / `0x0D` bytes (the trailing byte of a
        /// non-line-ending UTF-16 code unit) are not interpreted as
        /// line endings: only complete 2-byte aligned cells inside
        /// `[start, end)` are inspected.
        fn trim_trailing_line_break(&self, bytes: &[u8], start: usize, end: usize) -> usize {
            let len = bytes.len();
            let mut end = end.min(len).max(start);
            // The line-ending cell must be wholly inside `[start, end)`
            // and aligned to the same 2-byte grid as the rest of the
            // window. The slice produced by `next_line_start` always
            // ends on an even offset, so this check just
            // pins down that contract here.
            if end.saturating_sub(start) < 2 || (end - start) & 1 == 1 {
                return end;
            }
            let cell = [bytes[end - 2], bytes[end - 1]];
            if cell == E::LF {
                end -= 2;
                if end.saturating_sub(start) >= 2 {
                    let prev = [bytes[end - 2], bytes[end - 1]];
                    if prev == E::CR {
                        end -= 2;
                    }
                }
            } else if cell == E::CR {
                end -= 2;
            }
            end
        }
    }

    /// Returns the cached `&'static dyn EncodingEngine` for `UTF-16LE`
    /// / `UTF-16BE`, or `None` for any other encoding.
    ///
    /// Each endianness gets its own `OnceLock<Utf16Engine<...>>` static
    /// so callers always observe the same trait object across calls
    /// (Property 5: pointer-stable dispatch).
    pub(super) fn utf16_engine_for(
        encoding: DocumentEncoding,
    ) -> Option<&'static dyn EncodingEngine> {
        static LE: OnceLock<Utf16Engine<LittleEndian>> = OnceLock::new();
        static BE: OnceLock<Utf16Engine<BigEndian>> = OnceLock::new();
        match encoding.name() {
            "UTF-16LE" => Some(LE.get_or_init(Utf16Engine::<LittleEndian>::new)),
            "UTF-16BE" => Some(BE.get_or_init(Utf16Engine::<BigEndian>::new)),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// MultiByteEngine
// ---------------------------------------------------------------------------

/// MultiByteEngine plumbing for variable-length CJK encodings:
/// `Shift_JIS`, `gb18030` and `EUC-KR`.
///
/// The engine type, the leading-byte detector `char_len` and the
/// trait implementation:
///
/// - `step` walks one character forward via `char_len`. When a
///   multi-byte sequence is truncated by the slice end, `step` falls
///   back to advancing one byte so iteration cannot stall (the byte
///   is treated as malformed — encoding_rs would emit U+FFFD).
/// - `step_backward` finds a known-aligned anchor — the
///   start of a line within a 64 KiB backscan window — and walks
///   forward via `char_len` until the cursor reaches `offset`,
///   returning the last forward step. When the heuristic
///   anchor is not on a character boundary the function returns `1`
///   as a deg-fallback so callers resync on the next
///   `next_line_start`.
/// - `next_line_start` is the false-positive-aware
///   character walk: it advances character-by-character from
///   `line_start` via `char_len` and inspects `\n` / `\r` *only* when
///   the current character occupies one byte (i.e. ASCII). Trailing
///   bytes of 2- or 4-byte CJK sequences are consumed as part of the
///   leading step, so a stray `0x0A` or `0x0D` inside a multi-byte
///   character can never be mistaken for a line break. CRLF
///   collapses to one boundary only when both `\r` and
///   `\n` are observed as 1-byte characters in sequence.
/// - Column counters and `advance_offset_by_text_units` likewise walk
///   via `step`, with CRLF collapse only when the leading character
///   was 1 byte and the next is `\r`/`\n`.
pub(crate) mod multibyte {
    use super::{DocumentEncoding, EncodingEngine};
    use std::sync::OnceLock;

    /// Maximum byte distance the backward scan walks looking for a
    /// known-aligned anchor (start of line) when computing
    /// `step_backward` for variable-length CJK encodings.
    ///
    /// Mirrors `APPROX_LINE_BACKTRACK_BYTES` in `src/document.rs` so
    /// the heuristic stays consistent with the document layer's own
    /// line-anchoring backscan window. For inputs longer than this
    /// limit the anchor falls back to `offset - 64 KiB` (a heuristic
    /// floor); if even the heuristic walk doesn't converge on
    /// `offset` exactly the function returns `1` as a deg-fallback so
    /// callers resync on the next `next_line_start`.
    pub(super) const APPROX_LINE_BACKTRACK_BYTES: usize = 64 * 1024;

    /// Discriminant for the three CJK encodings driven by
    /// `MultiByteEngine`.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum CjkKind {
        /// `Shift_JIS` (Japanese): lead `0x81..=0x9F | 0xE0..=0xFC` →
        /// 2 bytes; otherwise 1 byte.
        ShiftJis,
        /// `gb18030` (Simplified Chinese): variable 1 / 2 / 4 bytes per
        /// character.
        Gb18030,
        /// `EUC-KR` (Korean): the WHATWG label `EUC-KR` actually
        /// dispatches to UHC (windows-949) inside `encoding_rs`, so
        /// the lead range is the UHC superset `0x81..=0xFE` → 2 bytes;
        /// otherwise 1 byte. The narrower KS X 1001 range
        /// (`0xA1..=0xFE`) misses Hangul syllables whose UHC encoding
        /// uses a lead in `0x81..=0xA0` (e.g. `U+B98B` → `[0x90,
        /// 0x45]`), so the detector must follow UHC, not pure
        /// KS X 1001.
        EucKr,
    }

    impl CjkKind {
        /// Canonical encoding label as known by `encoding_rs`.
        const fn label(self) -> &'static str {
            match self {
                CjkKind::ShiftJis => "Shift_JIS",
                CjkKind::Gb18030 => "gb18030",
                CjkKind::EucKr => "EUC-KR",
            }
        }
    }

    /// Byte-level engine for variable-length CJK encodings.
    ///
    /// One concrete type covers all three CJK encodings via the `kind`
    /// discriminant; `engine_for_encoding` hands out a separately
    /// cached `&'static MultiByteEngine` for each kind through its own
    /// `OnceLock`, so callers always observe the same trait object for
    /// a given encoding (Property 5).
    #[derive(Debug, Clone, Copy)]
    pub(crate) struct MultiByteEngine {
        kind: CjkKind,
        encoding: DocumentEncoding,
    }

    impl MultiByteEngine {
        /// Constructs the engine for `kind`.
        ///
        /// `encoding_rs` is expected to know the canonical label for
        /// every variant of [`CjkKind`]; failure to resolve it would
        /// indicate a build-time configuration error (the encoding is
        /// hardcoded in `CjkKind::label`), so the lookup is allowed to
        /// panic.
        pub(crate) fn new(kind: CjkKind) -> Self {
            let encoding = DocumentEncoding::from_label(kind.label())
                .unwrap_or_else(|| panic!("encoding_rs should know {}", kind.label()));
            Self { kind, encoding }
        }

        /// Returns the byte length of the character that begins at
        /// `offset` within `bytes[..end]`, based on the leading-byte
        /// table for this kind.
        ///
        /// - Returns `0` when `offset >= end` (no bytes remain).
        /// - Returns the full multi-byte length when there are enough
        ///   bytes for the sequence; otherwise falls back to `1` so
        ///   forward iteration cannot stall on a truncated tail
        ///   (`step` treats the orphan as malformed; encoding_rs would
        ///   render it as U+FFFD on decode).
        ///
        /// Detector tables:
        ///
        /// - **Shift_JIS** — lead in `0x81..=0x9F` or `0xE0..=0xFC`
        ///   starts a 2-byte sequence (one trail byte).
        /// - **GB18030** — lead in `0x81..=0xFE` peeks at the next
        ///   byte: a trail in `0x40..=0x7E` or `0x80..=0xFE` makes a
        ///   2-byte character; a trail in `0x30..=0x39` (and 4 bytes
        ///   total) makes a 4-byte sequence; anything else is a
        ///   1-byte ASCII / fallback.
        /// - **EUC-KR** — `encoding_rs`'s `EUC-KR` label dispatches
        ///   to UHC (windows-949), whose lead range is the UHC
        ///   superset `0x81..=0xFE` (e.g. `U+B98B` →
        ///   `[0x90, 0x45]` has a lead in `0x81..=0xA0` outside the
        ///   narrower KS X 1001 range). Any lead in that range
        ///   starts a 2-byte sequence. Trail bytes outside the legal
        ///   UHC trail set
        ///   (`0x41..=0x5A | 0x61..=0x7A | 0x81..=0xFE`) still
        ///   advance by 2 — `encoding_rs::Decoder` consumes the
        ///   trail byte unconditionally and emits U+FFFD when it is
        ///   invalid, so the engine matches that walk by treating
        ///   the trail loosely.
        #[inline]
        pub(crate) fn char_len(&self, bytes: &[u8], offset: usize, end: usize) -> usize {
            let end = end.min(bytes.len());
            if offset >= end {
                return 0;
            }
            let remaining = end - offset;
            let b = bytes[offset];
            match self.kind {
                CjkKind::ShiftJis => match b {
                    0x81..=0x9F | 0xE0..=0xFC if remaining >= 2 => 2,
                    _ => 1,
                },
                CjkKind::Gb18030 => match b {
                    0x00..=0x7F => 1,
                    0x81..=0xFE if remaining >= 2 => {
                        let t = bytes[offset + 1];
                        match t {
                            0x40..=0x7E | 0x80..=0xFE => 2,
                            0x30..=0x39 if remaining >= 4 => 4,
                            _ => 1,
                        }
                    }
                    _ => 1,
                },
                CjkKind::EucKr => match b {
                    0x81..=0xFE if remaining >= 2 => 2,
                    _ => 1,
                },
            }
        }
    }

    impl EncodingEngine for MultiByteEngine {
        fn encoding(&self) -> DocumentEncoding {
            self.encoding
        }

        /// Forward step via [`Self::char_len`].
        ///
        /// Returns `0` past `end`; otherwise the leading-byte detector
        /// length (see [`Self::char_len`]). Insufficient bytes for a
        /// multi-byte sequence fall back to `1` so the cursor still
        /// advances on malformed tails.
        fn step(&self, bytes: &[u8], offset: usize, end: usize) -> usize {
            self.char_len(bytes, offset, end)
        }

        /// Backward step via scan-from-anchor.
        ///
        /// Variable-length CJK encodings have no cheap way to inspect
        /// the byte right before `offset` and decide how many bytes
        /// belong to the character ending there: the trailing byte of
        /// a 2- or 4-byte sequence can collide with leading-byte
        /// patterns. Instead we look for a *known-aligned* anchor
        /// behind `offset` (the simplest one is the start of a line —
        /// any byte right after a 1-byte `\n` is on a character
        /// boundary because line breaks only ever appear as 1-byte
        /// sequences) and walk forward through `char_len` until the
        /// cursor lands on `offset`. The last forward step before that
        /// is the answer.
        ///
        /// Search behaviour:
        ///
        /// - Clamp `offset` to `bytes.len()`. Defensively treat
        ///   `offset <= start` as `0`.
        /// - Scan up to `APPROX_LINE_BACKTRACK_BYTES` bytes back from
        ///   `offset` (clamped at `start`) looking for `\n` or `\r`.
        ///   Found at byte `P`: anchor at `P + 1` (or `P + 2` for the
        ///   `\r\n` pair) — both positions are guaranteed character
        ///   boundaries because `\n` / `\r` cannot appear as the
        ///   trailing byte of any CJK multi-byte sequence we support.
        /// - If no line break is found within the window, fall back to
        ///   `max(start, offset - APPROX_LINE_BACKTRACK_BYTES)` as a
        ///   heuristic anchor. This is *not* guaranteed to sit on a
        ///   character boundary, so the forward walk may overshoot
        ///   `offset` (landing past it). In that case we return `1`
        ///   as a deg-fallback: callers resync on the next
        ///   `next_line_start` and the cursor recovers within one
        ///   character.
        fn step_backward(&self, bytes: &[u8], offset: usize, start: usize) -> usize {
            // Clamp offset to slice bounds and reject empty /
            // inverted ranges defensively.
            let offset = offset.min(bytes.len());
            if offset <= start {
                return 0;
            }
            let start = start.min(offset);

            // Locate the closest line-start anchor in the window.
            // 1-byte LF/CR cannot collide with CJK trail bytes so the
            // byte right after them is always on a character boundary.
            let scan_floor = offset
                .saturating_sub(APPROX_LINE_BACKTRACK_BYTES)
                .max(start);
            let window = &bytes[scan_floor..offset];
            let anchor = match window.iter().rposition(|b| matches!(*b, b'\n' | b'\r')) {
                Some(rel) => {
                    let idx = scan_floor + rel;
                    if bytes[idx] == b'\r' && idx + 1 < offset && bytes[idx + 1] == b'\n' {
                        idx + 2
                    } else {
                        idx + 1
                    }
                }
                None => scan_floor, // deg-fallback anchor (heuristic).
            };

            // Walk forward from the anchor through char_len; remember
            // the last completed step so we can return it once the
            // cursor reaches `offset`.
            //
            // Special case: if the anchor already sits exactly on
            // `offset`, the character ending at `offset` is the LF
            // (or the trailing LF of a CRLF pair) — both 1-byte
            // characters by construction (line breaks never
            // appear as trail bytes of CJK sequences).
            if anchor == offset {
                return 1;
            }

            let mut cursor = anchor;
            let mut last_step = 0usize;
            while cursor < offset {
                let step = self.char_len(bytes, cursor, offset);
                if step == 0 {
                    // Slice exhausted before we reached `offset`: this
                    // can only happen if `offset > bytes.len()`, which
                    // we already clamped — treat as deg-fallback.
                    return 1;
                }
                last_step = step;
                cursor += step;
            }

            // Normal case: cursor landed exactly on `offset`. The last
            // forward step is the byte length of the character that
            // ended there.
            if cursor == offset && last_step > 0 {
                last_step
            } else {
                // Forward walk overshot `offset` — the heuristic
                // anchor wasn't on a character boundary. Return 1 as
                // a deg-fallback; the document layer will
                // resync on the next `next_line_start`.
                1
            }
        }

        /// False-positive-aware newline scan via character walk.
        ///
        /// Walks characters from `line_start` using [`Self::char_len`]
        /// and only inspects `\n` / `\r` at positions where the current
        /// character is a single byte (i.e. ASCII). Trailing bytes of
        /// 2- or 4-byte sequences are consumed as part of the leading
        /// step before we ever inspect them, so a stray `0x0A` or
        /// `0x0D` inside a multi-byte character is never mistaken for
        /// a line break. The returned offset is
        /// always on a character boundary of the target encoding —
        /// either past a 1-byte LF, past the 1-byte LF of a CRLF pair,
        /// past a lone 1-byte CR, or `file_len` when no line break is
        /// reachable from `line_start`.
        ///
        /// `memchr2(b'\n', b'\r', bytes)` is intentionally *not* used:
        /// even though the trail-byte ranges of `Shift_JIS`,
        /// `gb18030` and `EUC-KR` exclude `0x0A` and `0x0D` by the
        /// encoding standards, a partial chunk that begins in the
        /// middle of a multi-byte character would let `memchr2`
        /// surface a `0x0A` / `0x0D` that the surrounding character
        /// walk would otherwise consume as part of a 2- / 4-byte
        /// step. The character walk stays O(n) over the same bytes
        /// `memchr2` would touch, but resyncs on every character
        /// boundary so it is robust against ill-formed input as well.
        ///
        /// CRLF collapses to one boundary only when both `\r` and
        /// `\n` are observed as 1-byte characters in sequence.
        fn next_line_start(&self, bytes: &[u8], file_len: usize, line_start: usize) -> usize {
            let file_len = file_len.min(bytes.len());
            let mut p = line_start.min(file_len);
            while p < file_len {
                let step = self.char_len(bytes, p, file_len);
                if step == 0 {
                    break;
                }
                if step == 1 {
                    let b = bytes[p];
                    if b == b'\n' {
                        return (p + 1).min(file_len);
                    }
                    if b == b'\r' {
                        // CRLF collapse: if the next character is a
                        // 1-byte LF, consume both as one line break.
                        let q = p + 1;
                        if q < file_len {
                            let next_step = self.char_len(bytes, q, file_len);
                            if next_step == 1 && bytes[q] == b'\n' {
                                return (q + 1).min(file_len);
                            }
                        }
                        return (p + 1).min(file_len);
                    }
                }
                p += step;
            }
            file_len
        }

        fn count_columns_exact(&self, bytes: &[u8]) -> usize {
            self.count_columns_bounded(bytes, usize::MAX)
        }

        fn count_columns_bounded(&self, bytes: &[u8], max_cols: usize) -> usize {
            let len = bytes.len();
            let mut p = 0usize;
            let mut cols = 0usize;
            while cols < max_cols && p < len {
                let step = self.char_len(bytes, p, len);
                if step == 0 {
                    break;
                }
                if step == 1 {
                    let b = bytes[p];
                    if b == b'\n' || b == b'\r' {
                        return cols;
                    }
                }
                p += step;
                cols += 1;
            }
            cols
        }

        fn advance_offset_by_text_units(
            &self,
            bytes: &[u8],
            file_len: usize,
            start: usize,
            text_units: usize,
        ) -> usize {
            let file_len = file_len.min(bytes.len());
            let mut p = start.min(file_len);
            if p >= file_len || text_units == 0 {
                return p;
            }
            let mut remaining = text_units;
            while remaining > 0 && p < file_len {
                let step = self.char_len(bytes, p, file_len);
                if step == 0 {
                    break;
                }
                let was_cr = step == 1 && bytes[p] == b'\r';
                p += step;
                remaining -= 1;
                // CRLF collapses to one text unit. The leading
                // character was 1-byte CR, so if the next character is
                // 1-byte LF, swallow it without consuming a new text
                // unit.
                if was_cr && p < file_len {
                    let next_step = self.char_len(bytes, p, file_len);
                    if next_step == 1 && bytes[p] == b'\n' {
                        p += 1;
                    }
                }
            }
            p.min(file_len)
        }
    }

    /// Returns the cached `&'static dyn EncodingEngine` for
    /// `Shift_JIS`, `gb18030` or `EUC-KR`, or `None` for any other
    /// encoding.
    ///
    /// Each kind gets its own `OnceLock<MultiByteEngine>` so callers
    /// always observe the same trait object across calls (Property 5:
    /// pointer-stable dispatch).
    pub(super) fn multibyte_engine_for(
        encoding: DocumentEncoding,
    ) -> Option<&'static dyn EncodingEngine> {
        static SJIS: OnceLock<MultiByteEngine> = OnceLock::new();
        static GB: OnceLock<MultiByteEngine> = OnceLock::new();
        static EUCK: OnceLock<MultiByteEngine> = OnceLock::new();
        match encoding.name() {
            "Shift_JIS" => Some(SJIS.get_or_init(|| MultiByteEngine::new(CjkKind::ShiftJis))),
            "gb18030" => Some(GB.get_or_init(|| MultiByteEngine::new(CjkKind::Gb18030))),
            "EUC-KR" => Some(EUCK.get_or_init(|| MultiByteEngine::new(CjkKind::EucKr))),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Tests confirming Utf8Engine matches the original free-function
// behavior across a few illustrative inputs. This is a baseline that must
// stay green when later phases swap inner implementations behind the same
// trait.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_engine_step_matches_free_function_for_ascii() {
        let bytes = b"abc\n";
        let engine = Utf8Engine;
        for i in 0..bytes.len() {
            assert_eq!(
                engine.step(bytes, i, bytes.len()),
                super::super::utf8_step(bytes, i, bytes.len()),
                "step disagreement at offset {i}",
            );
        }
    }

    #[test]
    fn utf8_engine_step_matches_free_function_for_multibyte() {
        // "А" is U+0410, 2 bytes in UTF-8 (D0 90). Stepping at the leading
        // byte should yield 2.
        let bytes = "А".as_bytes();
        let engine = Utf8Engine;
        assert_eq!(engine.step(bytes, 0, bytes.len()), 2);
    }

    #[test]
    fn utf8_engine_next_line_start_handles_crlf() {
        let bytes = b"line1\r\nline2\n";
        let engine = Utf8Engine;
        let after_first = engine.next_line_start(bytes, bytes.len(), 0);
        assert_eq!(after_first, 7, "CRLF must collapse to a single 2-byte step");
    }

    #[test]
    fn utf8_engine_count_columns_exact_excludes_trailing_newline() {
        // The exact column counter walks until the first line-ending byte
        // and excludes it.
        let bytes = b"abc\n";
        let engine = Utf8Engine;
        // Pass only the line content (no trailing newline) since
        // count_columns_exact stops at the first newline byte anyway. Both
        // forms must agree.
        assert_eq!(engine.count_columns_exact(b"abc"), 3);
        assert_eq!(engine.count_columns_exact(bytes), 3);
    }

    #[test]
    fn utf8_engine_count_columns_bounded_caps_at_max_cols() {
        let bytes = b"abcdef";
        let engine = Utf8Engine;
        assert_eq!(engine.count_columns_bounded(bytes, 3), 3);
        assert_eq!(engine.count_columns_bounded(bytes, 100), 6);
    }

    #[test]
    fn utf8_engine_advance_by_text_units_handles_crlf() {
        let bytes = b"a\r\nb";
        let engine = Utf8Engine;
        // Advancing by 2 text units from offset 0 must consume "a" then
        // "\r\n" (one text unit) and land at the byte for 'b'.
        let after = engine.advance_offset_by_text_units(bytes, bytes.len(), 0, 2);
        assert_eq!(after, 3);
    }

    // -----------------------------------------------------------------
    // SingleByteEngine
    // -----------------------------------------------------------------

    fn cp1251_engine() -> SingleByteEngine {
        let encoding =
            DocumentEncoding::from_label("windows-1251").expect("windows-1251 is a known encoding");
        SingleByteEngine::new(encoding)
    }

    #[test]
    fn single_byte_engine_supports_known_class_a_encodings() {
        for label in [
            "windows-1251",
            "windows-1252",
            "ISO-8859-15",
            "KOI8-R",
            "IBM866",
        ] {
            let encoding = DocumentEncoding::from_label(label)
                .unwrap_or_else(|| panic!("encoding_rs should know {label}"));
            assert!(
                SingleByteEngine::supports(encoding),
                "{label} should be supported"
            );
        }

        // Multibyte / variable-width encodings must not be claimed.
        for label in ["UTF-8", "UTF-16LE", "UTF-16BE", "Shift_JIS", "GB18030"] {
            let encoding = DocumentEncoding::from_label(label)
                .unwrap_or_else(|| panic!("encoding_rs should know {label}"));
            assert!(
                !SingleByteEngine::supports(encoding),
                "{label} must not be claimed by SingleByteEngine"
            );
        }
    }

    #[test]
    fn single_byte_engine_step_is_one_byte_per_unit() {
        // Bytes 0xC0..=0xC4 are uppercase Cyrillic А..Д in windows-1251.
        let bytes = [0xC0u8, 0xC1, 0xC2, b'\n'];
        let engine = cp1251_engine();
        for i in 0..bytes.len() {
            assert_eq!(engine.step(&bytes, i, bytes.len()), 1, "step at {i}");
        }
        // step at end yields 0 so iteration cannot advance past it.
        assert_eq!(engine.step(&bytes, bytes.len(), bytes.len()), 0);
    }

    #[test]
    fn single_byte_engine_next_line_start_collapses_crlf() {
        let bytes = b"\xC0\xC1\r\n\xC2\xC3\n";
        let engine = cp1251_engine();
        assert_eq!(
            engine.next_line_start(bytes, bytes.len(), 0),
            4,
            "CRLF after two cp1251 bytes must skip both"
        );
        assert_eq!(
            engine.next_line_start(bytes, bytes.len(), 4),
            7,
            "trailing LF on line 2"
        );
    }

    #[test]
    fn single_byte_engine_count_columns_exact_excludes_newline() {
        // Three cp1251 bytes followed by LF: exactly three columns.
        let bytes = b"\xC0\xC1\xC2\n";
        let engine = cp1251_engine();
        assert_eq!(engine.count_columns_exact(bytes), 3);
        // No newline at all: every byte counts.
        assert_eq!(engine.count_columns_exact(b"\xC0\xC1"), 2);
    }

    #[test]
    fn single_byte_engine_count_columns_bounded_caps_at_max_cols() {
        let bytes = b"\xC0\xC1\xC2\xC3\xC4";
        let engine = cp1251_engine();
        assert_eq!(engine.count_columns_bounded(bytes, 3), 3);
        assert_eq!(engine.count_columns_bounded(bytes, 100), 5);
        // Newline before max_cols still wins.
        let bytes_nl = b"\xC0\xC1\n\xC3\xC4";
        assert_eq!(engine.count_columns_bounded(bytes_nl, 100), 2);
    }

    #[test]
    fn single_byte_engine_advance_by_text_units_handles_crlf() {
        // cp1251 'А' (0xC0), CR, LF, cp1251 'Б' (0xC1).
        let bytes = b"\xC0\r\n\xC1";
        let engine = cp1251_engine();
        // Advance by 2 text units: consume 0xC0, then "\r\n" (one unit),
        // landing at 0xC1.
        let after = engine.advance_offset_by_text_units(bytes, bytes.len(), 0, 2);
        assert_eq!(after, 3);
    }

    #[test]
    fn single_byte_engine_advance_treats_high_bytes_as_one_unit() {
        // Five Cyrillic bytes — each must be one text unit.
        let bytes = b"\xC0\xC1\xC2\xC3\xC4";
        let engine = cp1251_engine();
        let after = engine.advance_offset_by_text_units(bytes, bytes.len(), 0, 4);
        assert_eq!(after, 4);
    }

    // -----------------------------------------------------------------
    // step_backward — round-trip with step
    // -----------------------------------------------------------------

    #[test]
    fn utf8_engine_step_backward_walks_to_char_boundary() {
        // "А" is U+0410, 2 bytes in UTF-8 (D0 90). Backward step from the
        // end must land at the leading byte (offset 0) and report 2.
        let bytes = "А".as_bytes();
        let engine = Utf8Engine;
        assert_eq!(engine.step_backward(bytes, bytes.len(), 0), 2);
        // ASCII at the boundary returns 1.
        let bytes = b"abc";
        assert_eq!(engine.step_backward(bytes, 3, 0), 1);
        // At `start` no step is possible.
        assert_eq!(engine.step_backward(bytes, 0, 0), 0);
    }

    #[test]
    fn utf8_engine_step_backward_collapses_crlf() {
        let bytes = b"a\r\nb";
        let engine = Utf8Engine;
        // Stepping back from byte index 3 (the 'b') across "\r\n" must
        // collapse to a single 2-byte text unit.
        assert_eq!(engine.step_backward(bytes, 3, 0), 2);
    }

    #[test]
    fn single_byte_engine_step_backward_is_one_byte_per_unit() {
        let bytes = [0xC0u8, 0xC1, 0xC2];
        let engine = cp1251_engine();
        assert_eq!(engine.step_backward(&bytes, 0, 0), 0);
        for i in 1..=bytes.len() {
            assert_eq!(
                engine.step_backward(&bytes, i, 0),
                1,
                "step_backward at {i}"
            );
        }
    }

    #[test]
    fn single_byte_engine_step_backward_collapses_crlf() {
        let bytes = b"\xC0\r\n\xC1";
        let engine = cp1251_engine();
        // Backward step from the byte after "\r\n" (offset 3) must
        // collapse to 2.
        assert_eq!(engine.step_backward(bytes, 3, 0), 2);
        // Single CR (no following LF) is one byte.
        let cr_only = b"\xC0\r\xC1";
        assert_eq!(engine.step_backward(cr_only, 2, 0), 1);
    }

    // -----------------------------------------------------------------
    // Utf16Engine — surrogate-aware step / step_backward
    // -----------------------------------------------------------------

    fn utf16le_engine() -> &'static dyn EncodingEngine {
        let encoding =
            DocumentEncoding::from_label("UTF-16LE").expect("UTF-16LE is a known encoding");
        engine_for_encoding(encoding)
    }

    fn utf16be_engine() -> &'static dyn EncodingEngine {
        let encoding =
            DocumentEncoding::from_label("UTF-16BE").expect("UTF-16BE is a known encoding");
        engine_for_encoding(encoding)
    }

    #[test]
    fn utf16_le_step_distinguishes_bmp_from_supplementary() {
        // 'A' (U+0041) — BMP, 2 bytes. '𐐷' (U+10437) — supplementary,
        // 4 bytes encoded as `D801 DC37` (LE: 01 D8 37 DC).
        let bytes = [0x41, 0x00, 0x01, 0xD8, 0x37, 0xDC, 0x42, 0x00];
        let engine = utf16le_engine();
        assert_eq!(engine.step(&bytes, 0, bytes.len()), 2, "BMP 'A'");
        assert_eq!(engine.step(&bytes, 2, bytes.len()), 4, "supplementary pair");
        assert_eq!(engine.step(&bytes, 6, bytes.len()), 2, "BMP 'B'");
        assert_eq!(engine.step(&bytes, bytes.len(), bytes.len()), 0, "at end");
    }

    #[test]
    fn utf16_le_step_handles_lone_high_surrogate() {
        // High surrogate alone (no low surrogate after) — must advance by
        // one code unit (2 bytes), not 4.
        let bytes = [0x01, 0xD8, 0x41, 0x00];
        let engine = utf16le_engine();
        assert_eq!(engine.step(&bytes, 0, bytes.len()), 2);
        // Insufficient bytes for any code unit.
        assert_eq!(engine.step(&bytes, 0, 1), 0);
    }

    #[test]
    fn utf16_be_step_distinguishes_bmp_from_supplementary() {
        // BE: 'A' = 00 41, '𐐷' = D8 01 DC 37, 'B' = 00 42.
        let bytes = [0x00, 0x41, 0xD8, 0x01, 0xDC, 0x37, 0x00, 0x42];
        let engine = utf16be_engine();
        assert_eq!(engine.step(&bytes, 0, bytes.len()), 2);
        assert_eq!(engine.step(&bytes, 2, bytes.len()), 4);
        assert_eq!(engine.step(&bytes, 6, bytes.len()), 2);
    }

    #[test]
    fn utf16_le_step_backward_distinguishes_bmp_from_supplementary() {
        let bytes = [0x41, 0x00, 0x01, 0xD8, 0x37, 0xDC, 0x42, 0x00];
        let engine = utf16le_engine();
        // Walking back from end: 'B' is 2 bytes, then supplementary is
        // 4 bytes, then 'A' is 2 bytes.
        assert_eq!(engine.step_backward(&bytes, 8, 0), 2, "back over 'B'");
        assert_eq!(engine.step_backward(&bytes, 6, 0), 4, "back over pair");
        assert_eq!(engine.step_backward(&bytes, 2, 0), 2, "back over 'A'");
        assert_eq!(engine.step_backward(&bytes, 0, 0), 0, "at start");
    }

    #[test]
    fn utf16_be_step_backward_distinguishes_bmp_from_supplementary() {
        let bytes = [0x00, 0x41, 0xD8, 0x01, 0xDC, 0x37, 0x00, 0x42];
        let engine = utf16be_engine();
        assert_eq!(engine.step_backward(&bytes, 8, 0), 2);
        assert_eq!(engine.step_backward(&bytes, 6, 0), 4);
        assert_eq!(engine.step_backward(&bytes, 2, 0), 2);
        assert_eq!(engine.step_backward(&bytes, 0, 0), 0);
    }

    #[test]
    fn utf16_step_forward_backward_round_trip() {
        // For every offset reachable by forward stepping from the
        // start, walking backward returns the same step size.
        let bytes_le = [0x41, 0x00, 0x01, 0xD8, 0x37, 0xDC, 0x42, 0x00];
        for engine in [utf16le_engine()] {
            let mut p = 0usize;
            while p < bytes_le.len() {
                let fwd = engine.step(&bytes_le, p, bytes_le.len());
                assert!(fwd > 0);
                let next = p + fwd;
                let back = engine.step_backward(&bytes_le, next, 0);
                assert_eq!(fwd, back, "round-trip mismatch at p={p}");
                p = next;
            }
        }

        let bytes_be = [0x00, 0x41, 0xD8, 0x01, 0xDC, 0x37, 0x00, 0x42];
        for engine in [utf16be_engine()] {
            let mut p = 0usize;
            while p < bytes_be.len() {
                let fwd = engine.step(&bytes_be, p, bytes_be.len());
                assert!(fwd > 0);
                let next = p + fwd;
                let back = engine.step_backward(&bytes_be, next, 0);
                assert_eq!(fwd, back, "round-trip mismatch at p={p}");
                p = next;
            }
        }
    }

    // -----------------------------------------------------------------
    // Utf16Engine — column counters, advance_offset_by_text_units, and
    // 2-byte aligned next_line_start scan with misaligned LF/CR
    // rejection.
    // -----------------------------------------------------------------

    #[test]
    fn utf16_le_count_columns_exact_bmp_only() {
        // "ABC\n" in UTF-16LE: 3 BMP code units then LF cell.
        let bytes = [
            0x41, 0x00, // 'A'
            0x42, 0x00, // 'B'
            0x43, 0x00, // 'C'
            0x0A, 0x00, // LF
        ];
        let engine = utf16le_engine();
        assert_eq!(engine.count_columns_exact(&bytes), 3);
        // Without any line-ending: every BMP cell counts.
        let bytes_no_nl = [0x41, 0x00, 0x42, 0x00, 0x43, 0x00];
        assert_eq!(engine.count_columns_exact(&bytes_no_nl), 3);
    }

    #[test]
    fn utf16_be_count_columns_exact_bmp_only() {
        // "ABC\n" in UTF-16BE.
        let bytes = [0x00, 0x41, 0x00, 0x42, 0x00, 0x43, 0x00, 0x0A];
        let engine = utf16be_engine();
        assert_eq!(engine.count_columns_exact(&bytes), 3);
    }

    #[test]
    fn utf16_le_count_columns_counts_supplementary_as_one() {
        // 'A' (BMP, 2B) + '𐐷' U+10437 supplementary (4B as D801 DC37
        // → LE: 01 D8 37 DC) + 'B' (BMP, 2B) = 3 columns.
        let bytes = [0x41, 0x00, 0x01, 0xD8, 0x37, 0xDC, 0x42, 0x00];
        let engine = utf16le_engine();
        assert_eq!(engine.count_columns_exact(&bytes), 3);
    }

    #[test]
    fn utf16_be_count_columns_counts_supplementary_as_one() {
        // BE: 'A' = 00 41, '𐐷' = D8 01 DC 37, 'B' = 00 42.
        let bytes = [0x00, 0x41, 0xD8, 0x01, 0xDC, 0x37, 0x00, 0x42];
        let engine = utf16be_engine();
        assert_eq!(engine.count_columns_exact(&bytes), 3);
    }

    #[test]
    fn utf16_le_count_columns_bounded_caps_at_max_cols() {
        let bytes = [0x41, 0x00, 0x42, 0x00, 0x43, 0x00, 0x44, 0x00, 0x45, 0x00];
        let engine = utf16le_engine();
        assert_eq!(engine.count_columns_bounded(&bytes, 3), 3);
        assert_eq!(engine.count_columns_bounded(&bytes, 100), 5);
        // LF before max_cols still wins.
        let bytes_nl = [0x41, 0x00, 0x42, 0x00, 0x0A, 0x00, 0x43, 0x00];
        assert_eq!(engine.count_columns_bounded(&bytes_nl, 100), 2);
    }

    #[test]
    fn utf16_le_count_columns_ignores_misaligned_lf_inside_bmp_cell() {
        // U+0A01 ("ਁ") encoded LE: 01 0A. The trailing 0x0A byte sits at
        // an odd byte position (index 1) and must NOT be treated as a
        // line break. It is one column.
        let bytes = [0x01, 0x0A, 0x42, 0x00];
        let engine = utf16le_engine();
        assert_eq!(engine.count_columns_exact(&bytes), 2);
    }

    #[test]
    fn utf16_le_count_columns_ignores_misaligned_cr_inside_bmp_cell() {
        // U+0D01 (Malayalam): LE bytes 01 0D — trailing 0x0D at odd
        // index must not be interpreted as CR.
        let bytes = [0x01, 0x0D, 0x42, 0x00];
        let engine = utf16le_engine();
        assert_eq!(engine.count_columns_exact(&bytes), 2);
    }

    #[test]
    fn utf16_le_advance_offset_by_text_units_bmp() {
        let bytes = [0x41, 0x00, 0x42, 0x00, 0x43, 0x00, 0x44, 0x00];
        let engine = utf16le_engine();
        assert_eq!(
            engine.advance_offset_by_text_units(&bytes, bytes.len(), 0, 0),
            0
        );
        assert_eq!(
            engine.advance_offset_by_text_units(&bytes, bytes.len(), 0, 1),
            2
        );
        assert_eq!(
            engine.advance_offset_by_text_units(&bytes, bytes.len(), 0, 3),
            6
        );
        // Past the end clamps to file_len.
        assert_eq!(
            engine.advance_offset_by_text_units(&bytes, bytes.len(), 0, 99),
            bytes.len()
        );
    }

    #[test]
    fn utf16_le_advance_offset_collapses_crlf() {
        // 'A' CR LF 'B' in LE: 41 00 0D 00 0A 00 42 00.
        let bytes = [0x41, 0x00, 0x0D, 0x00, 0x0A, 0x00, 0x42, 0x00];
        let engine = utf16le_engine();
        // 2 text units from start: 'A' (2 bytes), then "\r\n" (one
        // text unit, 4 bytes), landing at 'B' offset 6.
        let after = engine.advance_offset_by_text_units(&bytes, bytes.len(), 0, 2);
        assert_eq!(after, 6);
        // 3 text units: A, CRLF, B → end of slice.
        let after_three = engine.advance_offset_by_text_units(&bytes, bytes.len(), 0, 3);
        assert_eq!(after_three, 8);
    }

    #[test]
    fn utf16_be_advance_offset_collapses_crlf() {
        // BE: 00 41 00 0D 00 0A 00 42.
        let bytes = [0x00, 0x41, 0x00, 0x0D, 0x00, 0x0A, 0x00, 0x42];
        let engine = utf16be_engine();
        let after = engine.advance_offset_by_text_units(&bytes, bytes.len(), 0, 2);
        assert_eq!(after, 6);
    }

    #[test]
    fn utf16_le_advance_offset_lf_only_is_one_unit() {
        // 'A' LF 'B' in LE: 41 00 0A 00 42 00.
        let bytes = [0x41, 0x00, 0x0A, 0x00, 0x42, 0x00];
        let engine = utf16le_engine();
        // 2 text units: 'A' then LF — no CR present, no collapse.
        let after = engine.advance_offset_by_text_units(&bytes, bytes.len(), 0, 2);
        assert_eq!(after, 4);
    }

    #[test]
    fn utf16_le_advance_offset_cr_only_is_one_unit() {
        // 'A' CR 'B' (no following LF): 41 00 0D 00 42 00.
        let bytes = [0x41, 0x00, 0x0D, 0x00, 0x42, 0x00];
        let engine = utf16le_engine();
        // 2 text units: 'A' then CR alone (no collapse since next cell
        // is 'B', not LF).
        let after = engine.advance_offset_by_text_units(&bytes, bytes.len(), 0, 2);
        assert_eq!(after, 4);
    }

    #[test]
    fn utf16_le_advance_offset_supplementary_pair_is_one_unit() {
        // 'A' + '𐐷' (supplementary, 4 bytes) + 'B' in LE.
        let bytes = [0x41, 0x00, 0x01, 0xD8, 0x37, 0xDC, 0x42, 0x00];
        let engine = utf16le_engine();
        // 2 text units: 'A' (2B) + supplementary (4B) → offset 6.
        let after = engine.advance_offset_by_text_units(&bytes, bytes.len(), 0, 2);
        assert_eq!(after, 6);
        // 3 text units: through 'B' → end.
        let after_three = engine.advance_offset_by_text_units(&bytes, bytes.len(), 0, 3);
        assert_eq!(after_three, bytes.len());
    }

    #[test]
    fn utf16_le_advance_offset_aligns_odd_start() {
        // Starting at an odd byte must be rounded up to the next even
        // boundary before any cell read.
        let bytes = [0x41, 0x00, 0x42, 0x00];
        let engine = utf16le_engine();
        // start = 1 → aligned to 2; advance 1 unit → offset 4.
        let after = engine.advance_offset_by_text_units(&bytes, bytes.len(), 1, 1);
        assert_eq!(after, 4);
    }

    #[test]
    fn utf16_le_next_line_start_rejects_misaligned_lf() {
        // U+0A01 ("ਁ") + 'B' + LF in LE: 01 0A 42 00 0A 00.
        // The 0x0A at index 1 is the high byte of U+0A01 and must NOT
        // be detected as a line break. The genuine LF
        // cell at offset 4 is the first valid line ending.
        let bytes = [0x01, 0x0A, 0x42, 0x00, 0x0A, 0x00];
        let engine = utf16le_engine();
        let next = engine.next_line_start(&bytes, bytes.len(), 0);
        assert_eq!(next, 6, "misaligned 0x0A must not terminate the line");
    }

    #[test]
    fn utf16_le_next_line_start_rejects_misaligned_cr() {
        // U+0D01 + 'B' + CR + LF in LE: 01 0D 42 00 0D 00 0A 00.
        let bytes = [0x01, 0x0D, 0x42, 0x00, 0x0D, 0x00, 0x0A, 0x00];
        let engine = utf16le_engine();
        let next = engine.next_line_start(&bytes, bytes.len(), 0);
        // CRLF starts at offset 4; collapsed line break ends at 8.
        assert_eq!(next, 8);
    }

    #[test]
    fn utf16_le_next_line_start_handles_lf_only() {
        // 'A' LF 'B': 41 00 0A 00 42 00.
        let bytes = [0x41, 0x00, 0x0A, 0x00, 0x42, 0x00];
        let engine = utf16le_engine();
        assert_eq!(engine.next_line_start(&bytes, bytes.len(), 0), 4);
    }

    #[test]
    fn utf16_le_next_line_start_handles_cr_only() {
        // 'A' CR 'B' (no following LF): 41 00 0D 00 42 00. CR alone is
        // a line ending; next line starts after CR.
        let bytes = [0x41, 0x00, 0x0D, 0x00, 0x42, 0x00];
        let engine = utf16le_engine();
        assert_eq!(engine.next_line_start(&bytes, bytes.len(), 0), 4);
    }

    #[test]
    fn utf16_be_next_line_start_handles_crlf() {
        // BE: 00 41 00 0D 00 0A 00 42 — CR at cell offset 2, LF at 4.
        let bytes = [0x00, 0x41, 0x00, 0x0D, 0x00, 0x0A, 0x00, 0x42];
        let engine = utf16be_engine();
        assert_eq!(engine.next_line_start(&bytes, bytes.len(), 0), 6);
    }

    #[test]
    fn utf16_le_next_line_start_no_break_returns_file_len() {
        let bytes = [0x41, 0x00, 0x42, 0x00, 0x43, 0x00];
        let engine = utf16le_engine();
        assert_eq!(engine.next_line_start(&bytes, bytes.len(), 0), bytes.len());
    }

    // -----------------------------------------------------------------
    // MultiByteEngine — leading-byte detector and basic step
    // -----------------------------------------------------------------

    fn shift_jis_engine() -> &'static dyn EncodingEngine {
        let encoding =
            DocumentEncoding::from_label("Shift_JIS").expect("Shift_JIS is a known encoding");
        engine_for_encoding(encoding)
    }

    fn gb18030_engine() -> &'static dyn EncodingEngine {
        let encoding =
            DocumentEncoding::from_label("gb18030").expect("gb18030 is a known encoding");
        engine_for_encoding(encoding)
    }

    fn euc_kr_engine() -> &'static dyn EncodingEngine {
        let encoding = DocumentEncoding::from_label("EUC-KR").expect("EUC-KR is a known encoding");
        engine_for_encoding(encoding)
    }

    #[test]
    fn engine_for_encoding_routes_cjk_to_multibyte() {
        // engine_for_encoding must return the canonical encoding for
        // each CJK kind (Property 5: dispatch reflects encoding).
        assert_eq!(shift_jis_engine().encoding().name(), "Shift_JIS");
        assert_eq!(gb18030_engine().encoding().name(), "gb18030");
        assert_eq!(euc_kr_engine().encoding().name(), "EUC-KR");
    }

    #[test]
    fn engine_for_encoding_returns_pointer_stable_cjk_engine() {
        // Two calls with the same label must return the same trait
        // object, so engine identity is stable per encoding (Property 5).
        let sjis_label =
            DocumentEncoding::from_label("Shift_JIS").expect("Shift_JIS is a known encoding");
        let a = engine_for_encoding(sjis_label) as *const _ as *const ();
        let b = engine_for_encoding(sjis_label) as *const _ as *const ();
        assert_eq!(a, b, "Shift_JIS engine pointer must be stable");
    }

    #[test]
    fn multibyte_engine_step_for_shift_jis_two_byte_lead() {
        // Hiragana あ is U+3042; in Shift_JIS it encodes as 0x82 0xA0.
        // 0x82 is in 0x81..=0x9F → 2 bytes.
        let bytes = [0x82u8, 0xA0];
        let engine = shift_jis_engine();
        assert_eq!(engine.step(&bytes, 0, bytes.len()), 2);
        // ASCII byte before the multi-byte sequence is one byte.
        let mixed = [b'A', 0x82, 0xA0];
        assert_eq!(engine.step(&mixed, 0, mixed.len()), 1);
        assert_eq!(engine.step(&mixed, 1, mixed.len()), 2);
        // step at end yields 0.
        assert_eq!(engine.step(&bytes, bytes.len(), bytes.len()), 0);
    }

    #[test]
    fn multibyte_engine_step_for_shift_jis_truncated_lead_is_one_byte() {
        // Lone 0x82 with no trail byte: detector falls back to 1 so
        // iteration cannot stall on a malformed tail.
        let bytes = [0x82u8];
        let engine = shift_jis_engine();
        assert_eq!(engine.step(&bytes, 0, bytes.len()), 1);
    }

    #[test]
    fn multibyte_engine_step_for_gb18030_two_byte_sequence() {
        // 中 is U+4E2D; in GB18030 it encodes as 0xD6 0xD0. Lead 0xD6 is
        // in 0x81..=0xFE; trail 0xD0 is in 0x80..=0xFE → 2-byte.
        let bytes = [0xD6u8, 0xD0];
        let engine = gb18030_engine();
        assert_eq!(engine.step(&bytes, 0, bytes.len()), 2);
        // ASCII byte is 1 byte.
        let ascii = [b'A'];
        assert_eq!(engine.step(&ascii, 0, ascii.len()), 1);
    }

    #[test]
    fn multibyte_engine_step_for_gb18030_four_byte_sequence() {
        // U+00A4 (¤) encodes in GB18030 as the 4-byte sequence
        // 0x81 0x30 0x84 0x31 (lead 0x81, then 0x30..=0x39, then
        // 0x81..=0xFE, then 0x30..=0x39).
        let bytes = [0x81u8, 0x30, 0x84, 0x31];
        let engine = gb18030_engine();
        assert_eq!(engine.step(&bytes, 0, bytes.len()), 4);
        // Truncated 4-byte sequence (only 3 bytes available) falls
        // back to 2-byte detection because the 0x30 trail flips the
        // path away from the 4-byte case when remaining < 4.
        let truncated = [0x81u8, 0x30, 0x84];
        // remaining = 3 → not >= 4, so the 0x30 trail no longer
        // triggers the 4-byte branch and we fall through to the
        // ordinary "not in 0x40..=0x7E | 0x80..=0xFE" arm, which
        // yields 1.
        assert_eq!(engine.step(&truncated, 0, truncated.len()), 1);
    }

    #[test]
    fn multibyte_engine_step_for_euc_kr_two_byte_sequence() {
        // 가 is U+AC00; in EUC-KR it encodes as 0xB0 0xA1. Lead 0xB0 is
        // in 0x81..=0xFE → 2-byte sequence.
        let bytes = [0xB0u8, 0xA1];
        let engine = euc_kr_engine();
        assert_eq!(engine.step(&bytes, 0, bytes.len()), 2);
        // ASCII byte (lead < 0x81) is 1 byte.
        let ascii = [b'A'];
        assert_eq!(engine.step(&ascii, 0, ascii.len()), 1);
        // Lead 0x80 (below the 0x81..=0xFE range) is treated as 1 byte.
        let lone = [0x80u8, 0x41];
        assert_eq!(engine.step(&lone, 0, lone.len()), 1);
        // 릋 is U+B98B; in EUC-KR (UHC / windows-949 under the WHATWG
        // `EUC-KR` label) it encodes as 0x90 0x45. Lead 0x90 is in
        // the UHC superset `0x81..=0xA0`, which the narrower KS X 1001
        // detector would have miscounted as a single byte.
        let uhc = [0x90u8, 0x45];
        assert_eq!(engine.step(&uhc, 0, uhc.len()), 2);
    }

    #[test]
    fn multibyte_engine_step_at_end_returns_zero() {
        let engine = shift_jis_engine();
        let bytes: [u8; 0] = [];
        assert_eq!(engine.step(&bytes, 0, 0), 0);
    }

    #[test]
    fn multibyte_engine_next_line_start_skips_multibyte_lead_for_lf() {
        // Shift_JIS sequence 0x82 0xA0 (Hiragana あ) followed by an
        // ASCII LF. The trailing 0xA0 is *not* 0x0A, but the test
        // demonstrates the character walk correctly consumes the
        // 2-byte sequence and only inspects the LF byte after it.
        let engine = shift_jis_engine();
        let bytes = [0x82u8, 0xA0, b'\n', b'A'];
        assert_eq!(engine.next_line_start(&bytes, bytes.len(), 0), 3);
    }

    #[test]
    fn multibyte_engine_next_line_start_collapses_crlf_at_ascii() {
        // ASCII CR LF after one Shift_JIS 2-byte sequence.
        let engine = shift_jis_engine();
        let bytes = [0x82u8, 0xA0, b'\r', b'\n', b'A'];
        assert_eq!(engine.next_line_start(&bytes, bytes.len(), 0), 4);
    }

    #[test]
    fn multibyte_engine_next_line_start_rejects_lf_in_shift_jis_trail_position() {
        // Shift_JIS lead 0x82 (in 0x81..=0x9F → 2-byte) followed by a
        // byte that *looks* like ASCII LF (0x0A). The encoding
        // standard says 0x0A cannot legally be a trail byte of
        // Shift_JIS, but `char_len` only inspects the leading byte,
        // so it consumes two bytes regardless. The character walk
        // never inspects offset 1, which means the 0x0A in the
        // trail-byte slot does *not* trigger a false-positive line
        // break.
        //
        // A naive `memchr2(b'\n', b'\r', bytes)` would surface the
        // 0x0A at offset 1 and report `next_line_start == 2`, which
        // would land the cursor inside the 2-byte sequence. The
        // character walk reports 4 — past the actual ASCII LF at
        // offset 3.
        let engine = shift_jis_engine();
        let bytes = [0x82u8, 0x0A, b'A', b'\n', b'B'];
        assert_eq!(engine.next_line_start(&bytes, bytes.len(), 0), 4);
    }

    #[test]
    fn multibyte_engine_next_line_start_rejects_cr_in_euc_kr_trail_position() {
        // EUC-KR lead 0xA1 (in 0x81..=0xFE → 2-byte) followed by a
        // byte that *looks* like ASCII CR (0x0D). Same reasoning as
        // the Shift_JIS test above: the leading-byte detector
        // consumes two bytes for any lead in `0x81..=0xFE`, so the
        // 0x0D in the trail-byte slot is never inspected. The CRLF
        // pair at offsets 3..=4 (after the malformed 2-byte head)
        // collapses to one boundary, and the result is 5.
        let engine = euc_kr_engine();
        let bytes = [0xA1u8, 0x0D, b'A', b'\r', b'\n', b'B'];
        assert_eq!(engine.next_line_start(&bytes, bytes.len(), 0), 5);
    }

    #[test]
    fn multibyte_engine_next_line_start_does_not_split_gb18030_four_byte_sequence() {
        // GB18030 4-byte sequence 0x81 0x30 0x84 0x31 followed by an
        // ASCII LF. Bytes 0x30 and 0x31 inside the sequence sit in
        // 0x30..=0x39 by the standard (and never overlap with 0x0A
        // or 0x0D), but the test pins down the broader contract:
        // `next_line_start` consumes the whole 4-byte step and
        // reports the LF at offset 4 as the line break.
        let engine = gb18030_engine();
        let bytes = [0x81u8, 0x30, 0x84, 0x31, b'\n', b'A'];
        assert_eq!(engine.next_line_start(&bytes, bytes.len(), 0), 5);
    }

    #[test]
    fn multibyte_engine_next_line_start_returns_file_len_without_line_break() {
        // No `\n` or `\r` anywhere — even with multi-byte sequences,
        // `next_line_start` must return `file_len` (result on
        // a character boundary; here that boundary is the slice end).
        let engine = shift_jis_engine();
        let bytes = [0x82u8, 0xA0, 0x82, 0xA1, b'A', b'B'];
        assert_eq!(engine.next_line_start(&bytes, bytes.len(), 0), bytes.len());
    }

    #[test]
    fn multibyte_engine_next_line_start_starts_from_line_start_offset() {
        // Walking from a non-zero `line_start` must respect the
        // caller's anchor: starting at offset 3 (right after the
        // first 2-byte SJIS char + LF) finds the LF after the
        // second 2-byte char and returns 6.
        let engine = shift_jis_engine();
        let bytes = [0x82u8, 0xA0, b'\n', 0x82, 0xA1, b'\n', b'A'];
        assert_eq!(engine.next_line_start(&bytes, bytes.len(), 3), 6);
    }

    #[test]
    fn multibyte_engine_advance_offset_walks_multibyte_chars() {
        // Two Shift_JIS 2-byte sequences (4 bytes total) — advancing
        // by 2 text units lands at the end of the slice.
        let engine = shift_jis_engine();
        let bytes = [0x82u8, 0xA0, 0x82, 0xA1];
        let after = engine.advance_offset_by_text_units(&bytes, bytes.len(), 0, 2);
        assert_eq!(after, 4);
    }

    #[test]
    fn multibyte_engine_count_columns_exact_counts_per_character() {
        // 'A' + Hiragana あ + 'B' + LF — 3 columns before LF.
        let engine = shift_jis_engine();
        let bytes = [b'A', 0x82, 0xA0, b'B', b'\n'];
        assert_eq!(engine.count_columns_exact(&bytes), 3);
    }

    #[test]
    fn step_backward_walks_to_two_byte_boundary_in_shift_jis() {
        // ASCII 'A' + Hiragana あ (Shift_JIS 0x82 0xA0) + ASCII 'B'.
        // Layout: [A][82][A0][B]  → bytes 0..4
        // step_backward should return:
        //   1 -> 1   (after 'A')
        //   2 -> 1   (after 'A' — between A and the multi-byte lead)
        //   3 -> 2   (after the 2-byte Hiragana ends at 3)
        //   4 -> 1   (after 'B')
        let engine = shift_jis_engine();
        let bytes = [b'A', 0x82, 0xA0, b'B'];
        assert_eq!(engine.step_backward(&bytes, 0, 0), 0);
        assert_eq!(engine.step_backward(&bytes, 1, 0), 1);
        assert_eq!(engine.step_backward(&bytes, 3, 0), 2);
        assert_eq!(engine.step_backward(&bytes, 4, 0), 1);
    }

    #[test]
    fn step_backward_round_trip_for_gb18030_two_and_four_byte() {
        // 'A' + 中 (GB18030 0xD6 0xD0, 2 bytes) + ¤ (GB18030 0x81 0x30
        // 0x84 0x31, 4 bytes) + 'B'.
        // Layout: [A][D6 D0][81 30 84 31][B] → bytes 0..8
        let engine = gb18030_engine();
        let bytes = [b'A', 0xD6, 0xD0, 0x81, 0x30, 0x84, 0x31, b'B'];

        // step_forward / step_backward round-trip on every character
        // boundary: 0, 1, 3, 7, 8.
        let boundaries = [0usize, 1, 3, 7, 8];
        for window in boundaries.windows(2) {
            let (a, b) = (window[0], window[1]);
            let fwd = engine.step(&bytes, a, bytes.len());
            assert_eq!(a + fwd, b, "step_forward({a}) should reach {b}");
            let back = engine.step_backward(&bytes, b, 0);
            assert_eq!(
                back, fwd,
                "step_backward({b}) should mirror step_forward({a})"
            );
        }

        // Past-the-end clamping returns the last character length.
        assert_eq!(engine.step_backward(&bytes, bytes.len(), 0), 1);
    }

    #[test]
    fn step_backward_uses_line_anchor_when_present() {
        // ASCII LF immediately before a Shift_JIS run: the line break
        // gives an unambiguous anchor that the scan must use to
        // realign forward through the multi-byte sequences.
        // Layout: ['\n'][82 A0][82 A1][B] → bytes 0..6
        let engine = shift_jis_engine();
        let bytes = [b'\n', 0x82, 0xA0, 0x82, 0xA1, b'B'];

        // Each query lands on a character boundary downstream of the
        // anchor; the scan must return the byte length of the
        // character ending at that position.
        assert_eq!(engine.step_backward(&bytes, 1, 0), 1); // after '\n'
        assert_eq!(engine.step_backward(&bytes, 3, 0), 2); // after first 2-byte char
        assert_eq!(engine.step_backward(&bytes, 5, 0), 2); // after second 2-byte char
        assert_eq!(engine.step_backward(&bytes, 6, 0), 1); // after 'B'
    }

    #[test]
    fn step_backward_falls_back_for_unanchored_long_scan() {
        // Build a slice longer than APPROX_LINE_BACKTRACK_BYTES (64 KiB)
        // with no line breaks anywhere. The backward scan won't find
        // an anchor and will use offset - 64 KiB as a heuristic; for
        // pure ASCII content (1-byte chars) the heuristic anchor is
        // still aligned and the answer is 1, which doubles as the
        // deg-fallback value, so we don't depend on which branch
        // taken — only on the contract that the result is at least
        // 1 and at most 4.
        let engine = shift_jis_engine();
        let len = multibyte::APPROX_LINE_BACKTRACK_BYTES + 4096;
        let bytes = vec![b'A'; len];
        let result = engine.step_backward(&bytes, len, 0);
        assert!(
            (1..=4).contains(&result),
            "step_backward must return a valid byte length, got {result}"
        );
    }
}

// ---------------------------------------------------------------------------
// Property 1: encoding_engine reflects current encoding.
//
// This PBT lives inside the encoding_engine module rather than under
// `tests/encoding_engine/` because `Document::encoding_engine()` is a
// pub(crate) accessor that integration tests cannot reach. After every
// public operation that can change the document's encoding contract, the
// sticky engine field must agree with the encoding field.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod prop_dispatch_tests {
    use crate::document::{Document, DocumentEncoding, DocumentOpenOptions, DocumentSaveOptions};
    use proptest::prelude::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Builds a unique scratch directory for a test, honoring TmpDir
    /// policy: `$env:TMP` / `$env:TEMP` are expected to point at
    /// `D:\qem_test_tmp` on the developer machine; if neither is set the
    /// helper falls back to that path directly.
    ///
    /// Each call returns a fresh per-process, per-counter subdirectory so
    /// concurrent test threads do not collide on file names.
    fn fresh_test_dir(name: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let base = std::env::var_os("TMP")
            .or_else(|| std::env::var_os("TEMP"))
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"D:\qem_test_tmp"));
        let pid = std::process::id();
        let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = base.join(format!("qem-prop-dispatch-{pid}-{counter}-{name}"));
        std::fs::create_dir_all(&dir).expect("fresh_test_dir: create_dir_all");
        dir
    }

    /// Encodings that are routed to a non-fallback engine today.
    ///
    /// `engine_for_encoding` only branches into a dedicated engine for
    /// UTF-8 and Class A in this property test; UTF-16 / Shift_JIS /
    /// GB18030 / EUC-KR all have their own engines exercised separately
    /// by the integration suite. Restricting the generator to UTF-8 +
    /// Class A keeps the property meaningful here and trivially extends
    /// to other engines if the dispatch contract changes.
    fn supported_encoding_strategy() -> impl Strategy<Value = DocumentEncoding> {
        // The labels below are exactly the ones SingleByteEngine claims
        // plus UTF-8 itself. Each label is known to encoding_rs.
        let labels: &[&str] = &[
            "UTF-8",
            "windows-1250",
            "windows-1251",
            "windows-1252",
            "windows-1253",
            "windows-1254",
            "windows-1255",
            "windows-1256",
            "windows-1257",
            "windows-1258",
            "windows-874",
            "ISO-8859-2",
            "ISO-8859-3",
            "ISO-8859-4",
            "ISO-8859-5",
            "ISO-8859-7",
            "ISO-8859-10",
            "ISO-8859-13",
            "ISO-8859-14",
            "ISO-8859-15",
            "ISO-8859-16",
            "KOI8-R",
            "KOI8-U",
            "IBM866",
            "macintosh",
            "x-mac-cyrillic",
        ];
        prop::sample::select(labels.to_vec()).prop_map(|label| {
            DocumentEncoding::from_label(label)
                .unwrap_or_else(|| panic!("encoding_rs should know {label}"))
        })
    }

    /// Operations the property test applies in sequence to a single
    /// document. Each variant maps to one public Document constructor or
    /// mutator that must keep `encoding_engine` aligned with `encoding`.
    #[derive(Debug, Clone)]
    enum DispatchOp {
        /// Construct a fresh empty UTF-8 document via `Document::new`.
        New,
        /// Open a tiny on-disk fixture in `encoding`. The fixture content
        /// is plain ASCII so it round-trips through every Class A label.
        OpenWithEncoding(DocumentEncoding),
        /// Open a tiny on-disk fixture using `DocumentOpenOptions` with an
        /// explicit reinterpret encoding (covers the policy path).
        OpenWithOptions(DocumentEncoding),
        /// Save-with-conversion: writes the current document text in the
        /// requested target encoding and reloads under that contract.
        SaveWithEncoding(DocumentEncoding),
    }

    fn op_strategy() -> impl Strategy<Value = DispatchOp> {
        prop_oneof![
            1 => Just(DispatchOp::New),
            2 => supported_encoding_strategy().prop_map(DispatchOp::OpenWithEncoding),
            2 => supported_encoding_strategy().prop_map(DispatchOp::OpenWithOptions),
            2 => supported_encoding_strategy().prop_map(DispatchOp::SaveWithEncoding),
        ]
    }

    /// Asserts the Property 1 invariant on `doc`.
    fn assert_engine_matches_encoding(doc: &Document, context: &str) -> Result<(), TestCaseError> {
        let doc_encoding = doc.encoding();
        let engine_encoding = doc.encoding_engine().encoding();
        prop_assert_eq!(
            engine_encoding,
            doc_encoding,
            "Property 1 violated after {}: engine encoding {} != document encoding {}",
            context,
            engine_encoding.name(),
            doc_encoding.name()
        );
        Ok(())
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        #[test]
        fn property_1_engine_matches_encoding_through_lifecycle(
            ops in prop::collection::vec(op_strategy(), 1..12),
        ) {
            // Each test case gets its own scratch directory so on-disk
            // operations don't collide across proptest shrinks.
            let dir = fresh_test_dir("property_1");

            // Initial document is the default UTF-8 buffer.
            let mut doc = Document::new();
            assert_engine_matches_encoding(&doc, "Document::new()")?;

            for (idx, op) in ops.iter().enumerate() {
                match op {
                    DispatchOp::New => {
                        doc = Document::new();
                        assert_engine_matches_encoding(
                            &doc,
                            &format!("step {idx}: Document::new"),
                        )?;
                    }
                    DispatchOp::OpenWithEncoding(encoding) => {
                        let path = dir.join(format!("open-{idx}.bin"));
                        // Pure ASCII bytes round-trip through any Class A
                        // encoding and through UTF-8 unchanged, so the open
                        // path stays inside the encoding contract under
                        // test.
                        std::fs::write(&path, b"hello world\nsecond line\n")
                            .expect("write fixture");
                        match Document::open_with_encoding(&path, *encoding) {
                            Ok(opened) => {
                                doc = opened;
                                assert_engine_matches_encoding(
                                    &doc,
                                    &format!(
                                        "step {idx}: open_with_encoding({})",
                                        encoding.name()
                                    ),
                                )?;
                            }
                            // Some open paths may legitimately reject
                            // certain sizes / encodings. Such errors are
                            // acceptable for this property: they don't
                            // change `doc`, and `doc` was already verified
                            // to satisfy the invariant before this
                            // iteration began.
                            Err(_) => continue,
                        }
                    }
                    DispatchOp::OpenWithOptions(encoding) => {
                        let path = dir.join(format!("open-opts-{idx}.bin"));
                        std::fs::write(&path, b"abc\r\ndef\r\n")
                            .expect("write fixture");
                        let options =
                            DocumentOpenOptions::new().with_reinterpretation(*encoding);
                        match Document::open_with_options(&path, options) {
                            Ok(opened) => {
                                doc = opened;
                                assert_engine_matches_encoding(
                                    &doc,
                                    &format!(
                                        "step {idx}: open_with_options({})",
                                        encoding.name()
                                    ),
                                )?;
                            }
                            Err(_) => continue,
                        }
                    }
                    DispatchOp::SaveWithEncoding(encoding) => {
                        let path = dir.join(format!("save-{idx}.bin"));
                        let options =
                            DocumentSaveOptions::new().with_encoding(*encoding);
                        // Save may legitimately fail (e.g. unrepresentable
                        // text in the target encoding for an in-memory doc
                        // that already has Cyrillic content). Either way,
                        // the post-condition we care about is on `doc`.
                        match doc.save_to_with_options(&path, options) {
                            Ok(()) => {
                                assert_engine_matches_encoding(
                                    &doc,
                                    &format!(
                                        "step {idx}: save_to_with_options({})",
                                        encoding.name()
                                    ),
                                )?;
                            }
                            Err(_) => {
                                // Failed save must leave the encoding
                                // contract unchanged, so the field-level
                                // invariant still has to hold.
                                assert_engine_matches_encoding(
                                    &doc,
                                    &format!(
                                        "step {idx}: failed save_to_with_options({})",
                                        encoding.name()
                                    ),
                                )?;
                            }
                        }
                    }
                }
            }

            // Best-effort cleanup; on Windows the tempdir may still hold
            // mmap handles from `doc`, so we ignore failures here.
            drop(doc);
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}

// Property 2: SingleByteEngine.step is always 1 byte per text-unit.
//
// This PBT lives inline next to Property 1 (see `prop_dispatch_tests`
// above) rather than under `tests/encoding_engine/` because
// `engine_for_encoding`, the `EncodingEngine` trait, and
// `SingleByteEngine` are all `pub(crate)` — integration tests in
// `tests/` cannot reach them without widening the public API surface.
//
// Property 2 says: for every Class A encoding `e`, every byte slice, and
// every `offset <= end <= bytes.len()`,
// `engine_for_encoding(e).step(bytes, offset, end)` is exactly `1` when
// `offset < end` and `0` otherwise. The byte content is irrelevant: a
// single-byte ASCII superset never decides character length from the
// payload.

#[cfg(test)]
mod prop_step_tests {
    use super::{engine_for_encoding, SingleByteEngine};
    use crate::document::DocumentEncoding;
    use proptest::prelude::*;

    /// Class A labels exercised by Property 2.
    ///
    /// The set matches the task brief (windows-1251, windows-1252, koi8-r,
    /// ibm866, iso-8859-1, iso-8859-15). `iso-8859-1` is the WHATWG
    /// alias that `encoding_rs::Encoding::for_label` resolves to
    /// `windows-1252`; that's still a single-byte ASCII superset, so the
    /// dispatched engine remains a `SingleByteEngine` and the property
    /// holds for the alias just as it does for the canonical label.
    fn class_a_label_strategy() -> impl Strategy<Value = &'static str> {
        let labels: Vec<&'static str> = vec![
            "windows-1251",
            "windows-1252",
            "koi8-r",
            "ibm866",
            "iso-8859-1",
            "iso-8859-15",
        ];
        prop::sample::select(labels)
    }

    /// Generates `(bytes, offset, end)` triples satisfying
    /// `offset <= end <= bytes.len()` over arbitrary byte content.
    ///
    /// The generator picks two independent indices in `0..=bytes.len()`
    /// and sorts them so both endpoints are always inside the slice.
    /// `bytes` is bounded to 256 elements: large enough to exercise the
    /// `offset == end` and `offset < end` branches, small enough to keep
    /// shrinking fast.
    fn step_input_strategy() -> impl Strategy<Value = (Vec<u8>, usize, usize)> {
        prop::collection::vec(any::<u8>(), 0..=256)
            .prop_flat_map(|bytes| {
                let len = bytes.len();
                (Just(bytes), 0..=len, 0..=len)
            })
            .prop_map(|(bytes, a, b)| {
                let (offset, end) = if a <= b { (a, b) } else { (b, a) };
                (bytes, offset, end)
            })
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        #[test]
        fn property_2_single_byte_engine_step_is_one_or_zero(
            label in class_a_label_strategy(),
            (bytes, offset, end) in step_input_strategy(),
        ) {
            let encoding = DocumentEncoding::from_label(label)
                .unwrap_or_else(|| panic!("encoding_rs should know {label}"));

            // Sanity: the dispatched engine must be a SingleByteEngine for
            // every Class A label in this generator. If the label aliases
            // to a different canonical name (e.g. iso-8859-1 → windows-1252),
            // SingleByteEngine::supports must still claim it; otherwise
            // engine_for_encoding would silently fall back to UTF8_ENGINE
            // and the property below would be trivially true for the wrong
            // reason.
            prop_assert!(
                SingleByteEngine::supports(encoding),
                "label {label} (canonical {}) must route to SingleByteEngine",
                encoding.name(),
            );

            let engine = engine_for_encoding(encoding);
            let step = engine.step(&bytes, offset, end);

            if offset < end {
                prop_assert_eq!(
                    step, 1,
                    "step must be 1 when offset < end (encoding {}, offset {}, end {}, len {})",
                    encoding.name(), offset, end, bytes.len(),
                );
            } else {
                prop_assert_eq!(
                    step, 0,
                    "step must be 0 when offset == end (encoding {}, offset {}, end {}, len {})",
                    encoding.name(), offset, end, bytes.len(),
                );
            }
        }
    }
}

// Property 4 lives at `tests/encoding_engine/prop_columns.rs` so it is
// co-located with Property 3's `prop_newline.rs` and shares the same
// hidden `qem::document::__test_support` re-export surface. Keeping it
// out of this file avoids duplicating the same property in two places
// once the integration test lands.
