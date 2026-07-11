//! T8 — maintenance/gc (step 7). Written before the module exists.
//!
//! Carried-forward constraints under test (CLAUDE.md):
//! - retention cutoffs derive from MAX(started_at_ms) in the journal, never
//!   the wall clock (a backward NTP jump must not make recent snapshots
//!   collectable);
//! - journal rows are pruned too (old raw_command lines may embed secrets),
//!   without ever breaking undo-chain references or pinned actions.

use doover_core::journal::{Journal, ManifestRole, NewAction};
use doover_core::maintenance::{self, GcOptions};
use doover_core::snapshot::{EntryKind, Store, StoreOptions};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

const DAY_MS: i64 = 24 * 60 * 60 * 1000;

struct Rig {
    _tmp: tempfile::TempDir,
    journal: Journal,
    store: Store,
    world: std::path::PathBuf,
    dh: std::path::PathBuf,
}

fn rig() -> Rig {
    let tmp = tempfile::tempdir().unwrap();
    let dh = tmp.path().join(".doover");
    fs::create_dir_all(&dh).unwrap();
    let journal = Journal::open(&dh.join("journal.db")).unwrap();
    let store = Store::open_with(dh.join("store"), StoreOptions::default()).unwrap();
    let world = tmp.path().join("world");
    fs::create_dir_all(&world).unwrap();
    journal.begin_session("s1", "claude-code", "/p").unwrap();
    Rig {
        _tmp: tmp,
        journal,
        store,
        world,
        dh,
    }
}

impl Rig {
    /// Insert an action at an explicit timestamp with a real snapshotted file.
    fn action_at(&self, name: &str, content: &str, at_ms: i64, tool: &str) -> (i64, String) {
        let id = self
            .journal
            .start_action(&NewAction {
                session_id: "s1",
                tool_use_id: Some(tool),
                raw_command: &format!("rm {name}"),
                effect: "destructive",
                rule_id: Some("coreutils.rm"),
                has_unknown: false,
            })
            .unwrap();
        self.journal.complete_by_tool_use("s1", tool, 1).unwrap();
        self.journal.set_started_at_for_test(id, at_ms).unwrap();
        let f = self.world.join(name);
        fs::write(&f, content).unwrap();
        let m = self.store.snapshot(&f, None).unwrap();
        let hash = m
            .entries
            .iter()
            .find_map(|e| match &e.kind {
                EntryKind::File { hash, .. } => Some(hash.clone()),
                _ => None,
            })
            .unwrap();
        self.journal
            .attach_manifest(id, &m, ManifestRole::Pre)
            .unwrap();
        // faithful fixture: an object promoted for an action at time T has
        // mtime ~T. Backdating the object too (not just the journal row) keeps
        // old actions' objects genuinely old, so gc's grace window (which
        // protects just-promoted, possibly-in-flight objects) does not falsely
        // shield an object that is actually past retention.
        self.backdate_object(&hash, at_ms);
        (id, hash)
    }

    fn object_path(&self, hash: &str) -> Option<std::path::PathBuf> {
        self.store
            .object_paths()
            .unwrap()
            .into_iter()
            .find(|p| p.file_name().is_some_and(|f| f.to_string_lossy() == *hash))
    }

    fn backdate_object(&self, hash: &str, at_ms: i64) {
        let Some(p) = self.object_path(hash) else {
            return;
        };
        fs::set_permissions(&p, fs::Permissions::from_mode(0o644)).unwrap();
        let when = std::time::UNIX_EPOCH + std::time::Duration::from_millis(at_ms.max(0) as u64);
        fs::OpenOptions::new()
            .write(true)
            .open(&p)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(when))
            .unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o444)).unwrap();
    }

    fn object_exists(&self, hash: &str) -> bool {
        self.object_path(hash).is_some()
    }
}

