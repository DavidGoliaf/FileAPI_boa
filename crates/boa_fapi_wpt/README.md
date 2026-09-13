# `boa_fapi_wpt`

Pipeline role: dev-only deterministic WPT conformance gate over the pinned
upstream `FileAPI/**` tree (M9-E). Drives the accepted M1–M6 surface plus
the M9-B/M9-C/M9-D async host loop from fresh Boa `Context`s; adds no JS
API and changes no binding.

Corpus: 36 raw pinned upstream `.any.js` files executed byte-identically
(`direct` provenance) plus one handwritten project-owned FileList fixture
(`filelist-host.js`, `adapted`, never presented as WPT). Hashes:
`wpt-manifest.json` pins corpus SHA-256, `upstream_sha256` raw-content
evidence, and provenance; `wpt-inventory.json` pins the full 115-file
`FileAPI/` tree. The harness verifies corpus hashes before execution;
`--strict` additionally verifies inventory and raw upstream bytes under
`--upstream-root`. Any drift is a launch error.

```sh
cargo run --package boa_fapi_wpt -- --manifest wpt-manifest.json --smoke
cargo run --package boa_fapi_wpt -- --manifest wpt-manifest.json --expectations expectations.json --upstream-root <PINNED_WPT> --strict
cargo run --package boa_fapi_wpt -- --manifest wpt-manifest.json --expectations expectations.json --upstream-root <PINNED_WPT> --check-expectations
cargo run --package boa_fapi_wpt -- --manifest wpt-manifest.json --expectations expectations.json --upstream-root <PINNED_WPT> --strict --threads 2
```

`--smoke` reports `ADAPTED_SMOKE` (adapter/harness check only, no
inventory/expectations, never WPT conformance). `--strict` is the release
gate: it exits `0` **only** when `release_green == true`. `--check-expectations`
is the diagnostic observation mode: it exits `0` when the actual statuses
and identities reproduce the audited expectations, but always reports
`release_green: false` and is never a conformance/release pass. While the
five recorded open defects remain, `--strict` is non-zero with
`exit_reason: release_defects`.

One canonical run model (`accounting::CanonicalRun`) is the single source
for console, JSON (schema 2), JUnit and the exit code. The report carries
`expectations_match`, `release_green`, `release_blockers`, `exit_reason`,
`inventory` (`total`/`executed_direct`/`executed_adapted`/`excluded`/
`unaccounted`), `results` (`total`/`unique`/`upstream_pass`/`smoke_pass`/
`defects`/`notrun`/`unexpected`), executed `files[]` and the 79 file-level
`exclusions[]` (path, capability, reason, owner, issue, review date). The
legacy `strict_pass` field is removed. Exit reason classes are distinct in
JSON: `integrity`, `expectation_drift`, `execution_failure`,
`release_defects`.

Statuses: `PASS`/`FAIL`/`TIMEOUT`/`NOTRUN` with deterministic JSON/JUnit
output (`--json`/`--junit`). Expected `FAIL` (recorded open defect) keeps
`expectations_match` true but is a release blocker. Reports are CI
artifacts and are never committed. The `m9e-gate-negative-controls` CI job
runs `gate_remediation` (mutated upstream/inventory/expectation/result
sets must be non-zero); the `m9e-release-conformance` job runs `--strict`
without inversion and is honestly red until the defects are fixed.

Limits: conformance is claimed for the directly executed upstream subset
(417 executable subtests) plus 79 audited file-level inventory exclusions
(canonical `496/496`, inventory `115/115`); the rest of `FileAPI/**` is
covered by exact `NOTRUN` exclusions with closed-list capabilities. Full
DOM/Workers/Fetch (incl. WHATWG URL parsing)/full Streams stay out of
scope (exact gaps in `expectations.json`, never `PASS`).

See top-level `README.md`, `docs/wpt.md`, and `TZ_boa_fapi_FileAPI.md`.
