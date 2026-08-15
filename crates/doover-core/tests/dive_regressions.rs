//! Deep-dive audit (2026-08-15) regressions. Each test pins one confirmed
//! finding from the 29-agent adversarial dive of f343db9; the verification
//! traces live in the dive report next to the repo. Same contract as
//! trial_regressions / review_regressions: these were all RED against the
//! pre-fix code.

use doover_core::registry::Registry;
use doover_core::resolver::{Ctx, resolve};
use doover_core::snapshot::Store;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Finding `glob-cap-silent-truncation`: a destructive glob with more matches
/// than the 10k enumeration cap used to keep `has_unknown == false`, so the
/// files past the cap were deleted with no snapshot and no cwd fallback —
/// while a capped BRACE expansion correctly marked unknown. Over-cap globs
/// must set `has_unknown` (routing the cwd fallback) while still keeping the
/// paths they did enumerate (those still get precise snapshots).
#[test]
fn glob_over_enumeration_cap_marks_unknown_and_keeps_captured_paths() {
    let jail = tempfile::tempdir().unwrap();
    let cwd = jail.path().join("proj");
    let home = jail.path().join("home");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    // one past the cap, so exactly one match is dropped by enumeration
    for i in 0..10_001 {
        std::fs::write(cwd.join(format!("f{i:05}.log")), "").unwrap();
    }
    let reg = Registry::builtin().unwrap();
    let r = resolve(
        "rm -f *.log",
        &reg,
        &Ctx {
            cwd: &cwd,
            home: &home,
        },
    );
    assert!(
        r.has_unknown,
        "a glob truncated at the enumeration cap left has_unknown=false: \
         the dropped matches are invisible to scoping and get no snapshot"
    );
    assert_eq!(
        r.paths.len(),
        10_000,
        "the enumerated matches must still be captured precisely"
    );
}

/// Control for the same finding: a glob comfortably under the cap must NOT
/// set has_unknown — the fix may not degrade ordinary glob resolution.
#[test]
fn glob_under_enumeration_cap_stays_fully_resolved() {
    let jail = tempfile::tempdir().unwrap();
    let cwd = jail.path().join("proj");
    let home = jail.path().join("home");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    for i in 0..5 {
        std::fs::write(cwd.join(format!("f{i}.log")), "").unwrap();
    }
    let reg = Registry::builtin().unwrap();
    let r = resolve(
        "rm -f *.log",
        &reg,
        &Ctx {
            cwd: &cwd,
            home: &home,
        },
    );
    assert!(!r.has_unknown, "an under-cap glob must stay fully resolved");
    assert_eq!(r.paths.len(), 5);
}

/// The one content object under `<store>/objects/` (test rigs here ingest a
/// single file).
fn only_object(store_root: &std::path::Path) -> std::path::PathBuf {
    let mut found = Vec::new();
    for shard in fs::read_dir(store_root.join("objects")).unwrap().flatten() {
        if shard.path().is_dir() {
            for obj in fs::read_dir(shard.path()).unwrap().flatten() {
                found.push(obj.path());
            }
        }
    }
    assert_eq!(found.len(), 1, "rig expects exactly one store object");
    found.pop().unwrap()
}

/// Finding `dedup-defeats-gc-grace`: the dedup branch of ingestion adopted an
/// existing object's hash into a not-yet-journaled manifest WITHOUT touching
/// the object's mtime. gc's round-14 grace window is mtime-based and the
/// racing hook's manifest row doesn't exist yet — so a fresh pending action
/// holding an OLD object was unshielded, the exact mirror of the round-14
/// assumption ("an old row with a fresh object is a temporal impossibility").
/// Re-ingesting existing content must refresh the object's mtime so a racing
/// gc sees it as possibly-in-flight.
#[test]
fn dedup_reingestion_refreshes_object_mtime_and_survives_gc_grace() {
    let jail = tempfile::tempdir().unwrap();
    let store_root = jail.path().join("store");
    let store = Store::open(&store_root).unwrap();
    let world = jail.path().join("world");
    fs::create_dir_all(&world).unwrap();
    let f = world.join("data.txt");
    fs::write(&f, "precious bytes").unwrap();
    store.snapshot(&f, None).unwrap();

    // age the object well past gc's 1h grace window (round-14 test rule:
    // backdating must happen at the OBJECT, the temporal ground truth)
    let obj = only_object(&store_root);
    let old = UNIX_EPOCH
        + SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .saturating_sub(Duration::from_secs(3 * 3600));
    fs::set_permissions(&obj, fs::Permissions::from_mode(0o644)).unwrap();
    fs::OpenOptions::new()
        .write(true)
        .open(&obj)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(old))
        .unwrap();
    fs::set_permissions(&obj, fs::Permissions::from_mode(0o400)).unwrap();

    // a NEW action captures identical content: dedup adopts the old object
    store.snapshot(&f, None).unwrap();

    let age = SystemTime::now()
        .duration_since(fs::metadata(&obj).unwrap().modified().unwrap())
        .unwrap_or_default();
    assert!(
        age < Duration::from_secs(600),
        "dedup adoption left the object's mtime {age:?} old: a racing gc's \
         grace window cannot shield the pending action that just adopted it"
    );

    // end-to-end: the adopted object survives a prune that has no journal row
    // vouching for it (empty live set), exactly the racing-gc shape
    store
        .prune(&std::collections::BTreeSet::new(), 3_600_000, false)
        .unwrap();
    assert!(
        obj.exists(),
        "gc pruned an object a pending action had just adopted via dedup"
    );
}

