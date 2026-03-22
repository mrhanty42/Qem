use qem::{DocumentSession, TextPosition, ViewportRequest};
use std::env;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = env::args_os().skip(1);
    let input = args.next().map(PathBuf::from).ok_or(
        "usage: cargo run --example frontend_session --features editor -- <input> [output]",
    )?;
    let output = args.next().map(PathBuf::from);

    let mut session = DocumentSession::new();
    session.open_file_async(input.clone())?;
    wait_for_load(&mut session, Duration::from_secs(5))?;

    let viewport = session.read_viewport(ViewportRequest::new(0, 20).with_columns(0, 160));
    let status = session.status();

    println!("opened: {}", input.display());
    println!("generation: {}", status.generation());
    println!("bytes: {}", status.file_len());
    println!("backing: {}", status.backing().as_str());
    println!(
        "lines: {} ({})",
        status.display_line_count(),
        if status.is_line_count_exact() {
            "exact"
        } else {
            "estimated"
        }
    );
    println!("dirty: {}", status.is_dirty());
    println!("line ending: {:?}", status.line_ending());
    println!(
        "edit capability at line 1: {:?}",
        session.edit_capability_at(TextPosition::new(0, 0))
    );
    println!("viewport:");
    for row in viewport.rows() {
        println!(
            "{:>8}: [{}] {}",
            row.line_number(),
            if row.is_exact() { "=" } else { "~" },
            row.text()
        );
    }

    if let Some(output) = output {
        let _ = session.try_insert(
            TextPosition::new(0, 0),
            "// saved by qem frontend_session example\n",
        )?;

        if session.save_as_async(output.clone())? {
            wait_for_save(&mut session, Duration::from_secs(5))?;
        }

        println!("saved copy: {}", output.display());
        println!("dirty after save: {}", session.is_dirty());
    }

    Ok(())
}

fn wait_for_load(
    session: &mut DocumentSession,
    timeout: Duration,
) -> Result<(), Box<dyn std::error::Error>> {
    let deadline = Instant::now() + timeout;
    let mut last_progress = None;

    loop {
        let status = session.status();
        if let Some(progress) = status.loading_state() {
            let progress_key = (progress.completed_bytes(), progress.total_bytes());
            if last_progress != Some(progress_key) {
                println!(
                    "loading: {}/{} bytes from {}",
                    progress.completed_bytes(),
                    progress.total_bytes(),
                    progress.path().display()
                );
                last_progress = Some(progress_key);
            }
        }

        if let Some(result) = session.poll_load_job() {
            result?;
            wait_for_indexing(session, Duration::from_millis(500));
            return Ok(());
        }

        if !session.is_loading() {
            wait_for_indexing(session, Duration::from_millis(500));
            return Ok(());
        }

        if Instant::now() >= deadline {
            return Err("background load timed out".into());
        }

        thread::sleep(Duration::from_millis(10));
    }
}

fn wait_for_save(
    session: &mut DocumentSession,
    timeout: Duration,
) -> Result<(), Box<dyn std::error::Error>> {
    let deadline = Instant::now() + timeout;
    let mut last_progress = None;

    loop {
        let status = session.status();
        if let Some(progress) = status.save_state() {
            let progress_key = (progress.completed_bytes(), progress.total_bytes());
            if last_progress != Some(progress_key) {
                println!(
                    "saving: {}/{} bytes to {}",
                    progress.completed_bytes(),
                    progress.total_bytes(),
                    progress.path().display()
                );
                last_progress = Some(progress_key);
            }
        }

        if let Some(result) = session.poll_save_job() {
            result?;
            return Ok(());
        }

        if !session.is_saving() {
            return Ok(());
        }

        if Instant::now() >= deadline {
            return Err("background save timed out".into());
        }

        thread::sleep(Duration::from_millis(10));
    }
}

fn wait_for_indexing(session: &DocumentSession, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    let mut last_progress = None;

    while Instant::now() < deadline {
        let status = session.status();
        if !status.is_indexing() {
            break;
        }
        if let Some(progress) = status.indexing_state() {
            if last_progress != Some(progress) {
                println!(
                    "indexing: {}/{} bytes",
                    progress.completed_bytes(),
                    progress.total_bytes()
                );
                last_progress = Some(progress);
            }
        }

        thread::sleep(Duration::from_millis(10));
    }
}
