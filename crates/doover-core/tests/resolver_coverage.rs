//! T-cov (audit round 16): common destination-overwriting / in-place
//! destructive commands must identify their TRUE target precisely — captured
//! in `paths`, not merely left to the cwd-only unknown fallback (which misses
//! any target outside cwd). Targets here are OUTSIDE cwd so an under-capture
//! shows up as a real miss instead of being masked by over-broad coverage.

use doover_core::registry::Registry;
use doover_core::resolver::{Ctx, Severity, resolve};

struct Fix {
    _jail: tempfile::TempDir,
    reg: Registry,
    cwd: std::path::PathBuf,
    home: std::path::PathBuf,
    outside: std::path::PathBuf,
}

fn fix() -> Fix {
    let jail = tempfile::tempdir().unwrap();
    let cwd = jail.path().join("proj");
    let home = jail.path().join("home");
    let outside = jail.path().join("outside");
    for d in [&cwd, &home, &outside] {
        std::fs::create_dir_all(d).unwrap();
    }
    for f in ["a", "b", "important", "important.gz"] {
        std::fs::write(outside.join(f), "PRECIOUS").unwrap();
    }
    Fix {
        reg: Registry::builtin().unwrap(),
        cwd,
        home,
        outside,
        _jail: jail,
    }
}

impl Fix {
    /// The resolver must mark `command` destructive AND list `target` in its
    /// precise paths (not rely on the cwd fallback).
    fn assert_captures(&self, command: &str, target: &std::path::Path) {
        let ctx = Ctx {
            cwd: &self.cwd,
            home: &self.home,
        };
        let r = resolve(command, &self.reg, &ctx);
        assert!(
            r.severity >= Severity::Destructive,
            "`{command}` should be destructive, got {:?}",
            r.severity
        );
        assert!(
            r.paths.iter().any(|p| p == target),
            "`{command}` must precisely capture {} — got paths {:?} (has_unknown={})",
            target.display(),
            r.paths,
            r.has_unknown
        );
    }
}

#[test]
fn install_captures_its_overwritten_destination() {
    let f = fix();
    let dest = f.outside.join("important");
    let a = f.outside.join("a");
    f.assert_captures(
        &format!("install {} {}", a.display(), dest.display()),
        &dest,
    );
    f.assert_captures(
        &format!("install -m 0644 {} {}", a.display(), dest.display()),
        &dest,
    );
}

#[test]
fn gzip_family_captures_the_file_it_replaces() {
    let f = fix();
    let imp = f.outside.join("important");
    f.assert_captures(&format!("gzip {}", imp.display()), &imp);
    f.assert_captures(&format!("xz {}", imp.display()), &imp);
    f.assert_captures(&format!("zstd {}", imp.display()), &imp);
    let gz = f.outside.join("important.gz");
    f.assert_captures(&format!("gunzip {}", gz.display()), &gz);
}

#[test]
fn wget_output_flag_captures_the_file_it_overwrites() {
    // `wget -O file url` truncates `file`; bare `wget url` is additive (saves
    // as file.N, never overwrites) and stays non-destructive.
    let f = fix();
    let dest = f.outside.join("important");
    f.assert_captures(
        &format!("wget -O {} http://example.com/x", dest.display()),
        &dest,
    );
    f.assert_captures(
        &format!(
            "wget --output-document={} http://example.com/x",
            dest.display()
        ),
        &dest,
    );
    // bare download must NOT be destructive (nothing to snapshot)
    let ctx = Ctx {
        cwd: &f.cwd,
        home: &f.home,
    };
    let bare = resolve("wget http://example.com/x", &f.reg, &ctx);
    assert!(
        bare.severity < Severity::Destructive,
        "bare wget is additive, got {:?}",
        bare.severity
    );
}

