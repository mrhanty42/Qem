# Requirements Document

## Introduction

Спека описывает остаток работы (фазы 2–11) над encoding-aware движком в
библиотеке Qem (Rust text engine), завершающий релиз `0.8.0`. Фаза 1 уже
реализована: трейт `EncodingEngine` и реализация `Utf8Engine` живут в
`src/document/encoding_engine.rs`, accessor `Document::encoding_engine()`
маршрутизирует вызовы через `engine_for_encoding(self.encoding)`, базовый
набор из 339 тестов и 6 unit-тестов фазы 1 проходит зелёным.

Задача релиза `0.8.0` — превратить encoding-aware движок из «трейт +
UTF-8» в полностью нативный путь для всех заявленных кодировок без
транскодирования в UTF-8: одно- и многобайтовые ASCII-расширения и
UTF-16 LE/BE едут по своему `EncodingEngine`, правки не-UTF-8 идут через
piece-tree напрямую, а не через `ropey`. `Ropey` сохраняется в `0.8` для
UTF-8 буферов; его удаление — задача `0.9.0`.

Все требования сформулированы в формате EARS (ключевые слова `WHEN`,
`IF`, `THEN`, `WHILE`, `WHERE`, `THE`, `SHALL` оставлены на английском
как стандартизованные служебные термины EARS) и согласованы с правилами
качества INCOSE.

## Glossary

- **Document** — основная структура движка документа (`struct Document`
  в `src/document.rs`), владеющая backing-ом, piece-tree и текущим
  encoding-контрактом.
- **EncodingEngine** — публичный (внутри крейта) трейт в
  `src/document/encoding_engine.rs`, описывающий байтовые операции
  `step`, `next_line_start`, `count_columns_exact`,
  `count_columns_bounded`, `advance_offset_by_text_units`.
- **Utf8Engine** — реализация `EncodingEngine` для UTF-8, делегирующая
  существующим free-функциям.
- **SingleByteEngine** — реализация `EncodingEngine` для односбайтовых
  ASCII-расширений (см. Class A); шаг по символу всегда равен одному
  байту.
- **Utf16Engine** — реализация `EncodingEngine`, параметризованная типом
  endianness (`LittleEndian` / `BigEndian`), в которой шаг по символу
  равен 2 байтам (4 байта для surrogate pair) и поиск перевода строки
  выровнен по 2-байтовой границе.
- **MultiByteEngine** — реализация `EncodingEngine` для переменно-длинных
  CJK-кодировок (`Shift_JIS`, `GB18030`, `EUC-KR`) с собственным
  детектором лидирующих байт и false-positive-aware поиском `0x0A` /
  `0x0D`.
- **Class A** — множество кодировок, обрабатываемых `SingleByteEngine`:
  `windows-1251`, `windows-1252`, `latin1` (`ISO-8859-1`/-2/.../-16),
  `KOI8-R`, `KOI8-U`, `IBM866` (он же `cp866`), `windows-1250`,
  `windows-1253`–`windows-1258`, `windows-874`, `macintosh`,
  `x-mac-cyrillic`. Шаг символа — 1 байт.
- **Class B** — множество кодировок с собственным движком:
  `UTF-16LE`, `UTF-16BE`, `Shift_JIS`, `GB18030`, `EUC-KR`.
- **MmapPath** — путь чтения документа поверх `memmap2::Mmap` без
  полной материализации rope.
- **PieceTree** — piece-table движок Qem (`src/piece_tree.rs`).
- **Ropey** — внешняя зависимость `ropey 1.6`; в `0.8` сохраняется
  только для UTF-8 буферов и удаляется в `0.9.0`.
- **DocumentEncoding** — типизированная обёртка над
  `&'static encoding_rs::Encoding` (`src/document/types.rs`).
- **DocumentEncodingErrorKind** — типизированный набор причин ошибок
  кодировки (`UnrepresentableText`, `UnsupportedSaveTarget` и т.п.).
- **EncodingError_Unrepresentable** — вариант
  `DocumentEncodingErrorKind::UnrepresentableText`, возвращаемый при
  вставке символа, не представимого в целевой кодировке.
- **RegexEngine** — путь регулярных выражений Qem на базе крейтов
  `regex` и `regex_automata`. Свой движок регулярных выражений в
  `0.8.0` не пишется.
