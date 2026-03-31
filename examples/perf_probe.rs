use qem::{Document, LiteralSearchQuery, TextPosition, ViewportRequest};
use std::env;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy)]
enum ViewportAnchor {
    Head,
    Middle,
    Tail,
}

impl ViewportAnchor {
    fn parse(value: &str) -> Result<Self, &'static str> {
        match value {
            "head" => Ok(Self::Head),
            "middle" => Ok(Self::Middle),
            "tail" => Ok(Self::Tail),
            _ => Err("--viewport-anchor must be one of: head, middle, tail"),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Head => "head",
            Self::Middle => "middle",
            Self::Tail => "tail",
        }
    }

    fn resolve_line(self, display_line_count: usize) -> usize {
        match self {
            Self::Head => 0,
            Self::Middle => display_line_count.saturating_sub(1) / 2,
            Self::Tail => display_line_count.saturating_sub(1),
        }
    }
}

#[derive(Debug)]
struct PerfProbeOptions {
    input: PathBuf,
    needle: Option<String>,
    find_all_limit: Option<usize>,
    find_all_range_lines: Option<usize>,
    seed_edit: Option<String>,
    save: Option<PathBuf>,
    wait_timeout: Duration,
    viewport_anchor: ViewportAnchor,
    json: bool,
}

#[derive(Debug)]
struct PerfProbeReport {
    input: PathBuf,
    file_len_bytes: usize,
    backing: String,
    display_line_count: usize,
    line_count_exact: bool,
    line_count_pending: bool,
    exact_line_count: Option<usize>,
    indexed_bytes: usize,
    open_ms: f64,
    index_wait_ms: f64,
    indexing_complete: bool,
    exact_line_count_wait_ms: f64,
    seed_edit_ms: Option<f64>,
    seed_edit_cursor: Option<TextPosition>,
    viewport_ms: f64,
    viewport_anchor: String,
    viewport_line: usize,
    viewport_rows: usize,
    piece_count: Option<usize>,
    average_piece_bytes: Option<f64>,
    fragmentation_ratio: Option<f64>,
    compaction_urgency: Option<String>,
    maintenance_action: String,
    next_ms: Option<f64>,
    next_match: Option<(TextPosition, TextPosition)>,
    prev_ms: Option<f64>,
    prev_match: Option<(TextPosition, TextPosition)>,
    find_all_ms: Option<f64>,
    find_all_count: Option<usize>,
    find_all_limit: Option<usize>,
    find_all_range_lines: Option<usize>,
    save_ms: Option<f64>,
    save_output: Option<PathBuf>,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let options = parse_args()?;

    let open_started = Instant::now();
    let mut doc = Document::open(&options.input)?;
    let open_ms = millis(open_started.elapsed());

    let index_wait_ms = wait_until(
        options.wait_timeout,
        || doc.is_indexing(),
        Duration::from_millis(10),
    );
    let indexing_complete = !doc.is_indexing();

    let mut seed_edit_ms = None;
    let mut seed_edit_cursor = None;
    if let Some(seed_edit) = options.seed_edit.as_deref() {
        let edit_started = Instant::now();
        let cursor = doc.try_insert(TextPosition::new(0, 0), seed_edit)?;
        seed_edit_ms = Some(millis(edit_started.elapsed()));
        seed_edit_cursor = Some(cursor);
    }

    let exact_line_count_started = Instant::now();
    let exact_line_count = doc.wait_for_exact_line_count(options.wait_timeout);
    let exact_line_count_wait_ms = millis(exact_line_count_started.elapsed());

    let file_len_bytes = doc.file_len();
    let display_line_count = doc.display_line_count();
    let line_count_exact = doc.is_line_count_exact();
    let line_count_pending = doc.is_line_count_pending();
    let indexed_bytes = doc.indexed_bytes();
    let viewport_line = options.viewport_anchor.resolve_line(display_line_count);
    let viewport_request = ViewportRequest::new(viewport_line, 120).with_columns(0, 160);
    let viewport_started = Instant::now();
    let viewport = doc.read_viewport(viewport_request);
    let viewport_ms = millis(viewport_started.elapsed());

