//! User-#1 trial regressions (2026-07-14). The first real end-to-end run by a
//! live agent on a real machine found four things twenty-one adversarial audit
//! rounds did not. Every one of them lives on here.
//!
//! The headline, and the worst bug in the project's history: doover told a user
//! their files "cannot be undone" while the snapshot sat intact in the store.
//! The status check short-circuited before the conflict oracle that would have
//! handled the case correctly. A tool whose entire promise is recovery told the
//! user recovery had failed when it had not.

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
    rig_with_max_files(100_000)
}

fn rig_with_max_files(max_files: u64) -> Rig {
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
                max_files,
                max_bytes: 5 << 30,
                max_duration: None,
            },
            unknown_policy: UnknownPolicy::SnapshotCwd,
            maintenance: doover_core::maintenance::MaintenanceBudget::disabled(),
        },
        cwd,
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

impl Rig {
    /// Drive a command through the real engine: pre-hook, actually run it, post-hook.
    fn run(&self, session: &str, tool: &str, cmd: &str) -> i64 {
        let ev = hooks::parse_pre_event(&mkjson(session, tool, &self.cwd, cmd, false)).unwrap();
        let out = hooks::handle_pre(&self.cfg, &ev).unwrap();
        std::process::Command::new("bash")
            .args(["--noprofile", "--norc", "-c", cmd])
            .current_dir(&self.cwd)
            .status()
            .unwrap();
        let post = mkjson(session, tool, &self.cwd, cmd, true);
        hooks::handle_post(&self.cfg, &hooks::parse_post_event(&post).unwrap()).unwrap();
        out.action_id
    }

    /// Pre-hook + run, but the command is killed before the post-hook: the shape
    /// of a command the harness timed out on. Leaves the action `abandoned`.
    fn run_killed(&self, session: &str, tool: &str, cmd: &str) -> i64 {
        let ev = hooks::parse_pre_event(&mkjson(session, tool, &self.cwd, cmd, false)).unwrap();
        let out = hooks::handle_pre(&self.cfg, &ev).unwrap();
        std::process::Command::new("bash")
            .args(["--noprofile", "--norc", "-c", cmd])
            .current_dir(&self.cwd)
            .status()
            .unwrap();
        self.journal().end_session(session).unwrap();
        out.action_id
    }

