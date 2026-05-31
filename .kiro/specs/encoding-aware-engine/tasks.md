# Implementation Plan: encoding-aware-engine (релиз 0.8.0)

## Overview

План закрывает фазы 2–11 спеки `encoding-aware-engine` после уже
выполненной фазы 1 (трейт `EncodingEngine` + `Utf8Engine` + 339
зелёных тестов). Каждая фаза — это одна верхнеуровневая задача
(Task 2 = Phase 2, ..., Task 11 = Phase 11). Подзадачи внутри фазы
— это отдельные коммиты, каждый из которых проходит build gate
(`cargo fmt --all --check`,
`cargo clippy --all-targets --all-features --workspace -- -D warnings`,
`cargo test --all-features --workspace`).

Property-based тесты ссылаются на свойства из секции `Correctness
Properties` дизайна и задаются с `ProptestConfig { cases: 64, .. }`.
Каждый PBT-файл начинается с комментария-тега
`Feature: encoding-aware-engine, Property N: <текст>`.

Большие интеграционные тесты используют `TmpDir` через
`fresh_test_dir(name)` поверх `$env:TMP` / `$env:TEMP` (=`D:\qem_test_tmp`,
R14.5).

В рамках этой спеки крейт **не публикуется** на crates.io и **не
пушится** в GitHub без явного согласия пользователя (R16.5).

## Tasks

- [ ] 2. Phase 2 — Document владеет encoding_engine как полем
  - _Requirements: R1.1, R1.2, R1.3, R1.4, R1.5, R1.6_

  - [x] 2.1 Добавить поле `encoding_engine` в `Document` и helper `set_encoding_contract`
    - Добавить `encoding_engine: &'static dyn EncodingEngine` в `struct Document` (`src/document.rs`).
    - Ввести приватный helper `Document::set_encoding_contract(&mut self, encoding, origin)`, который атомарно обновляет `self.encoding`, `self.encoding_origin` и `self.encoding_engine = engine_for_encoding(encoding)`, и инвалидирует кэш `preserve_save_error_cache`.
    - Не менять никаких публичных сигнатур.
    - _Requirements: R1.1, R1.4_

  - [x] 2.2 Инициализировать `encoding_engine` во всех конструкторах `Document`
    - Заполнить поле через `engine_for_encoding(self.encoding)` во всех конструкторах: `Document::new` / `Default`, `Document::open`, `from_storage_with_progress`, `from_storage_with_encoding`, `from_storage_with_origin`, `with_text` (если есть), `reopen_with_encoding_contract` (`src/document/lifecycle.rs`).
    - Для пустого нового буфера без явной кодировки подставлять `engine_for_encoding(DocumentEncoding::utf8())` (= `UTF8_ENGINE`).
    - Все мутации `self.encoding` (reinterpret, save-конверсия, recovery с sidecar-meta) перевести на `set_encoding_contract`.
    - _Requirements: R1.2, R1.3, R1.4_

  - [x] 2.3 Заменить тело accessor `Document::encoding_engine()` на возврат поля
    - В `src/document/state.rs` `Document::encoding_engine(&self) -> &'static dyn EncodingEngine` возвращает сохранённое `self.encoding_engine` без вызова `engine_for_encoding`.
    - Сигнатура и видимость не меняются.
    - _Requirements: R1.5_

  - [x] 2.4 PBT для Property 1: `encoding_engine` стабильно отражает текущую кодировку
    - Создать `tests/encoding_engine/mod.rs` с общим helper `fresh_test_dir(name)` (поверх `$env:TMP` / `$env:TEMP`, R14.5).
    - Создать `tests/encoding_engine/prop_dispatch.rs` с заголовком-тегом `// Feature: encoding-aware-engine, Property 1: encoding_engine reflects current encoding`.
    - PBT-стратегия: случайная последовательность операций {open, from_storage_with_encoding, with_text, default, reinterpret, save с конверсией} над одним документом; после каждой операции assert `doc.encoding_engine().encoding() == doc.encoding()`.
    - `ProptestConfig { cases: 64, .. }`.
    - _Requirements: R1.2, R1.4 / Property 1_

  - [x] 2.5 Build gate Phase 2
    - Прогнать `cargo fmt --all --check`, `cargo clippy --all-targets --all-features --workspace -- -D warnings`, `cargo test --all-features --workspace`.
    - Все 339 тестов фазы 1 + новые тесты Phase 2 — зелёные.
    - _Requirements: R1.6, R16.4_

