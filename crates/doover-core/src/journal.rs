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

const SCHEMA_VERSION: i64 = 1;
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, thiserror::Error)]
pub enum JournalError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("manifest serialization: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("action {0} not found")]
    ActionNotFound(i64),
    #[error("no pending action with tool_use_id {tool_use_id} in session {session_id}")]
    NoPendingForToolUse {
        session_id: String,
        tool_use_id: String,
    },
    #[error(
        "cannot undo action {id}: status is {status:?} (must be completed, abandoned, or undone)"
    )]
    NotUndoable { id: i64, status: ActionStatus },
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
    pub pinned: bool,
    pub started_at_ms: i64,
    pub duration_ms: Option<i64>,
    pub note: Option<String>,
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
        let conn = rusqlite::Connection::open(path)?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        conn.pragma_update(None, "journal_mode", "wal")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;

        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version > SCHEMA_VERSION {
            return Err(JournalError::SchemaTooNew { found: version });
        }
        if version < SCHEMA_VERSION {
            conn.execute_batch(&format!(
                "BEGIN;
                 CREATE TABLE IF NOT EXISTS sessions(
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
                 PRAGMA user_version = {SCHEMA_VERSION};
                 COMMIT;"
            ))?;
        }
        Ok(Self { conn })
    }

    pub fn begin_session(&self, id: &str, harness: &str, cwd: &str) -> Result<(), JournalError> {
        self.conn.execute(
            "INSERT INTO sessions(id, harness, cwd, started_at_ms) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(id) DO NOTHING",
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
                "UPDATE actions SET status = 'completed', duration_ms = ?3
                 WHERE session_id = ?1 AND tool_use_id = ?2
                   AND status IN ('pending', 'abandoned')
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
            "INSERT INTO manifests(action_id, path, manifest_json, hashes, truncated)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                action,
                manifest.path.to_string_lossy(),
                json,
                serde_json::to_string(&hashes)?,
                manifest.truncated,
            ],
        )?;
        Ok(())
    }

    pub fn manifests(&self, action: ActionId) -> Result<Vec<Manifest>, JournalError> {
        let mut stmt = self
            .conn
            .prepare("SELECT manifest_json FROM manifests WHERE action_id = ?1 ORDER BY id")?;
        let rows = stmt.query_map([action], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for json in rows {
            out.push(serde_json::from_str(&json?)?);
        }
        Ok(out)
    }

    /// Record an undo of `target` as a new journaled action. The target flips
    /// to `undone`; undoing an *undo* additionally flips that undo's own
    /// target back to `completed` — that is redo, with no history rewritten.
    pub fn record_undo(
        &self,
        session_id: &str,
        target: ActionId,
    ) -> Result<ActionId, JournalError> {
        let target_rec = self.action(target)?;
        if matches!(target_rec.status, ActionStatus::Pending) {
            return Err(JournalError::NotUndoable {
                id: target,
                status: target_rec.status,
            });
        }
        self.conn.execute_batch("BEGIN IMMEDIATE;")?;
        let result = (|| -> Result<ActionId, JournalError> {
            let seq: i64 = self.conn.query_row(
                "SELECT COALESCE(MAX(seq), 0) + 1 FROM actions WHERE session_id = ?1",
                [session_id],
                |r| r.get(0),
            )?;
            self.conn.execute(
                "INSERT INTO actions(session_id, seq, kind, raw_command, effect,
                                     status, target_action_id, started_at_ms)
                 VALUES (?1, ?2, 'undo', ?3, 'destructive', 'completed', ?4, ?5)",
                rusqlite::params![
                    session_id,
                    seq,
                    format!("undo of action {target}"),
                    target,
                    now_ms(),
                ],
            )?;
            let undo_id = self.conn.last_insert_rowid();
            self.conn.execute(
                "UPDATE actions SET status = 'undone' WHERE id = ?1",
                [target],
            )?;
            // redo semantics: undoing an undo restores ITS target
            if target_rec.kind == ActionKind::Undo {
                if let Some(original) = target_rec.target_action_id {
                    self.conn.execute(
                        "UPDATE actions SET status = 'completed' WHERE id = ?1",
                        [original],
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

    /// Store hashes that GC must keep: referenced by a pinned action, or by
    /// any action started at/after `retain_after_ms`.
    pub fn live_hashes(&self, retain_after_ms: i64) -> Result<BTreeSet<String>, JournalError> {
        let mut stmt = self.conn.prepare(
            "SELECT m.hashes FROM manifests m
             JOIN actions a ON a.id = m.action_id
             WHERE a.pinned = 1 OR a.started_at_ms >= ?1",
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

    pub fn integrity_check(&self) -> Result<bool, JournalError> {
        let verdict: String = self
            .conn
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
        Ok(verdict == "ok")
    }
}

const SELECT_ACTION: &str = "SELECT id, session_id, seq, kind, tool_use_id, raw_command, effect,
        rule_id, has_unknown, status, target_action_id, pinned, started_at_ms, duration_ms, note
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
        pinned: r.get(11)?,
        started_at_ms: r.get(12)?,
        duration_ms: r.get(13)?,
        note: r.get(14)?,
    })
}
