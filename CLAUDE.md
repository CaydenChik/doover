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

- **DONE (D4): data-at-rest lockdown.** `ensure_private_home()` (hooks.rs)
  forces DOOVER_HOME to 0700 at EVERY creation path (hook open_journal, CLI
  open_journal_or_exit / cfg_journal_store / doctor) — umask-proof, and it
  TIGHTENS a pre-existing loose home from an older install on next run. The
  journal db is chmod 0600 on open (load-bearing, errors surface; WAL/SHM
  sidecars best-effort). Store objects are 0400, never world-readable (they
  are copies of user files). doctor reports "home private (0700)".
  FOR D8 DOCS: state the residual at-rest exposure plainly — the journal
  stores raw commands in PLAINTEXT (bounded by retention gc, masked at
  display); anyone with the SAME user account (or root) can read it.

- **DONE (round 19): all five round-18 leads verified and fixed.** (a) the
  snapshot budget is now ONE shared deadline per hook invocation
  (`slice_limits`/`hook_deadline` in hooks.rs, both handle_pre and
  handle_post) — N targets can no longer stack N×5s past the harness
  timeout; a spent budget truncates later targets immediately and loudly.
  (b) `doctor` cross-checks the effective snapshot budget against the
  installed hook timeout (warns on no-headroom and on unlimited). (c)
  `status` shows store size vs cap with an OVER marker. (d) the stderr
  eviction warning is pinned through the real binary, and the free-low
  rate limiter has a first-fires/second-suppressed behavior test. (e)
  dry-run gc says "would prune".
- **PROCESS RULE (round 19, learned the hard way): mutation testing runs in
  a WORKTREE, never the live tree.** A round-18 verifier agent mutated
  `touch_gc_marker` to a no-op in the real tree, died to a session limit
  before reverting, and the mutation SHIPPED in the round-18 commit (the
  gate was green precisely because that guard was untested — the agent had
  proven its own point). Caught by round 19's new rate-limiter test. The
  `make test` gate now has a `hygiene` tripwire failing on any `MUTATION`
  marker in source, and audit prompts must instruct agents to use isolated
  worktrees for destructive verification.

- **Snapshot limits must apply to ALL scopes, not just the unknown policy.** A
  known-destructive command with a huge scope (`chmod -R / …`) would otherwise
  snapshot unbounded. Step 5 (hook engine) must pass `Limits` to every
  `snapshot()` call and treat truncation as a loud, journaled gap.
- **DONE (bench D1): snapshot has a wall-clock budget, not just file/byte
  limits.** The benchmark showed cost is ~0.19 ms/file and the 10s hook
  timeout was hit at ~50k files → SIGKILL → destructive command proceeds
  UNPROTECTED and UNLOGGED. `MAX_FILES`/`MAX_BYTES` bound storage, not time.
  Fix: `Limits.max_duration` (default 5s via `DOOVER_MAX_SNAPSHOT_MS`,
  fail-safe parse — `0`=unlimited opt-out, garbage/unset=default, never
  silently off) stops the walk and sets `manifest.truncated`, riding the
  EXISTING loud-gap / partial-restore / PARTIAL-diff machinery (rounds 9, 13).
  The installed hook `timeout` was raised 10→20s so the 5s budget + wrap-up
  always wins the race and the loud gap is guaranteed, not probabilistic.
  Budget is checked between entries, so overshoot is bounded by one entry.
- **The unknown-policy fallback snapshots cwd only.** Opaque commands touching
  absolute paths outside cwd (`eval`, function bodies) are only partially
  covered. This is inherent to static analysis — the README/docs must state it
  plainly rather than imply total coverage.
