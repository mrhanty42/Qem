// Property 6: non-UTF-8 documents never build a UTF-8 rope.
//
//
// For any Class A encoding `e` (windows-1251, windows-1252, KOI8-R
// IBM866, ISO-8859-1/15, ...) and any byte sequence written to a temp
// file, opening the file through `Document::open_with_encoding(e)` and
// driving an arbitrary sequence of public *read-only* operations
// against the resulting document must never materialise a full UTF-8
// rope. The property is checked across the whole document lifecycle:
// `doc.has_rope() == false` immediately after open, after each
// individual operation, and after the operation sequence completes.
//
// Read-only operations exercised here cover the public API surface
// promised by the encoding-aware engine spec for Class A documents:
//
// * navigation: `line_count`, `display_line_count`, `file_len`
// * line / range reads: `line_slice`, `read_text`, `text_lossy`
// * literal search: `find_next`, `find_prev`
// * regex search: `find_next_regex`, `find_prev_regex`
// * viewport reads: `read_viewport`.
//
// Edit operations are explicitly out of scope: they are owned by Phase
// 8 where insertions for non-UTF-8 documents are routed through
// the piece-table without UTF-8 transcoding. Any read-only call that
// would silently promote a non-UTF-8 document to a full UTF-8 rope on
// today's code path is a correctness bug under , and
// this property is the harness that catches it.
//
// The cases count is intentionally pinned at 64 for this spec.

#[path = "mod.rs"]
mod helpers;

use helpers::fresh_test_dir;
use proptest::prelude::*;
use qem::{Document, DocumentEncoding, RegexSearchQuery, TextPosition, TextRange, ViewportRequest};
use std::path::{Path, PathBuf};

/// Class A encodings the spec wires through `SingleByteEngine`.
/// The labels match `encoding_rs` exactly so `DocumentEncoding::from_label`
/// always succeeds. Class B (UTF-16 LE/BE, Shift_JIS, GB18030, EUC-KR)
/// will be added once Phases 6 and 7 land their native engines and
/// open paths.
const CLASS_A_LABELS: &[&str] = &[
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

fn class_a_encoding_strategy() -> impl Strategy<Value = DocumentEncoding> {
    prop::sample::select(CLASS_A_LABELS.to_vec()).prop_map(|label| {
        DocumentEncoding::from_label(label)
            .unwrap_or_else(|| panic!("encoding_rs should know {label}"))
    })
}

/// One byte token for the document corpus. Mixing plain bytes (high-byte
/// glyphs of Class A encodings plus printable ASCII) with explicit line
/// terminators (`LF`, `CR`, `CRLF`) gives the open path realistic line
/// indexing work and the search/regex operations realistic targets to
/// match against. The total slice size is bounded so each proptest case
/// stays cheap.
#[derive(Debug, Clone, Copy)]
enum Atom {
    Lf,
    Cr,
    Crlf,
    Filler(u8),
}

fn atom_strategy() -> impl Strategy<Value = Atom> {
    prop_oneof![
        2 => Just(Atom::Lf),
        2 => Just(Atom::Cr),
        2 => Just(Atom::Crlf),
 // High-byte glyph range used by every Class A encoding.
        3 => (0xC0u8..=0xFE).prop_map(Atom::Filler),
 // Printable ASCII; excludes 0x0A / 0x0D by construction.
        3 => (b' '..=b'~').prop_map(Atom::Filler),
    ]
}

fn bytes_strategy() -> impl Strategy<Value = Vec<u8>> {
 // 0..=64 atoms turns into byte slices up to ~128 bytes. Large
 // enough to span multiple lines and exercise the line-offsets
 // index without inflating per-case cost.
    prop::collection::vec(atom_strategy(), 0..=64).prop_map(|atoms| {
        let mut bytes = Vec::with_capacity(128);
        for atom in atoms {
            match atom {
                Atom::Lf => bytes.push(b'\n'),
                Atom::Cr => bytes.push(b'\r'),
                Atom::Crlf => {
                    bytes.push(b'\r');
                    bytes.push(b'\n');
                }
                Atom::Filler(b) => bytes.push(b),
            }
        }
        bytes
    })
}

/// One read-only public operation. Each variant carries a deterministic
/// seed so proptest can shrink it independently of the corpus.
#[derive(Debug, Clone)]
enum Op {
    LineCount,
    DisplayLineCount,
    FileLen,
    TextLossy,
    LineSlice {
        line0_seed: u32,
        start_col_seed: u32,
        max_cols_seed: u32,
    },
    ReadText {
        start_line0_seed: u32,
        start_col_seed: u32,
        len_chars_seed: u32,
    },
    Viewport {
        first_line0_seed: u32,
        line_count_seed: u32,
        start_col_seed: u32,
        max_cols_seed: u32,
    },
    FindNext {
        needle: Vec<u8>,
        from_line0_seed: u32,
        from_col_seed: u32,
    },
    FindPrev {
        needle: Vec<u8>,
        before_line0_seed: u32,
        before_col_seed: u32,
    },
    FindNextRegex {
        pattern: String,
        from_line0_seed: u32,
        from_col_seed: u32,
    },
    FindPrevRegex {
        pattern: String,
        before_line0_seed: u32,
        before_col_seed: u32,
    },
}

/// Generates a literal-search needle. Needles are bounded to 8 bytes
/// and biased toward bytes that actually appear in the corpus
/// (high-byte glyphs and printable ASCII) so the operation has a real
/// chance of finding a match. Empty needles are also allowed: the
/// public API documents that they short-circuit to `None`, and the
/// property is about backing state regardless of the result.
fn needle_strategy() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(
        prop_oneof![
            3 => 0xC0u8..=0xFE,
            3 => b' '..=b'~',
            1 => Just(b'\n'),
        ],
        0..=8,
    )
}

