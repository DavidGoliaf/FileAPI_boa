# M7 — заказ на доработку после code acceptance review

| Поле | Значение |
|---|---|
| ID | M7-REWORK-1 |
| Статус | REWORK REQUIRED |
| База под review | commit b50c60ed52dd884253da559e30126b252027d116 |
| Предыдущая база | принятый M6 document head bc742df200c1aa1bc8f9cee63b0c20c37737d64c |
| Ветка | продолжать на текущей M7-ветке; новую ветку не создавать и не переносить исправления в main |
| Нормативный источник | TZ_boa_fapi_FileAPI.md §12–15 и tasks/09_TASK_WPT_HARDENING.md |
| CI | ещё не запускался; не заявлять CI PASS до предоставленного владельцем run URL/SHA |
| Результат | закрыть F1–F11 ниже, обновить trace/docs/handoff и остановиться для повторной приёмки |

## 1. Причина возврата

Code review реализации M7 обнаружил ошибки в границах strict gate, путях corpus,
модели статусов, обработке исключений async harness, ограничении timeout,
параллельном CLI-режиме, scrubber отчётов и lint contract.

Compile-only проверка cargo check --workspace --all-features на review tip
проходит. Это не заменяет исправление границ и не является доказательством
acceptance или CI.

Ниже приведены обязательные решения. Исполнитель не должен заново выбирать
архитектуру, ослаблять требования, переносить findings в QUESTIONS.md или
заменять их дополнительными тестами. Если точная реализация невозможна из-за
API Boa или Rust toolchain, остановись и зафиксируй конкретную техническую
причину с file/symbol и минимальным необходимым изменением scope.

## 2. Неизменяемые правила

1. Не менять ТЗ, M7-заказ, M1–M6 acceptance documents, существующую JS-visible
   семантику File API, trace IDs M1–M6 или thresholds.
2. Не удалять/ослаблять существующие assertions, не добавлять #[ignore],
   feature-gate для сокрытия failure и не менять expected result ради зелёного
   запуска.
3. Production-код остаётся safe Rust: deny unsafe_code; без production
   unwrap, expect, panic!, todo!, unimplemented! и blanket allow.
4. boa_fapi_core не изменяется для поддержки runner. Вся WPT-specific логика
   остаётся в boa_fapi_wpt, его corpus и тестовых целях.
5. Не выполнять сеть, shell-команды или произвольные внешние программы из
   WPT CLI. Pinned corpus читается только после hash/path validation.
6. В отчётах запрещены body, absolute paths, OS handles, capabilities,
   partition/nonce, полные blob URL и UUID. Ошибка должна либо пройти
   детерминированный scrubber, либо быть заменена стабильным общим detail.
7. После исправлений обновить только относящиеся к M7 документы и создать
   новый handoff/review evidence. Исторические M1–M6 документы не переписывать.

## 3. F1 — strict mode не должен обходиться через filter

### Проблема

В crates/boa_fapi_wpt/src/main.rs filter исключает файлы из files, после чего
report::strict_pass проверяет только оставшееся подмножество. Команда strict
с filter может быть зелёной, даже если исключённый файл имеет FAIL/TIMEOUT.

### Обязательное исправление

1. Оставить filter как диагностический режим.
2. В run до чтения corpus добавить проверку: strict вместе с filter запрещён и
   возвращает launch error с сообщением --filter cannot be combined with --strict.
3. Без strict filter запускает выбранные files и печатает diagnostic report.
4. Полный запуск без filter остаётся единственным CI gate.
5. Обновить docs/wpt.md: filter только non-strict diagnostic; strict + filter
   не является допустимой командой.
6. Добавить проверку argument contract: strict + filter -> launch error.

Не считать пропущенные filter-файлы capability gap и не превращать их в NOTRUN.

## 4. F2 — закрыть path traversal, absolute path и symlink escape

### Проблема

verify_hashes снимает префикс corpus/ и конкатенирует строки. ../,
absolute path, backslash path и symlink могут вывести чтение за пределы corpus
root. Fallback на crates/boa_fapi_wpt/corpus делает источник неоднозначным.

### Обязательная схема путей

Добавить в manifest обязательное поле:

    "corpus_root": "crates/boa_fapi_wpt/corpus"

Поле трактуется как path относительно directory manifest. Для текущего
root-level wpt-manifest.json это ровно
<repo>/crates/boa_fapi_wpt/corpus.

Правила:

1. corpus_root не может быть absolute, содержать backslash, пустой segment или
   .. .
2. file.path — логический path только вида corpus/<relative-file>.js; это не OS
   path.
3. Запретить leading slash/backslash, drive prefix C:, любой segment ..,
   NUL/control characters, path без corpus/ и расширение кроме .js.
