use criterion::{
    black_box, criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion, Throughput,
};
use qem::{Document, LiteralSearchQuery, TextPosition, TextSelection, ViewportRequest};
#[cfg(feature = "editor")]
use qem::{DocumentSession, EditorTab};
use ropey::Rope;
use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;

const VIEWPORT_LINES: usize = 120;
const VIEWPORT_COLS: usize = 160;
const SMALL_OPEN_LINE_COUNT: usize = 20_000;
const OPEN_LINE_COUNT: usize = 1_000_000;
const SCROLL_LINE_COUNT: usize = 400_000;
const SAVE_LINE_COUNT: usize = 250_000;
const PIECE_TABLE_EDIT_LINE_COUNT: usize = 64_000;
const TYPED_EDIT_LINE_COUNT: usize = 4_096;
const LONG_LINE_WIDTH: usize = 96;

#[derive(Clone, Debug)]
struct Fixture {
    label: &'static str,
    path: PathBuf,
    bytes: u64,
}

static SMALL_OPEN_FIXTURE: OnceLock<Fixture> = OnceLock::new();
static OPEN_FIXTURE: OnceLock<Fixture> = OnceLock::new();
static SCROLL_FIXTURE: OnceLock<Fixture> = OnceLock::new();
static SAVE_FIXTURE: OnceLock<Fixture> = OnceLock::new();
static PIECE_TABLE_EDIT_FIXTURE: OnceLock<Fixture> = OnceLock::new();
static TYPED_EDIT_FIXTURE: OnceLock<Fixture> = OnceLock::new();

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("qem-bench-data")
}

fn numbered_fixture(
    cache: &'static OnceLock<Fixture>,
    label: &'static str,
    line_count: usize,
    body_width: usize,
) -> &'static Fixture {
    cache.get_or_init(|| {
        let dir = fixture_dir();
        fs::create_dir_all(&dir).expect("create bench fixture dir");
        let path = dir.join(label);
        if !path.exists() {
            write_numbered_lines(&path, line_count, body_width).expect("write bench fixture");
        }

        let bytes = fs::metadata(&path).expect("fixture metadata").len();
        Fixture { label, path, bytes }
    })
}

fn open_fixture() -> &'static Fixture {
    numbered_fixture(&OPEN_FIXTURE, "open-large.log", OPEN_LINE_COUNT, 24)
}

fn small_open_fixture() -> &'static Fixture {
    numbered_fixture(
        &SMALL_OPEN_FIXTURE,
        "open-small.log",
        SMALL_OPEN_LINE_COUNT,
        24,
    )
}

fn scroll_fixture() -> &'static Fixture {
    numbered_fixture(
        &SCROLL_FIXTURE,
        "scroll-large.log",
        SCROLL_LINE_COUNT,
        LONG_LINE_WIDTH,
    )
}

fn save_fixture() -> &'static Fixture {
    numbered_fixture(&SAVE_FIXTURE, "save-large.log", SAVE_LINE_COUNT, 48)
}

fn piece_table_edit_fixture() -> &'static Fixture {
    numbered_fixture(
        &PIECE_TABLE_EDIT_FIXTURE,
        "piece-table-edit.log",
        PIECE_TABLE_EDIT_LINE_COUNT,
        48,
    )
}

fn typed_edit_fixture() -> &'static Fixture {
    numbered_fixture(
        &TYPED_EDIT_FIXTURE,
        "typed-edit.log",
        TYPED_EDIT_LINE_COUNT,
        48,
    )
}

fn write_numbered_lines(path: &Path, line_count: usize, body_width: usize) -> io::Result<()> {
    let file = File::create(path)?;
    let mut writer = BufWriter::new(file);
    let body = "x".repeat(body_width);
    for i in 0..line_count {
        writeln!(writer, "{i:08} {body}")?;
    }
    writer.flush()
}

fn wait_for_indexing(doc: &Document, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while doc.is_indexing() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
}

fn open_and_wait(path: &Path) -> Document {
    let doc = Document::open(path).expect("open fixture");
    wait_for_indexing(&doc, Duration::from_secs(20));
    doc
}

#[cfg(feature = "editor")]
fn open_session_and_wait(path: &Path) -> DocumentSession {
    let mut session = DocumentSession::new();
    session
        .open_file(path.to_path_buf())
        .expect("open bench document session");
    let deadline = Instant::now() + Duration::from_secs(20);
    while session.is_indexing() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    session
}

