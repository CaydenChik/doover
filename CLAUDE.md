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
- Exit codes: 0 ok, 1 runtime error, 2 hook block decision, 3 undo conflict.
  (64 not-implemented is retired — every subcommand is implemented as of
  step 8.)

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
- **DONE (round 15): restore is fail-closed on unsafe manifest paths.** `undo`
  is a WRITE primitive fed from on-disk manifests (journal JSON). Restore now
  refuses any entry whose `rel` is non-relative or contains `..`
  (`rel_is_safe`), before any mutation — a corrupt/tampered manifest can no
  longer steer `base.join(rel)` outside the target tree. The hash side was
  already fail-closed (a traversing hash fails content-verify); this closes
  the `rel` twin. NOT claimed as a security boundary — an agent can write
  directly — this is corruption robustness + defense-in-depth.
  STILL OPEN (accepted): `manifest.path` itself (the absolute restore root)
  is unvalidated; a tampered one could aim `remove_any`/rename elsewhere. No
  natural scope exists at the Store layer to check it against, and deleting
  `manifest.path` IS correct undo semantics for an Absent action — same
  non-escalating threat. Revisit only if a scope reference reaches the store.
- **DONE (round 15): gc cutoff arithmetic saturates.** `--keep-days i64::MAX`
  overflowed `keep_days * DAY_MS` (panic in debug, wrap in release). Now
  `saturating_sub`/`saturating_mul` → cutoff floors at i64::MIN (infinite
  window, keeps everything: the safe direction). resolver.rs already
  saturates; this was the only remaining overflow-prone site.
- **DONE (round 14): GC-vs-writer race.** Hooks are separate processes that
  promote a content object into `objects/` and only THEN journal the manifest
  referencing it. A `doover gc` racing that window saw an object no journal
  row vouched for yet and deleted it — stranding the about-to-be-written
  manifest, silent undo breakage. `Store::prune` now takes a `grace_ms` and
  keeps any unreferenced object younger than the window (same guard
  `clean_tmp` gives tmp files); gc passes `TMP_MAX_AGE_MS` (1h). Aged orphans
  (crash leftovers) still collect on a later pass. Fail-safe: an object whose
  mtime is unreadable is kept. This makes gc safe to run WHILE an agent works.
  NOTE for test authors: a backdated "old" action must also backdate its
  object's mtime (the rig's `action_at` does) — an old row with a fresh object
  is a temporal impossibility the grace window will (correctly) shield.
- **DONE (step 7): journal-row pruning + journal-relative retention.** `gc`
  prunes old unpinned/unreferenced rows (secret-bearing `raw_command`) and
  computes the cutoff from MAX(started_at_ms), never the wall clock. Known
  BENIGN asymmetry (intended, do not "fix" by keeping fewer rows): a row kept
  only because an OLD undo still references it can outlive its store objects
  by one gc pass — it is past retention, not user-undoable, and is pruned on
  the next pass. The bias is deliberately toward keeping rows. Undo of such a
  stranded old row must error cleanly (NothingToRestore / missing object),
  never panic or partially restore — the round-6/10 zero-manifest and
  fail-closed-restore guards already cover this.
- **DONE (step 8, hardened round 13): display-time secret redaction.** `log`
  and `show` pass `raw_command` through `redact::redact()`: auth/API-key
  headers, bearer tokens, secret-bearing flags, credential-named env
  assignments, `-u user:pass` basic auth, and `scheme://user:pass@host` URL
  userinfo. The journal keeps the raw string — undo semantics and audit
  ground truth are unchanged. The MIRROR failure is over-redaction: `-u`
  discriminates `user:pass` from `uid:gid` (docker) and port maps, verified
  by test. Pattern-based hygiene, NOT DLP: exotic shapes get through; docs
  must say so. Any future user-facing display of `raw_command` MUST go
  through `redact()`. Verified: the hook protection-gap warnings carry paths,
  not commands, so they are the only other command-adjacent output and do not
  leak.
- **DONE (round 13): `diff` degrades, never lies.** `diff_manifest` returns a
  `DiffReport { lines, partial }`. One unreadable file is `Unreadable`, not a
  fatal abort (informational command must not hide everything over one locked
  file). A root whose identity changed (dir → symlink) is reported and the
  walk STOPS — children are never stat'd through an impostor (misleading
  statuses + unbounded hashing of an unrelated tree). A truncated pre-manifest
  flags `partial`, and the CLI prints "this diff is PARTIAL" — same
  loud-coverage-gap honesty as the round-9 hook path.
- **A completed action can legitimately have zero manifests** (step 6): a
  crash between `start_action` and `attach_manifest`, or a safe/mutating
  action that snapshotted nothing. The undo engine must treat "no manifests"
  as "nothing to restore, warn" — never assume manifests exist for an action.