    let maintenance = doc.maintenance_status();
    let (piece_count, average_piece_bytes, fragmentation_ratio) =
        if let Some(stats) = maintenance.fragmentation_stats() {
            (
                Some(stats.piece_count()),
                Some(stats.average_piece_bytes()),
                Some(stats.fragmentation_ratio()),
            )
        } else {
            (None, None, None)
        };
    let compaction_urgency = maintenance
        .compaction_urgency()
        .map(|urgency| format!("{urgency:?}"));
    let maintenance_action = maintenance.recommended_action().as_str().to_owned();

    let mut next_ms = None;
    let mut next_match = None;
    let mut prev_ms = None;
    let mut prev_match = None;
    let mut find_all_ms = None;
    let mut find_all_count = None;
    if let Some(needle) = options.needle.as_deref() {
        let query = LiteralSearchQuery::new(needle).ok_or("empty search needle")?;

        let next_started = Instant::now();
        next_match = doc
            .find_next_query(&query, TextPosition::new(0, 0))
            .map(|found| (found.start(), found.end()));
        next_ms = Some(millis(next_started.elapsed()));

        let prev_started = Instant::now();
        prev_match = doc
            .find_prev_query(&query, TextPosition::new(usize::MAX, usize::MAX))
            .map(|found| (found.start(), found.end()));
        prev_ms = Some(millis(prev_started.elapsed()));

        if let Some(limit) = options.find_all_limit {
            let find_all_started = Instant::now();
            let count = if let Some(range_lines) = options.find_all_range_lines {
                let max_line = doc.display_line_count();
                let start_line = max_line.saturating_sub(range_lines) / 2;
                let end_line = start_line.saturating_add(range_lines).min(max_line);
                doc.find_all_query_between(
                    &query,
                    TextPosition::new(start_line, 0),
                    TextPosition::new(end_line, 0),
                )
                .take(limit)
                .count()
            } else {
                doc.find_all_query(&query).take(limit).count()
            };
            find_all_ms = Some(millis(find_all_started.elapsed()));
            find_all_count = Some(count);
        }
    }

    let mut save_ms = None;
    if let Some(output) = options.save.as_ref() {
        let save_started = Instant::now();
        doc.save_to(output)?;
        save_ms = Some(millis(save_started.elapsed()));
    }

    let report = PerfProbeReport {
        input: options.input,
        file_len_bytes,
        backing: doc.backing().as_str().to_owned(),
        display_line_count,
        line_count_exact,
        line_count_pending,
        exact_line_count,
        indexed_bytes,
        open_ms,
        index_wait_ms,
        indexing_complete,
        exact_line_count_wait_ms,
        seed_edit_ms,
        seed_edit_cursor,
        viewport_ms,
        viewport_anchor: options.viewport_anchor.as_str().to_owned(),
        viewport_line,
        viewport_rows: viewport.rows().len(),
        piece_count,
        average_piece_bytes,
        fragmentation_ratio,
        compaction_urgency,
        maintenance_action,
        next_ms,
        next_match,
        prev_ms,
        prev_match,
        find_all_ms,
        find_all_count,
        find_all_limit: options.find_all_limit,
        find_all_range_lines: options.find_all_range_lines,
        save_ms,
        save_output: options.save,
    };

    if options.json {
        print_json_report(&report);
    } else {
        print_human_report(&report);
    }

    Ok(())
}