#[cfg(feature = "editor")]
fn open_tab_and_wait(path: &Path) -> EditorTab {
    let mut tab = EditorTab::new(1);
    tab.open_file(path.to_path_buf())
        .expect("open bench editor tab");
    let deadline = Instant::now() + Duration::from_secs(20);
    while tab.is_indexing() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    tab
}

fn bench_open(c: &mut Criterion) {
    let fixture = open_fixture();
    let mut group = c.benchmark_group("document_open");
    group.sample_size(10);
    group.throughput(Throughput::Bytes(fixture.bytes));
    group.bench_function(BenchmarkId::new("open_and_index", fixture.label), |b| {
        b.iter_custom(|iters| {
            let start = Instant::now();
            for _ in 0..iters {
                let doc = Document::open(&fixture.path).expect("open bench document");
                wait_for_indexing(&doc, Duration::from_secs(20));
                black_box(doc.display_line_count());
            }
            start.elapsed()
        });
    });
    group.finish();
}

fn bench_small_open(c: &mut Criterion) {
    let fixture = small_open_fixture();
    let mut group = c.benchmark_group("small_document_open");
    group.sample_size(20);
    group.throughput(Throughput::Bytes(fixture.bytes));
    group.bench_function(BenchmarkId::new("qem_inline_index", fixture.label), |b| {
        b.iter_custom(|iters| {
            let start = Instant::now();
            for _ in 0..iters {
                let doc = Document::open(&fixture.path).expect("open small qem document");
                black_box(doc.display_line_count());
            }
            start.elapsed()
        });
    });
    group.bench_function(BenchmarkId::new("ropey_from_reader", fixture.label), |b| {
        b.iter_custom(|iters| {
            let start = Instant::now();
            for _ in 0..iters {
                let file = File::open(&fixture.path).expect("open small ropey fixture");
                let rope = Rope::from_reader(file).expect("load small ropey fixture");
                black_box(rope.len_lines());
            }
            start.elapsed()
        });
    });
    group.finish();
}

fn bench_scroll(c: &mut Criterion) {
    let fixture = scroll_fixture();
    let doc = open_and_wait(&fixture.path);
    let line_count = doc.display_line_count().max(VIEWPORT_LINES);
    let middle_line = line_count / 2;
    let tail_line = line_count.saturating_sub(VIEWPORT_LINES);

    let mut group = c.benchmark_group("viewport_reads");
    group.throughput(Throughput::Elements(VIEWPORT_LINES as u64));
    group.bench_function(BenchmarkId::new("middle", fixture.label), |b| {
        b.iter(|| {
            black_box(
                doc.read_viewport(
                    ViewportRequest::new(black_box(middle_line), VIEWPORT_LINES)
                        .with_columns(0, VIEWPORT_COLS),
                ),
            )
        });
    });
    group.bench_function(BenchmarkId::new("tail", fixture.label), |b| {
        b.iter(|| {
            black_box(
                doc.read_viewport(
                    ViewportRequest::new(black_box(tail_line), VIEWPORT_LINES)
                        .with_columns(0, VIEWPORT_COLS),
                ),
            )
        });
    });
    group.finish();
}

#[cfg(feature = "editor")]
fn bench_session_layer_reads(c: &mut Criterion) {
    let fixture = scroll_fixture();
    let doc = open_and_wait(&fixture.path);
    let session = open_session_and_wait(&fixture.path);
    let tab = open_tab_and_wait(&fixture.path);
    let line_count = doc.display_line_count().max(VIEWPORT_LINES);
    let middle_line = line_count / 2;
    let request = ViewportRequest::new(middle_line, VIEWPORT_LINES).with_columns(0, VIEWPORT_COLS);
    let selection = typed_read_selection(1024);
    let range = doc.text_range_for_selection(selection);

    let mut viewport_group = c.benchmark_group("session_layer_viewport_reads");
    viewport_group.throughput(Throughput::Elements(VIEWPORT_LINES as u64));
    viewport_group.bench_function(BenchmarkId::new("document", fixture.label), |b| {
        b.iter(|| black_box(doc.read_viewport(request)))
    });
    viewport_group.bench_function(BenchmarkId::new("session", fixture.label), |b| {
        b.iter(|| black_box(session.read_viewport(request)))
    });
    viewport_group.bench_function(BenchmarkId::new("tab", fixture.label), |b| {
        b.iter(|| black_box(tab.read_viewport(request)))
    });
    viewport_group.finish();

    let mut text_group = c.benchmark_group("session_layer_text_reads");
    text_group.throughput(Throughput::Elements(range.len_chars() as u64));
    text_group.bench_function(BenchmarkId::new("document", fixture.label), |b| {
        b.iter(|| black_box(doc.read_text(range)))
    });
    text_group.bench_function(BenchmarkId::new("session", fixture.label), |b| {
        b.iter(|| black_box(session.read_text(range)))
    });
    text_group.bench_function(BenchmarkId::new("tab", fixture.label), |b| {
        b.iter(|| black_box(tab.read_text(range)))
    });
    text_group.finish();

    let mut status_group = c.benchmark_group("session_layer_status");
    status_group.bench_function(BenchmarkId::new("document", fixture.label), |b| {
        b.iter(|| black_box(doc.status()))
    });
    status_group.bench_function(BenchmarkId::new("session", fixture.label), |b| {
        b.iter(|| black_box(session.status()))
    });
    status_group.bench_function(BenchmarkId::new("tab", fixture.label), |b| {
        b.iter(|| black_box(tab.status()))
    });
    status_group.finish();
}