// --- redirect scoping holes (dive finding `redirect-target-scoping-holes`) --

fn redirect_rig() -> (
    tempfile::TempDir,
    std::path::PathBuf,
    std::path::PathBuf,
    Registry,
) {
    let jail = tempfile::tempdir().unwrap();
    let cwd = jail.path().join("proj");
    let home = jail.path().join("home");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    let reg = Registry::builtin().unwrap();
    (jail, cwd, home, reg)
}

/// `> 2024` truncates a file named 2024, but the all-digit fd guard returned
/// before the redirect rule contributed: the line classified SAFE with no
/// paths and no unknown — silent data loss behind a safe host command. True
/// fd forms (`>&2`, `2>&1`) never reach the filename path (brush parses them
/// as Duplicate targets), so an all-digit Filename IS a real file.
#[test]
fn all_digit_redirect_target_is_a_real_file() {
    let (_j, cwd, home, reg) = redirect_rig();
    std::fs::write(cwd.join("2024"), "annual report").unwrap();
    let r = resolve(
        "echo x > 2024",
        &reg,
        &Ctx {
            cwd: &cwd,
            home: &home,
        },
    );
    assert!(
        r.severity >= doover_core::resolver::Severity::Destructive,
        "a truncating redirect to file `2024` must classify destructive, got {:?}",
        r.severity
    );
    assert!(
        r.paths.contains(&cwd.join("2024")),
        "the truncated file must be captured: {:?}",
        r.paths
    );
}

/// Control: real fd duplications must stay uncaptured and safe.
#[test]
fn fd_duplication_forms_stay_safe() {
    let (_j, cwd, home, reg) = redirect_rig();
    for cmd in ["echo x >&2", "echo x 2>&1", "echo x 2>&-"] {
        let r = resolve(
            cmd,
            &reg,
            &Ctx {
                cwd: &cwd,
                home: &home,
            },
        );
        assert!(
            r.paths.is_empty() && !r.has_unknown,
            "{cmd} is an fd operation, not a file write: {:?}",
            r.paths
        );
    }
}

/// `>& file` with a NON-numeric target redirects both streams to the file —
/// a truncating write bash performs even non-interactively. The
/// DuplicateOutput arm treated every such target as an fd operation.
#[test]
fn output_duplication_to_a_filename_is_a_write() {
    let (_j, cwd, home, reg) = redirect_rig();
    std::fs::write(cwd.join("logfile"), "history").unwrap();
    let r = resolve(
        "echo hi >& logfile",
        &reg,
        &Ctx {
            cwd: &cwd,
            home: &home,
        },
    );
    assert!(
        r.severity >= doover_core::resolver::Severity::Destructive
            && r.paths.contains(&cwd.join("logfile")),
        "`>& logfile` truncates logfile and must be captured: sev {:?} paths {:?}",
        r.severity,
        r.paths
    );
}

/// An unquoted glob redirect target is EXPANDED by bash: with one match the
/// write lands on that file, not on the literal pattern. The resolver scoped
/// the literal (an Absent path) while bash truncated the real match.
#[test]
fn single_match_glob_redirect_captures_the_match() {
    let (_j, cwd, home, reg) = redirect_rig();
    std::fs::write(cwd.join("app.conf"), "config").unwrap();
    let r = resolve(
        "echo x > *.conf",
        &reg,
        &Ctx {
            cwd: &cwd,
            home: &home,
        },
    );
    assert!(
        r.paths.contains(&cwd.join("app.conf")),
        "the real glob match must be captured, not the literal pattern: {:?}",
        r.paths
    );
}

