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
}

pub fn gc(
    journal: &Journal,
    store: &Store,
    _doover_home: &Path,
    opts: &GcOptions,
) -> Result<GcReport, MaintenanceError> {
    let mut report = GcReport {
        dry_run: opts.dry_run,
        ..Default::default()
    };

    // crash leftovers are collectable even on an empty journal
    report.tmp_removed = store.clean_tmp(TMP_MAX_AGE_MS, opts.dry_run)?;

    let Some(newest) = journal.max_started_at()? else {
        return Ok(report); // nothing journaled: nothing else to judge
    };
    let cutoff = newest - opts.keep_days.max(0) * DAY_MS;
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

    Ok(report)
}