#[cfg(not(feature = "editor"))]
fn bench_session_layer_reads(_: &mut Criterion) {}

#[derive(Debug)]
struct EditedDocumentCase {
    _dir: TempDir,
    doc: Document,
}

fn build_edited_document_case(fixture: &Fixture) -> EditedDocumentCase {
    let dir = tempfile::tempdir().expect("create edited bench temp dir");
    let path = dir.path().join("edited-case.log");
    fs::copy(&fixture.path, &path).expect("copy edited bench fixture");
    let mut doc = open_and_wait(&path);
    let _ = doc
        .try_insert_text_at(0, 0, "[qem-edited]\n")
        .expect("seed edited viewport document");
    EditedDocumentCase { _dir: dir, doc }
}

#[derive(Debug)]
struct PieceTableEditCase {
    _dir: TempDir,
    doc: Document,
}

fn build_piece_table_edit_case(fixture: &Fixture) -> PieceTableEditCase {
    let dir = tempfile::tempdir().expect("create piece-table bench temp dir");
    let path = dir.path().join("piece-table-edit.log");
    fs::copy(&fixture.path, &path).expect("copy piece-table bench fixture");
    let mut doc = open_and_wait(&path);
    let _ = doc
        .try_insert(TextPosition::new(0, 0), "[qem-piece-table]\n")
        .expect("seed piece-table edit document");
    PieceTableEditCase { _dir: dir, doc }
}

fn typed_read_selection(line0: usize) -> TextSelection {
    TextSelection::new(
        TextPosition::new(line0, 4),
        TextPosition::new(line0 + 2, 24),
    )
}

fn typed_edit_selection(line0: usize) -> TextSelection {
    TextSelection::new(
        TextPosition::new(line0, 4),
        TextPosition::new(line0 + 1, 16),
    )
}

fn bench_typed_reads(c: &mut Criterion) {
    let fixture = scroll_fixture();
    let mmap_doc = open_and_wait(&fixture.path);
    let edited_case = build_edited_document_case(fixture);
    let selection = typed_read_selection(1024);
    let range = mmap_doc.text_range_for_selection(selection);

    let mut group = c.benchmark_group("typed_text_reads");
    group.throughput(Throughput::Elements(range.len_chars() as u64));
    group.bench_function(BenchmarkId::new("mmap_read_text", fixture.label), |b| {
        b.iter(|| black_box(mmap_doc.read_text(range)))
    });
    group.bench_function(BenchmarkId::new("edited_read_text", fixture.label), |b| {
        b.iter(|| black_box(edited_case.doc.read_text(range)))
    });
    group.bench_function(
        BenchmarkId::new("mmap_read_selection", fixture.label),
        |b| b.iter(|| black_box(mmap_doc.read_selection(selection))),
    );
    group.finish();
}