/// Zero matches: bash (nullglob off) creates a file literally named like the
/// pattern — the literal capture is correct and must be kept.
#[test]
fn zero_match_glob_redirect_keeps_the_literal() {
    let (_j, cwd, home, reg) = redirect_rig();
    let r = resolve(
        "echo x > *.xyz",
        &reg,
        &Ctx {
            cwd: &cwd,
            home: &home,
        },
    );
    assert!(
        r.paths.contains(&cwd.join("*.xyz")),
        "with no match bash creates the literal name; capture it: {:?}",
        r.paths
    );
}

/// Multi-match: bash refuses ("ambiguous redirect") and writes nothing; the
/// matches are captured anyway — harmless overcapture, never undercapture.
#[test]
fn multi_match_glob_redirect_captures_matches() {
    let (_j, cwd, home, reg) = redirect_rig();
    std::fs::write(cwd.join("a.conf"), "a").unwrap();
    std::fs::write(cwd.join("b.conf"), "b").unwrap();
    let r = resolve(
        "echo x > *.conf",
        &reg,
        &Ctx {
            cwd: &cwd,
            home: &home,
        },
    );
    assert!(
        r.paths.contains(&cwd.join("a.conf")) && r.paths.contains(&cwd.join("b.conf")),
        "matches captured even though bash will refuse the ambiguous redirect: {:?}",
        r.paths
    );
}

// --- single-file-root budget bypass (dive `one-large-file-blows-hook-timeout`)

/// The single non-directory root branch is the literal `rm big-file` case; it
/// used to consult NEITHER max_bytes NOR the deadline, so one huge file
/// copied (and hashed) in full — on a non-reflink filesystem long enough to
/// blow the harness timeout: SIGKILL, no manifest, no loud gap.
#[test]
fn single_file_root_respects_max_bytes() {
    let jail = tempfile::tempdir().unwrap();
    let store = Store::open(jail.path().join("store")).unwrap();
    let f = jail.path().join("big.bin");
    fs::write(&f, vec![7u8; 1024]).unwrap();
    let limits = doover_core::snapshot::Limits {
        max_files: 100,
        max_bytes: 10,
        max_duration: None,
    };
    let m = store.snapshot(&f, Some(&limits)).unwrap();
    assert!(m.truncated, "an over-limit single-file root must truncate");
    assert!(
        m.entries.is_empty(),
        "the over-limit file must not be ingested"
    );
    assert!(
        m.warnings
            .iter()
            .any(|w| w.contains("exceeds snapshot limits")),
        "the gap must be loud: {:?}",
        m.warnings
    );
}

/// Same branch, wall-clock side: an expired budget must stop the copy/hash
/// mid-file (checked per 8 MiB chunk) instead of running to completion.
#[test]
fn single_file_root_respects_time_budget() {
    let jail = tempfile::tempdir().unwrap();
    let store = Store::open(jail.path().join("store")).unwrap();
    let f = jail.path().join("big.bin");
    fs::write(&f, vec![7u8; 1024]).unwrap();
    let limits = doover_core::snapshot::Limits {
        max_files: 100,
        max_bytes: u64::MAX,
        max_duration: Some(Duration::ZERO),
    };
    let m = store.snapshot(&f, Some(&limits)).unwrap();
    assert!(
        m.truncated && m.entries.is_empty(),
        "an expired budget must truncate the single-file root loudly \
         (truncated={}, entries={})",
        m.truncated,
        m.entries.len()
    );
}

/// Dive `unreadable-file-aborts-tree-snapshot`: one uncapturable file (mode
/// 000 — stat succeeds, open fails EACCES) used to propagate via `?` and
/// abort the ENTIRE tree snapshot, journaling the whole target as
/// UNPROTECTED, while walk/metadata errors on the same walk warned and
/// continued. The readable rest of the tree must stay protected.
#[test]
fn one_unreadable_file_does_not_abort_the_tree() {
    let jail = tempfile::tempdir().unwrap();
    let store = Store::open(jail.path().join("store")).unwrap();
    let d = jail.path().join("tree");
    fs::create_dir_all(&d).unwrap();
    fs::write(d.join("aaa-locked"), "cannot read me").unwrap();
    fs::set_permissions(d.join("aaa-locked"), fs::Permissions::from_mode(0o000)).unwrap();
    fs::write(d.join("zzz-good"), "precious").unwrap();

    let m = store
        .snapshot(&d, None)
        .expect("one unreadable file must not abort the tree snapshot");
    assert!(
        m.entries
            .iter()
            .any(|e| e.rel.to_string_lossy().ends_with("zzz-good")),
        "the readable sibling must be captured"
    );
    assert!(
        m.warnings.iter().any(|w| w.contains("aaa-locked")),
        "the unreadable file must be a loud warning: {:?}",
        m.warnings
    );
    assert!(m.skipped >= 1, "the miss must be counted");
    assert!(
        m.truncated,
        "a coverage hole must mark the manifest truncated so refuse-by-default \
         governs its restore"
    );
}

