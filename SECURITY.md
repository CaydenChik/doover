# Security policy

doover is a **safety net, not a security boundary** — it protects against
mistakes, not attacks (see "What doover is not" in the README). That line
matters for what counts as a vulnerability here.

## Reporting

Please report suspected vulnerabilities privately via GitHub's private
vulnerability reporting on this repository (Security → Report a
vulnerability). Do not open a public issue for anything you believe is
exploitable before it is fixed.

This is a single-maintainer project: reports get a best-effort
acknowledgement within a few days and an honest assessment of severity and
timeline. Fixed vulnerabilities are credited in the CHANGELOG unless you
ask otherwise.

## Supported versions

Only the latest 0.2.x release receives fixes. There is no backporting.

## What counts as a vulnerability

- doover **destroying or corrupting data** it promised to leave alone or
  restore (restore/rescue paths, carry machinery, store integrity).
- A **silent protection gap**: a destructive command class that classifies
  `safe` or captures the wrong paths *without* the loud journaled gap the
  design guarantees — i.e. worse than the documented cwd-fallback and
  budget-truncation behavior.
- **Secrets doover itself persists or leaks** beyond the documented
  at-rest posture (journal and store are 0600/0400 under a 0700 home;
  commands are redacted at write time; snapshots are plaintext copies of
  your files by design).
- Anything letting another **local user** read the store/journal or
  influence a restore.

## What does not count (documented design)

- A **deliberately adversarial agent** evading static command analysis.
  doover analyzes commands; an adversary who wants to defeat it can.
- Coverage limits the docs state plainly: unknown commands get a bounded
  working-directory snapshot only; snapshot budgets truncate loudly;
  redaction is pattern-based hygiene, not DLP; `.git` internals and
  remote/external state are out of scope.
- Denial of service against your own machine via pathological inputs to
  your own doover (it fails open and never blocks the agent).

If you are unsure which side of the line something falls on, report it
privately anyway — misclassifying honesty gaps is how tools lose trust,
and we would rather hear about it twice than never.
