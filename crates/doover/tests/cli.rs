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
    for sub in ["undo", "log", "doctor"] {
        Command::cargo_bin("doover")
            .unwrap()
            .arg(sub)
            .assert()
            .code(64)
            .stderr(predicate::str::contains("not implemented"));
    }
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

    let j = Journal::open(&dh.join("journal.db")).unwrap();
    let actions = j.session_actions("cli-s1").unwrap();
    assert_eq!(actions.len(), 1);
    assert_eq!(actions[0].status, ActionStatus::Completed);
    assert_eq!(actions[0].duration_ms, Some(7));
    assert_eq!(j.manifests(actions[0].id).unwrap().len(), 1);
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