/// Dive probe `glob-budget` (CONFIRMED): resolve()-time glob expansion ran
/// with NO time budget, BEFORE start_action — and glob 0.3 follows directory
/// symlinks under `**`, so a directory with two cycle-forming symlinks
/// branches k^32: 66+ seconds of measured CPU on a FOUR-entry directory,
/// guaranteeing a harness-timeout SIGKILL with no journal row while the
/// destructive command runs unprotected. With the budget, resolve() returns
/// within ~2s (default DOOVER_MAX_GLOB_MS) and marks the scope unknown.
/// Pre-fix this test FAILS via the 20s harness guard (the resolve thread
/// hangs effectively forever).
#[test]
fn symlink_cycle_globstar_is_time_bounded() {
    use std::os::unix::fs::symlink;
    let jail = tempfile::tempdir().unwrap();
    let cwd = jail.path().join("proj");
    let home = jail.path().join("home");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(cwd.join("a.txt"), "x").unwrap();
    std::fs::write(cwd.join("b.txt"), "y").unwrap();
    // RELATIVE self-links (`ln -s .`) are the pathological form: absolute
    // self-links exhaust ELOOP quickly, but relative ones keep every hop
    // cheap and the k^32 branching alive (probe-measured 66+s of CPU)
    symlink(".", cwd.join("loop")).unwrap();
    symlink(".", cwd.join("loop2")).unwrap();

    let (tx, rx) = std::sync::mpsc::channel();
    let cwd2 = cwd.clone();
    let home2 = home.clone();
    std::thread::spawn(move || {
        let reg = Registry::builtin().unwrap();
        let r = resolve(
            "rm -rf **/nomatch*",
            &reg,
            &Ctx {
                cwd: &cwd2,
                home: &home2,
            },
        );
        let _ = tx.send(r);
    });
    let r = rx
        .recv_timeout(Duration::from_secs(20))
        .expect("resolve() hung on a symlink-cycle globstar — the glob walk is unbounded");
    assert!(
        r.has_unknown,
        "a budget-expired (or zero-match) cycle glob must mark unknown so the \
         cwd fallback covers the scope"
    );
}

// --- restore-side data destroyers (dive findings
// `restore-failure-destroys-carried-dirs` + `undo-deletes-doover-home`) ----

use doover_core::snapshot::{SkipPolicy, SnapshotError};

/// Rig: proj/src/main.rs + a skipped (never-captured) node_modules holding a
/// sentinel. Returns (jail, proj, store, manifest-with-skipped-dirs).
fn carry_rig() -> (
    tempfile::TempDir,
    std::path::PathBuf,
    Store,
    doover_core::snapshot::Manifest,
) {
    let jail = tempfile::tempdir().unwrap();
    let proj = jail.path().join("proj");
    fs::create_dir_all(proj.join("src")).unwrap();
    fs::write(proj.join("src/main.rs"), "v1").unwrap();
    fs::create_dir_all(proj.join("node_modules")).unwrap();
    fs::write(proj.join("node_modules/dep.js"), "SENTINEL-NEVER-CAPTURED").unwrap();
    let store = Store::open(jail.path().join("store-home").join("store")).unwrap();
    let skips = SkipPolicy::new(vec!["node_modules".into()], None);
    let m = store.snapshot_scoped(&proj, None, &[], &skips).unwrap();
    assert_eq!(m.skipped_dirs.len(), 1, "rig sanity: node_modules skipped");
    fs::write(proj.join("src/main.rs"), "CLOBBERED").unwrap();
    (jail, proj, store, m)
}

fn no_staging_left(dir: &std::path::Path) -> bool {
    !fs::read_dir(dir).unwrap().flatten().any(|e| {
        e.file_name()
            .to_string_lossy()
            .starts_with(".doover-restore-")
    })
}

