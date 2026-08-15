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

/// Undoing the same action twice is a NO-OP, not an error (user-#1 trial).
///
/// This test used to assert a refusal. The refusal was the same status check
/// that, in a slightly different sequence, told a real user their files were
/// unrecoverable while the snapshot sat in the store — so it is gone, and undo
/// now asks the filesystem instead of the status column.
///
/// The property that actually mattered here was never "it errors": it was
/// "doover does not record a duplicate undo or touch anything". That is
/// asserted directly now, which is strictly stronger than what the old
/// assertion implied.
#[test]
fn double_undo_of_the_same_action_is_a_noop_and_records_nothing() {
    let r = rig();
    fs::write(r.cwd.join("f.txt"), "x").unwrap();
    let id = r.run("s1", "t1", "rm f.txt");
    let (j, s) = (r.journal(), r.store());
    engine(&j, &s)
        .undo(Selector::Action(id), false, false)
        .unwrap();
    assert_eq!(
        r.read("f.txt").as_deref(),
        Some("x"),
        "first undo restored it"
    );

    let j2 = r.journal();
    let rep = engine(&j2, &s)
        .undo(Selector::Action(id), false, false)
        .expect("a second undo is a no-op, not a failure");

    assert!(rep.already_satisfied, "reported as already-undone");
    assert_eq!(rep.paths_restored, 0, "nothing was restored again");
    assert!(rep.recorded_as.is_none(), "and NOTHING was journaled");
    assert_eq!(
        r.read("f.txt").as_deref(),
        Some("x"),
        "the file is untouched"
    );

    // the guard that the old refusal was really protecting: exactly one undo row
    let undos = j2
        .session_actions("s1")
        .unwrap()
        .into_iter()
        .filter(|a| a.kind == doover_core::journal::ActionKind::Undo)
        .count();
    assert_eq!(undos, 1, "a second undo row must never be appended");
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

// --- round 18: a truncated PRE capture must never be silently swapped in ---------

/// Restoring a TRUNCATED pre-manifest replaces the whole live tree with the
/// partial capture — every file the snapshot budget/limits skipped would be
/// DELETED BY UNDO. That must be a refusal (conflict, force-gated), never a
/// silent swap: undo must not destroy what it failed to capture.
#[test]
fn undo_refuses_to_swap_in_a_truncated_partial_capture() {
    let mut r = rig();
    r.cfg.limits = Limits {
        max_files: 2, // force truncation of the 4-file pre-snapshot
        max_bytes: u64::MAX,
        max_duration: None,
    };
    for i in 0..4 {
        fs::write(r.cwd.join(format!("f{i}.txt")), format!("content {i}")).unwrap();
    }
    // `chmod -R` classifies destructive over the tree but deletes nothing:
    // the full tree still exists after the action — exactly the state where
    // a partial-swap undo would destroy the uncaptured files
    let id = r.run("s1", "t1", "chmod -R u+w .");
    let j = r.journal();
    let s = r.store();

    let result = engine(&j, &s).undo(Selector::Action(id), false, false);
    match result {
        Err(UndoError::Conflicts(c)) => {
            assert!(
                c.iter().any(|m| m.to_lowercase().contains("truncated")
                    || m.to_lowercase().contains("partial")),
                "refusal must explain the partial capture: {c:?}"
            );
        }
        other => panic!("truncated pre-capture must refuse without --force, got {other:?}"),
    }
    for i in 0..4 {
        assert!(
            r.read(&format!("f{i}.txt")).is_some(),
            "refused undo must leave every live file intact (f{i}.txt)"
        );
    }

    // --force proceeds, eyes open, with a loud warning
    let report = engine(&j, &s)
        .undo(Selector::Action(id), true, false)
        .expect("--force may accept the partial restore");
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.to_lowercase().contains("truncated") || w.to_lowercase().contains("partial")),
        "forced partial restore must warn: {:?}",
        report.warnings
    );
}

// --- dive review 2026-08-15: honest failure surfacing through the engine ---

