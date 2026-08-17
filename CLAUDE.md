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
- docs/CLEANROOM.md lists unlicensed repos (ccundo, DiffBack) we may study but
  must never copy code from. Clean-room only. (Moved out of NOTICE.md in round 2:
  NOTICE propagates downstream and must carry attribution only.)
- Unknown/opaque shell constructs must never classify as `safe` — `unknown` or
  stricter. This is a load-bearing safety invariant with property tests behind it.
- Exit codes: 0 ok, 1 runtime error, 3 undo conflict. Exit 2 is RESERVED
  (it was specced as "hook block decision"; blocking was retired with
  ask-mode and no code path produces it — if blocking ever returns, the
  fail-open catch_unwind wrapper needs a deliberate carve-out). 64
  not-implemented is retired — every subcommand is implemented.

## The user-#1 trial (2026-07-14) — read this before writing another audit

Twenty-one adversarial audit rounds, a clean 15/15 mutation campaign, and a green
gate. Then ONE live agent used doover on a real machine for ten minutes and found
four bugs, two of them serious. The audits were not sloppy — they were looking in
the wrong place. **Correctness of the parts was never the bottleneck; contact with
reality was.** Prefer one real trial to another audit round.

The headline bug is the one to remember: `doover undo` REFUSED to restore a user's
files ("cannot be undone: status is Undone") while the snapshot sat intact in the
store. A `--force` undo of a LATER action had restored a world state that
re-applied an earlier action's effect, so the row said `Undone` while the files
were gone again. Both directions refused; the user was told their data was
unrecoverable when it was not.

**The lesson, and the rule: the status column records what doover DID. Only the
filesystem knows what is TRUE.** Any gate that refuses recovery must be answered by
the conflict oracle against the live filesystem, never by bookkeeping. If you find
yourself writing `if status == X { return Err(...) }` on a recovery path, stop.

- **DONE (trial): the `Undone` refusal is gone** (undo.rs `select_undo_target`).
  Undo of an undone action defers to the oracle: world == POST → the effect is back
  in force, restore it; world == PRE → genuinely already undone, a clean no-op
  (`UndoReport::already_satisfied`, exit 0, nothing journaled); anything else → a
  real conflict. `record_undo` keeps its in-transaction race guard by default and
  takes `allow_reundo`, which ONLY the engine sets, and only after proving the world
  no longer matches PRE. Two concurrent re-undos would both be admitted and write a
  redundant undo row — benign (restores are idempotent) and vastly preferable to
  stranding a user's data. Pinned by `tests/trial_regressions.rs`.
- **DONE (trial): bare `doover undo` picked a command that changed nothing.** It was
  a SQL `LIMIT 1` on "newest row with a pre-manifest", so it landed on a read-only
  command that had merely been given a defensive cwd snapshot. Now
  `pick_latest_undoable` skips actions whose POST == PRE (changed nothing) and whose
  PRE already matches the live filesystem (nothing to restore). Both predicates are
  about REALITY, not status — which is what lets a KILLED destructive command still
  be found while a killed read-only one is skipped.
- **DONE (trial): redaction moved to WRITE time** (hooks.rs). `doover show` printed
  `Authorization: [redacted]` while journal.db held the bearer token in plaintext,
  findable with `strings`. It was documented — and documenting it did not help,
  because a mask on screen is a promise. **Showing a mask while storing the secret is
  worse than no redaction: it buys false confidence.** Nothing functional reads
  `raw_command` back (undo restores from manifests), so redacting it costs nothing.
  Display still redacts: older journals exist and `redact` is idempotent. Pinned by a
  test that greps the BYTES of every file under DOOVER_HOME, which is how the trial
  found it — checking the deserialized row alone misses the WAL.
- **DONE (trial): the unknown rate.** 43 of 62 commands classified `unknown`, so
  doover walked and snapshotted the whole working directory for ~70% of everything
  the agent ran — `doover log` included. `registry/readonly.yaml` adds 71 provably
  read-only rules; the rate fell to 10% on the same command mix. THIS FILE IS
  DANGEROUS: `safe` means DO NOT SNAPSHOT, so one wrong entry is silent data loss
  with no fallback. Admission rule: the command must not be able to write a file BY
  ITSELF. Redirects don't count (shell.redirect-* captures those independently).
  Deliberately excluded, and DO NOT add them: command WRAPPERS (env, command, sudo,
  xargs, nohup, timeout, nice, watch — `env rm -rf /` would inherit `safe` and
  snapshot NOTHING) and commands taking an output positional or flag (uniq, xxd,
  tree, sort -o). Both exclusions are pinned by mirror tests in trial_regressions.rs.
