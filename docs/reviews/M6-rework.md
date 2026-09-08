# M6 Rework Order — code-boundary and clone-contract fixes

## Status

`REWORK REQUIRED` after independent code review of implementation commit
`9e6d47b` on branch `task/m6`.

The review was source-directed. The findings below are not waived by green
tests or by the current validation report. Fix the production boundaries
first, then add regression tests that prove the repaired contract.

## Scope and base

Base implementation under review:

```text
9e6d47b Fix M6 boundary review findings
```

Primary files:

- `crates/boa_fapi/src/extension.rs`
- `crates/boa_fapi/src/url_shim.rs`
- `crates/boa_fapi_core/src/blob_url.rs`
- `crates/boa_fapi_core/src/clone.rs`
- corresponding M6 integration/core tests and `docs/spec-matrix.md`

Do not change accepted M1–M5 behavior, filesystem security, central error
mapping, or the M6 public contract outside the findings below. Do not start
M7 work. Keep all changes on `task/m6` and do not work directly on `main`.

## Findings to fix

### R1 — P1: close the mutable Blob URL store escape

Current code exposes the live context store through:

```rust
FileApiHandle::url_store()
FileApiHandle::environment_key()
BlobUrlStore::insert_capped()
```

Together these APIs let host code bypass the M6 creation boundary:

- insert a caller-chosen or malformed URL without CSPRNG UUID generation;
- pass an arbitrary quota instead of `max_blob_urls_per_global`;
- create entries in `ServiceWorker`, where normal creation is forbidden;
- insert after shutdown, after `store.clear()` has run;
- retain or inject arbitrary `Arc<BlobData>` entries independently of the
  registered environment contract.

The store is intended to be an implementation detail behind create/resolve/
revoke. Remove the public mutable escape. Preferred solution:

1. Make `FileApiHandle::url_store()` and `FileApiHandle::environment_key()`
   non-public or remove them.
2. Keep environment/key construction internal to the registered handle.
3. Keep URL creation in the single `create_url_for_specs` path, where
   shutdown, ServiceWorker gate, quota, entropy, collision retry and owner
   key are checked together.
4. If `BlobUrlStore` remains public from `boa_fapi_core` for a legitimate
   engine-independent use, make its direct insertion API unable to affect a
   `FileApiHandle` store. Do not replace the escape with another public raw
   insertion token.
5. Replace tests that inspect `url_store().len()` with observable safe
   behavior or internal unit tests. Strong-reference release should be proved
   with `Weak<BlobData>`/resolve behavior, not by exposing the store.

Add a guard or compile-level API test demonstrating that the binding crate
does not expose a mutable store or environment identity escape. Also prove
that URL creation remains rejected in ServiceWorker and after shutdown.

### R2 — P1: implement required Web IDL conversion for revokeObjectURL

`URL.revokeObjectURL` is declared as:

```webidl
static undefined revokeObjectURL(DOMString url);
```

The current implementation returns `undefined` when the argument is missing
and silently converts a throwing `toString()` into a no-op. That is not the
required-argument/DOMString behavior.

Change the binding so that:

1. missing argument throws `TypeError` before store access;
2. conversion uses the existing central `webidl::dom_string` conversion;
3. an abrupt conversion (for example an object whose `toString()` throws)
   propagates the original JS exception;
4. valid non-blob, malformed, unknown, revoked and unauthorized string URLs
   still silently return `undefined` and do not disclose existence;
5. the method remains synchronous, returns `undefined`, and does not enqueue
   a Boa job.

Do not fix this by treating `undefined` as an empty string. The required
  parameter check must be explicit before conversion.

Add integration coverage for:

- `URL.revokeObjectURL()` → `TypeError`;
- `URL.revokeObjectURL(Symbol())` → conversion `TypeError`;
- a throwing `toString()` object → the thrown exception propagates;
- malformed/unknown/already-revoked strings → `undefined`, with no store
  mutation or existence oracle;
- ordinary valid revoke and repeated revoke.

### R3 — P1: make clone encoding bounds symmetric and enforceable

`serialized_file()` checks `MAX_CLONE_STRING_BYTES`, but
`serialized_blob()` does not check `media_type`. `push_str()` likewise only
checks total payload bytes. A `Blob` with an oversized media type can produce
an encoded payload that the own decoder rejects because `read_string()` does
enforce the string ceiling. The public `FileApiClonePayload` fields also
allow callers to construct unchecked values directly.

