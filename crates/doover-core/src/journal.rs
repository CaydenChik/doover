//! Append-only action journal (SQLite, WAL).
//!
//! Semantics grounded in the captured hook contract (fixtures README):
//! - success is implied by a PostToolUse arriving; there is no exit code —
//!   a `pending` action with no post event is closed as `abandoned` when the
//!   next action starts in its session, or at session end;
//! - `tool_use_id` correlates pre/post pairs;
//! - undo never deletes rows: it is itself a journaled action referencing its
//!   target, so undoing an undo is redo, and history stays append-only
//!   (YoloFS-style travel, not destructive rollback).
//!
//! Concurrency: hook invocations are separate short-lived processes. WAL mode
//! plus BEGIN IMMEDIATE transactions and a busy timeout give unique,
//! contiguous per-session sequence numbers across racing writers.

use crate::snapshot::{EntryKind, Manifest};
use std::collections::BTreeSet;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const SCHEMA_VERSION: i64 = 2;
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Which side of an action a manifest captures: state before the command
/// (restored by undo) or after (restored by redo, and the conflict oracle).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManifestRole {
    Pre,
    Post,
}

impl ManifestRole {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pre => "pre",
            Self::Post => "post",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum JournalError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("manifest serialization: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("action {0} not found")]
    ActionNotFound(i64),
    #[error("no completable action with tool_use_id {tool_use_id} in session {session_id}")]
    NoPendingForToolUse {
        session_id: String,
        tool_use_id: String,
    },
    #[error("cannot undo action {id}: status is {status:?} (must be completed or abandoned)")]
    NotUndoable { id: i64, status: ActionStatus },
    #[error(
        "cannot undo action {id}: it reverses another undo; run undo on the original action {original} instead"
    )]
    UndoTooDeep { id: i64, original: i64 },
    #[error(
        "manifest was written by a newer doover (schema {found}, this build understands {})",
        crate::snapshot::MANIFEST_SCHEMA
    )]
    ManifestTooNew { found: u32 },
    #[error(
        "journal schema version {found} is newer than this doover understands ({SCHEMA_VERSION})"
    )]
    SchemaTooNew { found: i64 },
}

pub type ActionId = i64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionKind {
    Command,
    Undo,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionStatus {
    Pending,
    Completed,
    Abandoned,
    Undone,
}

impl ActionKind {
    fn parse(s: &str) -> Self {
        match s {
            "undo" => Self::Undo,
            _ => Self::Command,
        }
    }
}

impl ActionStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Completed => "completed",
            Self::Abandoned => "abandoned",
            Self::Undone => "undone",
        }
    }
    fn parse(s: &str) -> Self {
        match s {
            "completed" => Self::Completed,
            "abandoned" => Self::Abandoned,
            "undone" => Self::Undone,
            _ => Self::Pending,
        }
    }
}

pub struct NewAction<'a> {
    pub session_id: &'a str,
    pub tool_use_id: Option<&'a str>,
    pub raw_command: &'a str,
    /// Severity string from the resolver ("safe" … "irreversible", "unknown").
    pub effect: &'a str,
    pub rule_id: Option<&'a str>,
    pub has_unknown: bool,
}

#[derive(Debug, Clone)]
pub struct ActionRecord {
    pub id: ActionId,
    pub session_id: String,
    pub seq: i64,
    pub kind: ActionKind,
    pub tool_use_id: Option<String>,
    pub raw_command: String,
    pub effect: String,
    pub rule_id: Option<String>,
    pub has_unknown: bool,
    pub status: ActionStatus,
    pub target_action_id: Option<ActionId>,
    /// For undo actions: the target's status before it was flipped to
    /// `undone` — what redo must restore (never a fabricated `completed`).
    pub target_prior_status: Option<ActionStatus>,
    pub pinned: bool,
    pub started_at_ms: i64,
    pub duration_ms: Option<i64>,
    pub note: Option<String>,
}