- **DONE (round 16): precise rules for common destructive commands that were
  falling to the cwd-only fallback.** A resolver probe (destructive commands
  with OUT-OF-CWD targets, so a miss can't hide behind cwd coverage) found
  `install` and the `gzip`/`gunzip`/`bzip2`/`bunzip2`/`xz`/`unxz`/`zstd`/
  `unzstd` family had NO rule → Unknown → cwd fallback → an out-of-cwd target
  was silently unprotected. Added precise rules (coreutils.yaml); the probe
  is now the `resolver_coverage.rs` regression test. No guarantee-violating
  bug existed (every destructive command either captured its target or set
  has_unknown), but precise capture strictly beats the lossy fallback.
  ACCEPTED LIMITATIONS (documented, not bugs): `dd of=…` stays `paths: none`
  → cwd fallback (target is `of=`, needs richer flag parsing); `sed -i.bak`
  (attached-suffix form) isn't matched by `flags_any: [-i]` → cwd fallback,
  but GNU sed writes the `.bak` backup so the original survives anyway, and a
  prefix-match fix risks breaking the common `sed -i 's//' file` form.
- **DONE (round 17): fixed MISCLASSIFICATIONS — commands that were `mutating`/
  `externalizing` (no snapshot) but overwrite local files.** The dangerous
  mirror of round 16: a wrong "no-snapshot" class means data loss with NO
  fallback. `wget -O file` (was `mutating`) and `curl -o file` (was
  `externalizing`) truncate an existing target — now `*-output` variants
  classify destructive and capture the target via `path_flags`. `curl -O`/
  `wget` bare stay additive. Added `git.restore`/`git.rm`/`git.switch
  --discard-changes` (working-tree clobberers, were Unknown→cwd-fallback) as
  destructive+repo-scoped like checkout. Audited EVERY `safe`/`mutating` rule;
  `find` was already correct (find-delete/find-exec companion rules exist).
  DELIBERATE TRADEOFF: `curl -o` now reads `destructive` not `externalizing`
  (severity model picks one; Destructive>Externalizing). undo-coverage wins
  over the exfil flag — and the common upload form `curl -d @x URL` (no `-o`)
  still flags externalizing. Revisit if effects ever become multi-valued.
- **`doover` is a safety net, not a security boundary** — reiterate in user
  docs; a deliberately adversarial agent can still defeat static scoping.
- **DONE (D3 corpus audit): long-tail coverage measured, not guessed.** A
  broad corpus probe found ZERO silent misses — nothing destructive/
  irreversible classifies as `safe`; the unknown→cwd-fallback default is a
  solid safety floor, now locked by `resolver_corpus_safety.rs`. Two quality
  fixes on top: (1) `rsync` gets precise dest capture (positional-last) instead
  of the cwd-only fallback; (2) external-state commands (`redis-cli`, `psql`,
  `mysql`, `mongo(sh)`, `kubectl delete`, `docker rm`/`rmi`) are now
  `externalizing` in `services.yaml` — FLAGGED as un-undoable AND no longer
  triggering a pointless full-cwd snapshot of the project for state that isn't
  in the working tree (which the generic unknown path did, and which could burn
  the D1 time budget). MODEL (load-bearing, enforced by
  `every_destructive_or_irreversible_rule_has_an_undo_strategy`): Destructive/
  Irreversible = the "we snapshot before" tier (MUST have snapshot-restore;
  `shred` fits — irreversible but we capture the pre-state); Externalizing and
  below = the "no local snapshot can reach it" tier (undo: none). External
  destruction → `externalizing`, never `irreversible`.
  ACCEPTED (fallback is fine): tar/unzip/patch/perl -i/npm ci/make clean stay
  on the unknown cwd fallback — covers the common in-cwd case; precise capture
  is complex (combined flags, extraction dirs) and low marginal value.
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
- **DONE (D2, revised by a 4-lens adversarial review before commit): store
  size cap + free-space floor + automatic gc.** `DOOVER_MAX_STORE_BYTES`
  (5 GiB default) caps the store's apparent size; over-cap gc evicts oldest
  actions (rows AND objects, same audited pipeline as retention: cutoff →
  live_hashes → prune → prune_before). ABSOLUTE eviction floors: pins,
  pending rows, chain-referenced rows, the 1h hot window, and the journal's
  newest action. LOAD-BEARING constraints from the review — do not "fix":
  (1) the free-space floor (`DOOVER_MIN_FREE_BYTES`, 1 GiB) NEVER drives
  automatic eviction — low disk is usually not doover's fault and deleting
  CoW clones frees ~0 physical bytes; deficit eviction runs ONLY from manual
  `doover gc` where the report is visible. Auto path: rate-limited (10 min
  marker) retention+cap pass + loud stderr warning.
  (2) automatic eviction is NEVER silent: journaled note on the triggering
  action + stderr.
  (3) `DOOVER_GC_EVERY` (50) gates ALL automatic gc; 0 = full opt-out.
  (4) triggered gc carries a 3s time budget (D1 discipline); manual gc is
  unbounded.
  (5) `DOOVER_KEEP_DAYS=0` = retention opt-out (keep forever), NOT "prune
  all" — the knob convention.
  (6) `live_hashes` protects `pending` rows' objects regardless of age (a
  long-running command's pre-snapshot must outlive the eviction window).
  (7) gc's anchor is min(MAX(started_at), now): one forward-skewed timestamp
  must not collapse the hot window (backward-skew rule untouched).
  (8) statvfs degenerate geometry (frsize/blocks == 0) reads None, never
  Some(0) — no phantom disk-pressure emergencies.
  (9) dry-run does not simulate eviction; it measures the same pressure and
  the CLI prints an explicit "real gc would also evict" caveat.
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