fn parse_args() -> Result<PerfProbeOptions, Box<dyn std::error::Error>> {
    let mut args = env::args_os().skip(1);
    let input = args.next().map(PathBuf::from).ok_or(
        "usage: cargo run --example perf_probe -- <input> [--needle text] [--find-all-limit n] [--find-all-range-lines n] [--seed-edit text] [--save output] [--wait-secs n] [--viewport-anchor head|middle|tail] [--json]",
    )?;

    let mut needle = None;
    let mut find_all_limit = None;
    let mut find_all_range_lines = None;
    let mut seed_edit = None;
    let mut save = None;
    let mut wait_timeout = Duration::from_secs(20);
    let mut viewport_anchor = ViewportAnchor::Middle;
    let mut json = false;

    while let Some(arg) = args.next() {
        match arg.to_string_lossy().as_ref() {
            "--needle" => {
                let value = args.next().ok_or("--needle requires a value")?;
                needle = Some(value.to_string_lossy().into_owned());
            }
            "--find-all-limit" => {
                let value = args.next().ok_or("--find-all-limit requires a number")?;
                let limit: usize = value
                    .to_string_lossy()
                    .parse()
                    .map_err(|_| "--find-all-limit must be an integer")?;
                find_all_limit = Some(limit);
            }
            "--find-all-range-lines" => {
                let value = args
                    .next()
                    .ok_or("--find-all-range-lines requires a number")?;
                let lines: usize = value
                    .to_string_lossy()
                    .parse()
                    .map_err(|_| "--find-all-range-lines must be an integer")?;
                find_all_range_lines = Some(lines);
            }
            "--seed-edit" => {
                let value = args.next().ok_or("--seed-edit requires a value")?;
                seed_edit = Some(value.to_string_lossy().into_owned());
            }
            "--save" => {
                let value = args.next().ok_or("--save requires a path")?;
                save = Some(PathBuf::from(value));
            }
            "--wait-secs" => {
                let value = args.next().ok_or("--wait-secs requires a number")?;
                let secs: u64 = value
                    .to_string_lossy()
                    .parse()
                    .map_err(|_| "--wait-secs must be an integer")?;
                wait_timeout = Duration::from_secs(secs);
            }
            "--viewport-anchor" => {
                let value = args.next().ok_or("--viewport-anchor requires a value")?;
                viewport_anchor = ViewportAnchor::parse(value.to_string_lossy().as_ref())?;
            }
            "--json" => {
                json = true;
            }
            other => {
                return Err(format!("unknown argument: {other}").into());
            }
        }
    }

    if find_all_limit.is_some() && needle.is_none() {
        return Err("--find-all-limit requires --needle".into());
    }

    Ok(PerfProbeOptions {
        input,
        needle,
        find_all_limit,
        find_all_range_lines,
        seed_edit,
        save,
        wait_timeout,
        viewport_anchor,
        json,
    })
}

