//! Undo/redo engine (step 6): the user-facing payoff of everything upstream.
//!
//! Model: each protected action carries PRE manifests (state before the
//! command — undo restores these) and POST manifests (state after — redo
//! restores these, and they answer "is the world still as the action left
//! it?" for conflict detection). The journal's bounded chain semantics do the
//! bookkeeping: undo is itself a journaled action; redo = undo of the undo;
//! undoing a redo is refused with a pointer to the original.
//!
//! Safety posture:
//! - conflict-checked by default: if the touched paths changed since the
//!   action (user edits, later agent actions) — or if the action failed and
//!   left no post-state to verify against — refuse unless --force;
//! - all-or-nothing and restore-BEFORE-record: a partial failure rolls the
//!   restored paths back and returns an error WITHOUT recording, so the target
//!   stays retryable and the journal never claims an undo that only half
//!   happened;
//! - dry-run plans without writing anything, journal included.

use crate::journal::{
    ActionId, ActionKind, ActionRecord, ActionStatus, Journal, JournalError, ManifestRole,
};
use crate::snapshot::{Manifest, Root, SnapshotError, Store};
use std::path::PathBuf;

/// How far back a bare `doover undo` looks for an action worth undoing. Deep
/// enough to see past a run of read-only commands, bounded so the scan cannot
/// walk the whole journal.
const UNDO_CANDIDATE_SCAN: usize = 128;

/// Whether an action's effect is still in force on disk — the question bare
/// `doover undo` asks the filesystem about each candidate.
enum Restorability {
    /// The world differs from the snapshot; restoring would change something.
    InForce,
    /// The world already equals the snapshot; nothing left to restore.
    AlreadyDone,
    /// Could not tell (unreadable file or truncated capture). Never treated as
    /// AlreadyDone, so an unreadable file can't make an action look undone.
    Indeterminate,
}

#[derive(Debug)]
pub enum Selector {
    /// The most recent plausible target (undoable command / live undo).
    Latest,
    /// A specific journal action id.
    Action(ActionId),
}

/// Do two manifest sets describe the same filesystem state? Used to tell a
/// command that changed something from one that did not (a read-only command
/// given a defensive snapshot has POST == PRE).
///
/// Conservative in the direction that matters: a truncated capture proves
/// nothing, so it never reads as "unchanged" — the action stays a candidate for
/// undo rather than being silently filtered out of reach.
fn same_state(a: &[Manifest], b: &[Manifest]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().all(|m| {
        b.iter()
            .find(|o| o.path == m.path)
            .is_some_and(|o| m.describes_same_state(o))
    })
}

#[derive(Debug, thiserror::Error)]
pub enum UndoError {
    #[error(
        "nothing to undo among recent actions: none of the latest commands left \
         changes that are still in force. `doover log` lists the full history; undo \
         a specific one with `doover undo <id>`."
    )]
    NoUndoableAction,
    #[error("action {id} has no restorable snapshot ({reason}); nothing to do")]
    NothingToRestore { id: ActionId, reason: String },
    #[error("action {id} cannot be {verb}: {reason}")]
    NotUndoable {
        id: ActionId,
        verb: &'static str,
        reason: String,
    },
    #[error(
        "refusing: the world changed since this action (use --force to restore anyway):\n{}",
        .0.join("\n")
    )]
    Conflicts(Vec<String>),
    #[error(transparent)]
    Journal(#[from] JournalError),
    #[error(transparent)]
    Snapshot(#[from] SnapshotError),
    #[error(
        "restore of {path} failed ({cause}); the partial restore was rolled back — \
         nothing changed, safe to retry"
    )]
    PartialRolledBack { path: String, cause: String },
    #[error(
        "restore of {path} failed ({cause}) AND rollback failed for: {}; \
         the tree is in a mixed state — inspect before retrying",
        .rollback_failures.join(", ")
    )]
    PartialInconsistent {
        path: String,
        cause: String,
        rollback_failures: Vec<String>,
    },
}

