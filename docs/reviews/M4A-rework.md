# M4-A Rework Order — FileReader acceptance blockers

## Status

`REWORK REQUIRED` after independent acceptance of commit `878d902` on
`task/m4a`.

Base commit: `040211d`.

The owner explicitly authorizes an exception to the approximate 3000-line
production+test diff limit from work order `tasks/05_TASK_FILEREADER_ASYNC.md`.
The measured M4-A change is 4582 changed lines under `crates/src+tests`
(2957 production and 1625 tests). This size deviation is accepted as a scope
exception and is not a rework blocker. Do not remove required behavior, tests,
or assertions to reduce the diff.

## Findings to fix

### 1. Correct validation evidence

The independent run reproduced these results:

- commands 1–11 and 13 from work order §7: exit code 0;
- `cargo deny check`: exit code 1 because the RustSec advisory database could
  not be fetched from GitHub;
- final CI: not verified by the owner (`awaiting customer verification`).

Update `docs/m4a-validation.md`, `docs/m4a-final-audit.md`, and
`docs/reviews/M4A-handoff.md` so that they do not claim `cargo deny` passed or
that all commands exited 0. Record the exact blocked reason and preserve the
required `BLOCKED` classification. Do not invent an advisory database result,
CI run, URL, or run ID.

### 2. Update the README

`README.md` still says that FileReader and the DOM shim are not implemented.
Update it to describe the installed M4-A surface, the explicit
`context.run_jobs()` contract, the memory-backed-only boundary, the
`dom-shim` capability, and the explicitly omitted M4-B features. Do not claim
full DOM, filesystem sources, `FileReaderSync`, blob URLs, structured clone, or
WPT support.

### 3. Close the stale-generation read race

In `crates/boa_fapi/src/filereader.rs`, `run_pump` dispatches `loadstart` and
then calls `reader_core.read_next()` without rechecking the generation. A
`loadstart` handler can call `abort()` or start a new read, so the old job must
become a strict no-op before it reads or mutates anything.

Add the required generation checks after every reentrant non-terminal dispatch
and before source reads, successor enqueue, packaging, slot release, or event
emission. Cover at least abort/replacement from `loadstart`, progress, and
final-progress handlers. A stale job must not read from the source, mutate the
new operation, release its slot, or emit any event.

### 4. Remove the extra Clock read at EOF

`finish_at_eof` currently reads the injected Clock again for final progress.
Pass the pump timestamp into the finish path so one pump uses one clock sample.
Add or strengthen a step/fake-clock assertion that detects the extra read and
update the audit text to match the implementation.

### 5. Add missing source-failure coverage

Extend `crates/boa_fapi/tests/m4_filereader_async.rs` with a controlled private
test `ByteSource`/source setup, without exposing a production test hook. Prove
JS-visible behavior for short response, long response, and source failure:

- `readyState === DONE`;
- `result === null`;
- `error.name === "NotReadableError"`;
- exactly `error` followed by conditional `loadend`;
- no partial result and no quota leak.

Also cover the required reentrant `error` and `abort` cases, not only the
successful `load` case.

### 6. Make the property/model test real

The current test uses a fixed script list and discards the JS `outcome`.
Replace it with a bounded generated or enumerated operation corpus that
executes real JS and compares the observed state/event log against a pure
model. It must exercise start, one-job advancement, abort, handler-start-new,
stale completion, every terminal kind, and generation replacement. Assertions
must fail when the implementation diverges. No `#[ignore]`, reduced corpus,
private-state-only assertion, or fabricated coverage is allowed.

## Required revalidation

After the final production/test change, run the exact work order §7 sequence
in order:

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo test --package boa_fapi --test m2_blob_file_filelist
cargo test --package boa_fapi --test m3_promise_blob_reads
cargo test --package boa_fapi --test m3_blob_streams
cargo test --package boa_fapi --test m4_filereader_async
$env:RUSTDOCFLAGS='-Dwarnings'; cargo doc --workspace --no-deps
cargo test --package boa_fapi --doc
cargo llvm-cov --package boa_fapi --all-features --fail-under-lines 85
cargo hack check --feature-powerset --depth 2
cargo deny check
git diff --check
```

Record every exit code as `PASS`, `FAIL`, or `BLOCKED`. `cargo deny` remains
`BLOCKED` if the advisory database is unavailable; the owner must later verify
final Windows and Ubuntu CI. Re-run the final audit after fixes and update the
handoff only with actual results.

## Scope and safety constraints

- Do not modify the work order or its acceptance criteria.
- Do not start M4-B or M5.
- Do not add `unsafe`, raw pointers, fabricated references, production
  `unwrap`/`expect`/`panic`, blanket lint suppression, or a production test
  hook.
- Do not weaken or delete existing assertions to make tests green.
- Preserve the owner-approved diff-size exception; optimize only when it
  improves correctness or maintainability without reducing required coverage.

## Completion evidence

The rework is ready for a fresh independent acceptance pass only when the
source fixes, missing tests, truthful documents, exact local validation, and
final audit are all complete. The handoff must explicitly reference this
rework document and state the actual CI status.