fn bench_literal_search(c: &mut Criterion) {
    let mmap_fixture = scroll_fixture();
    let piece_table_fixture = piece_table_edit_fixture();
    let mmap_doc = open_and_wait(&mmap_fixture.path);
    let piece_table_case = build_piece_table_edit_case(piece_table_fixture);
    let mmap_needle = format!("{:08}", SCROLL_LINE_COUNT / 2);
    let piece_table_needle = format!("{:08}", PIECE_TABLE_EDIT_LINE_COUNT / 2);
    let mmap_query = LiteralSearchQuery::new(mmap_needle.clone()).expect("build mmap search query");
    let piece_table_query = LiteralSearchQuery::new(piece_table_needle.clone())
        .expect("build piece-table search query");

    let mut group = c.benchmark_group("literal_search");
    group.bench_function(
        BenchmarkId::new("mmap_find_next", mmap_fixture.label),
        |b| {
            b.iter(|| {
                black_box(
                    mmap_doc.find_next(black_box(mmap_needle.as_str()), TextPosition::new(0, 0)),
                )
            })
        },
    );
    group.bench_function(
        BenchmarkId::new("mmap_find_prev", mmap_fixture.label),
        |b| {
            b.iter(|| {
                black_box(mmap_doc.find_prev(
                    black_box(mmap_needle.as_str()),
                    TextPosition::new(usize::MAX, usize::MAX),
                ))
            })
        },
    );
    group.bench_function(
        BenchmarkId::new("mmap_query_find_next", mmap_fixture.label),
        |b| {
            b.iter(|| {
                black_box(mmap_doc.find_next_query(black_box(&mmap_query), TextPosition::new(0, 0)))
            })
        },
    );
    group.bench_function(
        BenchmarkId::new("mmap_query_find_prev", mmap_fixture.label),
        |b| {
            b.iter(|| {
                black_box(mmap_doc.find_prev_query(
                    black_box(&mmap_query),
                    TextPosition::new(usize::MAX, usize::MAX),
                ))
            })
        },
    );
    group.bench_function(
        BenchmarkId::new("piece_table_find_next", piece_table_fixture.label),
        |b| {
            b.iter(|| {
                black_box(piece_table_case.doc.find_next(
                    black_box(piece_table_needle.as_str()),
                    TextPosition::new(0, 0),
                ))
            })
        },
    );
    group.bench_function(
        BenchmarkId::new("piece_table_find_prev", piece_table_fixture.label),
        |b| {
            b.iter(|| {
                black_box(piece_table_case.doc.find_prev(
                    black_box(piece_table_needle.as_str()),
                    TextPosition::new(usize::MAX, usize::MAX),
                ))
            })
        },
    );
    group.bench_function(
        BenchmarkId::new("piece_table_query_find_next", piece_table_fixture.label),
        |b| {
            b.iter(|| {
                black_box(
                    piece_table_case
                        .doc
                        .find_next_query(black_box(&piece_table_query), TextPosition::new(0, 0)),
                )
            })
        },
    );
    group.bench_function(
        BenchmarkId::new("piece_table_query_find_prev", piece_table_fixture.label),
        |b| {
            b.iter(|| {
                black_box(piece_table_case.doc.find_prev_query(
                    black_box(&piece_table_query),
                    TextPosition::new(usize::MAX, usize::MAX),
                ))
            })
        },
    );
    group.finish();
}

fn bench_text_materialization(c: &mut Criterion) {
    let small_fixture = small_open_fixture();
    let large_fixture = scroll_fixture();
    let small_doc = open_and_wait(&small_fixture.path);
    let edited_case = build_edited_document_case(large_fixture);

    let mut group = c.benchmark_group("full_text_materialization");
    group.measurement_time(Duration::from_secs(6));
    group.bench_function(
        BenchmarkId::new("small_text_lossy", small_fixture.label),
        |b| b.iter(|| black_box(small_doc.text_lossy())),
    );
    group.bench_function(
        BenchmarkId::new("edited_text_lossy", large_fixture.label),
        |b| b.iter(|| black_box(edited_case.doc.text_lossy())),
    );
    group.finish();
}

