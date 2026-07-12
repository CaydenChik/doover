//! T6 — hook engine (doover-implementation-plan.md §3). Written before the
//! hooks module exists; drives its design.
//!
//! The engine is the composition point: parse harness JSON → resolve scope →
//! snapshot (ALWAYS under limits — carried-forward requirement) → journal.
//! Fail-open lives in the binary; the library surfaces honest errors.

use doover_core::hooks::{self, HookConfig, UnknownPolicy};
use doover_core::journal::{ActionStatus, Journal};
use doover_core::snapshot::Limits;
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
    let cwd = tmp.path().join("project");
    let home = tmp.path().join("home");
    fs::create_dir_all(&cwd).unwrap();
    fs::create_dir_all(&home).unwrap();
    let cfg = HookConfig {
        doover_home: tmp.path().join(".doover"),
        home,
        limits: Limits {
            max_files: 100_000,
            max_bytes: 5 * 1024 * 1024 * 1024,
            max_duration: None,
        },
        unknown_policy: UnknownPolicy::SnapshotCwd,
        maintenance: doover_core::maintenance::MaintenanceBudget::disabled(),
    };
    Rig {
        _tmp: tmp,
        cfg,
        cwd,
    }
}

fn pre_json(session: &str, tool_use: &str, cwd: &Path, command: &str) -> String {
    serde_json::json!({
        "session_id": session,
        "transcript_path": "/tmp/t.jsonl",
        "cwd": cwd.to_string_lossy(),
        "permission_mode": "default",
        "hook_event_name": "PreToolUse",
        "prompt_id": "p-1",
        "tool_name": "Bash",
        "tool_use_id": tool_use,
        "tool_input": { "command": command }
    })
    .to_string()
}

fn post_json(session: &str, tool_use: &str, cwd: &Path, command: &str) -> String {
    serde_json::json!({
        "session_id": session,
        "transcript_path": "/tmp/t.jsonl",
        "cwd": cwd.to_string_lossy(),
        "permission_mode": "default",
        "hook_event_name": "PostToolUse",
        "prompt_id": "p-1",
        "tool_name": "Bash",
        "tool_use_id": tool_use,
        "duration_ms": 42,
        "tool_input": { "command": command },
        "tool_response": { "stdout": "", "stderr": "", "interrupted": false }
    })
    .to_string()
}

fn journal(cfg: &HookConfig) -> Journal {
    Journal::open(&cfg.doover_home.join("journal.db")).unwrap()
}

// --- golden fixtures parse through the real event parser -------------------------

#[test]
fn all_golden_fixtures_parse() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/hook-events");
    let mut pre = 0;
    let mut post = 0;
    for entry in fs::read_dir(dir).unwrap().filter_map(Result::ok) {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.ends_with(".json") {
            continue;
        }
        let text = fs::read_to_string(entry.path()).unwrap();
        if name.starts_with("pre_") {
            let e = hooks::parse_pre_event(&text).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert!(!e.command.is_empty(), "{name}");
            assert!(e.cwd.is_absolute(), "{name}");
            pre += 1;
        } else {
            let e = hooks::parse_post_event(&text).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert!(e.duration_ms > 0, "{name}");
            post += 1;
        }
    }
    assert!(
        pre >= 5 && post >= 3,
        "fixture set shrank: {pre} pre / {post} post"
    );
}

// --- pre: journal + snapshot behavior --------------------------------------------

#[test]
fn destructive_pre_snapshots_under_limits_and_journals() {
    let r = rig();
    fs::create_dir_all(r.cwd.join("build")).unwrap();
    fs::write(r.cwd.join("build/a.txt"), "A").unwrap();

    let ev = hooks::parse_pre_event(&pre_json("s1", "t1", &r.cwd, "rm -rf build")).unwrap();
    hooks::handle_pre(&r.cfg, &ev).unwrap();

    let j = journal(&r.cfg);
    let actions = j.session_actions("s1").unwrap();
    assert_eq!(actions.len(), 1);
    let a = &actions[0];
    assert_eq!(a.status, ActionStatus::Pending);
    assert_eq!(a.effect, "destructive");
    assert_eq!(a.rule_id.as_deref(), Some("coreutils.rm"));
    assert!(!a.has_unknown);

    let manifests = j.manifests(a.id).unwrap();
    assert_eq!(manifests.len(), 1, "one scoped path, one manifest");
    assert!(manifests[0].entries.len() >= 2, "dir + file captured");
}