    fn script(&self, name: &str, body: &str) {
        let p = self.cwd.join(name);
        fs::write(&p, body).unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
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
    fn mode(&self, rel: &str) -> u32 {
        fs::symlink_metadata(self.cwd.join(rel))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777
    }
}

// --- REVIEW FINDING 8 (HIGH): bare `doover undo` must not IO-abort on one bad file
//
// `pick_latest_undoable` calls the filesystem to decide whether each candidate
// still has something to restore. A single unreadable file inside a candidate's
// snapshot made `state_matches` error, and that error propagated straight out —
// bare `doover undo` died with `io error at .../secret.key: Permission denied`
// (exit 1), a raw failure naming an unrelated file, while a fully recoverable
// `rm` sat one candidate deeper. The scan must degrade, never propagate a raw
// snapshot/IO error to the user.
#[test]
fn bare_undo_does_not_io_abort_when_a_candidate_has_an_unreadable_file() {
    let r = rig();
    fs::create_dir_all(r.cwd.join("photos")).unwrap();
    fs::write(r.cwd.join("photos/wedding.jpg"), "irreplaceable").unwrap();
    fs::write(r.cwd.join("secret.key"), "sensitive").unwrap();

    // #1: the recoverable rm.
    r.run("s1", "t1", "rm -rf photos");
    // #2: a killed opaque command (no POST) leaves a defensive whole-cwd
    // snapshot that captured secret.key. A later change makes that file
    // unreadable, so evaluating #2's snapshot touches a file that errors.
    r.script("mystery.sh", "#!/bin/sh\nexit 0\n");
    r.run_killed("s1", "t2", "./mystery.sh");
    fs::set_permissions(r.cwd.join("secret.key"), fs::Permissions::from_mode(0o000)).unwrap();

    let j = r.journal();
    let s = r.store();
    let e = UndoEngine::new(&j, &s);

    // The guarantee: NOT a raw snapshot/IO error. A clean conflict (exit 3) is
    // acceptable; a cryptic `io error ... Permission denied` (exit 1) is not.
    let res = e.undo(Selector::Latest, false, false);
    assert!(
        !matches!(res, Err(UndoError::Snapshot(_))),
        "bare undo leaked a raw snapshot/IO error instead of degrading: {res:?}"
    );

    // And the recoverable action is still reachable by explicit id.
    let _ = fs::set_permissions(r.cwd.join("secret.key"), fs::Permissions::from_mode(0o600));
    e.undo(Selector::Action(1), false, false).unwrap();
    assert_eq!(
        r.read("photos/wedding.jpg").as_deref(),
        Some("irreplaceable"),
        "`doover undo 1` restores the photos"
    );
}

/// ROUND 3 (test-honesty): the test above no longer exercises the
/// `Restorability::Indeterminate => continue` branch, because the finding-9
/// mode-aware change makes a `chmod 000` file resolve via the MODE mismatch
/// (Ok(false) → InForce) before it ever reaches the read-error that produces
/// Indeterminate. This test constructs a GENUINE Indeterminate candidate — a
/// file that is unreadable while its mode is UNCHANGED (a deny-read ACL) — and
/// verifies bare `doover undo` skips PAST it to the recoverable action deeper,
/// rather than aborting the scan. macOS-only (ACL syntax; macOS is the ship
/// target and CI runner).
#[cfg(target_os = "macos")]
#[test]
fn bare_undo_continues_past_an_indeterminate_candidate_to_the_recoverable_one() {
    let r = rig();
    fs::create_dir_all(r.cwd.join("photos")).unwrap();
    fs::write(r.cwd.join("photos/w.jpg"), "irreplaceable").unwrap();
    fs::write(r.cwd.join("secret.key"), "sensitive").unwrap();
    fs::set_permissions(r.cwd.join("secret.key"), fs::Permissions::from_mode(0o644)).unwrap();

    // #1: the recoverable rm (small, readable snapshot).
    r.run("s1", "t1", "rm -rf photos");
    // #2: a killed opaque command whose defensive cwd snapshot captured
    // secret.key while it was readable (mode 644).
    r.script("op.sh", "#!/bin/sh\nexit 0\n");
    r.run_killed("s1", "t2", "./op.sh");

    // Now make secret.key unreadable WITHOUT changing its mode bits, so
    // evaluating #2's snapshot errors (Err → Indeterminate) rather than
    // mismatching (Ok(false) → InForce).
    let who = String::from_utf8(
        std::process::Command::new("whoami")
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_string();
    let acl = std::process::Command::new("chmod")
        .args(["+a", &format!("{who} deny read")])
        .arg(r.cwd.join("secret.key"))
        .status()
        .unwrap();
    assert!(acl.success(), "could not set the deny-read ACL");
    assert!(
        fs::read(r.cwd.join("secret.key")).is_err(),
        "precondition: secret.key is now unreadable"
    );

    let j = r.journal();
    let s = r.store();
    let e = UndoEngine::new(&j, &s);
    let rep = e
        .undo(Selector::Latest, false, false)
        .expect("bare undo must skip the Indeterminate #2 and reach the recoverable #1");
    // clear the ACL so the tempdir can be cleaned up
    let _ = std::process::Command::new("chmod")
        .arg("-N")
        .arg(r.cwd.join("secret.key"))
        .status();

    assert_eq!(rep.target_action, 1, "reached the recoverable rm, not #2");
    assert_eq!(
        r.read("photos/w.jpg").as_deref(),
        Some("irreplaceable"),
        "restored the photos from the action past the un-evaluable candidate"
    );
}

// --- ROUND 3 (HIGH): bare `doover undo` must find a TRUNCATED destructive snapshot
//
// A regression I introduced with the finding-8 fix: `restorability` short-
// circuited a truncated capture to Indeterminate, and bare undo SKIPS
// Indeterminate candidates. So a large `rm -rf` whose pre-snapshot hit the file/
// time limit — the exact big-deletion the tool exists to recover — made bare
// `doover undo` report "nothing to undo". It must be FOUND (offered); undo then
// applies the round-18 refuse-by-default on the truncated restore-set.
#[test]
fn bare_undo_finds_a_truncated_destructive_snapshot() {
    let r = rig_with_max_files(3); // force truncation of a 20-file deletion
    fs::create_dir_all(r.cwd.join("data")).unwrap();
    for i in 0..20 {
        fs::write(r.cwd.join(format!("data/f{i}.txt")), "precious").unwrap();
    }
    let id = r.run("s1", "t1", "rm -rf data");

    let j = r.journal();
    let s = r.store();
    assert!(
        j.manifests_by_role(id, doover_core::journal::ManifestRole::Pre)
            .unwrap()
            .iter()
            .any(|m| m.truncated),
        "precondition: the snapshot was truncated"
    );

    let e = UndoEngine::new(&j, &s);

    // Non-force bare undo must FIND the truncated action and refuse with a
    // CONFLICT (round-18: never swap a partial capture over a full tree) — NOT
    // report NoUndoableAction, which is the skip regression this pins. The
    // distinction matters: a `Conflicts` proves the action was selected; a
    // `NoUndoableAction` would mean it was skipped.
    match e.undo(Selector::Latest, false, false) {
        Err(UndoError::Conflicts(_)) => {} // found, correctly refused
        Err(UndoError::NoUndoableAction) => {
            panic!("REGRESSION: bare undo SKIPPED the truncated rm (reported nothing to undo)")
        }
        other => panic!("expected a truncated-capture conflict, got {other:?}"),
    }

    // And --force finds the same action and does the partial restore.
    let rep = e
        .undo(Selector::Latest, true, false)
        .expect("--force undo of the truncated action must proceed");
    assert_eq!(rep.target_action, id, "targeted the truncated deletion");
    assert!(
        rep.paths_restored >= 1,
        "restored at least the captured part"
    );
}

// --- REVIEW FINDING 9 (HIGH): bare `doover undo` must find a metadata-only change
//
// `chmod -R 777 .` on a tree containing a mode-600 key is destructive and
// snapshotted (pre-modes captured). But `pick_latest_undoable`'s "nothing to
// restore" check used content-only `state_matches`, which treats a live 777
// tree as matching the 600 snapshot — so bare `doover undo` skipped it and
// reported "nothing to undo", while `doover undo 1` restored the modes. A
// metadata-only effect is exactly the kind of destructive action doover exists
// to catch.
#[test]
fn bare_undo_finds_a_metadata_only_destructive_action() {
    let r = rig();
    fs::write(r.cwd.join("id_rsa"), "PRIVATE KEY").unwrap();
    fs::set_permissions(r.cwd.join("id_rsa"), fs::Permissions::from_mode(0o600)).unwrap();

    r.run("s1", "t1", "chmod -R 777 .");
    assert_eq!(r.mode("id_rsa"), 0o777, "precondition: chmod took effect");

    let j = r.journal();
    let s = r.store();
    let e = UndoEngine::new(&j, &s);

    let rep = e
        .undo(Selector::Latest, false, false)
        .expect("bare undo must find the chmod; its pre-modes are snapshotted");
    assert_eq!(
        r.mode("id_rsa"),
        0o600,
        "the secure mode was restored (target #{})",
        rep.target_action
    );
}

// --- FINDING 1: the state-machine trap (data declared unrecoverable) ---------

/// The exact sequence the trial hit.
///
/// Undo an `rm`, then `--force`-undo a LATER whole-directory action. That
/// later action's pre-state was captured *after* the rm, so restoring it
/// re-deletes the very files the first undo brought back — while the rm's row
/// still reads `Undone`.
///
/// doover then refused BOTH directions: `undo` said "status is Undone" and
/// `redo` said "not an undo action". The pre-snapshot was in the store the
/// whole time. The status check ran before the conflict oracle that already
/// knew how to handle exactly this.
#[test]
fn force_undoing_a_later_action_leaves_the_earlier_one_undoable_again() {
    let r = rig();
    fs::create_dir_all(r.cwd.join("photos")).unwrap();
    fs::write(r.cwd.join("photos/wedding.jpg"), "irreplaceable").unwrap();
    r.script("mystery.sh", "#!/bin/sh\nexit 0\n");

    let rm_id = r.run("s1", "t1", "rm -rf photos");
    assert_eq!(r.read("photos/wedding.jpg"), None, "the rm really ran");

    // An opaque command: doover cannot tell what it does, so it defensively
    // snapshots the whole working directory — which no longer contains photos/.
    let opaque_id = r.run("s1", "t2", "./mystery.sh");

    let j = r.journal();
    let s = r.store();
    let e = UndoEngine::new(&j, &s);

    e.undo(Selector::Action(rm_id), false, false).unwrap();
    assert_eq!(
        r.read("photos/wedding.jpg").as_deref(),
        Some("irreplaceable"),
        "undo brought the photos back"
    );

    // Force-undo the opaque action. Its pre-state has no photos/, so this
    // re-applies the rm's effect behind doover's back.
    e.undo(Selector::Action(opaque_id), true, false).unwrap();
    assert_eq!(
        r.read("photos/wedding.jpg"),
        None,
        "precondition: the forced undo re-deleted the photos"
    );
    assert_eq!(
        j.action(rm_id).unwrap().status,
        ActionStatus::Undone,
        "precondition: the rm's row still claims it is undone"
    );

    // THE BUG. The snapshot is right there. doover must not tell the user
    // their files are gone.
    let rep = e
        .undo(Selector::Action(rm_id), false, false)
        .expect("an action whose effect is back in force must be undoable again");
    assert!(rep.paths_restored >= 1);
    assert_eq!(
        r.read("photos/wedding.jpg").as_deref(),
        Some("irreplaceable"),
        "the photos came back from the snapshot that was never lost"
    );
}

/// The other half: when an action is undone AND the world still matches its
/// pre-state, there is genuinely nothing to do. Say so plainly. Do not run it
/// through the conflict oracle, which would correctly-but-uselessly report
/// "changed since the action" and exit 3.
#[test]
fn undoing_an_already_undone_action_is_a_clean_noop_not_a_conflict() {
    let r = rig();
    fs::write(r.cwd.join("data.txt"), "precious").unwrap();
    let rm_id = r.run("s1", "t1", "rm data.txt");

    let j = r.journal();
    let s = r.store();
    let e = UndoEngine::new(&j, &s);

    e.undo(Selector::Action(rm_id), false, false).unwrap();
    assert_eq!(r.read("data.txt").as_deref(), Some("precious"));

    let rep = e
        .undo(Selector::Action(rm_id), false, false)
        .expect("undoing an already-undone action is a no-op, not an error");
    assert!(
        rep.already_satisfied,
        "reported as already-undone, not as a restore"
    );
    assert_eq!(rep.paths_restored, 0);
    assert_eq!(
        r.read("data.txt").as_deref(),
        Some("precious"),
        "and the file is untouched"
    );
}

/// The advice must not go in a circle. The trial hit `undo N` -> "use redo to
/// revert it" -> `redo N` -> "nothing to do", with no way out. Whatever we
/// refuse, we must name the action id that does work.
#[test]
fn refusing_to_undo_an_undo_action_names_the_original_action() {
    let r = rig();
    fs::write(r.cwd.join("data.txt"), "precious").unwrap();
    let rm_id = r.run("s1", "t1", "rm data.txt");

    let j = r.journal();
    let s = r.store();
    let e = UndoEngine::new(&j, &s);
    let undo_id = e
        .undo(Selector::Action(rm_id), false, false)
        .unwrap()
        .recorded_as
        .unwrap();

    let err = e
        .undo(Selector::Action(undo_id), false, false)
        .expect_err("undoing an undo is still refused");
    let msg = err.to_string();
    assert!(
        msg.contains(&rm_id.to_string()),
        "the refusal must name the original action #{rm_id}, so the user has \
         somewhere to go. got: {msg}"
    );
}

// --- FINDING 2: `doover undo` (no args) picked a command that changed nothing -

/// The flagship command. In the trial it landed on a read-only command that had
/// been given a defensive working-directory snapshot, and either errored or --
/// worse -- would have "restored" a directory state that still had the user's
/// deleted file missing, reporting success.
///
/// A command whose post-state equals its pre-state changed nothing. Undoing it
/// is meaningless at best and reverts unrelated later work at worst. It must
/// never be the default target.
#[test]
fn plain_undo_skips_a_readonly_command_and_finds_the_destructive_one() {
    let r = rig();
    fs::write(r.cwd.join("data.txt"), "precious").unwrap();
    r.script(
        "readonly.sh",
        "#!/bin/sh\ncat data.txt >/dev/null 2>&1 || true\n",
    );

    let rm_id = r.run("s1", "t1", "rm data.txt");
    // Opaque but read-only: gets a full cwd snapshot, changes nothing.
    r.run("s1", "t2", "./readonly.sh");

    let j = r.journal();
    let s = r.store();
    let e = UndoEngine::new(&j, &s);

    let rep = e
        .undo(Selector::Latest, false, false)
        .expect("plain undo must find the rm, not the read-only script");
    assert_eq!(
        rep.target_action, rm_id,
        "targeted the command that actually changed something"
    );
    assert_eq!(
        r.read("data.txt").as_deref(),
        Some("precious"),
        "and the file is actually back"
    );
}

/// The mirror risk of the fix: don't over-filter. A destructive command the
/// harness killed mid-flight has no post-state, but it DID change the world and
/// it is exactly what the user wants back. It must still be reachable.
#[test]
fn plain_undo_still_finds_a_destructive_command_that_was_killed_mid_flight() {
    let r = rig();
    fs::write(r.cwd.join("data.txt"), "precious").unwrap();
    let rm_id = r.run_killed("s1", "t1", "rm data.txt");

    let j = r.journal();
    let s = r.store();
    let e = UndoEngine::new(&j, &s);

    assert_eq!(
        j.action(rm_id).unwrap().status,
        ActionStatus::Abandoned,
        "precondition: no post-state was recorded"
    );
    // No post-state means no oracle, so it needs --force -- but the SELECTION
    // must still land on it.
    let rep = e.undo(Selector::Latest, true, false).unwrap();
    assert_eq!(rep.target_action, rm_id);
    assert_eq!(r.read("data.txt").as_deref(), Some("precious"));
}

/// And a read-only command that was killed must NOT become the default target
/// either -- that is the precise shape the trial tripped over.
#[test]
fn plain_undo_skips_a_readonly_command_that_was_killed() {
    let r = rig();
    fs::write(r.cwd.join("data.txt"), "precious").unwrap();
    r.script(
        "readonly.sh",
        "#!/bin/sh\ncat data.txt >/dev/null 2>&1 || true\n",
    );

    let rm_id = r.run("s1", "t1", "rm data.txt");
    r.run_killed("s2", "t2", "./readonly.sh");

    let j = r.journal();
    let s = r.store();
    let e = UndoEngine::new(&j, &s);

    let rep = e.undo(Selector::Latest, false, false).unwrap();
    assert_eq!(
        rep.target_action, rm_id,
        "skipped the killed read-only script"
    );
    assert_eq!(r.read("data.txt").as_deref(), Some("precious"));
}

// --- FINDING 3: [redacted] on screen, the raw secret on disk -----------------

/// The journal displayed `Authorization: [redacted]` while writing
/// `Authorization: Bearer sk-live-...` into journal.db in plaintext. The column
/// is literally named `raw_command`.
///
/// This was documented, but documenting it does not fix it: showing a mask
/// while storing the secret manufactures false confidence, which is worse than
/// no redaction at all. Redact at WRITE time. The command string is display and
/// audit metadata -- undo restores from manifests and never reads it back.
///
/// This test greps the bytes of every file under DOOVER_HOME, which is exactly
/// how the trial found it (`strings journal.db`). Checking the deserialized row
/// alone would miss the write-ahead log.
#[test]
fn no_file_under_doover_home_contains_a_secret_in_plaintext() {
    let r = rig();
    let secret = "sk-live-Sup3rSecretDoNotLog";
    r.run(
        "s1",
        "t1",
        &format!("curl -H \"Authorization: Bearer {secret}\" -o out.txt https://example.invalid"),
    );

    let row = r.journal().action(1).unwrap();
    assert!(
        !row.raw_command.contains(secret),
        "the journal row still holds the secret: {}",
        row.raw_command
    );
    assert!(
        row.raw_command.contains("[redacted]"),
        "and it is masked, not just dropped: {}",
        row.raw_command
    );

    let needle = secret.as_bytes();
    let mut checked = 0usize;
    for entry in walkdir::WalkDir::new(&r.cfg.doover_home)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
    {
        let bytes = fs::read(entry.path()).unwrap_or_default();
        assert!(
            !bytes.windows(needle.len()).any(|w| w == needle),
            "{} contains the secret in plaintext",
            entry.path().display()
        );
        checked += 1;
    }
    assert!(checked > 0, "the walk actually inspected files");
}

// --- FINDING 4: an undo that reverts more than the command touched -----------

/// Undoing an opaque command restores the whole working directory, including
/// files the command never touched. The conflict oracle catches that (good),
/// but the advice it gives is `--force`, which then reverts everything (bad).
///
/// We cannot narrow the restore -- a defensive snapshot is all we have. What we
/// must do is say so, loudly, in the plan, BEFORE the user reaches for --force.
#[test]
fn restoring_a_defensive_snapshot_warns_that_it_reverts_the_whole_directory() {
    let r = rig();
    fs::write(r.cwd.join("a.txt"), "one").unwrap();
    fs::write(r.cwd.join("b.txt"), "two").unwrap();
    r.script("mystery.sh", "#!/bin/sh\nrm -f a.txt\n");

    let id = r.run("s1", "t1", "./mystery.sh");

    let j = r.journal();
    let s = r.store();
    let e = UndoEngine::new(&j, &s);

    let plan = e.undo(Selector::Action(id), false, true).unwrap();
    let text = format!("{}\n{}", plan.plan.join("\n"), plan.warnings.join("\n"));
    assert!(
        text.to_lowercase().contains("entire") || text.to_lowercase().contains("every file"),
        "the plan must warn that a defensive snapshot reverts the whole \
         directory, not just what the command touched. got:\n{text}"
    );
}

// --- FINDING 5: almost nothing classified `safe` ----------------------------
//
// 43 of 62 commands in the trial classified `unknown`, so doover walked and
// snapshotted the entire working directory for ~70% of everything the agent
// ran. `doover log` did it too. The unknown→snapshot-cwd fallback is the right
// safety FLOOR, but a floor you spend all your time on is a performance bug.
//
// The fix (registry/readonly.yaml) is the dangerous kind: every rule that says
// "safe" says "do not snapshot". A single wrong entry is silent data loss with
// no fallback -- exactly the class round 17 fixed for `wget -O`/`curl -o`. So
// the additions are pinned from both sides: the read-only commands must be
// safe, and the things that merely LOOK read-only must not be.

mod classification {
    use doover_core::registry::Registry;
    use doover_core::resolver::{Ctx, Severity, resolve};
    use std::path::Path;

    fn sev(cmd: &str) -> (Severity, bool) {
        let reg = Registry::builtin().unwrap();
        let ctx = Ctx {
            cwd: Path::new("/proj"),
            home: Path::new("/home/u"),
        };
        let r = resolve(cmd, &reg, &ctx);
        (r.severity, r.has_unknown)
    }

    /// The point of the change: these stop costing a full tree walk.
    #[test]
    fn provably_readonly_commands_no_longer_trigger_a_defensive_snapshot() {
        for cmd in [
            "doover log",
            "doover status",
            "doover show 3",
            "git rev-parse HEAD",
            "git ls-files",
            "git blame src/main.rs",
            "date",
            "whoami",
            "basename /a/b",
            "shasum -a 256 f.txt",
            "diff a.txt b.txt",
            "jq '.x' data.json",
            "printf 'hello\\n'",
        ] {
            let (s, unknown) = sev(cmd);
            assert_eq!(s, Severity::Safe, "`{cmd}` should be safe, got {s:?}");
            assert!(
                !unknown,
                "`{cmd}` must not trigger the defensive cwd snapshot"
            );
        }
    }

    /// The mirror, and the one that actually matters. A wrapper RUNS another
    /// command. If `env` or `xargs` or `sudo` were ever marked safe, then
    /// `env rm -rf /` inherits `safe`, doover snapshots NOTHING, and the tool
    /// silently fails at the only job it has.
    #[test]
    fn command_wrappers_are_never_safe() {
        for cmd in [
            "env rm -rf /important",
            "sudo rm -rf /important",
            "xargs rm < list.txt",
            "nohup ./destroy.sh",
            "timeout 5 rm -rf data",
            "nice rm -rf data",
            "command rm -rf data",
        ] {
            let (s, unknown) = sev(cmd);
            assert!(
                s != Severity::Safe,
                "`{cmd}` classified SAFE -- a wrapper must never inherit safe, \
                 this is silent data loss"
            );
            assert!(
                s >= Severity::Destructive || unknown,
                "`{cmd}` must be snapshotted, either precisely or defensively"
            );
        }
    }

    /// Commands that look read-only but take an output file. Each of these is a
    /// truncating write and must never have been let into readonly.yaml.
    #[test]
    fn commands_with_an_output_target_are_never_safe() {
        for cmd in [
            "uniq in.txt out.txt", // trailing positional is an OUTPUT file
            "xxd in.bin out.hex",  // same
            "sort -o victim.txt in.txt",
            "tee victim.txt",
            "tree -o victim.txt",
        ] {
            let (s, unknown) = sev(cmd);
            assert!(
                s != Severity::Safe,
                "`{cmd}` classified SAFE but it truncates a file"
            );
            assert!(
                s >= Severity::Destructive || unknown,
                "`{cmd}` must be snapshotted, either precisely or defensively"
            );
        }
    }
}