- [x] 3. Phase 3 — Миграция callsites с free-функций на `self.encoding_engine` (по одному файлу за коммит)
  - _Requirements: R2.1, R2.2, R2.3, R2.4, R2.5_

  - [x] 3.1 Мигрировать `src/document/positions.rs` на `self.encoding_engine` + build gate
    - Заменить прямые вызовы `next_line_start_exact`, `count_text_columns_exact`, `count_text_columns`, `advance_offset_by_text_units_in_bytes` на `self.encoding_engine.<method>(...)`.
    - Под кодировкой UTF-8 поведение байтово идентично текущему (R2.4).
    - В конце коммита прогнать build gate; переход к 3.2 разрешён только при зелёном gate (R2.2).
    - _Requirements: R2.1, R2.2, R2.4, R16.4_

  - [x] 3.2 Мигрировать `src/document/reads.rs` на `self.encoding_engine` + build gate
    - Заменить прямые вызовы `advance_offset_by_text_units_in_bytes`, `count_text_columns*`, `next_line_start_exact` на методы движка.
    - Под UTF-8 — байтовое тождество.
    - Build gate как условие перехода.
    - _Requirements: R2.1, R2.2, R2.4, R16.4_

  - [x] 3.3 Мигрировать `src/document/search.rs` на `self.encoding_engine` + build gate
    - Заменить вызовы `next_line_start_exact` и связанных байтовых helpers на методы движка.
    - Под UTF-8 — байтовое тождество.
    - Build gate как условие перехода.
    - _Requirements: R2.1, R2.2, R2.4, R16.4_

  - [x] 3.4 Мигрировать `src/document/regex_search.rs` на `self.encoding_engine` + build gate
    - Заменить прямые вызовы байтовых helpers на методы движка (forward path; reverse path трогается в Phase 9).
    - Под UTF-8 — байтовое тождество.
    - Build gate как условие перехода.
    - _Requirements: R2.1, R2.2, R2.4, R16.4_

  - [x] 3.5 Мигрировать `src/document/commands.rs` на `self.encoding_engine` + build gate (финал Phase 3)
    - Заменить прямые вызовы `utf8_step` / `next_line_start_exact` / `count_text_columns*` / `advance_offset_by_text_units_in_bytes` на методы движка.
    - Под UTF-8 — байтовое тождество.
    - Build gate Phase 3.
    - _Requirements: R2.1, R2.2, R2.3, R2.4, R2.5, R16.4_

  - [x] 3.6 Grep-guard `tests/migration_callsites.rs`
    - Тест запускает `grep`/regex по исходникам и фейлится, если в `src/document/{positions,reads,search,regex_search,commands}.rs` встречаются прямые вызовы `utf8_step` / `next_line_start_exact` / `count_text_columns_exact` / `count_text_columns` / `advance_offset_by_text_units_in_bytes` (исключая модуль `Utf8Engine` в `encoding_engine.rs`).
    - Build gate.
    - _Requirements: R2.3, R16.4_

- [x] 4. Phase 4 — Финализация `SingleByteEngine` и подъём unit-тестов в PBT
  - _Requirements: R3.1, R3.2, R3.3, R3.4, R3.5, R3.6, R3.8_

  - [x] 4.1 PBT Property 2 — `SingleByteEngine::step` всегда `1` или `0`
    - Создать `tests/encoding_engine/prop_step.rs` (или дополнить существующий) с тегом `// Feature: encoding-aware-engine, Property 2: SingleByteEngine.step is always 1 byte per text-unit`.
    - PBT по `e ∈ Class A`, любые `bytes`, `offset <= end <= bytes.len()`: assert `step == 1` если `offset < end`, иначе `0`.
    - 64 cases.
    - _Requirements: R3.2, R3.3 / Property 2_

  - [x] 4.2 PBT Property 3 — `SingleByteEngine::next_line_start` корректно обрабатывает LF / CR / CRLF
    - Создать `tests/encoding_engine/prop_newline.rs` с тегом Property 3.
    - PBT: для байтов любой Class A кодировки с инжектированными LF/CR/CRLF — результат указывает сразу за полной line-ending sequence либо `bytes.len()`.
    - 64 cases.
    - _Requirements: R3.4 / Property 3_

  - [x] 4.3 PBT Property 4 — `count_columns_*` и `advance_offset_by_text_units` следуют шагу 1
    - Создать `tests/encoding_engine/prop_columns.rs` с тегом Property 4.
    - PBT: для строк без line-endings `count_columns_exact == bytes.len()`; для строк с инжектированным CRLF `advance_offset_by_text_units` от 0 на `n` units продвигается ровно через `n` символов (CRLF = 1 unit).
    - 64 cases.
    - _Requirements: R3.5 / Property 4_

  - [x] 4.4 Build gate Phase 4
    - Стандартный gate. Все unit-тесты `SingleByteEngine` из `encoding_engine.rs` остаются зелёными как regression baseline.
    - _Requirements: R16.4_

