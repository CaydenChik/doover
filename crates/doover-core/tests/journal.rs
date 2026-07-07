//! T4 — journal test suite (doover-implementation-plan.md §3).
//! Written before the journal module exists; drives its design.
//!
//! Design constraints from the live hook capture (fixtures README):
//! - there is no exit code; success == a PostToolUse arrived
//! - failed commands emit NO post event → pendings are closed as `abandoned`
//!   when the next action starts or the session ends
//! - `tool_use_id` correlates pre/post pairs

use doover_core::journal::{ActionKind, ActionStatus, Journal, NewAction};
use doover_core::snapshot::{Store, StoreOptions};
use std::io::BufRead;
use std::path::Path;

fn mem_paths() -> (tempfile::TempDir, std::path::PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("journal.db");
    (tmp, db)
}

fn new_action<'a>(session: &'a str, cmd: &'a str, tool_use: Option<&'a str>) -> NewAction<'a> {
    NewAction {
        session_id: session,
        tool_use_id: tool_use,
        raw_command: cmd,
        effect: "destructive",
        rule_id: Some("coreutils.rm"),
        has_unknown: false,
    }
}

/// Snapshot a real file so manifests carry a genuine store hash.
fn manifest_with_content(dir: &Path, name: &str, content: &str) -> doover_core::snapshot::Manifest {
    let store = Store::open_with(dir.join("store"), StoreOptions::default()).unwrap();
    let f = dir.join(name);
    std::fs::write(&f, content).unwrap();
    store.snapshot(&f, None).unwrap()
}

// --- schema & lifecycle --------------------------------------------------------

#[test]
fn open_creates_wal_schema_and_reopen_preserves() {
    let (_tmp, db) = mem_paths();
    {
        let j = Journal::open(&db).unwrap();
        j.begin_session("s1", "claude-code", "/tmp/proj").unwrap();
        j.start_action(&new_action("s1", "rm -rf ./x", Some("toolu_1")))
            .unwrap();
    }
    let j = Journal::open(&db).unwrap(); // reopen
    let actions = j.session_actions("s1").unwrap();
    assert_eq!(actions.len(), 1);
    assert_eq!(actions[0].raw_command, "rm -rf ./x");
    assert_eq!(actions[0].seq, 1);
}

#[test]
fn garbage_file_is_a_clear_error_not_a_panic() {
    let (_tmp, db) = mem_paths();
    std::fs::write(&db, "this is not a sqlite database, honest").unwrap();
    assert!(Journal::open(&db).is_err());
}

#[test]
fn sequences_are_per_session_and_monotonic() {
    let (_tmp, db) = mem_paths();
    let j = Journal::open(&db).unwrap();
    j.begin_session("s1", "claude-code", "/a").unwrap();
    j.begin_session("s2", "claude-code", "/b").unwrap();
    let a1 = j.start_action(&new_action("s1", "ls", None)).unwrap();
    let a2 = j.start_action(&new_action("s1", "pwd", None)).unwrap();
    let b1 = j.start_action(&new_action("s2", "ls", None)).unwrap();
    assert_eq!(j.action(a1).unwrap().seq, 1);
    assert_eq!(j.action(a2).unwrap().seq, 2);
    assert_eq!(
        j.action(b1).unwrap().seq,
        1,
        "sessions sequence independently"
    );
}

#[test]
fn concurrent_writers_get_unique_contiguous_seqs() {
    // hook invocations are separate processes: model with two connections
    // racing on one session
    let (_tmp, db) = mem_paths();
    Journal::open(&db)
        .unwrap()
        .begin_session("s1", "claude-code", "/p")
        .unwrap();
    const N: usize = 40;
    let db2 = db.clone();
    let t = std::thread::spawn(move || {
        let j = Journal::open(&db2).unwrap();
        for i in 0..N {
            j.start_action(&new_action("s1", &format!("t2-{i}"), None))
                .unwrap();
        }
    });
    let j = Journal::open(&db).unwrap();
    for i in 0..N {
        j.start_action(&new_action("s1", &format!("t1-{i}"), None))
            .unwrap();
    }
    t.join().unwrap();
    let mut seqs: Vec<i64> = j
        .session_actions("s1")
        .unwrap()
        .iter()
        .map(|a| a.seq)
        .collect();
    seqs.sort_unstable();
    assert_eq!(
        seqs,
        (1..=(2 * N as i64)).collect::<Vec<_>>(),
        "unique and contiguous"
    );
}

// --- the missing-post rule -------------------------------------------------------

