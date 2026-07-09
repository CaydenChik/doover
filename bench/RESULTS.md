# Hook latency benchmark — results & verdict

Failure-mode map **D1** (hook critical path & latency). Harness:
[`hook_latency.py`](hook_latency.py). Re-run: `cargo build --release -p doover && python3 bench/hook_latency.py`.

Numbers below are from one host — **Darwin arm64 (Apple Silicon), APFS, NVMe** —
release binary, 20 timed iterations (+3 warmup). They are machine- and
filesystem-specific; the *shape* of the findings is what matters.

## Data

| scenario | reflink p95 (ms) | force-copy p95 (ms) |
|---|--:|--:|
| safe `ls -la` (no snapshot) | 5.1 | 4.0 |
| 1 file, 4 KB | 4.9 | 4.0 |
| 100 files | 31 | 28 |
| 1,000 files | 240 | 193 |
| 10,000 files | 2,143 | 2,208 |
| 1 file, 100 MB | 77 | 69 |

Timeout-cliff sweep (reflink, `rm -rf d`, median):

| files | median (ms) |
|--:|--:|
| 20,000 | 4,033 |
| 30,000 | 5,909 |
| 50,000 | 9,652 |
| 80,000 | 15,110 |

## Findings

1. **The per-command floor is negligible — ~4–5 ms.** Spawn + JSON parse +
   resolve + journal, paid on *every* Bash command. Imperceptible. Most agent
   commands are read-only/safe and never snapshot; there is no latency reason
   to uninstall for typical use. **Not a blocker.**

2. **File COUNT is the cost driver — not bytes, not reflink-vs-copy.** Cost is
   ~linear at **~0.19 ms/file**. A single 100 MB file is cheap (~70 ms; one
   clone + one blake3 pass). Reflink and force-copy are within noise of each
   other on this host — the per-file syscall/hash/manifest overhead dominates
   the byte movement for the small-file trees that actually blow up.

3. **The 10 s hook timeout is hit at ~50,000 files → the D1 blocker is real.**
   At ~50k files a snapshot reaches ~9.7 s; at 80k it is ~15 s. Past the
   timeout the harness SIGKILLs the hook, and the destructive command then
   proceeds **unprotected and unlogged** — no snapshot, no journaled gap. This
   lands on exactly the highest-value undo case: `rm -rf node_modules`,
   `rm -rf .git`, `git clean` / `chmod -R` over `vendor/`, `target/`, build
   trees — all routinely 30k–200k+ files.

4. **The per-snapshot limits do NOT bound time.** The default `MAX_FILES` is
   100k; a 100k-file tree would take ~19 s, but SIGKILL arrives at 10 s first.
   `MAX_FILES`/`MAX_BYTES` bound *storage*, not *wall-clock* — so the existing
   limits cannot prevent the timeout.

## Verdict

The design does **not** need a general async/deferred mode — the common path is
already fast. It needs one targeted fix on the tail:

- **Add a wall-clock time budget to the snapshot** (e.g. abort cleanly at
  ~5–7 s, comfortably under the harness timeout), journaling a **loud PARTIAL /
  UNPROTECTED gap** — converting a silent SIGKILL-with-nothing-logged into a
  visible, recorded partial. This is the D1 fix and reuses the existing
  truncation/gap machinery (rounds 9, 13).
- **Secondary:** consider a cheap pre-count that warns before starting a
  doomed snapshot; and/or raise the `init`-installed hook `timeout` from 10 s.
  Neither replaces the time budget — a bounded-time snapshot is the honest
  primitive.

Re-run this benchmark on a no-reflink Linux filesystem (ext4) and a slower disk
before launch to confirm the cliff location; the per-file constant will differ,
but the linear shape and the "time isn't bounded" structural gap will not.
