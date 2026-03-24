use qem::{Document, LiteralSearchQuery, TextPosition, TextSelection, ViewportRequest};
use std::env;
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = env::args_os().skip(1);
    let input = args
        .next()
        .map(PathBuf::from)
        .ok_or("usage: cargo run --example typed_editing -- <input> [output]")?;
    let output = args.next().map(PathBuf::from);

    let mut doc = Document::open(&input)?;
    print_viewport(&doc, "before edits");

    let title_selection = doc.clamp_selection(TextSelection::new(
        TextPosition::new(0, 0),
        TextPosition::new(0, 12),
    ));
    let selected = doc.read_selection(title_selection);
    println!(
        "selection: {:?}..{:?}, exact={}, text={:?}",
        title_selection.anchor(),
        title_selection.head(),
        selected.is_exact(),
        selected.text()
    );

    let cursor = doc.try_replace_selection(title_selection, "[qem-demo] ")?;
    println!("replace cursor: {:?}", cursor);

    let cut_selection = doc.clamp_selection(TextSelection::new(
        TextPosition::new(2, 0),
        TextPosition::new(2, 8),
    ));
    let cut = doc.try_cut_selection(cut_selection)?;
    println!(
        "cut text: {:?}, changed={}, cursor={:?}",
        cut.text(),
        cut.edit().changed(),
        cut.edit().cursor()
    );

    let sample =
        doc.read_text(doc.text_range_between(TextPosition::new(0, 0), TextPosition::new(2, 32)));
    println!("sample after edits: {:?}", sample.text());

    let query = LiteralSearchQuery::new("qem").ok_or("empty literal query")?;
    if let Some(found) = doc.find_next_query(&query, TextPosition::new(0, 0)) {
        println!(
            "found literal match: {:?}..{:?} -> {:?}",
            found.start(),
            found.end(),
            doc.read_text(found.range()).text()
        );

        if let Some(previous) = doc.find_prev_query(&query, found.end()) {
            println!(
                "previous literal match at-or-before {:?}: {:?}..{:?}",
                found.end(),
                previous.start(),
                previous.end()
            );
        }
    }

    print_viewport(&doc, "after edits");

    if let Some(output) = output {
        doc.save_to(&output)?;
        println!("saved copy: {}", output.display());
    }

    Ok(())
}

fn print_viewport(doc: &Document, label: &str) {
    println!("{label}:");
    let viewport = doc.read_viewport(ViewportRequest::new(0, 5).with_columns(0, 120));
    for row in viewport.rows() {
        println!(
            "{:>8}: [{}] {}",
            row.line_number(),
            if row.is_exact() { "=" } else { "~" },
            row.text()
        );
    }
}
