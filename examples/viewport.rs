use qem::{Document, ViewportRequest};
use std::env;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = env::args_os().skip(1);
    let path = args
        .next()
        .map(PathBuf::from)
        .ok_or("usage: cargo run --example viewport -- <path> [start_line0] [line_count]")?;
    let start_line0 = args
        .next()
        .and_then(|v| v.to_str().and_then(|v| v.parse::<usize>().ok()))
        .unwrap_or(0);
    let line_count = args
        .next()
        .and_then(|v| v.to_str().and_then(|v| v.parse::<usize>().ok()))
        .unwrap_or(10);

    let doc = Document::open(&path)?;
    let deadline = Instant::now() + Duration::from_millis(500);
    while doc.is_indexing() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }

    println!("path: {}", path.display());
    println!("bytes: {}", doc.status().file_len());
    println!("backing: {}", doc.status().backing().as_str());
    println!(
        "lines: {} ({})",
        doc.display_line_count(),
        if doc.is_line_count_exact() {
            "exact"
        } else {
            "estimated"
        }
    );

    let viewport =
        doc.read_viewport(ViewportRequest::new(start_line0, line_count).with_columns(0, 160));
    for row in viewport.rows() {
        println!(
            "{:>8}: [{}] {}",
            row.line_number(),
            if row.is_exact() { "=" } else { "~" },
            row.text()
        );
    }

    Ok(())
}
