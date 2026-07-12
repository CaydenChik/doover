use assert_cmd::Command;
use predicates::prelude::*;

#[test]
fn version_prints_and_exits_zero() {
    Command::cargo_bin("doover")
        .unwrap()
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::contains(env!("CARGO_PKG_VERSION")));
}

#[test]
fn help_lists_all_planned_subcommands() {
    let assert = Command::cargo_bin("doover")
        .unwrap()
        .arg("--help")
        .assert()
        .success();
    let out = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    for sub in [
        "init", "log", "show", "undo", "redo", "diff", "status", "gc", "doctor", "hook",
    ] {
        assert!(out.contains(sub), "--help must list `{sub}`");
    }
}

// --- step 8: show / diff (last stubs gone) -----------------------------------

/// Drive a pre+post hook pair through the real binary; returns (home, cwd).
fn journal_one_action(cmd: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let dh = tmp.path().join("dh");
    let cwd = tmp.path().join("proj");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::write(cwd.join("keep.txt"), "precious").unwrap();
    for (event, extra) in [
        ("PreToolUse", serde_json::json!({})),
        (
            "PostToolUse",
            serde_json::json!({
                "duration_ms": 5,
                "tool_response": { "stdout": "", "stderr": "", "interrupted": false }
            }),
        ),
    ] {
        let mut ev = serde_json::json!({
            "session_id": "s-show", "cwd": cwd.to_string_lossy(),
            "hook_event_name": event, "tool_name": "Bash",
            "tool_use_id": "t-show", "tool_input": { "command": cmd }
        });
        ev.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let sub = if event == "PreToolUse" { "pre" } else { "post" };
        hook_cmd(&dh, sub)
            .write_stdin(ev.to_string())
            .assert()
            .success();
    }
    let dh_owned = dh.clone();
    (tmp, dh_owned)
}

#[test]
fn show_prints_action_detail_with_manifest_summary() {
    let (_tmp, dh) = journal_one_action("rm keep.txt");
    Command::cargo_bin("doover")
        .unwrap()
        .args(["show", "1"])
        .env("DOOVER_HOME", &dh)
        .assert()
        .success()
        .stdout(
            predicate::str::contains("rm keep.txt")
                .and(predicate::str::contains("completed"))
                .and(predicate::str::contains("pre"))
                .and(predicate::str::contains("keep.txt")),
        );
}

#[test]
fn show_and_log_redact_secrets_at_display_time_but_journal_keeps_raw() {
    let secret = "sk-live-Sup3rSecret";
    let (_tmp, dh) = journal_one_action(&format!(
        "curl -H \"Authorization: Bearer {secret}\" -o out https://x"
    ));
    for args in [vec!["show", "1"], vec!["log"]] {
        let assert = Command::cargo_bin("doover")
            .unwrap()
            .args(&args)
            .env("DOOVER_HOME", &dh)
            .assert()
            .success();
        let out = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
        assert!(!out.contains(secret), "{args:?} leaked the secret: {out}");
        assert!(out.contains("[redacted]"), "{args:?} shows the mask");
    }
    // the journal itself keeps the raw command (undo semantics unchanged;
    // redaction is a display concern)
    let j = doover_core::journal::Journal::open(&dh.join("journal.db")).unwrap();
    assert!(j.action(1).unwrap().raw_command.contains(secret));
}

#[test]
fn show_unknown_id_is_a_clear_error() {
    let tmp = tempfile::tempdir().unwrap();
    Command::cargo_bin("doover")
        .unwrap()
        .args(["show", "999"])
        .env("DOOVER_HOME", tmp.path().join("dh"))
        .assert()
        .code(1)
        .stderr(predicate::str::contains("not found"));
}