#[test]
fn safe_pre_journals_without_snapshotting() {
    let r = rig();
    let ev = hooks::parse_pre_event(&pre_json("s1", "t1", &r.cwd, "ls -la")).unwrap();
    hooks::handle_pre(&r.cfg, &ev).unwrap();

    let j = journal(&r.cfg);
    let a = &j.session_actions("s1").unwrap()[0];
    assert_eq!(a.effect, "safe");
    assert!(
        j.manifests(a.id).unwrap().is_empty(),
        "safe actions snapshot nothing"
    );
}

#[test]
fn unknown_pre_snapshots_cwd_bounded() {
    let r = rig();
    fs::write(r.cwd.join("work.txt"), "state").unwrap();

    let ev = hooks::parse_pre_event(&pre_json("s1", "t1", &r.cwd, "eval \"$CLEANUP\"")).unwrap();
    hooks::handle_pre(&r.cfg, &ev).unwrap();

    let j = journal(&r.cfg);
    let a = &j.session_actions("s1").unwrap()[0];
    assert!(a.has_unknown);
    let manifests = j.manifests(a.id).unwrap();
    assert_eq!(manifests.len(), 1, "unknown policy snapshots the cwd");
    assert_eq!(manifests[0].path, r.cwd);
}

#[test]
fn unknown_passthrough_policy_skips_the_cwd_snapshot() {
    let mut r = rig();
    r.cfg.unknown_policy = UnknownPolicy::Passthrough;
    let ev = hooks::parse_pre_event(&pre_json("s1", "t1", &r.cwd, "eval \"$CLEANUP\"")).unwrap();
    hooks::handle_pre(&r.cfg, &ev).unwrap();

    let j = journal(&r.cfg);
    let a = &j.session_actions("s1").unwrap()[0];
    assert!(a.has_unknown, "the gap is still journaled loudly");
    assert!(j.manifests(a.id).unwrap().is_empty());
}

/// Carried-forward requirement: limits apply to KNOWN-destructive scopes, not
/// just the unknown policy — and truncation is a loud, journaled gap.
#[test]
fn limits_bound_known_destructive_scopes_and_note_the_gap() {
    let mut r = rig();
    r.cfg.limits = Limits {
        max_files: 3,
        max_bytes: u64::MAX,
        max_duration: None,
    };
    fs::create_dir_all(r.cwd.join("big")).unwrap();
    for i in 0..10 {
        fs::write(r.cwd.join(format!("big/f{i}.txt")), "x").unwrap();
    }

    let ev = hooks::parse_pre_event(&pre_json("s1", "t1", &r.cwd, "rm -rf big")).unwrap();
    hooks::handle_pre(&r.cfg, &ev).unwrap();

    let j = journal(&r.cfg);
    let a = &j.session_actions("s1").unwrap()[0];
    let manifests = j.manifests(a.id).unwrap();
    assert!(manifests[0].truncated, "limits must bound the snapshot");
    assert!(
        a.note.as_deref().is_some_and(|n| n.contains("truncated")),
        "truncation must be a loud journaled gap, note: {:?}",
        a.note
    );
}

#[test]
fn user_registry_overlay_is_honored() {
    let r = rig();
    let overlay = r.cfg.doover_home.join("registry.d");
    fs::create_dir_all(&overlay).unwrap();
    fs::write(
        overlay.join("custom.yaml"),
        "rules:\n  - id: my.nuke\n    match: { command: nuke }\n    effect: destructive\n    scope: { paths: positional }\n    undo: snapshot-restore\n",
    )
    .unwrap();
    fs::write(r.cwd.join("data.txt"), "x").unwrap();

    let ev = hooks::parse_pre_event(&pre_json("s1", "t1", &r.cwd, "nuke data.txt")).unwrap();
    hooks::handle_pre(&r.cfg, &ev).unwrap();

    let j = journal(&r.cfg);
    let a = &j.session_actions("s1").unwrap()[0];
    assert_eq!(a.rule_id.as_deref(), Some("my.nuke"));
    assert_eq!(j.manifests(a.id).unwrap().len(), 1);
}

