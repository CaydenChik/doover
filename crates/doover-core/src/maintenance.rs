//! Maintenance / garbage collection (step 7).
//!
//! Retention contract:
//! - the cutoff derives from MAX(started_at_ms) in the journal, NEVER the
//!   wall clock — a backward NTP jump must not make recent snapshots look
//!   collectable (carried-forward rule from the audit rounds);
//! - pinned actions and anything a live undo references survive regardless
//!   of age; the journal's newest action is always "recent" by definition;
//! - journal rows are pruned along with store objects (old raw_command
//!   strings may embed secrets), preserving chain references and pins;
//! - stale store/tmp entries (crash leftovers) are removed once they are
//!   comfortably older than any plausible in-flight ingestion.

use crate::journal::{Journal, JournalError};
use crate::snapshot::{SnapshotError, Store};
use std::path::Path;

const DAY_MS: i64 = 24 * 60 * 60 * 1000;
/// Tmp entries older than this are crash leftovers, not in-flight ingests.
const TMP_MAX_AGE_MS: u64 = 60 * 60 * 1000;
/// Size/free eviction never touches rows within this window of the journal's
/// newest action: they may belong to an in-flight session (round-12 FK
/// guard), their objects are grace-protected anyway (round 14), and the
/// just-ran action is the one a user most plausibly wants to undo. Aligned
/// with the object grace window on purpose.
const HOT_WINDOW_MS: i64 = TMP_MAX_AGE_MS as i64;
/// Rows evicted per size-cap iteration before re-measuring pressure.
const EVICTION_BATCH: u32 = 64;

