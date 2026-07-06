# doover — agent working rules

## Prime directive: no green, no claim

Never state that a task, feature, fix, or milestone is complete unless `make test`
has just been run and exited 0. If tests fail, report the failure output verbatim.
"It should work" is not a result. The CI `honesty-canary` job exists to prove that
failure reporting works; do not remove or weaken it.

## Workflow: tests first

Every build step follows: write/extend tests → confirm they fail (red) → implement →
confirm green → only then claim done. Build order and per-step test gates are in
`../doover-implementation-plan.md`; product behavior is specified in
`../doover-mvp-spec.md`. Do not start step N+1 while step N's gate is red.

## Commands

- `make test` — full local gate: fmt check + clippy (-D warnings) + unit + e2e (bats)
- `make unit` / `make e2e` / `make fmt` / `make clippy` — individual suites
- `make canary` — verifies failures are reported (expects the canary test to FAIL)

## Layout

- `crates/doover-core` — library: registry, parser, snapshot, journal, hooks, undo
- `crates/doover` — CLI binary
- `crates/doover-core/registry/` — reversibility data (YAML, CC0-licensed; code is
  Apache-2.0). Lives inside the crate so `include_str!` embedding survives publish.
- `tests/corpus/parser/` — data-driven parser cases (YAML)
- `tests/fixtures/hook-events/` — golden Claude Code hook payloads
- `tests/e2e/` — bats scenarios; ALL run inside mktemp jails with HOME overridden

## Hard rules

- Never pipe a gate command through grep/tail/head when deciding success —
  the pipe replaces the exit code and a red gate ships as green. Capture to a
  log (`make test > target/gate.log 2>&1`), check `$?`, then read the log.
  This shipped a red commit to main once already.

- E2E tests must never touch the real `$HOME`, `~/.claude`, `~/.doover`, or any
  user data. Fixture jails only.
- NOTICE.md lists unlicensed repos (ccundo, DiffBack) we may study but must never
  copy code from. Clean-room only.
- Unknown/opaque shell constructs must never classify as `safe` — `unknown` or
  stricter. This is a load-bearing safety invariant with property tests behind it.
- Exit codes: 0 ok, 1 runtime error, 2 hook block decision, 3 undo conflict,
  64 not-implemented.