/// Generates a regex pattern. The pattern set is intentionally tiny and
/// always valid: Property 6 cares about backing state, not regex
/// behaviour, and a compile error is a perfectly acceptable outcome
/// (the property holds vacuously: a query that never runs cannot
/// promote a rope).
fn regex_pattern_strategy() -> impl Strategy<Value = String> {
    prop_oneof![
        Just(r"[A-Za-z]+".to_string()),
        Just(r"\d+".to_string()),
        Just(r".".to_string()),
        Just(r"^.{0,4}".to_string()),
        Just(r"[\x{C0}-\x{FE}]+".to_string()),
    ]
}

fn op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        1 => Just(Op::LineCount),
        1 => Just(Op::DisplayLineCount),
        1 => Just(Op::FileLen),
        1 => Just(Op::TextLossy),
        2 => (any::<u32>(), any::<u32>(), any::<u32>()).prop_map(
            |(line0_seed, start_col_seed, max_cols_seed)| Op::LineSlice {
                line0_seed,
                start_col_seed,
                max_cols_seed,
            }
        ),
        2 => (any::<u32>(), any::<u32>(), any::<u32>()).prop_map(
            |(start_line0_seed, start_col_seed, len_chars_seed)| Op::ReadText {
                start_line0_seed,
                start_col_seed,
                len_chars_seed,
            }
        ),
        2 => (any::<u32>(), any::<u32>(), any::<u32>(), any::<u32>()).prop_map(
            |(first_line0_seed, line_count_seed, start_col_seed, max_cols_seed)| {
                Op::Viewport {
                    first_line0_seed,
                    line_count_seed,
                    start_col_seed,
                    max_cols_seed,
                }
            }
        ),
        2 => (needle_strategy(), any::<u32>(), any::<u32>()).prop_map(
            |(needle, from_line0_seed, from_col_seed)| Op::FindNext {
                needle,
                from_line0_seed,
                from_col_seed,
            }
        ),
        2 => (needle_strategy(), any::<u32>(), any::<u32>()).prop_map(
            |(needle, before_line0_seed, before_col_seed)| Op::FindPrev {
                needle,
                before_line0_seed,
                before_col_seed,
            }
        ),
        2 => (regex_pattern_strategy(), any::<u32>(), any::<u32>()).prop_map(
            |(pattern, from_line0_seed, from_col_seed)| Op::FindNextRegex {
                pattern,
                from_line0_seed,
                from_col_seed,
            }
        ),
        2 => (regex_pattern_strategy(), any::<u32>(), any::<u32>()).prop_map(
            |(pattern, before_line0_seed, before_col_seed)| Op::FindPrevRegex {
                pattern,
                before_line0_seed,
                before_col_seed,
            }
        ),
    ]
}