/// Audit round 9 (the fail-open dual): a DESTRUCTIVE action whose snapshot
/// fails must surface a loud protection gap, not pass silently. The engine
/// carries the gaps up so the binary can warn while still exiting 0.
#[test]
fn destructive_with_failed_snapshot_reports_a_protection_gap() {
    let r = rig();
    fs::create_dir_all(r.cwd.join("build")).unwrap();
    fs::write(r.cwd.join("build/a.txt"), "precious").unwrap();

    // a destructive priming action creates the store; then jam its object dir
    // read-only so the real action's writes fail
    fs::write(r.cwd.join("prime.txt"), "x").unwrap();
    hooks::handle_pre(
        &r.cfg,
        &hooks::parse_pre_event(&pre_json("s0", "t0", &r.cwd, "rm prime.txt")).unwrap(),
    )
    .unwrap();
    let objects = r.cfg.doover_home.join("store/objects");
    fs::set_permissions(&objects, std::fs::Permissions::from_mode(0o555)).unwrap();

    let ev = hooks::parse_pre_event(&pre_json("s1", "t1", &r.cwd, "rm -rf build")).unwrap();
    let outcome = hooks::handle_pre(&r.cfg, &ev).unwrap();
    fs::set_permissions(&objects, std::fs::Permissions::from_mode(0o755)).unwrap();

    assert!(outcome.severity >= doover_core::resolver::Severity::Destructive);
    assert_eq!(
        outcome.manifests_attached, 0,
        "the snapshot could not be written"
    );
    assert!(
        !outcome.gaps.is_empty(),
        "a destructive action with a failed snapshot must report a gap for the binary to warn on"
    );
    assert!(outcome.gaps.iter().any(|g| g.contains("UNPROTECTED")));
}

#[test]
fn fully_protected_destructive_reports_no_gap() {
    // the dual: don't cry wolf. A clean destructive snapshot has no gaps (the
    // binary must stay quiet to avoid the 93%-approval alarm-fatigue trap).
    let r = rig();
    fs::create_dir_all(r.cwd.join("build")).unwrap();
    fs::write(r.cwd.join("build/a.txt"), "A").unwrap();
    let ev = hooks::parse_pre_event(&pre_json("s1", "t1", &r.cwd, "rm -rf build")).unwrap();
    let outcome = hooks::handle_pre(&r.cfg, &ev).unwrap();
    assert!(outcome.manifests_attached >= 1);
    assert!(
        outcome.gaps.is_empty(),
        "a clean snapshot must not warn: {:?}",
        outcome.gaps
    );
}

#[test]
fn truncated_snapshot_of_a_destructive_action_is_a_gap() {
    let mut r = rig();
    r.cfg.limits = Limits {
        max_files: 2,
        max_bytes: u64::MAX,
        max_duration: None,
    };
    fs::create_dir_all(r.cwd.join("big")).unwrap();
    for i in 0..10 {
        fs::write(r.cwd.join(format!("big/f{i}.txt")), "x").unwrap();
    }
    let ev = hooks::parse_pre_event(&pre_json("s1", "t1", &r.cwd, "rm -rf big")).unwrap();
    let outcome = hooks::handle_pre(&r.cfg, &ev).unwrap();
    assert!(
        outcome.gaps.iter().any(|g| g.contains("truncated")),
        "a truncated (partial) snapshot is a protection gap: {:?}",
        outcome.gaps
    );
}

#[test]
fn unknown_command_with_a_truncated_defensive_snapshot_warns() {
    // the dual of the round-9 fix: an UNKNOWN command snapshots the cwd
    // defensively BECAUSE it might be destructive. If that snapshot is
    // incomplete, the warning must fire even though severity < Destructive.
    let mut r = rig();
    r.cfg.limits = Limits {
        max_files: 2,
        max_bytes: u64::MAX,
        max_duration: None,
    };
    for i in 0..10 {
        fs::write(r.cwd.join(format!("f{i}.txt")), "x").unwrap();
    }
    let ev = hooks::parse_pre_event(&pre_json("s1", "t1", &r.cwd, "eval \"$CLEANUP\"")).unwrap();
    let outcome = hooks::handle_pre(&r.cfg, &ev).unwrap();

    assert_eq!(outcome.severity, doover_core::resolver::Severity::Unknown);
    assert!(
        !outcome.gaps.is_empty(),
        "truncated defensive snapshot is a gap"
    );
    assert!(
        outcome.needs_warning(),
        "an unknown command's incomplete defensive snapshot MUST warn (severity < Destructive)"
    );
}

