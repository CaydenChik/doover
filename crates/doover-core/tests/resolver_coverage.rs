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