/// Maps a 32-bit seed onto the inclusive range `[0, modulus]`. Returns
/// `0` when `modulus == 0` so callers do not need to special-case the
/// empty document.
fn pick_in_range(seed: u32, modulus: usize) -> usize {
    if modulus == 0 {
        0
    } else {
        (seed as usize) % (modulus + 1)
    }
}

/// Builds a literal-search needle string from raw bytes. Property 6 is
/// about backing state, not search results: the needle does not need
/// to round-trip through the document's encoding for the assertion to
/// be meaningful. We feed the bytes through `String::from_utf8_lossy`
/// so the call never panics on arbitrary high-byte sequences and so
/// integration tests do not need an `encoding_rs` dev-dependency.
fn decode_needle(needle: &[u8], _encoding: DocumentEncoding) -> String {
    String::from_utf8_lossy(needle).into_owned()
}

/// Drives one operation against the document. The return value is
/// ignored: Property 6 only cares about the side effect on backing
/// state.
fn drive_op(doc: &Document, op: &Op) {
    let line_count_hint = doc.line_count().display_rows().max(1);

    match op {
        Op::LineCount => {
            let _ = doc.line_count();
        }
        Op::DisplayLineCount => {
            let _ = doc.display_line_count();
        }
        Op::FileLen => {
            let _ = doc.file_len();
        }
        Op::TextLossy => {
            let _ = doc.text_lossy();
        }
        Op::LineSlice {
            line0_seed,
            start_col_seed,
            max_cols_seed,
        } => {
            let line0 = pick_in_range(*line0_seed, line_count_hint.saturating_sub(1));
            let start_col = pick_in_range(*start_col_seed, 256);
            let max_cols = (*max_cols_seed as usize % 257).max(1);
            let _ = doc.line_slice(line0, start_col, max_cols);
        }
        Op::ReadText {
            start_line0_seed,
            start_col_seed,
            len_chars_seed,
        } => {
            let line0 = pick_in_range(*start_line0_seed, line_count_hint.saturating_sub(1));
            let start_col = pick_in_range(*start_col_seed, 64);
            let len_chars = (*len_chars_seed as usize) % 65;
            let range = TextRange::new(TextPosition::new(line0, start_col), len_chars);
            let _ = doc.read_text(range);
        }
        Op::Viewport {
            first_line0_seed,
            line_count_seed,
            start_col_seed,
            max_cols_seed,
        } => {
            let first_line0 = pick_in_range(*first_line0_seed, line_count_hint.saturating_sub(1));
            let count = (*line_count_seed as usize) % 8 + 1;
            let start_col = (*start_col_seed as usize) % 64;
            let max_cols = (*max_cols_seed as usize) % 128 + 1;
            let request =
                ViewportRequest::new(first_line0, count).with_columns(start_col, max_cols);
            let _ = doc.read_viewport(request);
        }
        Op::FindNext {
            needle,
            from_line0_seed,
            from_col_seed,
        } => {
            let line0 = pick_in_range(*from_line0_seed, line_count_hint.saturating_sub(1));
            let col0 = pick_in_range(*from_col_seed, 64);
            let needle = decode_needle(needle, doc.encoding());
            let _ = doc.find_next(&needle, TextPosition::new(line0, col0));
        }
        Op::FindPrev {
            needle,
            before_line0_seed,
            before_col_seed,
        } => {
            let line0 = pick_in_range(*before_line0_seed, line_count_hint.saturating_sub(1));
            let col0 = pick_in_range(*before_col_seed, 64);
            let needle = decode_needle(needle, doc.encoding());
            let _ = doc.find_prev(&needle, TextPosition::new(line0, col0));
        }
        Op::FindNextRegex {
            pattern,
            from_line0_seed,
            from_col_seed,
        } => {
            let line0 = pick_in_range(*from_line0_seed, line_count_hint.saturating_sub(1));
            let col0 = pick_in_range(*from_col_seed, 64);
 // Compile-error or match miss are both fine for Property 6.
            if let Ok(query) = RegexSearchQuery::new(pattern) {
                let _ = doc.find_next_regex_query(&query, TextPosition::new(line0, col0));
            }
        }
        Op::FindPrevRegex {
            pattern,
            before_line0_seed,
            before_col_seed,
        } => {
            let line0 = pick_in_range(*before_line0_seed, line_count_hint.saturating_sub(1));
            let col0 = pick_in_range(*before_col_seed, 64);
            if let Ok(query) = RegexSearchQuery::new(pattern) {
                let _ = doc.find_prev_regex_query(&query, TextPosition::new(line0, col0));
            }
        }
    }
}