/// Arm C (remove_any(target) fails midway — natural non-root trigger: a
/// chmod-000 subdir): the carried live node_modules used to die inside
/// `remove_any(&staging)`. It must be moved BACK, staging must be gone, the
/// error must say the target was disturbed (not "nothing changed"), and a
/// retry after fixing the blocker must converge.
#[test]
fn arm_c_failure_moves_carried_dirs_back_and_converges() {
    let (jail, proj, store, m) = carry_rig();
    fs::create_dir_all(proj.join("blocked")).unwrap();
    fs::write(proj.join("blocked/x"), "x").unwrap();
    fs::set_permissions(proj.join("blocked"), fs::Permissions::from_mode(0o000)).unwrap();

    let err = store.restore(&m).unwrap_err();
    fs::set_permissions(proj.join("blocked"), fs::Permissions::from_mode(0o755)).unwrap();

    assert!(
        matches!(err, SnapshotError::RestoreTargetDisturbed { .. }),
        "a post-carry failure with the target partially deleted must report \
         the disturbance, got: {err}"
    );
    assert_eq!(
        fs::read_to_string(proj.join("node_modules/dep.js")).unwrap(),
        "SENTINEL-NEVER-CAPTURED",
        "the carried live dir must be moved back, not deleted with staging"
    );
    assert!(no_staging_left(jail.path()), "staging must be cleaned up");

    store
        .restore(&m)
        .expect("retry after unblocking must converge");
    assert_eq!(fs::read_to_string(proj.join("src/main.rs")).unwrap(), "v1");
    assert!(proj.join("node_modules/dep.js").exists());
}

/// Arm D (the final staging→target rename fails, injected via the debug-only
/// single-shot marker): the target root is already gone; carries must be
/// moved back into a re-created shell and a retry must fully converge.
#[test]
fn swap_failure_moves_carries_back_and_retry_converges() {
    let (jail, proj, store, m) = carry_rig();
    fs::write(
        jail.path()
            .join("store-home/store/.doover-test-restore-swap-fail"),
        "",
    )
    .unwrap();

    let err = store.restore(&m).unwrap_err();
    assert!(
        matches!(err, SnapshotError::RestoreTargetDisturbed { .. }),
        "got: {err}"
    );
    assert_eq!(
        fs::read_to_string(proj.join("node_modules/dep.js")).unwrap(),
        "SENTINEL-NEVER-CAPTURED",
        "carried dir must be rescued into the re-created target shell"
    );
    assert!(no_staging_left(jail.path()));

    store
        .restore(&m)
        .expect("marker is single-shot; retry converges");
    assert_eq!(fs::read_to_string(proj.join("src/main.rs")).unwrap(), "v1");
}

/// If a move-back itself fails (injected), staging must be PRESERVED — it
/// holds live data — and the error must name it and the stranded entries.
#[test]
fn moveback_failure_preserves_staging_and_names_it() {
    let (jail, proj, store, m) = carry_rig();
    for marker in [
        ".doover-test-restore-swap-fail",
        ".doover-test-restore-moveback-fail",
    ] {
        fs::write(jail.path().join("store-home/store").join(marker), "").unwrap();
    }

    let err = store.restore(&m).unwrap_err();
    let SnapshotError::RestoreStagingPreserved {
        staging, remaining, ..
    } = err
    else {
        panic!("expected RestoreStagingPreserved, got: {err}");
    };
    assert!(
        staging.exists(),
        "staging holding live data must never be deleted"
    );
    assert_eq!(
        fs::read_to_string(
            staging
                .join(remaining[0].strip_prefix(&proj).unwrap())
                .join("dep.js")
        )
        .unwrap(),
        "SENTINEL-NEVER-CAPTURED",
        "the stranded live dir must still be inside the named staging"
    );
}

/// The nested-DOOVER_HOME half: a cwd manifest restore used to swap-delete
/// the live store and journal (the round-21 exclusion was capture-only). The
/// live home must ride the carry across the swap.
#[test]
fn nested_home_survives_restore_swap() {
    let jail = tempfile::tempdir().unwrap();
    let proj = jail.path().join("proj");
    fs::create_dir_all(&proj).unwrap();
    fs::write(proj.join("f.txt"), "v1").unwrap();
    let home = proj.join(".doover");
    let store = Store::open(home.join("store")).unwrap();
    fs::write(home.join("journal.db"), "LIVE-JOURNAL").unwrap();

    // hook-style capture: home excluded (round 21)
    let m = store
        .snapshot_excluding(&proj, None, std::slice::from_ref(&home))
        .unwrap();
    fs::write(proj.join("f.txt"), "CLOBBERED").unwrap();
    fs::write(home.join("journal.db"), "LIVE-JOURNAL-2").unwrap();

    store.restore(&m).unwrap();
    assert_eq!(fs::read_to_string(proj.join("f.txt")).unwrap(), "v1");
    assert_eq!(
        fs::read_to_string(home.join("journal.db")).unwrap(),
        "LIVE-JOURNAL-2",
        "the live nested home must survive the swap untouched"
    );
    assert!(home.join("store/objects").exists(), "store must survive");
}