/// A restore that strands live data in staging must surface the staging path
/// VERBATIM (not "nothing changed, safe to retry") and journal it durably —
/// one stderr line is not a record the user can find tomorrow. Drives the
/// debug-only single-shot markers through the real engine.
#[test]
fn failed_restore_surfaces_preserved_staging_and_journals_it() {
    let r = rig();
    fs::create_dir_all(r.cwd.join("node_modules")).unwrap();
    fs::write(r.cwd.join("node_modules/dep.js"), "SENTINEL").unwrap();
    fs::write(r.cwd.join("data.txt"), "v1").unwrap();
    // partially-unknown line -> cwd manifest with node_modules as a skipped
    // (carried) dir, so the restore has a carry to strand
    let id = r.run("s-sp", "t-sp", "rm -f data.txt; totally-unregistered-zz");

    for m in [
        ".doover-test-restore-swap-fail",
        ".doover-test-restore-moveback-fail",
    ] {
        fs::write(r.cfg.doover_home.join("store").join(m), "").unwrap();
    }
    let j = r.journal();
    let s = r.store();
    let engine = UndoEngine::new(&j, &s);
    let err = engine
        .undo(Selector::Action(id), true, false)
        .expect_err("the injected swap+moveback failure must fail the undo");
    let msg = err.to_string();
    assert!(
        msg.contains("LIVE data") && msg.contains(".doover-restore-"),
        "the error must name the preserved staging verbatim: {msg}"
    );
    assert!(
        !msg.contains("nothing changed"),
        "a stranded-staging failure must never claim nothing changed: {msg}"
    );
    let note = j.action(id).unwrap().note.unwrap_or_default();
    assert!(
        note.contains("left live data in"),
        "the staging path must be journaled durably, got note: {note:?}"
    );
}

/// Complete-or-refused: if the rollback point cannot be captured completely
/// (an unreadable file now sits in the tree), the undo must refuse up front
/// instead of proceeding with a rollback that would delete what it failed to
/// capture on the failure arm.
#[test]
fn truncated_rollback_point_refuses_the_undo() {
    let r = rig();
    fs::write(r.cwd.join("data.txt"), "v1").unwrap();
    let id = r.run("s-tr", "t-tr", "rm -f data.txt; totally-unregistered-zz");

    fs::create_dir_all(r.cwd.join("d")).unwrap();
    fs::write(r.cwd.join("d/locked"), "x").unwrap();
    fs::set_permissions(r.cwd.join("d/locked"), fs::Permissions::from_mode(0o000)).unwrap();

    let j = r.journal();
    let s = r.store();
    let engine = UndoEngine::new(&j, &s);
    let err = engine
        .undo(Selector::Action(id), true, false)
        .expect_err("an incomplete rollback point must refuse");
    fs::set_permissions(r.cwd.join("d/locked"), fs::Permissions::from_mode(0o644)).unwrap();
    assert!(
        err.to_string().contains("complete rollback point"),
        "got: {err}"
    );
    assert_eq!(
        r.read("data.txt"),
        None,
        "refusal must happen BEFORE any restore touches the tree"
    );
}

/// A truncated manifest with zero file entries (every ingest failed — store
/// unwritable) is a pure directory skeleton: nothing restorable, yet
/// truncated→InForce would offer it to bare undo forever, ahead of real
/// candidates. Selection must skip it; the real action behind it must win.
#[test]
fn bare_undo_skips_skeleton_manifests() {
    let r = rig();
    fs::write(r.cwd.join("real.txt"), "keep").unwrap();
    let a = r.run("s-sk", "t-a", "rm -f real.txt");

    // newer action whose every file ingest fails: dir-skeleton manifest
    fs::create_dir_all(r.cwd.join("tree")).unwrap();
    fs::write(r.cwd.join("tree/f"), "x").unwrap();
    let objects = r.cfg.doover_home.join("store/objects");
    fs::set_permissions(&objects, fs::Permissions::from_mode(0o555)).unwrap();
    let ev = hooks::parse_pre_event(&mkjson("s-sk", "t-b", &r.cwd, "rm -rf tree", false)).unwrap();
    hooks::handle_pre(&r.cfg, &ev).unwrap();
    fs::set_permissions(&objects, fs::Permissions::from_mode(0o755)).unwrap();
    std::process::Command::new("bash")
        .args(["--noprofile", "--norc", "-c", "rm -rf tree"])
        .current_dir(&r.cwd)
        .status()
        .unwrap();
    hooks::handle_post(
        &r.cfg,
        &hooks::parse_post_event(&mkjson("s-sk", "t-b", &r.cwd, "rm -rf tree", true)).unwrap(),
    )
    .unwrap();

    let j = r.journal();
    let s = r.store();
    let engine = UndoEngine::new(&j, &s);
    let report = engine
        .undo(Selector::Latest, false, false)
        .expect("bare undo must skip the skeleton and reach the real action");
    assert_eq!(
        report.target_action, a,
        "the skeleton must not hijack selection"
    );
    assert_eq!(r.read("real.txt").as_deref(), Some("keep"));
}