#[derive(Debug)]
pub struct UndoReport {
    /// The action whose effect was reverted (undo) or re-applied (redo).
    pub target_action: ActionId,
    /// The new journal row recording this undo/redo (absent on dry-run).
    pub recorded_as: Option<ActionId>,
    pub paths_restored: usize,
    pub forced: bool,
    pub dry_run: bool,
    /// The world already matched the target state, so nothing was restored and
    /// nothing was journaled. Undo is idempotent; this says so out loud instead
    /// of reporting a conflict the user cannot act on.
    pub already_satisfied: bool,
    /// Human-readable restore plan, one line per path.
    pub plan: Vec<String>,
    pub warnings: Vec<String>,
    /// The paths actually restored. A directory restore replaces the directory
    /// itself (stage-then-swap), so a shell standing inside one is left with a
    /// stale cwd; the CLI uses this to tell the user to `cd .`.
    pub restored_paths: Vec<PathBuf>,
}

pub struct UndoEngine<'a> {
    journal: &'a Journal,
    store: &'a Store,
}

impl<'a> UndoEngine<'a> {
    pub fn new(journal: &'a Journal, store: &'a Store) -> Self {
        Self { journal, store }
    }

    /// Revert `target`'s effect by restoring its PRE manifests.
    pub fn undo(&self, sel: Selector, force: bool, dry_run: bool) -> Result<UndoReport, UndoError> {
        let target = self.select_undo_target(sel)?;
        let pre = self
            .journal
            .manifests_by_role(target.id, ManifestRole::Pre)?;
        if pre.is_empty() {
            return Err(UndoError::NothingToRestore {
                id: target.id,
                reason: format!("a {} action snapshots nothing", target.effect),
            });
        }
        // Already undone AND the world still matches the pre-state: there is
        // genuinely nothing to do. Say that plainly. Falling through to the
        // conflict oracle here would compare the world against POST, correctly
        // find it different, and exit 3 with "changed since the action" — true,
        // useless, and alarming.
        if target.status == ActionStatus::Undone && self.world_matches(&pre) {
            return Ok(UndoReport {
                target_action: target.id,
                recorded_as: None,
                paths_restored: 0,
                forced: false,
                dry_run,
                already_satisfied: true,
                plan: Vec::new(),
                warnings: Vec::new(),
                restored_paths: Vec::new(),
            });
        }
        let post = self
            .journal
            .manifests_by_role(target.id, ManifestRole::Post)?;
        self.execute(&target, &pre, &post, force, dry_run)
    }

    /// Re-apply an undone action's effect by restoring its POST manifests.
    /// `sel` addresses the UNDO action to revert (Latest = most recent one).
    pub fn redo(&self, sel: Selector, force: bool, dry_run: bool) -> Result<UndoReport, UndoError> {
        let undo_action = match sel {
            Selector::Latest => self
                .journal
                .latest_redoable()?
                .ok_or(UndoError::NoUndoableAction)?,
            Selector::Action(id) => self.journal.action(id)?,
        };
        if undo_action.kind != ActionKind::Undo {
            return Err(UndoError::NotUndoable {
                id: undo_action.id,
                verb: "redone",
                reason: "not an undo action (redo reverts an undo)".into(),
            });
        }
        if undo_action.status != ActionStatus::Completed {
            return Err(UndoError::NotUndoable {
                id: undo_action.id,
                verb: "redone",
                reason: format!("status is {:?}", undo_action.status),
            });
        }
        let original_id = undo_action
            .target_action_id
            .ok_or_else(|| UndoError::NotUndoable {
                id: undo_action.id,
                verb: "redone",
                reason: "undo action has no target".into(),
            })?;
        // redo restores the original's POST state
        let post = self
            .journal
            .manifests_by_role(original_id, ManifestRole::Post)?;
        if post.is_empty() {
            let original = self.journal.action(original_id)?;
            return Err(UndoError::NothingToRestore {
                id: original_id,
                reason: if original.background {
                    // trial 2026-08-15: background commands NEVER get a post-
                    // state (the harness reports completion at launch) — this
                    // is by design, not a failure, and it makes a forced undo
                    // of one a one-way door. Say so precisely.
                    "background command: no post-state exists to re-apply (the \
                     harness reports completion at launch); a forced undo of a \
                     background command cannot be redone"
                        .into()
                } else {
                    "no post-state was recorded (the command may have failed)".into()
                },
            });
        }
        // conflict oracle after an undo: the world should equal the original's
        // PRE state (that is what the undo restored)
        let expect_now = self
            .journal
            .manifests_by_role(original_id, ManifestRole::Pre)?;
        self.execute(&undo_action, &post, &expect_now, force, dry_run)
    }