/// Writes `bytes` to `dir/file_name` and returns the full path. The
/// fixture directory itself is created by `fresh_test_dir` before this
/// is called.
fn write_fixture(dir: &Path, file_name: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.join(file_name);
    std::fs::write(&path, bytes).expect("write_fixture: write");
    path
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

 /// Property 6: a non-UTF-8 document opened through
 /// `Document::open_with_encoding` for a Class A encoding never
 /// materialises a UTF-8 rope, regardless of which read-only public
 /// operation is invoked or in what order.
 ///
 /// The test:
 /// 1. Opens a fresh fixture file containing `bytes` under the
 /// randomly chosen Class A encoding.
 /// 2. Asserts `doc.has_rope() == false` immediately after open
 ///.
 /// 3. Drives the generated `ops` sequence one operation at a
 /// time. After every operation, the same `has_rope() == false`
 /// invariant must still hold.
 /// 4. Drops the fixture file and parent dir at the end.
 ///
 /// Any read-only call that silently promotes a Class A document to
 /// a UTF-8 rope is exactly the bug forbids, and this
 /// property is the harness that catches it.
    #[test]
    fn property_6_class_a_documents_never_materialise_a_rope(
        encoding in class_a_encoding_strategy(),
        bytes in bytes_strategy(),
        ops in prop::collection::vec(op_strategy(), 0..=8),
    ) {
        let dir = fresh_test_dir("prop_backing");
        let path = write_fixture(&dir, "fixture.bin", &bytes);

        let doc = Document::open_with_encoding(&path, encoding)
            .expect("Class A open should succeed for arbitrary byte sequences");

        prop_assert_eq!(
            doc.encoding(),
            encoding,
            "open_with_encoding must install the requested encoding contract for {}",
            encoding.name(),
        );
        prop_assert!(
            !doc.has_rope(),
            "open_with_encoding({}, ..) must not materialise a UTF-8 rope; \
             open path violated the no-rope contract (file_len = {})",
            encoding.name(),
            bytes.len(),
        );

        for (idx, op) in ops.iter().enumerate() {
            drive_op(&doc, op);
            prop_assert!(
                !doc.has_rope(),
                "operation #{} on a {} document promoted the backing to a \
                 UTF-8 rope; this violates the no-rope contract for \
                 non-UTF-8 documents \
                 (op = {:?}, file_len = {})",
                idx,
                encoding.name(),
                op,
                bytes.len(),
            );
        }

 // Final invariant after the whole sequence has run. Redundant
 // with the per-step check above when `ops` is non-empty, but
 // also guards the empty-sequence case where the per-step loop
 // runs zero iterations.
        prop_assert!(
            !doc.has_rope(),
            "{} document materialised a UTF-8 rope by the end of the \
             read-only operation sequence (file_len = {})",
            encoding.name(),
            bytes.len(),
        );

 // Best-effort cleanup; missing files in tmp tolerated by design.
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }
}
