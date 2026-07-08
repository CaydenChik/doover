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

#[test]
fn unimplemented_subcommands_fail_honestly_with_exit_64() {
    for sub in ["show", "diff"] {
        Command::cargo_bin("doover")
            .unwrap()
            .arg(sub)
            .assert()
            .code(64)
            .stderr(predicate::str::contains("not implemented"));
    }
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
