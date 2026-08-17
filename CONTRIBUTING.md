# Contributing to doover

Thanks for considering it. Two kinds of contribution matter most here, and
they have very different shapes.

## 1. Registry rules — the most reusable thing you can send

The reversibility registry (`crates/doover-core/registry/`, CC0) is the
project's knowledge base: what each command destroys and which paths to
capture. Mapping one new command correctly helps every tool that ever
reuses the data.

**The safe-rule trap (read this before anything else).** A `safe` rule is
a promise NOT to snapshot — one wrong entry is silent data loss with no
fallback. History: eight of the first fifteen review findings against the
registry were commands that read by default but write a file via a flag
(`git grep -O` runs its argument as a shell command; `--output` flags
truncate; `file -C` writes a `.mgc`). So:

- Before marking anything `safe`, ask: does ANY flag make it write,
  truncate, or exec? Output flags (`-o`/`--output`/`-O`), output
  positionals (`uniq in out`), in-place (`-i`), compile (`-C`), and
  pager/exec hooks all disqualify the bare form — cover them with
  companion rules.
- **Verify by executing, not by reading man pages**, and on both BSD and
  GNU where behavior can differ. Several plausible-sounding findings were
  refuted only by running the command.
- Never add command WRAPPERS (`env`, `sudo`, `xargs`, `timeout`, `nohup`,
  …) as safe: `env rm -rf /` would inherit the classification and
  snapshot nothing.
- Destructive/irreversible rules MUST capture their targets
  (`snapshot-restore`); external-state destruction (`kubectl delete`,
  `DROP TABLE`) is `externalizing`, never `irreversible`.

Add data-driven cases in `tests/corpus/parser/` with your rule; the
corpus, oracle, and safety suites will hold you to it. Registry
contributions are accepted only as CC0 dedications (that is the point of
the data).

## 2. Code

Working rules live in `CLAUDE.md` — it is the project's institutional
memory (every audit round, every accepted limitation, every load-bearing
invariant) and it is binding for humans too. The short version:

- **No green, no claim.** `make test` (fmt + clippy `-D warnings` + unit +
  e2e) must exit 0 before any "done".
- **Tests first, red first.** Every fix carries a test that failed before
  it. Every bug ever found here lives on as a regression test; yours will
  too.
- **E2E never touches real user data.** Bats scenarios run in mktemp
  jails with `HOME` overridden; keep it that way.
- **Honest messages are load-bearing.** A wrong refusal message stranded a
  user's recoverable data once; "degrade, never lie" is enforced in
  review.
- **Clean-room policy:** `docs/CLEANROOM.md` lists reference-only projects
  you must not copy code from.

Code is Apache-2.0; by contributing you license your work under it
(registry data under CC0, as above).

## Practicalities

```console
$ make test      # the full local gate
$ make unit      # just the Rust suites
$ make e2e       # just the bats scenarios
```

PRs should be small and carry their tests. If you found a command doover
misclassifies, the *Protection gap* issue template captures exactly what
we need even if you don't want to write the rule yourself.