fn bench_typed_edits(c: &mut Criterion) {
    let fixture = typed_edit_fixture();
    let piece_table_fixture = piece_table_edit_fixture();
    let insert_pos = TextPosition::new(TYPED_EDIT_LINE_COUNT / 2, 12);
    let replace_selection = typed_edit_selection(TYPED_EDIT_LINE_COUNT / 2);
    let delete_selection = typed_edit_selection((TYPED_EDIT_LINE_COUNT / 2) + 8);
    let piece_table_insert_pos = TextPosition::new(128, 12);
    let piece_table_replace_selection = typed_edit_selection(128);
    let piece_table_delete_selection = typed_edit_selection(160);

    let mut group = c.benchmark_group("typed_edit_commands");
    group.bench_function(BenchmarkId::new("first_insert", fixture.label), |b| {
        b.iter_batched(
            || Document::open(&fixture.path).expect("open typed edit fixture"),
            |mut doc| {
                black_box(
                    doc.try_insert(insert_pos, "[typed-insert]")
                        .expect("typed insert bench"),
                )
            },
            BatchSize::SmallInput,
        );
    });
    group.bench_function(BenchmarkId::new("replace_selection", fixture.label), |b| {
        b.iter_batched(
            || Document::open(&fixture.path).expect("open typed edit fixture"),
            |mut doc| {
                black_box(
                    doc.try_replace_selection(replace_selection, "[typed-replace]")
                        .expect("typed replace bench"),
                )
            },
            BatchSize::SmallInput,
        );
    });
    group.bench_function(BenchmarkId::new("delete_selection", fixture.label), |b| {
        b.iter_batched(
            || Document::open(&fixture.path).expect("open typed edit fixture"),
            |mut doc| {
                black_box(
                    doc.try_delete_selection(delete_selection)
                        .expect("typed delete bench"),
                )
            },
            BatchSize::SmallInput,
        );
    });
    group.bench_function(
        BenchmarkId::new("piece_table_insert", piece_table_fixture.label),
        |b| {
            b.iter_batched(
                || build_piece_table_edit_case(piece_table_fixture),
                |mut case| {
                    black_box(
                        case.doc
                            .try_insert(piece_table_insert_pos, "[typed-piece]")
                            .expect("piece-table insert bench"),
                    )
                },
                BatchSize::LargeInput,
            );
        },
    );
    group.bench_function(
        BenchmarkId::new("piece_table_replace_selection", piece_table_fixture.label),
        |b| {
            b.iter_batched(
                || build_piece_table_edit_case(piece_table_fixture),
                |mut case| {
                    black_box(
                        case.doc
                            .try_replace_selection(
                                piece_table_replace_selection,
                                "[typed-piece-replace]",
                            )
                            .expect("piece-table replace bench"),
                    )
                },
                BatchSize::LargeInput,
            );
        },
    );
    group.bench_function(
        BenchmarkId::new("piece_table_delete_selection", piece_table_fixture.label),
        |b| {
            b.iter_batched(
                || build_piece_table_edit_case(piece_table_fixture),
                |mut case| {
                    black_box(
                        case.doc
                            .try_delete_selection(piece_table_delete_selection)
                            .expect("piece-table delete bench"),
                    )
                },
                BatchSize::LargeInput,
            );
        },
    );
    group.finish();
}

fn bench_edited_scroll(c: &mut Criterion) {
    let fixture = scroll_fixture();
    let case = build_edited_document_case(fixture);
    let mut group = c.benchmark_group("edited_viewport_reads");
    group.throughput(Throughput::Elements(VIEWPORT_LINES as u64));
    group.bench_function(BenchmarkId::new("prefix", fixture.label), |b| {
        b.iter(|| {
            black_box(
                case.doc.read_viewport(
                    ViewportRequest::new(black_box(1024), VIEWPORT_LINES)
                        .with_columns(0, VIEWPORT_COLS),
                ),
            )
        });
    });
    group.finish();
}

#[derive(Debug)]
struct SaveCase {
    _dir: TempDir,
    path: PathBuf,
    doc: Document,
}

fn build_save_case(fixture: &Fixture) -> SaveCase {
    let dir = tempfile::tempdir().expect("create bench temp dir");
    let path = dir.path().join("save-case.log");
    fs::copy(&fixture.path, &path).expect("copy save fixture");
    let mut doc = Document::open(&path).expect("open save case");
    let _ = doc.try_insert_text_at(0, 0, "[qem-bench]\n").unwrap();

    SaveCase {
        _dir: dir,
        path,
        doc,
    }
}

fn bench_save(c: &mut Criterion) {
    let fixture = save_fixture();
    let mut group = c.benchmark_group("document_save");
    group.sample_size(10);
    group.throughput(Throughput::Bytes(fixture.bytes));
    group.bench_function(BenchmarkId::new("streaming_in_place", fixture.label), |b| {
        b.iter_batched(
            || build_save_case(fixture),
            |mut case| {
                case.doc.save_to(&case.path).expect("save bench document");
                black_box(case.doc.file_len());
            },
            BatchSize::LargeInput,
        );
    });
    group.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .warm_up_time(Duration::from_millis(500))
        .measurement_time(Duration::from_secs(3))
        .sample_size(10);
    targets = bench_small_open, bench_open, bench_scroll, bench_session_layer_reads, bench_edited_scroll, bench_typed_reads, bench_literal_search, bench_text_materialization, bench_typed_edits, bench_save
}
criterion_main!(benches);