// --- post ------------------------------------------------------------------------

#[test]
fn post_completes_the_pending_action_by_tool_use_id() {
    let r = rig();
    let ev = hooks::parse_pre_event(&pre_json("s1", "t9", &r.cwd, "ls")).unwrap();
    hooks::handle_pre(&r.cfg, &ev).unwrap();

    let post = hooks::parse_post_event(&post_json("s1", "t9", &r.cwd, "ls")).unwrap();
    hooks::handle_post(&r.cfg, &post).unwrap();

    let j = journal(&r.cfg);
    let a = &j.session_actions("s1").unwrap()[0];
    assert_eq!(a.status, ActionStatus::Completed);
    assert_eq!(a.duration_ms, Some(42));
}

#[test]
fn post_without_matching_pre_is_an_error_for_the_bin_to_fail_open() {
    let r = rig();
    let post = hooks::parse_post_event(&post_json("s1", "ghost", &r.cwd, "ls")).unwrap();
    assert!(hooks::handle_post(&r.cfg, &post).is_err());
}

// --- parsing robustness ------------------------------------------------------------

#[test]
fn malformed_and_foreign_events_error_cleanly() {
    assert!(hooks::parse_pre_event("{ not json").is_err());
    assert!(hooks::parse_pre_event("{}").is_err());
    // a non-Bash tool event must be recognized as not-ours, not misparsed
    let foreign = serde_json::json!({
        "session_id": "s", "cwd": "/tmp", "hook_event_name": "PreToolUse",
        "tool_name": "Edit", "tool_use_id": "t",
        "tool_input": { "file_path": "/tmp/x" }
    })
    .to_string();
    let parsed = hooks::parse_pre_event(&foreign);
    assert!(
        parsed.is_err() || parsed.is_ok_and(|e| e.tool_name != "Bash"),
        "foreign tools must be distinguishable"
    );
}

/// The end-to-end promise, driven through the REAL engine: pre → the actual
/// rm runs → restore from the journaled manifests brings the data back.
#[test]
fn engine_snapshots_are_sufficient_to_undo_the_real_command() {
    let r = rig();
    fs::create_dir_all(r.cwd.join("photos")).unwrap();
    fs::write(r.cwd.join("photos/one.jpg"), "memories").unwrap();

    let ev = hooks::parse_pre_event(&pre_json("s1", "t1", &r.cwd, "rm -rf photos")).unwrap();
    hooks::handle_pre(&r.cfg, &ev).unwrap();

    let st = std::process::Command::new("bash")
        .args(["--noprofile", "--norc", "-c", "rm -rf photos"])
        .current_dir(&r.cwd)
        .status()
        .unwrap();
    assert!(st.success());
    assert!(!r.cwd.join("photos/one.jpg").exists());

    // restore straight from what the engine journaled
    let j = journal(&r.cfg);
    let a = &j.session_actions("s1").unwrap()[0];
    let store = doover_core::snapshot::Store::open(r.cfg.doover_home.join("store")).unwrap();
    for m in j.manifests(a.id).unwrap() {
        store.restore(&m).unwrap();
    }
    assert_eq!(
        fs::read_to_string(r.cwd.join("photos/one.jpg")).unwrap(),
        "memories"
    );
}