#[test]
fn diff_flags_a_truncated_snapshot_as_partial() {
    // audit round 13: if the pre-snapshot was truncated at limits, the diff
    // covers only part of the tree — and so does what undo could restore. The
    // user must be told, not shown a clean-looking partial comparison.
    let tmp = tempfile::tempdir().unwrap();
    let dh = tmp.path().join("dh");
    let cwd = tmp.path().join("proj");
    std::fs::create_dir_all(cwd.join("d")).unwrap();
    for i in 0..6 {
        std::fs::write(cwd.join("d").join(format!("f{i}.txt")), "x").unwrap();
    }
    let ev = |event: &str| {
        serde_json::json!({
            "session_id": "s", "cwd": cwd.to_string_lossy(),
            "hook_event_name": event, "tool_name": "Bash", "tool_use_id": "t",
            "tool_input": { "command": "rm -rf d" }
        })
        .to_string()
    };
    hook_cmd(&dh, "pre")
        .env("DOOVER_MAX_FILES", "2") // force truncation
        .write_stdin(ev("PreToolUse"))
        .assert()
        .success();

    Command::cargo_bin("doover")
        .unwrap()
        .args(["diff", "1"])
        .env("DOOVER_HOME", &dh)
        .assert()
        .success()
        .stdout(predicate::str::contains("PARTIAL"));
}

#[test]
fn diff_reports_per_path_status_against_the_current_world() {
    let (tmp, dh) = journal_one_action("rm keep.txt");
    // mutate after the action: the pre-state should now read as modified
    std::fs::write(tmp.path().join("proj/keep.txt"), "changed since").unwrap();
    Command::cargo_bin("doover")
        .unwrap()
        .args(["diff", "1"])
        .env("DOOVER_HOME", &dh)
        .assert()
        .success()
        .stdout(predicate::str::contains("keep.txt").and(predicate::str::contains("modified")));
}

// --- step 7: init / gc / status / doctor -----------------------------------------

#[test]
fn init_project_creates_hooks_and_is_idempotent() {
    let tmp = tempfile::tempdir().unwrap();
    // --project writes into ./.claude/settings.json (cwd of the child)
    Command::cargo_bin("doover")
        .unwrap()
        .args(["init", "--project"])
        .current_dir(tmp.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("installed"));

    let settings = std::fs::read_to_string(tmp.path().join(".claude/settings.json")).unwrap();
    assert!(settings.contains("doover hook pre"));
    assert!(settings.contains("doover hook post"));
    assert!(settings.contains("PreToolUse") && settings.contains("PostToolUse"));

    // second run: no duplication
    Command::cargo_bin("doover")
        .unwrap()
        .args(["init", "--project"])
        .current_dir(tmp.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("already installed"));
    let again = std::fs::read_to_string(tmp.path().join(".claude/settings.json")).unwrap();
    assert_eq!(
        again.matches("doover hook pre").count(),
        1,
        "no duplicate hook"
    );
}

#[test]
fn init_merges_with_existing_settings() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join(".claude")).unwrap();
    std::fs::write(
        tmp.path().join(".claude/settings.json"),
        r#"{"model":"opus","hooks":{"PreToolUse":[{"matcher":"Edit","hooks":[{"type":"command","command":"my-linter"}]}]}}"#,
    )
    .unwrap();

    Command::cargo_bin("doover")
        .unwrap()
        .args(["init", "--project"])
        .current_dir(tmp.path())
        .assert()
        .success();

    let s = std::fs::read_to_string(tmp.path().join(".claude/settings.json")).unwrap();
    assert!(s.contains("\"model\""), "existing keys preserved");
    assert!(s.contains("my-linter"), "existing hooks preserved");
    assert!(s.contains("doover hook pre"), "our hook added alongside");
}

#[test]
fn init_errors_on_unmergeable_hooks_shape_instead_of_lying() {
    // audit round 12: valid JSON whose SHAPE we cannot merge into used to
    // print "already installed" (a lie — nothing was installed) or silently
    // install only one of the two hooks. Must be a loud error, file untouched.
    for bad in [
        r#"{"hooks": []}"#,
        r#"{"hooks": "oops"}"#,
        r#"{"hooks": {"PreToolUse": {}}}"#,
        r#"{"hooks": {"PreToolUse": {}, "PostToolUse": []}}"#,
    ] {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".claude")).unwrap();
        std::fs::write(tmp.path().join(".claude/settings.json"), bad).unwrap();
        Command::cargo_bin("doover")
            .unwrap()
            .args(["init", "--project"])
            .current_dir(tmp.path())
            .assert()
            .code(1)
            .stderr(predicate::str::contains("cannot merge"));
        let after = std::fs::read_to_string(tmp.path().join(".claude/settings.json")).unwrap();
        assert_eq!(after, bad, "unmergeable settings must not be modified");
    }
}

