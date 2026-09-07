# M7 — заказ на доработку после повторной code acceptance review

| Поле | Значение |
|---|---|
| ID | `M7-REWORK-2` |
| Статус | `REWORK REQUIRED` |
| Основание | повторная проверка commit `aed9a0f` |
| Предыдущий review | `docs/reviews/M7-rework.md` |
| Ветка | продолжать текущую `task/m7`; не переносить изменения в `main` |
| Нормативные документы | `TZ_boa_fapi_FileAPI.md` §12–15, `tasks/09_TASK_WPT_HARDENING.md`, `docs/reviews/M7-rework.md` |
| CI | внешний CI ещё не подтверждён; URL/run SHA не придумывать |
| Цель | закрыть F12–F17, обновить evidence и остановиться для повторной приёмки |

## 1. Причина возврата

Штатный corpus M7 проходит локальные compile, clippy, workspace tests, strict
WPT, `--threads 2`, filter и rustdoc. Повторная проверка исходников выявила
ошибки в нештатных сценариях: зависший Boa job, неверное отображение worker
ошибок, неоднозначный JSON manifest, утечку embedded URL/path и маскирование
ошибок Miri.

Исполнитель обязан исправить код и добавить regression tests. Нельзя закрывать
findings только изменением handoff, переносом проблемы в `QUESTIONS.md`,
добавлением `#[ignore]`, изменением expected статусов или ослаблением strict
gate.

## 2. Неизменяемые правила

1. Не менять File API semantics M1–M6, существующие trace IDs, лимиты и
   accepted M6 decisions.
2. Не удалять assertions и не менять PASS/NOTRUN expectations ради зелёного
   запуска.
3. Production Rust остаётся safe: `#![deny(unsafe_code)]`, без production
   `unwrap`, `expect`, `panic!`, `todo!`, `unimplemented!` и blanket clippy
   allow.
4. WPT-specific логика остаётся в `boa_fapi_wpt`; `boa_fapi_core` не изменять.
5. Worker запускается только через `std::process::Command::current_exe()`;
   shell, command string, сеть и произвольные внешние программы запрещены.
6. В отчёты не попадают body, абсолютные пути, OS handles, partition/nonce,
   полные blob URL, UUID и секретные детали ошибок.
7. Для каждого нового поведения добавить тест самого инварианта, а не только
   тест штатного corpus.
8. Исторические M1–M6 документы не переписывать. Обновлять только M7 docs,
   traceability, validation и handoff.

## 3. F12 — hard timeout для обычного strict CLI

### Дефект

В `crates/boa_fapi_wpt/src/main.rs` ветка `slots <= 1` вызывает `run_file()`
непосредственно. В `run_file()` wall check выполняется до
`context.run_jobs()`, но не может прервать бесконечный/self-scheduling вызов
самого `run_jobs()`.

Следовательно команда по умолчанию:

    cargo run --package boa_fapi_wpt -- --manifest wpt-manifest.json --strict

может зависнуть навсегда. Сейчас hard kill существует только для
`--threads > 1`, поэтому timeout semantics зависят от threads.

### Обязательная реализация

1. Каждый file execution из CLI, включая default `--threads 1`, выполнять через
   `run_file_isolated()` и worker protocol F5.
2. `--threads 1` сохраняет manifest order и последовательность запуска, но не
   означает in-process execution.
3. `run_file()` оставить library mapping function для unit tests и library
   consumers; его pump guard не считать hard wall timeout.
4. Parent запускает child через `current_exe()`, передаёт только validated
   manifest path, file index и numeric timeout.
5. При превышении deadline parent вызывает `Child::kill()` и `wait()`, создаёт
   `TIMEOUT` rows только для этого file и продолжает следующий file.
6. `--threads N` и `--threads 1` используют один `run_file_isolated()` и один
   mapping worker result → `FileResult`; различается только число concurrent
   children.
7. Не добавлять Rust thread cancellation как альтернативу process boundary.

### Обязательные проверки

Добавить fixture с self-scheduling Promise/job без terminal harness result.
При малом `--timeout-ms` он обязан:

- завершаться за bounded time;
- давать `TIMEOUT` только своим subtests;
- не мешать следующему file получить PASS;
- давать одинаковый report при `--threads 1` и `--threads 2`.

## 4. F13 — worker errors не должны маскироваться под TIMEOUT

### Дефект

Worker возвращает `WORKER-FAIL` для любого `RunError`, включая register,
prelude и readback errors. Parent в `finish_worker()` любую non-success ситуацию
преобразует в synthetic `TIMEOUT`. В результате один дефект имеет разную
семантику при `--threads 1` и `--threads 2`.

### Обязательный protocol и mapping

Сохранить line protocol, но сделать его типизированным:

    WORKER-OK <json-file-result>
    WORKER-TIMEOUT <stable-detail>
    WORKER-ERROR <error-code>

