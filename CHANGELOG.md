# Changelog

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