#[test]
fn init_recognizes_hand_edited_absolute_path_hooks() {
    // a user who pinned the hook to an absolute binary path must not get a
    // duplicate entry on re-init
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join(".claude")).unwrap();
    std::fs::write(
        tmp.path().join(".claude/settings.json"),
        r#"{"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"/usr/local/bin/doover hook pre"}]}]}}"#,
    )
    .unwrap();
    Command::cargo_bin("doover")
        .unwrap()
        .args(["init", "--project"])
        .current_dir(tmp.path())
        .assert()
        .success();
    let s = std::fs::read_to_string(tmp.path().join(".claude/settings.json")).unwrap();
    assert_eq!(s.matches("hook pre").count(), 1, "no duplicate pre hook");
    assert!(s.contains("doover hook post"), "post hook still added");
}

#[test]
fn init_write_failure_is_a_clean_error_with_no_droppings() {
    // atomic write: a failed install must not leave temp files or a torn
    // settings.json behind
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join(".claude");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
    Command::cargo_bin("doover")
        .unwrap()
        .args(["init", "--project"])
        .current_dir(tmp.path())
        .assert()
        .code(1)
        .stderr(predicate::str::contains("cannot write"));
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(
        std::fs::read_dir(&dir).unwrap().next().is_none(),
        "no temp droppings after a failed write"
    );
}

#[test]
fn doctor_finds_project_level_hooks() {
    // audit round 12: doctor only looked at ~/.claude — after `init
    // --project` it told the user to run init again
    let tmp = tempfile::tempdir().unwrap();
    Command::cargo_bin("doover")
        .unwrap()
        .args(["init", "--project"])
        .current_dir(tmp.path())
        .assert()
        .success();
    Command::cargo_bin("doover")
        .unwrap()
        .arg("doctor")
        .current_dir(tmp.path())
        .env("DOOVER_HOME", tmp.path().join("dh"))
        .env_remove("HOME")
        .assert()
        .success()
        .stdout(predicate::str::contains("hooks installed"));
}

#[test]
fn init_refuses_to_clobber_malformed_settings() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join(".claude")).unwrap();
    std::fs::write(tmp.path().join(".claude/settings.json"), "{ not valid json").unwrap();
    Command::cargo_bin("doover")
        .unwrap()
        .args(["init", "--project"])
        .current_dir(tmp.path())
        .assert()
        .code(1)
        .stderr(predicate::str::contains("not valid JSON"));
}

#[test]
fn status_and_gc_and_doctor_run_on_empty_home() {
    let tmp = tempfile::tempdir().unwrap();
    let dh = tmp.path().join("dh");
    for (args, needle) in [
        (vec!["status"], "store objects"),
        (vec!["gc", "--dry-run"], "object(s)"),
    ] {
        Command::cargo_bin("doover")
            .unwrap()
            .args(&args)
            .env("DOOVER_HOME", &dh)
            .assert()
            .success()
            .stdout(predicate::str::contains(needle));
    }
    // doctor on a fresh (no hooks) home reports the missing-hooks warning but
    // still exits 0 (writable home, healthy empty journal)
    Command::cargo_bin("doover")
        .unwrap()
        .arg("doctor")
        .env("DOOVER_HOME", &dh)
        .env_remove("HOME")
        .assert()
        .success()
        .stdout(predicate::str::contains("[ok]"));
}