Разрешённые `error-code`: `register`, `prelude`, `readback`, `file-eval`,
`protocol`.

| Событие | Результат parent |
|---|---|
| child убит по wall deadline | все rows file → `TIMEOUT`, следующий file запускается |
| stdout overflow | `TIMEOUT` с `worker output overflow` |
| worker crash/non-zero без известного token | `TIMEOUT` с `worker non-zero exit` |
| protocol corruption | `TIMEOUT` с `worker protocol corruption` |
| `WORKER-ERROR register/prelude/readback` | launch error всего CLI, exit 2 |
| `WORKER-ERROR file-eval` | FAIL rows по F7 либо launch error, если rows построить нельзя |
| `WORKER-OK` | принять только полностью проверенный `FileResult` |

Нельзя использовать exit code как единственный тип ошибки. Parent обязан
проверять:

1. ровно одну protocol line и отсутствие непустого stdout до/после неё;
2. UTF-8 и размер stdout/stderr до parse;
3. точное совпадение path, subtest count, test/subtest IDs и expected statuses
   с parent manifest;
4. отсутствие неизвестного actual status;
5. scrubbed detail без absolute path/secrets.

Добавить tests для каждого `WORKER-ERROR` и отдельно для wall kill,
non-zero exit и protocol corruption.

## 5. F14 — отклонять duplicate JSON object keys

### Дефект

`JsonParser::object()` заменяет предыдущее значение последним. Поэтому
неоднозначный manifest принимается, хотя validation заявляет duplicate-key
rejection.

Пример, который обязан быть отклонён:

    {"sha256":"approved-value", "sha256":"other-value"}

### Обязательная реализация

1. В `JsonParser::object()` перед вставкой проверять наличие key в текущем
   object.
2. При повторе немедленно возвращать `ManifestError::DuplicateJsonKey(String)`
   или эквивалентный отдельный error variant.
3. Не использовать first-wins или last-wins semantics.
4. Проверка должна быть рекурсивной для root, source, files и subtests.
5. В ошибке разрешено раскрывать только имя key, но не value и не абсолютный
   путь.
6. Обновить schema documentation: duplicate keys — load error до hash/path
   verification.

Tests: duplicate `schema_version`, `source.repository`, file `path`, subtest
`status`, duplicate keys с разными value types и корректный nested object.

## 6. F15 — закрыть embedded URL/path leakage

### Дефект

