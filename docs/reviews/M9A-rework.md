# M9-A acceptance remediation report

| Поле | Значение |
|---|---|
| ID | `M9-A-ACCEPTANCE-REMEDIATION` |
| Branch | `task/m9a` |
| Implementation baseline | `271af7552005d5543ae97cb908416c1258f4e57f` |
| Scope | P0-A/P0-B/P0-C, P1-A/P1-B, traceability and stale-oracle cleanup |
| Implementation commit | `d8d4dd7` — `Fix M9-A decoder and Web IDL conformance` |
| Additional diff | `7 files changed, 615 insertions(+), 75 deletions(-)` from the implementation baseline |

## Исправления

1. `IncrementalDecoder` now loops over `CoderResult::OutputFull`, advances by
   the decoder-reported input count, uses checked fallible output growth and
   flushes pending output at EOF. Async and sync readers use the same result
   path and map allocation failure to the existing controlled resource error.
2. Packaging parses MIME type/subtype and parameters before using `charset`.
   Token and quoted values, case-insensitive names and first duplicate policy
   are covered. Label normalization removes only ASCII whitespace; lookup
   failure falls through to MIME and UTF-8.
3. `Blob` and `File` read `NewTarget.prototype` only after all argument
   conversions. Sequence conversion tracks a checked conservative lower size
   bound before the next iterator step and uses fallible vector growth; exact
   accounting remains after options and line-ending processing.
4. Regression tests assert actual BufferSource bytes, expanding UTF-16 and
   large single-byte outputs, EOF replacement, MIME/event behavior, Proxy
   order/precedence, exact quota boundaries and both UTF-8 split positions.

## Trace rows

`M9A-RW-07` through `M9A-RW-14` are listed with production and test/doc
anchors in `docs/spec-matrix.md`. The MIME parser choice is recorded in
ADR-0041. No new dependency was added.

## Required commands and observed targeted results

The following full and targeted commands completed successfully during this
report:

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

The M9 suite reports 22 passing tests; M4 async reports 33 and M4 sync 21.
The workspace test matrix, clippy, docs, cargo-deny, cargo-hack, formatting
and diff checks all exited 0. Cargo-deny emitted only pre-existing warnings
for missing crate license fields, duplicate transitive crates and one
unmatched license allowance; its four check groups were `ok`. Cargo-hack
emitted only pre-existing dead-code warnings for reduced feature sets.

## Targeted-search classification

The required search was run over `crates docs tasks`. Any remaining matches
are classified as follows:

- `tasks/05_TASK_FILEREADER_ASYNC.md`, `tasks/06_TASK_FILEREADER_SYNC.md`,
  `tasks/11_TASK_CONFORMANCE_REMEDIATION_PLAN.md` and related task files are
  historical scope/plan text for earlier work orders, not current
  implementation oracles.
- `docs/m4a-final-audit.md` and older ADR entries in `docs/DECISIONS.md` are
  historical audit/decision evidence, not the M9-A acceptance oracle.
- `crates/boa_fapi/tests/m9_webidl_conformance.rs` uses iterator protocol
  names as executable JavaScript and documents the required value iterator;
  these are current tests, not stale requirements.
- `crates/boa_fapi/src/streams.rs` and `crates/boa_fapi_wpt/src/manifest.rs`
  contain unrelated current comments using `per byte`; they do not describe
  the FileReader packaging decoder.
- Current production comments and current tests describe fallback/decoder
  semantics without asserting a superseded error path. No defect was found.

No targeted stale assertion remains in `docs/reviews/M9A-handoff.md` or this
report. This classification is intentionally not a claim that historical task
documents contain no old wording.

## Retrospective bug find

Review of the changed paths checked output-full progress, EOF flushing,
quoted/invalid MIME cases, non-ASCII label whitespace, constructor exception
precedence, no iterator closing on conversion quota, exact-limit acceptance,
line-ending lower-bound conservatism, byte snapshots and split-chunk output.
The targeted suites passed after those checks. No implementation outside the
listed M9-A remediation scope was started.

## External acceptance evidence

CI run [34258018136](https://github.com/DavidGoliaf/FileAPI_boa/actions/runs/34258018136)
for commit `2f53748` completed successfully on Windows job `102168615348`,
macOS job `102168615552` and Ubuntu job `102168615816`. The complete CI
matrix, including WPT strict runs and the release gates, passed. M9-B remains
outside this work order.
