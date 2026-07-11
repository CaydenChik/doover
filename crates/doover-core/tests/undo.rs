//! T7 — undo/redo engine (doover-implementation-plan.md §3, step 6).
//! Written before the undo module exists; drives its design.
//!
//! Model: the hook engine attaches a PRE manifest (state before the command)
//! at handle_pre and a POST manifest (state after) at handle_post. Undo
//! restores PRE; redo restores POST. POST also answers "is the world still as
//! our action left it?" for conflict detection.

use doover_core::hooks::{self, HookConfig, UnknownPolicy};
use doover_core::journal::{ActionStatus, Journal};
use doover_core::snapshot::{Limits, Store};
use doover_core::undo::{Selector, UndoEngine, UndoError};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

struct Rig {
    _tmp: tempfile::TempDir,
    cfg: HookConfig,
    cwd: PathBuf,
}

fn rig() -> Rig {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().join("proj");
    let home = tmp.path().join("home");
    let doover_home = tmp.path().join(".doover");
    fs::create_dir_all(&cwd).unwrap();
    fs::create_dir_all(&home).unwrap();
    Rig {
        _tmp: tmp,
        cfg: HookConfig {
            doover_home,
            home,
            limits: Limits {
                max_files: 100_000,
                max_bytes: 5 << 30,
                max_duration: None,
            },
            unknown_policy: UnknownPolicy::SnapshotCwd,
            maintenance: doover_core::maintenance::MaintenanceBudget::disabled(),
        },
        cwd,
    }
}

impl Rig {
    /// Pre-event + run the command, but NO post-event, then abandon it — the
    /// shape of a failed command (audit round 10). Returns the action id.
    fn run_failed(&self, session: &str, tool: &str, cmd: &str) -> i64 {
        let ev = hooks::parse_pre_event(&mkjson(session, tool, &self.cwd, cmd, false)).unwrap();
        let out = hooks::handle_pre(&self.cfg, &ev).unwrap();
        std::process::Command::new("bash")
            .args(["--noprofile", "--norc", "-c", cmd])
            .current_dir(&self.cwd)
            .status()
            .unwrap();
        self.journal().end_session(session).unwrap(); // abandons the pending action
        out.action_id
    }

    /// Drive a command through the REAL engine: pre-event, run the command for
    /// real, post-event. Returns the action id.
    fn run(&self, session: &str, tool: &str, cmd: &str) -> i64 {
        let pre = mkjson(session, tool, &self.cwd, cmd, false);
        let ev = hooks::parse_pre_event(&pre).unwrap();
        let outcome = hooks::handle_pre(&self.cfg, &ev).unwrap();
        std::process::Command::new("bash")
            .args(["--noprofile", "--norc", "-c", cmd])
            .current_dir(&self.cwd)
            .status()
            .unwrap();
        let post = mkjson(session, tool, &self.cwd, cmd, true);
        hooks::handle_post(&self.cfg, &hooks::parse_post_event(&post).unwrap()).unwrap();
        outcome.action_id
    }

    fn journal(&self) -> Journal {
        Journal::open(&self.cfg.doover_home.join("journal.db")).unwrap()
    }
    fn store(&self) -> Store {
        Store::open(self.cfg.doover_home.join("store")).unwrap()
    }
    fn read(&self, rel: &str) -> Option<String> {
        fs::read_to_string(self.cwd.join(rel)).ok()
    }
}

fn mkjson(session: &str, tool: &str, cwd: &Path, cmd: &str, post: bool) -> String {
    let mut v = serde_json::json!({
        "session_id": session, "cwd": cwd.to_string_lossy(),
        "tool_name": "Bash", "tool_use_id": tool,
        "tool_input": { "command": cmd },
        "hook_event_name": if post { "PostToolUse" } else { "PreToolUse" },
    });
    if post {
        v["duration_ms"] = serde_json::json!(5);
        v["tool_response"] = serde_json::json!({"stdout":"","stderr":"","interrupted":false});
    }
    v.to_string()
}

fn engine<'a>(j: &'a Journal, s: &'a Store) -> UndoEngine<'a> {
    UndoEngine::new(j, s)
}

// --- the canonical demo: undo a real rm ------------------------------------------

#[test]
fn undo_latest_restores_a_deleted_directory() {
    let r = rig();
    fs::create_dir_all(r.cwd.join("photos")).unwrap();
    fs::write(r.cwd.join("photos/wedding.jpg"), "irreplaceable").unwrap();

    r.run("s1", "t1", "rm -rf photos");
    assert!(r.read("photos/wedding.jpg").is_none(), "the rm really ran");

    let (j, s) = (r.journal(), r.store());
    let report = engine(&j, &s).undo(Selector::Latest, false, false).unwrap();
    assert_eq!(report.paths_restored, 1);
    assert_eq!(
        r.read("photos/wedding.jpg").as_deref(),
        Some("irreplaceable")
    );
}

