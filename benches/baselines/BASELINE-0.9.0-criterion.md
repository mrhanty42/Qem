# Qem criterion baseline — 0.9.0 (after the ropey replacement)

Date: 2026-11-02
Commit: 7ccaa0c (master, post-0.9.0, ropey removed)
Rust: 1.97.1 (8bab26f4f 2026-07-14), cargo 1.97.1, stable-x86_64-pc-windows-msvc

## Machine (same as in BASELINE-0.8.1.md)

- OS: Windows 10 Home, build 19045
- CPU: Intel Core i7-3770 @ 3.40GHz, 8 logical cores
- RAM: 16 GB
- Repository and criterion temp files: SSD (ADATA SU650, C:)
- Windows Defender RealTimeProtection: enabled

## Conditions

- `cargo bench --bench document_perf` (69 benches), warm OS cache,
  serial run, no parallel disk activity.
- Means taken from `target/criterion/*/new/estimates.json`
  (point_estimate, ns).
- No direct criterion baseline of 0.8.1 exists (the criterion table was
  not captured in 0.8.1), so these numbers are the 0.9.0 reference point,
  not a comparison against 0.8.1. The 0.8.1 comparison on real files is
  separate, in perf-50gb-*.jsonl.

## Results

| Group | Bench | Mean |
|---|---|---|
| document_open | open_and_index | 35.77 ms |
| document_save | streaming_in_place | 839.46 ms |
| edited_viewport_reads | prefix | 4.35 ms |
| full_text_materialization | edited_text_lossy | 52.73 ms |
| full_text_materialization | small_text_lossy | 391.9 us |
| literal_search | fragmented_piece_table_find_all_dense_match_first_512 | 1.44 ms |
| literal_search | fragmented_piece_table_find_all_in_range_dense_match_first_512 | 1.55 ms |
| literal_search | fragmented_piece_table_query_find_all_dense_match_first_512 | 1.49 ms |
| literal_search | fragmented_piece_table_query_find_all_in_range_dense_match_first_512 | 1.58 ms |
| literal_search | fragmented_piece_table_query_find_next | 9.14 ms |
| literal_search | fragmented_piece_table_query_find_next_from_middle | 71.0 us |
| literal_search | fragmented_piece_table_query_find_next_no_match | 18.46 ms |
| literal_search | fragmented_piece_table_query_find_prev_from_middle | 569.7 us |
| literal_search | mmap_find_all_dense_match_first_512 | 410.8 us |
| literal_search | mmap_find_next | 2.01 ms |
| literal_search | mmap_find_prev | 3.31 ms |
| literal_search | mmap_query_find_all_dense_match_first_512 | 392.3 us |
| literal_search | mmap_query_find_next | 1.98 ms |
| literal_search | mmap_query_find_prev | 3.37 ms |
| literal_search | piece_table_find_all_dense_match_first_512 | 1.36 ms |
| literal_search | piece_table_find_all_in_range_dense_match_first_512 | 1.61 ms |
| literal_search | piece_table_find_next | 9.01 ms |
| literal_search | piece_table_find_prev | 8.80 ms |
| literal_search | piece_table_query_find_all_dense_match_first_512 | 1.39 ms |
| literal_search | piece_table_query_find_all_in_range_dense_match_first_512 | 1.62 ms |
| literal_search | piece_table_query_find_next | 9.35 ms |
| literal_search | piece_table_query_find_next_from_middle | 70.2 us |
| literal_search | piece_table_query_find_next_many_match | 2.5 us |
| literal_search | piece_table_query_find_next_no_match | 17.90 ms |
| literal_search | piece_table_query_find_prev | 8.47 ms |
| literal_search | piece_table_query_find_prev_from_middle | 500.2 us |
| literal_search | piece_table_query_find_prev_many_match | 15.97 ms |
| literal_search | piece_table_query_find_prev_no_match | 393.2 us |
| piece_table_compaction | compact_current_state | 12.90 ms |
| regex_search | mmap_alt_query_find_next | 3.77 ms |
| regex_search | mmap_find_next | 3.83 ms |
| regex_search | mmap_query_find_all_first_512 | 893.8 us |
| regex_search | mmap_query_find_next | 3.72 ms |
| regex_search | mmap_query_find_next_no_match | 4.16 ms |
| regex_search | mmap_query_find_prev | 12.25 ms |
| regex_search | piece_table_query_find_all_first_512 | **613.55 ms** |
| regex_search | piece_table_query_find_next | 1.49 ms |
| regex_search | piece_table_query_find_next_no_match | 1.49 ms |
| regex_search | piece_table_query_find_prev | 3.17 ms |
| session_layer_maintenance_status | document | 80.6 us |
| session_layer_maintenance_status | session | 80.4 us |
| session_layer_maintenance_status | tab | 80.7 us |
| session_layer_status | document | 132 ns |
| session_layer_status | session | 205 ns |
| session_layer_status | tab | 192 ns |
| session_layer_text_reads | document | 2.6 us |
| session_layer_text_reads | session | 2.6 us |
| session_layer_text_reads | tab | 2.6 us |
| session_layer_viewport_reads | document | 18.6 us |
| session_layer_viewport_reads | session | 18.7 us |
| session_layer_viewport_reads | tab | 18.4 us |
| small_document_open | edit_buffer_builder (new baseline instead of ropey_from_reader) | 12.80 ms |
| small_document_open | qem_inline_index | 509.1 us |
| typed_edit_commands | delete_selection | 1.37 ms |
| typed_edit_commands | first_insert | 1.35 ms |
| typed_edit_commands | piece_table_delete_selection | 1.38 ms |
| typed_edit_commands | piece_table_insert | 1.15 ms |
| typed_edit_commands | piece_table_replace_selection | 1.32 ms |
| typed_edit_commands | replace_selection | 1.45 ms |
| typed_text_reads | edited_read_text | 331.0 us |
| typed_text_reads | mmap_read_selection | 8.1 us |
| typed_text_reads | mmap_read_text | 2.6 us |
| viewport_reads | middle | 18.7 us |
| viewport_reads | tail | 19.4 us |

## Notes

- `small_document_open`: `qem_inline_index` (509 us) is ~25× faster than
  the raw bulk buffer load (12.8 ms) — the small-file open path does not
  materialize the buffer, and that advantage survived the ropey
  replacement.
- **Outlier**: `regex_search/piece_table_query_find_all_first_512`
  (613 ms vs 894 us on mmap). Cause not established; no direct 0.8.1
  criterion baseline exists, so it is unknown whether this is a
  regression or a long-standing behavior of the chunked regex streaming
  over a piece table. Plan: run the same bench on the v0.8.1 tag in a
  worktree and compare.
