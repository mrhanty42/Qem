//! Short regression benchmarks that run as a CI job.
//!
//! The document_perf benchmark suite records thorough baselines on real
//! hardware; this file runs a much smaller set of the same operations with
//! tightened criterion settings, so a CI runner finishes in well under a
//! minute while still exercising the same code paths (open, first viewport,
//! first edit, edited save, literal and regex search). The numbers land in
//! the job log; compare them against the recorded baselines in
//! `benches/baselines/` when a change looks suspicious on shared-runner
//! hardware.

use criterion::{black_box, criterion_group, criterion_main, BatchSize, Criterion, Throughput};
use qem::{Document, RegexSearchQuery, TextPosition, ViewportRequest};
use std::fs;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;
use tempfile::TempDir;

const REGRESSION_LINE_COUNT: usize = 20_000;
const LINE: &str = "line 0123456789 abcdefghij\n";
const VIEWPORT_LINES: usize = 120;

#[derive(Clone, Debug)]
struct Fixture {
    path: PathBuf,
    bytes: u64,
}

static FIXTURE: OnceLock<Fixture> = OnceLock::new();

fn fixture() -> &'static Fixture {
    FIXTURE.get_or_init(|| {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("regression-input.txt");
        let mut bytes = Vec::with_capacity(REGRESSION_LINE_COUNT * LINE.len());
        for i in 0..REGRESSION_LINE_COUNT {
            let line = LINE.replacen("0123456789", &format!("{i:010}"), 1);
            bytes.extend_from_slice(line.as_bytes());
        }
        fs::write(&path, &bytes).expect("write fixture");
        // Keep the temp dir alive for the process lifetime.
        std::mem::forget(dir);
        Fixture {
            path,
            bytes: bytes.len() as u64,
        }
    })
}

fn quick() -> Criterion {
    Criterion::default()
        .sample_size(20)
        .measurement_time(Duration::from_millis(600))
}

fn bench_open_and_first_viewport(_c: &mut Criterion) {
    let mut criterion = quick();
    let fixture = fixture();
    let mut group = criterion.benchmark_group("regression_open");
    group.throughput(Throughput::Bytes(fixture.bytes));
    group.bench_function("open_first_viewport", |b| {
        b.iter(|| {
            let document = Document::open(&fixture.path).expect("open");
            let viewport = document.read_viewport(ViewportRequest::new(0, VIEWPORT_LINES));
            black_box(viewport.rows().len());
        })
    });
    group.finish();
}

fn bench_first_edit_and_save(_c: &mut Criterion) {
    let mut criterion = quick();
    let fixture = fixture();
    let mut group = criterion.benchmark_group("regression_edit");
    let save_path = fixture.path.with_extension("out.txt");
    group.bench_function("first_edit_and_save", |b| {
        b.iter_batched(
            || Document::open(&fixture.path).expect("open"),
            |mut document| {
                document
                    .try_insert(TextPosition::new(0, 0), "[regression]\n")
                    .expect("insert");
                document.save_to(&save_path).expect("save");
                let _ = fs::remove_file(&save_path);
            },
            BatchSize::LargeInput,
        )
    });
    group.finish();
}

fn bench_search(_c: &mut Criterion) {
    let mut criterion = quick();
    let fixture = fixture();
    let regex_query = RegexSearchQuery::new("line 0+[0-5]{4}").expect("compile regex query");
    let mut group = criterion.benchmark_group("regression_search");
    group.throughput(Throughput::Bytes(fixture.bytes));
    group.bench_function("literal_search", |b| {
        b.iter(|| {
            let document = Document::open(&fixture.path).expect("open");
            black_box(
                document
                    .find_next(black_box("line 0009"), TextPosition::new(0, 0))
                    .map(|m| m.start()),
            );
        })
    });
    group.bench_function("regex_search", |b| {
        b.iter(|| {
            let document = Document::open(&fixture.path).expect("open");
            black_box(
                document
                    .find_next_regex_query(black_box(&regex_query), TextPosition::new(0, 0))
                    .map(|m| m.start()),
            );
        })
    });
    group.finish();
}

criterion_group! {
    name = benches;
    config = quick();
    targets = bench_open_and_first_viewport, bench_first_edit_and_save, bench_search
}
criterion_main!(benches);