fn millis(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

fn wait_until(
    timeout: Duration,
    mut pending: impl FnMut() -> bool,
    poll_interval: Duration,
) -> f64 {
    let started = Instant::now();
    let deadline = started + timeout;
    while pending() && Instant::now() < deadline {
        thread::sleep(poll_interval);
    }
    millis(started.elapsed())
}

fn print_human_report(report: &PerfProbeReport) {
    println!(
        "file_len_bytes={}, indexed_bytes={}, display_line_count={}, line_count_exact={}, line_count_pending={}, exact_line_count={}",
        report.file_len_bytes,
        report.indexed_bytes,
        report.display_line_count,
        report.line_count_exact,
        report.line_count_pending,
        report
            .exact_line_count
            .map(|value| value.to_string())
            .unwrap_or_else(|| "estimated".to_owned())
    );
    println!("open_ms={:.3}", report.open_ms);
    println!(
        "index_wait_ms={:.3}, indexing_complete={}",
        report.index_wait_ms, report.indexing_complete
    );
    println!(
        "exact_line_count_wait_ms={:.3}, line_count_pending={}",
        report.exact_line_count_wait_ms, report.line_count_pending
    );
    if let (Some(ms), Some(cursor)) = (report.seed_edit_ms, report.seed_edit_cursor) {
        println!(
            "seed_edit_ms={:.3}, cursor={:?}, backing={}",
            ms, cursor, report.backing
        );
    }
    println!(
        "viewport_anchor={}, viewport_ms={:.3}, viewport_line={}, rows={}",
        report.viewport_anchor, report.viewport_ms, report.viewport_line, report.viewport_rows
    );
    if let Some(piece_count) = report.piece_count {
        println!(
            "maintenance: pieces={}, avg_bytes={:.1}, small_ratio={:.3}, urgency={:?}",
            piece_count,
            report.average_piece_bytes.unwrap_or_default(),
            report.fragmentation_ratio.unwrap_or_default(),
            report.compaction_urgency
        );
        println!("maintenance_action={}", report.maintenance_action);
    } else {
        println!(
            "maintenance: backing={}, no piece-table stats, action={}",
            report.backing, report.maintenance_action
        );
    }
    if let Some(ms) = report.next_ms {
        println!("find_next_ms={:.3}, next_match={:?}", ms, report.next_match);
    }
    if let Some(ms) = report.prev_ms {
        println!("find_prev_ms={:.3}, prev_match={:?}", ms, report.prev_match);
    }
    if let Some(ms) = report.find_all_ms {
        println!(
            "find_all_ms={:.3}, count={}, limit={}, range_lines={}",
            ms,
            report.find_all_count.unwrap_or_default(),
            report.find_all_limit.unwrap_or_default(),
            report
                .find_all_range_lines
                .map(|value| value.to_string())
                .unwrap_or_else(|| "full".to_owned())
        );
    }
    if let Some(ms) = report.save_ms {
        println!(
            "save_ms={:.3}, output={}",
            ms,
            report
                .save_output
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_default()
        );
    }
}

fn print_json_report(report: &PerfProbeReport) {
    println!(
        "{{\"input\":\"{}\",\"file_len_bytes\":{},\"backing\":\"{}\",\"display_line_count\":{},\"line_count_exact\":{},\"line_count_pending\":{},\"exact_line_count\":{},\"indexed_bytes\":{},\"open_ms\":{:.3},\"index_wait_ms\":{:.3},\"indexing_complete\":{},\"exact_line_count_wait_ms\":{:.3},\"seed_edit_ms\":{},\"seed_edit_cursor\":{},\"viewport_anchor\":\"{}\",\"viewport_ms\":{:.3},\"viewport_line\":{},\"viewport_rows\":{},\"piece_count\":{},\"average_piece_bytes\":{},\"fragmentation_ratio\":{},\"compaction_urgency\":{},\"maintenance_action\":\"{}\",\"next_ms\":{},\"next_match\":{},\"prev_ms\":{},\"prev_match\":{},\"find_all_ms\":{},\"find_all_count\":{},\"find_all_limit\":{},\"find_all_range_lines\":{},\"save_ms\":{},\"save_output\":{}}}",
        json_escape(&report.input.display().to_string()),
        report.file_len_bytes,
        json_escape(&report.backing),
        report.display_line_count,
        report.line_count_exact,
        report.line_count_pending,
        option_usize_json(report.exact_line_count),
        report.indexed_bytes,
        report.open_ms,
        report.index_wait_ms,
        report.indexing_complete,
        report.exact_line_count_wait_ms,
        option_f64_json(report.seed_edit_ms),
        option_position_json(report.seed_edit_cursor),
        json_escape(&report.viewport_anchor),
        report.viewport_ms,
        report.viewport_line,
        report.viewport_rows,
        option_usize_json(report.piece_count),
        option_f64_json(report.average_piece_bytes),
        option_f64_json(report.fragmentation_ratio),
        option_string_json(report.compaction_urgency.as_deref()),
        json_escape(&report.maintenance_action),
        option_f64_json(report.next_ms),
        option_match_json(report.next_match),
        option_f64_json(report.prev_ms),
        option_match_json(report.prev_match),
        option_f64_json(report.find_all_ms),
        option_usize_json(report.find_all_count),
        option_usize_json(report.find_all_limit),
        option_usize_json(report.find_all_range_lines),
        option_f64_json(report.save_ms),
        option_path_json(report.save_output.as_ref()),
    );
}

fn json_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn option_f64_json(value: Option<f64>) -> String {
    value
        .map(|value| format!("{value:.3}"))
        .unwrap_or_else(|| "null".to_owned())
}

fn option_usize_json(value: Option<usize>) -> String {
    value
        .map(|value| value.to_string())
        .unwrap_or_else(|| "null".to_owned())
}

fn option_string_json(value: Option<&str>) -> String {
    value
        .map(|value| format!("\"{}\"", json_escape(value)))
        .unwrap_or_else(|| "null".to_owned())
}

fn option_path_json(value: Option<&PathBuf>) -> String {
    value
        .map(|value| format!("\"{}\"", json_escape(&value.display().to_string())))
        .unwrap_or_else(|| "null".to_owned())
}

fn option_position_json(value: Option<TextPosition>) -> String {
    value
        .map(position_json)
        .unwrap_or_else(|| "null".to_owned())
}

fn option_match_json(value: Option<(TextPosition, TextPosition)>) -> String {
    value
        .map(|(start, end)| {
            format!(
                "{{\"start\":{},\"end\":{}}}",
                position_json(start),
                position_json(end)
            )
        })
        .unwrap_or_else(|| "null".to_owned())
}

fn position_json(position: TextPosition) -> String {
    format!(
        "{{\"line0\":{},\"col0\":{}}}",
        position.line0(),
        position.col0()
    )
}
