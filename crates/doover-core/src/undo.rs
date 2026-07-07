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
//!   action (user edits, later agent actions), refuse unless --force;
//! - every undo/redo snapshots the CURRENT state onto its own journal row
//!   before restoring, so even a forced mistake is itself recoverable;
//! - dry-run plans without writing anything, journal included.

use crate::journal::{
    ActionId, ActionKind, ActionRecord, ActionStatus, Journal, JournalError, ManifestRole,
};
use crate::snapshot::{Manifest, SnapshotError, Store};

#[derive(Debug)]
pub enum Selector {
    /// The most recent plausible target (undoable command / live undo).
    Latest,
    /// A specific journal action id.
    Action(ActionId),
}

#[derive(Debug, thiserror::Error)]
pub enum UndoError {
    #[error("nothing to undo: no completed action with a snapshot was found")]
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
    /// Human-readable restore plan, one line per path.
    pub plan: Vec<String>,
    pub warnings: Vec<String>,
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
        let post = self
            .journal
            .manifests_by_role(target.id, ManifestRole::Post)?;
        self.execute(&target, &pre, &post, "undo", force, dry_run)
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
            return Err(UndoError::NothingToRestore {
                id: original_id,
                reason: "no post-state was recorded (the command may have failed)".into(),
            });
        }
        // conflict oracle after an undo: the world should equal the original's
        // PRE state (that is what the undo restored)
        let expect_now = self
            .journal
            .manifests_by_role(original_id, ManifestRole::Pre)?;
        self.execute(&undo_action, &post, &expect_now, "redo", force, dry_run)
    }

    fn select_undo_target(&self, sel: Selector) -> Result<ActionRecord, UndoError> {
        let target = match sel {
            Selector::Latest => self
                .journal
                .latest_undoable()?
                .ok_or(UndoError::NoUndoableAction)?,
            Selector::Action(id) => self.journal.action(id)?,
        };
        if target.kind == ActionKind::Undo {
            return Err(UndoError::NotUndoable {
                id: target.id,
                verb: "undone",
                reason: "it is an undo action; use redo to revert it".into(),
            });
        }
        match target.status {
            ActionStatus::Completed | ActionStatus::Abandoned => Ok(target),
            other => Err(UndoError::NotUndoable {
                id: target.id,
                verb: "undone",
                reason: format!("status is {other:?}"),
            }),
        }
    }

    /// Shared tail: conflict-check against `oracle`, plan, record, snapshot
    /// current state onto the new journal row, restore `restore_set`.
    fn execute(
        &self,
        journal_target: &ActionRecord,
        restore_set: &[Manifest],
        oracle: &[Manifest],
        verb: &'static str,
        force: bool,
        dry_run: bool,
    ) -> Result<UndoReport, UndoError> {
        let mut warnings = Vec::new();
        let mut conflicts = Vec::new();
        for m in restore_set {
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
                None => warnings.push(format!(
                    "{}: no recorded state to verify against",
                    m.path.display()
                )),
            }
        }
        if !conflicts.is_empty() && !force {
            return Err(UndoError::Conflicts(conflicts));
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

        if dry_run {
            return Ok(UndoReport {
                target_action: journal_target.id,
                recorded_as: None,
                paths_restored: 0,
                forced: !conflicts.is_empty(),
                dry_run: true,
                plan,
                warnings,
            });
        }

        // record first: the journal transaction is the double-undo guard
        let recorded = self
            .journal
            .record_undo(&journal_target.session_id, journal_target.id)?;

        // capture the CURRENT state onto the new row so even a forced mistake
        // is recoverable; failures degrade to warnings
        for m in restore_set {
            match self.store.snapshot(&m.path, None) {
                Ok(current) => {
                    self.journal
                        .attach_manifest(recorded, &current, ManifestRole::Pre)?;
                }
                Err(e) => warnings.push(format!(
                    "could not snapshot current state of {}: {e}",
                    m.path.display()
                )),
            }
        }

        let mut restored = 0usize;
        for m in restore_set {
            match self.store.restore(m) {
                Ok(report) => {
                    restored += 1;
                    warnings.extend(report.warnings);
                }
                Err(e) => {
                    let msg = format!("{verb} of {} failed: {e}", m.path.display());
                    self.journal.add_note(recorded, &msg)?;
                    warnings.push(msg);
                    return Err(e.into());
                }
            }
        }

        Ok(UndoReport {
            target_action: journal_target.id,
            recorded_as: Some(recorded),
            paths_restored: restored,
            forced: !conflicts.is_empty(),
            dry_run: false,
            plan,
            warnings,
        })
    }
}