- **ReverseDfaRegex** — реверс-DFA, построенный через
  `regex_automata::dfa::dense::Builder::build_reverse_with_size_limit`,
  заменяющий текущий chunked-from-end fallback в обратном поиске.
- **PerEncodingTestSuite** — набор тестов для каждой реализованной
  кодировки, покрывающий open, viewport, search, edit и save round-trip.
- **PBT** — property-based тесты на крейте `proptest`.
- **TmpDir** — каталог для временных файлов больших тестов
  (`$env:TMP` / `$env:TEMP` указывают на `D:\qem_test_tmp`).
- **Build_Gate** — обязательная цепочка проверки перед признанием фазы
  завершённой: `cargo fmt --all --check`,
  `cargo clippy --all-targets --all-features --workspace -- -D warnings`,
  `cargo test --all-features --workspace`.
- **CHANGELOG** — файл `CHANGELOG.md` в корне репозитория.
- **MIGRATION_Doc** — файл `MIGRATION-0.8.md` в корне репозитория.
- **README** — файл `README.md` в корне репозитория.
- **Lib_Rustdoc** — публичный rustdoc на уровне крейта в `src/lib.rs`.

## Requirements

### Requirement 1: Document владеет ссылкой на encoding engine

**User Story:** Как мейнтейнер Qem, я хочу чтобы каждый `Document` хранил
ссылку на свой `EncodingEngine` как поле, а не вычислял его на каждом
вызове accessor-а, чтобы encoding-aware путь стал тривиально дешёвым и
все будущие фазы работали с одним и тем же engine-ом за весь жизненный
цикл документа.

#### Acceptance Criteria

1. THE Document SHALL содержать поле `encoding_engine` типа
   `&'static dyn EncodingEngine`.
2. WHEN `Document` создаётся через любой публичный или внутренний
   конструктор, THE Document SHALL заполнить `encoding_engine`
   значением, возвращённым `engine_for_encoding(self.encoding)`.
3. WHEN документ создаётся как новый пустой буфер без явной кодировки,
   THE Document SHALL установить `encoding_engine` равным
   `UTF8_ENGINE`.
4. WHEN кодировка документа сменяется в результате операции
   reinterpret или save-конверсии, THE Document SHALL пересчитать
   `encoding_engine` через `engine_for_encoding` в той же транзакции,
   что и обновление поля `encoding`.
5. THE Document SHALL предоставлять accessor `encoding_engine(&self)
   -> &'static dyn EncodingEngine`, возвращающий хранимое поле без
   дополнительной диспетчеризации.
6. WHEN фаза 2 завершена, THE Build_Gate SHALL проходить зелёным, а
   набор из 339 тестов проекта SHALL остаться зелёным без изменений в
   количестве и наблюдаемом поведении.

### Requirement 2: Миграция callsites с free-функций на engine

**User Story:** Как разработчик document-слоя, я хочу заменить прямые
вызовы свободных байтовых функций (`utf8_step`,
`next_line_start_exact`, `count_text_columns_exact`,
`count_text_columns`, `advance_offset_by_text_units_in_bytes`) в
горячих путях на вызовы `self.encoding_engine.method(...)`, чтобы
поведение каждого callsite определялось текущей кодировкой документа,
а не жёстко UTF-8.

#### Acceptance Criteria

1. THE Document SHALL направлять каждый вызов байтовых операций в
   модулях `src/document/positions.rs`, `src/document/reads.rs`,
   `src/document/search.rs`, `src/document/regex_search.rs` и
   `src/document/commands.rs` через метод хранимого
   `self.encoding_engine`, а не через свободные функции
   `utf8_step` / `next_line_start_exact` /
   `count_text_columns_exact` / `count_text_columns` /
   `advance_offset_by_text_units_in_bytes`.
2. WHEN миграция callsite применена в одном файле, THE Build_Gate
   SHALL быть запущен, и его прохождение SHALL быть условием перехода
   к миграции следующего файла.
3. WHEN все перечисленные модули мигрированы, THE Document SHALL не
   содержать прямых вызовов перечисленных свободных функций ни в одном
   методе, кроме реализации `Utf8Engine`.
4. WHILE кодировка документа равна `UTF-8`, THE Document SHALL
   возвращать байтово идентичные результаты для всех публичных и
   тестируемых байтовых операций по сравнению с поведением до
   миграции.