- [x] 5. Phase 5 — Open dispatch для Class A через mmap (без UTF-8-rope)
  - _Requirements: R3.6, R3.7, R3.8, R4.1, R4.2, R4.3, R4.4, R4.5, R16.1_

  - [x] 5.1 Реализовать `Document::from_storage_class_a_native`
    - В `src/document/lifecycle.rs` добавить ветку для Class A в `from_storage_with_encoding`: вызывать `from_storage_class_a_native`, который индексирует line offsets через `memchr2_iter(b'\n', b'\r', bytes)`, ставит `storage: Some(mmap)`, `rope: None`, `piece_table: None`, `dirty: false`, и инициализирует поле через `set_encoding_contract`.
    - Не строить полный `Rope`.
    - _Requirements: R4.1, R4.2, R3.6, R16.1_

  - [x] 5.2 Window-only decode в `reads.rs` для Class A
    - Заменить fullfile-decode на per-window decode: `bytes_window = &storage.bytes()[line_start..line_end]`; `(decoded, _) = encoding.as_encoding().decode_with_bom_removal(bytes_window)`; `LineSlice::new(decoded.into_owned(), exact=true)`.
    - Делать это только при чтении viewport / line-slice / search-результата, и только для запрошенного байтового окна.
    - _Requirements: R3.7, R4.3_

  - [x] 5.3 Удалить `MAX_ROPE_EDIT_FILE_BYTES`-гард для Class A
    - Удалить fallback на полный `Rope` для кодировок Class A в open-пути; гард остаётся релевантным только для UTF-8-BOM-decoded ветки и для будущего promotion piece-table → rope для UTF-8.
    - _Requirements: R4.2, R16.1_

  - [x] 5.4 PBT Property 5 — `engine_for_encoding` стабильно маршрутизирует и кэширует
    - Дополнить `tests/encoding_engine/prop_dispatch.rs` (либо отдельный файл) тегом Property 5.
    - PBT: для пар вызовов `engine_for_encoding(e1)` и `engine_for_encoding(e2)` с `e1.name() == e2.name()` — `std::ptr::eq` истинно; для любого `e ∈ Class A ∪ Class B ∪ {UTF-8}` — `engine.encoding() == e`.
    - 64 cases.
    - _Requirements: R3.6, R3.8 / Property 5_

  - [x] 5.5 PBT Property 6 — не-UTF-8 документы никогда не строят UTF-8-rope
    - Создать `tests/encoding_engine/prop_backing.rs` с тегом Property 6.
    - PBT: для случайной кодировки `e ∈ Class A` (Class B расширим в Phase 6/7) и случайной последовательности публичных операций — на всём жизненном цикле `document.rope.is_none()`.
    - 64 cases. Файлы создаются в `fresh_test_dir(...)` (R14.5).
    - _Requirements: R3.7, R4.1, R4.3, R16.1 / Property 6_

  - [x] 5.6 PBT Property 7 + per-encoding integration suite для Class A
    - Создать `tests/encoding_engine/prop_open_save_roundtrip.rs` с тегом Property 7: для `e ∈ {UTF-8} ∪ Class A` `open_with_encoding → save` без правок производит байтово-идентичные байты, 64 cases.
    - Создать модули `tests/encoding_engine/per_encoding/{windows_1251.rs, windows_1252.rs, koi8_r.rs, ibm866.rs, latin1.rs (=ISO-8859-1), iso_8859_15.rs}`, каждый с фиксированной четвёркой контрактов (R14.1–R14.4): `opens_and_indexes_lines`, `viewport_first_and_last_window`, `literal_and_regex_search_finds_known_match`, save round-trip без правок. Edit-тест добавляется в Phase 8.
    - Большие fixture (>16 KiB) — через `fresh_test_dir`.
    - _Requirements: R4.4, R9.1, R9.2, R9.3, R14.1, R14.2, R14.3, R14.5 / Property 7_

  - [x] 5.7 Build gate Phase 5
    - Стандартный gate.
    - _Requirements: R4.5, R16.4_

- [x] 6. Phase 6 — `Utf16Engine<E: Endian>` для UTF-16 LE и BE
  - _Requirements: R5.1, R5.2, R5.3, R5.4, R5.5, R5.6, R5.7, R5.8, R5.9, R5.10, R10.1, R11.3, R12.1_

  - [x] 6.1 Endian trait + LE/BE маркеры + struct `Utf16Engine<E>`
    - Внутри `src/document/encoding_engine.rs` добавить модуль `utf16` с `pub(crate) trait Endian`, маркерами `LittleEndian`/`BigEndian` (`NAME`, `LF`, `CR`, `read_u16`) и `pub(crate) struct Utf16Engine<E: Endian>(PhantomData<E>)` + `pub(crate) const fn new()`.
    - В `engine_for_encoding` добавить ветви для `UTF-16LE` и `UTF-16BE` через `OnceLock<Utf16Engine<LittleEndian>>` / `OnceLock<Utf16Engine<BigEndian>>`.
    - _Requirements: R5.1, R3.6_

  - [x] 6.2 `step` и `step_backward` с surrogate-aware логикой
    - `step`: BMP code unit → 2; high surrogate (`0xD800..=0xDBFF`) + low surrogate (`0xDC00..=0xDFFF`) → 4; нехватка байт → 0.
    - `step_backward`: симметричный — 2 для одиночного юнита, 4 если предыдущий — low surrogate с предшествующим high surrogate.
    - Inline-комментарии указывают на R5.2, R5.3, R10.1.
    - _Requirements: R5.2, R5.3, R10.1_

  - [x] 6.3 `next_line_start` с 2-байтовым выравниванием
    - Реализовать через 2-байтный цикл от выровненного `p`, ищущий `[E::LF]` и `[E::CR]` (с CRLF-схлопыванием на следующий выровненный юнит).
    - Кандидаты на нечётной байтовой позиции автоматически отбрасываются (R5.6, R11.3).
    - Реализовать `count_columns_exact`, `count_columns_bounded`, `advance_offset_by_text_units` поверх `step` с CRLF-семантикой.
    - _Requirements: R5.4, R5.5, R5.6, R11.3_

  - [x] 6.4 `from_storage_class_b_native` для UTF-16 (без UTF-8-rope)
    - В `src/document/lifecycle.rs` добавить `from_storage_class_b_native` и подключить его в `from_storage_with_encoding` для `UTF-16LE` / `UTF-16BE`.
    - Индексировать line offsets через `engine.next_line_start` (не `memchr2_iter`, потому что для UTF-16 нужно 2-байтное выравнивание).
    - `storage: Some(mmap)`, `rope: None`, `piece_table: None`, `set_encoding_contract`.
    - _Requirements: R4.1, R4.2, R5.4, R5.5, R5.6, R16.1_

  - [x] 6.5 Chunked decode + glue regex для UTF-16
    - В `src/document/regex_search.rs` для `find_next_regex_*` под UTF-16: разбивать mmap/piece-tree на байтовые окна `REGEX_CHUNK_BYTES = 8 MiB` (выровненные на чётный байт) с overlap `1 MiB`; декодировать через `encoding.decode_without_bom_handling(window)`; прогонять `regex::Regex` по `&decoded`; маппить `(start, end)` в исходные байты через таблицу offsets `encoding_rs::Decoder`.
    - Пост-фильтр: `start % 2 == 0 && end % 2 == 0`; иначе кандидат отбрасывается (R5.9, R12.1, R12.2).
    - _Requirements: R5.7, R5.8, R5.9, R12.1, R12.2_

  - [x] 6.6 PBT Property 8 — `Utf16Engine::step` различает BMP и supplementary
    - Дополнить `tests/encoding_engine/prop_step.rs` тегом Property 8.
    - PBT: для валидных UTF-16-байтовых слайсов — BMP unit → 2, surrogate pair → 4 от high surrogate.
    - 64 cases.
    - _Requirements: R5.2, R5.3 / Property 8_

  - [x] 6.7 PBT Property 9 — `Utf16Engine::next_line_start` выровнен на 2 байта
    - Дополнить `tests/encoding_engine/prop_newline.rs` тегом Property 9.
    - PBT: результат всегда чётный либо `file_len`; `0x0A`/`0x0D` на нечётной позиции не интерпретируются как line break.
    - 64 cases.
    - _Requirements: R5.4, R5.5, R5.6, R11.3 / Property 9_

  - [x] 6.8 PBT Property 10 — `Utf16Engine` придерживается своей endianness
    - Создать `tests/encoding_engine/prop_endianness.rs` с тегом Property 10.
    - PBT: `Utf16Engine<LittleEndian>` не возвращает BE-формы LF/CR на чётных позициях; симметрично для BE.
    - 64 cases.
    - _Requirements: R5.10 / Property 10_

  - [x] 6.9 Per-encoding integration tests `utf16_le` / `utf16_be`
    - Создать `tests/encoding_engine/per_encoding/utf16_le.rs` и `utf16_be.rs`: фиксированная четвёрка контрактов (open / viewport / literal+regex search / save round-trip без правок) с ASCII, не-ASCII (CJK / кириллицей), LF/CR/CRLF, пустым файлом и файлом без trailing newline.
    - Большие fixture в `fresh_test_dir`.
    - _Requirements: R9.1, R9.2, R9.3, R14.1, R14.2, R14.3, R14.5_

  - [x] 6.10 Build gate Phase 6
    - Стандартный gate.
    - _Requirements: R16.4_

