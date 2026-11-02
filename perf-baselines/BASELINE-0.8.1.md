# Qem perf baseline — 0.8.1 (for comparison after 0.9.0)

Date: 2026-09-26
Commit: 6ad8a64 (master, v0.8.1 release)
Rust: 1.97.1 (8bab26f4f 2026-07-14)

## Machine

- OS: Windows 10 Home, build 19045
- CPU: Intel Core i7-3770 @ 3.40GHz, 8 logical cores (Ivy Bridge)
- RAM: 16 GB
- Disk D:: HDD (~76 MB/s sequential write — measured during file generation)
- Benchmark file lives on D:, repository on C:

## File

- Path: `D:\qem-bench\huge_50gb.log`
- Size: exactly 50,000,000,000 bytes (~476 million lines)
- Content: log lines of ~105–140 bytes, an ERROR severity every 64 lines
  (as the `ERROR` needle), id= identifiers growing, blk= blocks numbered.
- Generator: `D:\qem-bench\gen50.rs` (deterministic, xorshift64*).
- Recreate if needed: `rustc -O gen50.rs -o gen50.exe` and
  `./gen50.exe huge_50gb.log 50000000000` (~11 minutes on this disk).

## Procedure (repeat EXACTLY like this after 0.9.0)

```powershell
cd C:\Users\HanTY\Desktop\Qem
powershell -ExecutionPolicy Bypass -File .\scripts\collect_perf_matrix.ps1 `
  'D:\qem-bench\huge_50gb.log' `
  -Needle ERROR `
  -FindAllLimit 128 `
  -FindAllRangeLines 2048 `
  -ViewportAnchors head,middle,tail `
  -Repeats 2 `
  -WaitSecs 900 `
  -MeasureSave `
  -Release `
  -MatrixLabel post-0.9.0-50gb `
  -OutputJsonl perf-baselines\perf-50gb-post-0.9.0.jsonl
powershell -ExecutionPolicy Bypass -File .\scripts\summarize_perf_matrix.ps1 `
  -InputJsonl perf-baselines\perf-50gb-post-0.9.0.jsonl `
  -OutputMarkdown perf-baselines\perf-50gb-post-0.9.0-summary.md
```

Procedure notes:

- The first run builds the index sidecar `D:\qem-bench\huge_50gb.log.qem.lineidx`
  from scratch (cold), later runs reuse it. Do NOT delete the sidecar between
  the 0.8.1 and 0.9.0 runs — that keeps the conditions equal (the sidecar was
  already there).
- If the sidecar must be rebuilt for an honest "cold" comparison, delete it
  before BOTH matrices identically.
- Repeats=2 (not 3 as in the docs): on HDD a single run is long.
- `-Release` is mandatory: without it the script runs a debug build, which
  does not finish on 50 GB (the flag was added to the script on 2026-09-26,
  locally).
- `-MeasureSave` adds save_ms and the process peak memory.

## Baseline files

- `perf-50gb-baseline.jsonl` — raw per-run lines
- `perf-50gb-baseline-summary.md` — summary table (medians/min/max)
