# M9F handoff — WPT defect remediation (5 M9-E defects)

| Поле | Значение |
|---|---|
| Заказы | M9-R1 defect orders: File.name verbatim; sync `abort()`; runner loadstart task boundary; `readAsDataURL` empty type |
| База | `task/m9e-gate-accounting-reporting` (`b6bf0c0`) |
| Ветка | `task/m9f-defect-filename` (стек от M9E-R1) |
| Upstream | `web-platform-tests/wpt` commit `0968c868d8095217d18d86b34c7f21dccae58768` |
| Нормативная база | File API WD (Editor's Draft 2026-09-12) §4.1, §6.2.3.5, §6.3; pinned WPT |

Закрывает все пять открытых дефектов M9-E, перечисленных в
`docs/reviews/M9E-handoff.md` §3 / `tasks/22…md` §8. После этой ветки
`release_green == true`, `--strict` exit 0, CI release job зелёный.

Разделы 1–6 ниже сохраняют исторический handoff и CI evidence для commit
`9975de5`; они не подтверждают текущий локальный audit follow-up. Его
отдельный статус и ожидаемая валидация указаны в §7.

## 1. Четыре изменения

1. **`File.name` verbatim** (WD §4.1 step 4.4). `normalize_file_name` больше
   не заменяет `/` на `:`; JS-конструктор и host-импорт хранят имя
   verbatim. Обновлены M2/M5/M6/appendix-A ожидания. Дефект:
   `File-constructor.any.js :: No replacement when using special character
   in fileName`.
2. **Синхронный `FileReader.abort()`** (WD §6.2.3.5 steps 5–6). `abort()`
   вызывает новый `dispatch_terminal_now`: `abort` и условный `loadend`
   диспатчатся на вызывающем стеке до возврата; `TerminalKind::Abort` и
   постановка abort-терминала через `run_jobs` удалены. Обновлены
   M4-pure-model (`bounded_operation_sequences_match_pure_model`),
   `m9_filereader_io::stale_completion_after_restart_is_noop`,
   `filereader::tests::loadstart_abort_then_restart_emits_only_new_operation`
   и `m8_observability`. Дефект: `fileReader.any.js :: FileReader States --
   abort`.
3. **Task/microtask boundary после `loadstart`**. `run_pump` после первого
   `loadstart` переоткладывает уже дренированный chunk в следующий
   `JobStep::PumpChunk` и возвращается, поэтому promise-реакции,
   поставленные во время `loadstart`, выполняются до применения чанка.
   Продолжение в `filereader_abort.any.js` видит `LOADING`, а не `DONE`;
   порядок событий и ровно одна пара `abort`+`loadend` на `abort()`
   сохранены. Дефект: `filereader_abort.any.js :: Aborting after read`.
4. **`readAsDataURL` пустого типа → `application/octet-stream`**
   (`package_data_url`/`data_url_len`), как в pinned
   `filereader_readAsDataURL.any.js` (две строки); M4 async/sync
   packaging-тесты обновлены. Change-control: WD §6.3 текст неоднозначен
   (issue #104), WPT — конформанс-цель гейта.

Все четыре заказа оформлены отдельными ADR: `docs/DECISIONS.md`
ADR-0048…0051. Пять строк дефектов в `tools/upstream-titles.json`
переведены из `FAIL` в `PASS`; `expectations.json`/`wpt-manifest.json`
перегенерированы `python tools/gen-m9e-gate.py`.

## 2. Totals

```text
--check-expectations: exit 0, expectations_match=true, release_green=true, exit_reason=ok
--strict:             exit 0, expectations_match=true, release_green=true, exit_reason=ok
inventory 115/115 (36 direct, 0 adapted, 79 excluded, 0 unaccounted)
results   496/496 (379 upstream pass, 6 smoke pass, 0 defects, 111 notrun, 0 unexpected)
ADAPTED_SMOKE: 379 upstream passed, 6 smoke passed, 0 defects, 32 exclusions, 0 unexpected
```

JUnit aggregate: `<testsuites name="WPT_STRICT" tests="496" failures="0"
skipped="111">`.

Reports (SHA-256):

| Report | SHA-256 |
|---|---|
| `target/wpt-observation.json` | `a25e94f1b1e6004d2139b99e57c596a6be8f2069555164c9fd23882909e382d0` |
| `target/wpt-release.json` | `106da98a751c4628a57869bb5e6a0ffeb9835bbc3ef85cef0a268294e217bece` |
| `target/wpt-release.xml` | `d21b588721d41ce8b07ab2df7f0e7c17f9d4152803dcd67c99c85c554f246dad` |

## 3. Приёмка

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features -- --test-threads=1   # green
cargo run --package boa_fapi_wpt -- --manifest wpt-manifest.json --smoke
cargo run --package boa_fapi_wpt -- --manifest wpt-manifest.json --expectations expectations.json --upstream-root target/pinned-wpt --check-expectations
cargo run --package boa_fapi_wpt -- --manifest wpt-manifest.json --expectations expectations.json --upstream-root target/pinned-wpt --strict
```

Все команды локально зелёные: `--strict` exit 0, `release_green == true`,
`0` defects; `cargo test --workspace --all-features -- --test-threads=1`
проходит без падений; fmt/clippy чисты.

Внешний CI полностью зелёный: run
`https://github.com/DavidGoliaf/FileAPI_boa/actions/runs/34772448617`
(commit `9975de5`) — `M8 validation (ubuntu-latest|windows-latest|
macos-14)` success, `M9E gate negative controls (ubuntu-latest|
windows-latest)` success, `M9E release conformance (strict gate)`
**success** (`--strict` exit 0, `release_green: true`).

## 4. Затронутые suites (обновлены под новое нормативное поведение)

- M2: `file_name_conversions`, `host_file_from_bytes`;
- M5: `file_from_resource_metadata_and_text`,
  `display_name_is_the_only_visible_name`;
- M6: `m6_structured_clone` (имя `a/b.txt`);
- appendix-A: `blob_creation_brands_prototypes_descriptors`;
- M4 async/sync: data-URL packaging, pure-model abort;
- M9-C (`m9_filereader_io`): stale-completion log теперь включает
  синхронную пару `abort`+`loadend`;
- M8 observability: порядок `loadstart|abort|loadend|...`;
- lib: `filereader::tests::loadstart_abort_then_restart_emits_only_new_operation`.

## 5. Отклонения

Нет. Все пять дефектов закрыты следованием pinned WPT; для двух
(`readAsDataURL`, `File.name`) поведение приведено к актуальному WD/WPT,
для двух продуктовых — синхронный abort и File.name, для одного —
раннерная task/microtask-граница.

## 6. Что дальше

- M9-F release/delivery closure (`tasks/17_TASK_M9F_RELEASE_DELIVERY.md`).
- Общая release-приёмка M9-E разрешена (`release_green == true`).

## 7. Текущий локальный audit follow-up — local validation green, CI pending

Пользователь принял remediation всех findings аудита и change-control
ADR-0048–0051. Это отдельный локальный follow-up, не повторное утверждение
исторических totals, report hashes или CI из §2–3.

Документационная часть: исправлены обещания slash replacement в TZ
(требования и приложение A), spec matrix, architecture и host integration;
clone metadata описывает verbatim name. Уточнены синхронный abort с
условным loadend, Data URL fallback и его влияние на квоту (+24 байта,
37-байтный prefix). CHANGELOG содержит unreleased compatibility changes;
spec-delta и ADR-0052 фиксируют границы принятого change-control.

Точный scope родительского исправления: очистка таблиц FileReader при
shutdown (включая roots и deferred state на границе `loadstart`), shutdown
guard после `abort` handler до `loadend`/enqueue listener error, проверка
полной длины в `package_data_url` до allocation base64 payload. Host byte
creation/context validation и изменения telemetry не входят в follow-up.
В этой документационной подзадаче Rust source и тесты не редактируются.

Регрессии, добавленные родителем (сообщённое evidence, без независимого
запуска в этой подзадаче):

- `M9C-FR-03`: `shutdown_drops_all_reader_state` — родитель сообщил RED
  на удержанных roots, затем green после исправления;
  `shutdown_in_abort_handler_suppresses_loadend_and_listener_error` —
  RED с `abort|loadend|returned`, затем исправление;
  `shutdown_at_loadstart_boundary_drops_deferred_reader_state` — проверка
  очистки deferred state.
- `M4-FR-05` / `M4B-FRS-05` (data URL и общий sync/async packaging):
  `package::tests::data_url_exact_limit_and_one_below_cover_base64_boundaries`.
  Старый prefix — 13 байт, новый — 37; +24 байта учитываются в квоте.

Повторное ревью расширило `M9C-FR-03` двумя проверками:
`shutdown_in_progress_handler_keeps_state_cleared` и
`shutdown_in_final_progress_preserves_loading_and_null_result`.
Промежуточный progress не восстанавливает очищенный PumpState. После
финального progress проверяется shutdown до packaging и публикации
DONE/result. Финальный тест перебирает четыре readAs-метода и пустой/
непустой Blob; до исправления получил `2:string:null` вместо `1:null:null`,
после исправления все восемь вариантов проходят, поздние poll/jobs не
меняют состояние и не добавляют событий. Причина пропуска: прежние
shutdown-тесты проверяли события/bridge, но не итоговые readyState/result.
В тесте промежуточного progress assert перенесён после run_jobs: первый
pump выполняет только loadstart и откладывает chunk.

Локальная валидация текущего рабочего дерева поверх `b1f1d7b` (Windows):

```powershell
cargo test -p boa_fapi --all-features --lib shutdown_in_final_progress_preserves_loading_and_null_result
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features -- --test-threads=1
cargo run --package boa_fapi_wpt -- --manifest wpt-manifest.json --expectations expectations.json --upstream-root target/pinned-wpt --strict
```

Все команды exit 0; strict: `expectations_match=true`, `release_green=true`,
379 upstream PASS, 6 smoke PASS, 0 defects, 111 NOTRUN, 0 unexpected.
Внешний CI текущего diff, coverage/deny/package/fresh-clone gates не
перезапускались; полная release/delivery приёмка не заявляется.
Старый CI commit `9975de5` не доказывает исправление новых findings.