- [x] 7. Phase 7 — `MultiByteEngine` для Shift_JIS, GB18030, EUC-KR
  - _Requirements: R6.1, R6.2, R6.3, R6.4, R6.5, R10.1, R11.1, R11.2_

  - [x] 7.1 `enum CjkKind`, `struct MultiByteEngine`, детектор `char_len`
    - В модуле `multibyte` внутри `src/document/encoding_engine.rs`: `pub(crate) enum CjkKind { ShiftJis, Gb18030, EucKr }`, `pub(crate) struct MultiByteEngine { kind, encoding }`, `pub(crate) fn new(kind)`.
    - Детектор `char_len` по таблицам leading-байт для Shift_JIS (lead `0x81..=0x9F | 0xE0..=0xFC` → 2 байта), GB18030 (1 / 2 / 4 байта по правилам trail), EUC-KR (lead `0xA1..=0xFE` → 2 байта).
    - В `engine_for_encoding` — три `OnceLock<MultiByteEngine>` для имён `Shift_JIS`, `gb18030`, `EUC-KR`.
    - _Requirements: R6.1, R6.2, R6.4, R3.6_

  - [x] 7.2 `step_backward` через scan-from-anchor
    - От ближайшего anchor (line_start, до 64 KiB назад — `APPROX_LINE_BACKTRACK_BYTES`) идти `step_forward` пока курсор не достигнет `offset`; вернуть последний `step`.
    - Если scan не сходится — вернуть `1` как deg-fallback (resync на следующем `next_line_start`).
    - _Requirements: R10.1, R11.2_

  - [x] 7.3 False-positive-aware `next_line_start` (символьный walk)
    - Вместо `memchr2(b'\n', b'\r', bytes)` ходить символами от `line_start` через `char_len`; LF/CR проверять только когда `char_len == 1`.
    - CRLF схлопывается при `char_len == 1` для `\r` и следующий байт `\n`.
    - _Requirements: R6.3, R11.1, R11.2_

  - [x] 7.4 Подключить `MultiByteEngine` в `from_storage_class_b_native`
    - В `src/document/lifecycle.rs`: для `Shift_JIS` / `gb18030` / `EUC-KR` идти через `from_storage_class_b_native` (уже добавленный в Phase 6) с индексом строк через `engine.next_line_start` (символьный walk).
    - Для regex-пути в `regex_search.rs` использовать ту же chunked decode + glue схему, что и для UTF-16, но без 2-байтного выравнивания.
    - _Requirements: R6.5, R4.1, R4.2, R16.1_

  - [x] 7.5 PBT Property 11 — `MultiByteEngine::step` совпадает с границами `encoding_rs::Decoder`
    - Дополнить `tests/encoding_engine/prop_step.rs` тегом Property 11.
    - PBT-стратегия: для каждого `kind ∈ {ShiftJis, Gb18030, EucKr}` сгенерировать `&str` и `encoded = e.encode(s)`; сравнить последовательность `step` от 0 с границами `encoding_rs::Decoder::decode_to_str` от начала encoded.
    - 64 cases.
    - _Requirements: R6.2 / Property 11_

  - [x] 7.6 PBT Property 12 — `next_line_start` всегда возвращает offset на границе символа
    - Дополнить `tests/encoding_engine/prop_newline.rs` тегом Property 12.
    - PBT: для всех движков (`Utf8Engine`, `SingleByteEngine`, `Utf16Engine<LE/BE>`, `MultiByteEngine` × 3 kind) результат `next_line_start` находится на границе символа целевой кодировки.
    - 64 cases.
    - _Requirements: R6.3, R11.1, R11.2 / Property 12_

  - [x] 7.7 Per-encoding integration tests `shift_jis` / `gb18030` / `euc_kr`
    - Создать `tests/encoding_engine/per_encoding/{shift_jis.rs, gb18030.rs, euc_kr.rs}`: фиксированная четвёрка контрактов (open / viewport / literal+regex search / save round-trip без правок) с ASCII, не-ASCII CJK, LF/CR/CRLF, пустым файлом и файлом без trailing newline.
    - Большие fixture в `fresh_test_dir`.
    - _Requirements: R9.1, R9.2, R9.3, R14.1, R14.2, R14.3, R14.5_

  - [x] 7.8 Build gate Phase 7
    - Стандартный gate.
    - _Requirements: R16.4_

