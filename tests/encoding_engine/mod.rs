//! Shared helpers for the encoding-aware-engine integration property
//! tests. Each top-level file under `tests/` is its own integration test
//! binary, and `tests/encoding_engine.rs` wires the per-property
//! submodules in here through `#[path]` attributes. This module exists
//! so those submodules can share common scaffolding such as
//! `fresh_test_dir` without each property test reinventing temp-file
//! management.
//!
//! The helper honours the `$env:TMP` / `$env:TEMP` convention (which
//! on the developer machine points at `D:\qem_test_tmp`). The Unix
//! fallback path is `/tmp`. Each call returns a unique per-process,
//! per-counter subdirectory so concurrent test threads (and shrinking
//! proptest cases) cannot collide on file names.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

/// Returns a unique temporary directory under the configured tmp root.
///
/// Resolution order:
///
/// 1. `$env:TMP` if set and non-empty.
/// 2. `$env:TEMP` if set and non-empty (Windows convention).
/// 3. `/tmp` on Unix-like targets, `D:\qem_test_tmp` on Windows.
///
/// The returned directory is created on disk before the function returns
/// so callers can immediately write fixture files into it.
#[allow(dead_code)] // each integration test binary may use only a subset of helpers
pub fn fresh_test_dir(name: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let base = std::env::var_os("TMP")
        .filter(|value| !value.is_empty())
        .or_else(|| std::env::var_os("TEMP").filter(|value| !value.is_empty()))
        .map(PathBuf::from)
        .unwrap_or_else(default_tmp_root);

    let pid = std::process::id();
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = base.join(format!("qem-encoding-engine-{pid}-{counter}-{name}"));
    std::fs::create_dir_all(&dir).expect("fresh_test_dir: create_dir_all");
    dir
}

#[cfg(windows)]
fn default_tmp_root() -> PathBuf {
    PathBuf::from(r"D:\qem_test_tmp")
}

#[cfg(not(windows))]
fn default_tmp_root() -> PathBuf {
    PathBuf::from("/tmp")
}
