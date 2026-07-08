//! T9 — inspection surface (step 8): `diff` semantics and display-time
//! secret redaction. Written before the module exists.
//!
//! Redaction context (CLAUDE.md carried-forward): journal `raw_command` may
//! embed secrets (curl auth headers, tokens, env assignments). The journal
//! keeps the raw string — redaction happens at DISPLAY time only, so `log`
//! and `show` never print credentials.

use doover_core::inspect::{self, PathStatus};
use doover_core::redact::redact;
use doover_core::snapshot::{Store, StoreOptions};
use std::fs;

fn rig() -> (tempfile::TempDir, Store, std::path::PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open_with(tmp.path().join("store"), StoreOptions::default()).unwrap();
    let world = tmp.path().join("world");
    fs::create_dir_all(&world).unwrap();
    (tmp, store, world)
}

#[test]
fn diff_reports_unchanged_modified_missing_and_type_changed() {
    let (_tmp, store, world) = rig();
    let d = world.join("proj");
    fs::create_dir_all(&d).unwrap();
    fs::write(d.join("same.txt"), "stable").unwrap();
    fs::write(d.join("edited.txt"), "v1").unwrap();
    fs::write(d.join("gone.txt"), "bye").unwrap();
    fs::write(d.join("swapped.txt"), "was a file").unwrap();

    let m = store.snapshot(&d, None).unwrap();

    // mutate the world after the snapshot
    fs::write(d.join("edited.txt"), "v2").unwrap();
    fs::remove_file(d.join("gone.txt")).unwrap();
    fs::remove_file(d.join("swapped.txt")).unwrap();
    std::os::unix::fs::symlink("same.txt", d.join("swapped.txt")).unwrap();

    let lines = inspect::diff_manifest(&m).unwrap();
    let status_of = |name: &str| {
        lines
            .iter()
            .find(|l| l.path.ends_with(name))
            .unwrap_or_else(|| panic!("no diff line for {name}"))
            .status
    };
    assert_eq!(status_of("same.txt"), PathStatus::Unchanged);
    assert_eq!(status_of("edited.txt"), PathStatus::Modified);
    assert_eq!(status_of("gone.txt"), PathStatus::Missing);
    assert_eq!(status_of("swapped.txt"), PathStatus::TypeChanged);
}

#[test]
fn diff_of_symlink_entries_compares_targets_not_contents() {
    let (_tmp, store, world) = rig();
    let d = world.join("proj");
    fs::create_dir_all(&d).unwrap();
    std::os::unix::fs::symlink("a", d.join("stable-link")).unwrap();
    std::os::unix::fs::symlink("a", d.join("retargeted")).unwrap();

    let m = store.snapshot(&d, None).unwrap();
    fs::remove_file(d.join("retargeted")).unwrap();
    std::os::unix::fs::symlink("b", d.join("retargeted")).unwrap();

    let lines = inspect::diff_manifest(&m).unwrap();
    let status_of = |name: &str| {
        lines
            .iter()
            .find(|l| l.path.ends_with(name))
            .unwrap()
            .status
    };
    assert_eq!(status_of("stable-link"), PathStatus::Unchanged);
    assert_eq!(status_of("retargeted"), PathStatus::Modified);
}

#[test]
fn diff_of_absent_root_reports_created_only_when_it_now_exists() {
    let (_tmp, store, world) = rig();
    let ghost = world.join("ghost.txt");

    let m = store.snapshot(&ghost, None).unwrap(); // Root::Absent
    let lines = inspect::diff_manifest(&m).unwrap();
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].status, PathStatus::Unchanged); // still absent

    fs::write(&ghost, "now real").unwrap();
    let lines = inspect::diff_manifest(&m).unwrap();
    assert_eq!(lines[0].status, PathStatus::Created);
}

// --- redaction ---------------------------------------------------------------

#[test]
fn redact_masks_authorization_headers_and_bearer_tokens() {
    let cmd = r#"curl -H "Authorization: Bearer sk-live-abc123XYZ" https://api.example.com"#;
    let out = redact(cmd);
    assert!(!out.contains("sk-live-abc123XYZ"), "token leaked: {out}");
    assert!(out.contains("[redacted]"));
    assert!(out.contains("curl"), "non-secret parts preserved: {out}");
    assert!(out.contains("https://api.example.com"));

    let lower = redact("curl -H 'authorization: token ghp_deadbeef' https://x");
    assert!(!lower.contains("ghp_deadbeef"), "leaked: {lower}");
}

#[test]
fn redact_masks_secret_bearing_flags_and_env_assignments() {
    for (cmd, secret) in [
        ("mysql --password=hunter2 -u root db", "hunter2"),
        ("vault login --token s.abc123", "s.abc123"),
        ("deploy --api-key=AKfoo42 prod", "AKfoo42"),
        (
            "AWS_SECRET_ACCESS_KEY=wJalrXUtnFEMI aws s3 ls",
            "wJalrXUtnFEMI",
        ),
        (r#"GITHUB_TOKEN="ghp quoted" gh api /user"#, "ghp quoted"),
    ] {
        let out = redact(cmd);
        assert!(!out.contains(secret), "secret leaked from {cmd:?}: {out}");
        assert!(out.contains("[redacted]"), "no mask in {out}");
    }
}

#[test]
fn redact_leaves_benign_commands_untouched() {
    for cmd in [
        "rm -rf build",
        "git push origin main",
        "cargo test --workspace",
        // words like "password" inside paths/prose are not values
        "vim docs/password-policy.md",
    ] {
        assert_eq!(redact(cmd), cmd, "benign command must pass through");
    }
}

#[test]
fn diff_of_a_single_file_root_hits_the_file_not_a_phantom_child() {
    // regression: the root entry has rel="", and path.join("") grows a
    // trailing slash — stat("…/file/") is ENOTDIR, so a perfectly intact
    // file reported as MISSING
    let (_tmp, store, world) = rig();
    let f = world.join("solo.txt");
    fs::write(&f, "v1").unwrap();
    let m = store.snapshot(&f, None).unwrap();

    let lines = inspect::diff_manifest(&m).unwrap();
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].path, f, "no trailing-slash phantom");
    assert_eq!(lines[0].status, PathStatus::Unchanged);

    fs::write(&f, "v2").unwrap();
    assert_eq!(
        inspect::diff_manifest(&m).unwrap()[0].status,
        PathStatus::Modified
    );
}
