//! T6 (prelude) — golden Claude Code hook payloads captured from a live
//! session (v2.1.201, 2026-07-06) pin the harness contract in CI. If Claude
//! Code changes its hook schema, this breaks loudly instead of doover
//! misparsing events in the field. The step-5 adapter builds on these.

use serde_json::Value;
use std::path::{Path, PathBuf};

fn fixtures() -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/hook-events");
    let mut out: Vec<PathBuf> = std::fs::read_dir(dir)
        .expect("fixtures dir must exist")
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .collect();
    out.sort();
    out
}

fn load(p: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(p).unwrap())
        .unwrap_or_else(|e| panic!("{}: invalid JSON: {e}", p.display()))
}

#[test]
fn all_fixtures_carry_the_contract_fields() {
    let files = fixtures();
    assert!(files.len() >= 8, "fixture set has shrunk: {}", files.len());
    for path in &files {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let v = load(path);
        let event = v["hook_event_name"].as_str().unwrap();
        let is_pre = name.starts_with("pre_");
        assert_eq!(
            event,
            if is_pre { "PreToolUse" } else { "PostToolUse" },
            "{name}: filename/event mismatch"
        );
        assert_eq!(v["tool_name"].as_str().unwrap(), "Bash", "{name}");
        assert!(
            Path::new(v["cwd"].as_str().unwrap()).is_absolute(),
            "{name}: cwd must be absolute"
        );
        assert!(!v["session_id"].as_str().unwrap().is_empty(), "{name}");
        assert!(!v["tool_use_id"].as_str().unwrap().is_empty(), "{name}");
        let cmd = v["tool_input"]["command"].as_str().unwrap();
        assert!(!cmd.is_empty(), "{name}: empty command");

        if !is_pre {
            // contract: response carries stdout/stderr/interrupted and
            // duration_ms — and NO exit code (verified live: the field does
            // not exist; failures simply never produce a PostToolUse)
            let resp = &v["tool_response"];
            assert!(resp["stdout"].is_string(), "{name}");
            assert!(resp["stderr"].is_string(), "{name}");
            assert!(resp["interrupted"].is_boolean(), "{name}");
            assert!(v["duration_ms"].is_u64(), "{name}");
            for key in [
                "exit_code",
                "exitCode",
                "exit_status",
                "exitStatus",
                "return_code",
                "returncode",
                "code",
                "status",
            ] {
                assert!(
                    resp.get(key).is_none(),
                    "{name}: `{key}` appeared in tool_response — the harness contract \
                     changed (exit codes now exist?); revisit the journal's missing-post design"
                );
            }
        }
    }
}

#[test]
fn cwd_tracks_the_sessions_live_directory() {
    // load-bearing for the whole design: after `cd cache-dir && rm a.tmp`,
    // the harness hands the NEXT call a cwd of .../cache-dir — so cross-call
    // cwd tracking is the harness's job, not the resolver's
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/hook-events");
    let post_cd = load(&dir.join("post_bash_cd_updates_cwd.json"));
    let next_pre = load(&dir.join("pre_bash_cwd_tracks_prior_cd.json"));
    assert!(post_cd["cwd"].as_str().unwrap().ends_with("/cache-dir"));
    assert!(next_pre["cwd"].as_str().unwrap().ends_with("/cache-dir"));
    assert_eq!(next_pre["tool_input"]["command"].as_str().unwrap(), "pwd");
}

#[test]
fn heredoc_commands_arrive_as_one_string_with_newlines() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/hook-events");
    let v = load(&dir.join("pre_bash_heredoc_multiline.json"));
    let cmd = v["tool_input"]["command"].as_str().unwrap();
    assert!(
        cmd.contains("<<EOF\n"),
        "heredoc newlines must survive: {cmd:?}"
    );
    assert!(cmd.ends_with("EOF"), "{cmd:?}");
}
