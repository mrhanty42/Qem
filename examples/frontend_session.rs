use qem::{Document, EditorTab};
use std::env;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy)]
struct ViewportRequest {
    first_line0: usize,
    line_count: usize,
    start_col: usize,
    max_cols: usize,
}

#[derive(Debug, Clone)]
struct ViewportRow {
    line0: usize,
    exact: bool,
    text: String,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = env::args_os().skip(1);
    let input = args.next().map(PathBuf::from).ok_or(
        "usage: cargo run --example frontend_session --features editor -- <input> [output]",
    )?;
    let output = args.next().map(PathBuf::from);

    let mut tab = EditorTab::new(1);
    tab.open_file_async(input.clone())?;
    wait_for_load(&mut tab, Duration::from_secs(5))?;

    let viewport = ViewportRequest {
        first_line0: 0,
        line_count: 20,
        start_col: 0,
        max_cols: 160,
    };
    let rows = read_viewport(tab.document(), viewport);

    println!("opened: {}", input.display());
    println!("generation: {}", tab.generation());
    println!(
        "lines: {} ({})",
        tab.document().display_line_count(),
        if tab.document().is_line_count_exact() {
            "exact"
        } else {
            "estimated"
        }
    );
    println!("dirty: {}", tab.is_dirty());
    println!(
        "cursor: line {}, column {}",
        tab.cursor().line(),
        tab.cursor().column()
    );
    println!("viewport:");
    for row in rows {
        println!(
            "{:>8}: [{}] {}",
            row.line0 + 1,
            if row.exact { "=" } else { "~" },
            row.text
        );
    }

    if let Some(output) = output {
        let _ = tab.document_mut().try_insert_text_at(
            0,
            0,
            "// saved by qem frontend_session example\n",
        )?;

        if tab.save_as_async(output.clone())? {
            wait_for_save(&mut tab, Duration::from_secs(5))?;
        }

        println!("saved copy: {}", output.display());
        println!("dirty after save: {}", tab.is_dirty());
    }

    Ok(())
}

fn read_viewport(doc: &Document, request: ViewportRequest) -> Vec<ViewportRow> {
    doc.line_slices(
        request.first_line0,
        request.line_count,
        request.start_col,
        request.max_cols,
    )
    .into_iter()
    .enumerate()
    .map(|(offset, slice)| ViewportRow {
        line0: request.first_line0 + offset,
        exact: slice.is_exact(),
        text: slice.into_text(),
    })
    .collect()
}

fn wait_for_load(tab: &mut EditorTab, timeout: Duration) -> Result<(), Box<dyn std::error::Error>> {
    let deadline = Instant::now() + timeout;
    let mut last_progress = None;

    loop {
        if let Some((loaded_bytes, total_bytes, path)) = tab.loading_progress() {
            let progress = (loaded_bytes, total_bytes);
            if last_progress != Some(progress) {
                println!(
                    "loading: {loaded_bytes}/{total_bytes} bytes from {}",
                    path.display()
                );
                last_progress = Some(progress);
            }
        }

        if let Some(result) = tab.poll_load_job() {
            result?;
            wait_for_indexing(tab.document(), Duration::from_millis(500));
            return Ok(());
        }

        if !tab.is_loading() {
            wait_for_indexing(tab.document(), Duration::from_millis(500));
            return Ok(());
        }

        if Instant::now() >= deadline {
            return Err("background load timed out".into());
        }

        thread::sleep(Duration::from_millis(10));
    }
}

fn wait_for_save(tab: &mut EditorTab, timeout: Duration) -> Result<(), Box<dyn std::error::Error>> {
    let deadline = Instant::now() + timeout;
    let mut last_progress = None;

    loop {
        if let Some((written_bytes, total_bytes, path)) = tab.save_progress() {
            let progress = (written_bytes, total_bytes);
            if last_progress != Some(progress) {
                println!(
                    "saving: {written_bytes}/{total_bytes} bytes to {}",
                    path.display()
                );
                last_progress = Some(progress);
            }
        }

        if let Some(result) = tab.poll_save_job() {
            result?;
            return Ok(());
        }

        if !tab.is_saving() {
            return Ok(());
        }

        if Instant::now() >= deadline {
            return Err("background save timed out".into());
        }

        thread::sleep(Duration::from_millis(10));
    }
}

fn wait_for_indexing(doc: &Document, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    let mut last_progress = None;

    while doc.is_indexing() && Instant::now() < deadline {
        if let Some(progress) = doc.indexing_progress() {
            if last_progress != Some(progress) {
                println!("indexing: {}/{} bytes", progress.0, progress.1);
                last_progress = Some(progress);
            }
        }

        thread::sleep(Duration::from_millis(10));
    }
}
