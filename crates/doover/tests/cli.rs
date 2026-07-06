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
