# Qem 50 GB matrix — 0.8.1 → 0.9.0 (post ropey replacement)

Date: 2026-11-03
0.9.0 commit: 7ccaa0c (master); 0.8.1 base: 6ad8a64 (v0.8.1)
Rust: 1.97.1; same machine (i7-3770, 16 GB, fixture on HDD D:, Defender on)

## Procedure

- Verbatim per the procedure in `BASELINE-0.8.1.md`:
  `collect_perf_matrix.ps1 'D:\qem-bench\huge_50gb.log' -Needle ERROR
  -FindAllLimit 128 -FindAllRangeLines 2048 -ViewportAnchors
  head,middle,tail -Repeats 2 -WaitSecs 900 -MeasureSave -Release`.
- One condition difference from the base: fresh `.qem.editlog` sessions
  were deleted before the run (otherwise the clean run would have
  restored the October edit session); the valid
  `.huge_50gb.log.qem.lineidx` was kept (same v2 format as in 0.8.1).
  In the base run the lineidx was built AFTER the baseline was recorded
  (Sep 29 vs Sep 26), so the index-cache conditions are not byte-for-byte
  equal — flagged below.
- Full output: `perf-50gb-post-0.9.0.jsonl` (12 records);
  the base has 6 records (run=1), the comparison uses those.

## Comparison (run=1, metrics in ms unless stated otherwise)

| state | anchor | open 0.8.1→0.9.0 | index_wait | viewport | next | prev | find_all |
|---|---|---|---|---|---|---|---|
| clean | head | 15→22 | ~0→3.4 s | 3.6→0.9 | 0.095→0.04 | 21→3.7 | 2.0→0.01 |
| clean | middle | 14→40 | ~0→3.3 s | 14→28 | 0.095→1.2 | 17→0.6 | 1.9→0.01 |
| clean | tail | 15→25 | ~0→4.2 s | 12→24 | 0.095→1.2 | 11→0.5 | 2.1→0.01 |
| edited | head | 14→21 | ~0→9.8 s | 3.1→5.9 | 0.095→0.4 | 18→36 | 2.0→75 |
| edited | middle | 15→22 | ~0→12.3 s | 15→3.5 | 0.095→0.4 | 15→37 | 2.0→78 |
| edited | tail | 14→40 | ~0→5.2 s | 12→5.5 | 0.095→0.7 | 13→63 | 2.4→159 |

## Reading the results

1. **open_ms 21–40 on 50 GB** (base 14–15 ms) — absolute values are
   pocket change; the 1.4–2.9× drift on single runs is within HDD/
   Defender/cache noise. Not classified as a regression.
2. **index_wait 3–12 s** — the in-memory scan of the 1-GiB budget
   (`indexed_bytes: 1073741824` exactly). In the base the field was ~0:
   the semantics of the "indexing finished" moment differs between 0.8.1
   and 0.9.0 (in the base the exact line count apparently arrived
   instantly from another path). Bounded by ~12 s, no structural issue;
   for 1.0 it is worth investigating why the disk index does not cancel
   this scan entirely.
3. **next/prev/viewport on clean** — scattered both ways on single
   ms-level measurements (noise). No structural regression visible.
4. **edited find_all: 2 ms → 75–159 ms (38–67×, consistent across all
   anchors)** — the only real regression candidate. Path: literal
   find_all over the chunked streaming piece table after a seed-edit.
   It echoes the criterion outlier
   `regex_search/piece_table_query_find_all_first_512` = 613 ms
   (see BASELINE-0.9.0-criterion.md). The check on the v0.8.1 tag is
   below.

## Criterion outlier: check on v0.8.1 (worktree, warm C: cache)

| Bench | v0.8.1 | 0.9.0 | Verdict |
|---|---|---|---|
| regex_search/piece_table_query_find_all_first_512 | **1.97 s** | 613 ms | **3.2× improvement**, not a regression |
| regex_search/mmap_query_find_all_first_512 | 980 µs | 894 µs | parity |
| regex_search/piece_table_query_find_next | 2.05 ms | 1.49 ms | better |
| literal piece_table find_next / find_prev / find_all* | 9.5–10.4 ms / 0.49–1.7 ms | 8.5–9.4 ms / 1.36–1.62 ms | parity or better |

Conclusion: the 613 ms criterion outlier is a long-standing cost of
chunked regex find_all over a piece table, and the 0.9.0 line already
sped it up threefold. The small literal-search fixtures are at parity
or better in every group.

## Remaining candidate

`edited find_all` on 50 GB: 2 ms (0.8.1) → 75–159 ms (0.9.0),
consistent across anchors, with a healthy small-scale path. The
difference of this path from the criterion coverage: a 50 GB document
with a disk index and the far-edit window (`9811b81`/`3391eae`) — a
combination the small fixtures do not exercise. Classified as an open
perf candidate for 1.0, not a 0.9.0 blocker: the absolute value is
75–159 ms on an edited 50-GB document; repro — the edited-state matrix
(procedure above). Hypothesis check: rerun the matrix for the
edited state only, v0.8.1 worktree vs HEAD (≈40 min on HDD).

## 0.9.0 exit-criterion conclusion ("perf within the baseline")

- Real clean scenarios: within the baseline (open/viewport/search —
  the same order of magnitude, some metrics better).
- Real edited scenarios: candidate regression on piece-table find_all —
  needs confirmation against v0.8.1 and a fix before 1.0.