/// bench D1: the time-budget cutoff must flow through the SAME loud, journaled
/// protection-gap path as the file/byte limits — a destructive command whose
/// snapshot ran out of time is UNPROTECTED and must say so, never a SIGKILL
/// with nothing recorded.
#[test]
fn a_snapshot_time_budget_is_a_loud_journaled_gap() {
    let mut r = rig();
    r.cfg.limits = Limits {
        max_files: u64::MAX,
        max_bytes: u64::MAX,
        max_duration: Some(std::time::Duration::ZERO),
    };
    fs::create_dir_all(r.cwd.join("big")).unwrap();
    for i in 0..10 {
        fs::write(r.cwd.join(format!("big/f{i}.txt")), "x").unwrap();
    }
    let ev = hooks::parse_pre_event(&pre_json("s1", "t1", &r.cwd, "rm -rf big")).unwrap();
    let outcome = hooks::handle_pre(&r.cfg, &ev).unwrap();
    assert!(
        outcome.gaps.iter().any(|g| g.contains("truncated")),
        "a time-budget cutoff is a protection gap: {:?}",
        outcome.gaps
    );
    let j = journal(&r.cfg);
    let a = &j.session_actions("s1").unwrap()[0];
    assert!(
        j.manifests(a.id).unwrap()[0].truncated,
        "manifest must be truncated"
    );
    assert!(
        a.note.as_deref().is_some_and(|n| n.contains("truncated")),
        "note: {:?}",
        a.note
    );
}

// --- D2: automatic gc trigger in the post hook --------------------------------

/// The post hook runs gc when the maintenance budget's cadence fires. On an
/// all-fresh store eviction can't touch anything (hot window + grace), so the
/// observable proof that gc RAN is its tmp sweep: a stale (2h) tmp file is
/// reaped by the triggered gc.
#[test]
fn post_hook_triggers_gc_on_cadence() {
    let mut r = rig();
    r.cfg.maintenance = doover_core::maintenance::MaintenanceBudget {
        cap_bytes: None,
        min_free_bytes: None,
        gc_every: 1, // every completed action
        keep_days: 365,
    };
    // plant a crash leftover: a 2h-old tmp entry the triggered gc must reap
    let tmp_dir = r.cfg.doover_home.join("store/tmp");
    fs::create_dir_all(&tmp_dir).unwrap();
    fs::write(tmp_dir.join("999-1"), "crash leftover").unwrap();
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(2 * 60 * 60);
    fs::OpenOptions::new()
        .write(true)
        .open(tmp_dir.join("999-1"))
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(old))
        .unwrap();

    fs::write(r.cwd.join("f.txt"), "x").unwrap();
    let pre = hooks::parse_pre_event(&pre_json("s1", "t1", &r.cwd, "rm f.txt")).unwrap();
    hooks::handle_pre(&r.cfg, &pre).unwrap();
    let post = hooks::parse_post_event(&post_json("s1", "t1", &r.cwd, "rm f.txt")).unwrap();
    hooks::handle_post(&r.cfg, &post).unwrap();

    assert!(
        !tmp_dir.join("999-1").exists(),
        "post-hook cadence must have run gc (stale tmp reaped)"
    );
}

/// gc_every = 0 disables the trigger entirely.
#[test]
fn post_hook_gc_trigger_disabled_by_zero_cadence() {
    let mut r = rig();
    r.cfg.maintenance = doover_core::maintenance::MaintenanceBudget {
        cap_bytes: None,
        min_free_bytes: None,
        gc_every: 0,
        keep_days: 365,
    };
    let tmp_dir = r.cfg.doover_home.join("store/tmp");
    fs::create_dir_all(&tmp_dir).unwrap();
    fs::write(tmp_dir.join("999-2"), "crash leftover").unwrap();
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(2 * 60 * 60);
    fs::OpenOptions::new()
        .write(true)
        .open(tmp_dir.join("999-2"))
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(old))
        .unwrap();

    fs::write(r.cwd.join("f.txt"), "x").unwrap();
    let pre = hooks::parse_pre_event(&pre_json("s1", "t1", &r.cwd, "rm f.txt")).unwrap();
    hooks::handle_pre(&r.cfg, &pre).unwrap();
    let post = hooks::parse_post_event(&post_json("s1", "t1", &r.cwd, "rm f.txt")).unwrap();
    hooks::handle_post(&r.cfg, &post).unwrap();

    assert!(
        tmp_dir.join("999-2").exists(),
        "cadence 0 must never trigger gc"
    );
}

