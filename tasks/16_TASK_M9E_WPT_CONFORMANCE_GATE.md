# Заказ M9-E — upstream-backed WPT conformance gate

| Поле | Значение |
|---|---|
| ID | `M9-E-WPT-CONFORMANCE` |
| База | принятый M9-D head |
| Ветка | `task/m9e` |
| Upstream | `web-platform-tests/wpt` commit `0968c868d8095217d18d86b34c7f21dccae58768` |
| Нормативная база | ТЗ §11, §15.8; WPT license and pinned FileAPI inventory |
| Закрывает | A-08, A-09; добавляет conformance evidence для A-02…A-07 |
| Предельный diff | 2800 строк production+tests+docs; generated inventory и внешнее WPT checkout не входят, но adapted handwritten corpus входит |

## 1. Цель

Превратить текущий `38 PASS` smoke suite в проверяемый conformance gate.
Новый gate обязан доказать связь каждого запускаемого теста с raw upstream
file на pinned commit, учитывать весь релевантный `FileAPI/**` inventory и
не выдавать адаптированный самописный тест за upstream PASS.

Полный WPT checkout не коммитится в этот репозиторий. CI и приёмщик получают
его отдельным pinned checkout; runner не загружает сеть и не запускает shell.

## 2. Два режима, разные заявления

Сохранить быстрый offline smoke для разработки:

```powershell
cargo run --package boa_fapi_wpt -- --manifest wpt-manifest.json --smoke
```

Он проверяет adapter/harness, но в summary и docs явно называется
`ADAPTED_SMOKE`, не `WPT conformance`.

Нормативный gate:

```powershell
cargo run --package boa_fapi_wpt -- `
  --manifest wpt-manifest.json `
  --expectations expectations.json `
  --upstream-root <PINNED_WPT_CHECKOUT> `
  --strict
```

`--strict` возвращает non-zero при hash/inventory/adaptation drift, missing or
duplicate result, unexpected PASS/FAIL/TIMEOUT/NOTRUN, просроченном exclusion
или поддержанной capability, ошибочно помеченной NOTRUN. Summary всегда
показывает отдельно upstream PASS, adapted smoke PASS и exclusions.

## 3. Pinned inventory и целостность

Добавить детерминированный `wpt-inventory.json`, полученный из Git tree pinned
commit и содержащий path/blob identity для релевантного `FileAPI/**` дерева.
Generation command и SHA самого inventory фиксируются в `docs/wpt.md`.

Runner без shell/network обязан:

1. Проверить schema и pinned repository/commit literals.
2. Перечислить upstream `FileAPI/**` под `--upstream-root` и сравнить с
   inventory: missing, extra или changed file — launch error.
3. Проверить raw-content SHA-256 каждого выбранного upstream file. Поле
   `upstream_blob_sha`, не проверяемое по raw content, больше не считается
   evidence; добавить реально проверяемый `upstream_sha256`.
4. Проверить SHA-256 adapter/patch/result и exact relationship
   `upstream path -> adapter -> executed tests`.
5. Не принимать logical path traversal, symlink escape, absolute path,
   case collision или duplicate test/subtest.

Если полный inventory слишком велик для review, хранить только metadata, не
исходные WPT pages. Generated metadata не освобождается от schema/hash tests.

## 4. Expectations и capability accounting

Создать отдельный `expectations.json`. Для каждого релевантного upstream
test/subtest требуется exact запись:

- `upstream_path`, test/subtest id и expected status;
- `capability` из закрытого allow-list;
- точная причина, owner, issue/question link и `review_by`;
- `adapter` либо `direct`, если тест выполняется;
- trace ID и нормативный раздел;
- classification: `supported`, `unsupported-host-capability` или
  `harness-gap`.

Правила:

- wildcard/файловый catch-all запрещён;
- `harness-gap` не является допустимым долгосрочным exclusion и ломает
  release gate;
- supported test не может быть NOTRUN;
- browser-only тест допускается NOTRUN только с конкретной capability
  (`html-file-input`, `navigation`, `fetch`, `mediasource`, `worker-runtime`,
  `network-wpt-server`), а не причиной `browser-only`;