- [ ] 8. Phase 8 — Edit buffer для не-UTF-8 через piece-tree
  - _Requirements: R7.1, R7.2, R7.3, R7.4, R7.5, R7.6, R7.7, R7.8, R12.1, R12.2, R13.1, R13.2, R14.4_

  - [x] 8.1 Модуль `src/document/alignment.rs` с `align_byte_offset`
    - Новый файл `src/document/alignment.rs`: `pub(crate) enum AlignDirection { Backward, Forward }` и `Document::align_byte_offset(&self, offset, dir)`.
    - UTF-8 → существующие `align_utf8_boundary_*`. Class A → no-op. UTF-16 → `offset & !1` / `(offset + 1) & !1`. Class B → scan-from-anchor через `self.encoding_engine`.
    - _Requirements: R7.4, R12.1, R12.2_

  - [x] 8.2 Развилка `try_insert_text_at_encoded` в `commands.rs`
    - В `src/document/commands.rs` развести `try_insert_text_at` на два пути: UTF-8 → существующий путь Ropey (R7.5), Class A ∪ Class B → `try_insert_text_at_encoded`.
    - `encode_for_insert` через `encoding_rs::Encoding::encode`; на `had_unmappable` вернуть `Err(DocumentError::Encoding{operation:"insert", kind:UnrepresentableText})` **до** записи в add-buffer (R7.3); на `output_encoding != target` — `RedirectedSaveTarget`.
    - Считать byte_offset вставки через `align_byte_offset(.., Backward)`.
    - Не транскодировать содержимое документа в UTF-8 (R7.6).
    - _Requirements: R7.1, R7.2, R7.3, R7.6, R7.7, R7.8_

  - [x] 8.3 `PieceTable::insert_encoded_bytes_at` в `editing.rs` / `piece_tree.rs`
    - Новый метод `pub(crate) fn insert_encoded_bytes_at(&mut self, byte_offset: usize, encoded: &[u8]) -> io::Result<EditOutcome>`: байты дописываются в `self.add`, `Piece(Add, add_offset, len)` вставляется в позиции `byte_offset` без UTF-8 нормализации.
    - При необходимости — рефакторинг `pieces.insert_piece_at_byte_offset` в `src/piece_tree.rs` для byte-offset режима.
    - _Requirements: R7.1, R7.2_

  - [x] 8.4 `try_replace_range` и `try_delete_range` для не-UTF-8
    - В `commands.rs`: для Class A ∪ Class B обе операции выравнивают границы через `align_byte_offset` (R7.4, R12.1) и работают напрямую с piece-tree без транскода в UTF-8.
    - Замена реализована как `delete + insert_encoded_bytes_at` в одной транзакции.
    - _Requirements: R7.1, R7.4, R7.6, R12.1, R12.2_

  - [x] 8.5 Save для не-UTF-8 piece-tree в `persistence.rs`
    - В `src/document/persistence.rs`: путь save для документа с `piece_table: Some` и не-UTF-8 кодировкой пишет байты piece-tree напрямую (без декода в UTF-8). UTF-8 путь не меняется.
    - Round-trip: open → edit → save → open даёт ту же декодированную последовательность (R14.4, R17.x — Property 17).
    - _Requirements: R7.1, R7.6, R9.1_

  - [x] 8.6 PBT Property 13 — insert с непредставимым символом → ошибка без мутации
    - Создать `tests/encoding_engine/prop_insert_unrepresentable.rs` с тегом Property 13.
    - PBT: для `e ∈ Class A ∪ Class B` и `&str` с хотя бы одним непредставимым в `e` Unicode scalar — `try_insert` возвращает `Err(UnrepresentableText)`; `is_dirty() == false`; общий байтовый размер документа не изменился; `add_buffer.len()` (если piece-tree существует) не вырос.
    - 64 cases.
    - _Requirements: R7.3 / Property 13_

  - [x] 8.7 PBT Property 14 — все смещения границ выровнены на символ текущей кодировки
    - Создать `tests/encoding_engine/prop_alignment.rs` с тегом Property 14.
    - PBT: для всех публичных операций, возвращающих байтовое или текстовое смещение (insert position, delete range, regex и literal match start/end, line start/end), результат на границе символа `d.encoding()` (UTF-16 — чётный байт; UTF-8 — `is_char_boundary`; Class A — любой; Class B — достижим `step_forward` от начала строки).
    - 64 cases.
    - _Requirements: R5.7, R5.9, R7.4, R12.1, R12.2 / Property 14_

  - [x] 8.8 PBT Property 16 — insert round-trip через encode/decode
    - Создать `tests/encoding_engine/prop_insert_roundtrip.rs` с тегом Property 16.
    - PBT: для `e ∈ Class A ∪ Class B` и представимого в `e` `&str`: `Document::with_encoding(e)` → `try_insert(origin, s)` → `decode_with_bom_removal(add_buffer_bytes) == s`.
    - 64 cases.
    - _Requirements: R13.1, R13.2 / Property 16_

  - [x] 8.9 PBT Property 17 — save round-trip после представимых правок
    - Создать `tests/encoding_engine/prop_edit_save_roundtrip.rs` с тегом Property 17.
    - PBT: для `e ∈ Class A ∪ Class B`, начального файла в `e` и последовательности правок ограниченных представимыми символами — `open(e) → edits → save → reopen(e)` даёт текстовое содержимое, идентичное результату последовательного применения тех же правок к decoded исходному тексту.
    - Ограничение размера: ≤ 1 MiB на iteration. Большие fixture — `fresh_test_dir` (R14.5).
    - 64 cases.
    - _Requirements: R14.4, R9.1 / Property 17_

  - [x] 8.10 Edit-tests в per-encoding suite
    - Дополнить `tests/encoding_engine/per_encoding/{windows_1251, windows_1252, koi8_r, ibm866, latin1, iso_8859_15, utf16_le, utf16_be, shift_jis, gb18030, euc_kr}.rs` пятым контрактом `edit_and_save_round_trip` (R14.4): вставка ASCII, вставка не-ASCII символа кодировки, удаление range, save, повторное открытие, проверка декодированного содержимого.
    - _Requirements: R14.4, R14.5_

  - [ ] 8.11 Build gate Phase 8
    - Стандартный gate.
    - _Requirements: R16.4_