/// True for SQLITE_BUSY / SQLITE_LOCKED — the transient contention class the
/// open() retry loop absorbs.
fn is_busy(e: &JournalError) -> bool {
    matches!(
        e,
        JournalError::Sqlite(rusqlite::Error::SqliteFailure(f, _))
            if matches!(
                f.code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
            )
    )
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub struct Journal {
    conn: rusqlite::Connection,
}

impl Journal {
    pub fn open(path: &Path) -> Result<Self, JournalError> {
        // Bounded retry on BUSY/LOCKED: the busy handler cannot wait in every
        // path (SQLite returns immediately where waiting could deadlock, e.g.
        // around the fresh-file WAL switch under concurrent first opens —
        // round 20). Total worst-case wait ~1s, far under the hook timeout,
        // and hook callers fail open anyway.
        let mut last = None;
        for _ in 0..40 {
            match Self::open_attempt(path) {
                Err(e) if is_busy(&e) => {
                    last = Some(e);
                    std::thread::sleep(Duration::from_millis(25));
                }
                other => return other,
            }
        }
        Err(last.expect("loop ran at least once"))
    }

    fn open_attempt(path: &Path) -> Result<Self, JournalError> {
        let conn = rusqlite::Connection::open(path)?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        conn.pragma_update(None, "journal_mode", "wal")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;

        // Double-checked migration under an EXCLUSIVE transaction (round 20,
        // found by the S9 concurrency stress test): concurrent FIRST opens of
        // a fresh journal each read user_version before anyone migrated, and
        // the losers re-ran `ALTER TABLE` into "duplicate column name" — every
        // hook failed open, so a brand-new install with parallel agents had
        // ZERO protection. BEGIN EXCLUSIVE serializes racers (busy_timeout
        // queues them) and the version is re-read INSIDE the lock, so late
        // arrivals see the migrated version and skip.
        conn.execute_batch("BEGIN EXCLUSIVE;")?;
        let migrate = (|| -> Result<(), JournalError> {
            let mut version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
            if version > SCHEMA_VERSION {
                return Err(JournalError::SchemaTooNew { found: version });
            }
            // stepwise migrations: each block moves exactly one version
            // forward, so any past journal upgrades along the same audited path
            if version == 0 {
                conn.execute_batch(
                    "CREATE TABLE IF NOT EXISTS sessions(
                    id TEXT PRIMARY KEY,
                    harness TEXT NOT NULL,
                    cwd TEXT NOT NULL,
                    started_at_ms INTEGER NOT NULL,
                    ended_at_ms INTEGER
                 );
                 CREATE TABLE IF NOT EXISTS actions(
                    id INTEGER PRIMARY KEY,
                    session_id TEXT NOT NULL REFERENCES sessions(id),
                    seq INTEGER NOT NULL,
                    kind TEXT NOT NULL CHECK(kind IN ('command','undo')),
                    tool_use_id TEXT,
                    raw_command TEXT NOT NULL,
                    effect TEXT NOT NULL,
                    rule_id TEXT,
                    has_unknown INTEGER NOT NULL DEFAULT 0,
                    status TEXT NOT NULL
                        CHECK(status IN ('pending','completed','abandoned','undone')),
                    target_action_id INTEGER REFERENCES actions(id),
                    target_prior_status TEXT,
                    pinned INTEGER NOT NULL DEFAULT 0,
                    started_at_ms INTEGER NOT NULL,
                    duration_ms INTEGER,
                    note TEXT,
                    UNIQUE(session_id, seq)
                 );
                 CREATE INDEX IF NOT EXISTS idx_actions_session ON actions(session_id);
                 CREATE INDEX IF NOT EXISTS idx_actions_tool_use ON actions(session_id, tool_use_id);
                 CREATE TABLE IF NOT EXISTS manifests(
                    id INTEGER PRIMARY KEY,
                    action_id INTEGER NOT NULL REFERENCES actions(id),
                    path TEXT NOT NULL,
                    manifest_json TEXT NOT NULL,
                    hashes TEXT NOT NULL,
                    truncated INTEGER NOT NULL
                 );
                 CREATE INDEX IF NOT EXISTS idx_manifests_action ON manifests(action_id);
                 PRAGMA user_version = 1;",
                )?;
                version = 1;
            }
            if version == 1 {
                // v2: manifests gain a role — pre (undo restores it) or post
                // (redo restores it; conflict oracle). Existing rows are all pre.
                conn.execute_batch(
                    "ALTER TABLE manifests ADD COLUMN role TEXT NOT NULL DEFAULT 'pre'
                        CHECK(role IN ('pre','post'));
                     PRAGMA user_version = 2;",
                )?;
            }
            Ok(())
        })();
        match &migrate {
            Ok(()) => conn.execute_batch("COMMIT;")?,
            Err(_) => conn.execute_batch("ROLLBACK;").unwrap_or(()),
        }
        migrate?;
        Ok(Self { conn })
    }

    pub fn begin_session(&self, id: &str, harness: &str, cwd: &str) -> Result<(), JournalError> {
        self.conn.execute(
            "INSERT INTO sessions(id, harness, cwd, started_at_ms) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(id) DO UPDATE SET cwd = excluded.cwd",
            rusqlite::params![id, harness, cwd, now_ms()],
        )?;
        Ok(())
    }

    /// Record a new action as `pending`. Any older still-pending action in the
    /// same session is closed as `abandoned` — its post event will never come
    /// (captured contract: failures emit no PostToolUse).
    pub fn start_action(&self, new: &NewAction) -> Result<ActionId, JournalError> {
        self.conn.execute_batch("BEGIN IMMEDIATE;")?;
        let result = (|| -> Result<ActionId, JournalError> {
            self.conn.execute(
                "UPDATE actions SET status = 'abandoned',
                        note = COALESCE(note, 'no post event received')
                 WHERE session_id = ?1 AND status = 'pending'",
                [new.session_id],
            )?;
            let seq: i64 = self.conn.query_row(
                "SELECT COALESCE(MAX(seq), 0) + 1 FROM actions WHERE session_id = ?1",
                [new.session_id],
                |r| r.get(0),
            )?;
            self.conn.execute(
                "INSERT INTO actions(session_id, seq, kind, tool_use_id, raw_command,
                                     effect, rule_id, has_unknown, status, started_at_ms)
                 VALUES (?1, ?2, 'command', ?3, ?4, ?5, ?6, ?7, 'pending', ?8)",
                rusqlite::params![
                    new.session_id,
                    seq,
                    new.tool_use_id,
                    new.raw_command,
                    new.effect,
                    new.rule_id,
                    new.has_unknown,
                    now_ms(),
                ],
            )?;
            Ok(self.conn.last_insert_rowid())
        })();
        match &result {
            Ok(_) => self.conn.execute_batch("COMMIT;")?,
            Err(_) => self.conn.execute_batch("ROLLBACK;").unwrap_or(()),
        }
        result
    }

    /// Close a pending action via its pre/post correlation key. Also accepts
    /// an `abandoned` action: with interleaved or background tool calls a post
    /// can arrive after the next action's start already abandoned its pre —
    /// the late post is ground truth and wins over our guess.
    pub fn complete_by_tool_use(
        &self,
        session_id: &str,
        tool_use_id: &str,
        duration_ms: i64,
    ) -> Result<ActionId, JournalError> {
        let updated: Option<i64> = self
            .conn
            .query_row(
                // newest matching row only: a duplicated correlation id (e.g.
                // session replay) must never complete multiple actions
                "UPDATE actions SET status = 'completed', duration_ms = ?3
                 WHERE id = (SELECT id FROM actions
                             WHERE session_id = ?1 AND tool_use_id = ?2
                               AND status IN ('pending', 'abandoned')
                             ORDER BY seq DESC LIMIT 1)
                 RETURNING id",
                rusqlite::params![session_id, tool_use_id, duration_ms],
                |r| r.get(0),
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other),
            })?;
        updated.ok_or_else(|| JournalError::NoPendingForToolUse {
            session_id: session_id.to_string(),
            tool_use_id: tool_use_id.to_string(),
        })
    }

    pub fn end_session(&self, id: &str) -> Result<(), JournalError> {
        self.conn.execute(
            "UPDATE actions SET status = 'abandoned',
                    note = COALESCE(note, 'session ended without post event')
             WHERE session_id = ?1 AND status = 'pending'",
            [id],
        )?;
        self.conn.execute(
            "UPDATE sessions SET ended_at_ms = ?2 WHERE id = ?1",
            rusqlite::params![id, now_ms()],
        )?;
        Ok(())
    }

    pub fn attach_manifest(
        &self,
        action: ActionId,
        manifest: &Manifest,
        role: ManifestRole,
    ) -> Result<(), JournalError> {
        let json = serde_json::to_string(manifest)?;
        let hashes: Vec<&str> = manifest
            .entries
            .iter()
            .filter_map(|e| match &e.kind {
                EntryKind::File { hash, .. } => Some(hash.as_str()),
                _ => None,
            })
            .collect();
        self.conn.execute(
            "INSERT INTO manifests(action_id, path, manifest_json, hashes, truncated, role)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                action,
                manifest.path.to_string_lossy(),
                json,
                serde_json::to_string(&hashes)?,
                manifest.truncated,
                role.as_str(),
            ],
        )?;
        Ok(())
    }

    /// All manifests of an action, regardless of role.
    pub fn manifests(&self, action: ActionId) -> Result<Vec<Manifest>, JournalError> {
        self.manifests_query(
            "SELECT manifest_json FROM manifests WHERE action_id = ?1 ORDER BY id",
            action,
        )
    }

    /// Manifests of an action filtered by role (pre = undo side, post = redo
    /// side / conflict oracle).
    pub fn manifests_by_role(
        &self,
        action: ActionId,
        role: ManifestRole,
    ) -> Result<Vec<Manifest>, JournalError> {
        match role {
            ManifestRole::Pre => self.manifests_query(
                "SELECT manifest_json FROM manifests
                 WHERE action_id = ?1 AND role = 'pre' ORDER BY id",
                action,
            ),
            ManifestRole::Post => self.manifests_query(
                "SELECT manifest_json FROM manifests
                 WHERE action_id = ?1 AND role = 'post' ORDER BY id",
                action,
            ),
        }
    }

    fn manifests_query(&self, sql: &str, action: ActionId) -> Result<Vec<Manifest>, JournalError> {
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map([action], |r| r.get::<_, String>(0))?;
        let mut out: Vec<Manifest> = Vec::new();
        for json in rows {
            let m: Manifest = serde_json::from_str(&json?)?;
            if m.schema > crate::snapshot::MANIFEST_SCHEMA {
                return Err(JournalError::ManifestTooNew { found: m.schema });
            }
            out.push(m);
        }
        Ok(out)
    }

    /// Newest command-kind action that is plausibly undoable: completed or
    /// abandoned, with at least one pre-manifest. Searches across sessions.
    pub fn latest_undoable(&self) -> Result<Option<ActionRecord>, JournalError> {
        let mut stmt = self.conn.prepare(&format!(
            "{SELECT_ACTION} WHERE kind = 'command'
               AND status IN ('completed','abandoned')
               AND EXISTS (SELECT 1 FROM manifests m
                           WHERE m.action_id = actions.id AND m.role = 'pre')
             ORDER BY id DESC LIMIT 1"
        ))?;
        let mut rows = stmt.query_map([], row_to_action)?;
        Ok(rows.next().transpose()?)
    }

    /// Most recent actions across all sessions, newest first (for `log`).
    pub fn recent_actions(&self, limit: i64) -> Result<Vec<ActionRecord>, JournalError> {
        let mut stmt = self
            .conn
            .prepare(&format!("{SELECT_ACTION} ORDER BY id DESC LIMIT ?1"))?;
        let rows = stmt.query_map([limit], row_to_action)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Newest live undo action (redo target): kind undo, still completed.
    pub fn latest_redoable(&self) -> Result<Option<ActionRecord>, JournalError> {
        let mut stmt = self.conn.prepare(&format!(
            "{SELECT_ACTION} WHERE kind = 'undo' AND status = 'completed'
             ORDER BY id DESC LIMIT 1"
        ))?;
        let mut rows = stmt.query_map([], row_to_action)?;
        Ok(rows.next().transpose()?)
    }

    /// Record an undo of `target` as a new journaled action. The target flips
    /// to `undone`, remembering its prior status on the undo row; undoing an
    /// *undo* restores that recorded status to the original — redo without
    /// fabricating history. The status check runs INSIDE the transaction, so
    /// racing double-undos admit exactly one winner, and an already-undone
    /// target is refused (redo targets the undo action, not the original).
    ///
    /// Chains are bounded by design: undoable targets are command actions and
    /// first-level undos (redo). Undoing a *redo* is refused with a pointer to
    /// the original — the same capability with a trivially-consistent state
    /// machine instead of recursive status cascades (audit round 4). The
    /// exhaustive small-model test in T4 checks every sequence to depth 4
    /// against a reference implementation of these rules.
    pub fn record_undo(
        &self,
        session_id: &str,
        target: ActionId,
    ) -> Result<ActionId, JournalError> {
        self.conn.execute_batch("BEGIN IMMEDIATE;")?;
        let result = (|| -> Result<ActionId, JournalError> {
            let target_rec = self.action(target)?;
            if matches!(
                target_rec.status,
                ActionStatus::Pending | ActionStatus::Undone
            ) {
                return Err(JournalError::NotUndoable {
                    id: target,
                    status: target_rec.status,
                });
            }
            if target_rec.kind == ActionKind::Undo {
                let inner = self.action(
                    target_rec
                        .target_action_id
                        .ok_or(JournalError::ActionNotFound(target))?,
                )?;
                if inner.kind == ActionKind::Undo {
                    // walk to the ultimate command action for the error message
                    let mut original = inner;
                    let mut hops = 0;
                    while original.kind == ActionKind::Undo && hops < 64 {
                        match original.target_action_id {
                            Some(t) => original = self.action(t)?,
                            None => break,
                        }
                        hops += 1;
                    }
                    return Err(JournalError::UndoTooDeep {
                        id: target,
                        original: original.id,
                    });
                }
            }
            let seq: i64 = self.conn.query_row(
                "SELECT COALESCE(MAX(seq), 0) + 1 FROM actions WHERE session_id = ?1",
                [session_id],
                |r| r.get(0),
            )?;
            self.conn.execute(
                "INSERT INTO actions(session_id, seq, kind, raw_command, effect,
                                     status, target_action_id, target_prior_status, started_at_ms)
                 VALUES (?1, ?2, 'undo', ?3, 'destructive', 'completed', ?4, ?5, ?6)",
                rusqlite::params![
                    session_id,
                    seq,
                    format!("undo of action {target}"),
                    target,
                    target_rec.status.as_str(),
                    now_ms(),
                ],
            )?;
            let undo_id = self.conn.last_insert_rowid();
            self.conn.execute(
                "UPDATE actions SET status = 'undone' WHERE id = ?1",
                [target],
            )?;
            // redo semantics: undoing an undo restores ITS target to the
            // status the undo recorded — completed stays completed, abandoned
            // stays abandoned
            if target_rec.kind == ActionKind::Undo {
                if let (Some(original), Some(prior)) =
                    (target_rec.target_action_id, target_rec.target_prior_status)
                {
                    self.conn.execute(
                        "UPDATE actions SET status = ?2 WHERE id = ?1",
                        rusqlite::params![original, prior.as_str()],
                    )?;
                }
            }
            Ok(undo_id)
        })();
        match &result {
            Ok(_) => self.conn.execute_batch("COMMIT;")?,
            Err(_) => self.conn.execute_batch("ROLLBACK;").unwrap_or(()),
        }
        result
    }

    /// Append a note line to an action (used for loud protection-gap records:
    /// truncated snapshots, per-path failures).
    pub fn add_note(&self, action: ActionId, note: &str) -> Result<(), JournalError> {
        let n = self.conn.execute(
            "UPDATE actions SET note = COALESCE(note || char(10), '') || ?2 WHERE id = ?1",
            rusqlite::params![action, note],
        )?;
        if n == 0 {
            return Err(JournalError::ActionNotFound(action));
        }
        Ok(())
    }

    pub fn set_pinned(&self, action: ActionId, pinned: bool) -> Result<(), JournalError> {
        let n = self.conn.execute(
            "UPDATE actions SET pinned = ?2 WHERE id = ?1",
            rusqlite::params![action, pinned],
        )?;
        if n == 0 {
            return Err(JournalError::ActionNotFound(action));
        }
        Ok(())
    }

    /// Store hashes that GC must keep: referenced by a pinned action, by any
    /// action started at/after `retain_after_ms`, or by an action that a live
    /// (pinned/recent) undo targets — a kept chain row must keep its objects,
    /// or redo would fail on a journal entry that still exists.
    pub fn live_hashes(&self, retain_after_ms: i64) -> Result<BTreeSet<String>, JournalError> {
        let mut stmt = self.conn.prepare(
            // `pending` is live regardless of age: a long-running command's
            // pre-snapshot must survive until its post event settles it —
            // evicting those objects would strand the action doover is
            // actively protecting (D2 review)
            "SELECT m.hashes FROM manifests m
             JOIN actions a ON a.id = m.action_id
             WHERE a.pinned = 1 OR a.started_at_ms >= ?1 OR a.status = 'pending'
                OR EXISTS (SELECT 1 FROM actions r
                           WHERE r.target_action_id = a.id
                             AND (r.pinned = 1 OR r.started_at_ms >= ?1))",
        )?;
        let rows = stmt.query_map([retain_after_ms], |r| r.get::<_, String>(0))?;
        let mut out = BTreeSet::new();
        for json in rows {
            let hashes: Vec<String> = serde_json::from_str(&json?)?;
            out.extend(hashes);
        }
        Ok(out)
    }

    pub fn action(&self, id: ActionId) -> Result<ActionRecord, JournalError> {
        self.conn
            .query_row(
                &format!("{SELECT_ACTION} WHERE id = ?1"),
                [id],
                row_to_action,
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => JournalError::ActionNotFound(id),
                other => other.into(),
            })
    }

    pub fn session_actions(&self, session_id: &str) -> Result<Vec<ActionRecord>, JournalError> {
        let mut stmt = self.conn.prepare(&format!(
            "{SELECT_ACTION} WHERE session_id = ?1 ORDER BY seq"
        ))?;
        let rows = stmt.query_map([session_id], row_to_action)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Newest action timestamp in the journal — the reference point for
    /// retention cutoffs. NEVER use the wall clock for that (CLAUDE.md: a
    /// backward NTP jump would make recent snapshots look collectable).
    pub fn max_started_at(&self) -> Result<Option<i64>, JournalError> {
        Ok(self
            .conn
            .query_row("SELECT MAX(started_at_ms) FROM actions", [], |r| r.get(0))?)
    }

    /// Prune journal rows older than `cutoff_ms`. Never touches pinned rows,
    /// pending rows, or rows referenced as an undo target by a surviving row
    /// (chain integrity; the referencing row's own pruning frees them for the
    /// NEXT pass — eventual cleanup). Old `raw_command` strings may embed
    /// secrets, which is why pruning rows (not just store objects) matters.
    /// Returns (actions_pruned, sessions_pruned); `dry_run` only counts.
    pub fn prune_before(&self, cutoff_ms: i64, dry_run: bool) -> Result<(u64, u64), JournalError> {
        const CANDIDATES: &str = "FROM actions a
             WHERE a.started_at_ms < ?1 AND a.pinned = 0 AND a.status != 'pending'
               AND NOT EXISTS (SELECT 1 FROM actions r WHERE r.target_action_id = a.id)";
        if dry_run {
            let n: i64 = self.conn.query_row(
                &format!("SELECT COUNT(*) {CANDIDATES}"),
                [cutoff_ms],
                |r| r.get(0),
            )?;
            // honest estimate, not a hardcoded zero: old sessions that are
            // already empty or whose every action is itself a candidate
            let s: i64 = self.conn.query_row(
                "SELECT COUNT(*) FROM sessions s
                 WHERE s.started_at_ms < ?1
                   AND NOT EXISTS (
                     SELECT 1 FROM actions a WHERE a.session_id = s.id
                       AND NOT (a.started_at_ms < ?1 AND a.pinned = 0
                                AND a.status != 'pending'
                                AND NOT EXISTS (SELECT 1 FROM actions r
                                                WHERE r.target_action_id = a.id)))",
                [cutoff_ms],
                |r| r.get(0),
            )?;
            return Ok((n as u64, s as u64));
        }
        self.conn.execute_batch("BEGIN IMMEDIATE;")?;
        let result = (|| -> Result<(u64, u64), JournalError> {
            self.conn.execute(
                &format!("DELETE FROM manifests WHERE action_id IN (SELECT a.id {CANDIDATES})"),
                [cutoff_ms],
            )?;
            let actions = self.conn.execute(
                &format!("DELETE FROM actions WHERE id IN (SELECT a.id {CANDIDATES})"),
                [cutoff_ms],
            )? as u64;
            // empty AND old (journal-relative), never merely empty: a session
            // between begin_session and its first start_action is empty but
            // live — deleting it would break the in-flight hook's FK insert
            // and silently drop that action's protection
            let sessions = self.conn.execute(
                "DELETE FROM sessions
                 WHERE started_at_ms < ?1
                   AND id NOT IN (SELECT DISTINCT session_id FROM actions)",
                [cutoff_ms],
            )? as u64;
            Ok((actions, sessions))
        })();
        match &result {
            Ok(_) => self.conn.execute_batch("COMMIT;")?,
            Err(_) => self.conn.execute_batch("ROLLBACK;").unwrap_or(()),
        }
        result
    }

    /// The `started_at_ms` of the last row in the next oldest-first batch of
    /// evictable actions strictly before `before_ms` (size-cap eviction, D2).
    /// Evictable = the same condition `prune_before` deletes by: unpinned,
    /// non-pending, not referenced by an undo. `None` = nothing left to evict
    /// below the ceiling — the caller must stop and report, never force.
    pub fn oldest_evictable_batch_end(
        &self,
        batch: u32,
        before_ms: i64,
    ) -> Result<Option<i64>, JournalError> {
        let mut stmt = self.conn.prepare(
            "SELECT a.started_at_ms FROM actions a
             WHERE a.started_at_ms < ?1 AND a.pinned = 0 AND a.status != 'pending'
               AND NOT EXISTS (SELECT 1 FROM actions r WHERE r.target_action_id = a.id)
             ORDER BY a.started_at_ms ASC
             LIMIT ?2",
        )?;
        let mut last = None;
        for row in stmt.query_map(rusqlite::params![before_ms, batch], |r| r.get::<_, i64>(0))? {
            last = Some(row?);
        }
        Ok(last)
    }

    /// Test support: rewrite an action's timestamp so retention tests can
    /// construct explicit timelines. Not part of the product surface.
    pub fn set_started_at_for_test(
        &self,
        action: ActionId,
        at_ms: i64,
    ) -> Result<(), JournalError> {
        let n = self.conn.execute(
            "UPDATE actions SET started_at_ms = ?2 WHERE id = ?1",
            rusqlite::params![action, at_ms],
        )?;
        if n == 0 {
            return Err(JournalError::ActionNotFound(action));
        }
        Ok(())
    }

    /// Test support: backdate a session's start so retention tests can build
    /// explicit timelines. Not part of the product surface.
    pub fn set_session_started_at_for_test(
        &self,
        session_id: &str,
        at_ms: i64,
    ) -> Result<(), JournalError> {
        self.conn.execute(
            "UPDATE sessions SET started_at_ms = ?2 WHERE id = ?1",
            rusqlite::params![session_id, at_ms],
        )?;
        Ok(())
    }

    /// (sessions, per-status action counts) for `status`/`doctor`.
    pub fn stats(&self) -> Result<(u64, Vec<(String, u64)>), JournalError> {
        let sessions: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))?;
        let mut stmt = self
            .conn
            .prepare("SELECT status, COUNT(*) FROM actions GROUP BY status ORDER BY status")?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64))
        })?;
        Ok((sessions as u64, rows.collect::<Result<Vec<_>, _>>()?))
    }

    pub fn integrity_check(&self) -> Result<bool, JournalError> {
        let verdict: String = self
            .conn
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
        Ok(verdict == "ok")
    }
}

const SELECT_ACTION: &str = "SELECT id, session_id, seq, kind, tool_use_id, raw_command, effect,
        rule_id, has_unknown, status, target_action_id, target_prior_status, pinned,
        started_at_ms, duration_ms, note
 FROM actions";

fn row_to_action(r: &rusqlite::Row) -> Result<ActionRecord, rusqlite::Error> {
    Ok(ActionRecord {
        id: r.get(0)?,
        session_id: r.get(1)?,
        seq: r.get(2)?,
        kind: ActionKind::parse(&r.get::<_, String>(3)?),
        tool_use_id: r.get(4)?,
        raw_command: r.get(5)?,
        effect: r.get(6)?,
        rule_id: r.get(7)?,
        has_unknown: r.get(8)?,
        status: ActionStatus::parse(&r.get::<_, String>(9)?),
        target_action_id: r.get(10)?,
        target_prior_status: r
            .get::<_, Option<String>>(11)?
            .map(|s| ActionStatus::parse(&s)),
        pinned: r.get(12)?,
        started_at_ms: r.get(13)?,
        duration_ms: r.get(14)?,
        note: r.get(15)?,
    })
}