/// The trigger is fail-open: a broken store must not fail the post hook.
#[test]
fn post_hook_gc_trigger_failure_is_swallowed() {
    let mut r = rig();
    r.cfg.maintenance = doover_core::maintenance::MaintenanceBudget {
        cap_bytes: Some(1),
        min_free_bytes: None,
        gc_every: 1,
        keep_days: 365,
    };
    fs::write(r.cwd.join("f.txt"), "x").unwrap();
    let pre = hooks::parse_pre_event(&pre_json("s1", "t1", &r.cwd, "rm f.txt")).unwrap();
    hooks::handle_pre(&r.cfg, &pre).unwrap();
    // sabotage AFTER the pre snapshot: an unreadable tmp dir lets Store::open
    // succeed (create_dir_all on an existing dir) and degrades the post-state
    // snapshot to a journaled note, while the triggered gc's clean_tmp
    // read_dir fails — exactly the failure maybe_gc must swallow
    use std::os::unix::fs::PermissionsExt;
    let tmp_dir = r.cfg.doover_home.join("store/tmp");
    fs::set_permissions(&tmp_dir, fs::Permissions::from_mode(0o000)).unwrap();

    let post = hooks::parse_post_event(&post_json("s1", "t1", &r.cwd, "rm f.txt")).unwrap();
    let result = hooks::handle_post(&r.cfg, &post);
    fs::set_permissions(&tmp_dir, fs::Permissions::from_mode(0o755)).unwrap();
    result.expect("gc failure must never fail the post hook");
}

/// D2 review (critical): automatic maintenance must never evict silently, and
/// the free-space floor must never drive automatic eviction at all. With a
/// tiny cap and an old evictable action, the triggered gc DOES evict — and
/// journals a loud note on the triggering action.
#[test]
fn post_hook_gc_eviction_is_journaled_never_silent() {
    let mut r = rig();
    r.cfg.maintenance = doover_core::maintenance::MaintenanceBudget {
        cap_bytes: Some(1),
        min_free_bytes: None,
        gc_every: 1,
        keep_days: 365,
    };
    // seed an OLD evictable action directly in the journal + store
    fs::create_dir_all(&r.cfg.doover_home).unwrap();
    let j = journal(&r.cfg);
    j.begin_session("old-s", "claude-code", "/p").unwrap();
    let old = j
        .start_action(&doover_core::journal::NewAction {
            session_id: "old-s",
            tool_use_id: Some("t-old"),
            raw_command: "rm ancient",
            effect: "destructive",
            rule_id: None,
            has_unknown: false,
        })
        .unwrap();
    j.complete_by_tool_use("old-s", "t-old", 1).unwrap();
    let ancient = doover_core::journal::now_ms() - 30 * 24 * 60 * 60 * 1000;
    j.set_started_at_for_test(old, ancient).unwrap();
    j.set_session_started_at_for_test("old-s", ancient).unwrap();
    let store = doover_core::snapshot::Store::open(r.cfg.doover_home.join("store")).unwrap();
    let f = r.cwd.join("ancient.txt");
    fs::write(&f, "ancient content").unwrap();
    let m = store.snapshot(&f, None).unwrap();
    j.attach_manifest(old, &m, doover_core::journal::ManifestRole::Pre)
        .unwrap();
    // backdate the object past the grace window so it is genuinely evictable
    let obj = store
        .object_paths()
        .unwrap()
        .into_iter()
        .next()
        .expect("one object");
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&obj, fs::Permissions::from_mode(0o644)).unwrap();
    let when = std::time::UNIX_EPOCH + std::time::Duration::from_millis(ancient as u64);
    fs::OpenOptions::new()
        .write(true)
        .open(&obj)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(when))
        .unwrap();

    // a fresh action triggers the cadence-1 gc
    fs::write(r.cwd.join("f.txt"), "x").unwrap();
    let pre = hooks::parse_pre_event(&pre_json("s1", "t1", &r.cwd, "rm f.txt")).unwrap();
    hooks::handle_pre(&r.cfg, &pre).unwrap();
    let post = hooks::parse_post_event(&post_json("s1", "t1", &r.cwd, "rm f.txt")).unwrap();
    let acted_on = hooks::handle_post(&r.cfg, &post).unwrap();

    assert!(!obj.exists(), "cap pressure evicts the ancient object");
    let note = j.action(acted_on).unwrap().note;
    assert!(
        note.as_deref().is_some_and(|n| n.contains("auto-gc")),
        "eviction must be journaled on the triggering action, got {note:?}"
    );
}