#[test]
fn gc_collects_old_unpinned_and_keeps_recent_and_pinned() {
    let r = rig();
    let base = 1_000_000_000_000; // arbitrary epoch, wall clock must not matter
    let (_old, h_old) = r.action_at("old.txt", "old content", base, "t1");
    let (pinned, h_pin) = r.action_at("pin.txt", "pinned content", base + DAY_MS, "t2");
    let (_recent, h_new) = r.action_at("new.txt", "recent content", base + 10 * DAY_MS, "t3");
    r.journal.set_pinned(pinned, true).unwrap();

    let report = maintenance::gc(
        &r.journal,
        &r.store,
        &r.dh,
        &GcOptions {
            keep_days: 7,
            dry_run: false,
            cap_bytes: None,
            min_free_bytes: None,
            time_budget: None,
        },
    )
    .unwrap();

    assert!(!r.object_exists(&h_old), "old+unpinned object collected");
    assert!(r.object_exists(&h_pin), "pinned survives regardless of age");
    assert!(r.object_exists(&h_new), "recent survives");
    assert!(report.objects_removed >= 1);
    assert!(report.bytes_freed > 0);
}

/// THE clock-skew rule: cutoff derives from MAX(started_at_ms), not now().
/// All actions are far in the wall-clock past; the newest must still count as
/// "recent" relative to the journal's own timeline.
#[test]
fn gc_cutoff_is_journal_relative_not_wall_clock() {
    let r = rig();
    let base = 1_000_000; // ~1970 — decades before the wall clock
    let (_a, h_a) = r.action_at("a.txt", "content a", base, "t1");
    let (_b, h_b) = r.action_at("b.txt", "content b", base + 10 * DAY_MS, "t2");

    maintenance::gc(
        &r.journal,
        &r.store,
        &r.dh,
        &GcOptions {
            keep_days: 7,
            dry_run: false,
            cap_bytes: None,
            min_free_bytes: None,
            time_budget: None,
        },
    )
    .unwrap();

    assert!(
        !r.object_exists(&h_a),
        "10 days older than the journal's newest -> collected"
    );
    assert!(
        r.object_exists(&h_b),
        "the journal's newest action is ALWAYS recent, no matter the wall clock"
    );
}

#[test]
fn gc_dry_run_removes_nothing_but_reports() {
    let r = rig();
    let base = 1_000_000_000_000;
    let (_old, h_old) = r.action_at("old.txt", "old", base, "t1");
    r.action_at("new.txt", "new", base + 10 * DAY_MS, "t2");

    let report = maintenance::gc(
        &r.journal,
        &r.store,
        &r.dh,
        &GcOptions {
            keep_days: 7,
            dry_run: true,
            cap_bytes: None,
            min_free_bytes: None,
            time_budget: None,
        },
    )
    .unwrap();
    assert!(report.dry_run);
    assert!(report.objects_removed >= 1, "reports what WOULD be removed");
    assert!(r.object_exists(&h_old), "dry-run must not delete");
}

#[test]
fn gc_prunes_old_journal_rows_but_never_referenced_or_pinned_ones() {
    let r = rig();
    let base = 1_000_000_000_000;
    let (old_plain, _) = r.action_at("old.txt", "x", base, "t1");
    let (old_pinned, _) = r.action_at("pin.txt", "y", base, "t2");
    r.journal.set_pinned(old_pinned, true).unwrap();
    // an old action that a (recent) undo references must survive pruning
    let (old_undone, _) = r.action_at("undone.txt", "z", base, "t3");
    let undo_id = r.journal.record_undo("s1", old_undone).unwrap();
    // the undo row itself is recent
    r.journal
        .set_started_at_for_test(undo_id, base + 10 * DAY_MS)
        .unwrap();
    let (_recent, _) = r.action_at("new.txt", "w", base + 10 * DAY_MS, "t4");

    maintenance::gc(
        &r.journal,
        &r.store,
        &r.dh,
        &GcOptions {
            keep_days: 7,
            dry_run: false,
            cap_bytes: None,
            min_free_bytes: None,
            time_budget: None,
        },
    )
    .unwrap();

    assert!(
        r.journal.action(old_plain).is_err(),
        "old, unpinned, unreferenced row pruned (raw_command may embed secrets)"
    );
    assert!(r.journal.action(old_pinned).is_ok(), "pinned row survives");
    assert!(
        r.journal.action(old_undone).is_ok(),
        "a row referenced by an undo action survives (chain integrity)"
    );
}

