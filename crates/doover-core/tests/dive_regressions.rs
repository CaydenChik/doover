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