4. Получать пути через std::path::Path, не через строковую конкатенацию.
5. manifest_path.parent() и corpus_root canonicalize до чтения.
6. Candidate canonicalize после join; candidate обязан быть строго внутри
   canonical corpus root.
7. Проверить каждый component через symlink_metadata. Любой symlink в corpus
   root или candidate отвергать launch error symlinks are not allowed in corpus.
8. Удалить fallback к фиксированному crate path и fallback к
   <manifest-dir>/<relative>. Использовать только manifest corpus_root.
9. Hash считать по исходным bytes после UTF-8 validation.
10. Duplicate path проверять после строгой логической нормализации; разные
    написания не должны ссылаться на один candidate.
11. Error detail для path должен печатать только manifest-relative file.path.

Добавить parser/loader errors и boundary checks для ../, ..\,
absolute Windows path, absolute Unix path, NUL, non-JS suffix, symlink и
candidate outside root.

## 5. F3 — отдельный фактический статус NOTRUN

### Проблема

ActualStatus содержит только Pass, Fail и Timeout. Ожидаемый NOTRUN записывается
как actual FAIL и принимается через detail prefix notrun:. JSON и JUnit
показывают failure для разрешённого capability gap.

### Обязательное исправление

1. Добавить ActualStatus::NotRun и token NOTRUN.
2. В run_file:
   - expected PASS + один passing record -> PASS;
   - expected NOTRUN + чистая evaluation -> NOTRUN с detail
     notrun: <manifest reason>;
   - top-level eval/job/readback error -> FAIL для строки, включая expected
     NOTRUN.
3. В strict_pass сравнивать enum-статусы:
   - PASS ожидает только actual PASS;
   - NOTRUN ожидает только actual NOTRUN;
   - FAIL/TIMEOUT всегда ломают strict;
   - detail prefix не является доказательством статуса.
4. JSON должен печатать actual NOTRUN.
5. JUnit должен печатать skipped для actual NOTRUN + expected NOTRUN, без
   failure и без увеличения failures.
6. Summary считать NOTRUN отдельно от PASS и FAIL.
7. Обновить docs/wpt.md status model и пример отчёта.
8. Добавить проверки для всех четырёх actual statuses и strict_pass.

## 6. F4 — ошибки async callbacks должны быть FAIL, не TIMEOUT

### Проблема

В harness.rs исключение из success callback promise_test попадает в новый
rejected Promise без terminal record(). Исключение из async_test.step() или
step_func() выходит в Boa job. Runner прекращает pump, но строка становится
TIMEOUT.

### Обязательная реализация prelude

Добавить JS helpers:

1. record_once(test, pass, message) со состоянием pending, passed, failed.
   Второй terminal completion всегда создаёт FAIL; PASS после FAIL запрещён.
2. safe_error(e) с try/catch и ограниченным scrub-safe текстом.
3. run_step(test, fn, thisArg, args), который ловит exception и записывает
   FAIL вместо выхода исключения в Boa job.
4. async_test:
   - t.step и t.step_func используют run_step;
   - done переводит pending в passed и запускает cleanup один раз;
   - done после terminal записывает duplicate FAIL;
   - cleanup exception переводит test в FAIL без второго PASS.
5. promise_test:
   - exception при создании Promise записывает FAIL;
   - fulfillment callback обёрнут try/catch; assertion exception записывает FAIL;
   - rejection callback записывает FAIL;
   - returned Promise от then имеет rejection handler.
6. test сохраняет synchronous catch semantics и не записывает PASS после
   cleanup failure.
7. record ограничивает длину name/message и безопасно преобразует значения.

В runner.rs:

1. run_jobs().is_err() превращать в FAIL, не просто break с последующим
   TIMEOUT.
2. При top-level eval error все rows файла становятся FAIL. Нельзя сохранять
   PASS rows, если тот же adapted file затем бросил top-level error.
3. Readback error не превращать в пустой list и TIMEOUT; вернуть Readback
   launch error либо однозначные FAIL rows.
4. Не менять file/subtest names из JS error detail.

## 7. F5 — действительно bounded timeout

### Проблема

file_timeout проверяется до context.run_jobs(), но сам вызов может долго или
бесконечно исполнять self-scheduling jobs. Wall guard не срабатывает.

### Обязательное решение

Использовать process isolation для одного CLI file run:

1. Основной CLI сохраняет orchestration, но каждый file execution в worker mode
   выполняется отдельным child-процессом того же boa_fapi_wpt executable.
2. Добавить внутренний режим --worker-file <validated-file-id>. Worker не
   принимает произвольный path и работает только с file ID из validated manifest.