5. WHEN фаза 3 завершена, THE Build_Gate SHALL проходить зелёным.

### Requirement 3: SingleByteEngine для всех кодировок Class A

**User Story:** Как пользователь Qem, я хочу читать и редактировать
файлы в `windows-1251`, `latin1`, `KOI8-R`, `cp866` и `windows-1252`
нативно, чтобы открытие большого файла не упиралось в полную
транскодировку в UTF-8 и хранение в rope.

#### Acceptance Criteria

1. THE SingleByteEngine SHALL быть параметризован значением
   `&'static encoding_rs::Encoding`, передаваемым через
   `DocumentEncoding`.
2. WHEN метод `step(bytes, offset, end)` вызывается на любой
   кодировке Class A и `offset < end`, THE SingleByteEngine SHALL
   вернуть значение `1`.
3. WHEN метод `step(bytes, offset, end)` вызывается и `offset >= end`,
   THE SingleByteEngine SHALL вернуть значение `0`.
4. WHEN метод `next_line_start` обрабатывает байт `0x0A`, байт `0x0D`
   или последовательность `0x0D 0x0A` в любой кодировке Class A, THE
   SingleByteEngine SHALL трактовать `CRLF` как один перенос строки и
   возвращать смещение сразу за байтом `0x0A`.
5. THE SingleByteEngine SHALL реализовывать
   `count_columns_exact`, `count_columns_bounded` и
   `advance_offset_by_text_units` с шагом ровно 1 байт на текстовый
   юнит и с тем же CRLF-схлопыванием, что и `Utf8Engine`.
6. WHEN `engine_for_encoding(encoding)` вызывается для любой кодировки
   Class A, THE Document SHALL получить ссылку на статический
   `SingleByteEngine`, кэшированный по имени кодировки.
7. WHEN viewport читает окно из mmap-backing для документа в кодировке
   Class A, THE Document SHALL декодировать только байты этого окна
   через `encoding_rs` и SHALL NOT материализовать полный rope для
   файла.
8. THE SingleByteEngine SHALL покрывать как минимум следующие имена
   кодировок (по `encoding_rs`): `windows-1251`, `windows-1252`,
   `windows-1250`, `windows-1253`, `windows-1254`, `windows-1255`,
   `windows-1256`, `windows-1257`, `windows-1258`, `windows-874`,
   `ISO-8859-2`–`ISO-8859-16`, `KOI8-R`, `KOI8-U`, `IBM866`,
   `macintosh`, `x-mac-cyrillic`.

### Requirement 4: open dispatch для Class A через SingleByteEngine

**User Story:** Как пользователь, открывающий легаси-файл в
`windows-1251` или `cp866`, я хочу чтобы Qem использовал нативный mmap
и SingleByteEngine вместо построения полного UTF-8-rope, чтобы открытие
больших файлов оставалось мгновенным и не требовало доступной памяти,
кратной размеру файла.

#### Acceptance Criteria

1. WHEN `Document::from_storage_with_encoding` вызывается для кодировки
   Class A, THE Document SHALL построить документ поверх MmapPath с
   `SingleByteEngine` без построения полного rope.
2. THE Document SHALL удалить путь декодирования с fallback в полный
   `Rope` для кодировок Class A.
3. WHEN документ Class A открыт через MmapPath, THE Document SHALL
   декодировать байты в строки только при чтении viewport,
   line-slice или search/regex-результата, и THE Document SHALL
   делать это только для запрошенного байтового окна.
4. WHEN документ Class A прошёл сохранение без правок,
   THE Document SHALL вернуть на выходе байты, идентичные исходным
   байтам файла (round-trip).
5. WHEN фаза 5 завершена, THE Build_Gate SHALL проходить зелёным.

### Requirement 5: Utf16Engine для UTF-16 LE и BE

**User Story:** Как пользователь, открывающий UTF-16-файл (например,
лог Windows-приложения), я хочу нативной поддержки `UTF-16LE` и
`UTF-16BE` без транскода в UTF-8, чтобы навигация, поиск и правка
сохраняли исходную кодировку.

#### Acceptance Criteria

1. THE Utf16Engine SHALL быть параметризован типом endianness
   (`LittleEndian` или `BigEndian`).
2. WHEN метод `step` вызывается на BMP-символе UTF-16, THE Utf16Engine
   SHALL вернуть значение `2`.