/// The strict-descendant GATE: when home CONTAINS the target (every tmp test
/// rig; DOOVER_HOME=$HOME in production), nothing may be phantom-carried and
/// the oracle must stay fully sighted.
#[test]
fn home_containing_target_gets_no_implicit_carry() {
    let jail = tempfile::tempdir().unwrap();
    let world = jail.path().join("world");
    fs::create_dir_all(&world).unwrap();
    fs::write(world.join("f.txt"), "v1").unwrap();
    // store at jail/store → derived home is the jail dir, which CONTAINS world
    let store = Store::open(jail.path().join("store")).unwrap();
    let m = store.snapshot(&world, None).unwrap();

    fs::write(world.join("extra.txt"), "user file").unwrap();
    assert!(
        !store.state_matches(&m).unwrap(),
        "an extra entry must still be a visible difference (oracle not blinded)"
    );
    store.restore(&m).unwrap();
    assert!(
        !world.join("extra.txt").exists(),
        "restore must fully replace the tree — no phantom carry when home \
         contains the target"
    );
}

/// Rollback capture (undo engine) must not re-ingest a nested home.
#[test]
fn rollback_capture_excludes_nested_home() {
    let jail = tempfile::tempdir().unwrap();
    let proj = jail.path().join("proj");
    let home = proj.join(".doover");
    let store = Store::open(home.join("store")).unwrap();
    fs::write(proj.join("f.txt"), "data").unwrap();
    fs::write(home.join("journal.db"), "SECRETS").unwrap();

    let m = store.snapshot_rollback(&proj).unwrap();
    assert!(
        m.entries.iter().all(|e| !e.rel.starts_with(".doover")),
        "rollback capture must exclude the nested home: {:?}",
        m.entries.iter().map(|e| &e.rel).collect::<Vec<_>>()
    );
    assert!(
        m.entries
            .iter()
            .any(|e| e.rel.to_string_lossy().contains("f.txt")),
        "user files must still be captured"
    );
}

/// The conflict oracle must ignore drift inside a nested home (running
/// doover mutates its own journal — nested layouts used to conflict
/// SYSTEMATICALLY, training users onto --force) while still seeing real
/// extra files.
#[test]
fn oracle_ignores_nested_home_but_sees_real_extras() {
    let jail = tempfile::tempdir().unwrap();
    let proj = jail.path().join("proj");
    let home = proj.join(".doover");
    let store = Store::open(home.join("store")).unwrap();
    fs::write(proj.join("f.txt"), "data").unwrap();
    fs::write(home.join("journal.db"), "J1").unwrap();

    let m = store
        .snapshot_excluding(&proj, None, std::slice::from_ref(&home))
        .unwrap();
    fs::write(home.join("journal.db"), "J2-DRIFTED").unwrap();
    assert!(
        store.state_matches(&m).unwrap(),
        "drift inside the nested home must not read as a conflict"
    );
    fs::write(proj.join("newfile.txt"), "user").unwrap();
    assert!(
        !store.state_matches(&m).unwrap(),
        "a genuine extra file must still conflict"
    );
}

/// A LEGACY manifest that captured the nested home (pre-round-21 hook, or
/// the old unexcluded rollback path) must not clobber the live home: its
/// under-home entries are void, the live home is preserved, and the restore
/// still succeeds for user files.
#[test]
fn legacy_manifest_with_home_entries_prefers_live_home() {
    let jail = tempfile::tempdir().unwrap();
    let proj = jail.path().join("proj");
    let home = proj.join(".doover");
    let store = Store::open(home.join("store")).unwrap();
    fs::write(proj.join("f.txt"), "v1").unwrap();
    fs::write(home.join("journal.db"), "OLD-CAPTURED").unwrap();

    // the contaminated capture the old code produced
    let m = store.snapshot(&proj, None).unwrap();
    assert!(
        m.entries.iter().any(|e| e.rel.starts_with(".doover")),
        "rig sanity: the legacy manifest contains home entries"
    );
    fs::write(proj.join("f.txt"), "CLOBBERED").unwrap();
    fs::write(home.join("journal.db"), "LIVE-NEWER").unwrap();

    store.restore(&m).unwrap();
    assert_eq!(fs::read_to_string(proj.join("f.txt")).unwrap(), "v1");
    assert_eq!(
        fs::read_to_string(home.join("journal.db")).unwrap(),
        "LIVE-NEWER",
        "the live home wins over the stale captured copy"
    );
}