#[test]
fn undo_with_no_history_is_a_clear_error() {
    let tmp = tempfile::tempdir().unwrap();
    Command::cargo_bin("doover")
        .unwrap()
        .arg("undo")
        .env("DOOVER_HOME", tmp.path().join("dh"))
        .assert()
        .code(1)
        .stderr(predicate::str::contains("nothing to undo"));
}

#[test]
fn log_with_no_history_prints_a_friendly_message() {
    let tmp = tempfile::tempdir().unwrap();
    Command::cargo_bin("doover")
        .unwrap()
        .arg("log")
        .env("DOOVER_HOME", tmp.path().join("dh"))
        .assert()
        .success()
        .stdout(predicate::str::contains("no journaled actions yet"));
}

#[test]
fn unknown_subcommand_is_a_usage_error() {
    Command::cargo_bin("doover")
        .unwrap()
        .arg("frobnicate")
        .assert()
        .failure()
        .code(predicate::ne(64));
}

// --- hook binary: fail-open is the prime directive (step 5 / S8) ------------------

fn hook_cmd(home: &std::path::Path, sub: &str) -> Command {
    let mut c = Command::cargo_bin("doover").unwrap();
    c.args(["hook", sub])
        .env("DOOVER_HOME", home)
        .env_remove("DOOVER_TEST_PANIC");
    c
}

#[test]
fn hook_pre_garbage_stdin_fails_open_with_exit_zero() {
    let tmp = tempfile::tempdir().unwrap();
    hook_cmd(&tmp.path().join("dh"), "pre")
        .write_stdin("this is not json {{{")
        .assert()
        .success()
        .stderr(predicate::str::contains("fail-open"));
}

#[test]
fn hook_panic_fails_open_with_exit_zero() {
    let tmp = tempfile::tempdir().unwrap();
    hook_cmd(&tmp.path().join("dh"), "pre")
        .env("DOOVER_TEST_PANIC", "1")
        .write_stdin("{}")
        .assert()
        .success()
        .stderr(predicate::str::contains("panicked (fail-open"));
}