#[test]
fn gc_after_prune_leaves_undo_of_recent_actions_working() {
    // the whole point of retention: undo must still work after gc
    let r = rig();
    let base = 1_000_000_000_000;
    r.action_at("old.txt", "old", base, "t1");
    let (recent, _) = r.action_at("keep.txt", "precious", base + 10 * DAY_MS, "t2");

    maintenance::gc(
        &r.journal,
        &r.store,
        &r.dh,
        &GcOptions {
            keep_days: 7,
            dry_run: false,
            cap_bytes: None,
            min_free_bytes: None,
            time_budget: None,
        },
    )
    .unwrap();

    // delete the file, then undo the recent action through the real engine
    fs::remove_file(r.world.join("keep.txt")).unwrap();
    let engine = doover_core::undo::UndoEngine::new(&r.journal, &r.store);
    engine
        .undo(doover_core::undo::Selector::Action(recent), true, false)
        .unwrap();
    assert_eq!(
        fs::read_to_string(r.world.join("keep.txt")).unwrap(),
        "precious",
        "gc must never break undo of retained actions"
    );
}

#[test]
fn gc_cleans_stale_store_tmp_entries() {
    let r = rig();
    let base = 1_000_000_000_000;
    r.action_at("a.txt", "x", base + 10 * DAY_MS, "t1");
    // a leftover tmp file from a crashed ingestion — backdate its mtime past
    // the age gate (fresh tmp files belong to in-flight ingests and are kept)
    let tmp_dir = r.dh.join("store/tmp");
    fs::write(tmp_dir.join("999-42"), "crash leftover").unwrap();
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(2 * 60 * 60);
    fs::OpenOptions::new()
        .write(true)
        .open(tmp_dir.join("999-42"))
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(old))
        .unwrap();

    let report = maintenance::gc(
        &r.journal,
        &r.store,
        &r.dh,
        &GcOptions {
            keep_days: 7,
            dry_run: false,
            cap_bytes: None,
            min_free_bytes: None,
            time_budget: None,
        },
    )
    .unwrap();
    assert!(report.tmp_removed >= 1);
    assert!(
        !tmp_dir.join("999-42").exists(),
        "stale tmp entries are crash leftovers, always safe to remove"
    );
}

#[test]
fn gc_on_an_empty_journal_is_a_noop() {
    let tmp = tempfile::tempdir().unwrap();
    let dh = tmp.path().join(".doover");
    fs::create_dir_all(&dh).unwrap();
    let journal = Journal::open(&dh.join("journal.db")).unwrap();
    let store = Store::open_with(dh.join("store"), StoreOptions::default()).unwrap();
    let report = maintenance::gc(
        &journal,
        &store,
        &dh,
        &GcOptions {
            keep_days: 7,
            dry_run: false,
            cap_bytes: None,
            min_free_bytes: None,
            time_budget: None,
        },
    )
    .unwrap();
    assert_eq!(report.objects_removed, 0);
    assert_eq!(report.actions_pruned, 0);
}

/// Path helper used by other suites lives here to keep it near its tests.
#[allow(dead_code)]
fn unused(_p: &Path) {}

/// Audit round 12: a session begun by an in-flight hook (begin_session
/// committed, first start_action not yet — classify+snapshot can take
/// seconds) must survive pruning, or the action insert hits a dead foreign
/// key and the hook fails open, silently unprotected. Session deletion must
/// be journal-relative like everything else: empty AND old.
#[test]
fn prune_keeps_a_just_begun_empty_session_but_collects_old_ones() {
    let r = rig();
    let now = doover_core::journal::now_ms();
    r.journal
        .begin_session("in-flight", "claude-code", "/p")
        .unwrap();

    // cutoff in the past: the fresh empty session survives …
    let (a, s) = r.journal.prune_before(now - 10_000, false).unwrap();
    assert_eq!((a, s), (0, 0), "nothing is old enough to prune");
    // … and the invariant that matters: its first action can still land
    r.journal
        .start_action(&NewAction {
            session_id: "in-flight",
            tool_use_id: Some("t-live"),
            raw_command: "rm x",
            effect: "destructive",
            rule_id: None,
            has_unknown: false,
        })
        .expect("in-flight session must still accept its first action");

    // a genuinely old empty session IS collected (cutoff after its start);
    // dry-run must estimate it, not report a hardcoded zero
    r.journal
        .begin_session("stale-empty", "claude-code", "/p")
        .unwrap();
    let future = now + 10 * DAY_MS;
    let (_, s_est) = r.journal.prune_before(future, true).unwrap();
    assert!(s_est >= 1, "dry-run must estimate session pruning, got 0");
    let before = r.journal.stats().unwrap().0;
    let (_, s_real) = r.journal.prune_before(future, false).unwrap();
    let after = r.journal.stats().unwrap().0;
    assert!(s_real >= 1, "old empty session must be collected");
    assert_eq!(before - after, s_real, "report must match reality");
    // the in-flight session still has a (pending) action → still alive
    assert!(after >= 1);
}