3. WHEN метод `step` вызывается на high surrogate UTF-16 (`0xD800`–
   `0xDBFF`) и за ним следует low surrogate (`0xDC00`–`0xDFFF`),
   THE Utf16Engine SHALL вернуть значение `4`.
4. WHEN метод `next_line_start` ищет перенос строки в `UTF-16LE`,
   THE Utf16Engine SHALL искать байтовые последовательности `0x0A 0x00`
   и `0x0D 0x00`, выровненные по чётной байтовой границе.
5. WHEN метод `next_line_start` ищет перенос строки в `UTF-16BE`,
   THE Utf16Engine SHALL искать байтовые последовательности `0x00 0x0A`
   и `0x00 0x0D`, выровненные по чётной байтовой границе.
6. IF поиск перевода строки попадает на нечётный байт (середину
   2-байтового кодового юнита), THEN THE Utf16Engine SHALL отвергнуть
   этот кандидат как ложно-положительный.
7. THE Utf16Engine SHALL выравнивать смещения начала и конца правки на
   чётную байтовую границу.
8. THE Document SHALL применить regex-поиск для UTF-16-документа одним
   из двух способов: либо компиляцией паттерна в UTF-16-байтовую форму,
   либо chunked-decode со склейкой граничных матчей; выбранный способ
   SHALL быть зафиксирован в design-документе спеки с обоснованием.
9. WHEN regex-матч возвращён для UTF-16-документа, THE Document SHALL
   возвращать смещения, выровненные на чётный байт.
10. WHILE экземпляр `Utf16Engine` сконфигурирован как `LittleEndian`,
    THE Utf16Engine SHALL искать только LE-формы переноса строки и
    SHALL NOT обрабатывать BE-формы; и наоборот, WHILE экземпляр
    `Utf16Engine` сконфигурирован как `BigEndian`, THE Utf16Engine SHALL
    искать только BE-формы и SHALL NOT обрабатывать LE-формы.

### Requirement 6: MultiByteEngine для Shift_JIS, GB18030, EUC-KR

**User Story:** Как пользователь, работающий с японскими, китайскими и
корейскими файлами в их исходных кодировках, я хочу нативной поддержки
переменно-длинных CJK-кодировок без транскода в UTF-8.

#### Acceptance Criteria

1. THE MultiByteEngine SHALL поддерживать кодировки `Shift_JIS`,
   `GB18030` и `EUC-KR`.
2. THE MultiByteEngine SHALL содержать собственный детектор лидирующих
   байт для каждой поддерживаемой кодировки и SHALL вернуть из метода
   `step` ровно тот шаг, который соответствует длине одного символа
   данной кодировки от текущего лидирующего байта.
3. WHEN метод `next_line_start` ищет байт `0x0A` или `0x0D` в
   документе кодировки Class B, IF этот байт может встречаться как
   trailing-байт многобайтового символа (false positive),
   THEN THE MultiByteEngine SHALL отбросить такого кандидата и
   продолжить поиск.
4. THE MultiByteEngine SHALL быть выбран `engine_for_encoding(encoding)`
   для каждой из перечисленных кодировок.
5. WHEN viewport читает окно для документа Class B (вне UTF-16),
   THE Document SHALL декодировать только байты окна через
   `encoding_rs` и SHALL NOT материализовать полный rope для файла.

### Requirement 7: Edit buffer для не-UTF-8 через piece-tree

**User Story:** Как пользователь, редактирующий файл в legacy-кодировке,
я хочу чтобы вставка текста писала байты целевой кодировки прямо в
piece-tree add buffer, без промежуточного UTF-8-rope, и чтобы Qem
честно сообщал о неподдерживаемых символах вместо тихих замен.

#### Acceptance Criteria

1. WHEN документ имеет кодировку из Class A или Class B и пользователь
   выполняет операцию вставки, THE Document SHALL направить правку в
   PieceTree, а не в Ropey.
2. WHEN операция вставки получает на вход `&str`, THE Document SHALL
   закодировать его в целевую кодировку через
   `encoding_rs::Encoding::encode` и SHALL дописать полученные байты в
   add-buffer PieceTree.
3. IF исходный `&str` содержит хотя бы один скаляр Unicode, не
   представимый в целевой кодировке, THEN THE Document SHALL вернуть
   ошибку варианта `DocumentEncodingErrorKind::UnrepresentableText` и
   SHALL NOT модифицировать add-buffer.