#[test]
fn dry_run_reports_the_plan_without_touching_disk() {
    let r = rig();
    fs::write(r.cwd.join("notes.txt"), "original").unwrap();
    r.run("s1", "t1", "echo clobbered > notes.txt");
    assert_eq!(r.read("notes.txt").as_deref(), Some("clobbered\n"));

    let (j, s) = (r.journal(), r.store());
    let plan = engine(&j, &s).undo(Selector::Latest, false, true).unwrap();
    assert!(plan.dry_run);
    assert!(!plan.plan.is_empty());
    assert_eq!(
        r.read("notes.txt").as_deref(),
        Some("clobbered\n"),
        "dry-run must not write"
    );
}

// --- redo -------------------------------------------------------------------------

#[test]
fn redo_reapplies_the_undone_effect() {
    let r = rig();
    fs::write(r.cwd.join("f.txt"), "before").unwrap();
    r.run("s1", "t1", "echo after > f.txt");

    let (j, s) = (r.journal(), r.store());
    engine(&j, &s).undo(Selector::Latest, false, false).unwrap();
    assert_eq!(
        r.read("f.txt").as_deref(),
        Some("before"),
        "undo restored pre-state"
    );

    let j2 = r.journal();
    engine(&j2, &s)
        .redo(Selector::Latest, false, false)
        .unwrap();
    assert_eq!(
        r.read("f.txt").as_deref(),
        Some("after\n"),
        "redo re-applied the command's effect"
    );
}

// --- conflict detection -----------------------------------------------------------

#[test]
fn undo_refuses_when_the_file_changed_since_the_action() {
    let r = rig();
    fs::write(r.cwd.join("f.txt"), "v1").unwrap();
    r.run("s1", "t1", "echo v2 > f.txt");
    // the user edits the file AFTER the agent's action, BEFORE undo
    fs::write(r.cwd.join("f.txt"), "user's own work").unwrap();

    let (j, s) = (r.journal(), r.store());
    let err = engine(&j, &s)
        .undo(Selector::Latest, false, false)
        .unwrap_err();
    assert!(matches!(err, UndoError::Conflicts(_)), "got {err:?}");
    assert_eq!(
        r.read("f.txt").as_deref(),
        Some("user's own work"),
        "refused undo must not clobber"
    );
}

#[test]
fn force_overrides_a_conflict() {
    let r = rig();
    fs::write(r.cwd.join("f.txt"), "v1").unwrap();
    r.run("s1", "t1", "echo v2 > f.txt");
    fs::write(r.cwd.join("f.txt"), "user's own work").unwrap();

    let (j, s) = (r.journal(), r.store());
    let report = engine(&j, &s).undo(Selector::Latest, true, false).unwrap();
    assert!(report.forced);
    assert_eq!(
        r.read("f.txt").as_deref(),
        Some("v1"),
        "force restores pre-state anyway"
    );
}

#[test]
fn undo_refuses_a_later_overlapping_action() {
    let r = rig();
    fs::write(r.cwd.join("shared.txt"), "gen0").unwrap();
    let first = r.run("s1", "t1", "echo gen1 > shared.txt");
    r.run("s1", "t2", "echo gen2 > shared.txt"); // later action touches the same path

    // undoing the FIRST action would clobber the second's result
    let (j, s) = (r.journal(), r.store());
    let err = engine(&j, &s)
        .undo(Selector::Action(first), false, false)
        .unwrap_err();
    assert!(matches!(err, UndoError::Conflicts(_)), "got {err:?}");
}

// --- selection & edge cases -------------------------------------------------------

#[test]
fn undo_of_a_safe_action_has_nothing_to_restore() {
    let r = rig();
    let id = r.run("s1", "t1", "ls");
    let (j, s) = (r.journal(), r.store());
    let err = engine(&j, &s)
        .undo(Selector::Action(id), false, false)
        .unwrap_err();
    assert!(
        matches!(err, UndoError::NothingToRestore { .. }),
        "got {err:?}"
    );
}

#[test]
fn double_undo_of_the_same_action_is_refused() {
    let r = rig();
    fs::write(r.cwd.join("f.txt"), "x").unwrap();
    let id = r.run("s1", "t1", "rm f.txt");
    let (j, s) = (r.journal(), r.store());
    engine(&j, &s)
        .undo(Selector::Action(id), false, false)
        .unwrap();
    let j2 = r.journal();
    let err = engine(&j2, &s)
        .undo(Selector::Action(id), false, false)
        .unwrap_err();
    assert!(
        matches!(err, UndoError::NotUndoable { .. } | UndoError::Journal(_)),
        "got {err:?}"
    );
}

#[test]
fn undo_with_no_undoable_history_is_a_clear_error() {
    let r = rig();
    r.run("s1", "t1", "ls"); // safe only
    let (j, s) = (r.journal(), r.store());
    let err = engine(&j, &s)
        .undo(Selector::Latest, false, false)
        .unwrap_err();
    assert!(matches!(err, UndoError::NoUndoableAction), "got {err:?}");
}

// --- audit round 10 regressions ---------------------------------------------------

