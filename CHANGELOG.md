# Changelog

## 0.2.2 — 2026-08-15

Built on the results of the second live-agent trial (run against released
0.2.1 with real Claude Code sessions).

### Fixed

- **Background commands no longer poison undo.** For `run_in_background`
  Bash commands the harness fires PostToolUse at tool *return* — measured
  live: 4 ms into a 3-second command, with no later event when the task
  finishes. The recorded post-state was therefore a copy of the pre-state,
  which caused false conflicts (training users onto `--force`), made bare
  `undo` skip the action, and made `redo` a no-op. doover now records **no**
  post-state for background actions and journals why; undo's conflict check
  honestly reports "cannot verify (no post-state)" and checks the live
  filesystem instead.
- `doover show` now prints an action's journal notes (protection gaps,
  restore-failure records). `log` marks them with `[notes]`; previously
  nothing displayed their content.

### Added

- **`doover pin <id>` / `doover unpin <id>`** — keep an action and its
  snapshots through every cleanup pass. The backend always honored pins;
  the commands now exist (the docs and gc hints already referred to them).
- Golden hook fixtures for background commands (captured live from Claude
  Code v2.1.232), pinning the at-tool-return contract.
- An e2e scenario driving the restore failure arms and the
  nested-`DOOVER_HOME` layout through the real binary.

### Changed

- README latency claim re-measured end-to-end in the live trial: ~5–10 ms
  per command (was "~4 ms").

## 0.2.1 — 2026-08-15

The restore-hardening release: the remaining confirmed findings from the
2026-08-15 deep-dive, focused on the paths where doover itself could destroy
data. All fixes adversarially reviewed before landing.

### Fixed — restore can no longer destroy what it never captured

- **Failure recovery moves carried directories back.** When a restore swaps
  in the rebuilt tree, live never-captured directories (`node_modules/`,
  build dirs — and now a nested `~/.doover`) are carried across the swap.
  Every failure path used to delete the staging directory WITH those live
  carries inside; now each carry is inventoried and moved back on failure.
  If a move-back itself fails, staging is preserved and the error (and a
  durable journal note) names it and the stranded entries.
- **A nested `DOOVER_HOME` survives undo.** Restoring a working-directory
  snapshot with `.doover` inside it used to delete the live store and
  journal (the capture-side exclusion never applied to the restore side),
  and the undo engine's rollback capture re-ingested the journal into the
  store. The store now derives its own home and — only when it is nested
  strictly inside the restored tree — carries it live across the swap,
  excludes it from rollback capture, ignores it in the conflict oracle
  (both live drift and stale entries in pre-0.2.1 manifests), and prefers
  the live home over any captured copy in a legacy manifest.
- **Honest failure messages.** "The partial restore was rolled back —
  nothing changed, safe to retry" is now said only when the target really
  is intact; a disturbed target or preserved staging is reported as such,
  with recovery instructions, and preserved-staging paths are recorded in
  the journal.
- **Undo refuses when it cannot be transactional**: if a complete rollback
  point cannot be captured (an unreadable file in the tree), the undo is
  refused up front instead of risking a partial rollback on failure.
- **Restoring "this path did not exist" refuses to delete a path that now
  contains the live `DOOVER_HOME`** — the one restore arm that could still
  take the store and journal with it.

### Fixed — protection

- **Glob resolution is time-bounded** (`DOOVER_MAX_GLOB_MS`, default 2s,
  ONE shared budget per command line — per-pattern budgets would stack).
  Scope resolution runs before anything is journaled, and glob's recursive
  walk follows directory symlinks: two cycle-forming symlinks in one
  directory made `**` expansion effectively unbounded — the hook was killed
  by the harness timeout with no journal row at all. Past the budget the
  command is treated as unknown (working-directory fallback, loud gap).
- **Single large files respect the snapshot budget.** The single-file
  snapshot path checked neither size limits nor the deadline, and file
  copy+hash was uninterruptible — one big file on a non-reflink filesystem
  could blow past the hook timeout silently. Ingestion is now chunked with
  per-chunk deadline checks, and over-limit or out-of-time captures
  truncate loudly.
- **One unreadable file no longer forfeits a whole tree's snapshot**: the
  capture continues, the hole is a loud warning, and the manifest is marked
  truncated so partial-restore protection governs it.
- **Redirect targets are scoped like bash treats them**: an all-digit
  filename (`> 2024`) is a real file, not a file descriptor; `>& file`
  with a non-numeric target is the truncating write bash performs; an
  unquoted glob target is expanded (one match = that file; none = the
  literal; several = bash refuses, matches captured anyway).

## 0.2.0 — 2026-08-15

The trial release: everything found by the first live-agent trial
(2026-07-14), four adversarial review rounds, and the 2026-08-15 deep-dive
audit. Users of 0.1.2 should upgrade — the first three items below are
serious.

### Fixed — recovery