    fn select_undo_target(&self, sel: Selector) -> Result<ActionRecord, UndoError> {
        let target = match sel {
            Selector::Latest => self.pick_latest_undoable()?,
            Selector::Action(id) => self.journal.action(id)?,
        };
        if target.kind == ActionKind::Undo {
            // Never leave the user in a loop. The trial hit `undo N` → "use
            // redo" → `redo N` → "nothing to do", with no third move. Whatever
            // we refuse, we name an id that WORKS — which means walking to the
            // ultimate command action, not the immediate target: for a redo row
            // (an undo of an undo) the immediate target is itself an undo row,
            // and pointing the user there just chains a second refusal
            // (adversarial review, finding 12).
            let reason = match self.original_command_of(&target) {
                Some(cmd) => format!(
                    "it is one of doover's own undo/redo records. To restore the state \
                     before command #{cmd}, run `doover undo {cmd}`; `doover log` shows it",
                ),
                None => "it is an undo action; use `doover redo` to revert it".into(),
            };
            return Err(UndoError::NotUndoable {
                id: target.id,
                verb: "undone",
                reason,
            });
        }
        match target.status {
            // `Undone` is deliberately NOT refused here (user-#1 trial).
            //
            // A --force undo of a LATER action can restore a world state that
            // re-applies THIS action's effect, leaving the row reading `Undone`
            // while the files are gone again. Refusing on the bookkeeping told
            // the user their data was unrecoverable while the pre-snapshot sat
            // intact in the store — the product promise inverting. The status
            // field is a record of what doover did; only the filesystem knows
            // what is true now. Defer to the conflict oracle in `execute`,
            // which already handles every case correctly: world == POST means
            // the effect is back in force (restore it), world == PRE means it
            // is genuinely already undone (a no-op, handled in `undo`), and
            // anything else is a real conflict.
            ActionStatus::Completed | ActionStatus::Abandoned | ActionStatus::Undone => Ok(target),
            other => Err(UndoError::NotUndoable {
                id: target.id,
                verb: "undone",
                reason: format!("status is {other:?}"),
            }),
        }
    }

    /// Walk an undo/redo record's `target_action_id` chain to the underlying
    /// command action — the id a user can actually `doover undo`. Bounded so a
    /// corrupt cycle can never spin.
    fn original_command_of(&self, undo_row: &ActionRecord) -> Option<ActionId> {
        let mut cur = undo_row.target_action_id?;
        for _ in 0..64 {
            let rec = self.journal.action(cur).ok()?;
            if rec.kind != ActionKind::Undo {
                return Some(rec.id);
            }
            cur = rec.target_action_id?;
        }
        None
    }

    /// The default target for a bare `doover undo`.
    ///
    /// Walks the journal newest-first and returns the first action that both
    /// *changed something* and *still has something to restore*. Both questions
    /// are answered against reality (manifests and the live filesystem), never
    /// against the status column.
    ///
    /// The trial's bare `doover undo` landed on a read-only command that had
    /// been given a defensive working-directory snapshot. Undoing that is
    /// meaningless at best — and at worst it reverts whatever legitimately
    /// happened afterwards, while reporting success.
    fn pick_latest_undoable(&self) -> Result<ActionRecord, UndoError> {
        for cand in self.journal.undo_candidates(UNDO_CANDIDATE_SCAN)? {
            let pre = self.journal.manifests_by_role(cand.id, ManifestRole::Pre)?;
            if pre.is_empty() {
                continue;
            }
            // A truncated manifest with NO file entries is a pure directory
            // skeleton — a snapshot whose every file ingest failed (store
            // unwritable). It holds nothing restorable, yet truncated→InForce
            // would offer it to bare undo forever, ahead of real candidates,
            // and restoring it deletes files (review 2026-08-15). Skip it in
            // SELECTION only; explicit `undo <id>` still reaches it under the
            // round-18 refuse-by-default partial gate.
            let skeleton = pre.iter().all(|m| {
                m.truncated
                    && !m
                        .entries
                        .iter()
                        .any(|e| matches!(e.kind, crate::snapshot::EntryKind::File { .. }))
            });
            if skeleton {
                continue;
            }
            // (a) Did this command change anything? A read-only command that got
            //     a defensive cwd snapshot has POST identical to PRE. Skip it:
            //     "undoing" it would revert later work, not this command.
            let post = self
                .journal
                .manifests_by_role(cand.id, ManifestRole::Post)?;
            if !post.is_empty() && same_state(&pre, &post) {
                continue;
            }
            // (b) Is there still something to restore? Answered against the live
            //     filesystem, mode included.
            match self.restorability(&pre) {
                // effect is in force (world differs from PRE): this is the
                // target. Also what lets a KILLED destructive command — no
                // post-state at all — still be found.
                Restorability::InForce => return Ok(cand),
                // already reverted (world == PRE): keep looking for one that
                // is still in force.
                Restorability::AlreadyDone => continue,
                // could not evaluate this candidate — a file in its snapshot is
                // unreadable. Skip it and keep scanning rather than abort the
                // whole command. Before the adversarial review (finding 8) a
                // single unreadable file made bare `doover undo` fail with a
                // cryptic IO error while a fully recoverable action sat one
                // candidate deeper. (A TRUNCATED capture is NOT Indeterminate —
                // it resolves InForce so a truncated `rm -rf` stays findable.)
                Restorability::Indeterminate => continue,
            }
        }
        Err(UndoError::NoUndoableAction)
    }