#[test]
fn curl_output_flag_captures_the_file_it_overwrites() {
    // `curl -o file url` writes the response over `file`. It also externalizes,
    // but for undo the local overwrite is what matters — must be snapshotted.
    let f = fix();
    let dest = f.outside.join("important");
    f.assert_captures(
        &format!("curl -o {} http://example.com/x", dest.display()),
        &dest,
    );
    f.assert_captures(
        &format!("curl --output {} http://example.com/x", dest.display()),
        &dest,
    );
    // `curl -O` uses the remote basename (not statically known) -> must at
    // least be destructive so the cwd fallback engages, not externalizing-only
    let ctx = Ctx {
        cwd: &f.cwd,
        home: &f.home,
    };
    let remote = resolve("curl -O http://example.com/important", &f.reg, &ctx);
    assert!(
        remote.severity >= Severity::Destructive && remote.has_unknown,
        "curl -O must be destructive+fallback, got {:?} unk={}",
        remote.severity,
        remote.has_unknown
    );
}

#[test]
fn git_working_tree_discarding_subcommands_are_destructive_repo_scoped() {
    // `git restore` (modern `checkout --`), `git rm`, and
    // `git switch --discard-changes` all clobber the working tree. Like
    // checkout/reset --hard/clean they must be destructive and repo-scoped —
    // precise capture, not the cwd-only fallback.
    let jail = tempfile::tempdir().unwrap();
    let repo = jail.path().join("repo");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    let home = jail.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let reg = Registry::builtin().unwrap();
    let ctx = Ctx {
        cwd: &repo,
        home: &home,
    };
    let repo_norm = repo.canonicalize().unwrap_or(repo.clone());

    for c in [
        "git restore .",
        "git restore src/main.rs",
        "git rm file.rs",
        "git switch --discard-changes main",
    ] {
        let r = resolve(c, &reg, &ctx);
        assert!(
            r.severity >= Severity::Destructive,
            "`{c}` must be destructive, got {:?}",
            r.severity
        );
        assert!(
            r.paths.iter().any(|p| p == &repo_norm || p == &repo),
            "`{c}` must capture the repo root precisely, got {:?} (unk={})",
            r.paths,
            r.has_unknown
        );
    }
}

// --- D3: precise capture + honest flagging of the long tail ------------------

#[test]
fn rsync_captures_its_overwritten_destination() {
    // `rsync -a src/ dst/` overwrites dst; `--delete` removes files there.
    // The destination is the last positional — capture it precisely instead
    // of leaning on the cwd-only fallback (which misses an out-of-cwd dst).
    let f = fix();
    let dst = f.outside.join("important");
    let src = f.outside.join("a");
    f.assert_captures(
        &format!("rsync -a {}/ {}", src.display(), dst.display()),
        &dst,
    );
    f.assert_captures(
        &format!("rsync --delete {}/ {}", src.display(), dst.display()),
        &dst,
    );
}

#[test]
fn external_state_commands_are_flagged_not_wastefully_snapshotted() {
    // redis/psql/mysql/kubectl/docker touch state no local snapshot can reach.
    // They must be FLAGGED (>= Externalizing) so the journal says "can't undo
    // this" — and must NOT set has_unknown, which would trigger a pointless
    // full-cwd snapshot of the project for state that isn't in the project.
    let f = fix();
    let ctx = Ctx {
        cwd: &f.cwd,
        home: &f.home,
    };
    let cases: &[(&str, Severity)] = &[
        ("redis-cli FLUSHALL", Severity::Externalizing),
        ("psql -c 'DROP TABLE users'", Severity::Externalizing),
        ("mysql -e 'DROP DATABASE app'", Severity::Externalizing),
        ("kubectl delete pod web", Severity::Externalizing),
        ("docker rm -f web", Severity::Externalizing),
        ("docker rmi myimage", Severity::Externalizing),
    ];
    for (cmd, want) in cases {
        let r = resolve(cmd, &f.reg, &ctx);
        assert!(
            r.severity >= *want,
            "`{cmd}` should be >= {:?}, got {:?}",
            want,
            r.severity
        );
        assert!(
            !r.has_unknown,
            "`{cmd}` must NOT fall to the cwd snapshot (external state); has_unknown was true"
        );
    }
    // a plain redis read still shouldn't snapshot the cwd (externalizing, flagged)
    let read = resolve("psql -c 'SELECT 1'", &f.reg, &ctx);
    assert!(!read.has_unknown, "psql read must not snapshot cwd");
}