#[test]
fn pending_without_post_is_abandoned_when_next_action_starts() {
    let (_tmp, db) = mem_paths();
    let j = Journal::open(&db).unwrap();
    j.begin_session("s1", "claude-code", "/p").unwrap();
    let a = j
        .start_action(&new_action("s1", "false", Some("toolu_a")))
        .unwrap();
    assert_eq!(j.action(a).unwrap().status, ActionStatus::Pending);

    let b = j
        .start_action(&new_action("s1", "ls", Some("toolu_b")))
        .unwrap();
    assert_eq!(
        j.action(a).unwrap().status,
        ActionStatus::Abandoned,
        "no post event ever came for `false` — closed at next action"
    );
    assert_eq!(j.action(b).unwrap().status, ActionStatus::Pending);
}

#[test]
fn end_session_abandons_remaining_pendings_and_stamps_end() {
    let (_tmp, db) = mem_paths();
    let j = Journal::open(&db).unwrap();
    j.begin_session("s1", "claude-code", "/p").unwrap();
    let a = j.start_action(&new_action("s1", "false", None)).unwrap();
    j.end_session("s1").unwrap();
    assert_eq!(j.action(a).unwrap().status, ActionStatus::Abandoned);
}

#[test]
fn completion_correlates_by_tool_use_id() {
    let (_tmp, db) = mem_paths();
    let j = Journal::open(&db).unwrap();
    j.begin_session("s1", "claude-code", "/p").unwrap();
    let a = j
        .start_action(&new_action("s1", "rm x", Some("toolu_abc")))
        .unwrap();
    let completed = j.complete_by_tool_use("s1", "toolu_abc", 994).unwrap();
    assert_eq!(completed, a);
    let rec = j.action(a).unwrap();
    assert_eq!(rec.status, ActionStatus::Completed);
    assert_eq!(rec.duration_ms, Some(994));

    // unknown correlation id is a loud error, not a silent no-op
    assert!(j.complete_by_tool_use("s1", "toolu_nope", 1).is_err());
}

#[test]
fn late_post_after_abandonment_self_heals_to_completed() {
    // interleaved/background tool calls could deliver a post AFTER the next
    // action's start already abandoned its pre; a late post is better data
    // than our guess and must win
    let (_tmp, db) = mem_paths();
    let j = Journal::open(&db).unwrap();
    j.begin_session("s1", "claude-code", "/p").unwrap();
    let a = j
        .start_action(&new_action("s1", "slow-thing", Some("toolu_slow")))
        .unwrap();
    let _b = j
        .start_action(&new_action("s1", "next", Some("toolu_next")))
        .unwrap();
    assert_eq!(j.action(a).unwrap().status, ActionStatus::Abandoned);

    let healed = j.complete_by_tool_use("s1", "toolu_slow", 1234).unwrap();
    assert_eq!(healed, a);
    assert_eq!(j.action(a).unwrap().status, ActionStatus::Completed);
    assert_eq!(j.action(a).unwrap().duration_ms, Some(1234));
}

// --- manifests -------------------------------------------------------------------

#[test]
fn manifest_round_trips_exactly() {
    let (tmp, db) = mem_paths();
    let j = Journal::open(&db).unwrap();
    j.begin_session("s1", "claude-code", "/p").unwrap();
    let a = j
        .start_action(&new_action("s1", "rm фото.jpg", Some("t1")))
        .unwrap();
    let m = manifest_with_content(tmp.path(), "фото 📸.jpg", "precious bytes");
    j.attach_manifest(a, &m).unwrap();

    let stored = j.manifests(a).unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0], m, "serde round-trip must be lossless");
}

// --- undo chains -----------------------------------------------------------------

#[test]
fn undo_is_an_action_and_undo_of_undo_is_redo() {
    let (_tmp, db) = mem_paths();
    let j = Journal::open(&db).unwrap();
    j.begin_session("s1", "claude-code", "/p").unwrap();
    let a = j
        .start_action(&new_action("s1", "rm x", Some("t1")))
        .unwrap();
    j.complete_by_tool_use("s1", "t1", 5).unwrap();

    // undo: a new journaled action, target marked undone
    let u = j.record_undo("s1", a).unwrap();
    let u_rec = j.action(u).unwrap();
    assert_eq!(u_rec.kind, ActionKind::Undo);
    assert_eq!(u_rec.target_action_id, Some(a));
    assert_eq!(u_rec.status, ActionStatus::Completed);
    assert_eq!(j.action(a).unwrap().status, ActionStatus::Undone);
    assert!(
        u_rec.seq > j.action(a).unwrap().seq,
        "history is append-only"
    );

    // redo: undoing the undo flips the original back
    let r = j.record_undo("s1", u).unwrap();
    assert_eq!(j.action(u).unwrap().status, ActionStatus::Undone);
    assert_eq!(
        j.action(a).unwrap().status,
        ActionStatus::Completed,
        "undo-of-undo restores the original's status"
    );
    assert_eq!(j.action(r).unwrap().kind, ActionKind::Undo);
}