- expected FAIL не делает release green: это открытый defect;
- unexpected PASS требует удалить/пересмотреть exclusion, а не маскируется;
- истёкший `review_by` ломает strict запуск.

## 5. Adaptation fidelity

Для JS-only `.any.js` предпочтителен direct execution с минимальным pinned
testharness compatibility layer. Если нужна адаптация, она производится
детерминированным adapter/patch из raw upstream input.

Запрещено:

- удалять custom `@@iterator`, ToString, exception-order или race cases;
- переименовывать изменённый самописный сценарий в upstream subtest;
- сокращать assertions ради совместимости Boa;
- отмечать PASS на основании похожего локального теста.

Если конкретный assertion требует отсутствующую platform capability, subtest
получает точный NOTRUN; реализуемая часть не переписывается на более простой
oracle.

## 6. Host fixtures и FileList

Runner получает per-file fixture hook, выполняемый до JS test на Boa thread.
Для FileList fixture обязан:

1. Создать два разных File через публичный host API.
2. Создать настоящий `FileList` через `FileApiHandle::file_list`.
3. Инъецировать только этот объект под зарезервированным harness binding.
4. Проверить из JS `length`, identity/order, `item()`, indexed access,
   out-of-range behavior, descriptors, clone round-trip и отсутствие public
   constructor.

Проверка обычного Array или списка File до создания FileList не засчитывается.
Iterator methods не добавляются: pinned W3C FileList IDL их не объявляет.

Аналогично Promise/FileReader/Streams tests используют host poll/run_jobs loop
M9-B, а не синхронный source read внутри job.

## 7. Обязательный minimum upstream set

В supported/direct или supported/adapted набор входят все применимые subtests
из pinned файлов как минимум для:

- Blob constructor, BlobPart conversion, slicing, MIME и promise reads;
- File constructor/name/type/lastModified;
- FileList host-created surface;
- FileReader states, read methods, encoding, events и abort;
- Blob URL create/revoke/isolation, если тест не требует Fetch/navigation;
- serialization cases, которые можно выполнить через существующий clone bridge.

Отдельно включить regression cases M9-A: iterable outer sequence, primitive и
object BlobPart fallback, throwing conversion/IteratorClose и unknown encoding
fallback. Если pinned upstream не содержит отдельного subtest, сохранить
проектный JS test с classification `PROJECT_ACCEPTANCE`, не выдавая его за WPT.

## 8. Trace IDs

| Trace ID | Требование |
|---|---|
| `M9E-WPT-01` | pinned full FileAPI inventory and raw upstream hashes |
| `M9E-WPT-02` | direct/adapted provenance and fidelity gate |
| `M9E-WPT-03` | exact expectations and capability accounting |
| `M9E-WPT-04` | real FileList host fixture |
| `M9E-WPT-05` | async host poll integration in runner |
| `M9E-WPT-06` | deterministic JSON/JUnit summary separating WPT and smoke |

Обновить `docs/wpt.md`, `docs/spec-matrix.md`, `docs/spec-delta.md`, CI и
runner README. Исторический M7 report остаётся историческим.

## 9. Приёмка

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo run --package boa_fapi_wpt -- --manifest wpt-manifest.json --smoke
cargo run --package boa_fapi_wpt -- --manifest wpt-manifest.json --expectations expectations.json --upstream-root <PINNED_WPT> --strict
cargo run --package boa_fapi_wpt -- --manifest wpt-manifest.json --expectations expectations.json --upstream-root <MUTATED_WPT> --strict
$env:RUSTDOCFLAGS='-Dwarnings'; cargo doc --workspace --no-deps
cargo deny check
git diff --check
```

Последняя команда обязана завершиться non-zero по ожидаемой hash/inventory
ошибке; handoff записывает это как negative control. Также выполнить
threads 1/2 при поддержке и доказать идентичность отсортированного JSON.

`docs/reviews/M9E-handoff.md` содержит точные totals по classification и все
NOTRUN reasons. Затем остановиться.
