//! Integration test binary for the encoding-aware engine.
//!
//! Cargo treats each top-level file in `tests/` as its own integration
//! test crate. Each `#[path = ...] mod foo;` line below pulls one
//! property test or per-encoding contract suite into this single
//! binary.

#[path = "encoding_engine/mod.rs"]
mod helpers;

#[path = "encoding_engine/prop_dispatch.rs"]
mod prop_dispatch;

#[path = "encoding_engine/prop_step.rs"]
mod prop_step;

#[path = "encoding_engine/prop_newline.rs"]
mod prop_newline;

#[path = "encoding_engine/prop_columns.rs"]
mod prop_columns;

#[path = "encoding_engine/prop_backing.rs"]
mod prop_backing;

#[path = "encoding_engine/prop_open_save_roundtrip.rs"]
mod prop_open_save_roundtrip;

#[path = "encoding_engine/prop_endianness.rs"]
mod prop_endianness;

// Per-encoding integration suite for Class A. Each module owns the
// fixed four-test contract for one Class A encoding.

#[path = "encoding_engine/per_encoding/windows_1251.rs"]
mod windows_1251;

#[path = "encoding_engine/per_encoding/windows_1252.rs"]
mod windows_1252;

#[path = "encoding_engine/per_encoding/koi8_r.rs"]
mod koi8_r;

#[path = "encoding_engine/per_encoding/ibm866.rs"]
mod ibm866;

#[path = "encoding_engine/per_encoding/latin1.rs"]
mod latin1;

#[path = "encoding_engine/per_encoding/iso_8859_15.rs"]
mod iso_8859_15;

// Per-encoding integration suite for Class B (UTF-16). Each module
// owns the same fixed four-test contract used by the Class A modules
// above, with UTF-16-specific fixtures that mix ASCII, CJK + Cyrillic
// non-ASCII payloads, LF / CRLF / CR terminators and an empty line.

#[path = "encoding_engine/per_encoding/utf16_le.rs"]
mod utf16_le;

#[path = "encoding_engine/per_encoding/utf16_be.rs"]
mod utf16_be;

// Per-encoding integration suite for Class B (CJK multibyte:
// Shift_JIS, gb18030, EUC-KR). Each module owns the same fixed
// four-test contract used by the Class A and UTF-16 modules above
// with CJK-specific fixtures that mix ASCII, CJK non-ASCII payloads
// LF / CRLF / CR terminators and an empty line.

#[path = "encoding_engine/per_encoding/shift_jis.rs"]
mod shift_jis;

#[path = "encoding_engine/per_encoding/gb18030.rs"]
mod gb18030;

#[path = "encoding_engine/per_encoding/euc_kr.rs"]
mod euc_kr;

// Non-UTF-8 piece-tree preserve-save and save-conversion round-trip
// pinning. Validates that, once a non-UTF-8 document picks up a
// piece-tree edit buffer through the encoded insert path, both
// `save_to` (preserve) and `save_to_with_encoding(UTF-8)` (convert)
// behave correctly.

#[path = "encoding_engine/preserve_save_piece_table.rs"]
mod preserve_save_piece_table;

// Property: insert with an unrepresentable scalar must return
// `Err(UnrepresentableText)` without mutating the document.

#[path = "encoding_engine/prop_insert_unrepresentable.rs"]
mod prop_insert_unrepresentable;

// Property: every boundary offset returned by the encoding engine
// (line starts, char-aligned cursor positions) is a fixed point of
// `Document::align_byte_offset(.., Backward)` after a sequence of
// representable edits over a non-UTF-8 document.

#[path = "encoding_engine/prop_alignment.rs"]
mod prop_alignment;

// Property: save round-trip after representable edits. For
// `e ∈ Class A ∪ Class B`, an initial file in `e`, and a sequence of
// inserts limited to representable scalars
// `open(e) → edits → save → reopen(e)` yields decoded text equal to
// the in-memory decoded text captured just before save.

#[path = "encoding_engine/prop_edit_save_roundtrip.rs"]
mod prop_edit_save_roundtrip;

// Property: insert round-trips through encode / decode. For
// `e ∈ Class A ∪ Class B`, an aligned-offset `try_insert` of a
// representable `&str` produces a byte range in the document whose
// decode via `e` equals the input.

#[path = "encoding_engine/prop_insert_roundtrip.rs"]
mod prop_insert_roundtrip;

// Property: a regex pattern that overflows the reverse-DFA size
// limit must surface through the regex search surface as a typed
// `RegexCompileError` with a non-empty message, never a panic /
// overflow / OOM.

#[path = "encoding_engine/prop_reverse_dfa_overflow.rs"]
mod prop_reverse_dfa_overflow;

// Perf gate — deterministic example-based dense vs sparse regex
// ratio. The reverse-DFA path makes reverse search O(slice) rather
// than O(matches), so dense and sparse fixtures of comparable size
// must finish in comparable wall time.

#[path = "encoding_engine/perf/dense_vs_sparse.rs"]
mod perf_dense_vs_sparse;

// Property: reverse-regex search is symmetric to forward search. For
// any pattern compilable in both forward and reverse DFAs and any
// document (rope, mmap, piece-table), the set of `(start, end)` pairs
// from `find_all_regex` matches the set produced by walking the same
// document end-to-start through `find_prev_regex_query`.

#[path = "encoding_engine/prop_reverse_regex.rs"]
mod prop_reverse_regex;

// Reproducer harness for diagnosing reverse-regex regressions.

#[path = "encoding_engine/prop_reverse_regex_repro.rs"]
mod prop_reverse_regex_repro;
