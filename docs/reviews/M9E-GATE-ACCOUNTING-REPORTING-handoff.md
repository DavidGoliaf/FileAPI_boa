# M9E-R1 handoff — release gate, inventory accounting и отчётность

| Поле | Значение |
|---|---|
| Заказ | `M9E-GATE-ACCOUNTING-REPORTING-REMEDIATION` (`tasks/22_…md`) |
| База | принятый head `task/m9d-gc-drop-lifecycle` (`37b6918`); фактическая ветка ответвлена от `9a8cd63` (`task/m9d-gc-drop-ring-fix`), который является тем же M9-D head плюс принятый ring-fix rework и CI case-sensitivity/coverage fixes |
| Ветка | `task/m9e-gate-accounting-reporting` |
| Upstream | `web-platform-tests/wpt` commit `0968c868d8095217d18d86b34c7f21dccae58768` |
| Нормативная база | `tasks/16_TASK_M9E_WPT_CONFORMANCE_GATE.md` §2–5, §8–9; `tasks/22…md` §1–§10 |

Замечание о базе: все WPT-артефакты (`crates/boa_fapi_wpt/**`, `wpt-manifest.json`,
`wpt-inventory.json`, `expectations.json`) побайтово совпадают с
`task/m9d-gc-drop-lifecycle`; отличие — только в CI/streams, уже принятых в
ring-fix. Ответвление сделано от текущего рабочего head, чтобы не откатывать
принятый M9-D rework. Это единственное отклонение в handoff.

## 1. Что построено

1. **Canonical run model** (`crates/boa_fapi_wpt/src/accounting.rs`):
   один validated `CanonicalRun` — единственный источник для console, JSON
   (schema **2**), JUnit и exit code. Никакой serializer не пересчитывает
   totals независимо. `strict_pass` удалён.
2. **Два недвусмысленных вердикта**: `expectations_match` (identity+status
   воспроизвели audited expectations, без missing/duplicate/extra/drift) и
   `release_green` (`expectations_match && defects == 0 && timeouts == 0`).
   `release_green == false` всегда, когда `defects > 0`.
3. **Inventory disposition bijection** (`inventory.rs`): каждый из 115
   pinned `FileAPI/**` путей получает ровно одну primary disposition
   (`executed-direct`/`executed-adapted`/`excluded-capability`/
   `unsupported-artifact`). Executed claims — из manifest provenance;
   исключения — из точных file-level exclusion rows. Missing/extra/
   duplicate/contradictory claim — launch error. Pure `project-acceptance`
   adapted smoke не претендует на upstream path, поэтому FileList host
   smoke остаётся отдельным `smoke_pass`, а `filelist.html` —
   `excluded-capability`.
4. **Exact expectation accounting**: 496/496 строк представлены ровно
   один раз, ordinal/case-sensitive по `(upstream_path, test, subtest)`;
   duplicate/missing/extra — `expectation_drift` и non-zero до вердикта.
   `tracker_subtests` больше не добавляет `DYNAMIC:` id, уже
   присутствующий в manifest/результате (было 420 строк при 417 уникальных).
5. **79 file-level exclusions** — top-level `exclusions[]` (path, test,
   capability, reason, owner, issue, review_by, trace), входят в totals и
   в synthetic JUnit suite `file-level-exclusions`, не прикрепляются к
   несвязанному manifest-файлу.
6. **CLI**: `--strict` (release gate), `--check-expectations`
   (диагностическая observation; всегда `release_green: false`),
   `--smoke` (`ADAPTED_SMOKE`). Exit reason классы различимы в JSON:
   `integrity`, `expectation_drift`, `execution_failure`,
   `release_defects`. Launch-ошибки пишут failure-JSON при `--json`.
7. **CI contract** (`.github/workflows/ci.yml`): required
   `m9e-release-conformance` job запускает `--strict` без inversion/
   `continue-on-error` (красный, пока 5 дефектов не исправлены; станет
   зелёным без изменения workflow); `m3-validation` запускает
   `--check-expectations` и проверяет `expectations_match == true`,
   `release_green == false`, `inventory.total == 115`,
   `unaccounted == 0`, `results.total == unique`, threads 1/2
   byte-identical; отдельный `m9e-gate-negative-controls` job прогоняет
   mutation fixtures.
8. **Тесты**: `accounting::tests::r1_01…06`, `inventory::tests::*`,
   `report::tests::*`, CLI parser tests, `cargo test --package boa_fapi_wpt
   --test gate_remediation` (`r1_01…r1_07`, self-contained fixtures:
   mutated upstream/inventory/expectation/result).

## 2. Totals (факт прогона на pinned `target/pinned-wpt`)