    /// Whether restoring `ms` would still change the live filesystem.
    ///
    /// `InForce` means the world differs from the snapshot, so there is
    /// something to restore. This is deliberately metadata-aware
    /// ([`Store::state_matches_mode`]): a `chmod -R 777` leaves file CONTENT
    /// untouched, so a content-only check reported "nothing to restore" and hid
    /// the one destructive action doover had snapshotted (finding 9).
    ///
    /// A read error is `Indeterminate` (skip this candidate — finding 8). A
    /// truncated capture is never `AlreadyDone`: it describes only part of a
    /// tree, so its captured entries all matching does NOT prove the uncaptured
    /// part is restored. It resolves to `InForce` so it is OFFERED to bare undo
    /// — which is what keeps a truncated flagship `rm -rf` (a huge deletion that
    /// hit the file/time limit) findable; `undo()` then applies the round-18
    /// refuse-by-default on the truncated restore-set. An earlier version
    /// short-circuited truncated to `Indeterminate` and SKIPPED it, so bare
    /// `doover undo` reported "nothing to undo" for exactly the large deletions
    /// the tool exists to recover (round-3 audit regression).
    fn restorability(&self, ms: &[Manifest]) -> Restorability {
        let mut all_match = true;
        let mut any_truncated = false;
        for m in ms {
            if m.truncated {
                any_truncated = true;
            }
            match self.store.state_matches_mode(m) {
                Ok(true) => {}
                Ok(false) => all_match = false,
                Err(_) => return Restorability::Indeterminate,
            }
        }
        if !all_match || any_truncated {
            Restorability::InForce
        } else {
            Restorability::AlreadyDone
        }
    }

    /// Does the live filesystem already look exactly like these manifests
    /// (content and mode)? Used for the already-undone no-op check. Any read
    /// error or truncated capture answers `false` — we only claim "already
    /// satisfied" when we can prove it.
    fn world_matches(&self, ms: &[Manifest]) -> bool {
        matches!(self.restorability(ms), Restorability::AlreadyDone)
    }

