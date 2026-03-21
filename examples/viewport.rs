use qem::Document;
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
    println!("estimated_lines: {}", doc.estimated_line_count());
    println!("precise_lines: {}", doc.line_count());

    for (offset, slice) in doc
        .line_slices(start_line0, line_count, 0, 160)
        .into_iter()
        .enumerate()
    {
        println!(
            "{:>8}: [{}] {}",
            start_line0 + offset + 1,
            if slice.is_exact() { "=" } else { "~" },
            slice.text()
        );
    }

    Ok(())
}