3. Parent запускает child через std::process::Command::current_exe(), передаёт
   validated file identifier и timeout через фиксированный протокол.
4. Parent читает bounded stdout/stderr и вызывает Child::kill() при wall
   deadline. Kill всегда даёт TIMEOUT и non-zero worker detail.
5. Child создаёт Context только внутри себя; после завершения процесса live Boa
   jobs не остаются.
6. Ограничить stdout/stderr worker. Переполнение — launch failure.
7. Сохранить run_file() как library mapping function, но strict CLI использует
   isolated path.
8. Не принимать command string и не запускать shell.
9. Документировать: hard timeout гарантируется CLI process boundary, library
   run_file имеет bounded pump budget.

Проверить self-scheduling Promise, job exception, worker timeout, worker
non-zero exit, truncated worker output и переход к следующему file.

## 8. F6 — реализовать --threads N

### Проблема

Сейчас любое N > 1 заканчивается launch error. Это не explicit opt-in mode.

### Обязательное решение

1. N=1 сохраняет manifest order и текущий deterministic path.
2. Для N>1 создать min(N, files.len()) worker slots.
3. Каждый worker получает immutable validated file record и запускает isolated
   worker process из F5. Context, JsObject, FileApiHandle и mutable JS state
   между workers не передаются.
4. Parent хранит result вместе с manifest index.
5. Перед report serialization отсортировать results по manifest index.
6. Ошибка одного file не должна терять результаты остальных; strict должен fail.
7. threads=0 — launch error.
8. threads не меняет expected statuses, hash checks, report schema или strict
   semantics.
9. filter разрешён только в non-strict diagnostic mode.
10. Обновить docs/wpt.md и handoff: N=1 deterministic, N>1 explicit parallel
    opt-in с ordered output.

## 9. F7 — failures не маскировать

Закрепить mapping:

| Failure | Actual status | Strict |
|---|---|---|
| manifest/path/hash/parser error | launch error, exit 2 | fail |
| prelude registration/evaluation error | launch error | fail |
| top-level adapted JS throw | FAIL for every row in file | fail |
| run_jobs JS/job error | FAIL for affected/unsettled rows; no TIMEOUT substitution | fail |
| child wall deadline | TIMEOUT | fail; текущий manifest не ожидает TIMEOUT |
| readback/protocol corruption | launch error или documented FAIL, consistently | fail |
| expected capability gap with clean file | NOTRUN | pass only with exact expected NOTRUN |

Ни один error path не должен становиться пустым result vector, expected PASS или
silent NOTRUN.

## 10. F8 — manifest completeness и schema hardening

В load_manifest добавить:

1. required fields: schema_version, source, corpus_root, default_timeout_ms,
   files;
2. source repository/license non-empty; repository — HTTPS WPT URL;
3. source commit — ровно 40 lowercase hex;
4. file path rules из F2;
5. upstream_path non-empty, no wildcard, starts with FileAPI/;
6. group — ровно одна из шести обязательных групп:
   FileAPI/blob, FileAPI/file, FileAPI/filelist-section,
   FileAPI/reading-data-section, FileAPI/FileReader, FileAPI/BlobURL;
7. после загрузки files проверить наличие всех шести groups;
8. каждый subtest содержит test, subtest, status, reason, capability, owner,
   review_by, trace; reason пуст только для PASS;
9. review_by — реальная календарная дата, включая month/day validation;
10. duplicate JSON object keys отвергать, не оставлять последнее значение;
11. duplicate logical path и duplicate test+subtest отвергать;
12. ввести лимиты длины paths/names/reason/owner/trace и corpus bytes;
13. timeout_ms и --timeout-ms ограничить диапазоном 1..=300000;
14. schema errors не должны печатать absolute paths.

## 11. F9 — отчёты не должны раскрывать данные

Scrubber должен:

1. заменять любое вхождение blob: независимо от prefix и punctuation до
   ближайшего whitespace/quote;
2. заменять file://, HTTP(S) URL с path/query, Windows drive path, UNC path и
   absolute Unix path фиксированными placeholders;
3. удалять control characters кроме tab/newline/carriage return;
4. ограничивать 480 Unicode scalar values и 48 tokens без slicing середины
   UTF-8;
5. возвращать <redacted-error>, если классификация небезопасна.

JSON должен оставаться валидным и deterministic. XML escape должен фильтровать
XML 1.0 invalid chars; NOTRUN — skipped, FAIL/TIMEOUT — failure. File paths в
report только manifest-relative. Добавить serializer checks для quote, &, NUL,
Unicode, blob: внутри token и Windows/Unix paths.

## 12. F10 — убрать blanket clippy allow