- **NOT A BUG (trial): "it does not skip build directories."** The trial's `target/`
  was not gitignored, so doover captured it — the gitignore gate being conservative
  exactly as designed. Verified: a real cargo project (whose `.gitignore` has
  `/target`) skips it, 76 KB store, ~580 ms. Do not "fix" this into a name-only skip;
  that is the data-loss path (source in a folder called `build/`) the gate exists to
  prevent.

## The 0.2.0 adversarial review (2026-07-14, round 2) — the safe-rule trap

The trial fixes went through a multi-lens adversarial review before shipping.
It found 15 verified defects, EIGHT of them in `readonly.yaml`, the file added
an hour earlier to reduce over-snapshotting. Every one was a command that reads
by default but WRITES A FILE via a flag — the exact class the file's own header
claimed to exclude. The lesson, now a rule:

**A `safe` rule is a promise NOT to snapshot. Admitting one is as dangerous as
any code change, and reading the man page is not the same as running the command
on both BSD and GNU.** Before marking a command safe, ask: does ANY flag make it
write, truncate, or exec? Output flags (`-o`/`--output`/`-O`), output positionals
(`uniq in out`), in-place (`-i`), compile (`-C`), and pager/exec hooks
(`git grep -O` runs its argument as a shell command) all disqualify the bare
form. Verify by executing, not by reading docs.

- **DONE (review): the output-flag family.** `git grep -O` (arbitrary exec, was
  SAFE → arbitrary `rm`), `base64 -o/--output`, `git log/diff/show/blame/
  diff-tree/rev-list/shortlog --output=<file>` (all truncate the named file),
  and `file -C` (writes `<magic>.mgc`) were all classified safe. Fix: companion
  rules that outrank the safe base rule when the dangerous flag is present. The
  `--output`/`-o` ones capture the file via `path_flags`; `git grep -O` declares
  `-O` value-taking in `flag_args` (so the attached-short form matches) and drops
  the value (it is a command, not a path) → zero captured paths → the
  destructive-with-no-paths fallback marks it unknown → cwd snapshot. Finding 7
  was PRE-EXISTING: `git.log`/`git.diff`/`git.show` in git.yaml had the same
  `--output` hole before readonly.yaml existed. All verified end-to-end:
  classified destructive AND recovered byte-identical against the 0.2.0 binary.
- **DONE (review): write-time redaction ate the command tail.** The
  Authorization/X-API-Key patterns matched `[^"'\\]+` — to the next quote or END
  OF STRING. For an unquoted header value (legal curl) that was the whole rest of
  the command, so redacting at WRITE time (the trial fix) permanently destroyed
  the destructive tail (`... && rm -rf ./build`). Fix: bound the value at
  whitespace, with an optional scheme word (`Bearer `/`Basic `/…) so a
  space-bearing token still fully masks. Redaction now never crosses into a
  following argument or command separator. redact() idempotence pinned.
- **DONE (review): two bare-`doover undo` selection bugs.** (8) `world_matches`
  propagated a `state_matches` IO error, so one unreadable file in a candidate's
  snapshot aborted the whole scan with a cryptic `io error … Permission denied`
  while a recoverable action sat one candidate deeper. Now an unreadable or
  truncated candidate is `Indeterminate` → skipped, never propagated. (9) the
  "nothing to restore" check used content-only `state_matches`, so a
  metadata-only destructive command (`chmod -R 777` exposing a key) looked
  already-undone and bare undo said "nothing to undo". New `state_matches_mode`
  (content + mode) is used ONLY by selection; the conflict oracle stays
  content-only on purpose. `Restorability` {InForce, AlreadyDone, Indeterminate}
  makes the three cases explicit.
- **DONE (review, low): the loop-refusal message named a non-working id for redo
  rows** (walk to the ultimate command action now); the "no snapshot found"
  message denied snapshots that the 128-scan cap merely hid (reworded); and
  `human_bytes` rendered `1024.0 KiB` at band tops (promote on rounded mantissa).
- **REFUTED by verifiers (do not re-add): `less`/`more` `-o`/`-O` (log file is
  interactive-only), and several undo-state-machine claims that the conflict
  oracle already handles.** The verify pass killed 6 of 21 raw findings; trust it
  but confirm anything that touches a snapshot decision.