- [ ] 9. Phase 9 — Reverse-DFA regex и удаление chunked-from-end fallback
  - _Requirements: R8.1, R8.2, R8.3, R8.4, R8.5_

  - [x] 9.1 Cargo.toml: добавить `regex_automata = "0.4"`
    - Добавить `regex_automata = "0.4"` в `[dependencies]` (точно зафиксированная минорная версия).
    - Не удалять `regex 1.12`; оба крейта используются совместно (R16.3).
    - Прогнать build, удостовериться, что lockfile обновлён без других пакетных скачков.
    - _Requirements: R8.1, R16.3_

  - [x] 9.2 Расширить `RegexSearchQuery`: поле `reverse: OnceLock<dense::DFA<Vec<u32>>>` и `ensure_reverse`
    - В `src/document/regex_search.rs` добавить поле `reverse: OnceLock<dense::DFA<Vec<u32>>>` и метод `pub(crate) fn ensure_reverse(&self) -> Result<&dense::DFA<Vec<u32>>, RegexCompileError>`.
    - Использовать `dense::Builder::new().configure(...).build_reverse_with_size_limit(REVERSE_DFA_SIZE_LIMIT_BYTES = 32 * 1024 * 1024, &self.pattern)`.
    - При превышении лимита — `Err(RegexCompileError)` (без panic).
    - Маршрутизация без поля «direction» в `RegexSearchQuery` (R8.2).
    - _Requirements: R8.1, R8.5_

  - [x] 9.3 `reverse_dfa_search_in_slice` — путь mmap (zero-copy)
    - В `regex_search.rs` функция `reverse_dfa_search_in_slice(doc, dfa, bound_start, slice_start_off, slice)` — `try_search_rev` по `&storage.bytes()[start..end]`, маппинг `HalfMatch` в `(start, end)` и пост-фильтр через `Document::align_byte_offset` (Property 14).
    - Без 8 MiB cap.
    - _Requirements: R8.2, R12.1_

  - [x] 9.4 `reverse_dfa_search_in_piece_tree` — chunked окна от end к start
    - Окна `REGEX_CHUNK_BYTES = 8 MiB` от `end_off` к `start_off` с overlap `1 MiB`. На каждом окне `try_search_rev`, маппинг и пост-фильтр выравнивания.
    - _Requirements: R8.2, R12.1_

  - [x] 9.5 `reverse_dfa_search_in_rope` — `rope.chunks()` в обратном порядке
    - Проход `rope.chunks()` с конверсией каждого `&str` в `&[u8]` через `.as_bytes()`; `try_search_rev` по байтам chunk; маппинг и пост-фильтр.
    - _Requirements: R8.2, R12.1_

  - [x] 9.6 Маршрутизация `find_prev_regex*` через reverse-DFA
    - В `regex_search.rs` все 4 публичные функции `find_prev_regex` / `find_prev_regex_query` / `find_prev_regex_query_in_range` / `find_prev_regex_query_between` (плюс обёртки в `editor/tab.rs` и `editor/session.rs`) делегируют на `find_prev_regex_via_reverse_dfa`.
    - Маршрутизация — по имени вызванной функции, без полей direction.
    - _Requirements: R8.2_

  - [x] 9.7 Удалить `find_prev_regex_via_forward_scan` и старые chunked-from-end функции
    - Удалить `find_prev_regex_via_forward_scan`, `find_prev_regex_in_bytes_bounded`, `find_prev_regex_in_byte_slice`, `find_prev_regex_in_rope_bounded` (или сохранить под `#[cfg(test)]` если на них завязаны тесты, но не использовать в production-пути).
    - _Requirements: R8.2_

  - [x] 9.8 Адаптация `regex_tests.rs` под reverse-DFA path
    - Обновить ожидаемые match coordinates / порядок матчей в `src/document/regex_tests.rs` под reverse-DFA вместо forward-scan.
    - Удалить тесты, специфичные для forward-scan fallback.
    - _Requirements: R8.2, R8.3_

  - [x] 9.9 PBT Property 18 — reverse симметричен forward regex
    - Создать `tests/encoding_engine/prop_reverse_regex.rs` с тегом Property 18.
    - PBT: для `pattern` (компилируемого в forward и reverse DFA в пределах лимитов) и любого `Document` (mmap / piece-tree / rope) множество `(start, end)` от `find_all_regex` (forward) совпадает с множеством, полученным reverse-DFA проходом от конца к началу (в обратном порядке).
    - 64 cases.
    - _Requirements: R8.3 / Property 18_

  - [x] 9.10 PBT Property 19 — reverse-DFA size limit как типизированная ошибка
    - Создать `tests/encoding_engine/prop_reverse_dfa_overflow.rs` с тегом Property 19.
    - PBT-стратегия: генерируется длинная альтернация (`a{0,N}|b{0,N}|...` с большими `N`); ожидается `Err(RegexCompileError)` с непустым сообщением, без panic / overflow / OOM.
    - 64 cases (с `prop_assume!` — отбрасываются паттерны, успешно компилирующиеся в лимит).
    - _Requirements: R8.5 / Property 19_

  - [x] 9.11 Perf-тест dense vs sparse regex ratio ≤ 5×
    - Создать `tests/encoding_engine/perf/dense_vs_sparse.rs`: deterministic example-based perf-тест на 2 фиксированных fixture (dense match pattern и sparse match pattern), отношение времени `find_prev_regex` ≤ 5× (заменяет действующий 80× гард).
    - Большие fixture — в `fresh_test_dir` (R14.5).
    - _Requirements: R8.4, R14.5_

  - [ ] 9.12 Build gate Phase 9
    - Стандартный gate + perf-тест зелёный.
    - _Requirements: R16.4_