```text
WPT_CHECK_EXPECTATIONS: expectations_match=true release_green=false exit_reason=release_defects
  inventory=115/115 (36 direct, 0 adapted, 79 excluded, 0 unaccounted)
  results=496/496 (374 upstream pass, 6 smoke pass, 5 defects, 111 notrun, 0 unexpected)
WPT_STRICT:              expectations_match=true release_green=false exit_reason=release_defects
  inventory=115/115 (36 direct, 0 adapted, 79 excluded, 0 unaccounted)
  results=496/496 (374 upstream pass, 6 smoke pass, 5 defects, 111 notrun, 0 unexpected)
ADAPTED_SMOKE:           results=417/417 (374 upstream pass, 6 smoke pass, 5 defects, 32 notrun, 0 unexpected)
```

JUnit aggregate: `<testsuites name="WPT_STRICT" tests="496" failures="5"
skipped="111">` (JSON/JUnit/console totals совпадают).

Release blockers (не изменялись, §8 заказа): 5 expected FAIL/actual FAIL:
`File-constructor.any.js :: No replacement when using special character in
fileName`; `fileReader.any.js :: FileReader States -- abort`;
`filereader_abort.any.js :: Aborting after read`;
`filereader_readAsDataURL.any.js :: … unspecified MIME type`;
`filereader_readAsDataURL.any.js :: … empty Blob`.

## 3. Приёмка (§10) — локальный прогон

```powershell
cargo fmt --all -- --check                                   # 0
cargo clippy --workspace --all-targets --all-features -- -D warnings  # clean
cargo test --package boa_fapi_wpt --all-features             # green (30 lib + 7 bin + 7 gate_remediation)
cargo test --workspace --all-features -- --test-threads=1    # green
cargo run --package boa_fapi_wpt -- --manifest wpt-manifest.json --smoke   # exit 0
cargo run --package boa_fapi_wpt -- --manifest wpt-manifest.json --expectations expectations.json --upstream-root target/pinned-wpt --check-expectations --json target/wpt-observation.json --junit target/wpt-observation.xml   # exit 0
cargo run --package boa_fapi_wpt -- --manifest wpt-manifest.json --expectations expectations.json --upstream-root target/pinned-wpt --strict --json target/wpt-release.json --junit target/wpt-release.xml                 # exit 1 (5 blockers)
$env:RUSTDOCFLAGS='-Dwarnings'; cargo doc --workspace --no-deps   # 0
cargo deny check                                             # advisories/bans/licenses/sources ok
git diff --check                                             # 0
```

Reports (SHA-256):

| Report | SHA-256 |
|---|---|
| `target/wpt-observation.json` | `6011b72452087edd642fb5466a714b6fe578f9d0fa51a0ecb1652f5c696688bd` |
| `target/wpt-observation.xml` | `20279b95bb427875297cd7698933b98de159dbd4bbe84d2a5b4a7617ab234b21` |
| `target/wpt-release.json` | `58448963fa743065c520668c8db23876d3377e48cf5dabd19dcce1dfd6c23b58` |
| `target/wpt-release.xml` | `227655cf45559b9ce67e172e82ea462e882cc5c918f8568d6ac69232583fec33` |

Threads determinism: `--check-expectations --threads 1` и `--threads 2`
JSON byte-identical (`target/wpt-observation.json` ==
`target/wpt-observation-t2.json`).

## 4. Mutation fixtures (§7) — non-zero

`cargo test --package boa_fapi_wpt --test gate_remediation` (каждый fixture
генерирует собственные manifest/inventory/expectations/corpus/upstream в temp):

| ID | Mutation | Ожидание | Результат |
|---|---|---|---|
| `M9E-R1-01` | один expected FAIL совпадает с actual FAIL | `--check-expectations` exit 0 / `--strict` non-zero, `defects=1` | ok (exit 0 / 1) |
| `M9E-R1-02` | duplicate exact id (contradictory terminal), extra row, missing row | strict non-zero, `expectation_drift` | ok |
| `M9E-R1-03` | `DYNAMIC:` уже в manifest | один раз; threads 1/2 byte-identical | ok |
| `M9E-R1-04` | удалить exclusion; fake path; duplicate exclusion; executed+excluded; invalid capability; expired review | strict non-zero (`integrity`/`expectation_drift`) | ok |
| `M9E-R1-05` | полная отчётность baseline | `inventory 7/7`, все id один раз, exclusions видны, JUnit totals | ok |
| `M9E-R1-06` | green/red таблица | exit 0 ⟺ green; red non-zero | ok |
| `M9E-R1-07` | mutated upstream bytes | strict non-zero, `exit_reason=integrity` | ok |

## 5. Отклонения

1. База: ответвлено от `9a8cd63`, а не буквально от `37b6918`; WPT-часть
   идентична, отличие — только в принятых M9-D ring-fix/CI fixes (см.
   таблицу). Иных отклонений нет.
2. `unsupported-artifact` определён в closed-list типе disposition, но
   текущая audited модель относит все 79 неисполненных путей к
   `excluded-capability` (у каждой строки есть точная capability/reason/
   review date). Число `excluded = 79` согласовано с §5 заказа;
   metadata-only путь без exclusion сейчас отсутствует, поэтому
   `unsupported-artifact == 0`.