#[derive(Debug, thiserror::Error)]
pub enum MaintenanceError {
    #[error(transparent)]
    Journal(#[from] JournalError),
    #[error(transparent)]
    Snapshot(#[from] SnapshotError),
}

pub struct GcOptions {
    /// Keep everything newer than this many days before the journal's newest
    /// action (journal-relative, not wall-clock).
    pub keep_days: i64,
    pub dry_run: bool,
    /// Store size ceiling (apparent bytes). When exceeded, the oldest
    /// evictable actions are removed — rows AND objects — until within the
    /// cap. Pins and the hot window are absolute floors. `None` = no cap.
    pub cap_bytes: Option<u64>,
    /// Minimum free space on the store's filesystem. Below it, eviction runs
    /// exactly like the cap. `None` = no floor. NOTE: the automatic post-hook
    /// trigger deliberately passes `None` here — deficit-driven eviction
    /// destroys history over pressure that is usually NOT doover's fault (and
    /// frees ~0 physical bytes on CoW filesystems), so it only ever runs from
    /// an explicit `doover gc`, where the report is in front of the user.
    pub min_free_bytes: Option<u64>,
    /// Wall-clock ceiling for the EVICTION loop (D1 discipline). On expiry
    /// the pass stops cleanly with `still_over_budget = true`; a later pass
    /// resumes where the row order left off. `None` = unbounded (manual gc).
    pub time_budget: Option<std::time::Duration>,
}

/// Env-driven maintenance settings shared by `doover gc` and the post-hook
/// trigger. Parsing is fail-safe (garbage → default); an explicit `0` is the
/// documented opt-out for each knob.
#[derive(Debug, Clone, Copy)]
pub struct MaintenanceBudget {
    /// DOOVER_MAX_STORE_BYTES (default 5 GiB; 0 = uncapped).
    pub cap_bytes: Option<u64>,
    /// DOOVER_MIN_FREE_BYTES (default 1 GiB; 0 = no floor). In the automatic
    /// path a breach triggers a retention+cap pass and a loud warning — it
    /// never drives eviction (that requires an explicit `doover gc`).
    pub min_free_bytes: Option<u64>,
    /// DOOVER_GC_EVERY: run gc from the post hook every N completed actions
    /// (default 50; 0 = NO automatic gc at all, including the free-space
    /// trigger — breaches then only warn).
    pub gc_every: u64,
    /// DOOVER_KEEP_DAYS (default 7; 0 = retention opt-out, keep forever) —
    /// the retention window the trigger uses.
    pub keep_days: i64,
}

impl MaintenanceBudget {
    pub fn from_env() -> Self {
        let get = |k: &str| std::env::var(k).ok();
        Self {
            cap_bytes: parse_opt_bytes(get("DOOVER_MAX_STORE_BYTES").as_deref(), 5 << 30),
            min_free_bytes: parse_opt_bytes(get("DOOVER_MIN_FREE_BYTES").as_deref(), 1 << 30),
            // clamped so the `as i64` in the cadence check can never wrap
            // negative and accidentally fire on every action
            gc_every: parse_u64_or(get("DOOVER_GC_EVERY").as_deref(), 50).min(i64::MAX as u64),
            // 0 follows the knob convention: opt OUT of retention (keep
            // forever). Without the special case, 0 would mean "prune
            // everything older than the newest action" — the exact opposite,
            // run silently from the trigger (D2 review).
            keep_days: match parse_u64_or(get("DOOVER_KEEP_DAYS").as_deref(), 7) {
                0 => i64::MAX,
                d => d.min(i64::MAX as u64) as i64,
            },
        }
    }

    /// No budgets and no trigger — the config for tests and for callers that
    /// only want explicit, manual gc.
    pub fn disabled() -> Self {
        Self {
            cap_bytes: None,
            min_free_bytes: None,
            gc_every: 0,
            keep_days: 7,
        }
    }
}

/// The exact GcOptions the AUTOMATIC (post-hook) trigger runs with. A pure
/// function so its load-bearing fields are pinned by unit test (round 18):
/// never dry-run, floor NEVER drives automatic eviction, and the pass always
/// carries the 3s time budget (D1 discipline on the hook path).
pub fn auto_gc_options(b: &MaintenanceBudget) -> GcOptions {
    GcOptions {
        keep_days: b.keep_days,
        dry_run: false,
        cap_bytes: b.cap_bytes,
        // deficit-driven eviction is a manual `doover gc` decision only
        min_free_bytes: None,
        time_budget: Some(std::time::Duration::from_secs(3)),
    }
}

/// Fail-safe byte-budget parse: unset/garbage → default, explicit 0 → None
/// (the opt-out). Config can never silently zero a protection budget.
fn parse_opt_bytes(v: Option<&str>, default: u64) -> Option<u64> {
    match v {
        None => Some(default),
        Some(s) => match s.trim().parse::<u64>() {
            Ok(0) => None,
            Ok(n) => Some(n),
            Err(_) => Some(default),
        },
    }
}

fn parse_u64_or(v: Option<&str>, default: u64) -> u64 {
    v.and_then(|s| s.trim().parse().ok()).unwrap_or(default)
}

#[derive(Debug, Default)]
pub struct GcReport {
    pub dry_run: bool,
    /// The journal-relative cutoff used (None = empty journal, no-op).
    pub cutoff_ms: Option<i64>,
    pub objects_removed: u64,
    pub bytes_freed: u64,
    pub actions_pruned: u64,
    pub sessions_pruned: u64,
    pub tmp_removed: u64,
    /// Bytes over `cap_bytes` measured BEFORE eviction — identical in dry-run
    /// and real mode (round-12 honesty: dry-run measures what real acts on).
    pub over_cap_bytes_before: u64,
    /// Free-space shortfall below `min_free_bytes` measured before eviction.
    pub free_deficit_bytes_before: u64,
    /// Actions evicted by the size/free pass (always 0 in dry-run — eviction
    /// is iterative and only measured, never simulated).
    pub cap_evicted_actions: u64,
    /// Real runs only: budgets still unmet after evicting everything
    /// evictable (pins + hot window are absolute floors). Surfaced so
    /// "bounded by pins" is a visible state, not silent failure.
    pub still_over_budget: bool,
}

pub fn gc(
    journal: &Journal,
    store: &Store,
    doover_home: &Path,
    opts: &GcOptions,
) -> Result<GcReport, MaintenanceError> {
    let mut report = GcReport {
        dry_run: opts.dry_run,
        ..Default::default()
    };

    // crash leftovers are collectable even on an empty journal
    report.tmp_removed = store.clean_tmp(TMP_MAX_AGE_MS, opts.dry_run)?;

    let Some(recorded_newest) = journal.max_started_at()? else {
        return Ok(report); // nothing journaled: nothing else to judge
    };
    // Clamp the anchor to now: one row with a forward-skewed timestamp (a
    // brief clock jump) must not become a phantom "newest" that makes every
    // REAL action look ancient — collapsing the hot window and dragging the
    // retention cutoff into the present. The clamp only engages when the
    // journal claims the future, so the round-7 rule (a BACKWARD jump must
    // not make recent snapshots collectable) is untouched: in that case
    // `recorded_newest` is in now's past and min() keeps it.
    let newest = recorded_newest.min(crate::journal::now_ms());
    // saturating: a pathological --keep-days must not overflow the i64 window
    // (panic in debug, wrap in release). Saturation lands at i64::MIN — an
    // infinite retention window that keeps everything, the safe direction.
    let cutoff = newest.saturating_sub(opts.keep_days.max(0).saturating_mul(DAY_MS));
    report.cutoff_ms = Some(cutoff);

    // store objects: everything the journal still vouches for stays. The
    // grace window keeps just-promoted objects a concurrent hook has not yet
    // journaled (GC-vs-writer race), so gc is safe to run while an agent works
    let live = journal.live_hashes(cutoff)?;
    let (objects_removed, bytes_freed) = store.prune(&live, TMP_MAX_AGE_MS, opts.dry_run)?;
    report.objects_removed = objects_removed;
    report.bytes_freed = bytes_freed;

    // journal rows: prune AFTER computing the live set so this pass's object
    // decisions were made against the journal state the user could inspect
    let (actions_pruned, sessions_pruned) = journal.prune_before(cutoff, opts.dry_run)?;
    report.actions_pruned = actions_pruned;
    report.sessions_pruned = sessions_pruned;

    // ---- size cap + free-space floor (D2): oldest-first eviction ----------
    // Pressure is measured in BOTH modes so dry-run reports what a real run
    // faces. The cap arm compares apparent bytes to apparent bytes, so
    // dry-run credits the bytes its simulated retention prune would have
    // freed. The DEFICIT arm is physical (statvfs) — apparent bytes must
    // never be subtracted from it (on CoW filesystems deleting a clone frees
    // ~0 physical blocks; the units simply differ), so no credit applies and
    // the dry-run deficit is an honest upper bound.
    let apparent = store.total_bytes()?;
    let cap_credit = if opts.dry_run { report.bytes_freed } else { 0 };
    let over_cap = match opts.cap_bytes {
        Some(cap) => apparent.saturating_sub(cap_credit).saturating_sub(cap),
        None => 0,
    };
    let deficit = free_deficit(doover_home, opts);
    report.over_cap_bytes_before = over_cap;
    report.free_deficit_bytes_before = deficit;
    if opts.dry_run || (over_cap == 0 && deficit == 0) {
        return Ok(report);
    }

    // The budgets outrank the retention window (disk-fill is the hard
    // failure), but never pins, never pending/chain-referenced rows, and
    // never the hot window. Eviction reuses the SAME audited pipeline as
    // retention gc — a cutoff advanced past the oldest evictable rows — so it
    // inherits the round-12/14 concurrency guarantees instead of new deletion
    // code. Apparent size is tracked incrementally (one walk, not one per
    // batch); the optional deadline keeps a triggered gc off the hook's
    // critical path (D1 discipline: bounded time, honest partial result).
    let deadline = opts.time_budget.map(|d| std::time::Instant::now() + d);
    let ceiling = newest.saturating_sub(HOT_WINDOW_MS);
    let mut apparent_now = apparent;
    loop {
        if deadline.is_some_and(|dl| std::time::Instant::now() >= dl) {
            report.still_over_budget = true;
            break;
        }
        let Some(batch_end) = journal.oldest_evictable_batch_end(EVICTION_BATCH, ceiling)? else {
            // nothing evictable is left below the hot window: budgets are
            // bounded by pins/hot rows. Report it — never force past floors.
            report.still_over_budget = true;
            break;
        };
        let evict_cutoff = batch_end.saturating_add(1).min(ceiling);
        let live = journal.live_hashes(evict_cutoff)?;
        let (objs, bytes) = store.prune(&live, TMP_MAX_AGE_MS, false)?;
        let (actions, sessions) = journal.prune_before(evict_cutoff, false)?;
        report.objects_removed += objs;
        report.bytes_freed += bytes;
        report.cap_evicted_actions += actions;
        report.sessions_pruned += sessions;

        apparent_now = apparent_now.saturating_sub(bytes);
        let over_cap = match opts.cap_bytes {
            Some(cap) => apparent_now.saturating_sub(cap),
            None => 0,
        };
        if over_cap == 0 && free_deficit(doover_home, opts) == 0 {
            break;
        }
        if actions == 0 && objs == 0 {
            // no forward progress (e.g. remaining old objects still inside
            // the mtime grace window): stop and say so rather than spin
            report.still_over_budget = true;
            break;
        }
    }
    Ok(report)
}

/// Bytes short of the free-space floor — zero when within budget or when the
/// signal is unavailable/degenerate (an unreadable filesystem must not
/// trigger history-destroying eviction over a phantom).
fn free_deficit(doover_home: &Path, opts: &GcOptions) -> u64 {
    match opts.min_free_bytes {
        Some(floor) => match crate::snapshot::free_bytes(doover_home) {
            Some(free) => floor.saturating_sub(free),
            None => 0,
        },
        None => 0,
    }
}

#[cfg(test)]
mod budget_parse_tests {
    use super::{parse_opt_bytes, parse_u64_or};

    #[test]
    fn byte_budget_parse_is_fail_safe() {
        // unset/garbage -> default: config can never silently zero a budget
        assert_eq!(parse_opt_bytes(None, 5 << 30), Some(5 << 30));
        assert_eq!(parse_opt_bytes(Some("garbage"), 5 << 30), Some(5 << 30));
        assert_eq!(parse_opt_bytes(Some("-1"), 5 << 30), Some(5 << 30));
        assert_eq!(parse_opt_bytes(Some(""), 5 << 30), Some(5 << 30));
        // explicit 0 is the documented opt-out
        assert_eq!(parse_opt_bytes(Some("0"), 5 << 30), None);
        // real values honored (whitespace tolerated)
        assert_eq!(parse_opt_bytes(Some(" 1048576 "), 5 << 30), Some(1_048_576));
        assert_eq!(parse_u64_or(Some("25"), 50), 25);
        assert_eq!(parse_u64_or(Some("junk"), 50), 50);
        assert_eq!(parse_u64_or(None, 50), 50);
    }
}

#[cfg(test)]
mod auto_gc_options_tests {
    use super::{MaintenanceBudget, auto_gc_options};

    /// Round 18 (mutation-confirmed gaps): these fields are load-bearing D2
    /// review constraints; a drive-by edit must fail HERE, not ship silently.
    #[test]
    fn auto_gc_options_pins_the_trigger_contract() {
        let b = MaintenanceBudget {
            cap_bytes: Some(123),
            min_free_bytes: Some(999), // set — and must NOT reach the options
            gc_every: 50,
            keep_days: 7,
        };
        let o = auto_gc_options(&b);
        assert!(!o.dry_run);
        assert_eq!(o.cap_bytes, Some(123), "cap passes through");
        assert_eq!(o.keep_days, 7, "retention passes through");
        assert_eq!(
            o.min_free_bytes, None,
            "floor NEVER drives automatic eviction (manual gc only)"
        );
        assert_eq!(
            o.time_budget,
            Some(std::time::Duration::from_secs(3)),
            "the triggered pass always carries the 3s budget (D1 discipline)"
        );
    }
}
