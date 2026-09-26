# M9 release validation record

Status: **candidate gates green; release delivery acceptance incomplete**.

Local validation below was run on source candidate
`644faf40cdf1ff5a3a0389397e96d857b43412be` on 2026-09-26. This PR adds
documentation and a corrected CI comment on top of that candidate; the PR
head gets a separate CI run before merge. This record does not claim the
candidate is on `master` or is the repository's default branch.

## Local results

| Gate | Result | Evidence |
|---|---|---|
| Formatting | PASS | `cargo fmt --all -- --check` (exit 0) |
| Clippy | PASS | `cargo clippy --workspace --all-targets --all-features -- -D warnings` (exit 0) |
| Workspace tests | PASS | `cargo test --workspace --all-features -- --test-threads=1` (exit 0) |
| Strict WPT | PASS | `cargo run --package boa_fapi_wpt -- --manifest wpt-manifest.json --expectations expectations.json --upstream-root target/pinned-wpt --strict` (exit 0): `release_green=true`, 115/115 inventory, 496/496 rows, 379 upstream PASS, 6 smoke PASS, 111 NOTRUN, 0 defects, 0 unexpected |
| Rust documentation | PASS | `$env:RUSTDOCFLAGS='-Dwarnings'; cargo doc --workspace --no-deps` (exit 0) |
| Core coverage | PASS | `cargo llvm-cov --package boa_fapi_core --all-features --fail-under-lines 85` (exit 0, 89.78% lines) |
| `boa_fapi` coverage | PASS | `cargo llvm-cov --package boa_fapi --all-features --fail-under-lines 80` (exit 0, 85.41% lines) |
| Feature powerset | PASS | `cargo hack check --feature-powerset --depth 2` (exit 0) |
| Package verification | PASS | `cargo package --workspace --all-features --offline` (exit 0) |
| Cargo deny | PASS in CI; local freshness BLOCKED | Local `cargo deny check` could not fetch `rustsec/advisory-db`; the current-candidate GitHub CI run below passed both `cargo deny fetch db` and `cargo deny check`. |
| Diff whitespace | PASS | `git diff --check` (exit 0) |

The initial online `cargo package --workspace --all-features` also could not
reach crates.io; the offline package verification above completed successfully.

## External evidence and repository state

- Current-SHA workflow run: [35216670609](https://github.com/DavidGoliaf/FileAPI_boa/actions/runs/35216670609)
  on `644faf40cdf1ff5a3a0389397e96d857b43412be`, conclusion `success`.
  It passed M8 validation on Ubuntu, Windows, and macOS, both M9-E negative
  control jobs, the strict M9-E release conformance job, cargo-deny, and the
  remaining enabled CI gates. Ubuntu-only package/coverage/wasm steps were
  skipped by the workflow matrix; package and coverage thresholds were
  checked locally as recorded above.
- Repository API reports `default_branch=task/m2`. `gh workflow list --all`
  shows only `CI`; nightly is not registered from the current default branch.
- Read-only protection checks returned 404 (`Branch not protected`) for both
  `master` and `task/m2`.
- A normal fresh clone at
  `C:\Users\gamer\AppData\Local\Temp\boa-fapi-m9f-fresh-clone-20260926`
  checked out `task/m2` at `5620dc466a2286cfb9924044f35fdad9aa363685`.
  This fails the M9-F fresh-clone release criterion, which expects the
  accepted M9 SHA from `master`.
- The preflight found no existing pull request for the repository.

## Delivery blockers

1. The M9-F order requires accepted M9-E on `master`; local `master` and
   `origin/master` are still `1519c3c0edc6dcbac7330ef16ba410e3eb9ac564`, while
   the candidate is 44 commits ahead. No merge has been performed.
2. Although current-SHA GitHub CI is green, `master` integration has not
   happened, branch protection is absent, and the candidate has no PR.
3. Nightly is not registered from the default branch and has not been
   dispatched.
4. An ordinary fresh clone still selects the old `task/m2` release state.
5. Local cargo-deny advisory freshness remains unverified; the current-SHA CI
   job did pass both advisory fetch and `cargo deny check`.

No remote push, merge, default-branch change, or tag was made.
