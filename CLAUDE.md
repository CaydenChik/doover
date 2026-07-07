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

## Carried-forward design risks (address at the step noted; do not forget)

- **Snapshot limits must apply to ALL scopes, not just the unknown policy.** A
  known-destructive command with a huge scope (`chmod -R / …`) would otherwise
  snapshot unbounded. Step 5 (hook engine) must pass `Limits` to every
  `snapshot()` call and treat truncation as a loud, journaled gap.
- **The unknown-policy fallback snapshots cwd only.** Opaque commands touching
  absolute paths outside cwd (`eval`, function bodies) are only partially
  covered. This is inherent to static analysis — the README/docs must state it
  plainly rather than imply total coverage.
- **`doover` is a safety net, not a security boundary** — reiterate in user
  docs; a deliberately adversarial agent can still defeat static scoping.
- **Journal rows are never GC'd and `raw_command` may embed secrets**
  (`curl -H "Authorization: ..."`). Step 7's `gc` needs a journal-row
  retention policy (age-based pruning of old sessions), not just store-hash
  GC. Consider redaction patterns for known secret-bearing flags.
- **GC retention must not trust the wall clock** (step 7): a backward NTP
  jump makes recent snapshots look old and collectable — the dangerous
  direction. Compute the cutoff relative to MAX(started_at_ms) in the
  journal, not `now()`, or apply generous slack.
- **A completed action can legitimately have zero manifests** (step 6): a
  crash between `start_action` and `attach_manifest`, or a safe/mutating
  action that snapshotted nothing. The undo engine must treat "no manifests"
  as "nothing to restore, warn" — never assume manifests exist for an action.