Repair the codec at the boundary, not only in tests:

1. Apply `MAX_CLONE_STRING_BYTES` to `SerializedBlob.media_type` in
   `serialized_blob()`.
2. Make `encode()` validate every public payload variant, including values
   constructed without helper constructors. Validate byte lengths, string
   lengths, file count and total encoded size before returning bytes.
3. Keep checked arithmetic for every framing addition; do not rely on a
   later append operation to discover an over-limit payload.
4. Preserve the current stable `FCL1`/v1/tag format; no silent format change.
5. Keep decode and encode compatible for every accepted payload. An accepted
   encode must be decodable by the current decoder; rejected values must
   return `CloneError::LimitExceeded` without partial output.
6. Preserve generic error messages with no bytes, names, paths, handles or
   capabilities.

Add core tests for oversized Blob media type and direct public payload
construction. Include a positive boundary case at the maximum accepted
string size and a negative case at `MAX_CLONE_STRING_BYTES + 1`. Repeat the
same checks through the host `clone_blob`/bridge path so the JS binding cannot
reintroduce the asymmetry.

### R4 — P2: redact EnvironmentKey debug output and narrow its API

`EnvironmentKey` derives `Debug` while storing `partition` and `nonce`.
That contradicts the M6 ADR and comments claiming that these host identity
values never appear in debug output. The public `environment_key()` method
makes the value reachable by host code even though it is not exposed to JS.

Fix the identity boundary:

1. Replace derived `Debug` for `EnvironmentKey` with a manual redacted
   implementation. It must not print partition, nonce, or any token-like
   identity value.
2. Prefer removing the public `environment_key()` method as part of R1.
   The handle's `resolve_blob_url(url)` already has the requester identity
   internally and does not need to return it.
3. Preserve `Eq`/`Hash` for internal comparisons and store ownership.
4. Add a regression assertion that `format!("{:?}", key)` contains neither
   the configured partition nor nonce and that no URL/error/tracing string
   contains them.

Do not redact only `EnvironmentDescriptor` while leaving `EnvironmentKey`
with a derived debug representation.

## Required code review pass after fixes

Before updating the handoff, inspect the final source directly and confirm:

- no public method returns the live URL store, raw insertion capability or
  internal environment key;
- every URL creation path (JS and host) shares the same shutdown, environment,
  quota, entropy and collision checks;
- revoke uses the central Web IDL DOMString conversion and preserves the
  specified silent behavior only after successful conversion;
- clone encode/decode apply identical bounds and checked arithmetic;
- all debug/display/error paths are generic and do not reveal partition,
  nonce, URL token, path, capability or OS detail;
- shutdown cannot leave an entry inserted through an exposed side channel.

Do not accept a solution that only changes test expectations or validation
documents.

## Required regression validation

Add exact traceability rows in `docs/spec-matrix.md` for R1–R4 and update the
M6 tests with source anchors. Then run the existing M6 and regression suites,
plus the new cases:

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo test --package boa_fapi_core --test blob_url -- --nocapture
cargo test --package boa_fapi --test m6_blob_url -- --nocapture
cargo test --package boa_fapi --test m6_structured_clone -- --nocapture
cargo hack check --feature-powerset --depth 2
$env:RUSTDOCFLAGS='-Dwarnings'; cargo doc --workspace --no-deps
cargo llvm-cov --workspace --all-features --fail-under-lines 80
cargo deny check
```

If `cargo deny` cannot fetch the advisory database, record `BLOCKED` with the
exact reason; never record it as PASS. CI must run on the final implementation
commit on Ubuntu and Windows. Unix-only filesystem/clone safety paths remain
required in the Ubuntu job.

## Handoff requirements

After fixing R1–R4:

1. Update `docs/m6-validation.md` with the actual command results and new
   regression evidence.
2. Update `docs/reviews/M6-handoff.md` from `REWORK REQUIRED` to a truthful
   re-submission, including the implementation commit and CI run links.
3. Update ADR/spec-matrix text only where it reflects the corrected API; do
   not erase the findings or claim that a test alone fixes the boundary.
4. Run the retrospective bug-find pass again and record any additional
   finding before commit.
5. Stop after handoff. Do not begin M7 in the same change.