- **TEST GAP the review exposed: `path_flag_rules_resist_all_value_forms` built
  its synthetic command WITHOUT the subcommand,** so it silently tested nothing
  for subcommand-scoped rules. Now subcommand-aware — it would have caught a
  broken `git … --output` rule.

## Round-2 re-audit (2026-07-14) — findings fixed, and two flagged

A second comprehensive adversarial audit of the 0.2.0 diff (plus my own
independent sweep) fixed seven more issues and flagged two niche ones. Every fix
was verified red-first by reverting it in a worktree and watching its test fail.

- **DONE: `find` write-primaries were classified safe** (posix.yaml). Found by
  the exhaustive re-audit AND independently by the review lens. `find -fprint /
  -fprint0 / -fprintf / -fls <FILE>` truncate FILE (confirmed on BSD find). The
  bare `posix.find` rule is safe and only `-delete`/`-exec` were companioned.
  New `posix.find-fprint` captures the target via `path_flags`; verified
  destructive + recovered byte-identical. The SAME write-via-flag class as the
  readonly.yaml holes, hiding in a pre-existing rule — so re-audit EVERY safe
  rule when adding neighbours, not just the new file.
- **DONE: resolver path-flag consume was "sticky"** (resolver.rs). `--output
  -weird` set a pending Path-consume that a flag-shaped value (`-weird`) did not
  claim, so it latched onto a LATER positional — capturing the wrong file and,
  because a (bogus) path WAS captured, defeating the destructive-with-no-paths
  cwd fallback. The real write target went unprotected. Fixed by honoring a
  pending consume at the top of the token loop (a value claims the next token
  even if flag-shaped). This was a GENERAL bug affecting every path_flags rule.
- **DONE: redaction had unbounded siblings of the finding-10 fix** (redact.rs).
  The finding-10 fix bounded the Authorization rules but left the secret-flag,
  `-u` basic-auth, and unquoted-header value captures using `\S+`/`[^"'\\\s]+`,
  which (a) ate a chained `&&rm`/`|tee`/`;rm` into the mask (audit-record loss)
  and (b) leaked a token after a backslash-escaped space
  (`Authorization:Bearer\ TOK`). Introduced one shared `CRED_VALUE` class
  (escape-aware, stops at shell metacharacters `\s"'\\$&|;<>()\``) used by every
  credential-value capture, reordered the bearer rule before the header rules,
  and added URL-query masking (`?api_key=`/`&token=`). NOTE: redaction bugs are
  never a data-loss or recovery risk — `resolve()` runs on the RAW command, so
  the snapshot/effect/rule_id/manifest are all raw-derived; the stakes are a
  persisted secret or a mangled audit string only. Do not over-invest, but keep
  every value bounded.