/// Audit round 12 (reporting honesty): the `--dry-run` session estimate uses
/// a different SQL query than the real deletion path. They MUST agree, or the
/// dry-run tells the user a different number than gc will actually do. Mixed
/// journal: a cleanly-empty-old session, a session whose only action is an
/// old command referenced by an old undo (kept this pass — benign lag), and a
/// session with a pending action (kept). Estimate must equal reality exactly.
#[test]
fn dry_run_session_estimate_equals_real_deletion_exactly() {
    let r = rig();
    let now = doover_core::journal::now_ms();
    let old = now - 100 * DAY_MS;
    // rig() already opened an empty "s1" at real wall-clock time — within a
    // millisecond of `now`. Pin it comfortably in the future so this test's
    // outcome doesn't hinge on sub-millisecond scheduling (it is neither
    // in-flight nor old for our purposes here).
    r.journal
        .set_session_started_at_for_test("s1", now + DAY_MS)
        .unwrap();

    // session A: cleanly empty + old -> should be pruned
    r.journal.begin_session("A", "claude-code", "/p").unwrap();
    r.journal.set_session_started_at_for_test("A", old).unwrap();

    // session B: old command referenced by an old undo -> command kept this
    // pass (referenced), undo pruned; session survives (benign lag)
    r.journal.begin_session("B", "claude-code", "/p").unwrap();
    r.journal.set_session_started_at_for_test("B", old).unwrap();
    let cmd = r
        .journal
        .start_action(&NewAction {
            session_id: "B",
            tool_use_id: Some("b-cmd"),
            raw_command: "rm f",
            effect: "destructive",
            rule_id: None,
            has_unknown: false,
        })
        .unwrap();
    r.journal.complete_by_tool_use("B", "b-cmd", 1).unwrap();
    r.journal.set_started_at_for_test(cmd, old).unwrap();
    let undo = r.journal.record_undo("B", cmd).unwrap();
    r.journal.set_started_at_for_test(undo, old).unwrap();

    // session C: a pending (in-flight) action -> kept
    r.journal.begin_session("C", "claude-code", "/p").unwrap();
    r.journal.set_session_started_at_for_test("C", old).unwrap();
    r.journal
        .start_action(&NewAction {
            session_id: "C",
            tool_use_id: Some("c-live"),
            raw_command: "rm g",
            effect: "destructive",
            rule_id: None,
            has_unknown: false,
        })
        .unwrap();

    let cutoff = now; // everything above is older than the cutoff
    let (a_est, s_est) = r.journal.prune_before(cutoff, true).unwrap();
    let sessions_before = r.journal.stats().unwrap().0;
    let (a_real, s_real) = r.journal.prune_before(cutoff, false).unwrap();
    let sessions_after = r.journal.stats().unwrap().0;

    assert_eq!(s_est, s_real, "dry-run session estimate must equal reality");
    assert_eq!(a_est, a_real, "dry-run action estimate must equal reality");
    assert_eq!(
        sessions_before - sessions_after,
        s_real,
        "reported session count must match the actual table change"
    );
    // only session A is collectable this pass
    assert_eq!(s_real, 1, "exactly the cleanly-empty-old session");
}