/// D2 review: the free-space floor alone must NOT evict in the automatic
/// path (deficit eviction is manual-gc-only). Round-18 mutation testing
/// showed the first version of this test was VACUOUS — its only action was
/// seconds old, so the hot window (not the floor rule) protected it and the
/// exact regression this test guards shipped green. Now the fixture is an
/// ANCIENT, genuinely evictable action: only the floor rule stands between
/// it and deletion.
#[test]
fn post_hook_free_space_floor_never_auto_evicts() {
    let mut r = rig();
    r.cfg.maintenance = doover_core::maintenance::MaintenanceBudget {
        cap_bytes: None,
        min_free_bytes: Some(u64::MAX), // permanently breached
        gc_every: 1,
        keep_days: 365,
    };
    // ancient evictable action whose object is past every age guard
    fs::create_dir_all(&r.cfg.doover_home).unwrap();
    let j = journal(&r.cfg);
    j.begin_session("old-s", "claude-code", "/p").unwrap();
    let old = j
        .start_action(&doover_core::journal::NewAction {
            session_id: "old-s",
            tool_use_id: Some("t-old"),
            raw_command: "rm ancient",
            effect: "destructive",
            rule_id: None,
            has_unknown: false,
        })
        .unwrap();
    j.complete_by_tool_use("old-s", "t-old", 1).unwrap();
    let ancient = doover_core::journal::now_ms() - 30 * 24 * 60 * 60 * 1000;
    j.set_started_at_for_test(old, ancient).unwrap();
    j.set_session_started_at_for_test("old-s", ancient).unwrap();
    let store = doover_core::snapshot::Store::open(r.cfg.doover_home.join("store")).unwrap();
    let f = r.cwd.join("ancient.txt");
    fs::write(&f, "ancient content").unwrap();
    let m = store.snapshot(&f, None).unwrap();
    j.attach_manifest(old, &m, doover_core::journal::ManifestRole::Pre)
        .unwrap();
    let obj = store
        .object_paths()
        .unwrap()
        .into_iter()
        .next()
        .expect("one object");
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&obj, fs::Permissions::from_mode(0o644)).unwrap();
    let when = std::time::UNIX_EPOCH + std::time::Duration::from_millis(ancient as u64);
    fs::OpenOptions::new()
        .write(true)
        .open(&obj)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(when))
        .unwrap();
    fs::set_permissions(&obj, fs::Permissions::from_mode(0o444)).unwrap();

    fs::write(r.cwd.join("f.txt"), "x").unwrap();
    let pre = hooks::parse_pre_event(&pre_json("s1", "t1", &r.cwd, "rm f.txt")).unwrap();
    hooks::handle_pre(&r.cfg, &pre).unwrap();
    let post = hooks::parse_post_event(&post_json("s1", "t1", &r.cwd, "rm f.txt")).unwrap();
    let acted_on = hooks::handle_post(&r.cfg, &post).unwrap();

    assert!(
        obj.exists(),
        "an ANCIENT evictable object must survive floor-only pressure — \
         deficit eviction is a manual-gc decision"
    );
    let note = j.action(acted_on).unwrap().note;
    assert!(
        !note.as_deref().is_some_and(|n| n.contains("evicted 1")),
        "no eviction happened, so no eviction note: {note:?}"
    );
}

