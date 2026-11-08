use super::lifecycle::OpenProgressPhase;
use super::*;
use crate::index::DiskLineIndex;
use encoding_rs::{GB18030, SHIFT_JIS, WINDOWS_1251};
use proptest::prelude::*;
use std::io::Write;
use tempfile::tempdir;

#[derive(Clone, Debug)]
enum EditOp {
    Insert {
        line_hint: usize,
        col0: usize,
        text: String,
    },
    Replace {
        line_hint: usize,
        col0: usize,
        len_chars: usize,
        text: String,
    },
    Backspace {
        line_hint: usize,
        col0: usize,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ModelLine {
    start: usize,
    end: usize,
    len_chars: usize,
}

fn model_lines(text: &str) -> Vec<ModelLine> {
    let mut lines = Vec::new();
    let mut start = 0usize;
    let mut len_chars = 0usize;

    for (idx, ch) in text.char_indices() {
        if ch == '\n' {
            lines.push(ModelLine {
                start,
                end: idx,
                len_chars,
            });
            start = idx + ch.len_utf8();
            len_chars = 0;
        } else {
            len_chars = len_chars.saturating_add(1);
        }
    }

    lines.push(ModelLine {
        start,
        end: text.len(),
        len_chars,
    });
    lines
}

fn clamp_model_line(text: &str, line_hint: usize) -> usize {
    let lines = model_lines(text);
    line_hint.min(lines.len().saturating_sub(1))
}

fn byte_offset_for_col(text: &str, line: ModelLine, col0: usize) -> usize {
    let clamped = col0.min(line.len_chars);
    if clamped == line.len_chars {
        return line.end;
    }

    let mut chars_seen = 0usize;
    for (offset, _) in text[line.start..line.end].char_indices() {
        if chars_seen == clamped {
            return line.start + offset;
        }
        chars_seen = chars_seen.saturating_add(1);
    }

    line.end
}

fn advance_byte_offset_by_chars(text: &str, start: usize, len_chars: usize) -> usize {
    if len_chars == 0 || start >= text.len() {
        return start.min(text.len());
    }

    let mut chars_seen = 0usize;
    for (offset, _) in text[start..].char_indices() {
        if chars_seen == len_chars {
            return start + offset;
        }
        chars_seen = chars_seen.saturating_add(1);
    }

    text.len()
}

fn visible_segment_for_model_line(
    text: &str,
    line0: usize,
    start_col: usize,
    max_cols: usize,
) -> String {
    if max_cols == 0 {
        return String::new();
    }

    let lines = model_lines(text);
    let Some(line) = lines.get(line0).copied() else {
        return String::new();
    };
    if start_col >= line.len_chars {
        return String::new();
    }

    let start = byte_offset_for_col(text, line, start_col);
    let end = byte_offset_for_col(text, line, start_col.saturating_add(max_cols));
    text[start..end].to_string()
}

fn model_insert(text: &mut String, line_hint: usize, col0: usize, insert: &str) -> (usize, usize) {
    let lines = model_lines(text);
    let line0 = line_hint.min(lines.len().saturating_sub(1));
    if insert.is_empty() {
        return (line0, col0);
    }
    let line = lines[line0];
    let insert_at = byte_offset_for_col(text, line, col0);
    let virtual_padding_cols = col0.saturating_sub(line.len_chars);
    let (normalized, added_lines, last_col) =
        normalize_insert_text(insert, virtual_padding_cols, LineEnding::Lf);
    text.insert_str(insert_at, &normalized);

    if added_lines == 0 {
        (line0, col0.saturating_add(last_col))
    } else {
        (line0.saturating_add(added_lines), last_col)
    }
}

fn model_delete(
    text: &mut String,
    line_hint: usize,
    col0: usize,
    len_chars: usize,
) -> (usize, usize) {
    if len_chars == 0 {
        return (clamp_model_line(text, line_hint), col0);
    }

    let lines = model_lines(text);
    let line0 = line_hint.min(lines.len().saturating_sub(1));
    let line = lines[line0];
    let start_col0 = col0.min(line.len_chars);
    let start = byte_offset_for_col(text, line, start_col0);
    let end = advance_byte_offset_by_chars(text, start, len_chars);
    if end > start {
        text.replace_range(start..end, "");
    }
    (line0, start_col0)
}

fn model_replace(
    text: &mut String,
    line_hint: usize,
    col0: usize,
    len_chars: usize,
    replacement: &str,
) -> (usize, usize) {
    let (line0, col0) = model_delete(text, line_hint, col0, len_chars);
    model_insert(text, line0, col0, replacement)
}

fn model_backspace(text: &mut String, line_hint: usize, col0: usize) -> (bool, usize, usize) {
    if text.is_empty() {
        return (false, 0, col0);
    }

    let lines = model_lines(text);
    let line0 = line_hint.min(lines.len().saturating_sub(1));
    let line = lines[line0];
    if col0 > line.len_chars {
        return (false, line0, col0.saturating_sub(1));
    }

    let cur = byte_offset_for_col(text, line, col0);
    if cur == 0 {
        return (false, line0, col0);
    }

    let (prev_start, prev_ch) = text[..cur]
        .char_indices()
        .last()
        .expect("non-empty prefix must contain a character");
    let previous_line_col0 = if prev_ch == '\n' {
        Some(lines[line0.saturating_sub(1)].len_chars)
    } else {
        None
    };
    text.replace_range(prev_start..cur, "");

    if prev_ch == '\n' {
        let new_line0 = line0.saturating_sub(1);
        let new_col0 = previous_line_col0.unwrap_or(0);
        (true, new_line0, new_col0)
    } else {
        (true, line0, col0.saturating_sub(1))
    }
}

fn assert_doc_matches_model(doc: &Document, expected: &str) {
    let expected_lines = model_lines(expected);
    let expected_text = if doc.has_edit_buffer() {
        expected.to_owned()
    } else {
        render_with_line_ending(expected, doc.line_ending())
    };

    assert_eq!(doc.text_lossy(), expected_text);
    assert_eq!(doc.exact_line_count(), Some(expected_lines.len()));
    assert_eq!(doc.line_count(), LineCount::Exact(expected_lines.len()));
    assert!(doc.has_precise_line_lengths());

    for (line0, line) in expected_lines.iter().copied().enumerate() {
        assert_eq!(
            doc.line_len_chars(line0),
            line.len_chars,
            "line_len_chars mismatch at line {line0}"
        );
        let visible = doc.line_slice(line0, 0, line.len_chars.saturating_add(4));
        assert!(
            visible.is_exact(),
            "line_slice should be exact at line {line0}"
        );
        assert_eq!(
            visible.text(),
            &expected[line.start..line.end],
            "line_slice text mismatch at line {line0}"
        );

        let offset_col = line.len_chars / 2;
        let partial = doc.line_slice(line0, offset_col, 3);
        assert_eq!(
            partial.text(),
            visible_segment_for_model_line(expected, line0, offset_col, 3),
            "partial line_slice mismatch at line {line0}"
        );
    }
}

fn apply_op_to_doc(doc: &mut Document, expected: &mut String, op: &EditOp) {
    match op {
        EditOp::Insert {
            line_hint,
            col0,
            text,
        } => {
            let line0 = clamp_model_line(expected, *line_hint);
            let expected_cursor = model_insert(expected, line0, *col0, text);
            let actual_cursor = doc.try_insert_text_at(line0, *col0, text).unwrap();
            assert_eq!(actual_cursor, expected_cursor);
        }
        EditOp::Replace {
            line_hint,
            col0,
            len_chars,
            text,
        } => {
            let line0 = clamp_model_line(expected, *line_hint);
            let expected_cursor = model_replace(expected, line0, *col0, *len_chars, text);
            let actual_cursor = doc
                .try_replace_range(line0, *col0, *len_chars, text)
                .unwrap();
            assert_eq!(actual_cursor, expected_cursor);
        }
        EditOp::Backspace { line_hint, col0 } => {
            let line0 = clamp_model_line(expected, *line_hint);
            let expected_cursor = model_backspace(expected, line0, *col0);
            let actual_cursor = doc.try_backspace_at(line0, *col0).unwrap();
            assert_eq!(actual_cursor, expected_cursor);
        }
    }

    assert_doc_matches_model(doc, expected);
}

fn apply_op_to_doc_text_only(doc: &mut Document, expected: &mut String, op: &EditOp) {
    match op {
        EditOp::Insert {
            line_hint,
            col0,
            text,
        } => {
            let line0 = clamp_model_line(expected, *line_hint);
            let expected_cursor = model_insert(expected, line0, *col0, text);
            let actual_cursor = doc.try_insert_text_at(line0, *col0, text).unwrap();
            assert_eq!(actual_cursor, expected_cursor);
        }
        EditOp::Replace {
            line_hint,
            col0,
            len_chars,
            text,
        } => {
            let line0 = clamp_model_line(expected, *line_hint);
            let expected_cursor = model_replace(expected, line0, *col0, *len_chars, text);
            let actual_cursor = doc
                .try_replace_range(line0, *col0, *len_chars, text)
                .unwrap();
            assert_eq!(actual_cursor, expected_cursor);
        }
        EditOp::Backspace { line_hint, col0 } => {
            let line0 = clamp_model_line(expected, *line_hint);
            let expected_cursor = model_backspace(expected, line0, *col0);
            let actual_cursor = doc.try_backspace_at(line0, *col0).unwrap();
            assert_eq!(actual_cursor, expected_cursor);
        }
    }

    assert_eq!(doc.text_lossy(), *expected);
}

fn op_text_strategy() -> impl Strategy<Value = String> {
    prop::collection::vec(
        prop_oneof![
            Just('a'),
            Just('b'),
            Just('c'),
            Just('x'),
            Just('y'),
            Just('z'),
            Just(' '),
            Just('\n'),
            Just('\r'),
            Just('\u{00E9}'),
            Just('\u{4E2D}'),
            Just('\u{1F642}'),
        ],
        0..8,
    )
    .prop_map(|chars| chars.into_iter().collect())
}

fn non_empty_op_text_strategy() -> impl Strategy<Value = String> {
    prop::collection::vec(
        prop_oneof![
            Just('a'),
            Just('b'),
            Just('c'),
            Just('x'),
            Just('y'),
            Just('z'),
            Just(' '),
            Just('\n'),
            Just('\r'),
            Just('\u{00E9}'),
            Just('\u{4E2D}'),
            Just('\u{1F642}'),
        ],
        1..6,
    )
    .prop_map(|chars| chars.into_iter().collect())
}

fn edit_op_strategy() -> impl Strategy<Value = EditOp> {
    prop_oneof![
        (0usize..12, 0usize..24, op_text_strategy()).prop_map(|(line_hint, col0, text)| {
            EditOp::Insert {
                line_hint,
                col0,
                text,
            }
        }),
        (0usize..12, 0usize..24, 0usize..12, op_text_strategy()).prop_map(
            |(line_hint, col0, len_chars, text)| {
                EditOp::Replace {
                    line_hint,
                    col0,
                    len_chars,
                    text,
                }
            },
        ),
        (0usize..12, 0usize..24)
            .prop_map(|(line_hint, col0)| EditOp::Backspace { line_hint, col0 }),
    ]
}

fn file_backed_edit_op_strategy() -> impl Strategy<Value = EditOp> {
    prop_oneof![
        (0usize..64, 0usize..12, non_empty_op_text_strategy()).prop_map(
            |(line_hint, col0, text)| EditOp::Insert {
                line_hint,
                col0,
                text,
            },
        ),
        (
            0usize..64,
            0usize..12,
            0usize..8,
            non_empty_op_text_strategy(),
        )
            .prop_map(|(line_hint, col0, len_chars, text)| EditOp::Replace {
                line_hint,
                col0,
                len_chars,
                text,
            }),
        (0usize..64, 0usize..12)
            .prop_map(|(line_hint, col0)| EditOp::Backspace { line_hint, col0 }),
    ]
}

fn render_with_line_ending(text: &str, line_ending: LineEnding) -> String {
    match line_ending {
        LineEnding::Lf => text.to_owned(),
        LineEnding::Crlf => text.replace('\n', "\r\n"),
        LineEnding::Cr => text.replace('\n', "\r"),
    }
}

fn write_disk_backed_fixture(path: &Path) {
    let mut file = std::fs::File::create(path).unwrap();
    let chunk = b"abc\ndef\n".repeat(1024);
    let target_len = PIECE_TREE_DISK_MIN_BYTES + chunk.len();
    let mut written = 0usize;
    while written < target_len {
        file.write_all(&chunk).unwrap();
        written = written.saturating_add(chunk.len());
    }
    file.flush().unwrap();
}

#[test]
fn line_lengths_from_bytes_preserves_newline_bytes() {
    let lengths = line_lengths_from_bytes(b"a\r\nbb\n", 16).unwrap();
    assert_eq!(lengths, vec![3, 3, 0]);
}

#[test]
fn line_lengths_from_bytes_respects_limit() {
    assert!(line_lengths_from_bytes(b"a\nb\nc\n", 2).is_none());
}

#[test]
fn precise_piece_table_line_lengths_require_complete_index() {
    let doc = Document {
        path: None,
        storage: None,
        line_offsets: Arc::new(RwLock::new(LineOffsets::U32(vec![0, 4, 9]))),
        disk_index: None,
        indexing: Arc::new(AtomicBool::new(true)),
        indexing_started: None,
        file_len: 9,
        indexed_bytes: Arc::new(AtomicUsize::new(4)),
        avg_line_len: Arc::new(AtomicUsize::new(AVG_LINE_LEN_ESTIMATE)),
        line_ending: LineEnding::Lf,
        encoding: DocumentEncoding::utf8(),
        encoding_origin: DocumentEncodingOrigin::NewDocument,
        encoding_engine: super::encoding_engine::engine_for_encoding(DocumentEncoding::utf8()),
        decoding_had_errors: false,
        preserve_save_error_cache: Cell::new(None),
        rope_file_len_cache: Cell::new(None),
        rope: None,
        rope_history: RopeHistory::new(),
        piece_table: None,
        dirty: false,
    };

    assert!(doc.precise_piece_table_line_lengths(false).is_none());
}

#[test]
fn precise_piece_table_line_lengths_reject_large_line_arrays() {
    let mut offsets = Vec::with_capacity(LINE_LENGTHS_MAX_SYNC_LINES + 1);
    for i in 0..=LINE_LENGTHS_MAX_SYNC_LINES {
        offsets.push(i as u32);
    }

    let doc = Document {
        path: None,
        storage: None,
        line_offsets: Arc::new(RwLock::new(LineOffsets::U32(offsets))),
        disk_index: None,
        indexing: Arc::new(AtomicBool::new(false)),
        indexing_started: None,
        file_len: LINE_LENGTHS_MAX_SYNC_LINES + 1,
        indexed_bytes: Arc::new(AtomicUsize::new(LINE_LENGTHS_MAX_SYNC_LINES + 1)),
        avg_line_len: Arc::new(AtomicUsize::new(AVG_LINE_LEN_ESTIMATE)),
        line_ending: LineEnding::Lf,
        encoding: DocumentEncoding::utf8(),
        encoding_origin: DocumentEncodingOrigin::NewDocument,
        encoding_engine: super::encoding_engine::engine_for_encoding(DocumentEncoding::utf8()),
        decoding_had_errors: false,
        preserve_save_error_cache: Cell::new(None),
        rope_file_len_cache: Cell::new(None),
        rope: None,
        rope_history: RopeHistory::new(),
        piece_table: None,
        dirty: false,
    };

    assert!(doc.precise_piece_table_line_lengths(true).is_none());
}

#[test]
fn piece_table_line_lengths_for_edit_builds_partial_prefix() {
    let dir = std::env::temp_dir().join(format!(
        "standpad-doc-partial-prefix-{}",
        std::process::id()
    ));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("partial.txt");
    std::fs::write(&path, b"alpha\nbeta\ngamma\n").unwrap();
    let storage = FileStorage::open(&path).unwrap();

    let doc = Document {
        path: Some(path.clone()),
        storage: Some(storage),
        line_offsets: Arc::new(RwLock::new(LineOffsets::default())),
        disk_index: None,
        indexing: Arc::new(AtomicBool::new(false)),
        indexing_started: None,
        file_len: 17,
        indexed_bytes: Arc::new(AtomicUsize::new(0)),
        avg_line_len: Arc::new(AtomicUsize::new(AVG_LINE_LEN_ESTIMATE)),
        line_ending: LineEnding::Lf,
        encoding: DocumentEncoding::utf8(),
        encoding_origin: DocumentEncodingOrigin::NewDocument,
        encoding_engine: super::encoding_engine::engine_for_encoding(DocumentEncoding::utf8()),
        decoding_had_errors: false,
        preserve_save_error_cache: Cell::new(None),
        rope_file_len_cache: Cell::new(None),
        rope: None,
        rope_history: RopeHistory::new(),
        piece_table: None,
        dirty: false,
    };

    let (line_lengths, full_index) = doc.piece_table_line_lengths_for_edit(0).unwrap();
    assert!(full_index);
    assert_eq!(line_lengths, vec![6, 5, 6, 0]);

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn piece_table_backspace_in_virtual_space_only_moves_cursor() {
    let dir = std::env::temp_dir().join(format!("standpad-doc-backspace-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("virtual.txt");
    std::fs::write(&path, b"abc\n").unwrap();
    let storage = FileStorage::open(&path).unwrap();
    let mut piece_table = PieceTable::new(storage, vec![4, 0], true);

    let before = piece_table.read_range(0, piece_table.total_len());
    let (edited, line0, col0) = piece_table.backspace_at(0, 5).unwrap();
    let after = piece_table.read_range(0, piece_table.total_len());

    assert!(!edited);
    assert_eq!((line0, col0), (0, 4));
    assert_eq!(before, after);

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn piece_table_insert_in_virtual_space_materializes_spaces() {
    let dir =
        std::env::temp_dir().join(format!("standpad-doc-insert-spaces-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("spaces.txt");
    std::fs::write(&path, b"abc\n").unwrap();
    let storage = FileStorage::open(&path).unwrap();
    let mut piece_table = PieceTable::new(storage, vec![4, 0], true);

    let outcome = piece_table
        .insert_text_at(LineEnding::Lf, 0, 6, "Z")
        .unwrap();
    let (line0, col0) = outcome.cursor;

    assert!(outcome.edited);
    assert_eq!((line0, col0), (0, 7));
    assert_eq!(piece_table.to_string_lossy(), "abc   Z\n");

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn piece_table_insert_encoded_bytes_at_appends_raw_bytes_to_add_buffer() {
    // The non-UTF-8 edit path must write the raw encoded bytes
    // verbatim into the piece-tree add buffer without any UTF-8
    // validation or normalization. Here we simulate a Class A
    // (windows-1251) document: the original bytes 0xCF 0xF0 0xE8 are
    // already in the target encoding, and we splice an additional
    // legacy byte (0xC0) into the middle of the document.
    let dir = std::env::temp_dir().join(format!(
        "standpad-doc-insert-encoded-bytes-{}",
        std::process::id()
    ));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("encoded.bin");
    let original: &[u8] = &[0xCF, 0xF0, 0xE8];
    std::fs::write(&path, original).unwrap();
    let storage = FileStorage::open(&path).unwrap();
    let mut piece_table = PieceTable::new(storage, vec![original.len()], true);

    let inserted: &[u8] = &[0xC0];
    let outcome = piece_table.insert_encoded_bytes_at(2, inserted).unwrap();

    assert!(outcome.edited);
    assert_eq!(outcome.cursor, (0, 0));

    // Reading the full byte range must return the spliced byte stream
    // exactly: original[..2] ++ inserted ++ original[2..].
    let read = piece_table.read_range(0, piece_table.total_len());
    assert_eq!(read, vec![0xCF, 0xF0, 0xC0, 0xE8]);
    assert_eq!(piece_table.total_len(), original.len() + inserted.len());

    // Inserting an empty slice must be a no-op and not bump total_len.
    let prev_len = piece_table.total_len();
    let no_op = piece_table.insert_encoded_bytes_at(1, &[]).unwrap();
    assert!(!no_op.edited);
    assert_eq!(piece_table.total_len(), prev_len);

    // Inserting bytes containing a raw 0x0A must update the line counter
    // (we count line breaks by scanning raw 0x0A / 0x0D bytes; this is
    // exact for ASCII-supersets and an accepted approximation for UTF-16).
    let line_count_before = piece_table.line_count();
    let with_lf: &[u8] = &[0xC1, 0x0A, 0xC2];
    let outcome = piece_table
        .insert_encoded_bytes_at(piece_table.total_len(), with_lf)
        .unwrap();
    assert!(outcome.edited);
    assert!(piece_table.line_count() > line_count_before);

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn piece_table_advance_text_units_crosses_chunk_boundaries_exactly_class_a() {
    // The engine-aware text-unit walker reads the piece-tree range in
    // 64 KiB chunks with one context byte. Advancing across an interior
    // chunk boundary must never consume the boundary byte twice: each
    // cell is counted exactly once, so N units from `start` land exactly
    // N cells forward.
    let dir = std::env::temp_dir().join(format!(
        "standpad-doc-advance-class-a-{}",
        std::process::id()
    ));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("advance-class-a.bin");
    std::fs::write(&path, vec![b'a'; 200_000]).unwrap();
    let storage = FileStorage::open(&path).unwrap();
    let piece_table = PieceTable::new(storage, vec![200_000], true);
    let engine = super::encoding_engine::engine_for_encoding(
        DocumentEncoding::from_label("windows-1251").unwrap(),
    );

    // 100_000 single-byte cells cross the 65_537-byte first chunk edge.
    assert_eq!(
        piece_table.advance_offset_by_text_units_with_engine(0, 100_000, engine),
        100_000
    );
    // ... and ten cells across the boundary land ten cells forward.
    assert_eq!(
        piece_table.advance_offset_by_text_units_with_engine(65_530, 10, engine),
        65_540
    );

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn piece_table_advance_text_units_crosses_chunk_boundaries_exactly_utf16le() {
    // UTF-16 code units are 2 bytes, so a 65_537-byte chunk always ends
    // with a lone trailing byte. The walker must carry at the first
    // unconsumed byte instead of stepping back into the previous cell:
    // a step-back re-decodes the tail from an odd offset and every
    // subsequent cell shifts by one byte.
    let dir =
        std::env::temp_dir().join(format!("standpad-doc-advance-utf16-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("advance-utf16.bin");
    // 40_000 hiragana `あ` cells = 80_000 bytes; the first chunk edge at
    // byte 65_537 splits a code unit.
    let mut bytes = Vec::with_capacity(80_000);
    for _ in 0..40_000 {
        bytes.extend_from_slice(&0x3042u16.to_le_bytes());
    }
    std::fs::write(&path, &bytes).unwrap();
    let storage = FileStorage::open(&path).unwrap();
    let piece_table = PieceTable::new(storage, vec![80_000], true);
    let engine = super::encoding_engine::engine_for_encoding(DocumentEncoding::utf16le());

    assert_eq!(
        piece_table.advance_offset_by_text_units_with_engine(0, 40_000, engine),
        80_000
    );
    assert_eq!(
        piece_table.advance_offset_by_text_units_with_engine(0, 32_769, engine),
        65_538
    );

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn piece_table_advance_text_units_does_not_split_cjk_cell_at_chunk_edge() {
    // Shift_JIS `あ` is two bytes (0x82 0xA0). Placing its lead byte on
    // the last bytes of a chunk means the engine falls back to a 1-byte
    // step for the truncated sequence; the walker must not let that
    // fallback split the cell across the chunk boundary.
    let dir = std::env::temp_dir().join(format!(
        "standpad-doc-advance-shift-jis-{}",
        std::process::id()
    ));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("advance-shift-jis.bin");
    let mut bytes = vec![b'a'; 65_536];
    bytes.extend_from_slice(&[0x82, 0xA0]); // lead byte sits at byte 65_536
    bytes.push(b'b');
    std::fs::write(&path, &bytes).unwrap();
    let storage = FileStorage::open(&path).unwrap();
    let piece_table = PieceTable::new(storage, vec![bytes.len()], true);
    let engine = super::encoding_engine::engine_for_encoding(
        DocumentEncoding::from_label("Shift_JIS").unwrap(),
    );

    // 65_537 cells = 65_536 `a` cells + the two-byte `あ` cell; the
    // result must land after the whole cell, not on its lead byte.
    assert_eq!(
        piece_table.advance_offset_by_text_units_with_engine(0, 65_537, engine),
        65_538
    );

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn piece_table_advance_text_units_keeps_collapse_across_chunk_edges() {
    // The one-byte chunk context exists so a CRLF split across the
    // boundary still collapses into one cell — both when the LF is
    // inside the chunk and when the walk runs out of units exactly on
    // the CR half and the LF lives in the next chunk.
    let dir =
        std::env::temp_dir().join(format!("standpad-doc-advance-crlf-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("advance-crlf.bin");
    let mut bytes = vec![b'a'; 65_536];
    bytes.extend_from_slice(b"\r\nb");
    std::fs::write(&path, &bytes).unwrap();
    let storage = FileStorage::open(&path).unwrap();
    let piece_table = PieceTable::new(storage, vec![bytes.len()], true);
    let engine = super::encoding_engine::engine_for_encoding(
        DocumentEncoding::from_label("windows-1251").unwrap(),
    );

    // Running out of units exactly on the CR half must still land
    // behind the collapsed LF.
    assert_eq!(
        piece_table.advance_offset_by_text_units_with_engine(0, 65_537, engine),
        65_538
    );
    // Same collapse with units to spare.
    assert_eq!(
        piece_table.advance_offset_by_text_units_with_engine(0, 65_540, engine),
        65_539
    );

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn piece_table_read_range_clamps_hostile_piece_bounds() {
    // Piece bounds come from the sidecar's piece pages. A page whose
    // CRC still holds but whose piece advertises a length past the real
    // source must not panic the reader with a slice out of range:
    // read_range clamps segments to the actual source length (byte_at
    // already used the defensive `get` form).
    let dir =
        std::env::temp_dir().join(format!("standpad-doc-hostile-piece-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("hostile.bin");
    std::fs::write(&path, b"abcdef\n").unwrap();
    let storage = FileStorage::open(&path).unwrap();
    let mut piece_table = PieceTable::new(storage, vec![7], true);

    // Simulate a parsed hostile page: the tree's only piece claims 100
    // bytes from a 7-byte source, with matching root totals.
    let hostile = Piece {
        src: PieceSource::Original,
        start: 0,
        len: 100,
        line_breaks: 1,
    };
    piece_table.pieces = PieceTree::from_pieces(vec![hostile]);
    piece_table.total_len = piece_table.pieces.total_len();

    let read = piece_table.read_range(0, piece_table.total_len);
    assert_eq!(
        read,
        b"abcdef\n".to_vec(),
        "a hostile piece must be clamped to the real source bytes, not panic"
    );

    // The same hostile tree reached through the other piece-bound
    // consumers (cursor mapping, char-index scan, the naive UTF-8
    // walker) must also clamp instead of panicking.
    let _ = piece_table.position_for_char_index(50);
    let _ = piece_table.advance_offset_by_text_units(0, 50);

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn piece_table_coalesce_survives_hostile_piece_totals() {
    // Hostile page payloads can carry piece.start near usize::MAX; the
    // coalesce pass computes `last.start + last.len` on plain adds, so a
    // forged leaf panicked the debug build (and wrapped in release,
    // merging unrelated pieces) on any edit touching that leaf.
    let dir = std::env::temp_dir().join(format!(
        "standpad-doc-coalesce-hostile-{}",
        std::process::id()
    ));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("coalesce-hostile.bin");
    std::fs::write(&path, b"abcdef\n").unwrap();
    let storage = FileStorage::open(&path).unwrap();
    let mut piece_table = PieceTable::new(storage, vec![7], true);

    piece_table.pieces = PieceTree::from_pieces(vec![
        Piece {
            src: PieceSource::Original,
            start: usize::MAX,
            len: 1,
            line_breaks: 0,
        },
        Piece {
            src: PieceSource::Original,
            start: 0,
            len: 7,
            line_breaks: 1,
        },
    ]);
    piece_table.total_len = piece_table.pieces.total_len();

    // The insert lands at offset 0, making the hostile piece the
    // coalesce pass's `last` against the real source piece.
    piece_table.insert_bytes(0, b"X").unwrap();
    assert_eq!(
        piece_table.read_range(0, piece_table.total_len),
        b"Xabcdef\n".to_vec(),
        "the hostile piece must be clamped away, not panic the merge"
    );

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn piece_table_line_lengths_for_edit_supports_large_exact_prefix() {
    let dir =
        std::env::temp_dir().join(format!("standpad-doc-large-prefix-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("large-prefix.bin");
    std::fs::write(&path, vec![b'x'; LINE_LENGTHS_MAX_SYNC_LINES + 1]).unwrap();
    let storage = FileStorage::open(&path).unwrap();

    let mut offsets = Vec::with_capacity(LINE_LENGTHS_MAX_SYNC_LINES + 1);
    for i in 0..=LINE_LENGTHS_MAX_SYNC_LINES {
        offsets.push(i as u32);
    }
    let doc = Document {
        path: Some(path.clone()),
        storage: Some(storage),
        line_offsets: Arc::new(RwLock::new(LineOffsets::U32(offsets))),
        disk_index: None,
        indexing: Arc::new(AtomicBool::new(false)),
        indexing_started: None,
        file_len: LINE_LENGTHS_MAX_SYNC_LINES + 1,
        indexed_bytes: Arc::new(AtomicUsize::new(LINE_LENGTHS_MAX_SYNC_LINES + 1)),
        avg_line_len: Arc::new(AtomicUsize::new(AVG_LINE_LEN_ESTIMATE)),
        line_ending: LineEnding::Lf,
        encoding: DocumentEncoding::utf8(),
        encoding_origin: DocumentEncodingOrigin::NewDocument,
        encoding_engine: super::encoding_engine::engine_for_encoding(DocumentEncoding::utf8()),
        decoding_had_errors: false,
        preserve_save_error_cache: Cell::new(None),
        rope_file_len_cache: Cell::new(None),
        rope: None,
        rope_history: RopeHistory::new(),
        piece_table: None,
        dirty: false,
    };

    let (line_lengths, full_index) = doc.piece_table_line_lengths_for_edit(150_000).unwrap();

    assert!(!full_index);
    assert_eq!(line_lengths.len(), 150_001);
    assert!(line_lengths.iter().all(|len| *len == 1));

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn piece_table_line_lengths_for_edit_full_index_beyond_legacy_cap() {
    let dir = std::env::temp_dir().join(format!(
        "standpad-doc-full-beyond-cap-{}",
        std::process::id()
    ));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("beyond-cap.bin");
    let lines = 5_000_000usize;
    std::fs::write(&path, vec![b'x'; lines + 1]).unwrap();
    let storage = FileStorage::open(&path).unwrap();

    let mut offsets = Vec::with_capacity(lines + 1);
    for i in 0..=lines {
        offsets.push(i as u32);
    }
    let doc = Document {
        path: Some(path.clone()),
        storage: Some(storage),
        line_offsets: Arc::new(RwLock::new(LineOffsets::U32(offsets))),
        disk_index: None,
        indexing: Arc::new(AtomicBool::new(false)),
        indexing_started: None,
        file_len: lines + 1,
        indexed_bytes: Arc::new(AtomicUsize::new(lines + 1)),
        avg_line_len: Arc::new(AtomicUsize::new(AVG_LINE_LEN_ESTIMATE)),
        line_ending: LineEnding::Lf,
        encoding: DocumentEncoding::utf8(),
        encoding_origin: DocumentEncodingOrigin::NewDocument,
        encoding_engine: super::encoding_engine::engine_for_encoding(DocumentEncoding::utf8()),
        decoding_had_errors: false,
        preserve_save_error_cache: Cell::new(None),
        rope_file_len_cache: Cell::new(None),
        rope: None,
        rope_history: RopeHistory::new(),
        piece_table: None,
        dirty: false,
    };

    // 5M lines exceeds the legacy 4M fixed cap but fits the byte budget, so
    // the document still gets a fully indexed edit buffer.
    let (line_lengths, full_index) = doc.piece_table_line_lengths_for_edit(lines).unwrap();
    assert!(full_index);
    assert_eq!(line_lengths.len(), lines + 1);
    assert!(line_lengths.iter().all(|len| *len == 1));

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn edit_beyond_window_reports_bounded_edit_window_error() {
    let dir = std::env::temp_dir().join(format!("standpad-doc-edit-window-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("window.txt");
    std::fs::write(&path, b"alpha\nbeta\ngamma\n").unwrap();
    let storage = FileStorage::open(&path).unwrap();

    let mut doc = Document {
        path: Some(path.clone()),
        storage: Some(storage),
        line_offsets: Arc::new(RwLock::new(LineOffsets::default())),
        disk_index: None,
        indexing: Arc::new(AtomicBool::new(false)),
        indexing_started: None,
        // The declared length makes the document rope-unmaterializable, so a
        // failed window seed must surface the bounded-window error, not the
        // rope materialization error.
        file_len: MAX_ROPE_EDIT_FILE_BYTES + 1,
        indexed_bytes: Arc::new(AtomicUsize::new(0)),
        avg_line_len: Arc::new(AtomicUsize::new(AVG_LINE_LEN_ESTIMATE)),
        line_ending: LineEnding::Lf,
        encoding: DocumentEncoding::utf8(),
        encoding_origin: DocumentEncodingOrigin::NewDocument,
        encoding_engine: super::encoding_engine::engine_for_encoding(DocumentEncoding::utf8()),
        decoding_had_errors: false,
        preserve_save_error_cache: Cell::new(None),
        rope_file_len_cache: Cell::new(None),
        rope: None,
        rope_history: RopeHistory::new(),
        piece_table: None,
        dirty: false,
    };

    let error = doc
        .ensure_edit_buffer_for_line(LINE_LENGTHS_MAX_SYNC_LINES + 10)
        .unwrap_err();
    let message = error.to_string();
    assert!(
        message.contains("bounded edit window"),
        "unexpected error: {message}"
    );

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn insert_uses_partial_piece_table_before_full_index_finishes() {
    let dir = std::env::temp_dir().join(format!(
        "standpad-doc-insert-partial-{}",
        std::process::id()
    ));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("insert.txt");
    let original = b"abc\ndef\n".repeat((PIECE_TABLE_MIN_BYTES / 8) + 1);
    std::fs::write(&path, &original).unwrap();
    let storage = FileStorage::open(&path).unwrap();

    let mut doc = Document {
        path: Some(path.clone()),
        storage: Some(storage),
        line_offsets: Arc::new(RwLock::new(LineOffsets::default())),
        disk_index: None,
        indexing: Arc::new(AtomicBool::new(true)),
        indexing_started: None,
        file_len: original.len(),
        indexed_bytes: Arc::new(AtomicUsize::new(0)),
        avg_line_len: Arc::new(AtomicUsize::new(AVG_LINE_LEN_ESTIMATE)),
        line_ending: LineEnding::Lf,
        encoding: DocumentEncoding::utf8(),
        encoding_origin: DocumentEncodingOrigin::NewDocument,
        encoding_engine: super::encoding_engine::engine_for_encoding(DocumentEncoding::utf8()),
        decoding_had_errors: false,
        preserve_save_error_cache: Cell::new(None),
        rope_file_len_cache: Cell::new(None),
        rope: None,
        rope_history: RopeHistory::new(),
        piece_table: None,
        dirty: false,
    };

    let (line0, col0) = doc.try_insert_text_at(0, 0, "X").unwrap();
    assert_eq!((line0, col0), (0, 1));
    assert!(doc.piece_table.is_some());
    assert!(doc.rope.is_none());
    assert!(doc.text_lossy().starts_with("Xabc\ndef\n"));

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn insert_fully_indexes_medium_unindexed_piece_table_documents() {
    let dir = std::env::temp_dir().join(format!(
        "standpad-doc-insert-full-piece-table-{}",
        std::process::id()
    ));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("insert-full.txt");

    let mut original = String::new();
    for i in 0..300_000usize {
        use std::fmt::Write as _;
        let _ = writeln!(&mut original, "L{i:06}");
    }
    std::fs::write(&path, original).unwrap();
    let storage = FileStorage::open(&path).unwrap();

    let mut doc = Document {
        path: Some(path.clone()),
        storage: Some(storage),
        line_offsets: Arc::new(RwLock::new(LineOffsets::default())),
        disk_index: None,
        indexing: Arc::new(AtomicBool::new(true)),
        indexing_started: None,
        file_len: std::fs::metadata(&path).unwrap().len() as usize,
        indexed_bytes: Arc::new(AtomicUsize::new(0)),
        avg_line_len: Arc::new(AtomicUsize::new(AVG_LINE_LEN_ESTIMATE)),
        line_ending: LineEnding::Lf,
        encoding: DocumentEncoding::utf8(),
        encoding_origin: DocumentEncodingOrigin::NewDocument,
        encoding_engine: super::encoding_engine::engine_for_encoding(DocumentEncoding::utf8()),
        decoding_had_errors: false,
        preserve_save_error_cache: Cell::new(None),
        rope_file_len_cache: Cell::new(None),
        rope: None,
        rope_history: RopeHistory::new(),
        piece_table: None,
        dirty: false,
    };

    let (line0, col0) = doc.try_insert_text_at(0, 0, "TOP\n").unwrap();
    let slice = doc.line_slice(5_000, 0, 16);

    assert_eq!((line0, col0), (1, 0));
    assert!(doc.piece_table.is_some());
    assert!(doc.has_precise_line_lengths());
    assert_eq!(doc.exact_line_count(), Some(300_002));
    assert!(slice.is_exact());
    assert_eq!(slice.text(), "L004999");

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn partial_piece_table_can_report_exact_total_lines_without_precise_line_lengths() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("partial-piece-table-exact-total-lines.txt");
    std::fs::write(&path, b"zero\none\ntwo\nthree").unwrap();
    let storage = FileStorage::open(&path).unwrap();

    let mut doc = Document {
        path: Some(path.clone()),
        storage: Some(storage.clone()),
        line_offsets: Arc::new(RwLock::new(LineOffsets::default())),
        disk_index: None,
        indexing: Arc::new(AtomicBool::new(false)),
        indexing_started: None,
        file_len: std::fs::metadata(&path).unwrap().len() as usize,
        indexed_bytes: Arc::new(AtomicUsize::new(0)),
        avg_line_len: Arc::new(AtomicUsize::new(AVG_LINE_LEN_ESTIMATE)),
        line_ending: LineEnding::Lf,
        encoding: DocumentEncoding::utf8(),
        encoding_origin: DocumentEncodingOrigin::Utf8FastPath,
        encoding_engine: super::encoding_engine::engine_for_encoding(DocumentEncoding::utf8()),
        decoding_had_errors: false,
        preserve_save_error_cache: Cell::new(None),
        rope_file_len_cache: Cell::new(None),
        rope: None,
        rope_history: RopeHistory::new(),
        piece_table: Some(PieceTable::new_with_encoding_state(
            storage,
            vec![5, 4],
            false,
            Some(4),
            DocumentEncodingOrigin::Utf8FastPath,
            false,
        )),
        dirty: false,
    };

    assert_eq!(doc.exact_line_count(), Some(4));
    assert!(doc.is_line_count_exact());
    assert!(!doc.has_precise_line_lengths());

    let cursor = doc.try_insert_text_at(0, 0, "TOP\n").unwrap();

    assert_eq!(cursor, (1, 0));
    assert_eq!(doc.exact_line_count(), Some(5));
    assert_eq!(doc.display_line_count(), 5);
    assert!(doc.is_line_count_exact());
    assert!(!doc.has_precise_line_lengths());
}

#[test]
fn line_slice_reads_exact_text_from_edited_rope() {
    let mut doc = Document::new();
    let _ = doc.try_insert_text_at(0, 0, "hello\nworld").unwrap();

    let slice = doc.line_slice(1, 1, 3);

    assert_eq!(slice.text(), "orl");
    assert!(slice.is_exact());
}

#[test]
fn mmap_line_slice_uses_character_columns_for_multibyte_utf8() {
    let dir = std::env::temp_dir().join(format!("qem-mmap-columns-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("utf8.txt");
    std::fs::write(&path, "A\u{1F600}\u{0411}\n").unwrap();

    let doc = Document::open(&path).unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while doc.is_indexing() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }

    let slice = doc.line_slice(0, 1, 2);

    assert_eq!(slice.text(), "\u{1F600}\u{0411}");
    assert!(slice.is_exact());

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn mmap_line_len_counts_invalid_utf8_bytes() {
    let dir = std::env::temp_dir().join(format!("qem-mmap-invalid-utf8-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("bytes.bin");
    std::fs::write(&path, [0xFF, b'a', b'\n']).unwrap();

    let doc = Document::open(&path).unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while doc.is_indexing() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }

    assert_eq!(doc.line_len_chars(0), 2);

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn empty_document_insert_preserves_utf8_text() {
    let mut doc = Document::new();

    let sample = "\u{1F600}\u{043F}\u{0440}\u{0438}\u{0432}\u{0435}\u{0442}";
    let (line0, col0) = doc.try_insert_text_at(0, 0, sample).unwrap();

    assert_eq!((line0, col0), (0, 7));
    assert_eq!(doc.text_lossy(), sample);
    assert_eq!(doc.line_len_chars(0), 7);
}

#[test]
fn piece_table_insert_at_end_does_not_create_zero_len_pieces() {
    let dir = std::env::temp_dir().join(format!("qem-piece-end-insert-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("end.txt");
    std::fs::write(&path, b"abc").unwrap();
    let storage = FileStorage::open(&path).unwrap();
    let mut piece_table = PieceTable::new(storage, vec![3], true);

    let outcome = piece_table
        .insert_text_at(LineEnding::Lf, 0, 3, "Z")
        .unwrap();
    let (line0, col0) = outcome.cursor;

    assert!(outcome.edited);
    assert_eq!((line0, col0), (0, 4));
    assert_eq!(piece_table.to_string_lossy(), "abcZ");
    assert!(piece_table
        .pieces
        .to_vec()
        .iter()
        .all(|piece| piece.len > 0));

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn piece_table_insert_after_multibyte_char_preserves_utf8_boundaries() {
    let dir = std::env::temp_dir().join(format!("qem-piece-utf8-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("utf8.txt");
    let line = "A\u{1F600}\u{0411}\n";
    let content = line.repeat((PIECE_TABLE_MIN_BYTES / line.len()) + 16);
    std::fs::write(&path, content).unwrap();

    let mut doc = Document::open(path.clone()).unwrap();
    let (line0, col0) = doc.try_insert_text_at(0, 2, "X").unwrap();

    assert_eq!((line0, col0), (0, 3));
    assert!(doc.has_edit_buffer());
    assert!(doc.text_lossy().starts_with("A\u{1F600}X\u{0411}\n"));

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn detected_crlf_style_is_used_for_inserted_newlines() {
    let dir = std::env::temp_dir().join(format!("qem-crlf-style-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let src = dir.join("windows.txt");
    let dst = dir.join("windows-saved.txt");
    let content = b"alpha\r\nbeta\r\n".repeat((PIECE_TABLE_MIN_BYTES / 12) + 16);
    std::fs::write(&src, &content).unwrap();

    let mut doc = Document::open(src.clone()).unwrap();
    let (line0, col0) = doc.try_insert_text_at(0, 5, "\nX").unwrap();
    doc.save_to(&dst).unwrap();

    assert_eq!((line0, col0), (1, 1));
    assert!(std::fs::read(&dst)
        .unwrap()
        .starts_with(b"alpha\r\nX\r\nbeta\r\n"));

    let _ = std::fs::remove_file(&src);
    let _ = std::fs::remove_file(&dst);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn rope_save_preserves_detected_crlf_style() {
    let dir = std::env::temp_dir().join(format!("qem-crlf-rope-style-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let src = dir.join("windows-small.txt");
    let dst = dir.join("windows-small-saved.txt");
    std::fs::write(&src, b"alpha\r\nbeta\r\n").unwrap();

    let mut doc = Document::open(src.clone()).unwrap();
    let (line0, col0) = doc.try_insert_text_at(0, 5, "\nX").unwrap();
    assert!(doc.rope.is_some());
    doc.save_to(&dst).unwrap();

    assert_eq!((line0, col0), (1, 1));
    assert_eq!(std::fs::read(&dst).unwrap(), b"alpha\r\nX\r\nbeta\r\n");

    let _ = std::fs::remove_file(&src);
    let _ = std::fs::remove_file(&dst);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn save_to_failure_preserves_dirty_state_and_current_path() {
    let dir = std::env::temp_dir().join(format!("qem-save-failure-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let src = dir.join("source.txt");
    let blocked_parent = dir.join("not-a-directory");
    let dst = blocked_parent.join("copy.txt");
    std::fs::write(&src, b"alpha\nbeta\n").unwrap();
    std::fs::write(&blocked_parent, b"blocker").unwrap();

    let mut doc = Document::open(src.clone()).unwrap();
    let _ = doc.try_insert_text_at(0, 0, "123").unwrap();

    let err = doc.save_to(&dst).unwrap_err();

    assert!(matches!(err, DocumentError::Write { .. }));
    assert!(doc.is_dirty());
    assert_eq!(doc.path(), Some(src.as_path()));
    assert_eq!(std::fs::read(&src).unwrap(), b"alpha\nbeta\n");
    assert!(!dst.exists());

    let _ = std::fs::remove_file(&src);
    let _ = std::fs::remove_file(&blocked_parent);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn escaped_utf8_insert_preserves_multibyte_text() {
    let mut doc = Document::new();
    let sample = "\u{1F600}\u{043F}\u{0440}\u{0438}\u{0432}\u{0435}\u{0442}";

    let (line0, col0) = doc.try_insert_text_at(0, 0, sample).unwrap();

    assert_eq!((line0, col0), (0, 7));
    assert_eq!(doc.text_lossy(), sample);
    assert_eq!(doc.line_len_chars(0), 7);
}

#[test]
fn piece_table_insert_after_escaped_multibyte_char_preserves_boundaries() {
    let dir = std::env::temp_dir().join(format!("qem-piece-utf8-escaped-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("utf8-escaped.txt");
    let line = "A\u{1F600}\u{0411}\n";
    let content = line.repeat((PIECE_TABLE_MIN_BYTES / line.len()) + 16);
    std::fs::write(&path, content).unwrap();

    let mut doc = Document::open(path.clone()).unwrap();
    let (line0, col0) = doc.try_insert_text_at(0, 2, "X").unwrap();

    assert_eq!((line0, col0), (0, 3));
    assert!(doc.has_edit_buffer());
    assert!(doc.text_lossy().starts_with("A\u{1F600}X\u{0411}\n"));

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn piece_table_fragmentation_stats_track_piece_growth_after_edit() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("fragmentation.txt");
    std::fs::write(&path, "abcdef").unwrap();

    let storage = FileStorage::open(&path).unwrap();
    let mut piece_table = PieceTable::new(storage, vec![6], true);

    let before = piece_table.fragmentation_stats_with_threshold(2);
    assert_eq!(before.piece_count, 1);
    assert_eq!(before.total_bytes, 6);
    assert_eq!(before.small_piece_count, 0);
    assert_eq!(before.small_piece_bytes, 0);

    piece_table.insert_bytes(3, b"XY").unwrap();

    let after = piece_table.fragmentation_stats_with_threshold(2);
    assert_eq!(after.piece_count, 3);
    assert_eq!(after.total_bytes, 8);
    assert_eq!(after.small_piece_threshold_bytes, 2);
    assert_eq!(after.small_piece_count, 1);
    assert_eq!(after.small_piece_bytes, 2);
    assert!((after.average_piece_bytes() - (8.0 / 3.0)).abs() < 1e-9);
    assert!((after.fragmentation_ratio() - (1.0 / 3.0)).abs() < 1e-9);
}

#[test]
fn piece_table_compaction_policy_recommends_deferred_for_fragmented_small_pieces() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("fragmented.txt");
    std::fs::write(&path, "abcdef").unwrap();

    let storage = FileStorage::open(&path).unwrap();
    let mut piece_table = PieceTable::new(storage, vec![6], true);
    piece_table.insert_bytes(3, b"XY").unwrap();

    let recommendation = piece_table.compaction_recommendation(CompactionPolicy {
        min_total_bytes: 0,
        min_piece_count: 3,
        small_piece_threshold_bytes: 2,
        max_average_piece_bytes: 4,
        min_fragmentation_ratio: 0.3,
        forced_piece_count: 8,
        forced_fragmentation_ratio: 0.6,
    });

    let recommendation = recommendation.expect("expected deferred compaction recommendation");
    assert_eq!(recommendation.urgency(), CompactionUrgency::Deferred);
    assert_eq!(recommendation.stats().piece_count, 3);
    assert_eq!(recommendation.stats().small_piece_count, 1);
}

#[test]
fn piece_table_compaction_policy_recommends_forced_when_fragmentation_crosses_hard_threshold() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("forced-fragmented.txt");
    std::fs::write(&path, "abcdef").unwrap();

    let storage = FileStorage::open(&path).unwrap();
    let mut piece_table = PieceTable::new(storage, vec![6], true);
    piece_table.insert_bytes(1, b"X").unwrap();
    piece_table.insert_bytes(3, b"Y").unwrap();
    piece_table.insert_bytes(5, b"Z").unwrap();

    let recommendation = piece_table.compaction_recommendation(CompactionPolicy {
        min_total_bytes: 0,
        min_piece_count: 5,
        small_piece_threshold_bytes: 1,
        max_average_piece_bytes: 4,
        min_fragmentation_ratio: 0.2,
        forced_piece_count: 7,
        forced_fragmentation_ratio: 0.4,
    });

    let recommendation = recommendation.expect("expected forced compaction recommendation");
    assert_eq!(recommendation.urgency(), CompactionUrgency::Forced);
    assert_eq!(recommendation.stats().piece_count, 7);
    assert_eq!(recommendation.stats().small_piece_count, 6);
}

#[test]
fn piece_table_compaction_rewrites_current_state_without_new_undo_step() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("compact-current.txt");
    std::fs::write(&path, "abcdef").unwrap();

    let storage = FileStorage::open(&path).unwrap();
    let mut piece_table = PieceTable::new(storage, vec![6], true);
    piece_table.insert_bytes(1, b"X").unwrap();
    piece_table.insert_bytes(3, b"Y").unwrap();
    piece_table.insert_bytes(5, b"Z").unwrap();

    let before_text = piece_table.to_string_lossy();
    let before_stats = piece_table.fragmentation_stats_with_threshold(1);
    assert_eq!(before_stats.piece_count, 7);

    assert!(piece_table.compact_current_state().unwrap());

    let after_stats = piece_table.fragmentation_stats_with_threshold(1);
    assert_eq!(piece_table.to_string_lossy(), before_text);
    assert_eq!(after_stats.piece_count, 1);
    assert!(piece_table.full_index());
    assert_eq!(piece_table.known_byte_len, piece_table.total_len());

    assert!(piece_table.undo().unwrap());
    assert_eq!(piece_table.to_string_lossy(), "aXbYcdef");
    assert!(piece_table.redo().unwrap());
    assert_eq!(piece_table.to_string_lossy(), before_text);
}

#[test]
fn document_compact_piece_table_preserves_recovery_and_clears_recommendation() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("compact-recovery.txt");
    let seed = "a".repeat(PIECE_TABLE_MIN_BYTES) + "abcdef";
    std::fs::write(&path, seed).unwrap();

    let storage = FileStorage::open(&path).unwrap();
    let mut piece_table = PieceTable::new(storage, vec![PIECE_TABLE_MIN_BYTES + 6], true);
    let base = PIECE_TABLE_MIN_BYTES;
    piece_table.insert_bytes(base + 1, b"X").unwrap();
    piece_table.insert_bytes(base + 3, b"Y").unwrap();
    piece_table.insert_bytes(base + 5, b"Z").unwrap();

    let before_text = piece_table.to_string_lossy();
    let recommendation = piece_table.compaction_recommendation(CompactionPolicy {
        min_total_bytes: 0,
        min_piece_count: 5,
        small_piece_threshold_bytes: 1,
        max_average_piece_bytes: 4,
        min_fragmentation_ratio: 0.2,
        forced_piece_count: 7,
        forced_fragmentation_ratio: 0.4,
    });
    assert!(recommendation.is_some());

    let mut doc = Document {
        path: Some(path.clone()),
        storage: None,
        line_offsets: Arc::new(RwLock::new(LineOffsets::default())),
        disk_index: None,
        indexing: Arc::new(AtomicBool::new(false)),
        indexing_started: None,
        file_len: PIECE_TABLE_MIN_BYTES + 6,
        indexed_bytes: Arc::new(AtomicUsize::new(0)),
        avg_line_len: Arc::new(AtomicUsize::new(1)),
        line_ending: LineEnding::Lf,
        encoding: DocumentEncoding::utf8(),
        encoding_origin: DocumentEncodingOrigin::NewDocument,
        encoding_engine: super::encoding_engine::engine_for_encoding(DocumentEncoding::utf8()),
        decoding_had_errors: false,
        preserve_save_error_cache: Cell::new(None),
        rope_file_len_cache: Cell::new(None),
        rope: None,
        rope_history: RopeHistory::new(),
        piece_table: Some(piece_table),
        dirty: true,
    };

    let applied = doc
        .compact_piece_table_if_recommended(CompactionPolicy {
            min_total_bytes: 0,
            min_piece_count: 5,
            small_piece_threshold_bytes: 1,
            max_average_piece_bytes: 4,
            min_fragmentation_ratio: 0.2,
            forced_piece_count: 7,
            forced_fragmentation_ratio: 0.4,
        })
        .unwrap();
    assert!(applied.is_some());
    assert_eq!(doc.text_lossy(), before_text);
    assert!(doc
        .compaction_recommendation_with_policy(CompactionPolicy {
            min_total_bytes: 0,
            min_piece_count: 5,
            small_piece_threshold_bytes: 1,
            max_average_piece_bytes: 4,
            min_fragmentation_ratio: 0.2,
            forced_piece_count: 7,
            forced_fragmentation_ratio: 0.4,
        })
        .is_none());

    doc.flush_session().unwrap();
    let recovered = Document::open(&path).unwrap();
    assert_eq!(recovered.text_lossy(), before_text);
}

#[test]
fn document_compaction_refuses_above_the_full_materialization_budget() {
    // ea6711d capped compaction by the full-materialization budget:
    // `run_idle_compaction` is advertised as an idle-time helper and
    // must never abort the process by materializing a multi-gigabyte
    // document. This pins both halves - the recommender never proposes
    // above the budget and the executor refuses - without writing
    // 128 MiB of content (the file tail is an OS-reserved hole).
    let dir = tempdir().unwrap();
    let path = dir.path().join("over-budget.txt");
    {
        let mut file = std::fs::File::create(&path).unwrap();
        use std::io::Write as _;
        file.write_all(b"abc\n").unwrap();
        file.set_len((MAX_ROPE_EDIT_FILE_BYTES + 1) as u64).unwrap();
    }

    let storage = FileStorage::open(&path).unwrap();
    let mut piece_table = PieceTable::new(storage, vec![MAX_ROPE_EDIT_FILE_BYTES + 1], true);
    piece_table.insert_bytes(1, b"X").unwrap();
    piece_table.insert_bytes(3, b"Y").unwrap();

    // A policy that would happily recommend (and force) compaction on a
    // small fragmented document.
    let policy = CompactionPolicy {
        min_total_bytes: 0,
        min_piece_count: 2,
        small_piece_threshold_bytes: 1,
        max_average_piece_bytes: 4,
        min_fragmentation_ratio: 0.0,
        forced_piece_count: 3,
        forced_fragmentation_ratio: 0.0,
    };

    assert!(
        piece_table.compaction_recommendation(policy).is_none(),
        "the recommender must never propose a rewrite above the budget"
    );
    assert!(
        !piece_table.compact_current_state().unwrap(),
        "the executor must refuse above the budget"
    );

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir_all(dir.path());
}

#[test]
fn idle_compaction_runs_for_deferred_recommendations() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("idle-deferred.txt");
    let line = b"0000target\n";
    let repeat = (1024 * 1024 / line.len()) + 64;
    std::fs::write(&path, line.repeat(repeat)).unwrap();

    let policy = CompactionPolicy {
        min_total_bytes: 0,
        min_piece_count: 2,
        small_piece_threshold_bytes: usize::MAX,
        max_average_piece_bytes: usize::MAX,
        min_fragmentation_ratio: 0.0,
        forced_piece_count: usize::MAX,
        forced_fragmentation_ratio: 1.0,
    };

    let mut doc = Document::open(path).unwrap();
    match doc.try_insert(TextPosition::new(0, 0), "[qem]") {
        Ok(_) => {}
        Err(DocumentError::Write { .. }) => return,
        Err(err) => panic!("unexpected insert error: {err}"),
    }
    let before = doc
        .fragmentation_stats()
        .expect("fragmentation stats before idle compaction")
        .piece_count();
    assert!(before > 1);

    let outcome = doc.run_idle_compaction_with_policy(policy).unwrap();
    match outcome {
        IdleCompactionOutcome::Compacted(recommendation) => {
            assert_eq!(recommendation.urgency(), CompactionUrgency::Deferred);
        }
        other => panic!("unexpected idle compaction outcome: {other:?}"),
    }

    let after = doc
        .fragmentation_stats()
        .expect("fragmentation stats after idle compaction")
        .piece_count();
    assert_eq!(after, 1);
}

#[test]
fn idle_compaction_reports_forced_threshold_without_rewriting_state() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("idle-forced.txt");
    let line = b"0000target\n";
    let repeat = (1024 * 1024 / line.len()) + 64;
    std::fs::write(&path, line.repeat(repeat)).unwrap();

    let policy = CompactionPolicy {
        min_total_bytes: 0,
        min_piece_count: 2,
        small_piece_threshold_bytes: usize::MAX,
        max_average_piece_bytes: usize::MAX,
        min_fragmentation_ratio: 0.0,
        forced_piece_count: 2,
        forced_fragmentation_ratio: 0.0,
    };

    let mut doc = Document::open(path).unwrap();
    match doc.try_insert(TextPosition::new(0, 0), "[qem]") {
        Ok(_) => {}
        Err(DocumentError::Write { .. }) => return,
        Err(err) => panic!("unexpected insert error: {err}"),
    }
    let before = doc
        .fragmentation_stats()
        .expect("fragmentation stats before forced idle pass")
        .piece_count();
    assert!(before > 1);
    assert_eq!(
        doc.maintenance_action_with_policy(policy),
        MaintenanceAction::ExplicitCompaction
    );

    let outcome = doc.run_idle_compaction_with_policy(policy).unwrap();
    match outcome {
        IdleCompactionOutcome::ForcedPending(recommendation) => {
            assert_eq!(recommendation.urgency(), CompactionUrgency::Forced);
        }
        other => panic!("unexpected idle compaction outcome: {other:?}"),
    }

    let after = doc
        .fragmentation_stats()
        .expect("fragmentation stats after forced idle pass")
        .piece_count();
    assert_eq!(after, before);
}

#[test]
fn prepare_save_with_forced_compaction_policy_compacts_before_snapshotting() {
    let dir = tempdir().unwrap();
    let source = dir.path().join("save-compact-source.txt");
    let target = dir.path().join("save-compact-target.txt");
    std::fs::write(&source, "abcdef").unwrap();

    let storage = FileStorage::open(&source).unwrap();
    let mut piece_table = PieceTable::new(storage, vec![6], true);
    piece_table.insert_bytes(1, b"X").unwrap();
    piece_table.insert_bytes(3, b"Y").unwrap();
    piece_table.insert_bytes(5, b"Z").unwrap();

    let expected_text = piece_table.to_string_lossy();
    let policy = CompactionPolicy {
        min_total_bytes: 0,
        min_piece_count: 5,
        small_piece_threshold_bytes: 1,
        max_average_piece_bytes: 4,
        min_fragmentation_ratio: 0.2,
        forced_piece_count: 7,
        forced_fragmentation_ratio: 0.4,
    };

    let mut doc = Document {
        path: Some(source.clone()),
        storage: None,
        line_offsets: Arc::new(RwLock::new(LineOffsets::default())),
        disk_index: None,
        indexing: Arc::new(AtomicBool::new(false)),
        indexing_started: None,
        file_len: 6,
        indexed_bytes: Arc::new(AtomicUsize::new(0)),
        avg_line_len: Arc::new(AtomicUsize::new(1)),
        line_ending: LineEnding::Lf,
        encoding: DocumentEncoding::utf8(),
        encoding_origin: DocumentEncodingOrigin::NewDocument,
        encoding_engine: super::encoding_engine::engine_for_encoding(DocumentEncoding::utf8()),
        decoding_had_errors: false,
        preserve_save_error_cache: Cell::new(None),
        rope_file_len_cache: Cell::new(None),
        rope: None,
        rope_history: RopeHistory::new(),
        piece_table: Some(piece_table),
        dirty: true,
    };

    let before = doc.fragmentation_stats_with_threshold(1).unwrap();
    assert_eq!(before.piece_count, 7);

    let prepared = doc.prepare_save_with_policy(&target, Some(policy)).unwrap();
    let after = doc.fragmentation_stats_with_threshold(1).unwrap();
    assert_eq!(after.piece_count, 1);

    let completion = prepared.execute(Arc::new(AtomicU64::new(0))).unwrap();
    doc.finish_save(
        completion.path,
        completion.reload_after_save,
        completion.encoding,
        completion.encoding_origin,
    )
    .unwrap();

    assert_eq!(std::fs::read_to_string(&target).unwrap(), expected_text);
}

#[test]
fn open_with_encoding_preserves_legacy_text_and_default_save_round_trips() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("legacy-cp1251.txt");
    let saved = dir.path().join("legacy-cp1251-saved.txt");
    let encoding = DocumentEncoding::from_label("windows-1251").unwrap();
    let source_text = "привет\nмир\n";
    let (bytes, used, had_errors) = WINDOWS_1251.encode(source_text);
    assert_eq!(used, WINDOWS_1251);
    assert!(!had_errors);
    std::fs::write(&path, bytes.as_ref()).unwrap();

    let mut doc = Document::open_with_encoding(path.clone(), encoding).unwrap();
    assert_eq!(doc.encoding(), encoding);
    assert_eq!(
        doc.encoding_origin(),
        DocumentEncodingOrigin::ExplicitReinterpretation
    );
    assert!(!doc.decoding_had_errors());
    assert_eq!(doc.text_lossy(), source_text);

    let edit = doc.try_insert_text_at(0, 0, "эй, ").unwrap();
    assert_eq!(edit, (0, 4));
    doc.save_to(&saved).unwrap();

    let raw = std::fs::read(&saved).unwrap();
    let (decoded, used, had_errors) = WINDOWS_1251.decode(&raw);
    assert_eq!(used, WINDOWS_1251);
    assert!(!had_errors);
    assert_eq!(decoded, "эй, привет\nмир\n");
}

#[test]
fn open_with_options_and_save_options_preserve_encoding_contract() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("legacy-cp1251-options.txt");
    let saved = dir.path().join("legacy-cp1251-options-saved.txt");
    let encoding = DocumentEncoding::from_label("windows-1251").unwrap();
    let (bytes, used, had_errors) = WINDOWS_1251.encode("данные\n");
    assert_eq!(used, WINDOWS_1251);
    assert!(!had_errors);
    std::fs::write(&path, bytes.as_ref()).unwrap();

    let mut doc =
        Document::open_with_options(path, DocumentOpenOptions::new().with_encoding(encoding))
            .unwrap();
    let _ = doc.try_insert_text_at(1, 0, "ещё\n").unwrap();
    doc.save_to_with_options(&saved, DocumentSaveOptions::new())
        .unwrap();

    let raw = std::fs::read(&saved).unwrap();
    let (decoded, used, had_errors) = WINDOWS_1251.decode(&raw);
    assert_eq!(used, WINDOWS_1251);
    assert!(!had_errors);
    assert_eq!(decoded, "данные\nещё\n");
}

#[test]
fn open_with_options_reinterpretation_decodes_legacy_bytes() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("legacy-windows-1252.txt");
    let encoding = DocumentEncoding::from_label("windows-1252").unwrap();
    std::fs::write(&path, b"caf\xe9\n").unwrap();

    let doc = Document::open_with_options(
        path,
        DocumentOpenOptions::new().with_reinterpretation(encoding),
    )
    .unwrap();

    assert_eq!(doc.encoding(), encoding);
    assert_eq!(doc.text_lossy(), "café\n");
}

#[test]
fn open_with_encoding_shift_jis_preserves_text_and_default_save_round_trips() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("legacy-shift-jis.txt");
    let saved = dir.path().join("legacy-shift-jis-saved.txt");
    let encoding = DocumentEncoding::from_label("shift_jis").unwrap();
    let source_text = "\u{65E5}\u{672C}\u{8A9E}\n";
    let inserted_text = "\u{8FFD}\u{52A0}\n";
    let expected_text = format!("{inserted_text}{source_text}");
    let (bytes, used, had_errors) = SHIFT_JIS.encode(source_text);
    assert_eq!(used, SHIFT_JIS);
    assert!(!had_errors);
    std::fs::write(&path, bytes.as_ref()).unwrap();

    let mut doc = Document::open_with_encoding(path.clone(), encoding).unwrap();
    assert_eq!(doc.encoding(), encoding);
    assert_eq!(
        doc.encoding_origin(),
        DocumentEncodingOrigin::ExplicitReinterpretation
    );
    assert!(!doc.decoding_had_errors());
    assert_eq!(doc.text_lossy(), source_text);

    let _ = doc.try_insert_text_at(0, 0, inserted_text).unwrap();
    doc.save_to(&saved).unwrap();

    let raw = std::fs::read(&saved).unwrap();
    let (decoded, used, had_errors) = SHIFT_JIS.decode(&raw);
    assert_eq!(used, SHIFT_JIS);
    assert!(!had_errors);
    assert_eq!(decoded, expected_text);
}

#[test]
fn preserve_save_rejects_lossy_shift_jis_source() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("lossy-shift-jis.txt");
    let saved = dir.path().join("lossy-shift-jis-saved.txt");
    let encoding = DocumentEncoding::from_label("shift_jis").unwrap();
    std::fs::write(&path, [0x82]).unwrap();

    let mut doc = Document::open_with_encoding(path, encoding).unwrap();

    assert_eq!(doc.encoding(), encoding);
    assert_eq!(
        doc.encoding_origin(),
        DocumentEncodingOrigin::ExplicitReinterpretation
    );
    assert!(doc.decoding_had_errors());
    assert_eq!(doc.text_lossy(), "\u{FFFD}");

    let err = doc.save_to(&saved).unwrap_err();
    assert!(matches!(
        err,
        DocumentError::Encoding {
            path: failed_path,
            operation: "save",
            encoding: failed_encoding,
            reason: DocumentEncodingErrorKind::LossyDecodedPreserve,
        } if failed_path == saved && failed_encoding == encoding
    ));
}

#[test]
fn lossy_shift_jis_source_can_be_explicitly_converted_to_utf8() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("lossy-shift-jis-convert.txt");
    let saved = dir.path().join("lossy-shift-jis-convert-saved.txt");
    let encoding = DocumentEncoding::from_label("shift_jis").unwrap();
    std::fs::write(&path, [0x82]).unwrap();

    let mut doc = Document::open_with_encoding(path, encoding).unwrap();
    assert!(doc.decoding_had_errors());

    doc.save_to_with_encoding(&saved, DocumentEncoding::utf8())
        .unwrap();

    assert_eq!(doc.encoding(), DocumentEncoding::utf8());
    assert_eq!(
        doc.encoding_origin(),
        DocumentEncodingOrigin::SaveConversion
    );
    assert!(!doc.decoding_had_errors());
    assert_eq!(std::fs::read_to_string(&saved).unwrap(), "\u{FFFD}");
}

#[test]
fn invalid_utf8_inline_open_tracks_lossy_decode_but_preserve_save_stays_raw_safe() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("invalid-utf8-inline-open.txt");
    let saved = dir.path().join("invalid-utf8-inline-open-saved.txt");
    let bytes = [0x66, 0x6f, 0x80, 0x6f, b'\n'];
    std::fs::write(&path, bytes).unwrap();

    let mut doc = Document::open(path).unwrap();
    assert_eq!(doc.encoding(), DocumentEncoding::utf8());
    assert_eq!(doc.encoding_origin(), DocumentEncodingOrigin::Utf8FastPath);
    assert!(doc.decoding_had_errors());
    assert!(doc.can_preserve_save());
    assert_eq!(doc.preserve_save_error(), None);
    assert!(doc.status().decoding_had_errors());
    assert_eq!(doc.text_lossy(), "fo\u{FFFD}o\n");

    doc.save_to(&saved).unwrap();

    assert_eq!(std::fs::read(&saved).unwrap(), bytes);
}

#[test]
fn invalid_utf8_fast_path_can_be_explicitly_converted_to_clean_utf8_without_edits() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("invalid-utf8-fast-path-convert.txt");
    let saved = dir.path().join("invalid-utf8-fast-path-converted.txt");
    std::fs::write(&path, [0x66, 0x6f, 0x80, 0x6f, b'\n']).unwrap();

    let mut doc = Document::open(path).unwrap();
    assert_eq!(doc.encoding(), DocumentEncoding::utf8());
    assert_eq!(doc.encoding_origin(), DocumentEncodingOrigin::Utf8FastPath);
    assert!(doc.decoding_had_errors());
    assert!(!doc.has_edit_buffer());

    doc.save_to_with_encoding(&saved, DocumentEncoding::utf8())
        .unwrap();

    assert_eq!(doc.path(), Some(saved.as_path()));
    assert_eq!(doc.encoding(), DocumentEncoding::utf8());
    assert_eq!(
        doc.encoding_origin(),
        DocumentEncodingOrigin::SaveConversion
    );
    assert!(!doc.decoding_had_errors());
    assert_eq!(std::fs::read_to_string(&saved).unwrap(), "fo\u{FFFD}o\n");
}

#[test]
fn edited_invalid_utf8_fast_path_becomes_lossy_preserve_rejecting() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("invalid-utf8-fast-path.txt");
    let saved = dir.path().join("invalid-utf8-fast-path-saved.txt");
    std::fs::write(&path, [0x66, 0x6f, 0x80, 0x6f, b'\n']).unwrap();

    let mut doc = Document::open(path).unwrap();
    assert_eq!(doc.encoding(), DocumentEncoding::utf8());
    assert_eq!(doc.encoding_origin(), DocumentEncodingOrigin::Utf8FastPath);
    assert!(doc.decoding_had_errors());
    assert!(doc.can_preserve_save());

    let _ = doc.try_insert_text_at(0, 0, "X").unwrap();

    assert!(doc.decoding_had_errors());
    assert_eq!(
        doc.preserve_save_error(),
        Some(DocumentEncodingErrorKind::LossyDecodedPreserve)
    );
    assert_eq!(
        doc.save_error_for_options(DocumentSaveOptions::new()),
        Some(DocumentEncodingErrorKind::LossyDecodedPreserve)
    );

    let err = doc.save_to(&saved).unwrap_err();
    assert!(matches!(
        err,
        DocumentError::Encoding {
            path: failed_path,
            operation: "save",
            encoding,
            reason: DocumentEncodingErrorKind::LossyDecodedPreserve,
        } if failed_path == saved && encoding == DocumentEncoding::utf8()
    ));

    doc.save_to_with_encoding(&saved, DocumentEncoding::utf8())
        .unwrap();
    assert_eq!(
        doc.encoding_origin(),
        DocumentEncodingOrigin::SaveConversion
    );
    assert!(!doc.decoding_had_errors());
    assert_eq!(std::fs::read_to_string(&saved).unwrap(), "Xfo\u{FFFD}o\n");
}

#[test]
fn piece_table_invalid_utf8_fast_path_converts_to_clean_utf8() {
    let dir = tempdir().unwrap();
    let src = dir.path().join("invalid-utf8-piece-table-source.txt");
    let saved = dir.path().join("invalid-utf8-piece-table-saved.txt");
    let mut bytes = Vec::with_capacity(PIECE_TABLE_MIN_BYTES + 16);
    bytes.extend_from_slice(&[0x66, 0x6f, 0x80, 0x6f, b'\n']);
    while bytes.len() < PIECE_TABLE_MIN_BYTES + 16 {
        bytes.extend_from_slice(b"abc\n");
    }
    std::fs::write(&src, &bytes).unwrap();

    let mut doc = Document::open(src.clone()).unwrap();
    let _ = doc.try_insert_text_at(0, 0, "X").unwrap();
    assert!(doc.has_piece_table());

    doc.save_to_with_encoding(&saved, DocumentEncoding::utf8())
        .unwrap();

    assert_eq!(doc.path(), Some(saved.as_path()));
    assert_eq!(doc.encoding(), DocumentEncoding::utf8());
    assert_eq!(
        doc.encoding_origin(),
        DocumentEncodingOrigin::SaveConversion
    );
    assert!(!doc.decoding_had_errors());
    assert!(!doc.has_piece_table());
    assert!(std::fs::read_to_string(&saved)
        .unwrap()
        .starts_with("Xfo\u{FFFD}o\n"));
}

#[test]
fn save_to_with_encoding_gb18030_converts_utf8_document() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("converted-gb18030.txt");
    let encoding = DocumentEncoding::from_label("gb18030").unwrap();
    let source_text = "\u{4F60}\u{597D}\u{4E16}\u{754C}\n";
    let mut doc = Document::new();
    doc.try_insert_text_at(0, 0, source_text).unwrap();

    doc.save_to_with_encoding(&path, encoding).unwrap();

    assert_eq!(doc.encoding(), encoding);
    assert_eq!(
        doc.encoding_origin(),
        DocumentEncodingOrigin::SaveConversion
    );
    assert!(!doc.decoding_had_errors());

    let raw = std::fs::read(&path).unwrap();
    let (decoded, used, had_errors) = GB18030.decode(&raw);
    assert_eq!(used, GB18030);
    assert!(!had_errors);
    assert_eq!(decoded, source_text);
}

#[test]
fn open_with_options_auto_detects_utf16le_bom_and_allows_utf8_convert_save() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("utf16le-source.txt");
    let saved = dir.path().join("utf16le-converted.txt");
    let source_text = "hello\nworld\n";
    let mut bytes = vec![0xFF, 0xFE];
    for unit in source_text.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    std::fs::write(&path, bytes).unwrap();

    let mut doc = Document::open_with_auto_encoding_detection(path).unwrap();
    assert_eq!(doc.encoding(), DocumentEncoding::utf16le());
    assert_eq!(doc.encoding_origin(), DocumentEncodingOrigin::AutoDetected);
    assert_eq!(doc.text_lossy(), source_text);

    let _ = doc.try_insert_text_at(0, 0, "header\n").unwrap();
    doc.save_to_with_options(
        &saved,
        DocumentSaveOptions::new().with_encoding(DocumentEncoding::utf8()),
    )
    .unwrap();

    assert_eq!(
        std::fs::read_to_string(&saved).unwrap(),
        "header\nhello\nworld\n"
    );
    assert_eq!(doc.encoding(), DocumentEncoding::utf8());
    assert_eq!(
        doc.encoding_origin(),
        DocumentEncodingOrigin::SaveConversion
    );
    assert!(!doc.decoding_had_errors());
}

#[test]
fn open_with_auto_detection_detects_utf16be_bom() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("utf16be-source.txt");
    let source_text = "hello\nworld\n";
    let mut bytes = vec![0xFE, 0xFF];
    for unit in source_text.encode_utf16() {
        bytes.extend_from_slice(&unit.to_be_bytes());
    }
    std::fs::write(&path, bytes).unwrap();

    let doc = Document::open_with_auto_encoding_detection(path).unwrap();

    assert_eq!(doc.encoding(), DocumentEncoding::utf16be());
    assert_eq!(doc.encoding_origin(), DocumentEncodingOrigin::AutoDetected);
    assert_eq!(doc.text_lossy(), source_text);
}

#[test]
fn open_with_auto_detection_detects_utf8_bom_and_strips_it_from_content() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("utf8-bom-source.txt");
    let mut bytes = vec![0xEF, 0xBB, 0xBF];
    bytes.extend_from_slice(b"hello\nworld\n");
    std::fs::write(&path, bytes).unwrap();

    let doc = Document::open_with_auto_encoding_detection(path).unwrap();

    assert_eq!(doc.encoding(), DocumentEncoding::utf8());
    assert_eq!(doc.encoding_origin(), DocumentEncodingOrigin::AutoDetected);
    assert!(!doc.decoding_had_errors());
    // BOM must not appear in document content.
    assert_eq!(doc.text_lossy(), "hello\nworld\n");
    assert_eq!(doc.line_len_chars(0), 5);
}

#[test]
fn open_without_options_does_not_strip_utf8_bom_under_default_fast_path() {
    // Without auto-detection the open path is the UTF-8 fast path. Frontends
    // that intentionally bypass auto-detection are responsible for handling
    // a BOM if they need to. This test pins that behavior so future changes
    // do not silently rewrite content during a default open.
    let dir = tempdir().unwrap();
    let path = dir.path().join("utf8-bom-default-open.txt");
    let mut bytes = vec![0xEF, 0xBB, 0xBF];
    bytes.extend_from_slice(b"hello\n");
    std::fs::write(&path, &bytes).unwrap();

    let doc = Document::open(&path).unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while doc.is_indexing() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }

    assert_eq!(doc.encoding(), DocumentEncoding::utf8());
    // Default open path: the BOM stays in raw bytes. Frontends opt in to
    // BOM-aware behavior through `Document::open_with_auto_encoding_detection`
    // or `DocumentOpenOptions::with_auto_encoding_detection`.
    assert_eq!(doc.file_len(), bytes.len());
}

#[test]
fn open_options_auto_detection_fallback_exposes_override() {
    let encoding = DocumentEncoding::from_label("windows-1252").unwrap();
    let options = DocumentOpenOptions::new().with_auto_encoding_detection_and_fallback(encoding);

    assert_eq!(options.encoding_override(), Some(encoding));
    assert_eq!(
        options.encoding_policy(),
        OpenEncodingPolicy::AutoDetectOrReinterpret(encoding)
    );
}

#[test]
fn open_with_auto_detection_tracks_utf8_fallback_origin() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("utf8-autodetect-fallback.txt");
    std::fs::write(&path, "hello\nworld\n").unwrap();

    let doc = Document::open_with_auto_encoding_detection(path).unwrap();

    assert_eq!(doc.encoding(), DocumentEncoding::utf8());
    assert_eq!(
        doc.encoding_origin(),
        DocumentEncodingOrigin::AutoDetectFallbackUtf8
    );
    assert_eq!(doc.text_lossy(), "hello\nworld\n");
}

#[test]
fn open_with_auto_detection_and_fallback_reinterprets_valid_utf8_when_bom_is_missing() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("autodetect-fallback-valid-utf8.txt");
    let encoding = DocumentEncoding::from_label("windows-1252").unwrap();
    std::fs::write(&path, "caf\u{00E9}\n".as_bytes()).unwrap();

    let doc = Document::open_with_options(
        path,
        DocumentOpenOptions::new().with_auto_encoding_detection_and_fallback(encoding),
    )
    .unwrap();

    assert_eq!(doc.encoding(), encoding);
    assert_eq!(
        doc.encoding_origin(),
        DocumentEncodingOrigin::AutoDetectFallbackOverride
    );
    assert_eq!(doc.text_lossy(), "caf\u{00C3}\u{00A9}\n");
    assert_ne!(doc.text_lossy(), "caf\u{00E9}\n");
}

#[test]
fn open_with_auto_detection_and_fallback_still_prefers_detected_bom() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("autodetect-fallback-utf16le.txt");
    let fallback = DocumentEncoding::from_label("windows-1251").unwrap();
    let mut bytes = vec![0xFF, 0xFE];
    for unit in "hello\n".encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    std::fs::write(&path, bytes).unwrap();

    let doc = Document::open_with_options(
        path,
        DocumentOpenOptions::new().with_auto_encoding_detection_and_fallback(fallback),
    )
    .unwrap();

    assert_eq!(doc.encoding(), DocumentEncoding::utf16le());
    assert_eq!(doc.encoding_origin(), DocumentEncodingOrigin::AutoDetected);
    assert_eq!(doc.text_lossy(), "hello\n");
}

#[test]
fn preserve_save_succeeds_for_unedited_utf16_source() {
    // A UTF-16 document opened through the Class B
    // native path is mmap-only (`rope: None`, `piece_table: None`),
    // so the on-disk bytes already exist in the target encoding.
    // Preserve-save streams them byte-for-byte through the
    // `SaveSnapshot::Mmap` branch even though `encoding_rs` cannot
    // re-encode through `UTF-16LE` directly. Edited UTF-16 documents
    // (rope or piece-table populated) still surface
    // `PreserveSaveUnsupported` because they require a real encode
    // path; that case is exercised by the dedicated
    // unrepresentable-text tests below.
    let dir = tempdir().unwrap();
    let path = dir.path().join("utf16le-preserve.txt");
    let saved = dir.path().join("utf16le-preserve-saved.txt");
    let mut bytes = vec![0xFF, 0xFE];
    for unit in "hello\n".encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    std::fs::write(&path, &bytes).unwrap();

    let mut doc = Document::open_with_auto_encoding_detection(path).unwrap();
    assert_eq!(doc.encoding(), DocumentEncoding::utf16le());
    assert!(
        doc.can_preserve_save(),
        "mmap-only UTF-16 document must allow preserve-save",
    );
    doc.save_to(&saved).expect("preserve-save must succeed");

    let saved_bytes = std::fs::read(&saved).unwrap();
    assert_eq!(
        saved_bytes, bytes,
        "preserve-save without edits must produce byte-identical bytes",
    );
}

#[test]
fn save_to_with_encoding_converts_utf8_document() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("converted-cp1251.txt");
    let encoding = DocumentEncoding::from_label("windows-1251").unwrap();
    let mut doc = Document::new();
    doc.try_insert_text_at(0, 0, "тест\n").unwrap();

    doc.save_to_with_encoding(&path, encoding).unwrap();

    assert_eq!(doc.encoding(), encoding);
    assert_eq!(
        doc.encoding_origin(),
        DocumentEncodingOrigin::SaveConversion
    );
    assert!(!doc.decoding_had_errors());
    let raw = std::fs::read(&path).unwrap();
    let (decoded, used, had_errors) = WINDOWS_1251.decode(&raw);
    assert_eq!(used, WINDOWS_1251);
    assert!(!had_errors);
    assert_eq!(decoded, "тест\n");
}

#[test]
fn save_to_with_encoding_rejects_unrepresentable_text() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("emoji-cp1251.txt");
    let encoding = DocumentEncoding::from_label("windows-1251").unwrap();
    let mut doc = Document::new();
    doc.try_insert_text_at(0, 0, "emoji 🙂\n").unwrap();

    let err = doc.save_to_with_encoding(&path, encoding).unwrap_err();
    assert!(matches!(
        err,
        DocumentError::Encoding {
            path: failed_path,
            operation: "save",
            encoding: failed_encoding,
            reason: DocumentEncodingErrorKind::UnrepresentableText,
        } if failed_path == path && failed_encoding == encoding
    ));
}

#[test]
fn save_to_with_encoding_rejects_unsupported_target_encoding() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("utf16le-save.txt");
    let mut doc = Document::new();
    doc.try_insert_text_at(0, 0, "hello\n").unwrap();

    let err = doc
        .save_to_with_encoding(&path, DocumentEncoding::utf16le())
        .unwrap_err();
    assert!(matches!(
        err,
        DocumentError::Encoding {
            path: failed_path,
            operation: "save",
            encoding,
            reason: DocumentEncodingErrorKind::UnsupportedSaveTarget,
        } if failed_path == path && encoding == DocumentEncoding::utf16le()
    ));
}

#[test]
fn line_slices_match_exact_mmap_lines() {
    let dir = std::env::temp_dir().join(format!("qem-line-slices-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("lines.txt");
    std::fs::write(&path, b"zero\none\ntwo\nthree\n").unwrap();

    let doc = Document::open(&path).unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while doc.is_indexing() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }

    let slices = doc.line_slices(1, 3, 0, 16);
    let texts: Vec<String> = slices.into_iter().map(LineSlice::into_text).collect();

    assert_eq!(texts, vec!["one", "two", "three"]);

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn incomplete_mmap_singular_line_read_uses_exact_scan_for_existing_line() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("incomplete-mmap-exact-lines.txt");
    std::fs::write(&path, b"zero\none\ntwo\n").unwrap();
    let storage = FileStorage::open(&path).unwrap();

    let doc = Document {
        path: Some(path.clone()),
        storage: Some(storage.clone()),
        line_offsets: Arc::new(RwLock::new(LineOffsets::default())),
        disk_index: None,
        indexing: Arc::new(AtomicBool::new(false)),
        indexing_started: None,
        file_len: std::fs::metadata(&path).unwrap().len() as usize,
        indexed_bytes: Arc::new(AtomicUsize::new(0)),
        avg_line_len: Arc::new(AtomicUsize::new(128)),
        line_ending: LineEnding::Lf,
        encoding: DocumentEncoding::utf8(),
        encoding_origin: DocumentEncodingOrigin::Utf8FastPath,
        encoding_engine: super::encoding_engine::engine_for_encoding(DocumentEncoding::utf8()),
        decoding_had_errors: false,
        preserve_save_error_cache: Cell::new(None),
        rope_file_len_cache: Cell::new(None),
        rope: None,
        rope_history: RopeHistory::new(),
        piece_table: None,
        dirty: false,
    };

    let slice = doc.line_slice(1, 0, 16);
    assert_eq!(slice.text(), "one");
    assert!(slice.is_exact());
}

#[test]
fn incomplete_mmap_single_row_batch_reads_match_singular_line_lookup() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("incomplete-mmap-single-row-batch.txt");
    std::fs::write(&path, b"zero\none\ntwo\n").unwrap();
    let storage = FileStorage::open(&path).unwrap();

    let doc = Document {
        path: Some(path.clone()),
        storage: Some(storage.clone()),
        line_offsets: Arc::new(RwLock::new(LineOffsets::default())),
        disk_index: None,
        indexing: Arc::new(AtomicBool::new(false)),
        indexing_started: None,
        file_len: std::fs::metadata(&path).unwrap().len() as usize,
        indexed_bytes: Arc::new(AtomicUsize::new(0)),
        avg_line_len: Arc::new(AtomicUsize::new(128)),
        line_ending: LineEnding::Lf,
        encoding: DocumentEncoding::utf8(),
        encoding_origin: DocumentEncodingOrigin::Utf8FastPath,
        encoding_engine: super::encoding_engine::engine_for_encoding(DocumentEncoding::utf8()),
        decoding_had_errors: false,
        preserve_save_error_cache: Cell::new(None),
        rope_file_len_cache: Cell::new(None),
        rope: None,
        rope_history: RopeHistory::new(),
        piece_table: None,
        dirty: false,
    };

    let single = doc.line_slice(1, 0, 16);
    assert_eq!(single.text(), "one");
    assert!(single.is_exact());

    let slices = doc.line_slices(1, 1, 0, 16);
    assert_eq!(slices.len(), 1);
    assert_eq!(slices[0].text(), "one");
    assert!(slices[0].is_exact());

    let viewport = doc.read_viewport(ViewportRequest::new(1, 1).with_columns(0, 16));
    assert_eq!(viewport.rows().len(), 1);
    assert_eq!(viewport.rows()[0].text(), "one");
    assert!(viewport.rows()[0].is_exact());
}

#[test]
fn fully_indexed_mmap_out_of_range_line_reads_are_exact_empty() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("exact-mmap-out-of-range-lines.txt");
    std::fs::write(&path, b"zero\none\n").unwrap();

    let doc = Document::open(&path).unwrap();
    assert!(doc.is_line_count_exact());

    let slice = doc.line_slice(99, 0, 16);
    assert_eq!(slice.text(), "");
    assert!(slice.is_exact());

    let slices = doc.line_slices(99, 2, 0, 16);
    assert_eq!(slices.len(), 2);
    assert!(slices.iter().all(|slice| slice.text().is_empty()));
    assert!(slices.iter().all(LineSlice::is_exact));

    let viewport = doc.read_viewport(ViewportRequest::new(99, 2).with_columns(0, 16));
    assert_eq!(viewport.rows().len(), 2);
    assert!(viewport.rows().iter().all(|row| row.text().is_empty()));
    assert!(viewport.rows().iter().all(ViewportRow::is_exact));
}

fn checkpoint_viewport_fixture(path: &Path, ending: &str) -> Document {
    // Variable line widths deliberately make average-byte extrapolation wrong.
    let mut file = std::io::BufWriter::new(std::fs::File::create(path).unwrap());
    for line0 in 0..100_020 {
        write!(
            file,
            "row-{line0:06}-{}{ending}",
            "x".repeat(200 + line0 % 37)
        )
        .unwrap();
    }
    file.flush().unwrap();
    drop(file);
    let storage = FileStorage::open(path).unwrap();
    Document {
        path: Some(path.to_path_buf()),
        storage: Some(storage.clone()),
        line_offsets: Arc::new(RwLock::new(LineOffsets::U32(vec![0]))),
        disk_index: Some(DiskLineIndex::build_for_tests(path, &storage)),
        indexing: Arc::new(AtomicBool::new(false)),
        indexing_started: None,
        file_len: storage.len(),
        indexed_bytes: Arc::new(AtomicUsize::new(0)),
        avg_line_len: Arc::new(AtomicUsize::new(128)),
        line_ending: LineEnding::Lf,
        encoding: DocumentEncoding::utf8(),
        encoding_origin: DocumentEncodingOrigin::Utf8FastPath,
        encoding_engine: super::encoding_engine::engine_for_encoding(DocumentEncoding::utf8()),
        decoding_had_errors: false,
        preserve_save_error_cache: Cell::new(None),
        rope_file_len_cache: Cell::new(None),
        rope: None,
        rope_history: RopeHistory::new(),
        piece_table: None,
        dirty: false,
    }
}

fn assert_checkpoint_viewport(doc: &Document, first: usize, count: usize, prefix_delta: usize) {
    let viewport = doc.read_viewport(ViewportRequest::new(first, count).with_columns(0, 16));
    assert_eq!(viewport.len(), count);
    for (offset, row) in viewport.rows().iter().enumerate() {
        let line0 = first + offset;
        let original_line0 = line0 - prefix_delta;
        let text = format!(
            "row-{original_line0:06}-{}",
            "x".repeat(200 + original_line0 % 37)
        );
        let expected = &text[..text.len().min(16)];
        assert_eq!(row.line0(), line0);
        assert_eq!(row.text(), expected, "logical line {line0}");
        assert!(row.is_exact(), "logical line {line0}");
        assert_eq!(doc.line_slice(line0, 0, 16), *row.slice());
    }
}

#[test]
fn disk_checkpoint_viewport_resolves_random_lines_and_prefix_crossing() {
    let dir = tempdir().unwrap();
    let doc = checkpoint_viewport_fixture(&dir.path().join("checkpoint-viewport.txt"), "\r\n");
    assert!(
        doc.disk_index
            .as_ref()
            .unwrap()
            .checkpoint_for_line(100_000)
            .unwrap()
            .line0
            > 4096
    );
    assert_checkpoint_viewport(&doc, 4096, 8, 0);
    assert_checkpoint_viewport(&doc, 100_000, 8, 0);
    assert_checkpoint_viewport(&doc, 100_018, 2, 0);
    assert_checkpoint_viewport(&doc, 8190, 8, 0);
    assert_checkpoint_viewport(&doc, 0, 8, 0);
    let tail = doc.read_viewport(ViewportRequest::new(100_019, 4).with_columns(0, 16));
    assert!(tail.rows().iter().all(ViewportRow::is_exact));
    assert!(tail.rows()[1..].iter().all(|row| row.text().is_empty()));
    assert!(!doc.has_edit_buffer());
    assert_eq!(doc.line_offsets.read().unwrap().len(), 1);
}

#[test]
fn disk_checkpoint_partial_piece_table_viewport_survives_prefix_edit_and_recovery() {
    let dir = tempdir().unwrap();
    let mut doc = checkpoint_viewport_fixture(&dir.path().join("checkpoint-piece-table.txt"), "\n");
    let storage = doc.storage.as_ref().unwrap().clone();
    let mut lengths = Vec::new();
    let mut start = 0;
    for _ in 0..4096 {
        let end = doc
            .encoding_engine()
            .next_line_start(storage.bytes(), storage.len(), start);
        lengths.push(end - start);
        start = end;
    }
    doc.piece_table = Some(PieceTable::new(storage.clone(), lengths, false));
    assert_checkpoint_viewport(&doc, 4094, 8, 0);
    assert_checkpoint_viewport(&doc, 100_000, 8, 0);
    doc.try_insert_text_at(0, 0, "TOP\n").unwrap();
    assert_eq!(doc.piece_table.as_ref().unwrap().line_count(), 4097);
    assert_checkpoint_viewport(&doc, 4095, 8, 1);
    assert_checkpoint_viewport(&doc, 100_000, 8, 1);
    let table = doc.piece_table.take().unwrap();
    let meta = table.session_meta();
    doc.piece_table = Some(PieceTable::from_recovered_session(
        storage,
        table.add,
        table.pieces,
        meta,
    ));
    assert_checkpoint_viewport(&doc, 4095, 8, 1);
    assert_checkpoint_viewport(&doc, 100_000, 8, 1);
    assert!(!doc.piece_table.as_ref().unwrap().full_index());
    assert_eq!(doc.line_offsets.read().unwrap().len(), 1);
}

#[test]
fn line_slices_use_exact_piece_table_fast_path_after_edit() {
    let dir = std::env::temp_dir().join(format!("qem-piece-table-slices-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("large.txt");
    write_disk_backed_fixture(&path);

    let mut doc = Document::open(path.clone()).unwrap();
    let _ = doc.try_insert_text_at(0, 0, "123").unwrap();

    let slices = doc.line_slices(0, 2, 0, 16);
    let texts: Vec<String> = slices.iter().map(|slice| slice.text().to_owned()).collect();

    assert_eq!(texts, vec!["123abc", "def"]);
    assert!(slices.iter().all(LineSlice::is_exact));

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn partial_piece_table_line_reads_follow_current_text_after_prefix_edit() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("partial-piece-table-lines.txt");
    std::fs::write(&path, b"zero\none\ntwo\nthree\n").unwrap();
    let storage = FileStorage::open(&path).unwrap();

    let mut doc = Document {
        path: Some(path.clone()),
        storage: Some(storage.clone()),
        line_offsets: Arc::new(RwLock::new(LineOffsets::default())),
        disk_index: None,
        indexing: Arc::new(AtomicBool::new(false)),
        indexing_started: None,
        file_len: std::fs::metadata(&path).unwrap().len() as usize,
        indexed_bytes: Arc::new(AtomicUsize::new(0)),
        avg_line_len: Arc::new(AtomicUsize::new(4)),
        line_ending: LineEnding::Lf,
        encoding: DocumentEncoding::utf8(),
        encoding_origin: DocumentEncodingOrigin::Utf8FastPath,
        encoding_engine: super::encoding_engine::engine_for_encoding(DocumentEncoding::utf8()),
        decoding_had_errors: false,
        preserve_save_error_cache: Cell::new(None),
        rope_file_len_cache: Cell::new(None),
        rope: None,
        rope_history: RopeHistory::new(),
        piece_table: Some(PieceTable::new(storage, vec![5, 4], false)),
        dirty: false,
    };

    let cursor = doc.try_insert_text_at(0, 0, "TOP\n").unwrap();
    assert_eq!(cursor, (1, 0));
    assert!(doc.piece_table.is_some());
    assert!(doc.text_lossy().starts_with("TOP\nzero\none\ntwo\nthree\n"));

    let slice = doc.line_slice(3, 0, 16);
    assert_eq!(slice.text(), "two");
    assert!(slice.is_exact());

    let slices = doc.line_slices(2, 3, 0, 16);
    let texts: Vec<String> = slices.into_iter().map(LineSlice::into_text).collect();
    assert_eq!(texts, vec!["one", "two", "three"]);

    let text = doc.read_text(TextRange::new(TextPosition::new(3, 0), 3));
    assert_eq!(text.text(), "two");
    assert!(text.is_exact());

    let found = doc.find_next("two", TextPosition::new(3, 0)).unwrap();
    assert_eq!(found.start(), TextPosition::new(3, 0));
    assert_eq!(found.end(), TextPosition::new(3, 3));
}

#[test]
fn singular_partial_piece_table_line_slice_does_not_fall_back_to_stale_mmap() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("partial-piece-table-singular-slice.txt");
    let mut bytes = b"zero\n".to_vec();
    bytes.extend(std::iter::repeat_n(
        b'x',
        PARTIAL_PIECE_TABLE_SCAN_BYTES.saturating_add(1),
    ));
    bytes.push(b'\n');
    bytes.extend_from_slice(b"tail\n");
    std::fs::write(&path, &bytes).unwrap();
    let storage = FileStorage::open(&path).unwrap();

    let mut doc = Document {
        path: Some(path.clone()),
        storage: Some(storage.clone()),
        line_offsets: Arc::new(RwLock::new(LineOffsets::default())),
        disk_index: None,
        indexing: Arc::new(AtomicBool::new(false)),
        indexing_started: None,
        file_len: bytes.len(),
        indexed_bytes: Arc::new(AtomicUsize::new(0)),
        avg_line_len: Arc::new(AtomicUsize::new(AVG_LINE_LEN_ESTIMATE)),
        line_ending: LineEnding::Lf,
        encoding: DocumentEncoding::utf8(),
        encoding_origin: DocumentEncodingOrigin::Utf8FastPath,
        encoding_engine: super::encoding_engine::engine_for_encoding(DocumentEncoding::utf8()),
        decoding_had_errors: false,
        preserve_save_error_cache: Cell::new(None),
        rope_file_len_cache: Cell::new(None),
        rope: None,
        rope_history: RopeHistory::new(),
        piece_table: Some(PieceTable::new(storage, vec![5], false)),
        dirty: false,
    };

    let _ = doc.try_insert_text_at(0, 0, "TOP\n").unwrap();

    let singular = doc.line_slice(3, 0, 16);
    let batched = doc.line_slices(3, 1, 0, 16).pop().unwrap();

    assert_eq!(singular, batched);
    assert!(!singular.is_exact());
    assert_eq!(singular.text(), "");
}

#[test]
fn partial_piece_table_find_prev_does_not_return_match_after_unresolved_before_position() {
    let dir = tempdir().unwrap();
    let path = dir
        .path()
        .join("partial-piece-table-find-prev-boundary.txt");
    let mut bytes = b"zero\n".to_vec();
    bytes.extend(std::iter::repeat_n(
        b'x',
        PARTIAL_PIECE_TABLE_SCAN_BYTES.saturating_add(32),
    ));
    bytes.extend_from_slice(b"target\n");
    bytes.extend_from_slice(b"tail target\n");
    std::fs::write(&path, &bytes).unwrap();
    let storage = FileStorage::open(&path).unwrap();

    let mut doc = Document {
        path: Some(path.clone()),
        storage: Some(storage.clone()),
        line_offsets: Arc::new(RwLock::new(LineOffsets::default())),
        disk_index: None,
        indexing: Arc::new(AtomicBool::new(false)),
        indexing_started: None,
        file_len: bytes.len(),
        indexed_bytes: Arc::new(AtomicUsize::new(0)),
        avg_line_len: Arc::new(AtomicUsize::new(AVG_LINE_LEN_ESTIMATE)),
        line_ending: LineEnding::Lf,
        encoding: DocumentEncoding::utf8(),
        encoding_origin: DocumentEncodingOrigin::Utf8FastPath,
        encoding_engine: super::encoding_engine::engine_for_encoding(DocumentEncoding::utf8()),
        decoding_had_errors: false,
        preserve_save_error_cache: Cell::new(None),
        rope_file_len_cache: Cell::new(None),
        rope: None,
        rope_history: RopeHistory::new(),
        piece_table: Some(PieceTable::new(storage, vec![5], false)),
        dirty: false,
    };

    let _ = doc.try_insert_text_at(0, 0, "TOP\n").unwrap();

    assert_eq!(
        doc.find_prev("target", TextPosition::new(2, 2)),
        None,
        "reverse search must not escape past an unresolved partial piece-table boundary"
    );
    let query = LiteralSearchQuery::new("target").unwrap();
    assert_eq!(doc.find_prev_query(&query, TextPosition::new(2, 2)), None);
}

#[test]
fn partial_piece_table_find_next_does_not_start_before_unresolved_from_position() {
    let dir = tempdir().unwrap();
    let path = dir
        .path()
        .join("partial-piece-table-find-next-boundary.txt");
    let mut bytes = b"zero\n".to_vec();
    bytes.extend(std::iter::repeat_n(
        b'x',
        PARTIAL_PIECE_TABLE_SCAN_BYTES.saturating_add(32),
    ));
    bytes.extend_from_slice(b"target\n");
    bytes.extend_from_slice(b"tail target\n");
    std::fs::write(&path, &bytes).unwrap();
    let storage = FileStorage::open(&path).unwrap();

    let mut doc = Document {
        path: Some(path.clone()),
        storage: Some(storage.clone()),
        line_offsets: Arc::new(RwLock::new(LineOffsets::default())),
        disk_index: None,
        indexing: Arc::new(AtomicBool::new(false)),
        indexing_started: None,
        file_len: bytes.len(),
        indexed_bytes: Arc::new(AtomicUsize::new(0)),
        avg_line_len: Arc::new(AtomicUsize::new(AVG_LINE_LEN_ESTIMATE)),
        line_ending: LineEnding::Lf,
        encoding: DocumentEncoding::utf8(),
        encoding_origin: DocumentEncodingOrigin::Utf8FastPath,
        encoding_engine: super::encoding_engine::engine_for_encoding(DocumentEncoding::utf8()),
        decoding_had_errors: false,
        preserve_save_error_cache: Cell::new(None),
        rope_file_len_cache: Cell::new(None),
        rope: None,
        rope_history: RopeHistory::new(),
        piece_table: Some(PieceTable::new(storage, vec![5], false)),
        dirty: false,
    };

    let _ = doc.try_insert_text_at(0, 0, "TOP\n").unwrap();

    assert_eq!(
        doc.find_next("target", TextPosition::new(3, 0)),
        None,
        "forward search must not rewind before an unresolved partial piece-table start position"
    );
    let query = LiteralSearchQuery::new("target").unwrap();
    assert_eq!(doc.find_next_query(&query, TextPosition::new(3, 0)), None);
}

#[test]
fn partial_piece_table_find_next_and_iterators_do_not_relabel_match_before_unresolved_start() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("partial-piece-table-find-next-relabel.txt");
    let mut bytes = b"zero\n".to_vec();
    bytes.extend_from_slice(b"target");
    bytes.extend(std::iter::repeat_n(
        b'x',
        PARTIAL_PIECE_TABLE_SCAN_BYTES.saturating_add(32),
    ));
    bytes.push(b'\n');
    bytes.extend_from_slice(b"tail\n");
    std::fs::write(&path, &bytes).unwrap();
    let storage = FileStorage::open(&path).unwrap();

    let mut doc = Document {
        path: Some(path.clone()),
        storage: Some(storage.clone()),
        line_offsets: Arc::new(RwLock::new(LineOffsets::default())),
        disk_index: None,
        indexing: Arc::new(AtomicBool::new(false)),
        indexing_started: None,
        file_len: bytes.len(),
        indexed_bytes: Arc::new(AtomicUsize::new(0)),
        avg_line_len: Arc::new(AtomicUsize::new(AVG_LINE_LEN_ESTIMATE)),
        line_ending: LineEnding::Lf,
        encoding: DocumentEncoding::utf8(),
        encoding_origin: DocumentEncodingOrigin::Utf8FastPath,
        encoding_engine: super::encoding_engine::engine_for_encoding(DocumentEncoding::utf8()),
        decoding_had_errors: false,
        preserve_save_error_cache: Cell::new(None),
        rope_file_len_cache: Cell::new(None),
        rope: None,
        rope_history: RopeHistory::new(),
        piece_table: Some(PieceTable::new(storage, vec![5], false)),
        dirty: false,
    };

    let _ = doc.try_insert_text_at(0, 0, "TOP\n").unwrap();

    assert_eq!(doc.find_next("target", TextPosition::new(3, 0)), None);
    let query = LiteralSearchQuery::new("target").unwrap();
    assert_eq!(doc.find_next_query(&query, TextPosition::new(3, 0)), None);
    assert_eq!(
        doc.find_all_from("target", TextPosition::new(3, 0))
            .collect::<Vec<_>>(),
        Vec::<SearchMatch>::new()
    );
    assert_eq!(
        doc.find_all_query_from(&query, TextPosition::new(3, 0))
            .collect::<Vec<_>>(),
        Vec::<SearchMatch>::new()
    );
}

#[test]
fn partial_piece_table_find_next_can_start_on_scannable_incomplete_line() {
    let dir = tempdir().unwrap();
    let path = dir
        .path()
        .join("partial-piece-table-find-next-scannable-incomplete-line.txt");
    let mut bytes = b"zero\n".to_vec();
    bytes.extend_from_slice(b"target");
    bytes.extend(std::iter::repeat_n(
        b'x',
        PARTIAL_PIECE_TABLE_SCAN_BYTES.saturating_add(32),
    ));
    std::fs::write(&path, &bytes).unwrap();
    let storage = FileStorage::open(&path).unwrap();

    let mut doc = Document {
        path: Some(path.clone()),
        storage: Some(storage.clone()),
        line_offsets: Arc::new(RwLock::new(LineOffsets::default())),
        disk_index: None,
        indexing: Arc::new(AtomicBool::new(false)),
        indexing_started: None,
        file_len: bytes.len(),
        indexed_bytes: Arc::new(AtomicUsize::new(0)),
        avg_line_len: Arc::new(AtomicUsize::new(AVG_LINE_LEN_ESTIMATE)),
        line_ending: LineEnding::Lf,
        encoding: DocumentEncoding::utf8(),
        encoding_origin: DocumentEncodingOrigin::Utf8FastPath,
        encoding_engine: super::encoding_engine::engine_for_encoding(DocumentEncoding::utf8()),
        decoding_had_errors: false,
        preserve_save_error_cache: Cell::new(None),
        rope_file_len_cache: Cell::new(None),
        rope: None,
        rope_history: RopeHistory::new(),
        piece_table: Some(PieceTable::new(storage, vec![5], false)),
        dirty: false,
    };

    let _ = doc.try_insert_text_at(0, 0, "TOP\n").unwrap();

    let visible = doc.read_text(TextRange::new(TextPosition::new(2, 0), 6));
    assert_eq!(visible.text(), "target");

    let found = doc.find_next("target", TextPosition::new(2, 0)).unwrap();
    assert_eq!(found.start(), TextPosition::new(2, 0));
    assert_eq!(found.end(), TextPosition::new(2, 6));
    let found_text = doc.read_text(found.range());
    assert_eq!(found_text.text(), "target");
    assert!(found_text.is_exact());

    let query = LiteralSearchQuery::new("target").unwrap();
    assert_eq!(
        doc.find_next_query(&query, TextPosition::new(2, 0)),
        Some(found)
    );
    assert_eq!(
        doc.find_all_from("target", TextPosition::new(2, 0))
            .collect::<Vec<_>>(),
        vec![found]
    );
    assert_eq!(
        doc.find_all_query_from(&query, TextPosition::new(2, 0))
            .collect::<Vec<_>>(),
        vec![found]
    );
}

#[test]
fn partial_piece_table_line_slice_stays_exact_on_scannable_incomplete_prefix() {
    let dir = tempdir().unwrap();
    let path = dir
        .path()
        .join("partial-piece-table-line-slice-scannable-incomplete-prefix.txt");
    let mut bytes = b"zero\n".to_vec();
    bytes.extend_from_slice(b"target");
    bytes.extend(std::iter::repeat_n(
        b'x',
        PARTIAL_PIECE_TABLE_SCAN_BYTES.saturating_add(32),
    ));
    std::fs::write(&path, &bytes).unwrap();
    let storage = FileStorage::open(&path).unwrap();

    let mut doc = Document {
        path: Some(path.clone()),
        storage: Some(storage.clone()),
        line_offsets: Arc::new(RwLock::new(LineOffsets::default())),
        disk_index: None,
        indexing: Arc::new(AtomicBool::new(false)),
        indexing_started: None,
        file_len: bytes.len(),
        indexed_bytes: Arc::new(AtomicUsize::new(0)),
        avg_line_len: Arc::new(AtomicUsize::new(AVG_LINE_LEN_ESTIMATE)),
        line_ending: LineEnding::Lf,
        encoding: DocumentEncoding::utf8(),
        encoding_origin: DocumentEncodingOrigin::Utf8FastPath,
        encoding_engine: super::encoding_engine::engine_for_encoding(DocumentEncoding::utf8()),
        decoding_had_errors: false,
        preserve_save_error_cache: Cell::new(None),
        rope_file_len_cache: Cell::new(None),
        rope: None,
        rope_history: RopeHistory::new(),
        piece_table: Some(PieceTable::new(storage, vec![5], false)),
        dirty: false,
    };

    let _ = doc.try_insert_text_at(0, 0, "TOP\n").unwrap();

    let slice = doc.line_slice(2, 0, 6);
    assert_eq!(slice.text(), "target");
    assert!(slice.is_exact());

    let batch = doc.line_slices(2, 1, 0, 6);
    assert_eq!(batch[0].text(), "target");
    assert!(batch[0].is_exact());

    let viewport = doc.read_viewport(ViewportRequest::new(2, 1).with_columns(0, 6));
    assert_eq!(viewport.rows()[0].text(), "target");
    assert!(viewport.rows()[0].is_exact());
}

#[test]
fn partial_piece_table_find_prev_can_end_on_scannable_incomplete_line() {
    let dir = tempdir().unwrap();
    let path = dir
        .path()
        .join("partial-piece-table-find-prev-scannable-incomplete-line.txt");
    let mut bytes = b"zero\n".to_vec();
    bytes.extend_from_slice(b"target");
    bytes.extend(std::iter::repeat_n(
        b'x',
        PARTIAL_PIECE_TABLE_SCAN_BYTES.saturating_add(32),
    ));
    std::fs::write(&path, &bytes).unwrap();
    let storage = FileStorage::open(&path).unwrap();

    let mut doc = Document {
        path: Some(path.clone()),
        storage: Some(storage.clone()),
        line_offsets: Arc::new(RwLock::new(LineOffsets::default())),
        disk_index: None,
        indexing: Arc::new(AtomicBool::new(false)),
        indexing_started: None,
        file_len: bytes.len(),
        indexed_bytes: Arc::new(AtomicUsize::new(0)),
        avg_line_len: Arc::new(AtomicUsize::new(AVG_LINE_LEN_ESTIMATE)),
        line_ending: LineEnding::Lf,
        encoding: DocumentEncoding::utf8(),
        encoding_origin: DocumentEncodingOrigin::Utf8FastPath,
        encoding_engine: super::encoding_engine::engine_for_encoding(DocumentEncoding::utf8()),
        decoding_had_errors: false,
        preserve_save_error_cache: Cell::new(None),
        rope_file_len_cache: Cell::new(None),
        rope: None,
        rope_history: RopeHistory::new(),
        piece_table: Some(PieceTable::new(storage, vec![5], false)),
        dirty: false,
    };

    let _ = doc.try_insert_text_at(0, 0, "TOP\n").unwrap();

    let found = doc.find_prev("target", TextPosition::new(2, 6)).unwrap();
    assert_eq!(found.start(), TextPosition::new(2, 0));
    assert_eq!(found.end(), TextPosition::new(2, 6));

    let query = LiteralSearchQuery::new("target").unwrap();
    assert_eq!(
        doc.find_prev_query(&query, TextPosition::new(2, 6)),
        Some(found)
    );
}

#[test]
fn partial_piece_table_empty_typed_reads_stay_exact_on_scannable_incomplete_line() {
    let dir = tempdir().unwrap();
    let path = dir
        .path()
        .join("partial-piece-table-empty-typed-read-scannable-incomplete-line.txt");
    let mut bytes = b"zero\n".to_vec();
    bytes.extend_from_slice(b"target");
    bytes.extend(std::iter::repeat_n(
        b'x',
        PARTIAL_PIECE_TABLE_SCAN_BYTES.saturating_add(32),
    ));
    std::fs::write(&path, &bytes).unwrap();
    let storage = FileStorage::open(&path).unwrap();

    let mut doc = Document {
        path: Some(path.clone()),
        storage: Some(storage.clone()),
        line_offsets: Arc::new(RwLock::new(LineOffsets::default())),
        disk_index: None,
        indexing: Arc::new(AtomicBool::new(false)),
        indexing_started: None,
        file_len: bytes.len(),
        indexed_bytes: Arc::new(AtomicUsize::new(0)),
        avg_line_len: Arc::new(AtomicUsize::new(AVG_LINE_LEN_ESTIMATE)),
        line_ending: LineEnding::Lf,
        encoding: DocumentEncoding::utf8(),
        encoding_origin: DocumentEncodingOrigin::Utf8FastPath,
        encoding_engine: super::encoding_engine::engine_for_encoding(DocumentEncoding::utf8()),
        decoding_had_errors: false,
        preserve_save_error_cache: Cell::new(None),
        rope_file_len_cache: Cell::new(None),
        rope: None,
        rope_history: RopeHistory::new(),
        piece_table: Some(PieceTable::new(storage, vec![5], false)),
        dirty: false,
    };

    let _ = doc.try_insert_text_at(0, 0, "TOP\n").unwrap();

    let slice = doc.read_text(TextRange::new(TextPosition::new(2, 0), 0));
    assert_eq!(slice.text(), "");
    assert!(slice.is_exact());

    let selection = doc.read_selection(TextSelection::caret(TextPosition::new(2, 0)));
    assert_eq!(selection.text(), "");
    assert!(selection.is_exact());
}

#[test]
fn selection_read_stays_exact_when_partial_piece_table_head_is_scannable_incomplete() {
    let dir = tempdir().unwrap();
    let path = dir
        .path()
        .join("selection-read-partial-piece-table-scannable-incomplete-head.txt");
    let mut bytes = b"zero\n".to_vec();
    bytes.extend_from_slice(b"target");
    bytes.extend(std::iter::repeat_n(
        b'x',
        PARTIAL_PIECE_TABLE_SCAN_BYTES.saturating_add(32),
    ));
    std::fs::write(&path, &bytes).unwrap();
    let storage = FileStorage::open(&path).unwrap();

    let mut doc = Document {
        path: Some(path.clone()),
        storage: Some(storage.clone()),
        line_offsets: Arc::new(RwLock::new(LineOffsets::default())),
        disk_index: None,
        indexing: Arc::new(AtomicBool::new(false)),
        indexing_started: None,
        file_len: bytes.len(),
        indexed_bytes: Arc::new(AtomicUsize::new(0)),
        avg_line_len: Arc::new(AtomicUsize::new(AVG_LINE_LEN_ESTIMATE)),
        line_ending: LineEnding::Lf,
        encoding: DocumentEncoding::utf8(),
        encoding_origin: DocumentEncodingOrigin::Utf8FastPath,
        encoding_engine: super::encoding_engine::engine_for_encoding(DocumentEncoding::utf8()),
        decoding_had_errors: false,
        preserve_save_error_cache: Cell::new(None),
        rope_file_len_cache: Cell::new(None),
        rope: None,
        rope_history: RopeHistory::new(),
        piece_table: Some(PieceTable::new(storage, vec![5], false)),
        dirty: false,
    };

    let _ = doc.try_insert_text_at(0, 0, "TOP\n").unwrap();

    let slice = doc.read_selection(TextSelection::new(
        TextPosition::new(1, 0),
        TextPosition::new(2, 6),
    ));
    assert_eq!(slice.text(), "zero\ntarget");
    assert!(slice.is_exact());
}

#[test]
fn typed_range_read_matches_selection_exactness_on_scannable_incomplete_partial_line() {
    let dir = tempdir().unwrap();
    let path = dir
        .path()
        .join("typed-range-read-partial-piece-table-scannable-incomplete-head.txt");
    let mut bytes = b"zero\n".to_vec();
    bytes.extend_from_slice(b"target");
    bytes.extend(std::iter::repeat_n(
        b'x',
        PARTIAL_PIECE_TABLE_SCAN_BYTES.saturating_add(32),
    ));
    std::fs::write(&path, &bytes).unwrap();
    let storage = FileStorage::open(&path).unwrap();

    let mut doc = Document {
        path: Some(path.clone()),
        storage: Some(storage.clone()),
        line_offsets: Arc::new(RwLock::new(LineOffsets::default())),
        disk_index: None,
        indexing: Arc::new(AtomicBool::new(false)),
        indexing_started: None,
        file_len: bytes.len(),
        indexed_bytes: Arc::new(AtomicUsize::new(0)),
        avg_line_len: Arc::new(AtomicUsize::new(AVG_LINE_LEN_ESTIMATE)),
        line_ending: LineEnding::Lf,
        encoding: DocumentEncoding::utf8(),
        encoding_origin: DocumentEncodingOrigin::Utf8FastPath,
        encoding_engine: super::encoding_engine::engine_for_encoding(DocumentEncoding::utf8()),
        decoding_had_errors: false,
        preserve_save_error_cache: Cell::new(None),
        rope_file_len_cache: Cell::new(None),
        rope: None,
        rope_history: RopeHistory::new(),
        piece_table: Some(PieceTable::new(storage, vec![5], false)),
        dirty: false,
    };

    let _ = doc.try_insert_text_at(0, 0, "TOP\n").unwrap();

    let selection = TextSelection::new(TextPosition::new(1, 0), TextPosition::new(2, 6));
    let selected = doc.read_selection(selection);
    let ranged = doc.read_text(doc.text_range_for_selection(selection));

    assert_eq!(ranged.text(), selected.text());
    assert_eq!(ranged.is_exact(), selected.is_exact());
    assert!(ranged.is_exact());
}

#[test]
fn partial_piece_table_bounded_search_does_not_escape_unresolved_end_position() {
    let dir = tempdir().unwrap();
    let path = dir
        .path()
        .join("partial-piece-table-bounded-search-end.txt");
    let mut bytes = b"zero\n".to_vec();
    bytes.extend(std::iter::repeat_n(
        b'x',
        PARTIAL_PIECE_TABLE_SCAN_BYTES.saturating_add(32),
    ));
    bytes.extend_from_slice(b"target\n");
    bytes.extend_from_slice(b"tail target\n");
    std::fs::write(&path, &bytes).unwrap();
    let storage = FileStorage::open(&path).unwrap();

    let mut doc = Document {
        path: Some(path.clone()),
        storage: Some(storage.clone()),
        line_offsets: Arc::new(RwLock::new(LineOffsets::default())),
        disk_index: None,
        indexing: Arc::new(AtomicBool::new(false)),
        indexing_started: None,
        file_len: bytes.len(),
        indexed_bytes: Arc::new(AtomicUsize::new(0)),
        avg_line_len: Arc::new(AtomicUsize::new(AVG_LINE_LEN_ESTIMATE)),
        line_ending: LineEnding::Lf,
        encoding: DocumentEncoding::utf8(),
        encoding_origin: DocumentEncodingOrigin::Utf8FastPath,
        encoding_engine: super::encoding_engine::engine_for_encoding(DocumentEncoding::utf8()),
        decoding_had_errors: false,
        preserve_save_error_cache: Cell::new(None),
        rope_file_len_cache: Cell::new(None),
        rope: None,
        rope_history: RopeHistory::new(),
        piece_table: Some(PieceTable::new(storage, vec![5], false)),
        dirty: false,
    };

    let _ = doc.try_insert_text_at(0, 0, "TOP\n").unwrap();

    assert_eq!(
        doc.find_all_between("target", TextPosition::new(0, 0), TextPosition::new(2, 2))
            .collect::<Vec<_>>(),
        Vec::<SearchMatch>::new(),
        "bounded search must not widen an unresolved partial piece-table end position to EOF"
    );
    let query = LiteralSearchQuery::new("target").unwrap();
    assert_eq!(
        doc.find_all_query_between(&query, TextPosition::new(0, 0), TextPosition::new(2, 2))
            .collect::<Vec<_>>(),
        Vec::<SearchMatch>::new()
    );
}

#[test]
fn partial_piece_table_bounded_search_does_not_expand_unresolved_end_line_to_eof() {
    let dir = tempdir().unwrap();
    let path = dir
        .path()
        .join("partial-piece-table-bounded-search-unresolved-line-end.txt");
    let mut bytes = b"zero\n".to_vec();
    bytes.extend(std::iter::repeat_n(
        b'x',
        PARTIAL_PIECE_TABLE_SCAN_BYTES.saturating_add(32),
    ));
    bytes.push(b'\n');
    bytes.extend_from_slice(b"target tail\n");
    std::fs::write(&path, &bytes).unwrap();
    let storage = FileStorage::open(&path).unwrap();

    let mut doc = Document {
        path: Some(path.clone()),
        storage: Some(storage.clone()),
        line_offsets: Arc::new(RwLock::new(LineOffsets::default())),
        disk_index: None,
        indexing: Arc::new(AtomicBool::new(false)),
        indexing_started: None,
        file_len: bytes.len(),
        indexed_bytes: Arc::new(AtomicUsize::new(0)),
        avg_line_len: Arc::new(AtomicUsize::new(AVG_LINE_LEN_ESTIMATE)),
        line_ending: LineEnding::Lf,
        encoding: DocumentEncoding::utf8(),
        encoding_origin: DocumentEncodingOrigin::Utf8FastPath,
        encoding_engine: super::encoding_engine::engine_for_encoding(DocumentEncoding::utf8()),
        decoding_had_errors: false,
        preserve_save_error_cache: Cell::new(None),
        rope_file_len_cache: Cell::new(None),
        rope: None,
        rope_history: RopeHistory::new(),
        piece_table: Some(PieceTable::new(storage, vec![5], false)),
        dirty: false,
    };

    let _ = doc.try_insert_text_at(0, 0, "TOP\n").unwrap();

    assert_eq!(
        doc.find_all_between("target", TextPosition::new(0, 0), TextPosition::new(3, 0))
            .collect::<Vec<_>>(),
        Vec::<SearchMatch>::new()
    );
    let query = LiteralSearchQuery::new("target").unwrap();
    assert_eq!(
        doc.find_all_query_between(&query, TextPosition::new(0, 0), TextPosition::new(3, 0))
            .collect::<Vec<_>>(),
        Vec::<SearchMatch>::new()
    );
}

#[test]
fn partial_piece_table_find_next_in_range_does_not_rewind_unresolved_start() {
    let dir = tempdir().unwrap();
    let path = dir
        .path()
        .join("partial-piece-table-find-next-in-range-start.txt");
    let mut bytes = b"zero\n".to_vec();
    bytes.extend_from_slice(b"target");
    bytes.extend(std::iter::repeat_n(
        b'x',
        PARTIAL_PIECE_TABLE_SCAN_BYTES.saturating_add(32),
    ));
    bytes.push(b'\n');
    bytes.extend_from_slice(b"tail\n");
    std::fs::write(&path, &bytes).unwrap();
    let storage = FileStorage::open(&path).unwrap();

    let mut doc = Document {
        path: Some(path.clone()),
        storage: Some(storage.clone()),
        line_offsets: Arc::new(RwLock::new(LineOffsets::default())),
        disk_index: None,
        indexing: Arc::new(AtomicBool::new(false)),
        indexing_started: None,
        file_len: bytes.len(),
        indexed_bytes: Arc::new(AtomicUsize::new(0)),
        avg_line_len: Arc::new(AtomicUsize::new(AVG_LINE_LEN_ESTIMATE)),
        line_ending: LineEnding::Lf,
        encoding: DocumentEncoding::utf8(),
        encoding_origin: DocumentEncodingOrigin::Utf8FastPath,
        encoding_engine: super::encoding_engine::engine_for_encoding(DocumentEncoding::utf8()),
        decoding_had_errors: false,
        preserve_save_error_cache: Cell::new(None),
        rope_file_len_cache: Cell::new(None),
        rope: None,
        rope_history: RopeHistory::new(),
        piece_table: Some(PieceTable::new(storage, vec![5], false)),
        dirty: false,
    };

    let _ = doc.try_insert_text_at(0, 0, "TOP\n").unwrap();

    assert_eq!(
        doc.find_next_in_range("target", TextRange::new(TextPosition::new(3, 0), 8)),
        None
    );
    let query = LiteralSearchQuery::new("target").unwrap();
    assert_eq!(
        doc.find_next_query_in_range(&query, TextRange::new(TextPosition::new(3, 0), 8)),
        None
    );
}

#[test]
fn partial_piece_table_line_slices_do_not_invent_trailing_empty_line_without_newline() {
    let dir = tempdir().unwrap();
    let path = dir
        .path()
        .join("partial-piece-table-no-trailing-newline.txt");
    let mut bytes = b"zero\n".to_vec();
    bytes.extend(std::iter::repeat_n(b'x', 64));
    std::fs::write(&path, &bytes).unwrap();
    let storage = FileStorage::open(&path).unwrap();

    let doc = Document {
        path: Some(path.clone()),
        storage: Some(storage.clone()),
        line_offsets: Arc::new(RwLock::new(LineOffsets::default())),
        disk_index: None,
        indexing: Arc::new(AtomicBool::new(false)),
        indexing_started: None,
        file_len: bytes.len(),
        indexed_bytes: Arc::new(AtomicUsize::new(0)),
        avg_line_len: Arc::new(AtomicUsize::new(AVG_LINE_LEN_ESTIMATE)),
        line_ending: LineEnding::Lf,
        encoding: DocumentEncoding::utf8(),
        encoding_origin: DocumentEncodingOrigin::Utf8FastPath,
        encoding_engine: super::encoding_engine::engine_for_encoding(DocumentEncoding::utf8()),
        decoding_had_errors: false,
        preserve_save_error_cache: Cell::new(None),
        rope_file_len_cache: Cell::new(None),
        rope: None,
        rope_history: RopeHistory::new(),
        piece_table: Some(PieceTable::new(storage, vec![5], false)),
        dirty: false,
    };

    let slices = doc.line_slices(1, 2, 0, 16);
    assert_eq!(slices[0].text(), "xxxxxxxxxxxxxxxx");
    assert!(slices[0].is_exact());
    assert_eq!(slices[1].text(), "");
    assert!(!slices[1].is_exact());

    let viewport = doc.read_viewport(ViewportRequest::new(1, 2).with_columns(0, 16));
    assert_eq!(viewport.rows()[0].text(), "xxxxxxxxxxxxxxxx");
    assert!(viewport.rows()[0].is_exact());
    assert_eq!(viewport.rows()[1].text(), "");
    assert!(!viewport.rows()[1].is_exact());
}

#[test]
fn partial_piece_table_position_helpers_do_not_fall_back_to_stale_mmap_beyond_scan_window() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("partial-piece-table-position-window.txt");
    let mut bytes = b"zero\n".to_vec();
    bytes.extend(std::iter::repeat_n(
        b'x',
        PARTIAL_PIECE_TABLE_SCAN_BYTES.saturating_add(1),
    ));
    bytes.push(b'\n');
    bytes.extend_from_slice(b"tail\n");
    std::fs::write(&path, &bytes).unwrap();
    let storage = FileStorage::open(&path).unwrap();

    let mut doc = Document {
        path: Some(path.clone()),
        storage: Some(storage.clone()),
        line_offsets: Arc::new(RwLock::new(LineOffsets::default())),
        disk_index: None,
        indexing: Arc::new(AtomicBool::new(false)),
        indexing_started: None,
        file_len: bytes.len(),
        indexed_bytes: Arc::new(AtomicUsize::new(0)),
        avg_line_len: Arc::new(AtomicUsize::new(AVG_LINE_LEN_ESTIMATE)),
        line_ending: LineEnding::Lf,
        encoding: DocumentEncoding::utf8(),
        encoding_origin: DocumentEncodingOrigin::Utf8FastPath,
        encoding_engine: super::encoding_engine::engine_for_encoding(DocumentEncoding::utf8()),
        decoding_had_errors: false,
        preserve_save_error_cache: Cell::new(None),
        rope_file_len_cache: Cell::new(None),
        rope: None,
        rope_history: RopeHistory::new(),
        piece_table: Some(PieceTable::new(storage, vec![5], false)),
        dirty: false,
    };

    let _ = doc.try_insert_text_at(0, 0, "TOP\n").unwrap();

    assert_eq!(doc.line_slice(3, 0, 16).text(), "");
    assert_eq!(doc.line_len_chars(3), 0);
    assert_eq!(
        doc.clamp_position(TextPosition::new(3, 99)),
        TextPosition::new(3, 0)
    );
}

#[test]
fn lines_iterator_yields_current_document_lines() {
    let mut doc = Document::new();
    let _ = doc.try_insert_text_at(0, 0, "zero\none\ntwo").unwrap();

    let lines: Vec<String> = doc.lines().map(LineSlice::into_text).collect();

    assert_eq!(lines, vec!["zero", "one", "two"]);
}

#[test]
fn lines_iterator_uses_known_lower_bound_while_mmap_indexing_is_incomplete() {
    let dir =
        std::env::temp_dir().join(format!("qem-doc-lines-lower-bound-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("single-line-large.bin");
    {
        let file = std::fs::File::create(&path).unwrap();
        file.set_len((INLINE_FULL_INDEX_MAX_FILE_BYTES + 1) as u64)
            .unwrap();
    }

    let doc = Document::open(&path).unwrap();
    assert!(doc.is_indexing());
    assert!(!doc.is_line_count_exact());

    let mut lines = doc.lines();
    assert_eq!(lines.len(), 1);
    assert!(lines.next().is_some());
    assert_eq!(lines.next(), None);

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn byte_progress_fraction_treats_empty_or_overreported_work_as_complete() {
    assert_eq!(ByteProgress::new(0, 0).fraction(), 1.0);
    assert_eq!(ByteProgress::new(12, 10).fraction(), 1.0);
}

#[test]
fn raw_rope_edit_helpers_clamp_out_of_range_line_indices() {
    let mut doc = Document::new();
    let _ = doc.try_insert_text_at(0, 0, "hello\nworld").unwrap();

    assert_eq!(doc.line_len_chars(99), 5);

    let cursor = doc.try_insert_text_at(99, 0, ">>").unwrap();
    assert_eq!(cursor, (1, 2));
    assert_eq!(doc.text_lossy(), "hello\n>>world");

    let cursor = doc.try_replace_range(99, 2, 3, "X").unwrap();
    assert_eq!(cursor, (1, 3));
    assert_eq!(doc.text_lossy(), "hello\n>>Xld");

    let backspace = doc.try_backspace_at(99, 3).unwrap();
    assert_eq!(backspace, (true, 1, 2));
    assert_eq!(doc.text_lossy(), "hello\n>>ld");
}

#[test]
fn replace_range_updates_rope_backed_documents() {
    let mut doc = Document::new();
    let _ = doc.try_insert_text_at(0, 0, "hello world").unwrap();

    let cursor = doc.try_replace_range(0, 6, 5, "qem").unwrap();

    assert_eq!(cursor, (0, 9));
    assert_eq!(doc.text_lossy(), "hello qem");
}

#[test]
fn replace_range_updates_piece_table_backed_documents() {
    let dir = std::env::temp_dir().join(format!("qem-piece-table-replace-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("replace.txt");
    write_disk_backed_fixture(&path);

    let mut doc = Document::open(path.clone()).unwrap();
    let cursor = doc.try_replace_range(0, 0, 3, "XYZ").unwrap();

    assert_eq!(cursor, (0, 3));
    assert!(doc.text_lossy().starts_with("XYZ\ndef\n"));

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn raw_piece_table_replace_range_clamps_out_of_range_line_indices() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("piece-table-raw-replace-clamp.txt");
    let repeat = (PIECE_TABLE_MIN_BYTES / 8).saturating_add(1);
    let mut bytes = b"abc\ndef\n".repeat(repeat);
    bytes.extend_from_slice(b"tail");
    std::fs::write(&path, &bytes).unwrap();

    let mut doc = Document::open(&path).unwrap();
    let cursor = doc.try_replace_range(usize::MAX, 2, 2, "XX").unwrap();

    let last_line0 = repeat.saturating_mul(2);
    assert_eq!(cursor, (last_line0, 4));
    assert!(doc.has_piece_table());
    assert!(doc.text_lossy().ends_with("taXX"));
}

#[test]
fn empty_insert_keeps_mmap_document_clean_and_unmaterialized() {
    let dir = std::env::temp_dir().join(format!("qem-empty-insert-noop-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("noop.txt");
    write_disk_backed_fixture(&path);

    let mut doc = Document::open(path.clone()).unwrap();
    let cursor = doc.try_insert_text_at(0, 0, "").unwrap();

    assert_eq!(cursor, (0, 0));
    assert!(!doc.is_dirty());
    assert!(!doc.has_edit_buffer());
    assert!(doc.text_lossy().starts_with("abc\ndef\n"));

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn empty_replace_keeps_mmap_document_clean_and_unmaterialized() {
    let dir = std::env::temp_dir().join(format!("qem-empty-replace-noop-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("noop.txt");
    write_disk_backed_fixture(&path);

    let mut doc = Document::open(path.clone()).unwrap();
    let cursor = doc.try_replace_range(0, 0, 0, "").unwrap();

    assert_eq!(cursor, (0, 0));
    assert!(!doc.is_dirty());
    assert!(!doc.has_edit_buffer());
    assert!(doc.text_lossy().starts_with("abc\ndef\n"));

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn replace_same_text_keeps_clean_rope_document_clean() {
    let mut doc = Document::new();
    let _ = doc.try_insert_text_at(0, 0, "hello world").unwrap();
    doc.mark_clean();

    let cursor = doc.try_replace_range(0, 6, 5, "world").unwrap();

    assert_eq!(cursor, (0, 11));
    assert_eq!(doc.text_lossy(), "hello world");
    assert!(!doc.is_dirty());
}

#[test]
fn replace_same_text_keeps_clean_piece_table_document_clean() {
    let dir = std::env::temp_dir().join(format!(
        "qem-piece-table-noop-replace-{}",
        std::process::id()
    ));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("replace.txt");
    write_disk_backed_fixture(&path);

    let mut doc = Document::open(path.clone()).unwrap();
    let _ = doc.try_insert_text_at(0, 0, "123").unwrap();
    doc.mark_clean();

    let cursor = doc.try_replace_range(0, 0, 3, "123").unwrap();

    assert_eq!(cursor, (0, 3));
    assert!(doc.text_lossy().starts_with("123abc\ndef\n"));
    assert!(!doc.is_dirty());

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn fully_indexed_short_line_mmap_uses_exact_line_count() {
    let dir = std::env::temp_dir().join(format!("qem-short-lines-exact-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("short.txt");
    std::fs::write(&path, "x\n".repeat(10_000)).unwrap();

    let doc = Document::open(&path).unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while doc.is_indexing() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }

    assert!(doc.is_fully_indexed());
    assert!(doc.has_precise_line_lengths());
    assert_eq!(doc.line_count(), LineCount::Exact(10_001));
    assert_eq!(doc.exact_line_count(), Some(10_001));

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn line_slices_near_tail_read_from_file_end_before_full_index() {
    let dir = std::env::temp_dir().join(format!("qem-tail-fast-path-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("tail.txt");
    std::fs::write(&path, b"a\nb\nc\nd\n").unwrap();
    let storage = FileStorage::open(&path).unwrap();

    let doc = Document {
        path: Some(path.clone()),
        storage: Some(storage),
        line_offsets: Arc::new(RwLock::new(LineOffsets::default())),
        disk_index: None,
        indexing: Arc::new(AtomicBool::new(true)),
        indexing_started: None,
        file_len: 8,
        indexed_bytes: Arc::new(AtomicUsize::new(0)),
        avg_line_len: Arc::new(AtomicUsize::new(2)),
        line_ending: LineEnding::Lf,
        encoding: DocumentEncoding::utf8(),
        encoding_origin: DocumentEncodingOrigin::NewDocument,
        encoding_engine: super::encoding_engine::engine_for_encoding(DocumentEncoding::utf8()),
        decoding_had_errors: false,
        preserve_save_error_cache: Cell::new(None),
        rope_file_len_cache: Cell::new(None),
        rope: None,
        rope_history: RopeHistory::new(),
        piece_table: None,
        dirty: false,
    };

    let slices = doc.line_slices(9_999, 3, 0, 16);
    let texts: Vec<String> = slices.into_iter().map(LineSlice::into_text).collect();

    assert_eq!(texts, vec!["c", "d", ""]);

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn line_slices_near_tail_do_not_invent_empty_eof_row_without_trailing_newline() {
    let dir = std::env::temp_dir().join(format!(
        "qem-tail-fast-path-no-eof-newline-{}",
        std::process::id()
    ));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("tail-no-eof-newline.txt");
    std::fs::write(&path, b"a\nb\nc\nd").unwrap();
    let storage = FileStorage::open(&path).unwrap();

    let doc = Document {
        path: Some(path.clone()),
        storage: Some(storage),
        line_offsets: Arc::new(RwLock::new(LineOffsets::default())),
        disk_index: None,
        indexing: Arc::new(AtomicBool::new(true)),
        indexing_started: None,
        file_len: 7,
        indexed_bytes: Arc::new(AtomicUsize::new(0)),
        avg_line_len: Arc::new(AtomicUsize::new(2)),
        line_ending: LineEnding::Lf,
        encoding: DocumentEncoding::utf8(),
        encoding_origin: DocumentEncodingOrigin::NewDocument,
        encoding_engine: super::encoding_engine::engine_for_encoding(DocumentEncoding::utf8()),
        decoding_had_errors: false,
        preserve_save_error_cache: Cell::new(None),
        rope_file_len_cache: Cell::new(None),
        rope: None,
        rope_history: RopeHistory::new(),
        piece_table: None,
        dirty: false,
    };

    let slices = doc.line_slices(9_999, 3, 0, 16);
    let texts: Vec<String> = slices.into_iter().map(LineSlice::into_text).collect();

    assert_eq!(texts, vec!["b", "c", "d"]);

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn trailing_tail_fast_path_bails_out_for_huge_final_line() {
    let mut bytes = b"a\nb\n".to_vec();
    bytes.resize(
        TAIL_FAST_PATH_MAX_BACKSCAN_BYTES.saturating_add(bytes.len() + 1),
        b'x',
    );

    let ranges = super::reads::trailing_mmap_line_ranges(
        &bytes,
        bytes.len(),
        3,
        TAIL_FAST_PATH_MAX_BACKSCAN_BYTES,
    );

    assert!(ranges.is_none());
}

#[test]
fn line_slices_bail_out_when_next_line_scan_would_be_unbounded() {
    let dir = std::env::temp_dir().join(format!("qem-midline-fast-path-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("midline.txt");

    let mut bytes = vec![b'a'; FALLBACK_NEXT_LINE_SCAN_BYTES + 1];
    bytes.push(b'\n');
    bytes.extend_from_slice(b"tail\n");
    std::fs::write(&path, &bytes).unwrap();
    let storage = FileStorage::open(&path).unwrap();

    let doc = Document {
        path: Some(path.clone()),
        storage: Some(storage),
        line_offsets: Arc::new(RwLock::new(LineOffsets::default())),
        disk_index: None,
        indexing: Arc::new(AtomicBool::new(true)),
        indexing_started: None,
        file_len: bytes.len(),
        indexed_bytes: Arc::new(AtomicUsize::new(0)),
        avg_line_len: Arc::new(AtomicUsize::new(1)),
        line_ending: LineEnding::Lf,
        encoding: DocumentEncoding::utf8(),
        encoding_origin: DocumentEncodingOrigin::NewDocument,
        encoding_engine: super::encoding_engine::engine_for_encoding(DocumentEncoding::utf8()),
        decoding_had_errors: false,
        preserve_save_error_cache: Cell::new(None),
        rope_file_len_cache: Cell::new(None),
        rope: None,
        rope_history: RopeHistory::new(),
        piece_table: None,
        dirty: false,
    };

    let slices = doc.line_slices(0, 3, 0, 16);
    let texts: Vec<String> = slices.iter().map(|slice| slice.text().to_owned()).collect();

    assert_eq!(texts[0], "aaaaaaaaaaaaaaaa");
    // The estimated line-1 start sits inside the first huge line, but the
    // exact scan chained from line 0's real boundary still resolves line 1
    // ("tail") and the trailing empty line within the bounded scan budget —
    // never via an unbounded scan.
    assert_eq!(texts[1], "tail");
    assert_eq!(texts[2], "");

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn indexing_progress_reports_inflight_state() {
    let doc = Document {
        path: None,
        storage: None,
        line_offsets: Arc::new(RwLock::new(LineOffsets::default())),
        disk_index: None,
        indexing: Arc::new(AtomicBool::new(true)),
        indexing_started: None,
        file_len: 128,
        indexed_bytes: Arc::new(AtomicUsize::new(32)),
        avg_line_len: Arc::new(AtomicUsize::new(AVG_LINE_LEN_ESTIMATE)),
        line_ending: LineEnding::Lf,
        encoding: DocumentEncoding::utf8(),
        encoding_origin: DocumentEncodingOrigin::NewDocument,
        encoding_engine: super::encoding_engine::engine_for_encoding(DocumentEncoding::utf8()),
        decoding_had_errors: false,
        preserve_save_error_cache: Cell::new(None),
        rope_file_len_cache: Cell::new(None),
        rope: None,
        rope_history: RopeHistory::new(),
        piece_table: None,
        dirty: false,
    };

    let progress = doc
        .indexing_state()
        .expect("typed indexing progress should exist");
    assert_eq!(progress.completed_bytes(), 32);
    assert_eq!(progress.total_bytes(), 128);
    assert_eq!(doc.indexing_state(), Some(ByteProgress::new(32, 128)));
}

#[test]
fn save_to_reopens_clean_mmap_documents() {
    let dir = std::env::temp_dir().join(format!("qem-doc-save-mmap-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let src = dir.join("source.txt");
    let dst = dir.join("copy.txt");
    std::fs::write(&src, b"alpha\nbeta\n").unwrap();

    let mut doc = Document::open(src.clone()).unwrap();
    doc.save_to(&dst).unwrap();

    assert_eq!(doc.path(), Some(dst.as_path()));
    assert_eq!(doc.mmap_bytes(), b"alpha\nbeta\n");
    assert!(!doc.is_dirty());
    assert_eq!(std::fs::read(&dst).unwrap(), b"alpha\nbeta\n");

    let _ = std::fs::remove_file(&src);
    let _ = std::fs::remove_file(&dst);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn save_to_reopens_large_piece_table_documents_clean() {
    let dir = std::env::temp_dir().join(format!("qem-doc-save-piece-table-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let src = dir.join("large.txt");
    let dst = dir.join("large-copy.txt");
    let original = b"abc\ndef\n".repeat((PIECE_TABLE_MIN_BYTES / 8) + 1);
    std::fs::write(&src, &original).unwrap();

    let mut doc = Document::open(src.clone()).unwrap();
    let _ = doc.try_insert_text_at(0, 0, "123").unwrap();
    assert!(doc.is_dirty());

    doc.save_to(&dst).unwrap();

    assert_eq!(doc.path(), Some(dst.as_path()));
    assert!(!doc.is_dirty());
    assert!(!doc.has_edit_buffer());
    assert_eq!(doc.backing(), DocumentBacking::Mmap);
    assert!(doc.text_lossy().starts_with("123abc\ndef\n"));
    assert!(std::fs::read(&dst).unwrap().starts_with(b"123abc\ndef\n"));

    let _ = std::fs::remove_file(&src);
    let _ = std::fs::remove_file(&dst);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn piece_table_save_to_rebases_future_recovery_to_saved_path() {
    let dir = std::env::temp_dir().join(format!(
        "qem-doc-save-piece-table-rebase-{}",
        std::process::id()
    ));
    let _ = std::fs::create_dir_all(&dir);
    let src = dir.join("large.txt");
    let dst = dir.join("large-copy.txt");
    write_disk_backed_fixture(&src);

    {
        let mut doc = Document::open(src.clone()).unwrap();
        let _ = doc.try_insert_text_at(0, 0, "123").unwrap();
        assert!(doc.has_piece_table());
        doc.save_to(&dst).unwrap();

        let _ = doc.try_insert_text_at(0, 0, "XYZ").unwrap();
        doc.flush_session().unwrap();

        assert!(
            !editlog_path(&src).exists(),
            "future piece-table recovery should stop targeting the old source path after save"
        );
        assert!(
            editlog_path(&dst).exists(),
            "future piece-table recovery should follow the saved path"
        );
    }

    let reopened = Document::open(dst.clone()).unwrap();

    assert!(reopened.is_dirty());
    assert!(reopened.has_piece_table());
    assert!(reopened.text_lossy().starts_with("XYZ123abc\ndef\n"));

    clear_session_sidecar(&dst);
    let _ = std::fs::remove_file(&src);
    let _ = std::fs::remove_file(&dst);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn piece_table_save_as_reopen_failure_keeps_old_recovery_sidecar() {
    let dir = tempdir().unwrap();
    let src = dir.path().join("large.txt");
    let dst = dir.path().join("large-copy.txt");
    write_disk_backed_fixture(&src);

    let mut doc = Document::open(src.clone()).unwrap();
    let _ = doc.try_insert_text_at(0, 0, "123").unwrap();
    assert!(doc.has_piece_table());
    doc.flush_session().unwrap();
    assert!(
        editlog_path(&src).exists(),
        "source sidecar should exist before forced reopen failure"
    );

    let prepared = doc.prepare_save(&dst).unwrap();
    let completion = prepared.execute(Arc::new(AtomicU64::new(0))).unwrap();
    std::fs::remove_file(&dst).unwrap();

    let err = doc
        .finish_save(
            completion.path.clone(),
            completion.reload_after_save,
            completion.encoding,
            completion.encoding_origin,
        )
        .unwrap_err();

    assert!(matches!(
        err,
        DocumentError::Open {
            path: failed_path,
            ..
        } if failed_path == dst
    ));
    assert_eq!(doc.path(), Some(src.as_path()));
    assert!(doc.is_dirty());
    assert!(doc.has_piece_table());
    assert!(doc.text_lossy().starts_with("123abc\ndef\n"));
    assert!(
        editlog_path(&src).exists(),
        "failed save-as reopen should keep the old recoverable sidecar"
    );
}

#[test]
fn piece_table_same_path_save_reopen_failure_restores_recovery_sidecar() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("large.txt");
    write_disk_backed_fixture(&path);

    let mut doc = Document::open(path.clone()).unwrap();
    let _ = doc.try_insert_text_at(0, 0, "123").unwrap();
    assert!(doc.has_piece_table());
    doc.flush_session().unwrap();
    let sidecar = editlog_path(&path);
    let original_sidecar = std::fs::read(&sidecar).unwrap();

    let prepared = doc.prepare_save(&path).unwrap();
    let completion = prepared.execute(Arc::new(AtomicU64::new(0))).unwrap();
    std::fs::remove_file(&path).unwrap();

    let err = doc
        .finish_save(
            completion.path.clone(),
            completion.reload_after_save,
            completion.encoding,
            completion.encoding_origin,
        )
        .unwrap_err();

    assert!(matches!(
        err,
        DocumentError::Open {
            path: failed_path,
            ..
        } if failed_path == path
    ));
    assert_eq!(doc.path(), Some(path.as_path()));
    assert!(doc.is_dirty());
    assert!(doc.has_piece_table());
    assert!(doc.text_lossy().starts_with("123abc\ndef\n"));
    assert!(
        sidecar.exists(),
        "failed same-path reopen should restore recoverable sidecar"
    );
    assert_eq!(std::fs::read(&sidecar).unwrap(), original_sidecar);
}

#[test]
fn large_piece_table_non_utf8_save_is_rejected_before_write() {
    let dir = tempdir().unwrap();
    let src = dir.path().join("huge-source.txt");
    let dst = dir.path().join("huge-cp1251.txt");
    let encoding = DocumentEncoding::from_label("windows-1251").unwrap();

    let mut file = std::fs::File::create(&src).unwrap();
    use std::io::Write as _;
    file.write_all(b"line\n").unwrap();
    file.set_len((MAX_ROPE_EDIT_FILE_BYTES + 1) as u64).unwrap();
    drop(file);

    let mut doc = Document::open(src.clone()).unwrap();
    let _ = doc.try_insert_text_at(0, 0, "X").unwrap();
    assert!(doc.has_piece_table());

    let err = doc.save_to_with_encoding(&dst, encoding).unwrap_err();
    assert!(matches!(
        err,
        DocumentError::Encoding {
            path: failed_path,
            operation: "save",
            encoding: failed_encoding,
            reason: DocumentEncodingErrorKind::SaveReopenTooLarge { max_bytes },
        } if failed_path == dst
            && failed_encoding == encoding
            && max_bytes == MAX_ROPE_EDIT_FILE_BYTES
    ));
    assert!(
        !dst.exists(),
        "rejected save must not write a partial destination"
    );
    assert_eq!(doc.path(), Some(src.as_path()));
    assert!(doc.is_dirty());
    assert!(doc.has_piece_table());
}

#[test]
fn large_piece_table_non_utf8_save_preflight_reports_reopen_limit() {
    let dir = tempdir().unwrap();
    let src = dir.path().join("huge-source.txt");
    let encoding = DocumentEncoding::from_label("windows-1251").unwrap();

    let mut file = std::fs::File::create(&src).unwrap();
    use std::io::Write as _;
    file.write_all(b"line\n").unwrap();
    file.set_len((MAX_ROPE_EDIT_FILE_BYTES + 1) as u64).unwrap();
    drop(file);

    let mut doc = Document::open(src).unwrap();
    let _ = doc.try_insert_text_at(0, 0, "X").unwrap();
    assert!(doc.has_piece_table());

    assert_eq!(
        doc.save_error_for_encoding(encoding),
        Some(DocumentEncodingErrorKind::SaveReopenTooLarge {
            max_bytes: MAX_ROPE_EDIT_FILE_BYTES,
        })
    );
    assert!(!doc.can_save_with_encoding(encoding));
}

#[test]
fn piece_table_save_to_with_encoding_reopens_as_class_a_native_contract() {
    let dir = std::env::temp_dir().join(format!(
        "qem-doc-save-piece-table-convert-{}",
        std::process::id()
    ));
    let _ = std::fs::create_dir_all(&dir);
    let src = dir.join("large.txt");
    let dst = dir.join("large-cp1251.txt");
    let encoding = DocumentEncoding::from_label("windows-1251").unwrap();
    let insert_hello = "\u{043F}\u{0440}\u{0438}\u{0432}\u{0435}\u{0442}\n";
    let insert_world = "\u{043C}\u{0438}\u{0440}\n";
    write_disk_backed_fixture(&src);

    let mut doc = Document::open(src.clone()).unwrap();
    let _ = doc.try_insert_text_at(0, 0, insert_hello).unwrap();
    assert!(doc.has_piece_table());

    doc.save_to_with_encoding(&dst, encoding).unwrap();

    assert_eq!(doc.path(), Some(dst.as_path()));
    assert_eq!(doc.encoding(), encoding);
    assert_eq!(
        doc.encoding_origin(),
        DocumentEncodingOrigin::SaveConversion
    );
    assert!(!doc.is_dirty());
    // windows-1251 (Class A) re-opens through the native mmap
    // path with no rope or piece-table backing.
    assert!(!doc.has_rope() && !doc.has_piece_table() && doc.encoding() == encoding);
    assert!(doc.text_lossy().starts_with(insert_hello));
    assert!(!editlog_path(&src).exists());
    assert!(!editlog_path(&dst).exists());

    let raw = std::fs::read(&dst).unwrap();
    let (decoded, used, had_errors) = WINDOWS_1251.decode(&raw);
    assert_eq!(used, WINDOWS_1251);
    assert!(!had_errors);
    assert!(decoded.starts_with(insert_hello));

    let _ = doc.try_insert_text_at(0, 0, insert_world).unwrap();
    doc.save_to(&dst).unwrap();

    let raw = std::fs::read(&dst).unwrap();
    let (decoded, used, had_errors) = WINDOWS_1251.decode(&raw);
    assert_eq!(used, WINDOWS_1251);
    assert!(!had_errors);
    assert!(decoded.starts_with(&format!("{insert_world}{insert_hello}")));

    let _ = std::fs::remove_file(&src);
    let _ = std::fs::remove_file(&dst);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn open_recovers_piece_table_session_from_editlog() {
    let dir = std::env::temp_dir().join(format!("qem-doc-session-open-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("session.txt");
    write_disk_backed_fixture(&path);

    {
        let mut doc = Document::open(path.clone()).unwrap();
        let _ = doc.try_insert_text_at(0, 0, "123").unwrap();
        doc.flush_session().unwrap();
    }

    let recovered = Document::open(path.clone()).unwrap();

    assert!(recovered.is_dirty());
    assert!(recovered.has_edit_buffer());
    assert_eq!(
        recovered.encoding_origin(),
        DocumentEncodingOrigin::Utf8FastPath
    );
    assert!(recovered.text_lossy().starts_with("123abc\ndef\n"));

    clear_session_sidecar(&path);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn recovered_piece_table_session_preserves_utf8_autodetect_origin() {
    let dir = std::env::temp_dir().join(format!(
        "qem-doc-session-autodetect-origin-{}",
        std::process::id()
    ));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("session.txt");
    write_disk_backed_fixture(&path);

    {
        let mut doc = Document::open_with_auto_encoding_detection(path.clone()).unwrap();
        assert_eq!(
            doc.encoding_origin(),
            DocumentEncodingOrigin::AutoDetectFallbackUtf8
        );
        let _ = doc.try_insert_text_at(0, 0, "123").unwrap();
        doc.flush_session().unwrap();
    }

    let recovered = Document::open(path.clone()).unwrap();

    assert!(recovered.is_dirty());
    assert!(recovered.has_piece_table());
    assert_eq!(recovered.encoding(), DocumentEncoding::utf8());
    assert_eq!(
        recovered.encoding_origin(),
        DocumentEncodingOrigin::AutoDetectFallbackUtf8
    );
    assert!(!recovered.decoding_had_errors());
    assert!(recovered.text_lossy().starts_with("123abc\ndef\n"));

    clear_session_sidecar(&path);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn recovered_piece_table_session_preserves_utf8_save_conversion_origin() {
    let dir = std::env::temp_dir().join(format!(
        "qem-doc-session-save-conversion-origin-{}",
        std::process::id()
    ));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("session.txt");
    write_disk_backed_fixture(&path);

    {
        let mut doc = Document::open(path.clone()).unwrap();
        doc.save_to_with_encoding(&path, DocumentEncoding::utf8())
            .unwrap();
        assert_eq!(
            doc.encoding_origin(),
            DocumentEncodingOrigin::SaveConversion
        );
        let _ = doc.try_insert_text_at(0, 0, "123").unwrap();
        doc.flush_session().unwrap();
    }

    let recovered = Document::open(path.clone()).unwrap();

    assert!(recovered.is_dirty());
    assert!(recovered.has_piece_table());
    assert_eq!(recovered.encoding(), DocumentEncoding::utf8());
    assert_eq!(
        recovered.encoding_origin(),
        DocumentEncodingOrigin::SaveConversion
    );
    assert!(!recovered.decoding_had_errors());
    assert!(recovered.text_lossy().starts_with("123abc\ndef\n"));

    clear_session_sidecar(&path);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn open_discards_corrupt_editlog_sidecar_and_falls_back_to_clean_document() {
    let dir = std::env::temp_dir().join(format!("qem-doc-session-corrupt-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("session.txt");
    write_disk_backed_fixture(&path);

    {
        let mut doc = Document::open(path.clone()).unwrap();
        let _ = doc.try_insert_text_at(0, 0, "123").unwrap();
        doc.flush_session().unwrap();
    }

    let sidecar = editlog_path(&path);
    assert!(sidecar.exists(), "expected persisted editlog sidecar");
    std::fs::write(&sidecar, b"broken-qem-editlog").unwrap();

    let reopened = Document::open(path.clone()).unwrap();

    assert_eq!(reopened.path(), Some(path.as_path()));
    assert!(!reopened.is_dirty());
    assert!(!reopened.has_edit_buffer());
    assert!(!reopened.has_piece_table());
    assert_eq!(reopened.backing(), DocumentBacking::Mmap);
    assert!(
        reopened.text_lossy().starts_with("abc\ndef\n"),
        "corrupt session should fall back to the clean source text"
    );
    assert!(
        !sidecar.exists(),
        "corrupt internal sidecar should be removed after failed recovery"
    );

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn open_with_progress_reports_partial_large_file_inspection_before_completion() {
    let dir = std::env::temp_dir().join(format!("qem-open-progress-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("open-progress.txt");

    let large_len = INLINE_FULL_INDEX_MAX_FILE_BYTES + 256 * 1024;
    let mut bytes = Vec::with_capacity(large_len);
    bytes.extend_from_slice(b"a\n");
    bytes.extend(std::iter::repeat_n(
        b'x',
        large_len.saturating_sub(bytes.len()),
    ));
    std::fs::write(&path, &bytes).unwrap();

    let mut reported = Vec::new();
    let doc =
        Document::open_with_progress(path.clone(), |completed| reported.push(completed)).unwrap();
    let total = doc.file_len() as u64;

    assert_eq!(reported.last().copied(), Some(total));
    assert!(
        reported.windows(2).all(|pair| pair[0] <= pair[1]),
        "open progress must be monotonic: {reported:?}"
    );
    assert!(
        reported.iter().any(|&value| value > 0 && value < total),
        "expected an intermediate partial progress update before completion: {reported:?}"
    );

    clear_session_sidecar(&path);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn open_with_reporting_exposes_expected_large_file_phases() {
    let dir = std::env::temp_dir().join(format!("qem-open-phases-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("open-phases.txt");

    let large_len = INLINE_FULL_INDEX_MAX_FILE_BYTES + 256 * 1024;
    let mut bytes = Vec::with_capacity(large_len);
    bytes.extend_from_slice(b"a\n");
    bytes.extend(std::iter::repeat_n(
        b'x',
        large_len.saturating_sub(bytes.len()),
    ));
    std::fs::write(&path, &bytes).unwrap();

    let mut phases = Vec::new();
    let doc = Document::open_with_reporting(path.clone(), |_| {}, &mut |phase| phases.push(phase))
        .unwrap();

    let phase_rank = |phase: OpenProgressPhase| match phase {
        OpenProgressPhase::OpeningStorage => 0u8,
        OpenProgressPhase::InspectingSource => 1,
        OpenProgressPhase::PreparingIndex => 2,
        OpenProgressPhase::RecoveringSession => 3,
        OpenProgressPhase::Ready => 4,
    };

    assert_eq!(
        phases.first().copied(),
        Some(OpenProgressPhase::OpeningStorage)
    );
    assert_eq!(phases.last().copied(), Some(OpenProgressPhase::Ready));
    assert!(
        phases
            .windows(2)
            .all(|pair| phase_rank(pair[0]) <= phase_rank(pair[1])),
        "open phases must be monotonic: {phases:?}"
    );
    assert!(phases.contains(&OpenProgressPhase::InspectingSource));
    assert!(phases.contains(&OpenProgressPhase::PreparingIndex));
    assert!(phases.contains(&OpenProgressPhase::RecoveringSession));
    assert_eq!(doc.file_len(), large_len);

    clear_session_sidecar(&path);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn editlog_flush_failure_surfaces_error_and_falls_back_to_memory() {
    let dir = std::env::temp_dir().join(format!("qem-doc-session-failure-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("failure.txt");
    write_disk_backed_fixture(&path);

    let mut doc = Document::open(path.clone()).unwrap();
    let _ = doc.try_insert_text_at(0, 0, "123").unwrap();
    doc.piece_table
        .as_mut()
        .expect("piece table expected")
        .pieces
        .poison_persistence_for_test();

    let err = doc.flush_session();
    assert!(matches!(err, Err(DocumentError::Write { .. })));

    let _ = doc.try_insert_text_at(0, 3, "X").unwrap();
    assert!(doc.text_lossy().starts_with("123Xabc\ndef\n"));
    assert!(
        doc.flush_session().is_ok(),
        "in-memory fallback should stop retrying failed persistence"
    );

    clear_session_sidecar(&path);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn scheduled_editlog_flush_failure_does_not_rollback_piece_table_edit() {
    let dir = std::env::temp_dir().join(format!(
        "qem-doc-session-scheduled-failure-{}",
        std::process::id()
    ));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("scheduled-failure.txt");
    write_disk_backed_fixture(&path);

    let mut doc = Document::open(path.clone()).unwrap();
    let _ = doc.try_insert_text_at(0, 0, "123").unwrap();
    let piece_table = doc.piece_table.as_mut().expect("piece table expected");
    piece_table.pieces.poison_persistence_for_test();
    piece_table.last_session_flush = Instant::now().checked_sub(Duration::from_secs(1));

    let cursor = doc.try_insert_text_at(0, 3, "X").unwrap();
    assert_eq!(cursor, (0, 4));
    assert!(doc.text_lossy().starts_with("123Xabc\ndef\n"));
    assert!(
        doc.flush_session().is_ok(),
        "scheduled flush failure should already detach persistence and keep future flushes in-memory"
    );

    clear_session_sidecar(&path);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn recovered_piece_table_session_supports_undo_and_redo() {
    let dir = std::env::temp_dir().join(format!("qem-doc-session-history-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("history.txt");
    write_disk_backed_fixture(&path);

    {
        let mut doc = Document::open(path.clone()).unwrap();
        let _ = doc.try_insert_text_at(0, 0, "123").unwrap();
        doc.flush_session().unwrap();
    }

    let mut recovered = Document::open(path.clone()).unwrap();
    assert!(recovered.try_undo().unwrap());
    assert!(recovered.text_lossy().starts_with("abc\ndef\n"));
    assert!(recovered.try_redo().unwrap());
    assert!(recovered.text_lossy().starts_with("123abc\ndef\n"));

    clear_session_sidecar(&path);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn save_clears_recoverable_piece_table_session() {
    let dir = std::env::temp_dir().join(format!("qem-doc-session-save-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("saved.txt");
    write_disk_backed_fixture(&path);

    {
        let mut doc = Document::open(path.clone()).unwrap();
        let _ = doc.try_insert_text_at(0, 0, "123").unwrap();
        doc.flush_session().unwrap();
        doc.save_to(&path).unwrap();
    }

    let reopened = Document::open(path.clone()).unwrap();

    assert!(!reopened.is_dirty());
    assert!(!reopened.has_edit_buffer());
    assert!(std::fs::read(&path).unwrap().starts_with(b"123abc\ndef\n"));

    clear_session_sidecar(&path);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn try_insert_rejects_large_mmap_rope_fallback() {
    let dir =
        std::env::temp_dir().join(format!("qem-doc-large-edit-reject-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("huge.bin");
    let file = std::fs::File::create(&path).unwrap();
    file.set_len((MAX_ROPE_EDIT_FILE_BYTES + 1) as u64).unwrap();
    drop(file);

    let mut doc = Document::open(path.clone()).unwrap();
    let err = doc.try_insert_text_at(LINE_LENGTHS_MAX_SYNC_LINES + 1, 0, "x");

    assert!(matches!(err, Err(DocumentError::EditUnsupported { .. })));
    assert!(!doc.has_edit_buffer());

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn try_insert_rejects_large_piece_table_promotion_to_rope() {
    let dir = std::env::temp_dir().join(format!(
        "qem-doc-piece-table-promotion-reject-{}",
        std::process::id()
    ));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("small.txt");
    std::fs::write(&path, b"x").unwrap();
    let storage = FileStorage::open(&path).unwrap();

    let mut doc = Document {
        path: Some(path.clone()),
        storage: Some(storage.clone()),
        line_offsets: Arc::new(RwLock::new(LineOffsets::default())),
        disk_index: None,
        indexing: Arc::new(AtomicBool::new(false)),
        indexing_started: None,
        file_len: MAX_ROPE_EDIT_FILE_BYTES + 1,
        indexed_bytes: Arc::new(AtomicUsize::new(0)),
        avg_line_len: Arc::new(AtomicUsize::new(AVG_LINE_LEN_ESTIMATE)),
        line_ending: LineEnding::Lf,
        encoding: DocumentEncoding::utf8(),
        encoding_origin: DocumentEncodingOrigin::NewDocument,
        encoding_engine: super::encoding_engine::engine_for_encoding(DocumentEncoding::utf8()),
        decoding_had_errors: false,
        preserve_save_error_cache: Cell::new(None),
        rope_file_len_cache: Cell::new(None),
        rope: None,
        rope_history: RopeHistory::new(),
        piece_table: Some(PieceTable {
            original: storage,
            add: Vec::new(),
            pieces: PieceTree::from_pieces(vec![Piece {
                src: PieceSource::Original,
                start: 0,
                len: 1,
                line_breaks: 0,
            }]),
            engine: encoding_engine::engine_for_encoding(DocumentEncoding::utf8()),
            known_line_count: 1,
            exact_base_line_breaks: None,
            exact_base_total_lines: None,
            known_byte_len: 1,
            total_len: MAX_ROPE_EDIT_FILE_BYTES + 1,
            full_index: false,
            encoding_origin: DocumentEncodingOrigin::Utf8FastPath,
            decoding_had_errors: false,
            pending_session_flush: false,
            pending_session_edits: 0,
            last_session_flush: None,
            edit_batch_depth: 0,
            edit_batch_dirty: false,
        }),
        dirty: true,
    };

    let err = doc.try_insert_text_at(1, 0, "x");

    assert!(matches!(err, Err(DocumentError::EditUnsupported { .. })));
    assert!(doc.rope.is_none());
    assert!(doc.piece_table.is_some());

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn small_files_index_inline_and_become_precise_immediately() {
    let dir = std::env::temp_dir().join(format!("qem-doc-inline-index-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("small.txt");
    std::fs::write(&path, b"zero\none\ntwo\n").unwrap();

    let doc = Document::open(&path).unwrap();

    assert!(!doc.is_indexing());
    assert!(doc.is_fully_indexed());
    assert!(doc.has_precise_line_lengths());
    assert_eq!(
        doc.indexed_bytes(),
        std::fs::metadata(&path).unwrap().len() as usize
    );
    assert_eq!(doc.line_count(), LineCount::Exact(4));
    assert_eq!(doc.line_slice(2, 0, 16).text(), "two");

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn document_exposes_detected_line_ending_style() {
    let dir = std::env::temp_dir().join(format!("qem-doc-line-ending-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("crlf.txt");
    std::fs::write(&path, b"alpha\r\nbeta\r\n").unwrap();

    let doc = Document::open(&path).unwrap();

    assert_eq!(doc.line_ending(), LineEnding::Crlf);
    assert_eq!(doc.line_ending().as_str(), "\r\n");

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn display_line_count_helpers_follow_exactness() {
    let mut doc = Document::new();
    assert_eq!(doc.display_line_count(), 1);
    assert!(doc.is_line_count_exact());

    let _ = doc.try_insert_text_at(0, 0, "one\ntwo").unwrap();

    assert_eq!(doc.display_line_count(), 2);
    assert!(doc.is_line_count_exact());
    assert_eq!(doc.line_count(), LineCount::Exact(2));
}

#[test]
fn typed_viewport_api_returns_rows_with_metadata() {
    let mut doc = Document::new();
    let _ = doc
        .try_insert(TextPosition::new(0, 0), "zero\none\ntwo\n")
        .unwrap();

    let viewport = doc.read_viewport(ViewportRequest::new(1, 2).with_columns(0, 16));

    assert_eq!(viewport.total_lines(), LineCount::Exact(4));
    assert_eq!(viewport.len(), 2);
    assert_eq!(viewport.rows()[0].line0(), 1);
    assert_eq!(viewport.rows()[0].line_number(), 2);
    assert_eq!(viewport.rows()[0].text(), "one");
    assert!(viewport.rows()[0].is_exact());
    assert_eq!(viewport.rows()[1].text(), "two");
}

#[test]
fn typed_edit_helpers_wrap_existing_edit_semantics() {
    let mut doc = Document::new();
    let cursor = doc
        .try_insert(TextPosition::new(0, 0), "alpha\nbeta")
        .unwrap();

    assert_eq!(cursor, TextPosition::new(1, 4));

    let cursor = doc
        .try_replace(TextRange::new(TextPosition::new(1, 0), 4), "BETA")
        .unwrap();
    assert_eq!(cursor, TextPosition::new(1, 4));

    let result = doc.try_backspace(TextPosition::new(1, 4)).unwrap();
    assert!(result.changed());
    assert_eq!(result.cursor(), TextPosition::new(1, 3));
    assert_eq!(doc.text_lossy(), "alpha\nBET");
}

#[test]
fn typed_delete_forward_wraps_existing_edit_semantics() {
    let mut doc = Document::new();
    let _ = doc
        .try_insert(TextPosition::new(0, 0), "alpha\nbeta")
        .unwrap();

    let result = doc.try_delete_forward(TextPosition::new(0, 5)).unwrap();
    assert!(result.changed());
    assert_eq!(result.cursor(), TextPosition::new(0, 5));
    assert_eq!(doc.text_lossy(), "alphabeta");

    let at_end = doc.try_delete_forward(TextPosition::new(0, 9)).unwrap();
    assert!(!at_end.changed());
    assert_eq!(at_end.cursor(), TextPosition::new(0, 9));
}

#[test]
fn typed_selection_helpers_build_ordered_ranges() {
    let mut doc = Document::new();
    let _ = doc
        .try_insert(TextPosition::new(0, 0), "alpha\nbeta\ngamma")
        .unwrap();

    let (start, end) = doc.ordered_positions(TextPosition::new(2, 3), TextPosition::new(0, 2));
    assert_eq!(start, TextPosition::new(0, 2));
    assert_eq!(end, TextPosition::new(2, 3));

    let range = doc.text_range_between(TextPosition::new(2, 3), TextPosition::new(0, 2));
    assert_eq!(range.start(), TextPosition::new(0, 2));
    assert_eq!(range.len_chars(), 12);
}

#[test]
fn typed_selection_helpers_treat_crlf_as_single_text_unit() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("crlf-range.txt");
    std::fs::write(&path, b"a\r\nb\r\n").unwrap();

    let doc = Document::open(&path).unwrap();

    let range = doc.text_range_between(TextPosition::new(0, 1), TextPosition::new(1, 1));
    assert_eq!(range.start(), TextPosition::new(0, 1));
    assert_eq!(doc.char_index_for_position(TextPosition::new(1, 0)), 2);
    assert_eq!(doc.char_index_for_position(TextPosition::new(1, 1)), 3);
    assert_eq!(doc.position_for_char_index(2), TextPosition::new(1, 0));
    assert_eq!(
        doc.text_units_between(TextPosition::new(0, 1), TextPosition::new(1, 1)),
        2
    );
    assert_eq!(range.len_chars(), 2);
}

#[test]
fn partial_piece_table_position_helpers_follow_current_text_after_prefix_edit() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("partial-piece-table-positions.txt");
    std::fs::write(&path, b"zero\none\ntwo\nthree\n").unwrap();
    let storage = FileStorage::open(&path).unwrap();

    let mut doc = Document {
        path: Some(path.clone()),
        storage: Some(storage.clone()),
        line_offsets: Arc::new(RwLock::new(LineOffsets::default())),
        disk_index: None,
        indexing: Arc::new(AtomicBool::new(false)),
        indexing_started: None,
        file_len: std::fs::metadata(&path).unwrap().len() as usize,
        indexed_bytes: Arc::new(AtomicUsize::new(0)),
        avg_line_len: Arc::new(AtomicUsize::new(4)),
        line_ending: LineEnding::Lf,
        encoding: DocumentEncoding::utf8(),
        encoding_origin: DocumentEncodingOrigin::Utf8FastPath,
        encoding_engine: super::encoding_engine::engine_for_encoding(DocumentEncoding::utf8()),
        decoding_had_errors: false,
        preserve_save_error_cache: Cell::new(None),
        rope_file_len_cache: Cell::new(None),
        rope: None,
        rope_history: RopeHistory::new(),
        piece_table: Some(PieceTable::new(storage, vec![5, 4], false)),
        dirty: false,
    };

    let _ = doc.try_insert_text_at(0, 0, "TOP\n").unwrap();

    assert_eq!(doc.line_len_chars(3), 3);
    assert_eq!(
        doc.clamp_position(TextPosition::new(3, 99)),
        TextPosition::new(3, 3)
    );
    assert_eq!(doc.char_index_for_position(TextPosition::new(3, 2)), 15);
    assert_eq!(
        doc.text_units_between(TextPosition::new(3, 1), TextPosition::new(4, 2)),
        5
    );

    let selection = TextSelection::new(TextPosition::new(4, 2), TextPosition::new(3, 1));
    let range = doc.text_range_for_selection(selection);
    assert_eq!(range.start(), TextPosition::new(3, 1));
    assert_eq!(range.len_chars(), 5);

    let selected = doc.read_selection(selection);
    assert!(selected.is_exact());
    assert_eq!(selected.text(), "wo\nth");
}

#[test]
fn partial_piece_table_clamp_position_preserves_scannable_lines_beyond_estimated_display_count() {
    let dir = tempdir().unwrap();
    let path = dir
        .path()
        .join("partial-piece-table-clamp-scannable-lines.txt");
    std::fs::write(&path, b"a\nb\nc\n").unwrap();
    let storage = FileStorage::open(&path).unwrap();

    let doc = Document {
        path: Some(path.clone()),
        storage: Some(storage.clone()),
        line_offsets: Arc::new(RwLock::new(LineOffsets::default())),
        disk_index: None,
        indexing: Arc::new(AtomicBool::new(false)),
        indexing_started: None,
        file_len: std::fs::metadata(&path).unwrap().len() as usize,
        indexed_bytes: Arc::new(AtomicUsize::new(0)),
        avg_line_len: Arc::new(AtomicUsize::new(128)),
        line_ending: LineEnding::Lf,
        encoding: DocumentEncoding::utf8(),
        encoding_origin: DocumentEncodingOrigin::Utf8FastPath,
        encoding_engine: super::encoding_engine::engine_for_encoding(DocumentEncoding::utf8()),
        decoding_had_errors: false,
        preserve_save_error_cache: Cell::new(None),
        rope_file_len_cache: Cell::new(None),
        rope: None,
        rope_history: RopeHistory::new(),
        piece_table: Some(PieceTable::new(storage, vec![2], false)),
        dirty: false,
    };

    // The 1-line window no longer masquerades as the display count: the
    // estimate extrapolates the tail from the window's own average line
    // length (2 bytes/line over a 6-byte file), which happens to be exact
    // for this fixture.
    assert_eq!(doc.display_line_count(), 3);
    assert!(!doc.is_line_count_exact());
    assert_eq!(doc.line_slice(2, 0, 16).text(), "c");
    assert!(doc.line_slice(2, 0, 16).is_exact());

    assert_eq!(
        doc.clamp_position(TextPosition::new(2, 99)),
        TextPosition::new(2, 1)
    );

    let slice = doc.read_text(TextRange::new(TextPosition::new(2, 0), 1));
    assert_eq!(slice.text(), "c");
    assert!(slice.is_exact());
}

#[test]
fn partial_piece_table_position_helpers_do_not_invent_text_units_past_safe_boundary() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("partial-piece-table-position-tail.txt");
    let mut bytes = b"zero\n".to_vec();
    bytes.extend(std::iter::repeat_n(
        b'x',
        PARTIAL_PIECE_TABLE_SCAN_BYTES.saturating_add(32),
    ));
    std::fs::write(&path, &bytes).unwrap();
    let storage = FileStorage::open(&path).unwrap();

    let doc = Document {
        path: Some(path.clone()),
        storage: Some(storage.clone()),
        line_offsets: Arc::new(RwLock::new(LineOffsets::default())),
        disk_index: None,
        indexing: Arc::new(AtomicBool::new(false)),
        indexing_started: None,
        file_len: bytes.len(),
        indexed_bytes: Arc::new(AtomicUsize::new(0)),
        avg_line_len: Arc::new(AtomicUsize::new(1)),
        line_ending: LineEnding::Lf,
        encoding: DocumentEncoding::utf8(),
        encoding_origin: DocumentEncodingOrigin::Utf8FastPath,
        encoding_engine: super::encoding_engine::engine_for_encoding(DocumentEncoding::utf8()),
        decoding_had_errors: false,
        preserve_save_error_cache: Cell::new(None),
        rope_file_len_cache: Cell::new(None),
        rope: None,
        rope_history: RopeHistory::new(),
        piece_table: Some(PieceTable::new(storage, vec![5], false)),
        dirty: false,
    };

    assert_eq!(doc.char_index_for_position(TextPosition::new(99, 0)), 5);
    assert_eq!(
        doc.text_units_between(TextPosition::new(0, 0), TextPosition::new(99, 0)),
        5
    );
    assert_eq!(
        doc.text_range_between(TextPosition::new(0, 0), TextPosition::new(99, 0)),
        TextRange::new(TextPosition::new(0, 0), 5)
    );
}

#[test]
fn incomplete_mmap_position_helpers_clamp_to_eof_instead_of_inventing_lines() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("incomplete-mmap-position-tail.txt");
    std::fs::write(&path, b"zero\n").unwrap();
    let storage = FileStorage::open(&path).unwrap();

    let doc = Document {
        path: Some(path.clone()),
        storage: Some(storage.clone()),
        line_offsets: Arc::new(RwLock::new(LineOffsets::default())),
        disk_index: None,
        indexing: Arc::new(AtomicBool::new(false)),
        indexing_started: None,
        file_len: 5,
        indexed_bytes: Arc::new(AtomicUsize::new(0)),
        avg_line_len: Arc::new(AtomicUsize::new(1)),
        line_ending: LineEnding::Lf,
        encoding: DocumentEncoding::utf8(),
        encoding_origin: DocumentEncodingOrigin::Utf8FastPath,
        encoding_engine: super::encoding_engine::engine_for_encoding(DocumentEncoding::utf8()),
        decoding_had_errors: false,
        preserve_save_error_cache: Cell::new(None),
        rope_file_len_cache: Cell::new(None),
        rope: None,
        rope_history: RopeHistory::new(),
        piece_table: None,
        dirty: false,
    };

    assert_eq!(doc.char_index_for_position(TextPosition::new(99, 0)), 5);
    assert_eq!(
        doc.text_units_between(TextPosition::new(0, 0), TextPosition::new(99, 0)),
        5
    );
    assert_eq!(
        doc.text_range_between(TextPosition::new(0, 0), TextPosition::new(99, 0)),
        TextRange::new(TextPosition::new(0, 0), 5)
    );
}

#[test]
fn incomplete_mmap_line_len_chars_does_not_invent_tail_columns() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("incomplete-mmap-line-len-tail.txt");
    std::fs::write(&path, b"zero\n").unwrap();
    let storage = FileStorage::open(&path).unwrap();

    let doc = Document {
        path: Some(path.clone()),
        storage: Some(storage.clone()),
        line_offsets: Arc::new(RwLock::new(LineOffsets::default())),
        disk_index: None,
        indexing: Arc::new(AtomicBool::new(false)),
        indexing_started: None,
        file_len: 5,
        indexed_bytes: Arc::new(AtomicUsize::new(0)),
        avg_line_len: Arc::new(AtomicUsize::new(1)),
        line_ending: LineEnding::Lf,
        encoding: DocumentEncoding::utf8(),
        encoding_origin: DocumentEncodingOrigin::Utf8FastPath,
        encoding_engine: super::encoding_engine::engine_for_encoding(DocumentEncoding::utf8()),
        decoding_had_errors: false,
        preserve_save_error_cache: Cell::new(None),
        rope_file_len_cache: Cell::new(None),
        rope: None,
        rope_history: RopeHistory::new(),
        piece_table: None,
        dirty: false,
    };

    assert_eq!(doc.line_len_chars(0), 4);
    assert_eq!(doc.line_len_chars(1), 0);
    assert_eq!(doc.line_len_chars(99), 0);
    assert_eq!(
        doc.clamp_position(TextPosition::new(99, 7)),
        TextPosition::new(1, 0)
    );
}

#[test]
fn typed_columns_count_combining_marks_as_separate_scalar_values() {
    let mut doc = Document::new();
    let _ = doc
        .try_insert(TextPosition::new(0, 0), "e\u{0301}x")
        .unwrap();

    assert_eq!(doc.line_len_chars(0), 3);
    assert_eq!(doc.char_index_for_position(TextPosition::new(0, 1)), 1);
    assert_eq!(doc.char_index_for_position(TextPosition::new(0, 2)), 2);
    assert_eq!(doc.position_for_char_index(1), TextPosition::new(0, 1));
    assert_eq!(doc.position_for_char_index(2), TextPosition::new(0, 2));
    assert_eq!(
        doc.text_units_between(TextPosition::new(0, 0), TextPosition::new(0, 2)),
        2
    );

    let viewport = doc.read_viewport(ViewportRequest::new(0, 1).with_columns(1, 1));
    assert_eq!(viewport.rows()[0].text(), "\u{0301}");
}

#[test]
fn typed_columns_count_wide_chars_as_single_scalar_values_not_display_cells() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("wide-columns.txt");
    std::fs::write(&path, "界a\n").unwrap();

    let doc = Document::open(&path).unwrap();

    assert_eq!(doc.line_len_chars(0), 2);
    assert_eq!(doc.char_index_for_position(TextPosition::new(0, 1)), 1);
    assert_eq!(doc.position_for_char_index(1), TextPosition::new(0, 1));
    assert_eq!(
        doc.text_units_between(TextPosition::new(0, 0), TextPosition::new(0, 2)),
        2
    );

    let viewport = doc.read_viewport(ViewportRequest::new(0, 1).with_columns(1, 1));
    assert_eq!(viewport.rows()[0].text(), "a");
}

#[test]
fn typed_text_reads_cover_ranges_and_selections() {
    let mut doc = Document::new();
    let _ = doc
        .try_insert(TextPosition::new(0, 0), "alpha\nbeta\ngamma")
        .unwrap();

    let range = doc.text_range_between(TextPosition::new(0, 2), TextPosition::new(1, 2));
    let slice = doc.read_text(range);
    assert!(slice.is_exact());
    assert_eq!(slice.text(), "pha\nbe");

    let selection = TextSelection::new(TextPosition::new(2, 2), TextPosition::new(1, 1));
    let selected = doc.read_selection(selection);
    assert!(selected.is_exact());
    assert_eq!(selected.text(), "eta\nga");
}

#[test]
fn selection_read_is_inexact_when_partial_piece_table_head_is_unresolved() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("selection-read-partial-piece-table.txt");
    let mut bytes = b"zero\n".to_vec();
    bytes.extend(std::iter::repeat_n(
        b'x',
        PARTIAL_PIECE_TABLE_SCAN_BYTES.saturating_add(32),
    ));
    bytes.push(b'\n');
    bytes.extend_from_slice(b"tail\n");
    std::fs::write(&path, &bytes).unwrap();
    let storage = FileStorage::open(&path).unwrap();

    let mut doc = Document {
        path: Some(path.clone()),
        storage: Some(storage.clone()),
        line_offsets: Arc::new(RwLock::new(LineOffsets::default())),
        disk_index: None,
        indexing: Arc::new(AtomicBool::new(false)),
        indexing_started: None,
        file_len: bytes.len(),
        indexed_bytes: Arc::new(AtomicUsize::new(0)),
        avg_line_len: Arc::new(AtomicUsize::new(AVG_LINE_LEN_ESTIMATE)),
        line_ending: LineEnding::Lf,
        encoding: DocumentEncoding::utf8(),
        encoding_origin: DocumentEncodingOrigin::Utf8FastPath,
        encoding_engine: super::encoding_engine::engine_for_encoding(DocumentEncoding::utf8()),
        decoding_had_errors: false,
        preserve_save_error_cache: Cell::new(None),
        rope_file_len_cache: Cell::new(None),
        rope: None,
        rope_history: RopeHistory::new(),
        piece_table: Some(PieceTable::new(storage, vec![5], false)),
        dirty: false,
    };

    let _ = doc.try_insert_text_at(0, 0, "TOP\n").unwrap();

    let slice = doc.read_selection(TextSelection::new(
        TextPosition::new(1, 0),
        TextPosition::new(3, 2),
    ));

    assert!(!slice.is_exact());
    assert!(slice.text().starts_with("zero\n"));
}

#[test]
fn selection_read_stays_exact_on_long_exact_partial_piece_table_line() {
    let dir = tempdir().unwrap();
    let path = dir
        .path()
        .join("selection-read-long-exact-partial-piece-table.txt");
    let prefix = "x".repeat(MAX_LINE_SCAN_CHARS.saturating_add(32));
    let text = format!("zero\n{prefix}target\n");
    std::fs::write(&path, text.as_bytes()).unwrap();
    let storage = FileStorage::open(&path).unwrap();

    let doc = Document {
        path: Some(path.clone()),
        storage: Some(storage.clone()),
        line_offsets: Arc::new(RwLock::new(LineOffsets::default())),
        disk_index: None,
        indexing: Arc::new(AtomicBool::new(false)),
        indexing_started: None,
        file_len: text.len(),
        indexed_bytes: Arc::new(AtomicUsize::new(0)),
        avg_line_len: Arc::new(AtomicUsize::new(AVG_LINE_LEN_ESTIMATE)),
        line_ending: LineEnding::Lf,
        encoding: DocumentEncoding::utf8(),
        encoding_origin: DocumentEncodingOrigin::Utf8FastPath,
        encoding_engine: super::encoding_engine::engine_for_encoding(DocumentEncoding::utf8()),
        decoding_had_errors: false,
        preserve_save_error_cache: Cell::new(None),
        rope_file_len_cache: Cell::new(None),
        rope: None,
        rope_history: RopeHistory::new(),
        piece_table: Some(PieceTable::new(storage, vec![5], false)),
        dirty: false,
    };

    let start_col = MAX_LINE_SCAN_CHARS.saturating_add(28);
    let selection = TextSelection::new(
        TextPosition::new(1, start_col),
        TextPosition::new(1, start_col.saturating_add(10)),
    );
    let slice = doc.read_selection(selection);

    assert!(slice.is_exact());
    assert_eq!(slice.text(), "xxxxtarget");
}

#[test]
fn empty_range_read_is_inexact_when_partial_piece_table_start_is_unresolved() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("empty-range-read-partial-piece-table.txt");
    let mut bytes = b"zero\n".to_vec();
    bytes.extend(std::iter::repeat_n(
        b'x',
        PARTIAL_PIECE_TABLE_SCAN_BYTES.saturating_add(32),
    ));
    bytes.push(b'\n');
    bytes.extend_from_slice(b"tail\n");
    std::fs::write(&path, &bytes).unwrap();
    let storage = FileStorage::open(&path).unwrap();

    let doc = Document {
        path: Some(path.clone()),
        storage: Some(storage.clone()),
        line_offsets: Arc::new(RwLock::new(LineOffsets::default())),
        disk_index: None,
        indexing: Arc::new(AtomicBool::new(false)),
        indexing_started: None,
        file_len: bytes.len(),
        indexed_bytes: Arc::new(AtomicUsize::new(0)),
        avg_line_len: Arc::new(AtomicUsize::new(AVG_LINE_LEN_ESTIMATE)),
        line_ending: LineEnding::Lf,
        encoding: DocumentEncoding::utf8(),
        encoding_origin: DocumentEncodingOrigin::Utf8FastPath,
        encoding_engine: super::encoding_engine::engine_for_encoding(DocumentEncoding::utf8()),
        decoding_had_errors: false,
        preserve_save_error_cache: Cell::new(None),
        rope_file_len_cache: Cell::new(None),
        rope: None,
        rope_history: RopeHistory::new(),
        piece_table: Some(PieceTable::new(storage, vec![5], false)),
        dirty: false,
    };

    let slice = doc.read_text(TextRange::new(TextPosition::new(3, 0), 0));

    assert_eq!(slice.text(), "");
    assert!(!slice.is_exact());
}

#[test]
fn incomplete_mmap_nonempty_range_read_past_eof_is_empty_and_inexact() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("incomplete-mmap-range-past-eof.txt");
    std::fs::write(&path, b"zero\n").unwrap();
    let storage = FileStorage::open(&path).unwrap();

    let doc = Document {
        path: Some(path.clone()),
        storage: Some(storage.clone()),
        line_offsets: Arc::new(RwLock::new(LineOffsets::default())),
        disk_index: None,
        indexing: Arc::new(AtomicBool::new(false)),
        indexing_started: None,
        file_len: 5,
        indexed_bytes: Arc::new(AtomicUsize::new(0)),
        avg_line_len: Arc::new(AtomicUsize::new(1)),
        line_ending: LineEnding::Lf,
        encoding: DocumentEncoding::utf8(),
        encoding_origin: DocumentEncodingOrigin::Utf8FastPath,
        encoding_engine: super::encoding_engine::engine_for_encoding(DocumentEncoding::utf8()),
        decoding_had_errors: false,
        preserve_save_error_cache: Cell::new(None),
        rope_file_len_cache: Cell::new(None),
        rope: None,
        rope_history: RopeHistory::new(),
        piece_table: None,
        dirty: false,
    };

    let slice = doc.read_text(TextRange::new(TextPosition::new(99, 0), 3));
    assert_eq!(slice.text(), "");
    assert!(slice.is_exact());

    let selection = doc.read_selection(TextSelection::new(
        TextPosition::new(99, 0),
        TextPosition::new(99, 3),
    ));
    assert_eq!(selection.text(), "");
    assert!(selection.is_exact());
}

#[test]
fn incomplete_mmap_typed_read_uses_exact_start_offset_instead_of_heuristic_line_range() {
    let dir = tempdir().unwrap();
    let path = dir
        .path()
        .join("incomplete-mmap-typed-read-exact-start.txt");
    std::fs::write(&path, b"zero\none\ntwo\n").unwrap();
    let storage = FileStorage::open(&path).unwrap();

    let doc = Document {
        path: Some(path.clone()),
        storage: Some(storage.clone()),
        line_offsets: Arc::new(RwLock::new(LineOffsets::default())),
        disk_index: None,
        indexing: Arc::new(AtomicBool::new(false)),
        indexing_started: None,
        file_len: std::fs::metadata(&path).unwrap().len() as usize,
        indexed_bytes: Arc::new(AtomicUsize::new(0)),
        avg_line_len: Arc::new(AtomicUsize::new(128)),
        line_ending: LineEnding::Lf,
        encoding: DocumentEncoding::utf8(),
        encoding_origin: DocumentEncodingOrigin::Utf8FastPath,
        encoding_engine: super::encoding_engine::engine_for_encoding(DocumentEncoding::utf8()),
        decoding_had_errors: false,
        preserve_save_error_cache: Cell::new(None),
        rope_file_len_cache: Cell::new(None),
        rope: None,
        rope_history: RopeHistory::new(),
        piece_table: None,
        dirty: false,
    };

    let slice = doc.read_text(TextRange::new(TextPosition::new(1, 0), 3));
    assert_eq!(slice.text(), "one");
    assert!(slice.is_exact());

    let selection = doc.read_selection(TextSelection::new(
        TextPosition::new(1, 0),
        TextPosition::new(1, 3),
    ));
    assert_eq!(selection.text(), "one");
    assert!(selection.is_exact());
}

#[test]
fn typed_text_reads_preserve_crlf_in_clean_mmap_documents() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("crlf-selection.txt");
    std::fs::write(&path, b"alpha\r\nbeta\r\n").unwrap();

    let doc = Document::open(&path).unwrap();
    let selection = TextSelection::new(TextPosition::new(0, 3), TextPosition::new(1, 2));
    let slice = doc.read_selection(selection);

    assert!(slice.is_exact());
    assert_eq!(slice.text(), "ha\r\nbe");
}

#[test]
fn typed_slices_support_standard_str_ergonomics() {
    let mut doc = Document::new();
    let _ = doc
        .try_insert(TextPosition::new(0, 0), "alpha\nbeta")
        .unwrap();

    let text_slice = doc.read_selection(TextSelection::new(
        TextPosition::new(0, 1),
        TextPosition::new(1, 2),
    ));
    assert_eq!(text_slice.as_ref(), "lpha\nbe");
    assert_eq!(&*text_slice, "lpha\nbe");
    assert_eq!(text_slice.to_string(), "lpha\nbe");

    let line_slice = doc.line_slice(1, 1, 3);
    assert_eq!(line_slice.as_ref(), "eta");
    assert_eq!(&*line_slice, "eta");
    assert_eq!(line_slice.to_string(), "eta");
}

#[test]
fn zero_width_line_reads_still_preserve_exactness_for_known_lines() {
    let mut doc = Document::new();
    let _ = doc
        .try_insert(TextPosition::new(0, 0), "alpha\nbeta")
        .unwrap();

    let slice = doc.line_slice(1, 1, 0);
    assert_eq!(slice.text(), "");
    assert!(slice.is_exact());

    let slices = doc.line_slices(0, 2, 0, 0);
    assert_eq!(slices.len(), 2);
    assert!(slices.iter().all(|slice| slice.text().is_empty()));
    assert!(slices.iter().all(LineSlice::is_exact));

    let viewport = doc.read_viewport(ViewportRequest::new(0, 2).with_columns(0, 0));
    assert_eq!(viewport.rows().len(), 2);
    assert!(viewport.rows().iter().all(|row| row.text().is_empty()));
    assert!(viewport.rows().iter().all(ViewportRow::is_exact));
}

#[test]
fn literal_search_finds_next_match_in_clean_mmap_document() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("search-mmap.txt");
    std::fs::write(&path, b"alpha\r\nbeta\r\ngamma\r\n").unwrap();

    let doc = Document::open(&path).unwrap();
    let found = doc.find_next("beta", TextPosition::new(0, 0)).unwrap();

    assert_eq!(found.start(), TextPosition::new(1, 0));
    assert_eq!(found.end(), TextPosition::new(1, 4));
    assert_eq!(doc.read_text(found.range()).text(), "beta");

    let query = LiteralSearchQuery::new("beta").unwrap();
    let compiled = doc
        .find_next_query(&query, TextPosition::new(0, 0))
        .unwrap();
    assert_eq!(compiled, found);
}

#[test]
fn literal_search_finds_next_match_in_rope_document() {
    let mut doc = Document::new();
    let _ = doc
        .try_insert(TextPosition::new(0, 0), "alpha\nbeta\ngamma\nbeta")
        .unwrap();

    let found = doc.find_next("beta", TextPosition::new(1, 1)).unwrap();

    assert_eq!(found.start(), TextPosition::new(3, 0));
    assert_eq!(found.end(), TextPosition::new(3, 4));
    assert_eq!(doc.read_text(found.range()).text(), "beta");
}

#[test]
fn literal_search_finds_next_match_across_piece_table_boundary() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("search-piece-table.txt");
    let line = "0000target\n";
    let repeat = (PIECE_TABLE_MIN_BYTES / line.len()).saturating_add(64);
    std::fs::write(&path, line.repeat(repeat)).unwrap();

    let mut doc = Document::open(&path).unwrap();
    let _ = doc.try_insert(TextPosition::new(0, 0), "[qem]").unwrap();
    assert_eq!(doc.backing(), DocumentBacking::PieceTable);

    let found = doc
        .find_next("[qem]0000target", TextPosition::new(0, 0))
        .unwrap();

    assert_eq!(found.start(), TextPosition::new(0, 0));
    assert_eq!(found.end(), TextPosition::new(0, 15));
    assert_eq!(doc.read_text(found.range()).text(), "[qem]0000target");
}

#[test]
fn literal_search_finds_next_match_from_nonzero_piece_table_anchor() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("search-piece-table-anchor.txt");
    let line = "alpha target\n";
    let repeat = (PIECE_TABLE_MIN_BYTES / line.len()).saturating_add(64);
    std::fs::write(&path, line.repeat(repeat)).unwrap();

    let mut doc = Document::open(&path).unwrap();
    let _ = doc.try_insert(TextPosition::new(0, 0), "[qem]").unwrap();
    let _ = doc.try_insert(TextPosition::new(1, 0), ">>").unwrap();
    let _ = doc.try_insert(TextPosition::new(2, 0), ">>").unwrap();
    assert_eq!(doc.backing(), DocumentBacking::PieceTable);

    let found = doc.find_next("target", TextPosition::new(2, 2)).unwrap();

    assert_eq!(found.start(), TextPosition::new(2, 8));
    assert_eq!(found.end(), TextPosition::new(2, 14));
    assert_eq!(doc.read_text(found.range()).text(), "target");
}

#[test]
fn literal_search_finds_previous_match_with_end_before_boundary() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("search-prev-mmap.txt");
    std::fs::write(&path, b"alpha\r\nbeta\r\ngamma\r\nbeta\r\n").unwrap();

    let doc = Document::open(&path).unwrap();
    let found = doc.find_prev("beta", TextPosition::new(3, 4)).unwrap();

    assert_eq!(found.start(), TextPosition::new(3, 0));
    assert_eq!(found.end(), TextPosition::new(3, 4));
    assert_eq!(doc.read_text(found.range()).text(), "beta");

    let previous = doc.find_prev("beta", TextPosition::new(3, 0)).unwrap();
    assert_eq!(previous.start(), TextPosition::new(1, 0));
    assert_eq!(previous.end(), TextPosition::new(1, 4));

    let query = LiteralSearchQuery::new("beta").unwrap();
    let compiled = doc
        .find_prev_query(&query, TextPosition::new(3, 0))
        .unwrap();
    assert_eq!(compiled, previous);
}

#[test]
fn long_exact_mmap_lines_do_not_cap_typed_columns_or_rewind_search_start() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("long-exact-mmap-columns.txt");
    let prefix = "x".repeat(MAX_LINE_SCAN_CHARS.saturating_add(32));
    let text = format!("{prefix}target\n");
    std::fs::write(&path, text.as_bytes()).unwrap();

    let doc = Document::open(&path).unwrap();
    let line_len = prefix.len().saturating_add("target".len());

    assert_eq!(doc.line_len_chars(0), line_len);
    assert_eq!(
        doc.clamp_position(TextPosition::new(0, line_len)),
        TextPosition::new(0, line_len)
    );
    assert_eq!(
        doc.find_next("target", TextPosition::new(0, line_len)),
        None
    );
}

#[test]
fn long_piece_table_lines_do_not_cap_typed_columns_or_rewind_search_start() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("long-piece-table-columns.txt");
    let prefix = "x".repeat(PIECE_TABLE_MIN_BYTES.saturating_add(32));
    let text = format!("{prefix}target\n");
    std::fs::write(&path, text.as_bytes()).unwrap();

    let mut doc = Document::open(&path).unwrap();
    let _ = doc.try_insert(TextPosition::new(0, 0), "!").unwrap();
    assert_eq!(doc.backing(), DocumentBacking::PieceTable);

    let line_len = 1usize
        .saturating_add(prefix.len())
        .saturating_add("target".len());
    assert_eq!(doc.line_len_chars(0), line_len);
    assert_eq!(
        doc.clamp_position(TextPosition::new(0, line_len)),
        TextPosition::new(0, line_len)
    );
    assert_eq!(
        doc.find_next("target", TextPosition::new(0, line_len)),
        None
    );
}

#[test]
fn literal_search_bounded_range_returns_only_fully_contained_matches() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("search-bounded-mmap.txt");
    std::fs::write(&path, b"alpha\r\nbeta\r\ngamma\r\nbeta\r\n").unwrap();

    let doc = Document::open(&path).unwrap();
    let range = doc.text_range_between(TextPosition::new(1, 0), TextPosition::new(3, 0));

    let next = doc.find_next_in_range("beta", range).unwrap();
    assert_eq!(next.start(), TextPosition::new(1, 0));
    assert_eq!(next.end(), TextPosition::new(1, 4));

    let prev = doc.find_prev_in_range("beta", range).unwrap();
    assert_eq!(prev.start(), TextPosition::new(1, 0));
    assert_eq!(prev.end(), TextPosition::new(1, 4));

    let query = LiteralSearchQuery::new("beta").unwrap();
    let compiled = doc.find_next_query_in_range(&query, range).unwrap();
    assert_eq!(compiled, next);

    let between_next = doc
        .find_next_between("beta", TextPosition::new(1, 0), TextPosition::new(3, 0))
        .unwrap();
    assert_eq!(between_next, next);

    let between_prev = doc
        .find_prev_query_between(&query, TextPosition::new(1, 0), TextPosition::new(3, 0))
        .unwrap();
    assert_eq!(between_prev, prev);
}

#[test]
fn literal_search_iterator_yields_non_overlapping_matches() {
    let mut doc = Document::new();
    let _ = doc.try_insert(TextPosition::new(0, 0), "aaaa").unwrap();

    let matches: Vec<_> = doc.find_all("aa").collect();
    let query = LiteralSearchQuery::new("aa").unwrap();
    let query_matches: Vec<_> = doc.find_all_query(&query).collect();

    assert_eq!(matches.len(), 2);
    assert_eq!(matches[0].start(), TextPosition::new(0, 0));
    assert_eq!(matches[0].end(), TextPosition::new(0, 2));
    assert_eq!(matches[1].start(), TextPosition::new(0, 2));
    assert_eq!(matches[1].end(), TextPosition::new(0, 4));
    assert_eq!(query_matches, matches);
}

#[test]
fn literal_search_iterator_is_fused() {
    fn assert_fused<I: std::iter::FusedIterator>(_iter: I) {}

    let mut doc = Document::new();
    let _ = doc.try_insert(TextPosition::new(0, 0), "aaaa").unwrap();

    assert_fused(doc.find_all("aa"));
    let query = LiteralSearchQuery::new("aa").unwrap();
    assert_fused(doc.find_all_query(&query));
}

#[test]
fn literal_search_iterator_respects_piece_table_range_boundaries() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("search-iterator-piece-table.txt");
    let line = "0000target\n";
    let repeat = (PIECE_TABLE_MIN_BYTES / line.len()).saturating_add(64);
    std::fs::write(&path, line.repeat(repeat)).unwrap();

    let mut doc = Document::open(&path).unwrap();
    let _ = doc.try_insert(TextPosition::new(0, 0), "[qem]").unwrap();
    assert_eq!(doc.backing(), DocumentBacking::PieceTable);

    let range = doc.text_range_between(TextPosition::new(0, 0), TextPosition::new(2, 0));
    let matches: Vec<_> = doc.find_all_in_range("target", range).collect();
    let between_matches: Vec<_> = doc
        .find_all_between("target", TextPosition::new(0, 0), TextPosition::new(2, 0))
        .collect();
    let query = LiteralSearchQuery::new("target").unwrap();
    let query_matches: Vec<_> = doc.find_all_query_in_range(&query, range).collect();
    let query_between_matches: Vec<_> = doc
        .find_all_query_between(&query, TextPosition::new(0, 0), TextPosition::new(2, 0))
        .collect();

    assert_eq!(matches.len(), 2);
    assert_eq!(matches[0].start(), TextPosition::new(0, 9));
    assert_eq!(matches[0].end(), TextPosition::new(0, 15));
    assert_eq!(matches[1].start(), TextPosition::new(1, 4));
    assert_eq!(matches[1].end(), TextPosition::new(1, 10));
    assert_eq!(between_matches, matches);
    assert_eq!(query_matches, matches);
    assert_eq!(query_between_matches, matches);
}

#[test]
fn literal_search_bounded_range_rejects_partial_piece_table_match_at_end_boundary() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("search-bounded-piece-table.txt");
    let line = "0000target\n";
    let repeat = (PIECE_TABLE_MIN_BYTES / line.len()).saturating_add(64);
    std::fs::write(&path, line.repeat(repeat)).unwrap();

    let mut doc = Document::open(&path).unwrap();
    let _ = doc.try_insert(TextPosition::new(0, 0), "[qem]").unwrap();
    assert_eq!(doc.backing(), DocumentBacking::PieceTable);

    let clipped = TextRange::new(TextPosition::new(0, 0), 14);
    assert!(
        doc.find_next_in_range("[qem]0000target", clipped).is_none(),
        "match must stay fully inside bounded range"
    );

    let query = LiteralSearchQuery::new("[qem]0000target").unwrap();
    assert!(doc.find_next_query_in_range(&query, clipped).is_none());
}

#[test]
fn literal_search_finds_previous_match_in_rope_document() {
    let mut doc = Document::new();
    let _ = doc
        .try_insert(TextPosition::new(0, 0), "alpha\nbeta\ngamma\nbeta")
        .unwrap();

    let found = doc.find_prev("beta", TextPosition::new(3, 4)).unwrap();

    assert_eq!(found.start(), TextPosition::new(3, 0));
    assert_eq!(found.end(), TextPosition::new(3, 4));

    let previous = doc.find_prev("beta", TextPosition::new(3, 0)).unwrap();
    assert_eq!(previous.start(), TextPosition::new(1, 0));
    assert_eq!(previous.end(), TextPosition::new(1, 4));
}

#[test]
fn literal_search_finds_previous_match_across_piece_table_boundary() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("search-prev-piece-table.txt");
    let line = "0000target\n";
    let repeat = (PIECE_TABLE_MIN_BYTES / line.len()).saturating_add(64);
    std::fs::write(&path, line.repeat(repeat)).unwrap();

    let mut doc = Document::open(&path).unwrap();
    let _ = doc.try_insert(TextPosition::new(0, 0), "[qem]").unwrap();
    assert_eq!(doc.backing(), DocumentBacking::PieceTable);

    let found = doc
        .find_prev("[qem]0000target", TextPosition::new(0, 15))
        .unwrap();
    assert_eq!(found.start(), TextPosition::new(0, 0));
    assert_eq!(found.end(), TextPosition::new(0, 15));

    assert!(doc
        .find_prev("[qem]0000target", TextPosition::new(0, 0))
        .is_none());
}

#[test]
fn literal_search_finds_previous_match_from_nonzero_piece_table_anchor() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("search-prev-anchor-piece-table.txt");
    let line = "0000target\n";
    let repeat = (PIECE_TABLE_MIN_BYTES / line.len()).saturating_add(64);
    std::fs::write(&path, line.repeat(repeat)).unwrap();

    let mut doc = Document::open(&path).unwrap();
    let _ = doc.try_insert(TextPosition::new(0, 0), "[qem]\n").unwrap();
    assert_eq!(doc.backing(), DocumentBacking::PieceTable);

    let before = TextPosition::new(1, 15);
    let found = doc.find_prev("0000target", before).unwrap();

    assert_eq!(found.start(), TextPosition::new(1, 0));
    assert_eq!(found.end(), TextPosition::new(1, 10));
}

#[test]
fn literal_search_finds_previous_same_line_piece_table_match_near_anchor() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("search-prev-same-line-piece-table.txt");
    let line = "0000target\n";
    let repeat = (PIECE_TABLE_MIN_BYTES / line.len()).saturating_add(64);
    std::fs::write(&path, line.repeat(repeat)).unwrap();

    let mut doc = Document::open(&path).unwrap();
    let _ = doc.try_insert(TextPosition::new(0, 0), "[qem]").unwrap();
    assert_eq!(doc.backing(), DocumentBacking::PieceTable);

    let found = doc.find_prev("00", TextPosition::new(0, 10)).unwrap();
    assert_eq!(found.start(), TextPosition::new(0, 7));
    assert_eq!(found.end(), TextPosition::new(0, 9));
}

#[test]
fn literal_search_finds_previous_piece_table_match_before_trailing_newline_anchor() {
    let dir = tempdir().unwrap();
    let path = dir
        .path()
        .join("search-prev-trailing-newline-piece-table.txt");
    let line = "0000target\n";
    let repeat = (PIECE_TABLE_MIN_BYTES / line.len()).saturating_add(64);
    std::fs::write(&path, line.repeat(repeat)).unwrap();

    let mut doc = Document::open(&path).unwrap();
    let _ = doc.try_insert(TextPosition::new(0, 0), "[qem]\n").unwrap();
    assert_eq!(doc.backing(), DocumentBacking::PieceTable);

    let found = doc
        .find_prev("00", TextPosition::new(usize::MAX, usize::MAX))
        .unwrap();
    let last_line0 = doc.line_count().display_rows().saturating_sub(2);
    assert_eq!(found.start(), TextPosition::new(last_line0, 2));
    assert_eq!(found.end(), TextPosition::new(last_line0, 4));
}

#[test]
fn typed_delete_forward_treats_crlf_as_single_text_unit() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("crlf-delete-forward.txt");
    std::fs::write(&path, b"alpha\r\nbeta\r\n").unwrap();

    let mut doc = Document::open(&path).unwrap();
    let result = doc.try_delete_forward(TextPosition::new(0, 5)).unwrap();

    assert!(result.changed());
    assert_eq!(result.cursor(), TextPosition::new(0, 5));
    assert_eq!(doc.text_lossy(), "alphabeta\n");
}

#[test]
fn typed_selection_delete_and_replace_helpers_work() {
    let mut doc = Document::new();
    let _ = doc
        .try_insert(TextPosition::new(0, 0), "alpha\nbeta")
        .unwrap();

    let selection = TextSelection::new(TextPosition::new(1, 2), TextPosition::new(0, 4));
    let range = doc.text_range_for_selection(selection);
    assert_eq!(range.start(), TextPosition::new(0, 4));
    assert_eq!(range.len_chars(), 4);

    let cursor = doc.try_replace_selection(selection, "Z").unwrap();
    assert_eq!(cursor, TextPosition::new(0, 5));
    assert_eq!(doc.text_lossy(), "alphZta");

    let caret = TextSelection::caret(TextPosition::new(0, 3));
    let delete = doc.try_delete_selection(caret).unwrap();
    assert!(!delete.changed());
    assert_eq!(delete.cursor(), TextPosition::new(0, 3));
}

#[test]
fn selection_replace_promotes_partial_piece_table_before_clamping_unresolved_head() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("selection-promote-partial-piece-table.txt");
    let mut bytes = b"zero\n".to_vec();
    bytes.extend(std::iter::repeat_n(
        b'x',
        PARTIAL_PIECE_TABLE_SCAN_BYTES.saturating_add(32),
    ));
    bytes.push(b'\n');
    bytes.extend_from_slice(b"tail\n");
    std::fs::write(&path, &bytes).unwrap();
    let storage = FileStorage::open(&path).unwrap();

    let mut doc = Document {
        path: Some(path.clone()),
        storage: Some(storage.clone()),
        line_offsets: Arc::new(RwLock::new(LineOffsets::default())),
        disk_index: None,
        indexing: Arc::new(AtomicBool::new(false)),
        indexing_started: None,
        file_len: bytes.len(),
        indexed_bytes: Arc::new(AtomicUsize::new(0)),
        avg_line_len: Arc::new(AtomicUsize::new(AVG_LINE_LEN_ESTIMATE)),
        line_ending: LineEnding::Lf,
        encoding: DocumentEncoding::utf8(),
        encoding_origin: DocumentEncodingOrigin::Utf8FastPath,
        encoding_engine: super::encoding_engine::engine_for_encoding(DocumentEncoding::utf8()),
        decoding_had_errors: false,
        preserve_save_error_cache: Cell::new(None),
        rope_file_len_cache: Cell::new(None),
        rope: None,
        rope_history: RopeHistory::new(),
        piece_table: Some(PieceTable::new(storage, vec![5], false)),
        dirty: false,
    };

    let selection = TextSelection::new(TextPosition::new(0, 2), TextPosition::new(2, 4));
    let cursor = doc.try_replace_selection(selection, "Z").unwrap();

    assert_eq!(cursor, TextPosition::new(0, 3));
    assert!(!doc.has_piece_table());
    assert_eq!(doc.text_lossy(), "zeZ\n");
}

#[test]
fn selection_edit_capability_requires_promotion_for_unresolved_partial_piece_table_head() {
    let dir = tempdir().unwrap();
    let path = dir
        .path()
        .join("selection-capability-partial-piece-table.txt");
    let mut bytes = b"zero\n".to_vec();
    bytes.extend(std::iter::repeat_n(
        b'x',
        PARTIAL_PIECE_TABLE_SCAN_BYTES.saturating_add(32),
    ));
    bytes.push(b'\n');
    bytes.extend_from_slice(b"tail\n");
    std::fs::write(&path, &bytes).unwrap();
    let storage = FileStorage::open(&path).unwrap();

    let doc = Document {
        path: Some(path.clone()),
        storage: Some(storage.clone()),
        line_offsets: Arc::new(RwLock::new(LineOffsets::default())),
        disk_index: None,
        indexing: Arc::new(AtomicBool::new(false)),
        indexing_started: None,
        file_len: bytes.len(),
        indexed_bytes: Arc::new(AtomicUsize::new(0)),
        avg_line_len: Arc::new(AtomicUsize::new(AVG_LINE_LEN_ESTIMATE)),
        line_ending: LineEnding::Lf,
        encoding: DocumentEncoding::utf8(),
        encoding_origin: DocumentEncodingOrigin::Utf8FastPath,
        encoding_engine: super::encoding_engine::engine_for_encoding(DocumentEncoding::utf8()),
        decoding_had_errors: false,
        preserve_save_error_cache: Cell::new(None),
        rope_file_len_cache: Cell::new(None),
        rope: None,
        rope_history: RopeHistory::new(),
        piece_table: Some(PieceTable::new(storage, vec![5], false)),
        dirty: false,
    };

    let capability = doc.edit_capability_for_selection(TextSelection::new(
        TextPosition::new(0, 2),
        TextPosition::new(2, 4),
    ));

    assert_eq!(
        capability,
        EditCapability::RequiresPromotion {
            from: DocumentBacking::PieceTable,
            to: DocumentBacking::Rope,
        }
    );
}

#[test]
fn typed_selection_delete_commands_handle_caret_and_range() {
    let mut doc = Document::new();
    let _ = doc
        .try_insert(TextPosition::new(0, 0), "alpha\nbeta")
        .unwrap();

    let range_selection = TextSelection::new(TextPosition::new(0, 3), TextPosition::new(1, 2));
    let deleted = doc.try_delete_forward_selection(range_selection).unwrap();
    assert!(deleted.changed());
    assert_eq!(deleted.cursor(), TextPosition::new(0, 3));
    assert_eq!(doc.text_lossy(), "alpta");

    let backspace = doc
        .try_backspace_selection(TextSelection::caret(TextPosition::new(0, 2)))
        .unwrap();
    assert!(backspace.changed());
    assert_eq!(backspace.cursor(), TextPosition::new(0, 1));
    assert_eq!(doc.text_lossy(), "apta");

    let forward = doc
        .try_delete_forward_selection(TextSelection::caret(TextPosition::new(0, 1)))
        .unwrap();
    assert!(forward.changed());
    assert_eq!(forward.cursor(), TextPosition::new(0, 1));
    assert_eq!(doc.text_lossy(), "ata");
}

#[test]
fn typed_cut_selection_returns_removed_text_and_cursor() {
    let mut doc = Document::new();
    let _ = doc
        .try_insert(TextPosition::new(0, 0), "alpha\nbeta")
        .unwrap();

    let selection = TextSelection::new(TextPosition::new(0, 3), TextPosition::new(1, 2));
    let cut = doc.try_cut_selection(selection).unwrap();

    assert!(cut.changed());
    assert_eq!(cut.text(), "ha\nbe");
    assert_eq!(cut.cursor(), TextPosition::new(0, 3));
    assert_eq!(doc.text_lossy(), "alpta");

    let caret = doc
        .try_cut_selection(TextSelection::caret(TextPosition::new(0, 1)))
        .unwrap();
    assert!(!caret.changed());
    assert!(caret.text().is_empty());
    assert_eq!(caret.cursor(), TextPosition::new(0, 1));
}

#[test]
fn document_status_reports_frontend_snapshot() {
    let mut doc = Document::new();
    let _ = doc
        .try_insert(TextPosition::new(0, 0), "alpha\nbeta")
        .unwrap();

    let status = doc.status();

    assert!(status.is_dirty());
    assert_eq!(doc.backing(), DocumentBacking::Rope);
    assert_eq!(status.backing(), DocumentBacking::Rope);
    assert_eq!(status.backing().as_str(), "rope");
    assert_eq!(status.line_count(), LineCount::Exact(2));
    assert_eq!(status.exact_line_count(), Some(2));
    assert_eq!(status.display_line_count(), 2);
    assert!(status.is_line_count_exact());
    assert_eq!(status.line_ending(), LineEnding::Lf);
    assert_eq!(
        status.encoding_origin(),
        DocumentEncodingOrigin::NewDocument
    );
    assert!(status.can_preserve_save());
    assert_eq!(status.preserve_save_error(), None);
    assert!(status.has_edit_buffer());
    assert!(status.has_rope());
    assert!(!status.has_piece_table());
    assert!(!status.is_indexing());
    assert!(!status.is_line_count_pending());
}

#[test]
fn document_status_reports_pending_exact_line_count() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("pending-line-count.txt");
    std::fs::write(&path, b"alpha\nbeta").unwrap();
    let storage = FileStorage::open(&path).unwrap();

    let doc = Document {
        path: Some(path),
        storage: Some(storage),
        line_offsets: Arc::new(RwLock::new(LineOffsets::default())),
        disk_index: Some(DiskLineIndex::building_for_tests()),
        indexing: Arc::new(AtomicBool::new(false)),
        indexing_started: None,
        file_len: 10,
        indexed_bytes: Arc::new(AtomicUsize::new(0)),
        avg_line_len: Arc::new(AtomicUsize::new(AVG_LINE_LEN_ESTIMATE)),
        line_ending: LineEnding::Lf,
        encoding: DocumentEncoding::utf8(),
        encoding_origin: DocumentEncodingOrigin::Utf8FastPath,
        encoding_engine: super::encoding_engine::engine_for_encoding(DocumentEncoding::utf8()),
        decoding_had_errors: false,
        preserve_save_error_cache: Cell::new(None),
        rope_file_len_cache: Cell::new(None),
        rope: None,
        rope_history: RopeHistory::new(),
        piece_table: None,
        dirty: false,
    };

    assert!(doc.is_line_count_pending());
    assert_eq!(doc.exact_line_count(), None);

    let status = doc.status();
    assert!(status.is_line_count_pending());
    assert!(!status.is_line_count_exact());
}

#[test]
fn wait_for_exact_line_count_observes_background_disk_index_completion() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("wait-line-count.txt");
    std::fs::write(&path, b"alpha\nbeta").unwrap();
    let storage = FileStorage::open(&path).unwrap();
    let disk_index = DiskLineIndex::building_for_tests();
    let background_index = disk_index.clone();

    let doc = Document {
        path: Some(path),
        storage: Some(storage),
        line_offsets: Arc::new(RwLock::new(LineOffsets::default())),
        disk_index: Some(disk_index),
        indexing: Arc::new(AtomicBool::new(false)),
        indexing_started: None,
        file_len: 10,
        indexed_bytes: Arc::new(AtomicUsize::new(0)),
        avg_line_len: Arc::new(AtomicUsize::new(AVG_LINE_LEN_ESTIMATE)),
        line_ending: LineEnding::Lf,
        encoding: DocumentEncoding::utf8(),
        encoding_origin: DocumentEncodingOrigin::Utf8FastPath,
        encoding_engine: super::encoding_engine::engine_for_encoding(DocumentEncoding::utf8()),
        decoding_had_errors: false,
        preserve_save_error_cache: Cell::new(None),
        rope_file_len_cache: Cell::new(None),
        rope: None,
        rope_history: RopeHistory::new(),
        piece_table: None,
        dirty: false,
    };

    let worker = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(20));
        background_index.finish_build_for_tests(2);
    });

    assert_eq!(
        doc.wait_for_exact_line_count(std::time::Duration::from_secs(1)),
        Some(2)
    );
    worker.join().unwrap();
}

#[test]
fn preserve_save_preflight_reports_lossy_for_decoded_documents() {
    // Mmap-only UTF-16 documents are byte-streamable
    // through preserve-save (no rope, no piece-table — the on-disk bytes
    // are already in the target encoding), so they no longer surface
    // `PreserveSaveUnsupported`. Lossy decoding through `Shift_JIS` (which
    // currently still decodes through a rope) still surfaces
    // `LossyDecodedPreserve`. The UTF-16 portion of this preflight test
    // therefore now asserts the positive contract.
    let dir = tempdir().unwrap();
    let lossy_path = dir.path().join("lossy-shift-jis.txt");
    let utf16_path = dir.path().join("utf16le-source.txt");
    std::fs::write(&lossy_path, [0x82]).unwrap();

    let lossy_encoding = DocumentEncoding::from_label("shift_jis").unwrap();
    let lossy_doc = Document::open_with_encoding(lossy_path, lossy_encoding).unwrap();
    assert!(!lossy_doc.can_preserve_save());
    assert_eq!(
        lossy_doc.preserve_save_error(),
        Some(DocumentEncodingErrorKind::LossyDecodedPreserve)
    );
    assert_eq!(
        lossy_doc.status().preserve_save_error(),
        Some(DocumentEncodingErrorKind::LossyDecodedPreserve)
    );

    let mut utf16_bytes = vec![0xFF, 0xFE];
    for unit in "hello\n".encode_utf16() {
        utf16_bytes.extend_from_slice(&unit.to_le_bytes());
    }
    std::fs::write(&utf16_path, utf16_bytes).unwrap();

    let utf16_doc = Document::open_with_auto_encoding_detection(utf16_path).unwrap();
    assert!(
        utf16_doc.can_preserve_save(),
        "mmap-only UTF-16 document must allow preserve-save",
    );
    assert_eq!(utf16_doc.preserve_save_error(), None);
    assert_eq!(utf16_doc.status().preserve_save_error(), None);
}

#[test]
fn save_conversion_preflight_reports_success_and_failures() {
    let cp1251 = DocumentEncoding::from_label("windows-1251").unwrap();

    let mut representable = Document::new();
    representable
        .try_insert_text_at(0, 0, "\u{043F}\u{0440}\u{0438}\u{0432}\u{0435}\u{0442}\n")
        .unwrap();
    assert_eq!(representable.save_error_for_encoding(cp1251), None);
    assert!(representable.can_save_with_encoding(cp1251));

    let mut unrepresentable = Document::new();
    unrepresentable
        .try_insert_text_at(0, 0, "emoji \u{1F642}\n")
        .unwrap();
    assert_eq!(
        unrepresentable.save_error_for_encoding(cp1251),
        Some(DocumentEncodingErrorKind::UnrepresentableText)
    );
    assert!(!unrepresentable.can_save_with_encoding(cp1251));
    assert_eq!(
        unrepresentable.save_error_for_options(
            DocumentSaveOptions::new().with_encoding(DocumentEncoding::utf16le())
        ),
        Some(DocumentEncodingErrorKind::UnsupportedSaveTarget)
    );
}

#[test]
fn lossy_document_save_conversion_preflight_allows_utf8_salvage() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("lossy-shift-jis-preflight.txt");
    let shift_jis = DocumentEncoding::from_label("shift_jis").unwrap();
    std::fs::write(&path, [0x82]).unwrap();

    let doc = Document::open_with_encoding(path, shift_jis).unwrap();

    assert_eq!(doc.save_error_for_encoding(DocumentEncoding::utf8()), None);
    assert!(doc.can_save_with_encoding(DocumentEncoding::utf8()));
    assert_eq!(
        doc.save_error_for_encoding(shift_jis),
        Some(DocumentEncodingErrorKind::UnrepresentableText)
    );
}

#[test]
fn preserve_save_preflight_reports_unrepresentable_legacy_edits() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("legacy-cp1251-preflight.txt");
    let _saved = dir.path().join("legacy-cp1251-preflight-saved.txt");
    let encoding = DocumentEncoding::from_label("windows-1251").unwrap();
    let (bytes, used, had_errors) =
        WINDOWS_1251.encode("\u{043F}\u{0440}\u{0438}\u{0432}\u{0435}\u{0442}\n");
    assert_eq!(used, WINDOWS_1251);
    assert!(!had_errors);
    std::fs::write(&path, bytes.as_ref()).unwrap();

    let mut doc = Document::open_with_encoding(path, encoding).unwrap();
    // The non-UTF-8 edit path rejects unrepresentable scalars at the
    // insert itself rather than letting the rope-decode bridge
    // swallow them and then surfacing the failure only at preserve-save
    // preflight. The typed
    // error path is now `try_insert_text_at` directly with the operation
    // tagged as `"insert"`.
    let err = doc
        .try_insert_text_at(0, 0, "emoji \u{1F642}\n")
        .unwrap_err();
    assert!(matches!(
        err,
        DocumentError::Encoding {
            operation: "insert",
            encoding: failed_encoding,
            reason: DocumentEncodingErrorKind::UnrepresentableText,
            ..
        } if failed_encoding == encoding
    ));

    // The document remained clean and on its native mmap backing because
    // unrepresentable inserts must not touch the add buffer.
    assert!(!doc.is_dirty());
    assert!(!doc.has_piece_table());
    assert!(!doc.has_rope());
    assert!(doc.can_preserve_save());
}

#[test]
fn document_maintenance_status_is_empty_for_non_piece_table_backings() {
    let mut doc = Document::new();
    let _ = doc
        .try_insert(TextPosition::new(0, 0), "alpha\nbeta")
        .unwrap();

    let maintenance = doc.maintenance_status();

    assert_eq!(maintenance.backing(), DocumentBacking::Rope);
    assert!(!maintenance.has_piece_table());
    assert!(!maintenance.has_fragmentation_stats());
    assert_eq!(maintenance.fragmentation_stats(), None);
    assert!(!maintenance.is_compaction_recommended());
    assert_eq!(maintenance.compaction_recommendation(), None);
    assert_eq!(maintenance.compaction_urgency(), None);
    assert_eq!(maintenance.recommended_action(), MaintenanceAction::None);
    assert!(!maintenance.should_run_idle_compaction());
    assert!(!maintenance.should_wait_for_explicit_compaction());
}

#[test]
fn document_maintenance_status_reports_piece_table_fragmentation_and_policy() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("large-piece-table.txt");
    let line = b"0000target\n";
    let repeat = (1024 * 1024 / line.len()) + 64;
    std::fs::write(&path, line.repeat(repeat)).unwrap();

    let policy = CompactionPolicy {
        min_total_bytes: 0,
        min_piece_count: 2,
        small_piece_threshold_bytes: usize::MAX,
        max_average_piece_bytes: usize::MAX,
        min_fragmentation_ratio: 0.0,
        forced_piece_count: usize::MAX,
        forced_fragmentation_ratio: 1.0,
    };

    let mut doc = Document::open(path).unwrap();
    match doc.try_insert(TextPosition::new(0, 0), "[qem]") {
        Ok(_) => {}
        Err(DocumentError::Write { .. }) => return,
        Err(err) => panic!("unexpected insert error: {err}"),
    }
    assert_eq!(doc.backing(), DocumentBacking::PieceTable);

    let maintenance = doc.maintenance_status_with_policy(policy);
    let stats = maintenance
        .fragmentation_stats()
        .expect("piece-table maintenance stats");
    let recommendation = maintenance
        .compaction_recommendation()
        .expect("piece-table maintenance recommendation");

    assert_eq!(maintenance.backing(), DocumentBacking::PieceTable);
    assert!(maintenance.has_piece_table());
    assert!(maintenance.has_fragmentation_stats());
    assert!(stats.piece_count() > 1);
    assert!(maintenance.is_compaction_recommended());
    assert_eq!(
        maintenance.compaction_urgency(),
        Some(CompactionUrgency::Deferred)
    );
    assert_eq!(recommendation.urgency(), CompactionUrgency::Deferred);
    assert_eq!(recommendation.stats(), stats);
    assert_eq!(
        maintenance.recommended_action(),
        MaintenanceAction::IdleCompaction
    );
    assert!(maintenance.should_run_idle_compaction());
    assert!(!maintenance.should_wait_for_explicit_compaction());
}

#[test]
fn edit_capability_reports_promotions_and_current_backing() {
    let doc = Document::new();
    assert_eq!(
        doc.edit_capability_at(TextPosition::new(0, 0)),
        EditCapability::RequiresPromotion {
            from: DocumentBacking::Mmap,
            to: DocumentBacking::Rope,
        }
    );

    let mut edited = Document::new();
    let _ = edited.try_insert(TextPosition::new(0, 0), "alpha").unwrap();
    assert_eq!(
        edited.edit_capability_at(TextPosition::new(0, 2)),
        EditCapability::Editable {
            backing: DocumentBacking::Rope,
        }
    );
}

#[test]
fn edit_capability_clamps_large_mmap_positions_to_eof_before_reporting_promotion() {
    let dir = std::env::temp_dir().join(format!("qem-doc-edit-capability-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("huge.bin");
    let file = std::fs::File::create(&path).unwrap();
    file.set_len((MAX_ROPE_EDIT_FILE_BYTES + 1) as u64).unwrap();
    drop(file);

    let doc = Document::open(path.clone()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while doc.is_indexing() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    let capability = doc.edit_capability_at(TextPosition::new(LINE_LENGTHS_MAX_SYNC_LINES + 1, 0));

    assert_eq!(
        capability,
        EditCapability::RequiresPromotion {
            from: DocumentBacking::Mmap,
            to: DocumentBacking::PieceTable,
        }
    );
    assert!(capability.is_editable());
    assert_eq!(capability.reason(), None);

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn edit_capability_reports_partial_piece_table_promotion_limits() {
    let dir = std::env::temp_dir().join(format!(
        "qem-doc-edit-capability-piece-table-{}",
        std::process::id()
    ));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("small.txt");
    std::fs::write(&path, b"x").unwrap();
    let storage = FileStorage::open(&path).unwrap();

    let doc = Document {
        path: Some(path.clone()),
        storage: Some(storage.clone()),
        line_offsets: Arc::new(RwLock::new(LineOffsets::default())),
        disk_index: None,
        indexing: Arc::new(AtomicBool::new(false)),
        indexing_started: None,
        file_len: MAX_ROPE_EDIT_FILE_BYTES + 1,
        indexed_bytes: Arc::new(AtomicUsize::new(0)),
        avg_line_len: Arc::new(AtomicUsize::new(AVG_LINE_LEN_ESTIMATE)),
        line_ending: LineEnding::Lf,
        encoding: DocumentEncoding::utf8(),
        encoding_origin: DocumentEncodingOrigin::NewDocument,
        encoding_engine: super::encoding_engine::engine_for_encoding(DocumentEncoding::utf8()),
        decoding_had_errors: false,
        preserve_save_error_cache: Cell::new(None),
        rope_file_len_cache: Cell::new(None),
        rope: None,
        rope_history: RopeHistory::new(),
        piece_table: Some(PieceTable {
            original: storage,
            add: Vec::new(),
            pieces: PieceTree::from_pieces(vec![Piece {
                src: PieceSource::Original,
                start: 0,
                len: 1,
                line_breaks: 0,
            }]),
            engine: encoding_engine::engine_for_encoding(DocumentEncoding::utf8()),
            known_line_count: 1,
            exact_base_line_breaks: None,
            exact_base_total_lines: None,
            known_byte_len: 1,
            total_len: MAX_ROPE_EDIT_FILE_BYTES + 1,
            full_index: false,
            encoding_origin: DocumentEncodingOrigin::Utf8FastPath,
            decoding_had_errors: false,
            pending_session_flush: false,
            pending_session_edits: 0,
            last_session_flush: None,
            edit_batch_depth: 0,
            edit_batch_dirty: false,
        }),
        dirty: true,
    };

    assert_eq!(
        doc.edit_capability_at(TextPosition::new(0, 0)),
        EditCapability::Editable {
            backing: DocumentBacking::PieceTable,
        }
    );
    assert_eq!(
        doc.edit_capability_at(TextPosition::new(1, 0)),
        EditCapability::Unsupported {
            backing: DocumentBacking::PieceTable,
            reason: "document is too large to widen partial piece-table editing beyond the indexed prefix",
        }
    );

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir_all(&dir);
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn randomized_in_memory_edits_match_string_model(
        initial in op_text_strategy(),
        ops in prop::collection::vec(edit_op_strategy(), 1..48),
    ) {
        let mut doc = Document::new();
        let mut expected = String::new();

        let initial_cursor = model_insert(&mut expected, 0, 0, &initial);
        let doc_cursor = doc.try_insert_text_at(0, 0, &initial).unwrap();
        prop_assert_eq!(doc_cursor, initial_cursor);
        assert_doc_matches_model(&doc, &expected);

        for op in &ops {
            apply_op_to_doc(&mut doc, &mut expected, op);
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]

    #[test]
    fn randomized_small_file_roundtrip_matches_model(
        initial in op_text_strategy(),
        use_crlf in any::<bool>(),
        ops in prop::collection::vec(edit_op_strategy(), 1..24),
    ) {
        let dir = tempdir().unwrap();
        let source = dir.path().join("input.txt");
        let saved = dir.path().join("output.txt");
        let requested_line_ending = if use_crlf { LineEnding::Crlf } else { LineEnding::Lf };

        let mut expected = String::new();
        let _ = model_insert(&mut expected, 0, 0, &initial);
        let source_text = render_with_line_ending(&expected, requested_line_ending);
        let persisted_line_ending = detect_line_ending(source_text.as_bytes());
        std::fs::write(&source, source_text).unwrap();

        let mut doc = Document::open(&source).unwrap();
        for op in &ops {
            apply_op_to_doc(&mut doc, &mut expected, op);
        }

        doc.save_to(&saved).unwrap();
        let reopened = Document::open(&saved).unwrap();
        let rendered = render_with_line_ending(&expected, persisted_line_ending);
        let live_text = if doc.has_edit_buffer() {
            expected.clone()
        } else {
            rendered.clone()
        };

        prop_assert_eq!(reopened.line_ending(), detect_line_ending(rendered.as_bytes()));
        prop_assert_eq!(reopened.text_lossy(), rendered);
        prop_assert_eq!(doc.text_lossy(), live_text);
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(10))]

    #[test]
    fn randomized_piece_table_recovery_and_history_roundtrip(
        ops in prop::collection::vec(file_backed_edit_op_strategy(), 1..12),
    ) {
        let dir = tempdir().unwrap();
        let path = dir.path().join("piece-table.txt");
        write_disk_backed_fixture(&path);

        let mut expected = std::fs::read_to_string(&path).unwrap();
        let mut states = vec![expected.clone()];

        {
            let mut doc = Document::open(path.clone()).unwrap();
            for op in &ops {
                let before = expected.clone();
                apply_op_to_doc_text_only(&mut doc, &mut expected, op);
                assert!(
                    doc.piece_table.is_some(),
                    "large file edits should stay on the piece-table path"
                );
                assert!(
                    !doc.has_rope(),
                    "low-line edits should not require rope promotion"
                );
                if expected != before {
                    states.push(expected.clone());
                }
            }

            doc.flush_session().unwrap();
        }

        let mut recovered = Document::open(path.clone()).unwrap();
        assert!(recovered.is_dirty());
        assert!(
            recovered.piece_table.is_some(),
            "recovered document should restore piece-table session"
        );
        assert!(
            !recovered.has_rope(),
            "recovered document should keep the piece-table session"
        );
        assert_eq!(recovered.text_lossy(), expected);

        for state in states[..states.len().saturating_sub(1)].iter().rev() {
            assert!(recovered.try_undo().unwrap());
            assert_eq!(recovered.text_lossy(), *state);
        }
        assert!(!recovered.try_undo().unwrap());

        for state in states.iter().skip(1) {
            assert!(recovered.try_redo().unwrap());
            assert_eq!(recovered.text_lossy(), *state);
        }
        assert!(!recovered.try_redo().unwrap());
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(8))]

    #[test]
    fn randomized_piece_table_save_to_clears_sessions_and_reopens_clean(
        ops in prop::collection::vec(file_backed_edit_op_strategy(), 1..12),
    ) {
        let dir = tempdir().unwrap();
        let source = dir.path().join("source.txt");
        let saved = dir.path().join("saved.txt");
        write_disk_backed_fixture(&source);

        let original = std::fs::read_to_string(&source).unwrap();
        let mut expected = original.clone();

        {
            let mut doc = Document::open(source.clone()).unwrap();
            for op in &ops {
                apply_op_to_doc_text_only(&mut doc, &mut expected, op);
                assert!(
                    doc.piece_table.is_some(),
                    "large file edits should stay on the piece-table path"
                );
                assert!(
                    !doc.has_rope(),
                    "save path test should stay on the piece-table path"
                );
            }

            doc.flush_session().unwrap();
            assert!(
                editlog_path(&source).exists(),
                "flush_session should materialize a recoverable sidecar before save"
            );

            doc.save_to(&saved).unwrap();

            assert_eq!(doc.path(), Some(saved.as_path()));
            assert!(!doc.is_dirty());
            assert!(!doc.has_edit_buffer());
            assert_eq!(doc.backing(), DocumentBacking::Mmap);
            assert_eq!(doc.text_lossy(), expected);
            assert_eq!(std::fs::read_to_string(&saved).unwrap(), expected);
            assert_eq!(std::fs::read_to_string(&source).unwrap(), original);
            assert!(
                !editlog_path(&source).exists(),
                "save_to should clear the old recoverable sidecar"
            );
            assert!(
                !editlog_path(&saved).exists(),
                "save_to should not leave a recoverable sidecar at the destination"
            );
        }

        let reopened = Document::open(saved.clone()).unwrap();
        assert_eq!(reopened.text_lossy(), expected);
        assert!(!reopened.is_dirty());
        assert!(!reopened.has_edit_buffer());
        assert!(
            !editlog_path(&source).exists(),
            "reopening the saved file must not revive the old sidecar"
        );
        assert!(
            !editlog_path(&saved).exists(),
            "clean reopened save must not create a recoverable sidecar"
        );
    }
}
// ---------------------------------------------------------------------
// try_replace_range / try_delete_range for non-UTF-8 documents.
// Each test opens a document through
// the Class A / Class B native path, runs an encoded edit, and asserts
// that `save_to` round-trips the resulting target-encoding bytes
// byte-identically through `decode_with_bom_removal`.
// ---------------------------------------------------------------------

#[test]
fn try_replace_range_encoded_windows_1251_replaces_ascii_substring() {
    // A windows-1251 document opened through the Class A
    // native path runs every edit through `try_replace_range_encoded`,
    // which composes encoded delete and encoded insert. The replacement
    // bytes land in the piece-tree's add buffer in the target encoding
    // (single-byte ASCII supersets are byte-identical to ASCII for the
    // ASCII subset, so the assertion is on the literal cyrillic and
    // ASCII bytes).
    let dir = tempdir().unwrap();
    let path = dir.path().join("cp1251-replace.txt");
    let saved = dir.path().join("cp1251-replace-saved.txt");
    let encoding = DocumentEncoding::from_label("windows-1251").unwrap();

    let source = "hello мир\n";
    let (bytes, used, had_errors) = WINDOWS_1251.encode(source);
    assert_eq!(used, WINDOWS_1251);
    assert!(!had_errors);
    std::fs::write(&path, bytes.as_ref()).unwrap();

    let mut doc = Document::open_with_encoding(path, encoding).unwrap();
    assert_eq!(doc.encoding(), encoding);

    // Replace "hello" (5 ASCII chars at column 0) with "Привет" — six
    // cyrillic glyphs, each one byte in windows-1251.
    let cursor = doc
        .try_replace_range(0, 0, 5, "Привет")
        .expect("encoded replace must succeed");
    assert_eq!(cursor, (0, 6));
    assert!(doc.is_dirty());

    doc.save_to(&saved).unwrap();
    let raw = std::fs::read(&saved).unwrap();
    let (decoded, used, had_errors) = WINDOWS_1251.decode(&raw);
    assert_eq!(used, WINDOWS_1251);
    assert!(!had_errors);
    assert_eq!(decoded, "Привет мир\n");
}

#[test]
fn try_replace_range_encoded_windows_1251_replacing_with_empty_deletes_range() {
    // A non-UTF-8 replace with an empty replacement string
    // must be byte-equivalent to a delete. The piece-tree byte range is
    // aligned through `align_byte_offset` (a no-op for Class A) so the
    // delete spans exactly five ASCII bytes.
    let dir = tempdir().unwrap();
    let path = dir.path().join("cp1251-delete.txt");
    let saved = dir.path().join("cp1251-delete-saved.txt");
    let encoding = DocumentEncoding::from_label("windows-1251").unwrap();

    let source = "hello мир\n";
    let (bytes, used, had_errors) = WINDOWS_1251.encode(source);
    assert_eq!(used, WINDOWS_1251);
    assert!(!had_errors);
    std::fs::write(&path, bytes.as_ref()).unwrap();

    let mut doc = Document::open_with_encoding(path, encoding).unwrap();
    let cursor = doc
        .try_replace_range(0, 0, 6, "")
        .expect("encoded delete via replace must succeed");
    assert_eq!(cursor, (0, 0));
    assert!(doc.is_dirty());

    doc.save_to(&saved).unwrap();
    let raw = std::fs::read(&saved).unwrap();
    let (decoded, used, had_errors) = WINDOWS_1251.decode(&raw);
    assert_eq!(used, WINDOWS_1251);
    assert!(!had_errors);
    assert_eq!(decoded, "мир\n");
}

#[test]
fn try_replace_range_encoded_utf16_le_replaces_ascii_substring() {
    // UTF-16 LE replace runs through the encoded
    // path with `align_byte_offset` rounding the deletion endpoints
    // onto even byte boundaries before `PieceTable::delete_range` runs.
    // The encoded insert encodes the replacement via `str::encode_utf16`
    // and writes the LE bytes verbatim into the piece-tree add buffer.
    //
    // `len_chars` is engine-aware: one UTF-16 BMP cell
    // counts as one text unit (two bytes), so replacing the full "hi"
    // line content takes `len_chars = 2`. The piece-tree's
    // `*_with_engine` walkers map (0, 0) → byte 0 and advance two text
    // units to byte 4, exactly the encoded "hi" range; the encoded
    // insert then writes "AB" verbatim.
    let dir = tempdir().unwrap();
    let path = dir.path().join("utf16le-replace.txt");
    let saved = dir.path().join("utf16le-replace-saved.txt");
    let encoding = DocumentEncoding::from_label("UTF-16LE").unwrap();

    // "hi\n" in UTF-16LE: 6 bytes total (h=68 00, i=69 00, \n=0A 00).
    let source = "hi\n";
    let mut bytes = Vec::new();
    for unit in source.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    std::fs::write(&path, &bytes).unwrap();

    let mut doc = Document::open_with_encoding(path, encoding).unwrap();
    assert_eq!(doc.encoding(), encoding);

    // Replace the two BMP cells of "hi" — `len_chars = 2`.
    let cursor = doc
        .try_replace_range(0, 0, 2, "AB")
        .expect("encoded replace must succeed for UTF-16LE");
    assert!(doc.is_dirty());
    assert_eq!(cursor, (0, 2));

    doc.save_to(&saved).unwrap();
    let raw = std::fs::read(&saved).unwrap();
    let (decoded, had_errors) = encoding_rs::UTF_16LE.decode_with_bom_removal(&raw);
    assert!(!had_errors);
    assert_eq!(decoded, "AB\n");
}

#[test]
fn try_replace_range_encoded_utf16_le_pure_delete_keeps_alignment() {
    // Deleting a 2-text-unit range on a UTF-16LE
    // document removes the four bytes encoding "bc" through
    // `PieceTable::delete_range`. `align_byte_offset` keeps both
    // endpoints even-aligned (byte 2 → 2, byte 6 → 6); the engine-
    // aware text-unit walker advances exactly two BMP cells from byte
    // 2 to byte 6.
    let dir = tempdir().unwrap();
    let path = dir.path().join("utf16le-delete.txt");
    let saved = dir.path().join("utf16le-delete-saved.txt");
    let encoding = DocumentEncoding::from_label("UTF-16LE").unwrap();

    let source = "abcd\n";
    let mut bytes = Vec::new();
    for unit in source.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    std::fs::write(&path, &bytes).unwrap();

    let mut doc = Document::open_with_encoding(path, encoding).unwrap();
    let cursor = doc
        .try_replace_range(0, 1, 2, "")
        .expect("encoded delete via replace must succeed");
    assert_eq!(cursor, (0, 1));
    assert!(doc.is_dirty());

    doc.save_to(&saved).unwrap();
    let raw = std::fs::read(&saved).unwrap();
    let (decoded, had_errors) = encoding_rs::UTF_16LE.decode_with_bom_removal(&raw);
    assert!(!had_errors);
    assert_eq!(decoded, "ad\n");
}

#[test]
fn try_backspace_at_encoded_utf16_le_walks_back_one_bmp_cell() {
    // Backspace on UTF-16 LE removes exactly one
    // BMP cell (two bytes) through the engine's `step_backward`. The
    // piece-tree's `delete_range` runs on aligned offsets so adjacent
    // cells stay intact.
    let dir = tempdir().unwrap();
    let path = dir.path().join("utf16le-backspace.txt");
    let saved = dir.path().join("utf16le-backspace-saved.txt");
    let encoding = DocumentEncoding::from_label("UTF-16LE").unwrap();

    let source = "abc\n";
    let mut bytes = Vec::new();
    for unit in source.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    std::fs::write(&path, &bytes).unwrap();

    let mut doc = Document::open_with_encoding(path, encoding).unwrap();
    let (edited, line0, col0) = doc
        .try_backspace_at(0, 3)
        .expect("encoded backspace must succeed for UTF-16LE");
    assert!(edited);
    assert_eq!((line0, col0), (0, 2));
    assert!(doc.is_dirty());

    doc.save_to(&saved).unwrap();
    let raw = std::fs::read(&saved).unwrap();
    let (decoded, had_errors) = encoding_rs::UTF_16LE.decode_with_bom_removal(&raw);
    assert!(!had_errors);
    assert_eq!(decoded, "ab\n");
}

#[test]
fn try_replace_range_encoded_unrepresentable_text_leaves_document_unchanged() {
    // Replacing a range with text that contains a Unicode scalar
    // unrepresentable in the document's target encoding must return
    // `UnrepresentableText` *before* the existing range is removed.
    // The document must remain dirty-clean and byte-identical to the
    // original.
    let dir = tempdir().unwrap();
    let path = dir.path().join("cp1251-replace-fail.txt");
    let encoding = DocumentEncoding::from_label("windows-1251").unwrap();

    let source = "hello\n";
    let (bytes, used, had_errors) = WINDOWS_1251.encode(source);
    assert_eq!(used, WINDOWS_1251);
    assert!(!had_errors);
    let original_bytes = bytes.into_owned();
    std::fs::write(&path, &original_bytes).unwrap();

    let mut doc = Document::open_with_encoding(&path, encoding).unwrap();
    let pre_total = doc.file_len();
    // 「日本」 — Japanese kanji, not representable in windows-1251.
    let result = doc.try_replace_range(0, 0, 5, "日本");
    assert!(matches!(
        result,
        Err(DocumentError::Encoding {
            reason: DocumentEncodingErrorKind::UnrepresentableText,
            ..
        })
    ));
    // No mutation on unrepresentable input.
    assert!(!doc.is_dirty());
    assert_eq!(doc.file_len(), pre_total);
    let saved = dir.path().join("cp1251-replace-fail-saved.txt");
    doc.save_to(&saved).unwrap();
    let after = std::fs::read(&saved).unwrap();
    assert_eq!(after, original_bytes);
}

#[test]
fn try_replace_range_encoded_shift_jis_replaces_2byte_kanji_substring() {
    // Shift_JIS replace runs through
    // `try_replace_range_encoded`. The encoded delete walks Shift_JIS
    // 2-byte cells via `MultiByteEngine`, so `len_chars = 2` removes
    // exactly two characters (four bytes). The encoded insert encodes
    // ASCII directly into the piece-tree's add buffer.
    let dir = tempdir().unwrap();
    let path = dir.path().join("shift_jis-replace.txt");
    let saved = dir.path().join("shift_jis-replace-saved.txt");
    let encoding = DocumentEncoding::from_label("Shift_JIS").unwrap();

    // 「あい」 — two Hiragana characters in Shift_JIS, two bytes each
    // (82 A0, 82 A2), followed by LF.
    let source = "あい\n";
    let (bytes, used, had_errors) = encoding_rs::SHIFT_JIS.encode(source);
    assert_eq!(used, encoding_rs::SHIFT_JIS);
    assert!(!had_errors);
    std::fs::write(&path, bytes.as_ref()).unwrap();

    let mut doc = Document::open_with_encoding(path, encoding).unwrap();
    let cursor = doc
        .try_replace_range(0, 0, 2, "ok")
        .expect("encoded replace must succeed for Shift_JIS");
    assert_eq!(cursor, (0, 2));
    assert!(doc.is_dirty());

    doc.save_to(&saved).unwrap();
    let raw = std::fs::read(&saved).unwrap();
    let (decoded, used, had_errors) = encoding_rs::SHIFT_JIS.decode(&raw);
    assert_eq!(used, encoding_rs::SHIFT_JIS);
    assert!(!had_errors);
    assert_eq!(decoded, "ok\n");
}

#[test]
fn try_backspace_at_encoded_shift_jis_walks_back_one_kanji_cell() {
    // Backspace on Shift_JIS walks back one
    // 2-byte cell through `MultiByteEngine::step_backward` (which
    // scans forward from the line anchor and reports the last full
    // character). The deletion removes both bytes of the trailing
    // Hiragana, leaving the first Hiragana cell intact.
    let dir = tempdir().unwrap();
    let path = dir.path().join("shift_jis-backspace.txt");
    let saved = dir.path().join("shift_jis-backspace-saved.txt");
    let encoding = DocumentEncoding::from_label("Shift_JIS").unwrap();

    let source = "あい\n";
    let (bytes, used, had_errors) = encoding_rs::SHIFT_JIS.encode(source);
    assert_eq!(used, encoding_rs::SHIFT_JIS);
    assert!(!had_errors);
    std::fs::write(&path, bytes.as_ref()).unwrap();

    let mut doc = Document::open_with_encoding(path, encoding).unwrap();
    let (edited, line0, col0) = doc
        .try_backspace_at(0, 2)
        .expect("encoded backspace must succeed for Shift_JIS");
    assert!(edited);
    assert_eq!((line0, col0), (0, 1));

    doc.save_to(&saved).unwrap();
    let raw = std::fs::read(&saved).unwrap();
    let (decoded, used, had_errors) = encoding_rs::SHIFT_JIS.decode(&raw);
    assert_eq!(used, encoding_rs::SHIFT_JIS);
    assert!(!had_errors);
    assert_eq!(decoded, "あ\n");
}

#[test]
fn piece_table_undo_redo_restores_known_length_on_full_index() {
    // A file above `PIECE_TABLE_MIN_BYTES` gets a fully indexed piece
    // table on first edit. Regression: undo clamped `known_byte_len` down
    // and redo never restored it, so `line_range` ended the last line at
    // the stale boundary and the document tail disappeared from
    // viewport / read answers until the next edit.
    let dir = std::env::temp_dir().join(format!("qem-undo-full-index-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("full-index.log");
    let content = format!("{}\nTAIL\n", "x".repeat(1_600_000));
    std::fs::write(&path, &content).unwrap();

    let mut doc = Document::open(&path).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while doc.is_indexing() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }

    doc.try_insert(TextPosition::new(0, 0), "[qem]\n").unwrap();
    assert!(
        doc.piece_table.is_some(),
        "precondition: the edit created a piece table"
    );
    assert!(
        doc.piece_table.as_ref().unwrap().full_index,
        "precondition: the line table fits the budget and is fully indexed"
    );

    doc.try_undo().unwrap();
    doc.try_redo().unwrap();

    // The state-level invariant behind the read-path damage: a full-index
    // piece table must track the document length exactly after undo->redo.
    {
        let pt = doc.piece_table.as_ref().unwrap();
        assert_eq!(
            pt.known_byte_len,
            pt.total_len(),
            "full-index known boundary went stale across undo->redo"
        );
    }

    let marker_line0 = doc
        .line_count()
        .exact()
        .expect("full-index document knows its exact line count")
        // The trailing newline means the exact count includes one virtual
        // empty line after `TAIL`; the marker sits one above it.
        .saturating_sub(2);
    let viewport = doc.read_viewport(ViewportRequest::new(marker_line0, 1));
    let row = viewport.rows().first().expect("marker line row");
    assert_eq!(
        row.text(),
        "TAIL",
        "undo→redo must keep the last content line intact"
    );

    // The line/col → byte mapping clamps its backward scan at the known
    // region boundary; a stale boundary must not truncate the tail.
    let tail = doc.read_text(TextRange::new(TextPosition::new(marker_line0, 0), 4));
    assert_eq!(
        tail.text(),
        "TAIL",
        "read_text from line/col must see the tail"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn piece_table_partial_undo_restores_known_boundary() {
    // A partial window (full_index = false, known < total) records its
    // boundary in the history alongside each root; undoing a delete must
    // restore the pre-delete boundary instead of keeping the clamped one
    // (the clamp left the tail beyond `known` and truncated reads of the
    // last known lines).
    let dir = std::env::temp_dir().join(format!("qem-pt-partial-undo-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("partial.log");
    let line = format!(
        "{}
",
        "y".repeat(99)
    );
    std::fs::write(&path, line.repeat(4096)).unwrap();

    let storage = FileStorage::open(&path).unwrap();
    // The known region covers only the first 2000 lines: a partial window
    // like the disk-index far-edit plan creates.
    let mut pt = PieceTable::new(storage, vec![100; 2000], false);
    assert_eq!(pt.total_len(), 409_600);
    assert_eq!(pt.known_byte_len, 200_000);
    assert!(!pt.full_index);

    pt.insert_bytes(50, b"[qem]").unwrap();
    assert_eq!(pt.known_byte_len, 200_005);
    pt.delete_range(100_000, 50).unwrap();
    assert_eq!(pt.known_byte_len, 199_955);

    assert!(pt.undo().unwrap());
    assert_eq!(
        pt.known_byte_len, 200_005,
        "undo must restore the pre-delete known boundary"
    );
    assert!(pt.redo().unwrap());
    assert_eq!(pt.known_byte_len, 199_955);

    let _ = std::fs::remove_dir_all(&dir);
}

// Regression for the third audit round: a windows-1251 (Class A)
// document edited into a piece table must decode its line reads through
// the document's engine. The naive UTF-8 decode returned U+FFFD garbage
// for every non-UTF-8 piece-table line after the first edit.
#[test]
fn non_utf8_piece_table_line_reads_decode_through_engine() {
    let dir = std::env::temp_dir().join(format!("qem-cp1251-piece-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("cp1251.txt");
    // windows-1251 for the word "привет" followed by four exclamation marks.
    std::fs::write(
        &path,
        [0xEF, 0xF0, 0xE8, 0xE2, 0xE5, 0xF2, 0x21, 0x21, 0x21, 0x21],
    )
    .unwrap();

    let mut doc = Document::open_with_encoding(
        &path,
        DocumentEncoding::from_label("windows-1251").expect("cp1251 label"),
    )
    .unwrap();
    doc.try_insert(TextPosition::new(0, 0), "[qem]").unwrap();
    assert!(
        doc.piece_table.is_some(),
        "precondition: non-UTF-8 edits create a piece table"
    );

    // Columns are bytes for Class A: 5 + 6 + 4 = 15 text units.
    assert_eq!(doc.line_len_chars(0), 15);

    let slice = doc.read_viewport(ViewportRequest::new(0, 1));
    assert_eq!(
        slice.rows().first().map(|r| r.text().to_owned()).as_deref(),
        Some("[qem]\u{43f}\u{440}\u{438}\u{432}\u{435}\u{442}!!!!")
    );

    // Inserting at column 5 lands after "[qem]п" - Class A
    // columns are bytes, so the cursor and the rendered line are exact.
    let cursor = doc.try_insert(TextPosition::new(0, 5), "|").unwrap();
    assert_eq!(cursor, TextPosition::new(0, 6));
    let slice = doc.read_viewport(ViewportRequest::new(0, 1));
    assert_eq!(
        slice.rows().first().map(|r| r.text().to_owned()).as_deref(),
        Some("[qem]|\u{43f}\u{440}\u{438}\u{432}\u{435}\u{442}!!!!")
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn piece_table_composite_undo_restores_partial_known_boundary() {
    // Regression for the batch-boundary bug: a composite replace (delete +
    // insert inside one edit batch) used to overwrite the pre-batch
    // history entry's known boundary, so undoing the composite restored a
    // stale boundary on partial-window documents. Both directions are
    // pinned: undo -> pre-batch boundary, redo -> post-batch boundary.
    let dir = std::env::temp_dir().join(format!("qem-pt-composite-undo-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("partial.log");
    let line = format!(
        "{}
",
        "y".repeat(99)
    );
    std::fs::write(&path, line.repeat(4096)).unwrap();

    let storage = FileStorage::open(&path).unwrap();
    // Partial window: known covers only the first 2000 of 4096 lines.
    let mut pt = PieceTable::new(storage, vec![100; 2000], false);
    assert_eq!(pt.known_byte_len, 200_000);

    // Composite replace inside the known region with ins != del:
    // 60 bytes deleted, 5 bytes inserted. The line at the edit point is
    // line 1000 (bytes 100_000..100_100); the replace spans 60 bytes
    // starting at column 0 of that line, so it stays inside the line.
    let line0 = 1000;
    pt.replace_range_at(LineEnding::Lf, line0, 0, 60, "[qem]")
        .unwrap();
    let post_batch = pt.known_byte_len;
    assert_ne!(
        post_batch, 200_000,
        "precondition: the batch changed the boundary"
    );

    assert!(pt.undo().unwrap());
    assert_eq!(
        pt.known_byte_len, 200_000,
        "undo must restore the pre-batch boundary, got the post-batch one"
    );
    assert!(pt.redo().unwrap());
    assert_eq!(pt.known_byte_len, post_batch);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn small_rope_document_undo_redo_round_trip() {
    let dir = std::env::temp_dir().join(format!("qem-undo-small-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("small.txt");
    std::fs::write(&path, b"alpha\nbeta\n").unwrap();

    let mut doc = Document::open(&path).unwrap();
    assert!(
        doc.rope.is_none(),
        "precondition: small file is mmap-backed"
    );

    let cursor = doc.try_insert(TextPosition::new(0, 0), "X").unwrap();
    assert_eq!(cursor, TextPosition::new(0, 1));
    assert!(doc.rope.is_some());
    assert_eq!(doc.text_lossy(), "Xalpha\nbeta\n");

    let undo = doc.try_undo_with_cursor().unwrap();
    assert!(undo.changed());
    assert_eq!(undo.cursor(), Some(TextPosition::new(0, 0)));
    assert_eq!(doc.text_lossy(), "alpha\nbeta\n");
    assert!(doc.is_dirty(), "undo keeps the document dirty");

    let redo = doc.try_redo_with_cursor().unwrap();
    assert!(redo.changed());
    assert_eq!(redo.cursor(), Some(TextPosition::new(0, 1)));
    assert_eq!(doc.text_lossy(), "Xalpha\nbeta\n");

    assert!(
        doc.try_undo().unwrap(),
        "history must still hold the first edit"
    );
    assert!(
        !doc.try_undo().unwrap(),
        "history exhausted after both undos"
    );

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn untitled_document_undo_redo_round_trip() {
    let mut doc = Document::new();
    assert!(!doc.try_undo().unwrap(), "nothing to undo before any edit");

    doc.try_insert_text_at(0, 0, "hello").unwrap();
    doc.try_backspace_at(0, 5).unwrap();
    assert_eq!(doc.text_lossy(), "hell");

    let undo = doc.try_undo_with_cursor().unwrap();
    assert!(undo.changed());
    assert_eq!(undo.cursor(), Some(TextPosition::new(0, 5)));
    assert_eq!(doc.text_lossy(), "hello");

    let redo = doc.try_redo_with_cursor().unwrap();
    assert!(redo.changed());
    assert_eq!(redo.cursor(), Some(TextPosition::new(0, 4)));
    assert_eq!(doc.text_lossy(), "hell");
}

#[test]
fn fresh_edit_after_undo_drops_redo() {
    let mut doc = Document::new();
    doc.try_insert_text_at(0, 0, "ab").unwrap();
    assert!(doc.try_undo().unwrap());
    assert_eq!(doc.text_lossy(), "");
    doc.try_insert_text_at(0, 0, "c").unwrap();
    assert_eq!(doc.text_lossy(), "c");
    assert!(
        !doc.try_redo().unwrap(),
        "redo stack must drop after a new edit"
    );
}

#[test]
fn composite_replace_range_undoes_in_one_step() {
    let dir = std::env::temp_dir().join(format!("qem-undo-replace-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("replace.txt");
    std::fs::write(&path, b"alpha\nbeta\n").unwrap();

    let mut doc = Document::open(&path).unwrap();
    let cursor = doc.try_replace_range(0, 0, 5, "omega").unwrap();
    assert_eq!(cursor, (0, 5));
    assert_eq!(doc.text_lossy(), "omega\nbeta\n");

    let undo = doc.try_undo_with_cursor().unwrap();
    assert!(undo.changed());
    assert_eq!(doc.text_lossy(), "alpha\nbeta\n");
    assert_eq!(undo.cursor(), Some(TextPosition::new(0, 5)));

    let redo = doc.try_redo_with_cursor().unwrap();
    assert!(redo.changed());
    assert_eq!(redo.cursor(), Some(TextPosition::new(0, 5)));
    assert_eq!(doc.text_lossy(), "omega\nbeta\n");

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn backspace_delete_forward_undo_round_trip() {
    let mut doc = Document::new();
    doc.try_insert_text_at(0, 0, "abc\n").unwrap();

    doc.try_delete_forward_at(0, 0).unwrap();
    assert_eq!(doc.text_lossy(), "bc\n");
    doc.try_backspace_at(0, 1).unwrap();
    assert_eq!(doc.text_lossy(), "c\n");

    assert!(doc.try_undo().unwrap());
    assert_eq!(doc.text_lossy(), "bc\n");
    assert!(doc.try_undo().unwrap());
    assert_eq!(doc.text_lossy(), "abc\n");
    assert!(doc.try_redo().unwrap());
    assert_eq!(doc.text_lossy(), "bc\n");
    assert!(doc.try_redo().unwrap());
    assert_eq!(doc.text_lossy(), "c\n");
}

#[test]
fn rope_undo_survives_save_and_marks_dirty() {
    let dir = std::env::temp_dir().join(format!("qem-undo-save-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let src = dir.join("source.txt");
    let dst = dir.join("copy.txt");
    std::fs::write(&src, b"alpha\nbeta\n").unwrap();

    let mut doc = Document::open(&src).unwrap();
    doc.try_insert_text_at(0, 0, "X").unwrap();
    doc.save_to(&dst).unwrap();
    assert!(!doc.is_dirty());
    assert_eq!(std::fs::read(&dst).unwrap(), b"Xalpha\nbeta\n");

    let undo = doc.try_undo_with_cursor().unwrap();
    assert!(undo.changed());
    assert_eq!(doc.text_lossy(), "alpha\nbeta\n");
    assert!(
        doc.is_dirty(),
        "undo after save re-marks the document dirty"
    );
    assert_eq!(
        std::fs::read(&dst).unwrap(),
        b"Xalpha\nbeta\n",
        "undo must not touch the saved file"
    );

    let _ = std::fs::remove_file(&src);
    let _ = std::fs::remove_file(&dst);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn piece_table_promotion_rope_history_covers_post_promotion_edits() {
    let dir = std::env::temp_dir().join(format!("qem-undo-promote-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("big.txt");
    let original = b"abc\ndef\n".repeat((PIECE_TABLE_MIN_BYTES / 8) + 1);
    std::fs::write(&path, &original).unwrap();

    let mut doc = Document::open(&path).unwrap();
    doc.try_insert_text_at(0, 0, "X").unwrap();
    assert!(
        doc.piece_table.is_some(),
        "precondition: PIECE_TABLE_MIN_BYTES file edits through the piece table"
    );

    // Piece-tree snapshot history keeps working, without caret bookkeeping.
    let undo = doc.try_undo_with_cursor().unwrap();
    assert!(undo.changed());
    assert_eq!(undo.cursor(), None);
    assert!(doc.try_redo().unwrap());

    // Simulate what an out-of-window edit does on partial-index documents:
    // promote the piece tree into the rope buffer.
    doc.promote_piece_table_to_rope().unwrap();
    assert!(doc.rope.is_some());
    assert!(doc.piece_table.is_none());

    // Post-promotion edits are undoable through rope history.
    doc.try_insert_text_at(0, 0, "Y").unwrap();
    assert!(doc.text_lossy().starts_with("YXabc\n"));
    let undo = doc.try_undo_with_cursor().unwrap();
    assert!(undo.changed());
    assert_eq!(undo.cursor(), Some(TextPosition::new(0, 0)));
    assert!(doc.text_lossy().starts_with("Xabc\n"));

    // Pre-promotion piece-tree history is intentionally not carried over.
    assert!(!doc.try_undo().unwrap());

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn line_ending_probe_is_bounded_when_first_break_is_beyond_window() {
    let dir = std::env::temp_dir().join(format!("qem-probe-bound-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("no-early-newline.bin");
    // The first line break sits 1 MiB past the 16 MiB probe window. The
    // open path must stop at the probe instead of memchr-scanning the rest
    // of the file synchronously (the 50 GiB freeze).
    let mut content = vec![b'a'; super::lifecycle::LINE_ENDING_PROBE_MAX_BYTES + 1024 * 1024];
    content.extend_from_slice(b"\r\ntail\n");
    std::fs::write(&path, &content).unwrap();

    let started = Instant::now();
    let doc = Document::open(&path).unwrap();
    let elapsed = started.elapsed();

    assert_eq!(doc.line_ending(), LineEnding::Lf);
    assert!(
        elapsed < Duration::from_secs(2),
        "open scanned past the probe window in {elapsed:?}"
    );

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn line_ending_probe_still_detects_crlf_within_window() {
    let dir = std::env::temp_dir().join(format!("qem-probe-crlf-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("crlf-head.bin");
    // Past the 8 MiB inline-analysis gate so the probe path is exercised,
    // with CRLF inside the probe window.
    let line = b"alpha\r\nbeta\r\n";
    let content = line.repeat((9 * 1024 * 1024 / line.len()) + 1);
    std::fs::write(&path, &content).unwrap();

    let doc = Document::open(&path).unwrap();

    assert_eq!(doc.line_ending(), LineEnding::Crlf);

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn clamp_position_at_column_zero_skips_line_length_walk() {
    let dir = std::env::temp_dir().join(format!("qem-clamp-col0-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("no-newline.bin");
    // A single line spanning the whole file: computing its length requires a
    // full-file walk, so clamping a column-0 caret must not need it.
    std::fs::write(&path, vec![b'a'; 3 * 1024 * 1024]).unwrap();

    let doc = Document::open(&path).unwrap();
    assert_eq!(
        doc.clamp_position(TextPosition::new(0, 0)),
        TextPosition::new(0, 0)
    );

    // Non-zero columns still clamp to the real (O(line)) line length.
    let clamped = doc.clamp_position(TextPosition::new(0, usize::MAX));
    assert_eq!(clamped, TextPosition::new(0, 3 * 1024 * 1024));

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn partial_piece_table_window_does_not_claim_exact_line_count() {
    let dir = std::env::temp_dir().join(format!("qem-partial-window-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("windowed.bin");
    let file_len = 2 * 1024 * 1024;
    std::fs::write(&path, vec![b'a'; file_len]).unwrap();
    let storage = FileStorage::open(&path).unwrap();

    // A bounded window covering only the first 512 KiB of the 2 MiB file:
    // boundaries for the tail do not exist yet.
    let window_lines = 100usize;
    let line_lengths = vec![512 * 1024 / window_lines; window_lines];
    let piece_table = PieceTable::new_with_encoding_state(
        storage.clone(),
        line_lengths,
        false,
        None,
        DocumentEncodingOrigin::Utf8FastPath,
        false,
    );
    assert!(piece_table.known_byte_len() < piece_table.total_len());

    let doc = Document {
        path: Some(path.clone()),
        storage: Some(storage),
        line_offsets: Arc::new(RwLock::new(LineOffsets::new_for_file_len(file_len))),
        disk_index: None,
        indexing: Arc::new(AtomicBool::new(false)),
        indexing_started: None,
        file_len,
        indexed_bytes: Arc::new(AtomicUsize::new(file_len)),
        avg_line_len: Arc::new(AtomicUsize::new(AVG_LINE_LEN_ESTIMATE)),
        line_ending: LineEnding::Lf,
        encoding: DocumentEncoding::utf8(),
        encoding_origin: DocumentEncodingOrigin::Utf8FastPath,
        encoding_engine: super::encoding_engine::engine_for_encoding(DocumentEncoding::utf8()),
        decoding_had_errors: false,
        preserve_save_error_cache: Cell::new(None),
        rope_file_len_cache: Cell::new(None),
        rope: None,
        piece_table: Some(piece_table),
        rope_history: RopeHistory::new(),
        dirty: false,
    };

    // The window count must never surface as the exact document total: doing
    // so capped scroll ranges at the window size after the first edit.
    assert!(doc.exact_line_count().is_none());
    assert!(!doc.is_line_count_exact());
    // The scroll estimate extrapolates the tail from the window's own
    // average line length and stays well above the window.
    assert!(doc.display_line_count() > window_lines);

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn partial_piece_table_covering_whole_file_keeps_exact_count() {
    let dir = std::env::temp_dir().join(format!("qem-partial-full-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("covered.bin");
    let line = b"abc\n";
    let content_len = 1024 * 1024;
    let line_count = content_len / line.len();
    std::fs::write(&path, line.repeat(line_count)).unwrap();
    let storage = FileStorage::open(&path).unwrap();

    // full_index=false, but the boundary list covers every line of the file:
    // the delta math over the base total stays exact.
    let line_lengths = vec![line.len(); line_count];
    let piece_table = PieceTable::new_with_encoding_state(
        storage,
        line_lengths,
        false,
        Some(line_count),
        DocumentEncodingOrigin::Utf8FastPath,
        false,
    );

    let doc = Document {
        path: Some(path.clone()),
        storage: None,
        line_offsets: Arc::new(RwLock::new(LineOffsets::new_for_file_len(content_len))),
        disk_index: None,
        indexing: Arc::new(AtomicBool::new(false)),
        indexing_started: None,
        file_len: content_len,
        indexed_bytes: Arc::new(AtomicUsize::new(content_len)),
        avg_line_len: Arc::new(AtomicUsize::new(AVG_LINE_LEN_ESTIMATE)),
        line_ending: LineEnding::Lf,
        encoding: DocumentEncoding::utf8(),
        encoding_origin: DocumentEncodingOrigin::Utf8FastPath,
        encoding_engine: super::encoding_engine::engine_for_encoding(DocumentEncoding::utf8()),
        decoding_had_errors: false,
        preserve_save_error_cache: Cell::new(None),
        rope_file_len_cache: Cell::new(None),
        rope: None,
        piece_table: Some(piece_table),
        rope_history: RopeHistory::new(),
        dirty: false,
    };

    assert_eq!(doc.exact_line_count(), Some(line_count));
    assert!(doc.is_line_count_exact());

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn windowed_piece_table_edits_far_lines_via_disk_index() {
    let dir = std::env::temp_dir().join(format!("standpad-doc-windowed-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("windowed.log");
    // The file must exceed FULL_SYNC_PIECE_TABLE_MAX_FILE_BYTES (64 MiB) so
    // the synchronous line-length scan is skipped and the checkpoint-window
    // path engages for a far first edit.
    let lines = 1_000_000usize;
    let line_len = 72usize; // "LINE-NNNNNNNN-" + 58 filler bytes + '\n'
    let mut bytes = Vec::with_capacity(lines * line_len);
    for i in 0..lines {
        bytes.extend_from_slice(format!("LINE-{i:07}-{}\n", "x".repeat(58)).as_bytes());
    }
    assert!(bytes.len() > FULL_SYNC_PIECE_TABLE_MAX_FILE_BYTES);
    assert_eq!(bytes.len(), lines * line_len);
    std::fs::write(&path, &bytes).unwrap();
    let storage = FileStorage::open(&path).unwrap();
    let disk = crate::index::DiskLineIndex::build_for_tests(&path, &storage);

    let mut doc = Document {
        path: Some(path.clone()),
        storage: Some(storage),
        line_offsets: Arc::new(RwLock::new(LineOffsets::default())),
        disk_index: Some(disk),
        indexing: Arc::new(AtomicBool::new(false)),
        indexing_started: None,
        file_len: bytes.len(),
        indexed_bytes: Arc::new(AtomicUsize::new(0)),
        avg_line_len: Arc::new(AtomicUsize::new(AVG_LINE_LEN_ESTIMATE)),
        line_ending: LineEnding::Lf,
        encoding: DocumentEncoding::utf8(),
        encoding_origin: DocumentEncodingOrigin::NewDocument,
        encoding_engine: super::encoding_engine::engine_for_encoding(DocumentEncoding::utf8()),
        decoding_had_errors: false,
        preserve_save_error_cache: Cell::new(None),
        rope_file_len_cache: Cell::new(None),
        rope: None,
        rope_history: RopeHistory::new(),
        piece_table: None,
        dirty: false,
    };

    // First edit far beyond the 16 MiB sync-scan reach.
    let target = 650_000usize;
    doc.try_insert_text_at(target, 0, "EDITED\n").unwrap();
    let row = doc.line_slice(target, 0, 32);
    assert!(row.is_exact(), "edited far row must be exact");
    assert!(row.text().starts_with("EDITED"), "got {:?}", row.text());
    let shifted = doc.line_slice(target + 1, 0, 24);
    assert!(
        shifted.text().starts_with(&format!("LINE-{target:07}-")),
        "got {:?}",
        shifted.text()
    );
    // The document total grew by exactly one line.
    assert_eq!(doc.exact_line_count(), Some(lines + 2));

    // A second far region: the annotated region extends monotonically and
    // both distant edits coexist.
    let target2 = 900_000usize;
    doc.try_insert_text_at(target2, 0, "SECOND\n").unwrap();
    let row2 = doc.line_slice(target2, 0, 32);
    assert!(row2.is_exact());
    assert!(row2.text().starts_with("SECOND"), "got {:?}", row2.text());
    // An edit back inside the first known region still resolves.
    doc.try_insert_text_at(10, 0, "EARLY\n").unwrap();
    let row3 = doc.line_slice(10, 0, 32);
    assert!(row3.text().starts_with("EARLY"), "got {:?}", row3.text());
    assert_eq!(doc.exact_line_count(), Some(lines + 4));

    // Save streams the pieces byte-accurately and the saved file reopens
    // with every far edit in place.
    let saved = dir.join("windowed-saved.log");
    doc.save_to(&saved).unwrap();
    drop(doc);
    let reopened = Document::open(&saved).unwrap();
    let expected_total = lines + 4;
    let mut settled = false;
    for _ in 0..600 {
        if reopened.exact_line_count() == Some(expected_total) {
            settled = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(settled, "reopened document did not finish indexing");
    assert!(
        reopened.line_slice(10, 0, 16).text().starts_with("EARLY"),
        "early edit missing after reopen"
    );
    assert!(
        reopened
            .line_slice(target + 1, 0, 16)
            .text()
            .starts_with("EDITED"),
        "far edit missing after reopen"
    );
    assert!(
        reopened
            .line_slice(target2 + 1, 0, 16)
            .text()
            .starts_with("SECOND"),
        "second far edit missing after reopen"
    );

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&saved);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn recovered_far_edit_session_reads_beyond_annotated_region() {
    let dir =
        std::env::temp_dir().join(format!("standpad-doc-recovered-far-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("recovered.log");
    let lines = 1_000_000usize;
    let line_len = 72usize;
    let mut bytes = Vec::with_capacity(lines * line_len);
    for i in 0..lines {
        bytes.extend_from_slice(format!("LINE-{i:07}-{}\n", "x".repeat(58)).as_bytes());
    }
    std::fs::write(&path, &bytes).unwrap();
    let storage = FileStorage::open(&path).unwrap();
    let disk = crate::index::DiskLineIndex::build_for_tests(&path, &storage);

    let mut doc = Document {
        path: Some(path.clone()),
        storage: Some(storage),
        line_offsets: Arc::new(RwLock::new(LineOffsets::default())),
        disk_index: Some(disk),
        indexing: Arc::new(AtomicBool::new(false)),
        indexing_started: None,
        file_len: bytes.len(),
        indexed_bytes: Arc::new(AtomicUsize::new(0)),
        avg_line_len: Arc::new(AtomicUsize::new(AVG_LINE_LEN_ESTIMATE)),
        line_ending: LineEnding::Lf,
        encoding: DocumentEncoding::utf8(),
        encoding_origin: DocumentEncodingOrigin::NewDocument,
        encoding_engine: super::encoding_engine::engine_for_encoding(DocumentEncoding::utf8()),
        decoding_had_errors: false,
        preserve_save_error_cache: Cell::new(None),
        rope_file_len_cache: Cell::new(None),
        rope: None,
        rope_history: RopeHistory::new(),
        piece_table: None,
        dirty: false,
    };

    // Far edit through the checkpoint window; the session journal is flushed
    // on the first edit (no debounce anchor yet).
    doc.try_insert_text_at(650_000, 0, "EDITED\n").unwrap();
    assert!(doc.is_dirty());
    drop(doc);

    // Reopen: the session journal must recover the far edit as a piece
    // table, and reads BEYOND the annotated region must still resolve
    // through the disk index.
    let reopened = Document::open(&path).unwrap();
    assert_eq!(
        reopened.status().backing().as_str(),
        "piece-table",
        "session did not recover as a piece table"
    );
    let row = reopened.line_slice(650_000, 0, 16);
    assert!(
        row.text().starts_with("EDITED"),
        "recovered edit missing: {:?}",
        row.text()
    );
    if let Some(piece_table) = reopened.piece_table.as_ref() {
        eprintln!(
            "DEBUG recovered: known_byte={} known_lines={} total_len={} pieces={}",
            piece_table.known_byte_len(),
            piece_table.line_count(),
            piece_table.total_len(),
            piece_table.pieces_snapshot().len(),
        );
        for (i, p) in piece_table.pieces_snapshot().iter().take(36).enumerate() {
            eprintln!(
                "  p[{i}] start={} len={} breaks={}",
                p.start, p.len, p.line_breaks
            );
        }
    }
    // Lines after the recovered edit are shifted by one document line.
    for l in [700_000usize, 900_000, 999_000] {
        let row = reopened.line_slice(l, 0, 24);
        assert!(
            row.is_exact() && row.text().starts_with(&format!("LINE-{:07}-", l - 1)),
            "line {l} did not resolve after recovery: [exact={}] {:?}",
            row.is_exact(),
            row.text()
        );
    }

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(path.with_extension("log.qem.editlog"));
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// Audit regression pins
//
// Engine-aware bounded literal search on non-UTF-8 piece tables, the
// Class B trailing-line model, cell-aligned piece splits on UTF-16, the
// edit-capability plan parity for non-UTF-8 documents, the rope-backed
// non-UTF-8 `file_len` cache, and the zero-step cursor scan guard.
// ---------------------------------------------------------------------------

#[test]
fn bounded_literal_search_respects_range_on_cp1251_piece_table() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("cp1251-bounded.txt");
    // Five windows-1251 collision pairs (0xD0 0x90: two single-byte
    // characters that also parse as one UTF-8 sequence) then "XY\nF\n".
    // The old UTF-8-stepping range advance swallowed every pair into one
    // unit, pushed the range end past line 0 into line 1, and surfaced
    // the "F" match that lives outside the requested range.
    let mut bytes = Vec::new();
    for _ in 0..5 {
        bytes.extend_from_slice(&[0xD0u8, 0x90]);
    }
    bytes.extend_from_slice(b"XY\nF\n");
    std::fs::write(&path, &bytes).unwrap();

    let encoding = DocumentEncoding::from_label("windows-1251").unwrap();
    let mut doc = Document::open_with_encoding(path.clone(), encoding).unwrap();
    // One edit promotes the document to the piece-tree backing, which is
    // where the bounded-range arithmetic used to run UTF-8 stepping.
    doc.try_insert_text_at(0, 0, "Z").unwrap();

    // The range covers 11 text units from (0, 0): "Z" plus the ten
    // collision-pair bytes plus "X" - it ends mid-line 0. "F" on line 1
    // lies outside the requested range in both search directions.
    let range = TextRange::new(TextPosition::new(0, 0), 11);
    assert!(
        doc.find_next_in_range("F", range).is_none(),
        "F on line 1 is outside the requested range (forward)"
    );
    assert!(
        doc.find_prev_in_range("F", range).is_none(),
        "F on line 1 is outside the requested range (reverse)"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn class_b_native_open_counts_trailing_empty_line_like_utf8() {
    let dir = tempdir().unwrap();

    let utf16le_encode = |text: &str| -> Vec<u8> {
        let mut bytes = Vec::new();
        for unit in text.encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        bytes
    };
    let wait_indexed = |doc: &Document| {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while doc.is_indexing() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    };

    let trailing = dir.path().join("utf16le-trailing.txt");
    std::fs::write(&trailing, utf16le_encode("AB\n")).unwrap();
    let doc = Document::open_with_encoding(&trailing, DocumentEncoding::utf16le()).unwrap();
    wait_indexed(&doc);
    assert_eq!(doc.text_lossy(), "AB\n");
    assert_eq!(
        doc.display_line_count(),
        2,
        "a trailing UTF-16 line break opens an empty last line, like UTF-8"
    );

    let no_trailing = dir.path().join("utf16le-no-trailing.txt");
    std::fs::write(&no_trailing, utf16le_encode("AB")).unwrap();
    let doc = Document::open_with_encoding(&no_trailing, DocumentEncoding::utf16le()).unwrap();
    wait_indexed(&doc);
    assert_eq!(
        doc.display_line_count(),
        1,
        "no phantom trailing line without a line break"
    );

    let (sj, used, had_errors) = SHIFT_JIS.encode("AB\n");
    assert_eq!(used, SHIFT_JIS);
    assert!(!had_errors);
    let sj_trailing = dir.path().join("shift-jis-trailing.txt");
    std::fs::write(&sj_trailing, sj.as_ref()).unwrap();
    let doc = Document::open_with_encoding(
        &sj_trailing,
        DocumentEncoding::from_label("shift_jis").unwrap(),
    )
    .unwrap();
    wait_indexed(&doc);
    assert_eq!(
        doc.display_line_count(),
        2,
        "a trailing Shift_JIS line break opens an empty last line, like UTF-8"
    );

    let (sj_plain, used, had_errors) = SHIFT_JIS.encode("AB");
    assert_eq!(used, SHIFT_JIS);
    assert!(!had_errors);
    let sj_no_trailing = dir.path().join("shift-jis-no-trailing.txt");
    std::fs::write(&sj_no_trailing, sj_plain.as_ref()).unwrap();
    let doc = Document::open_with_encoding(
        &sj_no_trailing,
        DocumentEncoding::from_label("shift_jis").unwrap(),
    )
    .unwrap();
    wait_indexed(&doc);
    assert_eq!(doc.display_line_count(), 1);

    // Reference: the UTF-8 inline scanner model on identical content.
    let utf8_ref = dir.path().join("utf8-trailing.txt");
    std::fs::write(&utf8_ref, b"AB\n").unwrap();
    let doc = Document::open_with_encoding(&utf8_ref, DocumentEncoding::utf8()).unwrap();
    wait_indexed(&doc);
    assert_eq!(doc.display_line_count(), 2);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn utf16le_piece_table_split_stays_cell_aligned() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("utf16le-split.txt");
    // Byte stream `41 00 41 C2 80 C2 80 01 42 00`: a valid UTF-16LE cell
    // sequence AND simultaneously valid UTF-8 - exactly the input that
    // made the old piece-split aligner (gated on `from_utf8` validity
    // instead of on the document encoding) pull a UTF-16 split into the
    // middle of a code unit.
    let text = "A\u{C241}\u{C280}\u{0180}B";
    let mut bytes = Vec::new();
    for unit in text.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    assert_eq!(
        bytes,
        vec![0x41, 0x00, 0x41, 0xC2, 0x80, 0xC2, 0x80, 0x01, 0x42, 0x00]
    );
    std::fs::write(&path, &bytes).unwrap();

    let mut doc = Document::open_with_encoding(&path, DocumentEncoding::utf16le()).unwrap();
    // Inserting at (0, 2) splits the original piece at byte offset 4.
    doc.try_insert_text_at(0, 2, "X").unwrap();

    assert_eq!(doc.text_lossy(), "A\u{C241}X\u{C280}\u{0180}B");
    let piece_table = doc
        .piece_table
        .as_ref()
        .expect("edit promotes to piece table");
    assert_eq!(
        piece_table.line_visible_segment(0, 0, 40),
        "A\u{C241}X\u{C280}\u{0180}B",
        "the viewport line must decode across every piece boundary"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn edit_capability_advertises_piece_table_for_small_non_utf8_documents() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("cp1251-capability.txt");
    let (bytes, used, had_errors) = WINDOWS_1251.encode("данные\n");
    assert_eq!(used, WINDOWS_1251);
    assert!(!had_errors);
    std::fs::write(&path, bytes.as_ref()).unwrap();

    let doc = Document::open_with_encoding(
        path.clone(),
        DocumentEncoding::from_label("windows-1251").unwrap(),
    )
    .unwrap();
    // The first edit creates a piece table for non-UTF-8 documents
    // regardless of size, so the capability surface must advertise the
    // same plan instead of promising a rope promotion.
    match doc.edit_capability_at(TextPosition::new(0, 0)) {
        EditCapability::RequiresPromotion {
            to: DocumentBacking::PieceTable,
            ..
        } => {}
        other => panic!("expected RequiresPromotion to PieceTable, got {other:?}"),
    }

    // UTF-8 twin keeps the rope contract for small files.
    let utf8_path = dir.path().join("utf8-capability.txt");
    std::fs::write(&utf8_path, b"abc\n").unwrap();
    let doc = Document::open_with_encoding(utf8_path, DocumentEncoding::utf8()).unwrap();
    match doc.edit_capability_at(TextPosition::new(0, 0)) {
        EditCapability::RequiresPromotion {
            to: DocumentBacking::Rope,
            ..
        } => {}
        other => panic!("expected RequiresPromotion to Rope for a small UTF-8 file, got {other:?}"),
    }

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn scan_cursor_position_bytes_advances_over_zero_step_tail_cells() {
    // A one-byte tail of a truncated UTF-16 segment: `Utf16Engine::step`
    // answers 0 (remaining < 2), so the cursor scan must clamp the step
    // to 1 instead of re-counting the same byte once per remaining unit.
    let bytes = [0x41u8];
    let engine = super::encoding_engine::engine_for_encoding(DocumentEncoding::utf16le());
    let mut state = CursorScanState::new(3);
    scan_cursor_position_bytes(&bytes, &mut state, engine);
    assert_eq!(
        state.position(),
        (0, 1),
        "one truncated UTF-16 cell counts as one text unit"
    );
}

#[test]
fn file_len_tracks_rope_backed_non_utf8_documents() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("big5-rope.txt");
    let (bytes, used, had_errors) = encoding_rs::BIG5.encode("你好");
    assert_eq!(used, encoding_rs::BIG5);
    assert!(!had_errors);
    std::fs::write(&path, bytes.as_ref()).unwrap();

    // big5 is neither Class A nor a supported Class B encoding, so open
    // materializes the decoded rope; `file_len` must answer the encoded
    // length of the current content and keep tracking edits - the cache
    // behind it must not go stale across mark_dirty boundaries.
    let mut doc =
        Document::open_with_encoding(path, DocumentEncoding::from_label("big5").unwrap()).unwrap();
    assert_eq!(doc.backing(), DocumentBacking::Rope);
    assert_eq!(doc.file_len(), 4);
    doc.try_insert_text_at(0, 0, "A").unwrap();
    assert_eq!(
        doc.file_len(),
        5,
        "file_len tracks edits after the cached read"
    );
    doc.try_insert_text_at(0, 0, "XY").unwrap();
    assert_eq!(doc.file_len(), 7);
    assert_eq!(doc.text_lossy(), "XYA你好");

    let _ = std::fs::remove_dir_all(&dir);
}