- [ ] 10. Phase 10 — Симметрия `step_backward` и финализация alignment
  - _Requirements: R10.1, R10.2, R10.3_

  - [x] 10.1 PBT Property 15 — `step_forward` и `step_backward` взаимно обратны
    - Дополнить `tests/encoding_engine/prop_step.rs` тегом Property 15.
    - PBT: для всех 4 движков (`Utf8Engine`, `SingleByteEngine`, `Utf16Engine<LE/BE>`, `MultiByteEngine` × 3 kind) и любой валидной byte sequence в соответствующей кодировке, для любого `p` на границе символа: `step_forward(p) > 0 ⟹ step_backward(p + step_forward(p)) == step_forward(p)`; `step_backward(p) > 0 ⟹ step_forward(p - step_backward(p)) == step_backward(p)`.
    - При необходимости — добавить недостающие реализации `step_backward` (если что-то не было дополнено в Phase 6/7/8).
    - 64 cases.
    - _Requirements: R10.1, R10.2, R10.3 / Property 15_

  - [ ] 10.2 Усиление PBT Property 14 на все публичные методы со смещениями
    - Дополнить `tests/encoding_engine/prop_alignment.rs`: добавить генераторы для всех публичных API, возвращающих байтовое/текстовое смещение (включая `find_*_regex_*`, `find_*_literal_*`, viewport bounds, line bounds), для всех реализованных кодировок.
    - 64 cases.
    - _Requirements: R5.7, R5.9, R7.4, R12.1, R12.2 / Property 14_

  - [ ] 10.3 Build gate Phase 10
    - Стандартный gate.
    - _Requirements: R16.4_