4. THE Document SHALL выравнивать смещения начала и конца правки на
   границу символа целевой кодировки (1 байт для Class A, 2 байта для
   UTF-16 с учётом surrogate pair, длина текущего символа для Class B).
5. WHILE документ имеет кодировку UTF-8, THE Document SHALL продолжать
   использовать существующий путь редактирования через Ropey.
6. WHILE документ имеет кодировку из Class A или Class B,
   THE Document SHALL NOT транскодировать содержимое документа в UTF-8
   при вставке в не-UTF-8 документ.
7. WHERE документ уже находится в кодировке UTF-8, THE Document SHALL
   разрешать UTF-8 операции чтения и редактирования над текстом без
   ограничения, накладываемого критерием 6.
8. WHERE пользователь явно запросил конверсию через
   `DocumentSaveOptions::with_encoding(UTF-8)` или явный
   reinterpret-open в UTF-8, THE Document SHALL разрешать
   операционно-необходимое преобразование байтов в UTF-8 для этой
   операции, и критерий 6 SHALL NOT применяться к такому явному
   запросу.

### Requirement 8: Reverse-DFA regex для обратного поиска

**User Story:** Как пользователь, выполняющий regex find-prev на
большом файле, я хочу чтобы Qem использовал предкомпилированный
реверс-DFA вместо chunked-from-end fallback, чтобы обратный поиск был
не катастрофически медленнее прямого.

#### Acceptance Criteria

1. THE Document SHALL построить ReverseDfaRegex через
   `regex_automata::dfa::dense::Builder::build_reverse_with_size_limit`
   при компиляции `RegexSearchQuery`, для которого требуется обратный
   поиск.
2. WHEN пользователь вызывает `find_prev_regex` или `find_prev_regex_*`
   на mmap, piece-tree или rope backing, THE Document SHALL
   маршрутизировать поиск через ReverseDfaRegex и SHALL NOT
   использовать существующий chunked-from-end fallback. Маршрутизация
   SHALL происходить исключительно по вызванной функции и SHALL NOT
   зависеть от каких-либо дополнительных полей направления поиска в
   `RegexSearchQuery`.
3. FOR ALL запросов и backing-ов, на которых ReverseDfaRegex и текущий
   forward-поиск возвращают совпадения, THE Document SHALL вернуть из
   обратного поиска ровно те же совпадения, что и forward-поиск, в
   обратном порядке (свойство симметрии reverse vs forward).
4. THE PerEncodingTestSuite SHALL содержать перформанс-контракт,
   утверждающий что отношение времени dense regex к sparse regex не
   превышает `5x` (в `0.7.x` действовало `80x`).
5. IF построение реверс-DFA превышает лимит размера, THEN THE Document
   SHALL вернуть типизированную ошибку компиляции
   `RegexCompileError`, не паникуя и не падая в fallback.

### Requirement 9: Round-trip кодировки на open плюс save без правок

**User Story:** Как пользователь, я хочу чтобы открытие файла в
поддерживаемой кодировке и его сохранение без правок возвращали байты,
идентичные исходным, чтобы Qem не «нормализовывал» легаси-файлы тихо.

#### Acceptance Criteria

1. FOR ALL кодировок из множества `{UTF-8} ∪ Class A ∪ Class B`,
   WHEN документ открыт без правок и сохранён через `save`,
   THE Document SHALL вернуть на выходе байтовую последовательность,
   идентичную исходному файлу (round-trip property).
2. WHEN PerEncodingTestSuite проверяет round-trip, THE PerEncodingTestSuite
   SHALL включать как минимум один тест property-based на крейте
   `proptest` для каждой реализованной кодировки.
3. WHEN PerEncodingTestSuite проверяет round-trip, THE PerEncodingTestSuite
   SHALL покрывать ASCII-диапазон, не-ASCII символы целевой кодировки,
   `LF`, `CRLF` и `CR` строковые окончания, пустой файл и файл без
   завершающего перевода строки.

### Requirement 10: Симметрия step_forward и step_backward

**User Story:** Как мейнтейнер, я хочу чтобы для каждой реализации
`EncodingEngine` шаг вперёд и шаг назад были взаимно обратны, чтобы
курсорная навигация и обратный regex-поиск никогда не «застревали» и
не пересекали границы символа.