3. Внешние CI jobs прогнаны на ветке. Итоговый run
   `https://github.com/DavidGoliaf/FileAPI_boa/actions/runs/34760217649`
   (commit `d6591e5`): `M8 validation (ubuntu-latest|windows-latest|
   macos-14)` — success, `M9E gate negative controls (ubuntu-latest|
   windows-latest)` — success, `M9E release conformance (strict gate)` —
   failure по `exit_reason=release_defects` (5 defects, 0 unexpected,
   0 timeout, 0 drift; полный summary в логе шага). Это ожидаемое
   состояние §6.6: mechanics/tests/negative controls зелёные, release job
   честно красный ровно из-за перечисленных пяти product/harness дефектов.
   Предыдущий run `34756831855` (`1d225b7`) отличался только macOS flake
   `gate_remediation::r1_03` (threads-2 worker wall deadline 5s), он
   исправлен (`1d225b7`: fixture default timeout 60s).
4. Осознанно НЕ сделано (обсуждено, решение за ревьюером): вынос release
   gate в отдельный workflow, чтобы основной `CI`-workflow был полностью
   зелёным, а release-статус оставался отдельным красным сигналом.
   Текущая реализация оставлена буквально по §6.3/§6.6: required
   release job обязан быть красным, пока открыты пять product/harness
   дефектов; маскировка (`continue-on-error`/инверсия exit code)
   запрещена и не применялась. Полностью зелёный pipeline возможен только
   после устранения пяти дефектов отдельными заказами (в этом заказе §8
   их правка запрещена).

## 6. DECISIONS

Добавлен `ADR-0047 (M9-E-R1): canonical run model, expectations-match vs
release-green` в `docs/DECISIONS.md`. Новых зависимостей нет; `cargo-deny`
не меняется.

## 7. Ретроспектива и найденные дефекты (AGENTS.md §10)

Найдено и исправлено до handoff:

- **Двойной учёт `DYNAMIC:`**: 3 id, уже присутствовавших в manifest,
  добавлялись `tracker_subtests` повторно (420 строк при 417 уникальных,
  `exclusions=35` вместо 32). Исправлено: tracker пропускает id,
  присутствующий в manifest; totals теперь 417 + 79 = 496 ровно один раз.
- **`strict_pass: true` при exit 0 и красном release**: ложное смешение
  observation и verdict. Исправлено: `strict_pass` удалён, `--strict`
  non-zero при `release_green == false`.
- **79 file-level exclusions вне totals**: теперь top-level `exclusions[]`
  + synthetic JUnit suite, входят в `results.notrun` (111 = 32 + 79).
- **Отсутствие inventory disposition**: биjection inventory ↔ disposition
  добавлен и валидируется до вердикта; `unaccounted == 0`.
- **CI green при известных FAIL**: проверка `defects == 5` удалена;
  required release job честно красный.
- **`--unknown`/`copy`-rights**: негативные controls вынесены в
  `gate_remediation` (self-contained fixtures), а не в shell-мутацию
  pinned checkout.

Дополнительный ретроспективный проход (второй) нашёл и исправил:

- **Класс exit reason для timeout**: чистый TIMEOUT (нет harness entry)
  классифицировался как `expectation_drift`, потому что `blockers.unexpected`
  инкрементировался в timeout-ветке и проверялся раньше. Теперь timeout —
  только `blockers.timeouts` → `execution_failure`, а `expectations_match`
  дополнительно требует `timeouts == 0`. `--strict` по-прежнему non-zero.
- **Adapted-conformance учёт**: PASS-строка genuine `adapted`-файла
  (classification `supported`) ошибочно попадала в `smoke_pass`, потому что
  бакет выбирался по provenance. Теперь бакет выбирается по classification:
  `project-acceptance` → smoke, иначе upstream. На текущих данных 374/6 не
  меняется, но будущие `executed-adapted` файлы считаются upstream.
- **JSON/JUnit totals при duplicate result**: `files[]` сохранял дубликаты,
  из-за чего JUnit `tests` мог превысить `results.total`. Канонический
  `files` теперь дедуплицируется (первая occurrence), дубликат остаётся
  только в `expectation_drift`; JSON/JUnit/console totals совпадают для
  любого входа.
- **Bare `--manifest` без режима**: принимался и мог маркировать smoke-run
  как `WPT_STRICT`. Теперь требуется ровно один из
  `--smoke`/`--strict`/`--check-expectations`.
- **Launch-ошибки теперь несут JSON reason**: чтение/парсинг manifest,
  hash-проверка и worker launch errors пишут failure-JSON с
  `exit_reason=integrity`/`execution_failure` (раньше часть уходила в
  `Err` → exit 2 без JSON).

## 8. Что дальше

- Пять product/harness дефектов §8 закрыты отдельными defect-заказами
  (см. `docs/reviews/M9F-WPT-DEFECT-REMEDIATION-handoff.md`): sync
  `abort()`, task/microtask boundary после `loadstart`,
  `readAsDataURL` `application/octet-stream`, `File.name` verbatim.
  `m9e-release-conformance` стал зелёным без изменения workflow.
- Общая release-приёмка M9-E разрешена: `release_green == true`.
