# M9-F pre-acceptance handoff

**Status: NOT READY FOR MERGE OR RELEASE.** Local source, conformance,
coverage, package, documentation, and feature-matrix checks pass, but the
release-delivery acceptance criteria are incomplete.

Source/test candidate checked: `644faf40cdf1ff5a3a0389397e96d857b43412be`;
the local validation results are in `docs/m9-validation.md`. This PR adds
release documentation and a CI comment correction on top of that SHA. CI
must pass again on the resulting PR head before merge.

## Completed in this preparation pass

- Updated the root README to describe M1–M9 and the current M9-E conformance
  gate, including the exact executed and excluded WPT totals.
- Corrected the changelog's status for the accepted audit follow-up and its
  recorded CI commit.
- Corrected the CI workflow comment that still described the now-remediated
  WPT defects as open.
- Ran formatting, strict Clippy, full workspace tests, strict WPT, rustdoc,
  both required package coverage thresholds, cargo-hack feature powerset,
  offline package verification, and `git diff --check`; all passed.

## Required before merge

1. Establish the required integration sequence into `master`; the current
   candidate is 44 commits and 118 files ahead of the unchanged `master` and
   therefore is not a small M9-F-only merge. The preflight found no existing
   pull request.
2. Verify current-SHA CI on Linux, macOS, and Windows after integration.

### Integration topology and order conflict

The M2–M8 branch tips are already ancestors of `master`; they need no further
merge. The unmerged M9 heads are ancestors of the candidate. The side branch
`task/ci-green-wpt-gate` is already included through merge commit `09f9d53`.
The candidate contains 37 commits through M9E-R1 plus 7 subsequent WPT
remediation commits.

There is an integration-order conflict in the work orders. The final M9E-R1
head `b6bf0c0` has CI run
[34764341977](https://github.com/DavidGoliaf/FileAPI_boa/actions/runs/34764341977):
the validation, negative-control, coverage, package, and deny jobs passed,
but the required strict release job failed on its five documented defects.
The combined candidate `644faf40` has green current-SHA run
[35216670609](https://github.com/DavidGoliaf/FileAPI_boa/actions/runs/35216670609).
Merging the M9E-only head first would therefore leave `master` with a failing
required release gate. The safe green integration is one reviewed PR from the
candidate containing the full M9A–M9E chain and the seven remediation commits.
This follows the requested single-PR integration; preserve the accepted M9-E
handoff and verify the full M9 chain in that PR rather than merging the known
red M9E-only head separately.

## Required after merge

- Verify GitHub default branch and branch protection on `master`.
- Confirm the nightly workflow is registered from `master`, dispatch one
  run, and record its terminal conclusion.
- Perform a normal fresh clone and run the documented package and release
  gates from it.
- Record the post-merge SHA and external evidence in the final acceptance
  record. Do not infer any of these results from the current local run.

Pre-PR external state was verified read-only: default branch is `task/m2`,
neither `master` nor `task/m2` is protected, only the `CI` workflow is
registered from the current default branch, and a fresh clone lands on
`task/m2` at `5620dc4`. Current candidate CI is green at run `35216670609`,
but this does not substitute for merge, default-branch, nightly, or fresh-clone
acceptance. No push, merge, default-branch change, nightly dispatch, or tag
was performed.
