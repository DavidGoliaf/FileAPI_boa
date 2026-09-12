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
cargo run --package boa_fapi_wpt -- --manifest wpt-manifest.json --expectations expectations.json --upstream-root <PINNED_WPT> --strict --threads 2
```

`--smoke` reports `ADAPTED_SMOKE` (adapter/harness check only, never WPT
conformance); `--strict` reports `WPT_STRICT` (normative gate: upstream
PASS, smoke PASS, open defects, and exclusions counted separately).

Statuses: `PASS`/`FAIL`/`TIMEOUT`/`NOTRUN` with deterministic JSON/JUnit
output (`--json`/`--junit`). Expected `FAIL` (one recorded open defect)
satisfies the strict comparison but keeps the release gate red. Reports
are CI artifacts and are never committed.

Limits: conformance is claimed for the directly executed upstream subset
(417 executable subtests); the rest of `FileAPI/**` is covered by exact
`NOTRUN` exclusions with closed-list capabilities. Full DOM/Workers/Fetch
(incl. WHATWG URL parsing)/full Streams stay out of scope (exact gaps in
`expectations.json`, never `PASS`).

See top-level `README.md`, `docs/wpt.md`, and `TZ_boa_fapi_FileAPI.md`.
