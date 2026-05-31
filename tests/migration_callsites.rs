//! Integration test guarding the encoding-aware migration.
//!
//! All hot byte-level callsites in the document layer have been moved
//! off the bare free helpers (`utf8_step`, `next_line_start_exact`
//! `count_text_columns_exact`, `count_text_columns`
//! `advance_offset_by_text_units_in_bytes`) and onto
//! `self.encoding_engine.<method>(...)`. This test fails if a future
//! change reintroduces a direct call to one of those free functions
//! inside the migrated source files.
//!
//! The check is pure text grep: each forbidden symbol must not appear
//! as a `function_call(` token in the migrated files. We allow comments
//! that mention these symbols (they document what each engine wraps)
//! but we reject any text that looks like an actual invocation of one
//! of the free helpers.
//!
//! `encoding_engine.rs` is intentionally exempted — that module owns
//! the trait and `Utf8Engine`, which legitimately delegates to the
//! free helpers.

use std::fs;

/// Forbidden symbols that must not appear as direct call sites in
/// `Document` methods.
const FORBIDDEN_SYMBOLS: &[&str] = &[
    "utf8_step",
    "next_line_start_exact",
    "count_text_columns_exact",
    "count_text_columns",
    "advance_offset_by_text_units_in_bytes",
];

/// Files in the `document/` module that have been migrated to the
/// engine surface. Each one is grepped for any forbidden symbol.
const MIGRATED_FILES: &[&str] = &[
    "src/document/positions.rs",
    "src/document/reads.rs",
    "src/document/search.rs",
    "src/document/regex_search.rs",
    "src/document/commands.rs",
];

#[test]
fn migrated_files_have_no_direct_free_helper_calls() {
    for file in MIGRATED_FILES {
        let content = fs::read_to_string(file)
            .unwrap_or_else(|e| panic!("failed to read {file}: {e}"));

        for sym in FORBIDDEN_SYMBOLS {
 // Look for the symbol followed by `(`, which is how Rust
 // function calls land in source. Mentions inside comments
 // (e.g. "wraps utf8_step") almost never use that exact
 // form, so the false-positive rate is low.
            let needle = format!("{sym}(");
            if let Some(idx) = content.find(needle.as_str()) {
 // Count the line number for a better diagnostic.
                let line = content[..idx].matches('\n').count() + 1;
                panic!(
                    "{file}:{line} contains a direct call to `{sym}(...)`. \
                     Route this through `self.encoding_engine.<method>(...)` \
                     instead so non-UTF-8 documents stay correct.",
                );
            }
        }
    }
}