- **Undo no longer strands recoverable data behind its own bookkeeping.**
  `doover undo` refused with "cannot be undone: status is Undone" while the
  snapshot sat intact in the store (a forced undo of a later action had
  re-applied the effect). Recovery gates are now answered by the conflict
  oracle against the live filesystem, never by the status column.
- Bare `doover undo` skips actions that changed nothing (and those whose
  before-state already matches the disk) instead of landing on a read-only
  command's defensive snapshot; a truncated destructive snapshot stays
  findable; one unreadable file in one candidate no longer aborts selection.
- Metadata-only destructive commands (`chmod -R 777`) are selectable by bare
  undo (content+mode selection oracle; conflict checking stays content-only).

### Fixed — protection

- **Secrets are redacted before they reach the journal**, not at display
  time. 0.1.2 printed `[redacted]` while `journal.db` held the token in
  plaintext, findable with `strings`. Redaction is bounded (never eats a
  chained command or crosses an argument) and covers headers, bearer/basic
  auth, secret flags, env assignments, URL userinfo and query tokens.
- **The write-via-flag family**: `git grep -O` (arbitrary exec), `git log`/
  `diff`/`show --output=<file>`, `base64 -o`, `file -C`, `find -fprint`/
  `-fprintf`/`-fls`/`-okdir`, `cp -t`, `wget -O`/`curl -o` (truncate their
  target), `sort -o` and friends — all were classified safe or non-capturing;
  all now classify destructive and capture the real target.
- **71 provably read-only rules** (`registry/readonly.yaml`) cut the
  unknown-classification rate from ~70% to ~10%, with admission rules and
  mirror tests preventing the wrapper/output-flag traps.
- **Registry overlays cannot weaken shipped protection**: a different-id
  overlay shadowing `rm` as safe is out-competed by a protection floor;
  same-id downgrades are refused; ties resolve toward the stronger effect.
- A glob matching more files than the 10,000-entry enumeration cap now marks
  the scope unknown instead of silently dropping the excess; the
  working-directory fallback is captured FIRST so a long path list cannot
  starve it of the snapshot budget. Past the budget, the excess is a loud
  journaled gap (2026-08-15 dive).
- A journal failure mid-way through snapshotting no longer forfeits the
  remaining targets or the PROTECTION INCOMPLETE warning (one non-UTF-8
  filename was a deterministic trigger), and an unopenable store degrades to
  the same loud gap instead of skipping the warning block (2026-08-15 dive).
- Re-capturing content that already exists in the store re-promotes the
  fresh copy in place (young mtime, verified bytes), so a concurrently
  running `gc` cannot evict a snapshot a pending action just adopted, and a
  silently corrupted object cannot absorb new captures (2026-08-15 dive).
- A cwd snapshot never ingests doover's own store/journal, even when
  `DOOVER_HOME` nests inside the project.
- One shared snapshot deadline per hook invocation (multiple targets can no
  longer stack budgets past the harness timeout); the installed hook timeout
  is 20s so the 5s budget always finishes with a loud, journaled gap.

### Fixed — CLI honesty

- `doover init` refuses to replace a settings.json it cannot read (an IO
  error used to read as "empty file" and the user's config was silently
  replaced); it also refuses a dangling settings symlink and writes THROUGH
  a healthy one instead of replacing the link. `doctor` says what it could
  not check instead of recommending `init` at an unreadable file
  (2026-08-15 dive).
- `doover diff` reports partial coverage instead of hiding it; `status`
  shows store size against the cap; gc's dry run says what a real run would
  also evict; eviction is never silent.

### Added

- `doover doctor` cross-checks the snapshot budget against the installed
  hook timeout.
- Store size cap (`DOOVER_MAX_STORE_BYTES`, 5 GiB) with absolute eviction
  floors (pins, pending, the last hour, the newest action); automatic gc
  every `DOOVER_GC_EVERY` actions with a 3s budget.
- Regression suites pinning every finding above (`trial_regressions`,
  `review_regressions`, `dive_regressions`, plus resolver corpus/coverage/
  property suites and an 8-session concurrency e2e).

### Known limitations (documented, accepted)

See README "What doover is not" — notably: ownership (uid/gid) is not
captured; opaque commands are covered by a cwd-only snapshot; redaction is
hygiene, not DLP; doover is a safety net, not a security boundary.

## 0.1.2 — 2026-07-13

- Skip regenerable build directories (`target/`, `node_modules/`, …) when
  walking a tree — but only when git also ignores them; a name alone is
  never trusted.
- Tell the user when undo replaces the directory their shell is sitting in.

## 0.1.0 — 2026-07-13

Initial release: PreToolUse/PostToolUse hook engine, bash-AST resolver over
a CC0 reversibility registry, content-addressed CoW snapshot store,
SQLite journal, conflict-checked undo/redo, `init`/`doctor`/`log`/`show`/
`diff`/`status`/`gc`.