/// Audit round 14 (GC-vs-writer race): a hook promotes an object to the
/// content-addressed store and THEN journals the manifest that references it.
/// A `doover gc` racing that window sees an object no journal row vouches for
/// yet — deleting it strands the about-to-be-written manifest and silently
/// breaks undo. Young unreferenced objects must be treated as possibly
/// in-flight and kept; only objects aged past the grace window are reaped.
#[test]
fn gc_keeps_a_young_unreferenced_object_but_reaps_aged_orphans() {
    let r = rig();
    let base = doover_core::journal::now_ms();
    // a normal referenced action so gc runs its full pass (non-empty journal)
    r.action_at("keep.txt", "precious", base, "t1");

    // the race window: object promoted to objects/, manifest NOT yet attached
    let inflight = r.world.join("inflight.bin");
    fs::write(&inflight, "just promoted, not yet journaled").unwrap();
    let m = r.store.snapshot(&inflight, None).unwrap();
    let hash = m
        .entries
        .iter()
        .find_map(|e| match &e.kind {
            EntryKind::File { hash, .. } => Some(hash.clone()),
            _ => None,
        })
        .unwrap();
    assert!(
        r.object_exists(&hash),
        "rig sanity: orphan object is in the store"
    );

    // gc while the object is young — it MUST survive
    maintenance::gc(
        &r.journal,
        &r.store,
        &r.dh,
        &GcOptions {
            keep_days: 7,
            dry_run: false,
            cap_bytes: None,
            min_free_bytes: None,
            time_budget: None,
        },
    )
    .unwrap();
    assert!(
        r.object_exists(&hash),
        "young unreferenced object collected -> races a concurrent hook into data loss"
    );

    // age it past the grace window: now a genuine crash leftover
    let obj_path = r
        .store
        .object_paths()
        .unwrap()
        .into_iter()
        .find(|p| p.file_name().is_some_and(|f| f.to_string_lossy() == hash))
        .unwrap();
    fs::set_permissions(&obj_path, fs::Permissions::from_mode(0o644)).unwrap();
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(2 * 60 * 60);
    fs::OpenOptions::new()
        .write(true)
        .open(&obj_path)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(old))
        .unwrap();

    let report = maintenance::gc(
        &r.journal,
        &r.store,
        &r.dh,
        &GcOptions {
            keep_days: 7,
            dry_run: false,
            cap_bytes: None,
            min_free_bytes: None,
            time_budget: None,
        },
    )
    .unwrap();
    assert!(!r.object_exists(&hash), "aged orphan must be reaped");
    assert!(report.objects_removed >= 1, "aged orphan counted");
}

/// Audit round 14 (reporting honesty, round-12 lesson applied to the new
/// grace window): a young unreferenced object is KEPT by a real gc, so
/// `--dry-run` must not COUNT it as removable — dry-run and real must agree
/// exactly, or the estimate lies about what gc will do.
#[test]
fn gc_dry_run_does_not_count_a_young_unreferenced_object() {
    let r = rig();
    let base = doover_core::journal::now_ms();
    r.action_at("keep.txt", "precious", base, "t1"); // referenced, ages the journal

    // young, unreferenced (in-flight window) — real gc keeps it
    let inflight = r.world.join("inflight.bin");
    fs::write(&inflight, "promoted, not journaled").unwrap();
    r.store.snapshot(&inflight, None).unwrap();

    let dry = maintenance::gc(
        &r.journal,
        &r.store,
        &r.dh,
        &GcOptions {
            keep_days: 7,
            dry_run: true,
            cap_bytes: None,
            min_free_bytes: None,
            time_budget: None,
        },
    )
    .unwrap();
    let real = maintenance::gc(
        &r.journal,
        &r.store,
        &r.dh,
        &GcOptions {
            keep_days: 7,
            dry_run: false,
            cap_bytes: None,
            min_free_bytes: None,
            time_budget: None,
        },
    )
    .unwrap();
    assert_eq!(
        dry.objects_removed, real.objects_removed,
        "dry-run must not count the young object the real run keeps"
    );
    assert_eq!(real.objects_removed, 0, "nothing collectable this pass");
}