- **FLAGGED (not fixed) — ownership is not captured (F3b).** `EntryKind` stores
  content/mode/mtime/xattrs but NOT uid/gid. So `chown`/`chgrp` (destructive,
  snapshot-restore) captures content but cannot restore ownership, and an
  ownership-only change has PRE==POST manifests so bare `doover undo` skips it.
  DATA is always safe (content captured). Fixing needs a manifest-schema change
  (add uid/gid) plus a privilege-sensitive best-effort restore chown — deferred
  as niche (mostly root containers). Documented in README "What doover is not".
  If you add it: gate the uid/gid comparison on manifest schema version so old
  manifests (default 0) don't cause false conflicts, and make restore's chown
  best-effort (EPERM as non-root is expected, warn don't fail).
- **FLAGGED (not fixed) — bare undo skips an xattr/mtime-only change (F3a).**
  The selection oracle (`state_matches_mode`) compares content+mode; xattr and
  mtime are captured and ARE reverted by targeted `doover undo <id>`, but a
  change in only those fields isn't auto-selected by bare undo. mtime
  deliberately cannot join the selection oracle (nearly every command bumps it →
  bare undo would target the wrong action constantly); xattr-only changes
  (setfattr) are vanishingly rare and fully recoverable by explicit id. Left as
  a documented, content-safe UX gap.
- **REFUTED by verifiers (confirmed, do not re-add):** `less`/`more` `-o`/`-O`
  (does not write non-interactively — verified: left the target untouched),
  `column -o` (sets an output SEPARATOR string, not a file). Reading a man page
  suggested a write; running the command proved otherwise.

## Round-3 audit (2026-07-14) — trial-fix verification + new regressions

A third audit verified every ORIGINAL trial finding (T1-T8) still holds end-to-
end through the real binary, and found more issues (two I had just introduced).
All fixes verified red-first by reverting in a worktree.

- **DONE: `find -okdir` classified safe** (posix.yaml). The exec companion listed
  -exec/-execdir/-ok but missed -okdir (the -execdir variant with a prompt), so
  `find … -okdir rm {} ;` classified safe → no snapshot. Added -okdir. Two lenses
  found this independently.
- **DONE: a TRUNCATED destructive snapshot was skipped by bare `doover undo`**
  (undo.rs). A regression I introduced with the finding-8 fix: `restorability`
  short-circuited `truncated → Indeterminate`, and bare undo skips Indeterminate
  — so a large `rm -rf` whose snapshot hit the file/time limit made bare undo
  report "nothing to undo". Fix: truncated → InForce (never AlreadyDone, since
  the uncaptured part is unknown; offer it and let undo() apply the round-18
  refuse-by-default). Indeterminate is now ONLY a read error.
- **DONE: redaction leaked Proxy-Authorization / -H-glued / X-Amz-Security-Token
  / Cookie / Basic** (redact.rs). Pre-existing gaps, but common: broadened the
  header-name pattern to `(?:proxy-)?authorization|cookie|x-…-(key|token|secret)`,
  dropped the `\b` (so `-HAuthorization:` glued form is caught), and added Basic
  to the bearer rule. Redaction is hygiene not DLP and never affects recovery
  (resolve uses the raw command), but a write-time leak persists on disk.
- **DONE: `cp -t/--target-directory` captured a source, not the destination**
  (coreutils.yaml). cp used `paths: positional-last`; with -t the dest is a flag
  value and positionals are sources, so it snapshotted a source and missed the
  overwritten target. Added the flag as a path_flag. (mv/install use `paths:
  positional`, so their -t value is already captured — only cp needed it.)
- **DONE: added the MISSING tests the audit named.** (a) the headline T2 trap now
  has an END-TO-END CLI test through the real binary (`cli_force_undo_…`), not
  just the engine unit test. (b) the `already_satisfied` no-op CLI branch is
  covered (`cli_re_undoing_…`). (c) the finding-8 test had gone partly VACUOUS —
  the finding-9 mode-aware change made `chmod 000` resolve via the mode mismatch
  before reaching the read-error that yields Indeterminate, so that branch was no
  longer exercised. New macOS-gated test constructs a genuine Indeterminate (a
  deny-read ACL keeps mode bits but blocks read) and pins continue-past-it.
- **FLAGGED (pathological, not fixed): a file literally named `--`.** `sort -o --
  input` / `git log --output -- x` — the shell/arg `--` terminator interacts with
  path-flag capture so the real target `--` isn't captured. A file named `--` is
  bizarre and the fix (teach the tokenizer that `--` after a path-flag is a value,
  not a terminator) is complex for zero real-world benefit. Left as a known LOW
  limitation.
- **NOTED (not a defect): `redaction_does_not_leak_nonstandard_auth_schemes` also
  passes on the 0.1.2 baseline.** It is red-first against the BROKEN intermediate
  (scheme-list) commit — which is what matters — and correctly green on both the
  old-correct and new-correct code. A regression test passing on old-correct code
  is right, not dishonest.

## The 2026-08-15 deep-dive (29-agent audit + adversarial review of the fixes)

A full-repo dive (8 subsystem maps → 5 hunt lenses → triage → 14 adversarial
verifications, report in `../doover-deep-dive.md`) confirmed 14 findings; the
four smallest/highest-value were fixed the same day, and the fix diff itself
went through a 6-lens adversarial review (14 more findings, 6 refuted) before
commit. Pins live in `tests/dive_regressions.rs` + additions to
`tests/hooks.rs` / `cli.rs`.

- **DONE: glob >10k-match cap marks unknown** (resolver.rs) — the excess used
  to be silently unprotected while `contributed>0` defeated the no-paths
  fallback; the brace cap always had the correct pattern. The review then
  showed the routed fallback was STARVED: targets share one deadline and the
  cwd fallback was ordered last, after 10,000 single-file snapshots. The cwd
  fallback is now FIRST in the target order. ACCEPTED (loud-gap tier):
  beyond-cap coverage is budget-bounded best-effort — the guarantee is the
  journaled gap, not a snapshot.
- **DONE: dedup re-promotes by rename** (snapshot.rs ingest_via). Adopting an
  existing object used to leave its old mtime, so gc's round-14 grace window
  (mtime-based) could not shield a pending action's just-adopted hash — the
  exact mirror of round 14's "old row with a fresh object" assumption. The
  fresh clone now renames over the existing object (atomic, young mtime,
  known-good bytes — also stops a corrupted object absorbing new captures).
  ACCEPTED residual: a gc pass already between its mtime stat and its unlink
  can still reap the object; closing that needs store-wide locking.
- **DONE: journal/store failure mid-hook degrades, never unwinds** (hooks.rs).
  `note_gap(..)?`/`attach_manifest(..)?`/`Store::open(..)?` after
  start_action used to abort remaining targets AND the PROTECTION INCOMPLETE
  block — one non-UTF-8 filename (serde refuses non-UTF-8 paths) forfeited a
  whole action's protection on Linux. Gaps now flow in-memory even when
  add_note fails (summary line says `doover log` may be incomplete — "some or
  all", a sticky bool cannot know which); attach failures become explicit
  gaps; the post loop finishes all paths and surfaces the first error after.
  Test injection: `.doover-test-journal-ro` marker in DOOVER_HOME, DEBUG
  BUILDS ONLY, consumed on read + loud stderr line (an agent-planted marker
  degrades one invocation visibly, never silently forever). The real
  non-UTF-8 trigger is pinned Linux-gated — it compiles/runs only in the
  ubuntu CI leg, never on the macOS dev machine.
- **DONE: init IO-error and symlink honesty** (main.rs). An unreadable
  settings.json read as empty via `unwrap_or_default()` and was atomically
  REPLACED with a doover-only file (un-undoable — init is not a hook path),
  while doctor read the same failure as "hooks not found — run `doover
  init`". Reads now refuse on any non-NotFound error; a dangling settings
  symlink refuses (read follows links, so it looked like NotFound and the
  rename destroyed the link); a healthy symlink is written THROUGH
  (canonicalize before write_atomic); doctor reports what it could not read
  and never recommends init at it.
- **DONE (Phase C, same day): all five queued items landed as 0.2.1**, each
  design pressure-tested by probe agents before implementation, each pinned
  in `dive_regressions.rs`. (1) restore keeps a carry INVENTORY: every
  carried live dir (skipped build dirs AND a nested DOOVER_HOME) is moved
  back on any failure arm; a failed move-back PRESERVES staging and the
  error + a journal note name it (`RestoreTargetDisturbed` /
  `RestoreStagingPreserved`; "nothing changed" is now said only when true).
  Injection: `.doover-test-restore-swap-fail` / `-moveback-fail` markers in
  the store root (debug-only, single-shot, loud — the journal-ro precedent).
  (2) Store derives `home = root.parent()` at open; nested-home protections
  are GATED on home-strictly-inside-target (ungated they would blind the
  oracle for every home-contains-target layout — probe amendment 1): undo
  rollback captures via `snapshot_rollback` (home-excluding), restore
  carries the live home FIRST and voids under-home entries in legacy
  manifests (live home beats stale captured copy), compare_state skips home
  on BOTH live and expected sides. (3) redirect scoping: all-digit targets
  are real files (fd forms ride brush's Duplicate arms), `>&file`
  non-numeric = write, glob targets expand (1 match = that file, 0 =
  literal, >1 = bash refuses; matches captured anyway); corpus cases added.
  (4) ingestion is CHUNKED with per-8MiB deadline checks (copy and hash;
  reflink clones hash-bounded too) and the single-file root now respects
  max_bytes + deadline; per-file ingest errors warn-and-continue and mark
  the manifest TRUNCATED (refuse-by-default governs). (5) CONFIRMED by
  probe (symlink cycles under `**` branch k^32 — 66s CPU on a 4-entry dir;
  relative `ln -s .` links are the pathological form): glob expansion runs
  on a detached walker thread under `DOOVER_MAX_GLOB_MS` (2s default,
  fail-safe parse, 0=unlimited); expiry keeps received paths + marks
  unknown. Per-item deadline checks were proven UNSOUND (a zero-match walk
  completes inside the iterator's first next()).
  ACCEPTED residuals (documented, do not "re-fix" blindly): gc's
  stat-to-unlink instant vs dedup re-promotion; lexical home compare under
  relative DOOVER_HOME; rescue-recreated parent dirs get default modes; an
  abandoned glob walker does read-only IO until the hook process exits;
  rollback capture remains unlimited (complete-or-refused, tracked
  separately).
- **DONE (Phase C review round, same day): a 6-lens adversarial review of
  the 0.2.1 diff confirmed 27 findings; the serious ones fixed before
  commit.** (a) the glob budget was PER-EXPANSION and stacked (N globs x
  2s each — the round-19 "budgets must not stack" blind spot reborn in my
  own fix): now ONE deadline per resolve(), minted on the Walker. (b)
  rollback capture silently lost complete-or-refused when per-file errors
  began degrading: a TRUNCATED rollback point now refuses the undo up
  front (NotUndoable). (c) home nested inside a skipped build dir: the
  home-first carry pre-created the skipped dir's staging slot → ENOTEMPTY
  on every retry, a never-converging restore for a layout that worked in
  0.2.0; the home now rides inside the wholesale skipped-dir carry. (d)
  the Absent-root restore arm bypassed the whole nested-home gate →
  RestoreWouldDeleteHome refusal. (e) dir-skeleton truncated manifests
  (every ingest failed) were permanently InForce and hijacked bare-undo
  selection → skipped in SELECTION only; explicit id keeps round-18
  refuse-by-default. (f) smaller: rescue uses symlink_metadata (dangling-
  symlink carries survive) and carries move-back errnos in the error;
  write_in_place mid-write failure reports Disturbed, never "nothing
  changed"; StagingPreserved during a ROLLBACK restore is journaled too;
  invalid-glob redirect targets keep write-deref; SnapshotError is now
  #[non_exhaustive] (three variants were added in a 0.x patch bump).
  ACCEPTED/DEFERRED from the review (revisit deliberately, not
  accidentally): doctor's orphaned-staging scan looks in the store but
  staging lives beside the restore TARGET — discovery is inert; the
  durable journal notes are the record for now. `doover diff` still
  displays nested-home entries of legacy manifests (display-only,
  degrade-not-lie unaffected). copy_hash_bounded has no byte cap beyond
  the stat-time max_bytes check (a TOCTOU-grown file is bounded only by
  the deadline). The mid-file Budget arm and glob write-deref through a
  matched SYMLINK are unpinned (timing/fixture cost). chmod-000 rigs
  assume a non-root runner. No bats/e2e scenario exercises the restore
  failure arms or nested homes — add one with the debug markers when e2e
  is next touched.
- **PROCESS: 0.2.0 shipped with audits but no second real-agent trial.** The
  user-#1 lesson stands: contact with reality, not audit rounds, finds the
  bugs that matter. Run a live trial on the released build before trusting
  this round's fixes.

## The 0.2.x trial (2026-08-15) — second real-agent trial, on released 0.2.1

Six scenarios driven by REAL headless Claude Code sessions (claude -p, live
hooks, crates.io binary) against a git workspace with DOOVER_HOME NESTED in
the cwd. Full report: ../doover-trial-2026-08-15.md. Every recovery was
byte-verified against pristine mirrors; every CLI surface exercised.

- ALL PASSED: precise rm undo (7ms, byte-identical past 3 later safe
  actions); `> 2024` classified destructive live (the 0.2.1 fix working);
  whole-cwd undo with the live nested .doover CARRIED across the swap and
  the journal functional afterward; git reset+clean fully reverted
  (uncommitted + untracked back); chmod-only found by mode-aware bare undo,
  redo/re-undo/already_satisfied clean; conflict exit 3 + --force; gc.
- **CONFIRMED live: run_in_background POST==PRE** (the dive's premise-
  caveated HIGH). Background action journaled duration_ms=9 for an 8-second
  script — PostToolUse fires at tool RETURN, POST is a copy of PRE, no
  corrective event. FIXED in 0.2.2 (Phase D, below) — the shipped design
  records NO post-state, journals a v3 `background` flag, and bare undo
  skips background actions by design.
- Harness note: in headless -p mode a run_in_background child DIES at
  session exit (the deletion never landed); interactive sessions are where
  the full hazard lives.
- Latency: safe ~6ms pre + ~3ms post (incl. spawn), unknown 86-file cwd
  walk 28ms, undo 7-49ms. README's "~4 ms" is ballpark-honest but "~5-10ms
  per command" would be truer.
- Agent-behavior datum: the agent REFUSED rm of photos/ until told, spared
  gitignored files, and warned about .doover before offering clean -fdx.
  The residual risk doover covers is user-confirmed mistakes and opaque
  scripts — both were the scenarios that needed recovery.

## Phase D (2026-08-15) — 0.2.2: background undo, pin/unpin, and its review round

The trial's confirmed HIGH fixed, plus the phantom pin feature — then the
diff's own 4-lens adversarial review confirmed 14 findings (0 refuted),
headlined by a HIGH design flaw in the first draft of the background fix.

- **DONE: background commands (run_in_background).** Wire ground truth
  captured live from harness 2.1.232 (fixtures + README finding #6): the
  flag rides in tool_input on BOTH events, the post fires at tool RETURN
  (duration_ms=4 for a 3s command), no completion event ever follows.
  handle_post records NO post-state + a journaled explanation. THE REVIEW
  CAUGHT THE FIRST DRAFT: with no POST, the changed-nothing filter died and
  bare undo OFFERED background no-ops (dev servers), dead-ending users in a
  --force whose execution reverts post-launch work. Shipped design: journal
  schema v3 adds a `background` column (threaded from the pre event);
  `undo_candidates` excludes background rows so bare undo skips them BY
  DESIGN; explicit `undo <id>` reaches them with a precise refusal (never
  "may have failed" — it says the outcome is unverifiable by harness
  design, that forcing reverts later changes too, and that a forced undo
  cannot be redone), a forced-undo warning, and a background-aware redo
  error. Pinned at engine level (skip + shadowing + honest messages) and
  fixture level (background fixtures assert the parsed flag; floors bumped
  6 pre/4 post/10 files so deleting them breaks the gate).
- **DONE: pin/unpin CLI** over the existing backend; `log` gained a
  [pinned] flag (gc's "unpin" advice finally has an enumerable surface);
  README documents the commands; unpin-RELEASES-to-gc is now pinned by a
  maintenance test (survival alone was).
- **DONE: `show` prints journal notes** (through redact(), round-13 belt) —
  the durable gap/failure records were previously written but unreadable.
- **DONE (review honesty round): the background note says exactly what
  happens** (an earlier draft promised a live-filesystem fallback that does
  not exist — same defect class as the round-13 mask lesson); note-write
  failure is loud; S10b pins the exit-3 conflict BEFORE --force; CHANGELOG
  rewritten to match reality.
- **Process note: this is the third consecutive review round to catch a
  serious flaw in freshly written protection code (Phase A: 14, Phase C:
  27, Phase D: 14 findings). The review-before-commit rule has earned
  permanence.**

## Dive round 2 (2026-08-15) — trust lenses on the SHIPPED artifacts → 0.2.3

Four lenses (supply chain, licensing-of-artifacts, performance-under-load,
racing undos) run against the published crates/tarballs and the released
binary; 17 confirmed findings, 1 refuted. Full reports in the session
scratchpad; upstream drafts in ../doover-upstream-*.md (maintainer files).

- **DONE: restore cross-process flock** (Store::lock_restores, held across
  the whole engine mutation sequence). Racing undos were REPRODUCED
  destroying live carried dirs — racer B saw skipped dirs absent (in A's
  staging), carried nothing, swap-deleted A's restored tree; also fixed the
  nondeterministic double-undo depth lottery. Loser gets RestoreInProgress.
  kernel-owned (no stale locks). Racing probe also VERIFIED live: the
  round-1 'two concurrent re-undos benign' claim, and undo-vs-live-hook
  safety (conflict refusal; forced mid-flight undo clean).
- **DONE: .git always walked past** (SkipPolicy, before the name/gitignore
  gate; root-targeted .git still captured). Unknown-command tax was 2x the
  documented model with .git included; undo no longer rewinds git state
  behind git's back; oracle ignores .git drift (false-conflict class gone).
- **DONE: journal bytes count toward DOOVER_MAX_STORE_BYTES** (measured 26x
  the store, ~2MB/100 commands, previously unbounded by any cap) and
  idx_actions_target (created idempotently at open) flattens the inline
  auto-gc pause that grew with retained manifests.
- **DONE (licensing):** LICENSE+NOTICE now packaged in both crates
  (published 0.2.x crates shipped NEITHER — Apache §4.1/4.4 gap);
  doover-core license = 'Apache-2.0 AND CC0-1.0'; NOTICE.md rewritten as a
  real notice (clean-room policy → docs/CLEANROOM.md; the untrue
  binpash/try 'we vendor' claim corrected to future-tense-conditional);
  THIRD-PARTY-LICENSES.txt generated (scripts/third_party_licenses.py,
  178 packages) and shipped in binary tarballs.
- **DONE (supply chain):** README installs --locked (unlocked drifted 49
  deps off the audited set incl. build-script crate cc — verified by
  running both installs); release.yml gained an audit gate (tags shipped
  unaudited if an advisory landed after the last push); ci gained an
  informational Linux bench job (F5: zero Linux numbers existed).
- **UPSTREAM (maintainer to file):** tree-sitter-bash segfault report
  (finally drafted) and brush-parser's accidental runtime `insta` dep
  (build-surface + lockfile advisory exposure; linker dead-strips it).
- **DONE (round-2 review of the 0.2.3 diff itself: 15 confirmed, 1 refuted;
  the serious ones fixed pre-commit).** (a) GS1: describes_same_state now
  compares skipped_dirs — PRE-skipped-but-POST-gone means the command
  DELETED the dir; ignoring that read opaque `rm -rf .git` as "changed
  nothing" (this also closes the original phase-1 risk note). Restore warns
  loudly when a skipped dir no longer exists ("restore CANNOT bring it
  back"). (b) CIP-1: journal-in-cap would have been PERMANENT pressure —
  SQLite DELETE never shrinks files; gc now VACUUMs after pruning rows
  (retention and cap arms) + wal_checkpoint(TRUNCATE), also fixing the
  WAL-inflation measurement bias (CIP-3). status/gc wording matches the new
  cap semantics (CIP-2). (c) RL-1: the restore lock is taken BEFORE the
  conflict oracle (a verdict computed outside the lock could be stale by
  mutation time — silent overwrite of a racer's finished restore). (d)
  RL-2: only EWOULDBLOCK maps to RestoreInProgress; other flock errnos
  (ENOTSUP/ENOLCK on network mounts) surface as real IO errors instead of a
  phantom "wait for the other restore". (e) GS5: the .git skip requires
  dir/HEAD to exist — a data dir coincidentally named .git keeps the
  capture the gitignore gate protects. (f) messages: .git carry says "left
  to git", never "(regenerable)". (g) THIRD-PARTY licenses script walks the
  resolve graph (normal+build edges, per release target) — dev-deps and
  foreign-platform crates no longer misattributed; a missing registry dir
  is a loud error, not silent "(no license file)"; insta legitimately
  remains until upstream fixes brush (see the draft issue). LAUNCH.md
  sanity-check installs --locked.
  ACCEPTED from this round (recorded, low): GS2 — .git's unversioned files
  (config/hooks/info-exclude) lost their only net with the skip; the
  documented tradeoff for the 2x walk tax, revisit if real losses appear.
  GS3 — rollback capture skips .git so legacy-manifest undo restores .git
  while a failed-undo rollback carries it (asymmetric but coherent). RL-4 —
  gc row-eviction racing the restore-to-record window strands an
  unrecorded-but-applied undo (narrow, loud). GS4(2) — undo of repo-scoped
  git commands leaves tree/HEAD divergence to `git status`.
- **ACCEPTED/known:** dedup flattens SPACE not TIME (unchanged-tree unknown
  cost never amortizes — consequence of the round-2 gc-grace rename fix; do
  not 'optimize' it away without reading that entry); loss-braid messages
  from pre-lock races are moot with the lock; journal growth is bounded by
  retention+cap now but remains manifest-dominated (compression is future
  work).

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
- **DONE (round 21): overlay SHADOW attack fixed.** The same-id downgrade
  guard was insufficient — a DIFFERENT-id overlay rule matching the same
  command (e.g. `aaa.rm` effect:safe) out-competed shipped `coreutils.rm` at
  lookup and made `rm -rf` classify SAFE with no snapshot, silently. Fix:
  Registry tracks `shipped_count`; `lookup_command` enforces a PROTECTION
  FLOOR — the resolved effect is never weaker than the shipped-only registry
  would give for that command, regardless of id/score/tie-break. Lookup
  tie-break also now favors the STRONGER effect (a protection tool must not
  resolve ties toward the safer class). Load-time warning when an overlay
  shadows protected shipped protection. Legitimate use preserved (new
  commands, upgrades). Pinned by 4 registry tests.
- **DONE (round 21, closes the D7 self-snapshot open item): a cwd snapshot
  never ingests DOOVER_HOME.** With the store/journal nested inside a project,
  the defensive cwd snapshot captured doover's own internals (13/15 entries)
  — recursive bloat AND capture of the secret-bearing journal into a
  snapshot. `Store::snapshot_excluding` prunes any subtree under DOOVER_HOME
  (lexical prefix, symlink-resolution intentionally avoided so identity games
  can't dodge it); both hook loops pass `cfg.doover_home`. Pinned by a
  regression test proven red-without-fix.
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
