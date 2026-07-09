#!/usr/bin/env python3
"""
doover PreToolUse hook latency benchmark.

Measures the REAL cost paid on the agent's critical path: the full
`doover hook pre` process — spawn + JSON parse + resolve + snapshot + journal —
against fixture trees of varying size, on both the reflink (copy-on-write) and
the force-copy fallback paths.

Why it matters (failure-mode map D1): this hook runs before every Bash command.
If the tax is perceptible, users uninstall; if a snapshot approaches the 10s
hook timeout, the harness SIGKILLs it and the action proceeds unprotected.

Run:  python3 bench/hook_latency.py
      (build first: cargo build --release -p doover)

Numbers are machine- and filesystem-specific. Always report the host FS.
"""

import json, os, shutil, statistics, subprocess, sys, tempfile, time

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BIN = os.path.join(REPO, "target", "release", "doover")
ITERS = 20          # timed iterations per scenario
WARMUP = 3          # discarded leading iterations
FILE_BYTES = 4096   # per small file
BIG_BYTES = 100 * 1024 * 1024  # single large-file scenario

# (label, command, fixture-builder) — builder returns the relative path the
# command targets, created under the workspace.
def build_files(root, rel, n, each=FILE_BYTES):
    d = os.path.join(root, rel)
    os.makedirs(d, exist_ok=True)
    blob = (b"doover-bench-payload-" * 64)[:each]
    for i in range(n):
        with open(os.path.join(d, f"f{i:05d}.dat"), "wb") as fh:
            fh.write(blob)
    return rel

def build_one_file(root, rel, size):
    chunk = (b"x" * (1024 * 1024))
    with open(os.path.join(root, rel), "wb") as fh:
        written = 0
        while written < size:
            fh.write(chunk)
            written += len(chunk)

def build_single_small(root):
    with open(os.path.join(root, "f.dat"), "wb") as fh:
        fh.write((b"doover-bench-payload-" * 64)[:FILE_BYTES])

# builders create the fixture the command targets; their return value is unused
# (the command string carries the path). All paths are relative to cwd.
SCENARIOS = [
    ("safe: ls -la (no snapshot)",   "ls -la",     lambda r: None),
    ("destructive: 1 file (4 KB)",   "rm f.dat",   build_single_small),
    ("destructive: 100 files",       "rm -rf d",   lambda r: build_files(r, "d", 100)),
    ("destructive: 1,000 files",     "rm -rf d",   lambda r: build_files(r, "d", 1000)),
    ("destructive: 10,000 files",    "rm -rf d",   lambda r: build_files(r, "d", 10000)),
    ("destructive: 1 file (100 MB)", "rm big.bin", lambda r: build_one_file(r, "big.bin", BIG_BYTES)),
]

def event(cwd, command, i):
    return json.dumps({
        "session_id": "bench",
        "cwd": cwd,
        "hook_event_name": "PreToolUse",
        "tool_name": "Bash",
        "tool_use_id": f"t{i}",
        "tool_input": {"command": command},
    })

def time_once(cwd, doover_home, command, i, force_copy):
    env = dict(os.environ)
    env["DOOVER_HOME"] = doover_home
    if force_copy:
        env["DOOVER_FORCE_COPY"] = "1"
    else:
        env.pop("DOOVER_FORCE_COPY", None)
    payload = event(cwd, command, i).encode()
    t0 = time.perf_counter()
    p = subprocess.run([BIN, "hook", "pre"], input=payload, env=env,
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    dt = (time.perf_counter() - t0) * 1000.0
    return dt, p.returncode

def pct(xs, q):
    xs = sorted(xs)
    if not xs:
        return 0.0
    k = (len(xs) - 1) * q
    lo = int(k)
    hi = min(lo + 1, len(xs) - 1)
    return xs[lo] + (xs[hi] - xs[lo]) * (k - lo)

def run_matrix(force_copy):
    label = "FORCE-COPY (no-reflink FS)" if force_copy else "REFLINK (CoW: APFS/Btrfs/XFS)"
    print(f"\n=== {label} ===")
    print(f"{'scenario':<32}{'p50':>9}{'p95':>9}{'max':>9}   ms")
    print("-" * 70)
    results = []
    for name, command, builder in SCENARIOS:
        ws = tempfile.mkdtemp(prefix="doover-bench-")
        home = tempfile.mkdtemp(prefix="doover-home-")
        try:
            builder(ws)
            times = []
            for i in range(WARMUP + ITERS):
                dt, rc = time_once(ws, home, command, i, force_copy)
                if i >= WARMUP:
                    times.append(dt)
            p50, p95, mx = pct(times, .5), pct(times, .95), max(times)
            results.append((name, p50, p95, mx))
            print(f"{name:<32}{p50:>9.1f}{p95:>9.1f}{mx:>9.1f}")
        finally:
            shutil.rmtree(ws, ignore_errors=True)
            shutil.rmtree(home, ignore_errors=True)
    return results

def main():
    if not os.path.exists(BIN):
        sys.exit(f"release binary not found at {BIN}\n  build it: cargo build --release -p doover")
    # host facts
    uname = subprocess.run(["uname", "-msr"], capture_output=True, text=True).stdout.strip()
    print(f"host: {uname}")
    print(f"binary: {BIN}")
    print(f"iterations: {ITERS} (+{WARMUP} warmup discarded), hook timeout budget: 10000 ms")

    reflink = run_matrix(force_copy=False)
    copy = run_matrix(force_copy=True)

    # headline conclusions
    print("\n=== read-out ===")
    floor = next(r for r in reflink if r[0].startswith("safe"))
    print(f"per-command floor (safe, paid on EVERY command): p50 {floor[1]:.1f} ms / p95 {floor[2]:.1f} ms")
    for tag, res in (("reflink", reflink), ("copy", copy)):
        big = next(r for r in res if "100 MB" in r[0])
        tenk = next(r for r in res if "10,000" in r[0])
        print(f"{tag:>7}: 10k files p95 {tenk[2]:.0f} ms | 100 MB file p95 {big[2]:.0f} ms")

if __name__ == "__main__":
    main()