#### Acceptance Criteria

1. THE EncodingEngine SHALL предоставлять метод `step_backward(bytes,
   offset, start)`, симметричный методу `step` (`step_forward`).
2. FOR ALL валидных байтовых смещений `p` в документе, THE EncodingEngine
   SHALL удовлетворять равенство
   `step_forward(step_backward(p)) == p` (PBT-свойство).
3. FOR ALL валидных байтовых смещений `p`, THE EncodingEngine SHALL
   удовлетворять равенство
   `step_backward(step_forward(p)) == p` для всех `p`, в которых
   `step_forward(p) > 0`.

### Requirement 11: Поиск перевода строки не пересекает границу символа

**User Story:** Как пользователь Qem на CJK-файле, я хочу чтобы поиск
перевода строки не «стрелял» в trailing-байт `0x0A` или `0x0D` посреди
многобайтового символа, чтобы навигация по строкам не сдвигала курсор
в середину символа.

#### Acceptance Criteria

1. FOR ALL байтовых смещений `s`, возвращённых
   `EncodingEngine::next_line_start(bytes, file_len, line_start)`,
   THE EncodingEngine SHALL гарантировать что `s` находится на границе
   символа целевой кодировки.
2. WHEN MultiByteEngine видит байт `0x0A` или `0x0D` в позиции, которая
   распознана детектором лидирующих байт как trailing-байт текущего
   многобайтового символа, THE MultiByteEngine SHALL пропустить эту
   позицию и продолжить поиск.
3. WHEN Utf16Engine видит байт `0x0A` или `0x0D` в нечётной байтовой
   позиции, THE Utf16Engine SHALL пропустить эту позицию и продолжить
   поиск.

### Requirement 12: Regex match не пересекает границу символа

**User Story:** Как пользователь Qem на не-UTF-8 файле, я хочу чтобы
найденный regex-матч начинался и заканчивался на границе символа
текущей кодировки, чтобы выделение и замена не разрывали многобайтовые
символы.

#### Acceptance Criteria

1. FOR ALL `(start, end)` пар, возвращённых regex-поиском
   (`find_next_regex`, `find_prev_regex`, `find_all_regex` и их
   `_query` / `_in_range` / `_between` варианты),
   THE Document SHALL гарантировать что `start` и `end` находятся на
   границе символа текущей кодировки документа.
2. IF regex-движок предлагает кандидат, у которого хотя бы одна из
   границ попадает в середину многобайтового символа,
   THEN THE Document SHALL отвергнуть этого кандидата и продолжить
   поиск со смещения на следующей валидной границе символа.

### Requirement 13: Round-trip insert через encode плюс decode

**User Story:** Как разработчик, тестирующий кодировку в Qem, я хочу
property-based уверенности, что вставка строки `&str` в не-UTF-8
документ даёт после повторного декодирования ту же самую строку.

#### Acceptance Criteria

1. FOR ALL `&str`, состоящих только из символов, представимых в целевой
   кодировке `E`, WHEN текст вставлен в пустой документ кодировки `E`,
   THE Document SHALL гарантировать что декодирование байтов add-buffer
   через `encoding_rs::Encoding::decode_with_bom_removal` возвращает
   исходный `&str`.
2. THE PerEncodingTestSuite SHALL включать PBT-тест свойства из
   критерия 1 для каждой реализованной кодировки Class A и Class B,
   включая UTF-16LE и UTF-16BE.

### Requirement 14: PerEncodingTestSuite покрывает open viewport search edit save

**User Story:** Как мейнтейнер, я хочу чтобы каждая реализованная
кодировка имела одинаковый набор интеграционных тестов, чтобы регрессии
ловились локально для конкретной кодировки, а не только для UTF-8.

#### Acceptance Criteria

1. FOR ALL кодировок из множества `Class A ∪ Class B ∪ {UTF-8}`,
   THE PerEncodingTestSuite SHALL содержать как минимум один тест
   open: открытие файла, состоящего из ASCII, не-ASCII символов
   кодировки и смешанных строковых окончаний.
2. FOR ALL кодировок из того же множества, THE PerEncodingTestSuite
   SHALL содержать как минимум один тест viewport, читающий первое и
   последнее окно файла.