- [ ] 11. Phase 11 — Документация и финализация релиза 0.8.0
  - _Requirements: R15.1, R15.2, R15.3, R15.4, R17.1, R17.2_

  - [ ] 11.1 README матрица поддержки кодировок
    - В `README.md` добавить раздел `## Encoding Support Matrix (0.8.0)` с таблицей: Encoding, Engine (`Utf8Engine` / `SingleByteEngine` / `Utf16Engine` / `MultiByteEngine`), Open, Viewport, Search, Edit, Save round-trip — для всех имён из R3.8 + UTF-8 / UTF-8 BOM + UTF-16LE/BE + Shift_JIS / GB18030 / EUC-KR.
    - Указать, что прочие labels через `OpenEncodingPolicy::Reinterpret` не входят в supported contract.
    - _Requirements: R15.1_

  - [ ] 11.2 lib.rs rustdoc матрица
    - В `src/lib.rs` в crate-level rustdoc (`//!` блок сразу после overview) повторить ту же матрицу из README.
    - Прогнать `cargo doc --no-deps` локально — без warnings.
    - _Requirements: R15.2_

  - [ ] 11.3 CHANGELOG секция 0.8.0
    - В `CHANGELOG.md` создать секцию `## 0.8.0 - <YYYY-MM-DD>` со списком всех ломающих изменений encoding-слоя относительно `0.7.1`: добавление поля `Document::encoding_engine` и helper `set_encoding_contract`, запрет full-rope для не-UTF-8, новые движки `SingleByteEngine` / `Utf16Engine<E>` / `MultiByteEngine`, edit через piece-tree без UTF-8 транскода, переход `find_prev_regex*` на reverse-DFA, удаление chunked-from-end fallback, новый perf-гард 5×.
    - Зафиксировать R17.1 (в `< 1.0.0` ломающие изменения разрешены) и R17.2.
    - _Requirements: R15.3, R17.1, R17.2_

  - [ ] 11.4 MIGRATION-0.8.md
    - Создать `MIGRATION-0.8.md` в корне репозитория: для каждой публичной точки encoding-слоя описать что изменилось от `0.7.1`, какие методы поменяли сигнатуру, какие политики open и save получили новые гарантии, какие пути декодирования удалены.
    - Включить разделы по фазам: Document accessor, open dispatch, edit policy, regex reverse path.
    - _Requirements: R15.4, R17.2_

  - [ ] 11.5 Финальная сверка 17 требований и 19 properties + build gate Phase 11
    - Создать чек-лист `docs/0.8.0-readiness.md` (или дописать в MIGRATION-0.8.md) сопоставляющий каждое из 17 требований с задачей tasks.md и каждое из 19 properties с PBT-файлом, в котором оно проверяется.
    - Прогнать финальный build gate: `cargo fmt --all --check`, `cargo clippy --all-targets --all-features --workspace -- -D warnings`, `cargo test --all-features --workspace`.
    - Проверить `cargo doc --no-deps` без warnings.
    - Не публиковать на crates.io и не пушить в GitHub без отдельного явного согласия пользователя (R16.5).
    - _Requirements: R15.1, R15.2, R15.3, R15.4, R16.4, R16.5_

## Notes

- Каждая верхнеуровневая задача (Task 2..11) — это одна фаза. Подзадачи — отдельные коммиты внутри фазы.
- Каждая фаза заканчивается build gate task: `cargo fmt --all --check`, `cargo clippy --all-targets --all-features --workspace -- -D warnings`, `cargo test --all-features --workspace` (R16.4).
- Phase 3 имеет build gate **между каждой парой** миграций callsites (3.1 → 3.2 → ... → 3.5), как явно требует R2.2.
- Все PBT-задачи запускаются с `ProptestConfig { cases: 64, .. }` и тегируются `Feature: encoding-aware-engine, Property N: ...`.
- Большие интеграционные тесты используют `fresh_test_dir(name)` поверх `$env:TMP` / `$env:TEMP` (= `D:\qem_test_tmp`, R14.5).
- В рамках этой спеки **запрещено** публиковать крейт на crates.io, пушить в GitHub, создавать релизные теги без явного согласия пользователя (R16.5). Эти действия не входят ни в одну задачу плана.
- Каждая подзадача рассчитана на один коммит, проходящий build gate.

## Task Dependency Graph

```json
{
  "waves": [
    { "id": 0, "tasks": ["2.1"] },
    { "id": 1, "tasks": ["2.2", "2.3"] },
    { "id": 2, "tasks": ["2.4"] },
    { "id": 3, "tasks": ["3.1"] },
    { "id": 4, "tasks": ["3.2"] },
    { "id": 5, "tasks": ["3.3"] },
    { "id": 6, "tasks": ["3.4"] },
    { "id": 7, "tasks": ["3.5"] },
    { "id": 8, "tasks": ["3.6"] },
    { "id": 9, "tasks": ["4.1", "4.2", "4.3"] },
    { "id": 10, "tasks": ["5.1"] },
    { "id": 11, "tasks": ["5.2", "5.3"] },
    { "id": 12, "tasks": ["5.4", "5.5", "5.6"] },
    { "id": 13, "tasks": ["6.1"] },
    { "id": 14, "tasks": ["6.2", "6.3"] },
    { "id": 15, "tasks": ["6.4", "6.5"] },
    { "id": 16, "tasks": ["6.6", "6.7", "6.8", "6.9"] },
    { "id": 17, "tasks": ["7.1"] },
    { "id": 18, "tasks": ["7.2", "7.3"] },
    { "id": 19, "tasks": ["7.4"] },
    { "id": 20, "tasks": ["7.5", "7.6", "7.7"] },
    { "id": 21, "tasks": ["8.1"] },
    { "id": 22, "tasks": ["8.3"] },
    { "id": 23, "tasks": ["8.2"] },
    { "id": 24, "tasks": ["8.4", "8.5"] },
    { "id": 25, "tasks": ["8.6", "8.7", "8.8", "8.9", "8.10"] },
    { "id": 26, "tasks": ["9.1"] },
    { "id": 27, "tasks": ["9.2"] },
    { "id": 28, "tasks": ["9.3", "9.4", "9.5"] },
    { "id": 29, "tasks": ["9.6"] },
    { "id": 30, "tasks": ["9.7", "9.8"] },
    { "id": 31, "tasks": ["9.9", "9.10", "9.11"] },
    { "id": 32, "tasks": ["10.1", "10.2"] },   
    { "id": 33, "tasks": ["11.1", "11.2", "11.3", "11.4"] },
    { "id": 34, "tasks": ["11.5"] }
  ]
}
```