#[test]
fn hook_pre_then_post_journals_and_completes_through_the_real_binary() {
    use doover_core::journal::{ActionStatus, Journal};
    let tmp = tempfile::tempdir().unwrap();
    let dh = tmp.path().join("dh");
    let cwd = tmp.path().join("proj");
    std::fs::create_dir_all(cwd.join("build")).unwrap();
    std::fs::write(cwd.join("build/a.txt"), "A").unwrap();

    let pre = serde_json::json!({
        "session_id": "cli-s1", "cwd": cwd.to_string_lossy(),
        "hook_event_name": "PreToolUse", "tool_name": "Bash",
        "tool_use_id": "cli-t1",
        "tool_input": { "command": "rm -rf build" }
    });
    hook_cmd(&dh, "pre")
        .write_stdin(pre.to_string())
        .assert()
        .success();

    let post = serde_json::json!({
        "session_id": "cli-s1", "cwd": cwd.to_string_lossy(),
        "hook_event_name": "PostToolUse", "tool_name": "Bash",
        "tool_use_id": "cli-t1", "duration_ms": 7,
        "tool_input": { "command": "rm -rf build" },
        "tool_response": { "stdout": "", "stderr": "", "interrupted": false }
    });
    hook_cmd(&dh, "post")
        .write_stdin(post.to_string())
        .assert()
        .success();

    use doover_core::journal::ManifestRole;
    let j = Journal::open(&dh.join("journal.db")).unwrap();
    let actions = j.session_actions("cli-s1").unwrap();
    assert_eq!(actions.len(), 1);
    assert_eq!(actions[0].status, ActionStatus::Completed);
    assert_eq!(actions[0].duration_ms, Some(7));
    // pre (before rm) + post (after rm) captured by the real engine
    assert_eq!(
        j.manifests_by_role(actions[0].id, ManifestRole::Pre)
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        j.manifests_by_role(actions[0].id, ManifestRole::Post)
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn hook_pre_warns_loudly_when_a_destructive_action_is_unprotected() {
    // audit round 9: exit 0 (never block) but WARN — a destructive command
    // whose snapshot failed must not pass silently
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let dh = tmp.path().join("dh");
    let cwd = tmp.path().join("proj");
    std::fs::create_dir_all(cwd.join("build")).unwrap();
    std::fs::write(cwd.join("build/a.txt"), "precious").unwrap();

    let ev = |cmd: &str| {
        serde_json::json!({
            "session_id": "s1", "cwd": cwd.to_string_lossy(),
            "hook_event_name": "PreToolUse", "tool_name": "Bash",
            "tool_use_id": "t1", "tool_input": { "command": cmd }
        })
        .to_string()
    };

    // a destructive priming action creates the store, then make it unwritable
    std::fs::write(cwd.join("prime.txt"), "x").unwrap();
    hook_cmd(&dh, "pre")
        .write_stdin(ev("rm prime.txt"))
        .assert()
        .success();
    let objects = dh.join("store/objects");
    std::fs::set_permissions(&objects, std::fs::Permissions::from_mode(0o555)).unwrap();

    let assert = hook_cmd(&dh, "pre")
        .write_stdin(ev("rm -rf build"))
        .assert()
        .success(); // still fail-open: never block the agent
    std::fs::set_permissions(&objects, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert.stderr(
        predicate::str::contains("PROTECTION INCOMPLETE")
            .or(predicate::str::contains("UNPROTECTED")),
    );
}

#[test]
fn hook_post_without_pre_fails_open() {
    let tmp = tempfile::tempdir().unwrap();
    let post = serde_json::json!({
        "session_id": "ghost", "cwd": "/tmp",
        "hook_event_name": "PostToolUse", "tool_name": "Bash",
        "tool_use_id": "never-seen", "duration_ms": 1,
        "tool_input": { "command": "ls" },
        "tool_response": { "stdout": "", "stderr": "", "interrupted": false }
    });
    hook_cmd(&tmp.path().join("dh"), "post")
        .write_stdin(post.to_string())
        .assert()
        .success()
        .stderr(predicate::str::contains("fail-open"));
}

#[test]
fn gc_keep_days_zero_flag_matches_the_env_opt_out_convention() {
    // round 18: DOOVER_KEEP_DAYS=0 is documented as "keep forever" — the CLI
    // flag must not mean the opposite (prune everything older than the newest
    // action). Consistency here is data-loss-critical.
    let (_tmp, dh) = journal_one_action("rm keep.txt");
    Command::cargo_bin("doover")
        .unwrap()
        .args(["gc", "--keep-days", "0"])
        .env("DOOVER_HOME", &dh)
        .assert()
        .success()
        .stdout(predicate::str::contains("retention disabled"));
    // the journaled action must still exist
    Command::cargo_bin("doover")
        .unwrap()
        .arg("log")
        .env("DOOVER_HOME", &dh)
        .assert()
        .success()
        .stdout(predicate::str::contains("rm keep.txt"));
}

#[test]
fn gc_honors_the_keep_days_env_and_rejects_negative_footguns() {
    // round 18: the clap default silently overrode DOOVER_KEEP_DAYS; and
    // negative --keep-days values meant "prune all but the newest".
    let (_tmp, dh) = journal_one_action("rm keep.txt");
    // env opt-out honored when no flag is given
    Command::cargo_bin("doover")
        .unwrap()
        .arg("gc")
        .env("DOOVER_HOME", &dh)
        .env("DOOVER_KEEP_DAYS", "0")
        .assert()
        .success()
        .stdout(predicate::str::contains("retention disabled"));
    // negative flag = same keep-forever treatment, never a purge
    Command::cargo_bin("doover")
        .unwrap()
        .args(["gc", "--keep-days=-3"])
        .env("DOOVER_HOME", &dh)
        .assert()
        .success()
        .stdout(predicate::str::contains("retention disabled"));
    Command::cargo_bin("doover")
        .unwrap()
        .arg("log")
        .env("DOOVER_HOME", &dh)
        .assert()
        .success()
        .stdout(predicate::str::contains("rm keep.txt"));
}