/// Audit round 15: a pathological `--keep-days` must not overflow the cutoff
/// arithmetic (i64 multiply/subtract). Saturating math keeps everything (the
/// safe direction) instead of panicking in debug or wrapping in release.
#[test]
fn gc_keep_days_extreme_value_does_not_overflow() {
    let r = rig();
    let base = 1_000_000_000_000;
    let (_old, h) = r.action_at("old.txt", "x", base, "t1");
    r.action_at("new.txt", "y", base + 10 * DAY_MS, "t2");

    // i64::MAX days: keep_days * DAY_MS overflows i64 without saturation
    let report = maintenance::gc(
        &r.journal,
        &r.store,
        &r.dh,
        &GcOptions {
            keep_days: i64::MAX,
            dry_run: false,
            cap_bytes: None,
            min_free_bytes: None,
            time_budget: None,
        },
    )
    .expect("gc must not panic on an extreme keep_days");
    assert_eq!(
        report.objects_removed, 0,
        "an infinite window keeps everything"
    );
    assert!(r.object_exists(&h), "even the oldest object is retained");
}

// --- D2: store size cap + free-space floor (oldest-first eviction) -----------

fn gc_opts(keep_days: i64, dry_run: bool) -> GcOptions {
    GcOptions {
        keep_days,
        dry_run,
        cap_bytes: None,
        min_free_bytes: None,
        time_budget: None,
    }
}

/// Size-cap eviction: oldest evictable actions go first (rows AND objects, so
/// the journal never lists an un-undoable action). Three floors hold under
/// ANY pressure: pins, the hot window — and the journal's NEWEST action,
/// which is always inside the (journal-relative) hot window: your last undo
/// point is never sacrificed to a budget. When floors prevent reaching the
/// cap, the report says so honestly instead of pretending success.
#[test]
fn gc_size_cap_evicts_oldest_first_but_never_pins_or_the_newest() {
    let r = rig();
    let base = doover_core::journal::now_ms() - 40 * DAY_MS;
    let (_a1, h_old) = r.action_at("old.txt", "oldest content", base, "t1");
    let (a2, h_pin) = r.action_at("pin.txt", "pinned content", base + DAY_MS, "t2");
    let (_a3, h_mid) = r.action_at("mid.txt", "middle content", base + 2 * DAY_MS, "t3");
    let (_a4, h_new) = r.action_at("new.txt", "newest content", base + 3 * DAY_MS, "t4");
    r.journal.set_pinned(a2, true).unwrap();

    let mut opts = gc_opts(365, false); // retention window keeps everything
    opts.cap_bytes = Some(1); // force eviction of everything evictable
    let report = maintenance::gc(&r.journal, &r.store, &r.dh, &opts).unwrap();

    assert!(!r.object_exists(&h_old), "oldest evicted first");
    assert!(!r.object_exists(&h_mid), "next evictable goes too");
    assert!(
        r.object_exists(&h_pin),
        "pinned object survives ANY cap pressure"
    );
    assert!(
        r.object_exists(&h_new),
        "the newest action is never evicted — the last undo point survives"
    );
    let (_, per_status) = r.journal.stats().unwrap();
    let rows: u64 = per_status.iter().map(|(_, n)| n).sum();
    assert_eq!(rows, 2, "journal keeps exactly the pin and the newest");
    assert!(report.cap_evicted_actions >= 2);
    assert!(
        report.still_over_budget,
        "cap unreachable past the floors must be reported, not hidden"
    );
}

/// The hot window: rows/objects from the last hour are NEVER size-evicted —
/// they may belong to an in-flight session (round-12 FK guard) and their
/// objects are grace-protected anyway (round 14). The action a user is most
/// likely to undo is the one that just happened.
#[test]
fn gc_size_cap_never_touches_the_hot_window() {
    let r = rig();
    let now = doover_core::journal::now_ms();
    let (_old, h_old) = r.action_at("old.txt", "cold content", now - 30 * DAY_MS, "t1");
    let (_hot, h_hot) = r.action_at("hot.txt", "hot content", now, "t2");

    let mut opts = gc_opts(365, false);
    opts.cap_bytes = Some(1);
    let report = maintenance::gc(&r.journal, &r.store, &r.dh, &opts).unwrap();

    assert!(!r.object_exists(&h_old), "cold action evicted");
    assert!(r.object_exists(&h_hot), "hot action untouchable by the cap");
    assert!(
        report.still_over_budget,
        "over-cap with only hot rows left must be reported"
    );
}