/// Round 19 (round-18 lead confirmed): the snapshot time budget must be
/// shared across ALL of one hook invocation's targets — N huge targets must
/// not stack N budgets past the harness timeout (that re-creates the exact
/// SIGKILL blind spot D1 closed). With a 300ms budget consumed by the first
/// (huge) target, the second target must come out truncated near-empty; under
/// per-target budgets it would capture completely.
#[test]
fn snapshot_budget_is_shared_across_targets_not_stacked() {
    let mut r = rig();
    r.cfg.limits = Limits {
        max_files: u64::MAX,
        max_bytes: u64::MAX,
        max_duration: Some(std::time::Duration::from_millis(300)),
    };
    // "aaa_big" sorts before "zzz_small" in the resolver's ordered path set
    let big = r.cwd.join("aaa_big");
    fs::create_dir_all(&big).unwrap();
    for i in 0..20_000 {
        fs::write(big.join(format!("f{i:05}.dat")), "x").unwrap();
    }
    let small = r.cwd.join("zzz_small");
    fs::create_dir_all(&small).unwrap();
    for i in 0..10 {
        fs::write(small.join(format!("s{i}.txt")), "y").unwrap();
    }

    let ev =
        hooks::parse_pre_event(&pre_json("s1", "t1", &r.cwd, "rm -rf aaa_big zzz_small")).unwrap();
    let outcome = hooks::handle_pre(&r.cfg, &ev).unwrap();
    assert!(outcome.needs_warning(), "budget cutoff is a loud gap");

    let j = journal(&r.cfg);
    let manifests = j
        .manifests_by_role(outcome.action_id, doover_core::journal::ManifestRole::Pre)
        .unwrap();
    let small_manifest = manifests
        .iter()
        .find(|m| m.path.ends_with("zzz_small"))
        .expect("second target still journaled (protection gap is loud, not absent)");
    assert!(
        small_manifest.truncated,
        "second target must inherit the SPENT shared budget, not a fresh one"
    );
    assert!(
        small_manifest.entries.len() <= 1,
        "spent budget -> near-empty capture, got {} entries (a fresh per-target \
         budget would have captured all 11)",
        small_manifest.entries.len()
    );
}

/// Round 19 (round-18 lead d): the free-space trigger is rate-limited by the
/// .last-auto-gc marker — a persistently low disk must not re-run a full gc
/// on every action. First breach runs (marker absent); the next action, with
/// the marker fresh, must NOT run. Observable via the stale-tmp sweep.
#[test]
fn free_space_trigger_is_rate_limited_by_the_marker() {
    let mut r = rig();
    r.cfg.maintenance = doover_core::maintenance::MaintenanceBudget {
        cap_bytes: None,
        min_free_bytes: Some(u64::MAX), // permanently breached
        gc_every: 1_000_000,            // cadence never fires for tiny ids
        keep_days: 365,
    };
    let tmp_dir = r.cfg.doover_home.join("store/tmp");
    let plant = |name: &str| {
        fs::create_dir_all(&tmp_dir).unwrap();
        fs::write(tmp_dir.join(name), "leftover").unwrap();
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(2 * 60 * 60);
        fs::OpenOptions::new()
            .write(true)
            .open(tmp_dir.join(name))
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(old))
            .unwrap();
    };
    let cycle = |tool: &str| {
        fs::write(r.cwd.join(format!("{tool}.txt")), "x").unwrap();
        let pre = hooks::parse_pre_event(&pre_json("s1", tool, &r.cwd, "ls")).unwrap();
        hooks::handle_pre(&r.cfg, &pre).unwrap();
        let post = hooks::parse_post_event(&post_json("s1", tool, &r.cwd, "ls")).unwrap();
        hooks::handle_post(&r.cfg, &post).unwrap();
    };

    plant("999-a");
    cycle("t1"); // marker absent -> free-low gc runs, reaps the stale tmp
    assert!(!tmp_dir.join("999-a").exists(), "first breach triggers gc");

    plant("999-b");
    cycle("t2"); // marker fresh -> suppressed
    assert!(
        tmp_dir.join("999-b").exists(),
        "a fresh marker must suppress the free-low re-trigger"
    );
}

/// D4: DOOVER_HOME holds plaintext commands and file snapshots — it must be
/// 0700 regardless of umask, and a pre-existing LOOSE home (created by an
/// older doover or a permissive umask) must be tightened on the next run.
#[test]
fn doover_home_is_private_and_loose_installs_are_tightened() {
    use std::os::unix::fs::PermissionsExt;
    let r = rig();
    // pre-create the home world-readable, like an old install would have
    fs::create_dir_all(&r.cfg.doover_home).unwrap();
    fs::set_permissions(&r.cfg.doover_home, fs::Permissions::from_mode(0o755)).unwrap();

    fs::write(r.cwd.join("f.txt"), "x").unwrap();
    let pre = hooks::parse_pre_event(&pre_json("s1", "t1", &r.cwd, "rm f.txt")).unwrap();
    hooks::handle_pre(&r.cfg, &pre).unwrap();

    let mode = fs::metadata(&r.cfg.doover_home)
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o700, "home must be tightened to 0700, got {mode:o}");
}