/// Review RC-1/NH-2: a home nested INSIDE a skipped build dir must ride the
/// skipped dir's wholesale carry. Carrying the home separately first
/// pre-created the skipped dir's staging slot and made the later rename fail
/// ENOTEMPTY — a deterministic, never-converging restore failure for
/// DOOVER_HOME-inside-gitignored-dir layouts (which pre-0.2.1 worked).
/// Also the only test running BOTH carry blocks in one restore.
#[test]
fn home_inside_skipped_dir_rides_the_wholesale_carry() {
    let jail = tempfile::tempdir().unwrap();
    let proj = jail.path().join("proj");
    fs::create_dir_all(proj.join("src")).unwrap();
    fs::write(proj.join("src/main.rs"), "v1").unwrap();
    let skipped = proj.join("node_modules");
    let home = skipped.join(".doover");
    let store = Store::open(home.join("store")).unwrap();
    fs::write(home.join("journal.db"), "LIVE").unwrap();
    fs::write(skipped.join("dep.js"), "SENTINEL").unwrap();

    let skips = SkipPolicy::new(vec!["node_modules".into()], None);
    let m = store
        .snapshot_scoped(&proj, None, std::slice::from_ref(&home), &skips)
        .unwrap();
    assert_eq!(m.skipped_dirs.len(), 1, "rig sanity");
    fs::write(proj.join("src/main.rs"), "CLOBBERED").unwrap();

    store
        .restore(&m)
        .expect("home-inside-skipped-dir must not collide the carries");
    assert_eq!(fs::read_to_string(proj.join("src/main.rs")).unwrap(), "v1");
    assert_eq!(fs::read_to_string(home.join("journal.db")).unwrap(), "LIVE");
    assert_eq!(
        fs::read_to_string(skipped.join("dep.js")).unwrap(),
        "SENTINEL"
    );
}

/// Review RC-2/NH-1: the Absent-root restore arm ran remove_any before the
/// nested-home gate existed — undoing an action whose pre-state was "this
/// path did not exist" would delete a home that has since moved inside it,
/// store and journal included, unrecoverably. It must refuse.
#[test]
fn absent_root_restore_refuses_to_delete_a_nested_home() {
    let jail = tempfile::tempdir().unwrap();
    let target = jail.path().join("newdir");
    // capture Absent BEFORE the path exists (home elsewhere for the capture)
    let outside_store = Store::open(jail.path().join("elsewhere/store")).unwrap();
    let m = outside_store.snapshot(&target, None).unwrap();

    // the path now exists and the live home moved INSIDE it
    let home = target.join(".doover");
    let store = Store::open(home.join("store")).unwrap();
    fs::write(home.join("journal.db"), "LIVE").unwrap();

    let err = store.restore(&m).unwrap_err();
    assert!(
        matches!(err, SnapshotError::RestoreWouldDeleteHome { .. }),
        "got: {err}"
    );
    assert_eq!(
        fs::read_to_string(home.join("journal.db")).unwrap(),
        "LIVE",
        "the live home must be untouched"
    );
}

/// Review TQ-4: the EXPECTED-side home filter in compare_state was pinned by
/// nothing (the oracle test's manifest was captured with the exclusion, so
/// there was nothing to filter). A LEGACY manifest carrying home entries must
/// still read as matching once user files match — those entries can never
/// match a live journal and must not count.
#[test]
fn oracle_filters_home_entries_on_the_expected_side_too() {
    let jail = tempfile::tempdir().unwrap();
    let proj = jail.path().join("proj");
    let home = proj.join(".doover");
    let store = Store::open(home.join("store")).unwrap();
    fs::write(proj.join("f.txt"), "data").unwrap();
    fs::write(home.join("journal.db"), "J1").unwrap();

    // legacy capture: contains .doover entries
    let m = store.snapshot(&proj, None).unwrap();
    assert!(m.entries.iter().any(|e| e.rel.starts_with(".doover")));
    // the live journal has since drifted (running doover always drifts it)
    fs::write(home.join("journal.db"), "J2-DRIFTED").unwrap();
    assert!(
        store.state_matches(&m).unwrap(),
        "stale under-home entries in a legacy manifest must not read as a \
         permanent conflict"
    );
}

