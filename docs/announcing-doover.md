# doover: undo for your AI agent's shell commands

A while back a coding agent deleted files of mine while tidying up a
project. Not build artifacts, files I needed, and there was no trash can,
no reflog, no checkpoint. They were just gone.

I am not an unusual case. The Claude Code repo has a data-loss label with
over 400 issues filed against it, more than 120 of them open as I write
this. One user lost around 50GB to an rm -rf on a parent directory
([#49129](https://github.com/anthropics/claude-code/issues/49129)).
Another lost a three-year Unity project through git itself
([#70687](https://github.com/anthropics/claude-code/issues/70687)).
The uncomfortable part is that nothing malfunctions when this happens.
Agents run shell commands all day, and shell commands have no undo. Every
safety layer I thought I had stopped one step short:

- Claude Code's checkpoints rewind edits made through its file tools, but
  commands run through the Bash tool are not checkpointed.
- Sandboxes limit the blast radius to your workspace, which is exactly where
  your files live.
- git protects what you committed. It does nothing for untracked files,
  ignored files (your `.env`, your local database, your test data), or any
  folder that is not a repo. And the agent can destroy uncommitted work
  *with* git: `git checkout .` and `git clean -fd` are one keystroke of
  agent enthusiasm away.

So I built doover. It gives agent shell commands an undo.

## What it looks like

This is real output from the released binary, not a mockup. It comes from
a script (docs/demo.sh in the repo) that drives the exact hook flow Claude
Code drives, so you can reproduce it in ten seconds:

```console
$ doover log
#1     completed  destructive   rm -rf dist/ photos/

$ doover undo
undo of action #1 complete: 2 path(s) restored
  restore .../proj/dist (2 entries)
  restore .../proj/photos (3 entries)

$ shasum -c sums.before
photos/birthday.jpg: OK
photos/wedding.jpg: OK
```

Byte-identical, checked with checksums recorded before the deletion.

## How it works

doover sits behind Claude Code's `PreToolUse` and `PostToolUse` hooks. Before
each Bash command runs, it parses the command with a real bash parser (not
regexes), classifies it against a registry of 152 rules describing what
commands destroy, and snapshots the affected paths into a content-addressed,
copy-on-write store. Then the command runs. A SQLite journal records all of
it, and `doover undo` restores the pre-command state, conflict-checked
against the live filesystem so it refuses rather than clobber later work.

Three tiers of protection, depending on what the parser can prove:

1. Known destructive commands (`rm`, `mv`, `git reset --hard`, `rsync
   --delete`, redirects like `> file`, and so on): the exact affected paths
   are captured, anywhere on disk.
2. Opaque commands (`./cleanup.sh`, `eval`, tools it does not know): it
   snapshots your working directory as a precaution and journals that
   coverage was best-effort.
3. Things no local snapshot can save (`DROP TABLE`, `kubectl delete`,
   force pushes): flagged in the journal as unrecoverable, so at least you
   know.

On copy-on-write filesystems (APFS, Btrfs, XFS) the snapshots share blocks
with the originals, so capturing a large directory costs very little until
something actually changes. A full working session in my testing left a
store measured in tens of kilobytes.

## I pointed a real agent at it

Before releasing this version I ran a live trial: real Claude Code sessions
with the hooks installed, on a git project with source, untracked data,
binary files, and secrets, then tried to break it. Some things that
happened:

- The agent deleted a photos directory after it refused once and I told
  it to proceed. Bare
  `doover undo` picked the right action out of the session history and
  restored both photos, checksum-identical, in 7 milliseconds.
- The agent ran `git reset --hard && git clean -fd`. doover restored the
  uncommitted function and the untracked notes directory git had just
  destroyed.
- An opaque cleanup script deleted a data directory. The working-directory
  snapshot brought it back, and doover's own state survived being inside
  the directory that got swapped.
- The agent emptied a file named `2024` with a bare shell redirect. That
  used to be a blind spot. It is captured now.

The trial also caught a real bug: commands run in the background got a
bogus after-state recorded, which could have steered undo wrong. That was
fixed and shipped before this post went up. I mention it because a trial
that only produces successes is a trial that was not trying.

Overhead, measured on Apple Silicon: about 6 ms before the command and
3 ms after it, so call it 5 to 10 ms per command when nothing needs
snapshotting, about 28 ms when an
unknown command triggers a working-directory snapshot on a small project
(measured on 0.2.1; 0.2.3 no longer walks `.git`, which roughly halves
that number), and single-digit to double-digit milliseconds for restores.
You do not feel it.

## What it is not

- It is not a defense against a malicious agent. doover analyzes commands
  statically, and an adversary who wants to evade it can. It protects
  against mistakes, which is what agents actually produce.
- It is not a backup tool. History is bounded (7 days and 5 GiB by
  default) and lives on the same disk. Keep real backups.
- It cannot undo remote effects. Dropped databases and force-pushed
  branches are gone; doover tells you it happened and that it cannot help.
- `.git` is left to git. Snapshots walk past it, and undo leaves it alone.
- Coverage of commands it cannot parse is your working directory only. A
  script that deletes something in your home directory is outside what
  static analysis can see. The journal says so when it happens.

I have tried to make the failure modes loud instead of silent. When doover
cannot fully protect something, it says so at the time, in the journal and
on stderr, rather than letting you find out during a restore.

## The part you can steal

The registry data (what `sort -o` truncates, which `find` flags write
files, which git subcommands clobber the working tree) is CC0, public
domain, on purpose. If you are building anything that needs to know what
shell commands destroy, take it. If you have mapped a command doover gets
wrong, the project has an issue template for exactly that, and it is the
most valuable contribution you can make.

## Install

macOS and Linux (WSL works, native Windows does not):

```console
$ cargo install doover --locked     # or: brew tap caydenchik/doover && brew install doover
$ doover init
$ doover doctor
```

Everything else is in the [README](https://github.com/CaydenChik/doover).
Apache-2.0 for the code, CC0 for the registry. I would love to hear what
breaks.