    /// Shared tail: conflict-check `restore_set` against `oracle`, then (unless
    /// dry-run) capture rollback state, restore all-or-nothing, and only record
    /// the undo once every path is restored.
    fn execute(
        &self,
        journal_target: &ActionRecord,
        restore_set: &[Manifest],
        oracle: &[Manifest],
        force: bool,
        dry_run: bool,
    ) -> Result<UndoReport, UndoError> {
        // Cross-process exclusion is taken BEFORE the conflict oracle runs
        // (round-2 review RL-1): a verdict computed outside the lock can be
        // stale by mutation time — a racer's finished restore would be
        // silently overwritten where sequential execution would have refused
        // with a conflict. Dry-run never mutates and takes no lock.
        let _restore_lock = if dry_run {
            None
        } else {
            Some(self.store.lock_restores()?)
        };
        let mut warnings = Vec::new();
        let mut conflicts = Vec::new();
        for m in restore_set {
            // A TRUNCATED capture is a partial tree. Restore is stage-then-
            // swap: swapping the partial in DELETES every live file the
            // snapshot skipped — undo destroying exactly what it failed to
            // capture (round 18). Refuse by default; --force proceeds with a
            // loud warning for the "partial restore beats nothing" cases
            // (e.g. recovering some of an rm -rf'd tree).
            if m.truncated {
                let msg = format!(
                    "{}: the recorded capture was TRUNCATED (partial); restoring would \
                     replace the tree with the partial capture, deleting anything it \
                     missed",
                    m.path.display()
                );
                if force {
                    warnings.push(msg);
                } else {
                    conflicts.push(msg);
                }
            }
            match oracle.iter().find(|o| o.path == m.path) {
                Some(o) => {
                    if o.truncated {
                        warnings.push(format!(
                            "{}: recorded state was truncated; conflict check is partial",
                            m.path.display()
                        ));
                    }
                    if !self.store.state_matches(o)? {
                        conflicts.push(format!("{} changed since the action", m.path.display()));
                    }
                }
                // no recorded state to verify against (audit round 10): a
                // failed/abandoned command has no post-snapshot, so we CANNOT
                // confirm the world is unchanged. Refusing-by-default is the
                // safe choice — undoing might clobber the user's own work.
                // --force proceeds. Background commands land here BY DESIGN
                // (their post-state is unverifiable — trial 2026-08-15), so
                // diagnose them precisely instead of "may have failed".
                None if journal_target.background => conflicts.push(format!(
                    "{}: cannot verify it is unchanged — background commands \
                     report completion at launch, so no post-state exists. \
                     Forcing reverts EVERYTHING under this path to its \
                     pre-command state, including changes made afterward, and \
                     cannot be redone",
                    m.path.display()
                )),
                None => conflicts.push(format!(
                    "{}: cannot verify it is unchanged (no post-state was recorded); \
                     the command may have failed",
                    m.path.display()
                )),
            }
        }
        if !conflicts.is_empty() && !force {
            return Err(UndoError::Conflicts(conflicts));
        }
        if force && journal_target.background {
            warnings.push(
                "forced undo of a background command cannot be redone: no post-state \
                 exists to re-apply"
                    .to_string(),
            );
        }

        let plan: Vec<String> = restore_set
            .iter()
            .map(|m| {
                if m.root == crate::snapshot::Root::Absent {
                    format!("delete {} (did not exist before)", m.path.display())
                } else {
                    format!("restore {} ({} entries)", m.path.display(), m.entries.len())
                }
            })
            .collect();

        // A defensive snapshot covers the whole working directory, because
        // doover could not tell what the command would touch. Restoring it
        // therefore reverts EVERY file in that directory, not just the ones the
        // command actually changed. The conflict oracle catches the dangerous
        // case, but its advice is `--force` — so the scope has to be spelled
        // out here, before the user reaches for it (user-#1 trial).
        if journal_target.has_unknown {
            for m in restore_set.iter().filter(|m| m.root != Root::Absent) {
                warnings.push(format!(
                    "{}: doover could not tell what this command would touch, so it \
                     snapshotted the entire directory. Undoing it reverts every file in \
                     {} ({} entries) to its state before the command — including files \
                     the command never touched.",
                    m.path.display(),
                    m.path.display(),
                    m.entries.len()
                ));
            }
        }

        if dry_run {
            return Ok(UndoReport {
                target_action: journal_target.id,
                recorded_as: None,
                paths_restored: 0,
                forced: !conflicts.is_empty(),
                dry_run: true,
                already_satisfied: false,
                plan,
                warnings,
                restored_paths: Vec::new(),
            });
        }

        // All-or-nothing, restore-BEFORE-record (audit round 10): if any path
        // fails, roll the succeeded ones back to their pre-undo state and
        // return an error WITHOUT recording — the target stays in its current
        // status so `doover undo` can simply be retried. The journal never
        // claims an undo that only partly happened.
        //
        // Capture each path's current state first (in memory) as the rollback
        // point. A path we cannot even snapshot is a path we cannot safely
        // restore transactionally, so refuse before touching anything.
        let mut rollback: Vec<Manifest> = Vec::with_capacity(restore_set.len());
        for m in restore_set {
            // snapshot_rollback, not snapshot: with DOOVER_HOME nested inside
            // the target, a plain capture re-ingested the live journal/store
            // (unbounded, secret-bearing) into the rollback manifest — round
            // 21's exclusion applied only to the hook path (dive 2026-08-15)
            match self.store.snapshot_rollback(&m.path) {
                // complete-or-refused: a TRUNCATED rollback point (per-file
                // capture errors now degrade instead of aborting) cannot make
                // the undo transactional — restoring it on the failure arm
                // would delete whatever it failed to capture (review
                // 2026-08-15). Refuse up front, before touching anything.
                Ok(current) if current.truncated => {
                    return Err(UndoError::NotUndoable {
                        id: journal_target.id,
                        verb: "restored",
                        reason: format!(
                            "cannot capture a complete rollback point for {} ({}); \
                             a partial rollback could destroy data on failure — fix \
                             the unreadable paths and retry",
                            m.path.display(),
                            current.warnings.join("; ")
                        ),
                    });
                }
                Ok(current) => rollback.push(current),
                Err(e) => {
                    return Err(UndoError::Snapshot(e));
                }
            }
        }

        let mut restored = 0usize;
        for (i, m) in restore_set.iter().enumerate() {
            match self.store.restore(m) {
                Ok(report) => {
                    restored += 1;
                    warnings.extend(report.warnings);
                }
                Err(e) => {
                    // roll back everything already restored (best effort) so
                    // the world returns to its pre-undo state
                    let mut rollback_failures = Vec::new();
                    for done in rollback.iter().take(i) {
                        if let Err(re) = self.store.restore(done) {
                            // a ROLLBACK restore can itself strand live data
                            // in staging; that path must be journaled no
                            // matter which error started the failure chain
                            // (review 2026-08-15)
                            if let SnapshotError::RestoreStagingPreserved {
                                staging,
                                remaining,
                                ..
                            } = &re
                            {
                                let _ = self.journal.add_note(
                                    journal_target.id,
                                    &format!(
                                        "ROLLBACK FAILURE left live data in {} ({} stranded \
                                         entrie(s)); move it back manually — do not delete",
                                        staging.display(),
                                        remaining.len()
                                    ),
                                );
                            }
                            rollback_failures.push(format!("{}: {re}", done.path.display()));
                        }
                    }
                    // A restore that failed after DISTURBING the target, or
                    // that left a staging dir holding live data, must never
                    // be reported as "nothing changed, safe to retry" (dive
                    // 2026-08-15). Surface those verbatim — their messages
                    // carry the recovery instructions — and journal the
                    // preserved-staging path durably: one stderr line is not
                    // a record the user can find tomorrow.
                    match &e {
                        SnapshotError::RestoreStagingPreserved {
                            staging, remaining, ..
                        } => {
                            let _ = self.journal.add_note(
                                journal_target.id,
                                &format!(
                                    "RESTORE FAILURE left live data in {} ({} stranded \
                                     entrie(s)); move it back manually — do not delete",
                                    staging.display(),
                                    remaining.len()
                                ),
                            );
                            for f in &rollback_failures {
                                let _ = self.journal.add_note(
                                    journal_target.id,
                                    &format!("rollback failure during failed undo: {f}"),
                                );
                            }
                            return Err(UndoError::Snapshot(e));
                        }
                        SnapshotError::RestoreTargetDisturbed { .. } => {
                            for f in &rollback_failures {
                                let _ = self.journal.add_note(
                                    journal_target.id,
                                    &format!("rollback failure during failed undo: {f}"),
                                );
                            }
                            return Err(UndoError::Snapshot(e));
                        }
                        _ => {}
                    }
                    if rollback_failures.is_empty() {
                        return Err(UndoError::PartialRolledBack {
                            path: m.path.display().to_string(),
                            cause: e.to_string(),
                        });
                    }
                    return Err(UndoError::PartialInconsistent {
                        path: m.path.display().to_string(),
                        cause: e.to_string(),
                        rollback_failures,
                    });
                }
            }
        }

        // every path restored: NOW record the undo (flips status). record_undo
        // is the double-undo guard; if a concurrent undo already recorded, this
        // errors but the world is already correctly restored (idempotent).
        let recorded = self
            .journal
            // Only the engine may opt into re-undoing an already-undone action,
            // and only here: reaching this point with status == Undone means the
            // no-op check in `undo` already proved the world no longer matches the
            // pre-state, i.e. the action's effect is genuinely back in force.
            .record_undo(
                &journal_target.session_id,
                journal_target.id,
                journal_target.status == ActionStatus::Undone,
            )?;
        // stash the pre-undo state on the new row for forensics/manual recovery
        for rb in &rollback {
            let _ = self
                .journal
                .attach_manifest(recorded, rb, ManifestRole::Pre);
        }

        Ok(UndoReport {
            target_action: journal_target.id,
            recorded_as: Some(recorded),
            paths_restored: restored,
            forced: !conflicts.is_empty(),
            dry_run: false,
            already_satisfied: false,
            plan,
            warnings,
            restored_paths: restore_set.iter().map(|m| m.path.clone()).collect(),
        })
    }
}
