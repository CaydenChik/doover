//! T7 — undo/redo engine (doover-implementation-plan.md §3, step 6).
//! Written before the undo module exists; drives its design.
//!
//! Model: the hook engine attaches a PRE manifest (state before the command)
//! at handle_pre and a POST manifest (state after) at handle_post. Undo
//! restores PRE; redo restores POST. POST also answers "is the world still as
//! our action left it?" for conflict detection.

use doover_core::hooks::{self, HookConfig, UnknownPolicy};
use doover_core::journal::Journal;
use doover_core::snapshot::{Limits, Store};
use doover_core::undo::{Selector, UndoEngine, UndoError};
use std::fs;
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
            },
            unknown_policy: UnknownPolicy::SnapshotCwd,
        },
        cwd,
    }
}

impl Rig {
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