#[test]
fn undoing_a_pending_action_is_refused() {
    let (_tmp, db) = mem_paths();
    let j = Journal::open(&db).unwrap();
    j.begin_session("s1", "claude-code", "/p").unwrap();
    let a = j.start_action(&new_action("s1", "rm x", None)).unwrap();
    assert!(
        j.record_undo("s1", a).is_err(),
        "cannot undo an in-flight action"
    );
}

// --- GC support ------------------------------------------------------------------

#[test]
fn live_hashes_honor_pins_and_recency() {
    let (tmp, db) = mem_paths();
    let j = Journal::open(&db).unwrap();
    j.begin_session("s1", "claude-code", "/p").unwrap();

    let mk = |name: &str, content: &str| manifest_with_content(tmp.path(), name, content);
    let h = |m: &doover_core::snapshot::Manifest| -> String {
        match &m.entries[0].kind {
            doover_core::snapshot::EntryKind::File { hash, .. } => hash.clone(),
            other => panic!("expected file entry, got {other:?}"),
        }
    };

    let old_unpinned = j.start_action(&new_action("s1", "a", Some("t1"))).unwrap();
    let m1 = mk("f1", "content one");
    j.attach_manifest(old_unpinned, &m1).unwrap();

    let old_pinned = j.start_action(&new_action("s1", "b", Some("t2"))).unwrap();
    let m2 = mk("f2", "content two");
    j.attach_manifest(old_pinned, &m2).unwrap();
    j.set_pinned(old_pinned, true).unwrap();

    let cutoff = doover_core::journal::now_ms() + 1;
    std::thread::sleep(std::time::Duration::from_millis(5));

    let recent = j.start_action(&new_action("s1", "c", Some("t3"))).unwrap();
    let m3 = mk("f3", "content three");
    j.attach_manifest(recent, &m3).unwrap();

    let live = j.live_hashes(cutoff).unwrap();
    assert!(!live.contains(&h(&m1)), "old + unpinned is collectable");
    assert!(live.contains(&h(&m2)), "pinned survives regardless of age");
    assert!(live.contains(&h(&m3)), "recent survives regardless of pin");
}

// --- crash safety ----------------------------------------------------------------

/// Child half of the kill-9 test: writes one committed action, then leaves a
/// second insert dangling inside an open transaction and blocks forever.
/// Skipped unless invoked by the parent below.
#[test]
fn kill9_child_writer() {
    let Ok(db) = std::env::var("DOOVER_T4_CHILD_DB") else {
        return;
    };
    let j = Journal::open(Path::new(&db)).unwrap();
    j.begin_session("crash", "claude-code", "/p").unwrap();
    j.start_action(&new_action("crash", "committed-before-crash", Some("t1")))
        .unwrap();

    // raw uncommitted write through a second connection
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.busy_timeout(std::time::Duration::from_secs(5))
        .unwrap();
    conn.execute_batch("BEGIN IMMEDIATE;").unwrap();
    conn.execute(
        "INSERT INTO actions(session_id, seq, kind, raw_command, effect, status, started_at_ms)
         VALUES ('crash', 999, 'command', 'torn-write-should-vanish', 'safe', 'pending', 0)",
        [],
    )
    .unwrap();
    println!("READY");
    std::thread::sleep(std::time::Duration::from_secs(60)); // parent kill -9s us here
}

#[test]
fn kill9_mid_transaction_leaves_no_torn_rows() {
    let (_tmp, db) = mem_paths();
    let exe = std::env::current_exe().unwrap();
    let mut child = std::process::Command::new(exe)
        .args(["kill9_child_writer", "--exact", "--nocapture"])
        .env("DOOVER_T4_CHILD_DB", &db)
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut lines = std::io::BufReader::new(stdout).lines();
    loop {
        let line = lines.next().expect("child exited before READY").unwrap();
        if line.trim() == "READY" {
            break;
        }
    }
    // SIGKILL: no destructors, no rollback — WAL must recover on next open
    child.kill().unwrap();
    child.wait().unwrap();

    let j = Journal::open(&db).unwrap();
    let actions = j.session_actions("crash").unwrap();
    assert_eq!(
        actions.len(),
        1,
        "uncommitted row must not survive: {actions:?}"
    );
    assert_eq!(actions[0].raw_command, "committed-before-crash");
    assert!(
        j.integrity_check().unwrap(),
        "database must pass integrity_check"
    );

    // and the journal is fully writable afterwards
    let after = j
        .start_action(&new_action("crash", "post-crash-write", None))
        .unwrap();
    assert_eq!(j.action(after).unwrap().seq, 2);
}
