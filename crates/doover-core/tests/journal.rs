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
    // the name says WAL: assert it (audit round 3 — a claim without an assertion)
    let conn = rusqlite::Connection::open(&db).unwrap();
    let mode: String = conn
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .unwrap();
    assert_eq!(mode.to_lowercase(), "wal");

    let j = Journal::open(&db).unwrap(); // reopen
    let actions = j.session_actions("s1").unwrap();
    assert_eq!(actions.len(), 1);
    assert_eq!(actions[0].raw_command, "rm -rf ./x");
    assert_eq!(actions[0].seq, 1);
}

#[test]
fn resumed_session_updates_cwd() {
    let (_tmp, db) = mem_paths();
    let j = Journal::open(&db).unwrap();
    j.begin_session("s1", "claude-code", "/old/place").unwrap();
    j.begin_session("s1", "claude-code", "/new/place").unwrap();
    let cwd: String = rusqlite::Connection::open(&db)
        .unwrap()
        .query_row("SELECT cwd FROM sessions WHERE id = 's1'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        cwd, "/new/place",
        "a resumed session's cwd must not go stale"
    );
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

#[test]
fn manifest_from_a_newer_doover_is_refused_loudly() {
    // journals outlive binaries: a manifest written by a future schema must
    // refuse, not misparse
    let (tmp, db) = mem_paths();
    let j = Journal::open(&db).unwrap();
    j.begin_session("s1", "claude-code", "/p").unwrap();
    let a = j
        .start_action(&new_action("s1", "rm x", Some("t1")))
        .unwrap();
    let m = manifest_with_content(tmp.path(), "f.txt", "bytes");
    j.attach_manifest(a, &m).unwrap();

    // simulate a future doover having written this manifest
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute(
        "UPDATE manifests SET manifest_json =
            json_set(manifest_json, '$.schema', 999)",
        [],
    )
    .unwrap();

    let err = j.manifests(a).unwrap_err();
    assert!(
        err.to_string().contains("newer"),
        "must explain the version problem, got: {err}"
    );
}

#[test]
fn legacy_manifest_json_without_schema_field_still_reads() {
    // pre-versioning JSON deserializes with schema=0 rather than erroring
    let (tmp, db) = mem_paths();
    let j = Journal::open(&db).unwrap();
    j.begin_session("s1", "claude-code", "/p").unwrap();
    let a = j
        .start_action(&new_action("s1", "rm x", Some("t1")))
        .unwrap();
    let m = manifest_with_content(tmp.path(), "f.txt", "bytes");
    j.attach_manifest(a, &m).unwrap();

    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute(
        "UPDATE manifests SET manifest_json = json_remove(manifest_json, '$.schema')",
        [],
    )
    .unwrap();

    let stored = j.manifests(a).unwrap();
    assert_eq!(stored[0].schema, 0, "missing field defaults to 0 (legacy)");
    assert_eq!(stored[0].entries, m.entries, "content unaffected");
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
fn double_undo_is_refused() {
    // audit round 3: undoing an already-undone target appended duplicate rows
    let (_tmp, db) = mem_paths();
    let j = Journal::open(&db).unwrap();
    j.begin_session("s1", "claude-code", "/p").unwrap();
    let a = j
        .start_action(&new_action("s1", "rm x", Some("t1")))
        .unwrap();
    j.complete_by_tool_use("s1", "t1", 1).unwrap();
    j.record_undo("s1", a).unwrap();
    assert!(
        j.record_undo("s1", a).is_err(),
        "undoing an undone action must be refused (redo targets the undo, not the original)"
    );
}

#[test]
fn concurrent_double_undo_admits_exactly_one() {
    // audit round 3: the status check lived outside the transaction (TOCTOU)
    let (_tmp, db) = mem_paths();
    let j = Journal::open(&db).unwrap();
    j.begin_session("s1", "claude-code", "/p").unwrap();
    let a = j
        .start_action(&new_action("s1", "rm x", Some("t1")))
        .unwrap();
    j.complete_by_tool_use("s1", "t1", 1).unwrap();

    let db2 = db.clone();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let b2 = barrier.clone();
    let h = std::thread::spawn(move || {
        let j2 = Journal::open(&db2).unwrap();
        b2.wait();
        j2.record_undo("s1", a).is_ok()
    });
    barrier.wait();
    let r1 = j.record_undo("s1", a).is_ok();
    let r2 = h.join().unwrap();
    assert!(r1 ^ r2, "exactly one racer may win, got r1={r1} r2={r2}");
    let undo_rows = j
        .session_actions("s1")
        .unwrap()
        .iter()
        .filter(|r| r.target_action_id == Some(a))
        .count();
    assert_eq!(undo_rows, 1, "one target, one undo row");
}

#[test]
fn redo_restores_prior_status_not_a_fabricated_completed() {
    // audit round 3: redo hardcoded 'completed' even for abandoned targets
    let (_tmp, db) = mem_paths();
    let j = Journal::open(&db).unwrap();
    j.begin_session("s1", "claude-code", "/p").unwrap();
    let a = j
        .start_action(&new_action("s1", "false", Some("t1")))
        .unwrap();
    j.end_session("s1").unwrap(); // a -> abandoned (no post ever came)

    let u = j.record_undo("s1", a).unwrap();
    assert_eq!(j.action(a).unwrap().status, ActionStatus::Undone);
    assert_eq!(
        j.action(u).unwrap().target_prior_status,
        Some(ActionStatus::Abandoned),
        "the undo row must remember what it undid"
    );

    j.record_undo("s1", u).unwrap(); // redo
    assert_eq!(
        j.action(a).unwrap().status,
        ActionStatus::Abandoned,
        "redo must restore the TRUE prior status, never invent 'completed'"
    );
}

#[test]
fn duplicate_tool_use_id_completes_only_the_newest() {
    // audit round 3: an unbounded UPDATE completed every matching row
    let (_tmp, db) = mem_paths();
    let j = Journal::open(&db).unwrap();
    j.begin_session("s1", "claude-code", "/p").unwrap();
    let a = j
        .start_action(&new_action("s1", "cmd-one", Some("dup")))
        .unwrap();
    let b = j
        .start_action(&new_action("s1", "cmd-two", Some("dup")))
        .unwrap();

    let completed = j.complete_by_tool_use("s1", "dup", 7).unwrap();
    assert_eq!(completed, b, "the newest matching action wins");
    assert_eq!(j.action(b).unwrap().status, ActionStatus::Completed);
    assert_eq!(
        j.action(a).unwrap().status,
        ActionStatus::Abandoned,
        "the older duplicate keeps its abandoned status"
    );
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

#[test]
fn undo_of_a_redo_is_refused_with_pointer_to_the_original() {
    // audit round 4: cascading status through undo-of-redo chains is where
    // round 3's fix broke down; the design answer is to bound the chain —
    // command actions and first-level undos (redo) are undoable, deeper is
    // refused with guidance
    let (_tmp, db) = mem_paths();
    let j = Journal::open(&db).unwrap();
    j.begin_session("s1", "claude-code", "/p").unwrap();
    let a = j
        .start_action(&new_action("s1", "rm x", Some("t1")))
        .unwrap();
    j.complete_by_tool_use("s1", "t1", 1).unwrap();
    let u1 = j.record_undo("s1", a).unwrap();
    let r1 = j.record_undo("s1", u1).unwrap(); // redo — allowed

    let err = j.record_undo("s1", r1).unwrap_err();
    assert!(
        err.to_string().contains(&format!("original action {a}")),
        "refusal must point at the original, got: {err}"
    );
    // refusal must not perturb any status
    assert_eq!(j.action(a).unwrap().status, ActionStatus::Completed);
    assert_eq!(j.action(u1).unwrap().status, ActionStatus::Undone);
    assert_eq!(j.action(r1).unwrap().status, ActionStatus::Completed);

    // the sanctioned path still works: undo the original again
    let u2 = j.record_undo("s1", a).unwrap();
    assert_eq!(j.action(a).unwrap().status, ActionStatus::Undone);
    assert_eq!(j.action(u2).unwrap().status, ActionStatus::Completed);
}

// --- exhaustive small-model check (audit round 4) ---------------------------------
//
// Four audit rounds in a row found bugs one step past the hand-picked test
// paths. For a state machine the structural fix is exhaustive small-model
// testing: a trivially-correct reference model in the test, every sequence of
// undo attempts to depth 4, journal behavior compared against the model after
// every step.

mod small_model {
    use super::*;

    #[derive(Clone, Copy, PartialEq, Debug)]
    enum MStatus {
        Pending,
        Completed,
        Abandoned,
        Undone,
    }

    #[derive(Clone)]
    struct MAction {
        is_undo: bool,
        status: MStatus,
        target: Option<usize>,
        prior: Option<MStatus>,
    }

    /// Reference semantics: succeed iff target is completed/abandoned AND is a
    /// command or a first-level undo; on success append the undo, flip the
    /// target, and (for redo) restore the original's recorded prior status.
    fn model_apply(model: &mut Vec<MAction>, t: usize) -> bool {
        let tr = model[t].clone();
        if !matches!(tr.status, MStatus::Completed | MStatus::Abandoned) {
            return false;
        }
        if tr.is_undo && model[tr.target.unwrap()].is_undo {
            return false; // undo of a redo: bounded chain
        }
        model.push(MAction {
            is_undo: true,
            status: MStatus::Completed,
            target: Some(t),
            prior: Some(tr.status),
        });
        model[t].status = MStatus::Undone;
        if tr.is_undo {
            let original = tr.target.unwrap();
            model[original].status = tr.prior.unwrap();
        }
        true
    }

    fn to_model_status(s: ActionStatus) -> MStatus {
        match s {
            ActionStatus::Pending => MStatus::Pending,
            ActionStatus::Completed => MStatus::Completed,
            ActionStatus::Abandoned => MStatus::Abandoned,
            ActionStatus::Undone => MStatus::Undone,
        }
    }

    /// One journal per sequence: a completed, b abandoned, c pending.
    fn fresh() -> (tempfile::TempDir, Journal, Vec<i64>, Vec<MAction>) {
        let tmp = tempfile::tempdir().unwrap();
        let j = Journal::open(&tmp.path().join("j.db")).unwrap();
        j.begin_session("s1", "claude-code", "/p").unwrap();
        let a = j.start_action(&new_action("s1", "a", Some("ta"))).unwrap();
        j.complete_by_tool_use("s1", "ta", 1).unwrap();
        let b = j.start_action(&new_action("s1", "b", Some("tb"))).unwrap();
        let c = j.start_action(&new_action("s1", "c", Some("tc"))).unwrap(); // abandons b
        let ids = vec![a, b, c];
        let model = vec![
            MAction {
                is_undo: false,
                status: MStatus::Completed,
                target: None,
                prior: None,
            },
            MAction {
                is_undo: false,
                status: MStatus::Abandoned,
                target: None,
                prior: None,
            },
            MAction {
                is_undo: false,
                status: MStatus::Pending,
                target: None,
                prior: None,
            },
        ];
        (tmp, j, ids, model)
    }

    fn run_sequence(seq: &[usize]) -> Result<(), String> {
        let (_tmp, j, mut ids, mut model) = fresh();
        for (step, &t) in seq.iter().enumerate() {
            if t >= model.len() {
                return Ok(()); // target doesn't exist yet in this sequence; skip
            }
            let expect_ok = model_apply(&mut model, t);
            let got = j.record_undo("s1", ids[t]);
            if expect_ok != got.is_ok() {
                return Err(format!(
                    "seq {seq:?} step {step}: model says ok={expect_ok}, journal says {got:?}"
                ));
            }
            if let Ok(id) = got {
                ids.push(id);
            }
            for (i, m) in model.iter().enumerate() {
                let actual = to_model_status(j.action(ids[i]).unwrap().status);
                if actual != m.status {
                    return Err(format!(
                        "seq {seq:?} step {step}: action #{i} model={:?} journal={actual:?}",
                        m.status
                    ));
                }
            }
        }
        Ok(())
    }

    #[test]
    fn every_undo_sequence_to_depth_4_matches_the_reference_model() {
        // targets can reference actions created mid-sequence: enumerate over a
        // generous index space and skip not-yet-existing targets
        const DEPTH: usize = 4;
        const MAX_IDX: usize = 7; // 3 initial + up to 4 created
        let mut failures = Vec::new();
        let mut checked = 0usize;

        let mut seq = vec![0usize; DEPTH];
        'outer: loop {
            if let Err(msg) = run_sequence(&seq) {
                failures.push(msg);
                if failures.len() > 5 {
                    break;
                }
            }
            checked += 1;
            // odometer increment
            for d in (0..DEPTH).rev() {
                seq[d] += 1;
                if seq[d] < MAX_IDX {
                    continue 'outer;
                }
                seq[d] = 0;
            }
            break;
        }
        assert!(
            failures.is_empty(),
            "journal diverges from the reference model:\n{}",
            failures.join("\n")
        );
        assert!(checked >= 2000, "enumeration shrank: {checked}");
    }
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
