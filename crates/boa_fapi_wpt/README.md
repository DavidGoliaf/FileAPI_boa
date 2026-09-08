# `boa_fapi_wpt`

Pipeline role: dev-only deterministic conformance harness over the pinned
adapted WPT subset (M7). Drives the accepted M1–M6 surface from fresh Boa
`Context`s; adds no JS API and changes no binding.

Manifest hash: `wpt-manifest.json` pins the adapted corpus by SHA-256;
the harness verifies hashes before execution. Strict command:

```sh
cargo run --package boa_fapi_wpt -- --manifest wpt-manifest.json --strict
cargo run --package boa_fapi_wpt -- --manifest wpt-manifest.json --strict --threads 2
```

Statuses: `PASS`/`FAIL`/`TIMEOUT`/`NOTRUN` with deterministic JSON/JUnit
output (`--json`/`--junit`). Reports are CI artifacts and are never
committed.

Limits: conformance is claimed only for the adapted subset; full upstream
WPT pass is not claimed. Full DOM/Workers/Fetch/full Streams stay out of
scope (exact capability gaps in expectations, never `PASS`).

See top-level `README.md` and `TZ_boa_fapi_FileAPI.md`.
