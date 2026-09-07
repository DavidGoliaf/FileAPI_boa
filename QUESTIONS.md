# QUESTIONS.md — M7 WPT supply decision (§3)

## Q1: Какой способ поставки WPT corpus используется?

**Решение (зафиксировано до реализации runner):** второй разрешённый
способ — небольшой лицензированно разрешённый адаптированный набор,
хранимый в репозитории.

- Upstream repository: `https://github.com/web-platform-tests/wpt`
- Pinned commit SHA: `0968c868d8095217d18d86b34c7f21dccae58768`
  (`master` на 2026-09-07, проверен через GitHub API до реализации).
- License: WPT распространяется под `BSD-3-Clause`
  (файл `LICENSE.md` в корне upstream); адаптированные выдержки хранятся
  с атрибуцией upstream path + commit SHA в `wpt-manifest.json`.
- Хранимые файлы: адаптированные JS-кейсы в
  `crates/boa_fapi_wpt/corpus/*.js` (не verbatim-копии полных
  copyrighted страниц — только покрывающие assertions, переписанные под
  поддерживаемые capabilities). Полные upstream-страницы в репозиторий
  не коммитятся (запрет §2.8).
- Целостность: `wpt-manifest.json` хранит SHA-256 каждого хранимого
  адаптированного файла; harness проверяет hash до выполнения, mismatch
  — ошибка запуска, а не `NOTRUN`. Upstream blob SHA (git SHA-1 из API)
  зафиксированы в manifest как provenance, SHA-256 оригиналов не
  требуются, т.к. исполняется адаптированный набор (честно заявлено в
  `docs/wpt.md`: conformance заявляется только для адаптированного
  набора, полный upstream pass не заявляется).

## Q2: Почему не pinned checkout под `wpt/`?

CI и локальный запуск не гарантируют наличие `wpt/` checkout, а runtime
network download запрещён (§2.4). Адаптированный набор — единственный
способ с воспроизводимыми запусками из fresh clone только с Rust
toolchain.

## Q3: Блокеры?

Блокеров нет для адаптированного набора. Полный upstream pass —
`NOTRUN` по capability gaps (точные записи в expectations), не PASS.
