# M9-A handoff — current acceptance state

| Поле | Значение |
|---|---|
| ID | `M9-A-ACCEPTANCE-REMEDIATION` |
| Ветка | `task/m9a` |
| Implementation baseline | `271af7552005d5543ae97cb908416c1258f4e57f` |
| Implementation commit | `d8d4dd7` — `Fix M9-A decoder and Web IDL conformance` |
| Scope | decoder completeness, MIME parsing, Web IDL constructor order, bounded sequence preflight, snapshot/BOM regressions |

## Реализация

- `package::IncrementalDecoder` обрабатывает `OutputFull`, продвигает вход по
  фактически прочитанным байтам, fallibly расширяет UTF-8 output и flush'ит
  pending output до завершения.
- MIME `charset` читается только после успешного локального MIME parse:
  type/subtype и параметры валидируются, quoted values поддерживаются,
  duplicate parameters используют first-parameter-wins. Encoding labels
  очищаются только от ASCII whitespace; неизвестный label продолжает fallback
  MIME → UTF-8.
- `Blob` и `File` выполняют все argument conversions до observable
  `NewTarget.prototype`; phase-1 sequence conversion ведёт checked нижнюю
  size-bound и fallible `Vec` growth. Финальный exact accounting остаётся в
  processing после `endings`.
- Snapshot regressions читают фактические bytes через `FileReaderSync`, а
  UTF-8 split cases проверяют независимое ожидаемое содержимое для обоих
  boundary positions.
- `filereader.rs` и `filereader_sync.rs` изменены только для подключения
  общего fallible decoder path и сохранения существующей async/sync error
  mapping.

## Traceability

`M9A-RW-07`…`M9A-RW-14` имеют production и test anchors в
`docs/spec-matrix.md`. Новых crate dependencies нет; MIME parser decision
зафиксирован в ADR-0041.

## Локальная проверка

Локальная command matrix:

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo test -p boa_fapi --test m9_webidl_conformance -- --nocapture
cargo test -p boa_fapi --test m4_filereader_async -- --nocapture
cargo test -p boa_fapi --test m4_filereader_sync -- --nocapture
$env:RUSTDOCFLAGS='-D warnings'; cargo doc --workspace --no-deps
cargo deny check
cargo hack check --feature-powerset --depth 2
git diff --check 271af7552005d5543ae97cb908416c1258f4e57f...HEAD
```

Все локальные команды завершились с exit code 0; targeted suites дали 22
M9, 33 M4 async и 21 M4 sync. `cargo deny` сообщил только существующие
warnings (license fields/duplicate transitive crates), при этом его checks
`advisories`, `bans`, `licenses` и `sources` — `ok`. `cargo hack` также
завершился успешно с pre-existing dead-code warnings в feature-reduced
комбинациях. Incremental diff реализации: 7 файлов, 615 insertions(+),
75 deletions; лимит 3000 строк не превышен.

CI evidence для текущего implementation commit не получено: commit не
публиковался, external runs и ссылки отсутствуют. Это единственный
оставшийся acceptance blocker; локальные результаты не выдаются за CI.

## Targeted-search classification

В обязательном `rg`-поиске совпадения классифицированы так: `tasks/19` —
текущий acceptance task и его поисковая команда; `tasks/05`, `tasks/06`,
`tasks/11`, `tasks/18` и связанные task-файлы — исторические планы/решения,
не runtime-oracle; старые записи `docs/m4a-final-audit.md` и
`docs/DECISIONS.md` — исторические evidence. `Symbol.iterator` в текущих
M9-тестах и FileList-коде — актуальный protocol surface, а `per byte` в
`streams.rs`/WPT manifest — несвязанные текущие комментарии. Остальные
production/test совпадения описывают актуальный fallback/decoder behavior;
дефектных stale assertions не найдено.

## Stop boundary

M9-B не начинался. После независимой повторной приёмки дальнейшие изменения
делаются отдельным work order.
