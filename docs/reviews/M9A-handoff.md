# M9-A handoff — current acceptance state

| Поле | Значение |
|---|---|
| ID | `M9-A-ACCEPTANCE-REMEDIATION` |
| Ветка | `task/m9a` |
| Implementation baseline | `271af7552005d5543ae97cb908416c1258f4e57f` |
| Implementation commit | `edb8bb2` — `Accept EOF-terminated MIME quoted strings` |
| Scope | decoder completeness, MIME parsing, Web IDL constructor order, bounded sequence preflight, snapshot/BOM regressions |

## Реализация

- `package::IncrementalDecoder` обрабатывает `OutputFull`, продвигает вход по
  фактически прочитанным байтам, fallibly расширяет UTF-8 output и flush'ит
  pending output до завершения.
- MIME `charset` читается только после успешного локального MIME parse:
  type/subtype валидируются, а отдельные malformed parameters пропускаются;
  quoted values поддерживают `;` и завершаются накопленным значением на EOF;
  suffix после закрывающей кавычки игнорируется, duplicate parameters
  используют first-parameter-wins.
  Encoding labels очищаются только от ASCII whitespace; неизвестный label
  продолжает fallback MIME → UTF-8.
- `Blob` и `File` выполняют все argument conversions до observable
  `NewTarget.prototype`; phase-1 sequence conversion ведёт checked нижнюю
  size-bound и fallible `Vec` growth. Финальный exact accounting остаётся в
  processing после `endings`.
- Snapshot regressions читают фактические bytes через `FileReaderSync`, а
  UTF-8 split cases проверяют независимое ожидаемое содержимое. BOM split
  `EF | BB BF 42` и `EF BB | BF 42` проверяется прямым unit-test decoder;
  public reader отдельно проверяет BOM в начале входа и multibyte boundary.
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

Все локальные команды завершились с exit code 0; targeted suites дали 23
M9, 33 M4 async и 21 M4 sync. `cargo deny` сообщил только существующие
warnings (license fields/duplicate transitive crates), при этом его checks
`advisories`, `bans`, `licenses` и `sources` — `ok`. `cargo hack` также
завершился успешно с pre-existing dead-code warnings в feature-reduced
комбинациях. Follow-up P0 diff реализации: 2 файла, 9 insertions(+), 18
deletions; лимит 3000 строк не превышен.

CI evidence получено для follow-up P0-коммита `431a355`:
[run 34268593654](https://github.com/DavidGoliaf/FileAPI_boa/actions/runs/34268593654)
завершился `success`; Windows, Ubuntu и macOS jobs — все `success`. В CI
прошли workspace tests, WPT strict runs, coverage, cargo-hack, cargo-deny,
package, docs и diff check.

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