`runner::scrub_token()` редактирует file/HTTP/drive/UNC/Unix path только если
префикс находится в начале whitespace-token. Не защищены, например:

    url=https://host/private/path?token=secret
    path=/home/user/private/file.js
    (file:///C:/private/file.js)
    error: https://host/private
    C:\private\secret.js,

### Обязательная реализация

1. `blob:` искать в любом месте token и редактировать до whitespace/quote,
   сохранив punctuation-preserving поведение.
2. `file://`, `http://`, `https://` искать в любом месте token, включая формы
   после `=`, `:`, `(`, `[`, `{`.
3. Для HTTP(S) сохранять только scheme и безопасный host; path/query/fragment
   заменять на `<redacted-path>`.
4. Для file/drive/UNC/absolute Unix path использовать fixed placeholder.
5. Сохранить лимиты 48 tokens и 480 Unicode scalars, обрезая по границе
   Unicode scalar.
6. Сохранить XML 1.0 control filtering.
7. После scrubber detail не должен содержать полный blob URL, file URL, URL
   path/query/fragment, absolute path, UUID, nonce или partition value.

Обязательные tests: `url=https://...`, `(https://...)`, `path=/tmp/x`,
`(file:///tmp/x)`, drive path с запятой, UNC path в скобках, Unicode на границе
480 scalars, controls и JSON/XML round-trip.

## 7. F16 — exact repository identity

### Дефект

Проверка `source.repository` использует prefix match, поэтому
`https://github.com/web-platform-tests/wpt-malicious` принимается как WPT.

### Обязательная реализация

1. Принимать только canonical URL:

       https://github.com/web-platform-tests/wpt

2. Запретить query, fragment, дополнительный path, userinfo и `http://`.
3. Если trailing slash допускается schema, нормализовать его к одному
   canonical form и хранить только canonical form.
4. Добавить tests на `wpt-malicious`, `wpt/other`, query, fragment, HTTP и
   canonical valid URL.

## 8. F17 — collision по subtest name

### Дефект

Manifest uniqueness проверяется по `(test, subtest)`, но runner хранит harness
results только по `subtest`. Два разных test ID с одним subtest name внутри
одного file могут получить один результат и ложный strict PASS.

### Обязательная реализация

В этом rework использовать вариант A: запретить одинаковые `subtest` names
внутри одного manifest file независимо от `test`. В loader добавить отдельный
`seen_file_subtests` и вернуть manifest load error при collision.

Не менять harness protocol на composite key: это расширение формата и не нужно
для закрытия текущего M7 scope.

Tests: collision внутри file → error; одинаковый subtest в разных files →
разрешён; разные subtests → разрешены; штатный manifest загружается.

## 9. CI и nightly

### Miri

В `.github/workflows/nightly.yml` нельзя оборачивать установку toolchain и
сам Miri одной конструкцией `|| echo`.

1. Отдельно определить, что компонент Miri недоступен, и только тогда вывести
   explicit `SKIP (no-miri-toolchain)`.
2. Если `cargo +nightly miri test --package boa_fapi_core --lib` стартовал и
   завершился non-zero, workflow обязан завершиться failure.
3. Windows остаётся compatible-only SKIP, но Windows hardening hooks выполняются.

### CI boundary coverage

В `.github/workflows/ci.yml` добавить проверки:

- self-scheduling fixture с малым timeout;
- bounded completion для `--threads 1` и `--threads 2`;
- SHA-256 equality JSON t1/t2;
- worker error не становится PASS/NOTRUN;
- exact `strict_pass` и actual status tokens.

Не использовать `continue-on-error` для этих проверок.

## 10. Документы и traceability

Обновить только M7-документы:

1. `docs/spec-matrix.md`: rows `M7-REWORK-2-F12` … `F17` с symbols и tests.
2. `docs/wpt.md`: оба CLI режима используют isolated workers; `run_file()` —
   library-only mapping; описать protocol, error mapping, duplicate keys и
   embedded scrubber.
3. `docs/m7-validation.md`: фактические boundary commands и результаты.
4. `docs/reviews/M7-handoff.md`: оставить `REWORK REQUIRED` до повторной
   приёмки; CI URL/SHA добавляется только после реального запуска владельцем.
5. Этот документ: добавить resolution checklist и commit SHA после исправлений.
6. `QUESTIONS.md` не менять, если текущие Rust/Boa API позволяют реализацию.

## 11. Обязательная локальная проверка

    cargo fmt --all -- --check
    cargo check --workspace --all-features
    cargo clippy --workspace --all-targets --all-features -- -D warnings
    cargo test --workspace --all-features
    cargo test --package boa_fapi_wpt -- --nocapture
    cargo run --package boa_fapi_wpt -- --manifest wpt-manifest.json --strict
    cargo run --package boa_fapi_wpt -- --manifest wpt-manifest.json --strict --threads 1 --json target/wpt-t1.json
    cargo run --package boa_fapi_wpt -- --manifest wpt-manifest.json --strict --threads 2 --json target/wpt-t2.json
    cargo run --package boa_fapi_wpt -- --manifest wpt-manifest.json --filter corpus/blob
    cargo run --package boa_fapi_wpt -- --manifest wpt-manifest.json --strict --filter corpus/blob
    cargo doc --workspace --no-deps
    git diff --check

Ожидаемые результаты:

| Сценарий | Ожидание |
|---|---|
| full strict | exit 0, 38 PASS, 0 unexpected |
| strict threads 1 | exit 0 через isolated child |
| strict threads 2 | exit 0, JSON byte-identical к threads 1 |
| non-strict filter | exit 0, только выбранные files |
| strict + filter | exit 2 до выполнения corpus |
| self-scheduling fixture | bounded completion, `TIMEOUT`, следующий file выполняется |
| worker `ERROR prelude` | exit 2, не TIMEOUT и не NOTRUN |
| worker kill | TIMEOUT, strict failure, следующий file выполняется |
| duplicate JSON key | manifest load error, exit 2 |
| embedded path/URL | report не содержит исходный path/secret |
| malformed repository URL | manifest load error, exit 2 |
| duplicate subtest name | manifest load error, exit 2 |

Дополнительно проверить, что M1–M6 source files не изменялись и их suites
остаются зелёными. Unix-only behavior подтверждается только Ubuntu CI.

## 12. Resolution checklist перед handoff

- [ ] F12: `--threads 1` также использует isolated worker и hard kill.
- [ ] F13: worker error types не маскируются под timeout.
- [ ] F14: duplicate JSON object keys рекурсивно отклоняются.
- [ ] F15: embedded URL/path forms scrubbed и покрыты tests.
- [ ] F16: repository identity проверяется exact match.
- [ ] F17: subtest collision внутри file невозможен.
- [ ] Miri test failure не маскируется `|| echo`.
- [ ] CI проверяет timeout, worker mapping и t1/t2 byte identity.
- [ ] production code без unsafe/unwrap/expect/panic/blanket allow.
- [ ] все команды раздела 11 выполнены и записаны фактически.
- [ ] M7 docs и handoff синхронизированы.
- [ ] handoff содержит новый commit SHA, но не выдуманный CI URL.
- [ ] после handoff следующий work order не начинается до acceptance.