// --- dive round 2 (2026-08-15): racing restores ----------------------------

/// Round-2 racing probe (reproduced live): two concurrent restores interleave
/// the carry machinery destructively — racer B sees live skipped dirs absent
/// (they sit in A's staging), carries nothing, and swap-deletes A's restored
/// tree, live dirs included. Restores now take a non-blocking cross-process
/// flock: while one holds it, a second errors clearly instead of mutating.
#[test]
fn concurrent_restores_are_mutually_excluded() {
    let jail = tempfile::tempdir().unwrap();
    let store = Store::open(jail.path().join("store-home/store")).unwrap();
    let _held = store.lock_restores().expect("first lock acquires");
    // a second handle on the same store (another process, in real life)
    let store2 = Store::open(jail.path().join("store-home/store")).unwrap();
    let err = store2.lock_restores().unwrap_err();
    assert!(
        matches!(err, SnapshotError::RestoreInProgress),
        "the loser must get the clear in-progress refusal, got: {err}"
    );
    drop(_held);
    store2
        .lock_restores()
        .expect("the lock releases with its holder");
}

/// Round-2 perf F1: BOTH hooks walk the full cwd for unknown commands, and
/// `.git` was included — 131 of 231 walked files in a 100-file project,
/// making the unknown tax ~2x the documented model. `.git` is now always
/// walked past (and carried live across restore swaps, like build dirs);
/// a DIRECTLY TARGETED `.git` is still captured in full (root never skipped).
#[test]
fn dot_git_is_walked_past_but_captured_when_targeted() {
    let jail = tempfile::tempdir().unwrap();
    let store = Store::open(jail.path().join("store-home/store")).unwrap();
    let proj = jail.path().join("proj");
    fs::create_dir_all(proj.join("src")).unwrap();
    fs::write(proj.join("src/main.rs"), "code").unwrap();
    fs::create_dir_all(proj.join(".git/objects/ab")).unwrap();
    fs::write(proj.join(".git/HEAD"), "ref: refs/heads/main").unwrap();
    fs::write(proj.join(".git/objects/ab/cdef"), "blob").unwrap();

    // tree walk (the unknown-command cwd fallback shape)
    let m = store
        .snapshot_scoped(&proj, None, &[], &doover_core::snapshot::SkipPolicy::none())
        .unwrap();
    assert!(
        m.entries.iter().all(|e| !e.rel.starts_with(".git")),
        "the walk must not descend .git: {:?}",
        m.entries.iter().map(|e| &e.rel).collect::<Vec<_>>()
    );
    assert!(
        m.skipped_dirs.iter().any(|p| p.ends_with(".git")),
        ".git must be recorded as a skipped (carried) dir"
    );
    assert!(
        m.entries
            .iter()
            .any(|e| e.rel.to_string_lossy().contains("main.rs")),
        "the working tree is still captured"
    );

    // a direct target is the ROOT — never skipped, captured in full
    let direct = store.snapshot(&proj.join(".git"), None).unwrap();
    assert!(
        direct
            .entries
            .iter()
            .any(|e| e.rel.to_string_lossy().contains("HEAD")),
        "an explicitly targeted .git must be captured in full"
    );
}

/// Round-2 review GS1: skipped dirs are part of the described state. PRE
/// skipping .git (it existed) vs POST not skipping it (the command DELETED
/// it) must NOT read as "changed nothing" — that hid opaque `rm -rf .git`
/// from bare undo entirely. Closes the long-standing phase-1 risk note.
#[test]
fn skipped_dir_deletion_is_a_state_change() {
    let jail = tempfile::tempdir().unwrap();
    let store = Store::open(jail.path().join("store-home/store")).unwrap();
    let proj = jail.path().join("proj");
    fs::create_dir_all(proj.join(".git")).unwrap();
    fs::write(proj.join(".git/HEAD"), "ref: refs/heads/main").unwrap();
    fs::write(proj.join("f.txt"), "data").unwrap();
    let skips = doover_core::snapshot::SkipPolicy::none();

    let pre = store.snapshot_scoped(&proj, None, &[], &skips).unwrap();
    assert!(pre.skipped_dirs.iter().any(|p| p.ends_with(".git")));
    fs::remove_dir_all(proj.join(".git")).unwrap();
    let post = store.snapshot_scoped(&proj, None, &[], &skips).unwrap();
    assert!(post.skipped_dirs.is_empty());
    assert!(
        !pre.describes_same_state(&post),
        "deleting a skipped dir must read as a change, or opaque `rm -rf .git` \
         is invisible to bare undo"
    );
}
