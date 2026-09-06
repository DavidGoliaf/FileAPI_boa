# AGENTS.md — Working Contract for Coding Agents

This file is the entry point for **any** AI coding agent. Different agents may work on different tasks; this contract is tool-agnostic and applies to all of them. If your harness reads a different file name (CLAUDE.md, .cursorrules, etc.), that file only points here.

## Ground rules 

1. One work order at a time. Implement nothing outside it, even if "obvious" or "while I'm here."
2. No `unsafe` in code. Escalate via QUESTIONS.md if you believe a case requires it.
3. `thiserror` in libraries, `anyhow` in binaries; no `unwrap`/`expect` outside tests and documented-infallible cases.
4. Every behavior change ships with trace coverage and tests. CI green is part of "done."
5. New dependency = one-paragraph ADR in `docs/DECISIONS.md` (maintained? widely used? permissive license?) before use. Prefer crates already listed in SPEC §4.
6. Rust edition 2024, latest stable. `cargo fmt` and `cargo clippy --all-targets -- -D warnings` must pass locally before you declare completion.
7. Public items documented; each crate has a README stating its pipeline role.
8. No TODO/FIXME without a tracking entry (issue link or QUESTIONS.md reference).
9. Never commit secrets, snapshots of copyrighted corpus pages, or generated binaries.
10. Retrospective review and bug find process after every completed task before commit. Fix it  

## Workflow

- Branch per work order: `task/m0`, `task/m1`, … Base: `main`. No direct commits to `main`.
- Commits: imperative subject ≤ 72 chars, body explains why; one logical change per commit (e.g. `Add trace session JSON-lines sink`).
- When the work order's acceptance criteria all pass locally, write `docs/reviews/M<N>-handoff.md`: what was built, how to run the demo commands, deviations (should be none) and DECISIONS.md entries added. Then stop and hand off for acceptance — do not start the next work order.
- Rework after review: fix findings on the same branch, update the handoff file, resubmit.

## Definition of Done (every work order)

fmt + clippy clean; tests added and green on both CI platforms; `cargo-deny` clean; trace coverage for new behavior; docs updated; DECISIONS.md updated for any choice made; demo commands from the work order run from a fresh clone with only a Rust toolchain (plus Ollama when the order says so); handoff file written.

## Escalate (stop and ask) when

- Acceptance criteria conflict with each other or with the spec.
- A required crate's current API makes a spec requirement impractical (likely candidate: stylo).
- You need `unsafe`, a build script with network access, or a dependency with a non-permissive license.
- Estimated diff for the work order exceeds ~3000 lines — the order is probably mis-scoped; say so instead of delivering a monster.