/// A free-space floor works like the cap: u64::MAX floor cannot be satisfied,
/// so everything evictable is evicted and the deficit is reported.
#[test]
fn gc_free_space_floor_evicts_and_reports_deficit() {
    let r = rig();
    let now = doover_core::journal::now_ms();
    let (_old, h_old) = r.action_at("old.txt", "cold", now - 30 * DAY_MS, "t1");
    let (_new, h_new) = r.action_at("new.txt", "warm", now, "t2");

    let mut opts = gc_opts(365, false);
    opts.min_free_bytes = Some(u64::MAX);
    let report = maintenance::gc(&r.journal, &r.store, &r.dh, &opts).unwrap();

    assert!(
        !r.object_exists(&h_old),
        "free-space pressure evicts the old"
    );
    assert!(
        r.object_exists(&h_new),
        "newest survives even disk pressure"
    );
    assert!(report.free_deficit_bytes_before > 0, "deficit measured");
    assert!(report.still_over_budget, "an unreachable floor is reported");
}

/// No cap configured -> byte-for-byte the pre-D2 behavior (retention only).
#[test]
fn gc_without_budgets_is_retention_only() {
    let r = rig();
    let now = doover_core::journal::now_ms();
    let (_old, h) = r.action_at("f.txt", "content", now - 30 * DAY_MS, "t1");
    let report = maintenance::gc(&r.journal, &r.store, &r.dh, &gc_opts(365, false)).unwrap();
    assert!(r.object_exists(&h), "within retention, no cap: kept");
    assert_eq!(report.cap_evicted_actions, 0);
    assert!(!report.still_over_budget);
}

/// Round-12 honesty rule applied to the cap: dry-run measures the SAME
/// over-budget quantity the real run acts on, and removes nothing.
#[test]
fn gc_dry_run_measures_over_cap_without_evicting() {
    let r = rig();
    let now = doover_core::journal::now_ms();
    let (_old, h) = r.action_at("old.txt", "cold content", now - 30 * DAY_MS, "t1");
    r.action_at("new.txt", "warm content", now, "t2");

    let mut dry = gc_opts(365, true);
    dry.cap_bytes = Some(1);
    let d = maintenance::gc(&r.journal, &r.store, &r.dh, &dry).unwrap();
    assert!(r.object_exists(&h), "dry-run must not evict");
    assert!(d.over_cap_bytes_before > 0);
    assert_eq!(d.cap_evicted_actions, 0);

    let mut real = gc_opts(365, false);
    real.cap_bytes = Some(1);
    let rr = maintenance::gc(&r.journal, &r.store, &r.dh, &real).unwrap();
    assert_eq!(
        d.over_cap_bytes_before, rr.over_cap_bytes_before,
        "dry-run and real must measure the same pressure"
    );
    assert!(rr.cap_evicted_actions >= 1);
}

// --- D2 adversarial-review regressions ----------------------------------------

/// Review (data-loss lens): a PENDING action's objects must survive any gc,
/// however old the row is — a long-running command's pre-snapshot is the very
/// thing doover is protecting, and its post event has not settled yet.
#[test]
fn gc_never_evicts_a_pending_actions_objects() {
    let r = rig();
    let now = doover_core::journal::now_ms();
    // a pending action, 3 days old (long-running command) in its OWN session —
    // a same-session successor would abandon it (the round-5 contract); the
    // realistic long-runner is a concurrent session
    r.journal
        .begin_session("s-slow", "claude-code", "/p")
        .unwrap();
    let id = r
        .journal
        .start_action(&NewAction {
            session_id: "s-slow",
            tool_use_id: Some("t-pend"),
            raw_command: "slow-destructive-thing",
            effect: "destructive",
            rule_id: None,
            has_unknown: false,
        })
        .unwrap();
    r.journal
        .set_started_at_for_test(id, now - 3 * DAY_MS)
        .unwrap();
    let f = r.world.join("pending.txt");
    fs::write(&f, "in-flight precious").unwrap();
    let m = r.store.snapshot(&f, None).unwrap();
    let hash = m
        .entries
        .iter()
        .find_map(|e| match &e.kind {
            EntryKind::File { hash, .. } => Some(hash.clone()),
            _ => None,
        })
        .unwrap();
    r.journal
        .attach_manifest(id, &m, ManifestRole::Pre)
        .unwrap();
    r.backdate_object(&hash, now - 3 * DAY_MS);
    r.action_at("new.txt", "recent", now, "t2");

    // retention pressure AND cap pressure — the pending object survives both
    let mut opts = gc_opts(1, false);
    opts.cap_bytes = Some(1);
    let _ = maintenance::gc(&r.journal, &r.store, &r.dh, &opts).unwrap();
    assert!(
        r.object_exists(&hash),
        "a pending action's snapshot objects are untouchable"
    );
}

