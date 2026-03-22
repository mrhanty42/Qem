use criterion::{
    black_box, criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion, Throughput,
};
use qem::{Document, ViewportRequest};
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

fn build_edited_viewport_doc(fixture: &Fixture) -> Document {
    let mut doc = open_and_wait(&fixture.path);
    let _ = doc
        .try_insert_text_at(0, 0, "[qem-edited]\n")
        .expect("seed edited viewport document");
    doc
}

fn bench_edited_scroll(c: &mut Criterion) {
    let fixture = scroll_fixture();
    let doc = build_edited_viewport_doc(fixture);
    let mut group = c.benchmark_group("edited_viewport_reads");
    group.throughput(Throughput::Elements(VIEWPORT_LINES as u64));
    group.bench_function(BenchmarkId::new("prefix", fixture.label), |b| {
        b.iter(|| {
            black_box(
                doc.read_viewport(
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
    targets = bench_small_open, bench_open, bench_scroll, bench_edited_scroll, bench_save
}
criterion_main!(benches);