В crates/boa_fapi_wpt/src/lib.rs удалить crate-wide allow clippy::expect_used.

Production targets должны проходить workspace lint без allow. Для cfg(test)
unit modules предпочесть явные assertions/match; если allow остаётся, он
должен быть только на конкретном test module/function и не распространяться
на production или binary main.

## 13. F11 — CI и nightly wiring

В .github/workflows/ci.yml:

1. job name заменить с M6 validation на M7 validation;
2. комментарий Work order M6 заменить на M7;
3. сохранить порядок M1–M6, затем M7;
4. WPT strict run выполнять полным manifest без filter;
5. artifacts upload оставить после успешного report generation;
6. добавить JSON report check на actual PASS/NOTRUN.

В .github/workflows/nightly.yml:

1. не просто устанавливать Miri;
2. на Ubuntu установить nightly + Miri и запустить bounded
   cargo +nightly miri test --package boa_fapi_core --lib;
3. добавить bounded hardening/fuzz command с fixed seed/input cap и
   timeout-minutes;
4. Windows запускать только совместимые hardening hooks;
5. unavailable target — явный SKIP с причиной, не PASS.

Сеть и toolchain setup остаются workflow responsibility; Rust harness сеть не
использует.

## 14. Документы и traceability

Обновить:

- docs/spec-matrix.md: rows M7-REWORK-F1 … M7-REWORK-F11 с exact symbols;
- docs/wpt.md: filter, corpus_root/path policy, NOTRUN, process timeout,
  N>1 ordering и report scrubber;
- docs/m7-validation.md: M7 validation rework required, CI not run,
  compile-only check отдельно;
- docs/reviews/M7-handoff.md: status менять только после исправлений;
- docs/reviews/M7-rework.md: этот заказ и resolution checklist;
- QUESTIONS.md: не добавлять архитектурные варианты; только внешний blocker,
  если API/toolchain реально не позволяет выполнить F5/F6.

## 15. Проверка перед повторной передачей

Выполнить в порядке:

    cargo fmt --all -- --check
    cargo check --workspace --all-features
    cargo clippy --workspace --all-targets --all-features -- -D warnings
    cargo test --package boa_fapi_wpt -- --nocapture
    cargo test --package boa_fapi --test appendix_a_acceptance -- --nocapture
    cargo test --package boa_fapi --test abort_races -- --nocapture
    cargo test --package boa_fapi --test abort_races_fs -- --nocapture
    cargo test --package boa_fapi --test hardening_hooks -- --nocapture
    cargo run --package boa_fapi_wpt -- --manifest wpt-manifest.json --strict
    cargo run --package boa_fapi_wpt -- --manifest wpt-manifest.json --strict --threads 2
    cargo run --package boa_fapi_wpt -- --manifest wpt-manifest.json --filter corpus/blob
    cargo run --package boa_fapi_wpt -- --manifest wpt-manifest.json --strict --filter corpus/blob
    cargo doc --workspace --no-deps
    git diff --check

Ожидаемые результаты:

- full strict — exit 0 только при всех PASS/ожидаемых NOTRUN;
- threads 2 — exit 0, output manifest-order identical to N=1;
- non-strict filter — exit 0 с subset report;
- strict + filter — launch error, exit 2;
- traversal/symlink fixtures — launch error, exit 2;
- async assertion throw — FAIL, не TIMEOUT;
- self-scheduling worker — TIMEOUT, parent продолжает следующий file;
- expected NOTRUN — JSON/JUnit actual NOTRUN и JUnit skipped.

CI не запускать от имени исполнителя и не заполнять handoff CI ссылкой.
В handoff указать exact local outputs, commit SHA, unresolved deviations и
CI: awaiting owner verification.

## 16. Критерии закрытия rework

Rework закрывается только если:

1. F1–F11 реализованы exactly as specified;
2. нет crate-wide clippy allow;
3. manifest не может выйти за corpus root и не может пропустить mandatory group;
4. strict/filter нельзя использовать для обхода coverage;
5. NOTRUN — отдельный фактический статус во всех report formats;
6. async callback errors и job errors не маскируются TIMEOUT;
7. hard timeout завершает isolated worker и не оставляет job/context;
8. threads N реально работает для N>1 и сохраняет report order;
9. sanitizer закрывает blob URL, paths, control chars и XML validity;
10. boundary commands дают ожидаемые результаты;
11. docs/spec matrix/handoff обновлены, CI остаётся awaiting owner verification;
12. изменения M1–M6 отсутствуют без отдельного обоснования.

После выполнения создать новый commit на той же M7-ветке, обновить
docs/reviews/M7-handoff.md и остановиться для повторной code acceptance.