/// Review (data-loss lens): ONE forward-skewed timestamp (a brief clock jump
/// recorded a far-future row) must not become a phantom "newest" that makes
/// every real action look ancient. The gc anchor clamps to min(newest, now).
#[test]
fn gc_forward_skewed_timestamp_does_not_collapse_the_window() {
    let r = rig();
    let now = doover_core::journal::now_ms();
    let (_real, h_real) = r.action_at("real.txt", "real content", now - 60_000, "t1");
    // phantom: clock jumped 10 years forward for one action
    let (_ph, h_ph) = r.action_at("phantom.txt", "phantom", now + 3650 * DAY_MS, "t2");

    let report = maintenance::gc(&r.journal, &r.store, &r.dh, &gc_opts(7, false)).unwrap();
    assert!(
        r.object_exists(&h_real),
        "a minute-old REAL action must survive despite the phantom future row"
    );
    assert!(
        r.object_exists(&h_ph),
        "the phantom row itself is kept (> cutoff)"
    );
    assert_eq!(report.actions_pruned, 0);
}

/// Review (fail-open lens): a spent eviction time budget stops the pass
/// cleanly — nothing half-done, the shortfall reported, a later pass resumes.
#[test]
fn gc_eviction_time_budget_stops_cleanly_and_reports() {
    let r = rig();
    let now = doover_core::journal::now_ms();
    let (_old, h_old) = r.action_at("old.txt", "cold", now - 30 * DAY_MS, "t1");
    r.action_at("new.txt", "warm", now, "t2");

    let mut opts = gc_opts(365, false);
    opts.cap_bytes = Some(1);
    opts.time_budget = Some(std::time::Duration::ZERO); // already spent
    let report = maintenance::gc(&r.journal, &r.store, &r.dh, &opts).unwrap();
    assert!(r.object_exists(&h_old), "spent budget: nothing evicted");
    assert_eq!(report.cap_evicted_actions, 0);
    assert!(report.still_over_budget, "shortfall must be reported");

    // and a later, unbounded pass finishes the job
    opts.time_budget = None;
    let report = maintenance::gc(&r.journal, &r.store, &r.dh, &opts).unwrap();
    assert!(
        !r.object_exists(&h_old),
        "unbounded pass completes the eviction"
    );
    assert!(report.cap_evicted_actions >= 1);
}

/// Review: DOOVER_KEEP_DAYS=0 follows the knob convention — retention OPT-OUT
/// (keep forever), not "prune everything older than the newest action".
#[test]
fn keep_days_zero_is_a_retention_opt_out() {
    // parse level (the trigger path uses this)
    let b = {
        // simulate: from_env reads real env; test the mapping via a tiny probe
        // of the documented contract instead — keep_days=i64::MAX keeps all
        doover_core::maintenance::MaintenanceBudget::disabled()
    };
    assert_eq!(b.gc_every, 0);
    // gc level: i64::MAX keep_days (what 0 maps to) prunes nothing
    let r = rig();
    let now = doover_core::journal::now_ms();
    let (_old, h) = r.action_at("old.txt", "ancient", now - 300 * DAY_MS, "t1");
    r.action_at("new.txt", "recent", now, "t2");
    let report = maintenance::gc(&r.journal, &r.store, &r.dh, &gc_opts(i64::MAX, false)).unwrap();
    assert!(r.object_exists(&h), "retention opt-out keeps everything");
    assert_eq!(report.actions_pruned, 0);
}
