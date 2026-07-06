# doover

**Every agent deserves a do-over.**

Your agent's checkpoints don't cover bash. Doover does — a session-scoped transaction
layer that snapshots the effects of any AI agent shell command *before* it runs,
journals everything, and gives you a real `undo`. Including files **outside your
workspace**, which no coding harness protects today.

```console
$ claude> "clean up the backup folders"       # agent runs: rm -rf ~/photos-backup
$ doover undo                                 # …and it's back.
```

## Status

**Pre-alpha, build step 0** (scaffold + CI + test harness). Nothing is protected yet.
See `doover-mvp-spec.md` and `doover-implementation-plan.md` for the design, the
test-first build order, and the research this is based on (Atomix, Parallax, YoloFS).

## How it will work

1. A `PreToolUse` hook (Claude Code first; Cursor/Gemini/OpenClaw later) intercepts
   every Bash tool call.
2. An open **reversibility registry** (`registry/`, CC0 data) classifies the command
   and resolves the paths it will affect — including redirect targets.
3. Destructive effects are snapshotted into a content-addressed store using
   copy-on-write clones (`clonefile` on APFS, reflinks on Linux) — near-zero cost.
4. Everything lands in a SQLite journal. `doover log` shows what the agent did;
   `doover undo` restores it, selectively, with conflict detection.

Doover is a **safety net against agent mistakes**, not a security boundary against
a deliberately malicious process.

## Development

`make test` runs the full gate (fmt, clippy, unit, e2e). E2E tests run in throwaway
jails and never touch your real home directory. See `CLAUDE.md` for the working
rules — the first of which is: **no green, no claim**.

## License

Code: Apache-2.0. Registry data (`registry/`): CC0-1.0.
