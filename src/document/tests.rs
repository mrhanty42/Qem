use super::lifecycle::OpenProgressPhase;
use super::*;
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
    text.replace_range(prev_start..cur, "");

    if prev_ch == '\n' {
        let new_line0 = line0.saturating_sub(1);
        let new_col0 = model_lines(text)[new_line0].len_chars;
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
        rope: None,
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
        rope: None,
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
        rope: None,
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
        rope: None,
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
        rope: None,
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
        rope: None,
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
    let missing_parent = dir.join("missing");
    let dst = missing_parent.join("copy.txt");
    std::fs::write(&src, b"alpha\nbeta\n").unwrap();

    let mut doc = Document::open(src.clone()).unwrap();
    let _ = doc.try_insert_text_at(0, 0, "123").unwrap();

    let err = doc.save_to(&dst).unwrap_err();

    assert!(matches!(err, DocumentError::Write { .. }));
    assert!(doc.is_dirty());
    assert_eq!(doc.path(), Some(src.as_path()));
    assert_eq!(std::fs::read(&src).unwrap(), b"alpha\nbeta\n");
    assert!(!dst.exists());

    let _ = std::fs::remove_file(&src);
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
fn lines_iterator_yields_current_document_lines() {
    let mut doc = Document::new();
    let _ = doc.try_insert_text_at(0, 0, "zero\none\ntwo").unwrap();

    let lines: Vec<String> = doc.lines().map(LineSlice::into_text).collect();

    assert_eq!(lines, vec!["zero", "one", "two"]);
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
        rope: None,
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
        rope: None,
        piece_table: None,
        dirty: false,
    };

    let slices = doc.line_slices(0, 3, 0, 16);
    let texts: Vec<String> = slices.into_iter().map(LineSlice::into_text).collect();

    assert_eq!(texts[0], "aaaaaaaaaaaaaaaa");
    assert_eq!(texts[1], "");
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
        rope: None,
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
fn save_to_keeps_large_piece_table_documents_clean_without_reopen() {
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
    assert!(doc.has_edit_buffer());
    assert!(doc.text_lossy().starts_with("123abc\ndef\n"));
    assert!(std::fs::read(&dst).unwrap().starts_with(b"123abc\ndef\n"));

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
    let err = doc.try_insert_text_at(PARTIAL_PIECE_TABLE_MAX_LINES + 1, 0, "x");

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
        rope: None,
        piece_table: Some(PieceTable {
            original: storage,
            add: Vec::new(),
            pieces: PieceTree::from_pieces(vec![Piece {
                src: PieceSource::Original,
                start: 0,
                len: 1,
                line_breaks: 0,
            }]),
            known_line_count: 1,
            known_byte_len: 1,
            total_len: MAX_ROPE_EDIT_FILE_BYTES + 1,
            full_index: false,
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
    assert!(status.has_edit_buffer());
    assert!(status.has_rope());
    assert!(!status.has_piece_table());
    assert!(!status.is_indexing());
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
fn edit_capability_reports_large_mmap_positions_as_unsupported() {
    let dir = std::env::temp_dir().join(format!("qem-doc-edit-capability-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("huge.bin");
    let file = std::fs::File::create(&path).unwrap();
    file.set_len((MAX_ROPE_EDIT_FILE_BYTES + 1) as u64).unwrap();
    drop(file);

    let doc = Document::open(path.clone()).unwrap();
    let capability =
        doc.edit_capability_at(TextPosition::new(PARTIAL_PIECE_TABLE_MAX_LINES + 1, 0));

    assert_eq!(
        capability,
        EditCapability::Unsupported {
            backing: DocumentBacking::Mmap,
            reason:
                "document is too large to materialize into a rope; editing this region is disabled",
        }
    );
    assert!(!capability.is_editable());
    assert_eq!(
        capability.reason(),
        Some("document is too large to materialize into a rope; editing this region is disabled")
    );

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
        rope: None,
        piece_table: Some(PieceTable {
            original: storage,
            add: Vec::new(),
            pieces: PieceTree::from_pieces(vec![Piece {
                src: PieceSource::Original,
                start: 0,
                len: 1,
                line_breaks: 0,
            }]),
            known_line_count: 1,
            known_byte_len: 1,
            total_len: MAX_ROPE_EDIT_FILE_BYTES + 1,
            full_index: false,
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
            assert!(doc.has_edit_buffer());
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