3. FOR ALL кодировок из того же множества, THE PerEncodingTestSuite
   SHALL содержать как минимум один тест literal-search и один тест
   regex-search, проверяющий нахождение известного подстрочного
   совпадения.
4. FOR ALL кодировок из множества `Class A ∪ Class B`,
   THE PerEncodingTestSuite SHALL содержать как минимум один тест
   edit, применяющий вставку и удаление и проверяющий save round-trip.
5. WHILE PerEncodingTestSuite запускается на тестах размером, требующем
   временных файлов, THE PerEncodingTestSuite SHALL использовать
   TmpDir, заданный переменными окружения `$env:TMP` / `$env:TEMP`
   (значение `D:\qem_test_tmp`).

### Requirement 15: Документация encoding-aware движка в `0.8.0`

**User Story:** Как пользователь Qem, я хочу видеть в README и
rustdoc-обзоре крейта матрицу поддерживаемых кодировок и в CHANGELOG /
MIGRATION-0.8.md явный список ломающих изменений `0.8.0`, чтобы
интеграция была предсказуемой.

#### Acceptance Criteria

1. THE README SHALL содержать матрицу поддержки кодировок,
   перечисляющую кодировку, путь движка (`Utf8Engine`,
   `SingleByteEngine`, `Utf16Engine`, `MultiByteEngine`), статус open,
   статус viewport, статус search, статус edit и статус save round-trip.
2. THE Lib_Rustdoc SHALL воспроизводить ту же матрицу поддержки
   кодировок в обзоре крейта `src/lib.rs`.
3. THE CHANGELOG SHALL фиксировать в секции `0.8.0` каждое ломающее
   изменение публичного API encoding-слоя относительно `0.7.1`, в том
   числе изменение конструкторов `Document` и сигнатур, связанных с
   кодировкой.
4. THE MIGRATION_Doc SHALL описывать путь миграции с `0.7.1` на
   `0.8.0` для encoding-API: какие методы изменили сигнатуру, какие
   политики open и save получили новые гарантии и какие пути
   декодирования удалены.

### Requirement 16: Build gate, ограничения релиза и неиспользование костылей

**User Story:** Как мейнтейнер релизного процесса, я хочу чтобы спека
явно фиксировала запрет транскода как стратегии, удержание ropey в
релизе `0.8`, использование готовых regex-крейтов и обязательную
зелёность build gate перед признанием каждой фазы завершённой.

#### Acceptance Criteria

1. THE Document SHALL NOT включать путь, который материализует
   содержимое не-UTF-8 файла полностью в UTF-8-rope как стратегию
   поддержки кодировки в `0.8.0`.
2. THE Document SHALL продолжать использовать `ropey 1.6` как
   реализацию edited-buffer для UTF-8-документов в `0.8.0`.
3. THE Document SHALL NOT включать собственную реализацию движка
   регулярных выражений в `0.8.0` и SHALL использовать крейты `regex`
   и `regex_automata`.
4. WHEN фаза `N` (для `N` от 2 до 11) объявляется завершённой,
   THE Build_Gate SHALL быть запущен и SHALL завершиться без ошибок:
   `cargo fmt --all --check`,
   `cargo clippy --all-targets --all-features --workspace -- -D warnings`,
   `cargo test --all-features --workspace`.
5. WHILE спека `encoding-aware-engine` реализуется,
   THE Document SHALL NOT публиковаться на crates.io и SHALL NOT
   пушиться в GitHub без явного подтверждения пользователя.

### Requirement 17: Семантика стабильности API в `0.8.0`

**User Story:** Как пользователь Qem с одним известным потребителем, я
готов к ломающим изменениям публичного API в `0.8.0`, при условии что
они описаны и не размазаны на минорные релизы после `1.0.0`.

#### Acceptance Criteria

1. WHILE релиз номер версии меньше `1.0.0`, THE Document SHALL
   разрешать ломающие изменения публичного API encoding-слоя без
   обещаний обратной совместимости.
2. WHEN изменение публичного API применено, THE CHANGELOG SHALL
   зафиксировать его в секции соответствующего релиза, и
   THE MIGRATION_Doc SHALL содержать раздел с миграционными шагами.
3. WHEN релиз `1.0.0` будет подготавливаться, THE Document SHALL
   зафиксировать публичный API encoding-слоя как стабильный, и
   дальнейшие ломающие изменения SHALL соответствовать semver.
