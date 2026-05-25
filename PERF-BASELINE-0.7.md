# Qem 0.7.0 Perf Baseline

This is the initial in-repo real-file performance baseline for the `0.7.0`
integration cycle.

It is meant for regression tracking inside this repository, not for public
large-file throughput claims. The measured inputs below are real repository
files so later changes can be compared against a stable local seed matrix
without depending on external corpora.

## Machine

- Capture date: `2026-04-07`
- OS: `Windows 10 Home 10.0.19045 (build 19045)`
- CPU: `Intel(R) Core(TM) i7-3770 CPU @ 3.40GHz`
- RAM: `16.0 GiB`
- Storage class: `SSD (ATA)`
- Rust toolchain: `rustc 1.94.1 (e408947bf 2026-03-25)`

## Command

```powershell
.\scripts\collect_perf_matrix.ps1 `
  -InputPaths @('README.md', 'src\lib.rs', 'src\editor\tests.rs') `
  -ViewportAnchors middle `
  -Repeats 3 `
  -MatrixLabel 0.7-seed `
  -MeasureSave `
  -OutputJsonl target\perf-matrix-0.7.jsonl

.\scripts\summarize_perf_matrix.ps1 `
  -InputJsonl target\perf-matrix-0.7.jsonl `
  -OutputMarkdown target\perf-matrix-0.7.md
```

No search needle was provided for this seed matrix, so the search columns are
intentionally blank.

## Notes

- `clean` measures the initial file-backed open/read/save path.
- `edited` seeds one small insert through `perf_probe`, then measures the
  edited in-memory path and save.
- `peak WS MiB` is a coarse child-process working-set sample gathered by
  `collect_perf_matrix.ps1` while `perf_probe` runs.
- This seed matrix is intentionally repo-local. Before publishing huge-file or
  real-world throughput claims, run the same scripts against external corpora
  and keep those numbers separate from this baseline.

## Summary

| input | size | label | anchor | state | backing | runs | open ms | index wait ms | exact line wait ms | edit ms | viewport ms | save ms | next ms | prev ms | find_all ms | peak WS MiB |
| --- | --- | --- | --- | --- | --- | ---: | --- | --- | --- | --- | --- | --- | --- | --- | --- | ---: |
| lib.rs | 15.869 KiB | 0.7-seed | middle | clean | mmap | 3 | 2.987 [2.483-2.987] | 0.003 [0.002-0.003] | 0.007 [0.005-0.007] | - [---] | 0.139 [0.103-0.139] | 6.899 [6.418-6.899] | - [---] | - [---] | - [---] | 2.367 [2.270-2.367] |
| lib.rs | 15.877 KiB | 0.7-seed | middle | edited | rope | 3 | 3.181 [2.942-3.181] | 0.002 [0.002-0.002] | 0.003 [0.003-0.003] | 2.213 [2.063-2.213] | 3.173 [3.163-3.173] | 6.287 [5.619-6.287] | - [---] | - [---] | - [---] | 2.320 [1.469-2.320] |
| README.md | 24.746 KiB | 0.7-seed | middle | clean | mmap | 3 | 4.791 [2.612-4.791] | 0.003 [0.002-0.003] | 0.007 [0.005-0.007] | - [---] | 0.199 [0.097-0.199] | 8.777 [6.764-8.777] | - [---] | - [---] | - [---] | 3.781 [2.207-3.781] |
| README.md | 24.754 KiB | 0.7-seed | middle | edited | rope | 3 | 2.630 [2.438-2.630] | 0.002 [0.002-0.002] | 0.004 [0.003-0.004] | 3.299 [2.541-3.299] | 2.732 [2.568-2.732] | 8.119 [6.375-8.119] | - [---] | - [---] | - [---] | 4.418 [2.305-4.418] |
| tests.rs | 110.947 KiB | 0.7-seed | middle | clean | mmap | 3 | 5.644 [3.775-5.644] | 0.003 [0.002-0.003] | 0.008 [0.006-0.008] | - [---] | 0.147 [0.092-0.147] | 15.389 [8.857-15.389] | - [---] | - [---] | - [---] | 3.906 [2.309-3.906] |
| tests.rs | 110.955 KiB | 0.7-seed | middle | edited | rope | 3 | 4.785 [3.315-4.785] | 0.002 [0.002-0.002] | 0.003 [0.003-0.003] | 11.650 [9.172-11.650] | 3.038 [2.799-3.038] | 9.490 [8.280-9.490] | - [---] | - [---] | - [---] | 4.355 [4.348-4.355] |