#[test]
fn undo_of_a_failed_command_refuses_without_a_post_oracle() {
    // an abandoned (failed) action has no post-state to verify against: undo
    // must refuse-by-default rather than silently clobber later work
    let r = rig();
    fs::write(r.cwd.join("f.txt"), "v1").unwrap();
    r.run_failed("s1", "t1", "echo v2 > f.txt");
    fs::write(r.cwd.join("f.txt"), "user's own work").unwrap();

    let (j, s) = (r.journal(), r.store());
    let err = engine(&j, &s)
        .undo(Selector::Latest, false, false)
        .unwrap_err();
    assert!(matches!(err, UndoError::Conflicts(_)), "got {err:?}");
    assert_eq!(
        r.read("f.txt").as_deref(),
        Some("user's own work"),
        "must not clobber"
    );

    // --force still lets the user proceed deliberately
    let j2 = r.journal();
    engine(&j2, &s).undo(Selector::Latest, true, false).unwrap();
    assert_eq!(r.read("f.txt").as_deref(), Some("v1"));
}

#[test]
fn a_failed_restore_rolls_back_and_leaves_the_target_retryable() {
    // audit round 10: record-after-restore. A restore failure must NOT mark
    // the action 'undone' (a lie) — the world rolls back and undo can retry.
    let r = rig();
    fs::create_dir_all(r.cwd.join("a")).unwrap();
    fs::write(r.cwd.join("a/x.txt"), "A-original").unwrap();
    let id = r.run("s1", "t1", "rm -rf a");
    assert!(r.read("a/x.txt").is_none());

    // make the cwd read-only so restoring `a` cannot create the directory
    fs::set_permissions(&r.cwd, fs::Permissions::from_mode(0o555)).unwrap();
    let (j, s) = (r.journal(), r.store());
    let err = engine(&j, &s)
        .undo(Selector::Action(id), false, false)
        .unwrap_err();
    fs::set_permissions(&r.cwd, fs::Permissions::from_mode(0o755)).unwrap();

    assert!(
        matches!(
            err,
            UndoError::PartialRolledBack { .. } | UndoError::Snapshot(_)
        ),
        "a failed restore must report a rollback/pre-flight error, got {err:?}"
    );
    // the crucial invariant: the target is NOT marked undone
    assert_eq!(
        j.action(id).unwrap().status,
        ActionStatus::Completed,
        "a failed undo must leave the target retryable, not lie that it succeeded"
    );

    // and retry now succeeds (perms restored)
    let j2 = r.journal();
    engine(&j2, &s)
        .undo(Selector::Action(id), false, false)
        .unwrap();
    assert_eq!(
        r.read("a/x.txt").as_deref(),
        Some("A-original"),
        "retry restores"
    );
    assert_eq!(j2.action(id).unwrap().status, ActionStatus::Undone);
}

#[test]
fn multipath_failure_rolls_back_the_already_restored_path() {
    // round-10 follow-up: the rollback LOOP itself (path 0 restored, path 1
    // fails, path 0 must return to its pre-undo state). The earlier regression
    // only exercised the i=0 branch where nothing had been restored yet.
    let r = rig();
    fs::create_dir_all(r.cwd.join("sub1")).unwrap();
    fs::create_dir_all(r.cwd.join("sub2")).unwrap();
    fs::write(r.cwd.join("sub1/a.txt"), "A-original").unwrap();
    fs::write(r.cwd.join("sub2/b.txt"), "B-original").unwrap();
    let id = r.run("s1", "t1", "rm sub1/a.txt sub2/b.txt");
    assert!(r.read("sub1/a.txt").is_none() && r.read("sub2/b.txt").is_none());

    // only sub2 becomes read-only: restoring a.txt succeeds, b.txt fails
    fs::set_permissions(r.cwd.join("sub2"), fs::Permissions::from_mode(0o555)).unwrap();
    let (j, s) = (r.journal(), r.store());
    let err = engine(&j, &s)
        .undo(Selector::Action(id), false, false)
        .unwrap_err();
    fs::set_permissions(r.cwd.join("sub2"), fs::Permissions::from_mode(0o755)).unwrap();

    assert!(
        matches!(err, UndoError::PartialRolledBack { .. }),
        "got {err:?}"
    );
    assert!(
        r.read("sub1/a.txt").is_none(),
        "the successfully-restored path must be rolled back to its pre-undo (absent) state"
    );
    assert_eq!(
        j.action(id).unwrap().status,
        ActionStatus::Completed,
        "retryable"
    );

    // retry succeeds once the obstacle is gone
    let j2 = r.journal();
    engine(&j2, &s)
        .undo(Selector::Action(id), false, false)
        .unwrap();
    assert_eq!(r.read("sub1/a.txt").as_deref(), Some("A-original"));
    assert_eq!(r.read("sub2/b.txt").as_deref(), Some("B-original"));
}

#[test]
fn undo_recreates_a_file_the_command_created() {
    // pre-state was ABSENT (file didn't exist): undo must DELETE it
    let r = rig();
    r.run("s1", "t1", "echo hi > created.txt");
    assert_eq!(r.read("created.txt").as_deref(), Some("hi\n"));
    let (j, s) = (r.journal(), r.store());
    engine(&j, &s).undo(Selector::Latest, false, false).unwrap();
    assert!(
        r.read("created.txt").is_none(),
        "undo of a creation deletes the file"
    );
}