// --- Phase D review 2026-08-15: background commands and undo ---------------

impl Rig {
    /// Pre + post with run_in_background=true injected into tool_input, like
    /// harness 2.1.232 sends it (measured: the flag rides on BOTH events and
    /// the post fires at tool return, before the command's effects exist).
    /// The command is NOT executed — that is the point.
    fn run_background(&self, session: &str, tool: &str, cmd: &str) -> i64 {
        let bg = |json: String| {
            let mut v: serde_json::Value = serde_json::from_str(&json).unwrap();
            v["tool_input"]["run_in_background"] = serde_json::json!(true);
            v.to_string()
        };
        let pre =
            hooks::parse_pre_event(&bg(mkjson(session, tool, &self.cwd, cmd, false))).unwrap();
        let out = hooks::handle_pre(&self.cfg, &pre).unwrap();
        let post =
            hooks::parse_post_event(&bg(mkjson(session, tool, &self.cwd, cmd, true))).unwrap();
        hooks::handle_post(&self.cfg, &post).unwrap();
        out.action_id
    }
}

/// A background no-op (a dev server) must NEVER be offered by bare undo —
/// its outcome is unverifiable, and offering it dead-ends users in a --force
/// whose execution reverts post-launch work. Bare undo must skip it AND
/// still find the genuine destructive action behind it.
#[test]
fn bare_undo_skips_background_actions_and_finds_real_ones_behind() {
    let r = rig();
    fs::write(r.cwd.join("real.txt"), "keep").unwrap();
    let real = r.run("s-bgu", "t-real", "rm -f real.txt");
    // newer background "server" — unknown command, defensive cwd PRE, no POST
    let _bg = r.run_background("s-bgu", "t-srv", "python server.py");
    // the world drifts (an Edit-tool change no Bash hook sees)
    fs::write(r.cwd.join("app.js"), "edited later").unwrap();

    let j = r.journal();
    let s = r.store();
    let engine = UndoEngine::new(&j, &s);
    let report = engine
        .undo(Selector::Latest, false, false)
        .expect("bare undo must skip the background action, not refuse on it");
    assert_eq!(
        report.target_action, real,
        "the background no-op must not shadow the genuine destructive action"
    );
    assert_eq!(r.read("real.txt").as_deref(), Some("keep"));
    assert_eq!(
        r.read("app.js").as_deref(),
        Some("edited later"),
        "the skip means later edits are never collateral"
    );
}

/// Explicit `undo <id>` of a background action: refuse without --force with
/// a message that says WHY (by design, not "may have failed") and what
/// forcing costs; with --force it proceeds, warns about redo, and a
/// subsequent redo errors cleanly naming the background cause.
#[test]
fn explicit_undo_of_background_action_is_honest_end_to_end() {
    let r = rig();
    fs::write(r.cwd.join("f.txt"), "v1").unwrap();
    let id = r.run_background("s-bge", "t-bge", "bash slow-task.sh");
    // the background task later DID change something
    fs::write(r.cwd.join("f.txt"), "v2-from-background-task").unwrap();

    let j = r.journal();
    let s = r.store();
    let engine = UndoEngine::new(&j, &s);
    let err = engine.undo(Selector::Action(id), false, false).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("background") && msg.contains("cannot be redone"),
        "the refusal must diagnose the background design and the force cost: {msg}"
    );
    assert!(
        !msg.contains("may have failed"),
        "a background command did not fail; the message must not say so: {msg}"
    );

    let report = engine
        .undo(Selector::Action(id), true, false)
        .expect("--force must proceed");
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.contains("cannot be redone")),
        "the forced undo must warn about the one-way door: {:?}",
        report.warnings
    );
    assert_eq!(r.read("f.txt").as_deref(), Some("v1"));

    let redo_err = engine.redo(Selector::Latest, false, false).unwrap_err();
    assert!(
        redo_err.to_string().contains("background"),
        "redo must explain the background cause, not 'may have failed': {redo_err}"
    );
}
