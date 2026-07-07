//! Hook engine: the composition point where harness events become protected
//! actions. `handle_pre` = parse → resolve scope → snapshot (ALWAYS under
//! limits) → journal pending; `handle_post` = correlate by tool_use_id →
//! completed. Contract facts baked in from the live capture (fixtures
//! README): the harness sends the session's live cwd per call, there are no
//! exit codes, and failed commands never emit a post event.
//!
//! Error philosophy: this library returns honest errors; the BINARY converts
//! them to fail-open (never block the agent). One exception is snapshotting:
//! once an action is journaled, per-path snapshot failures degrade to loud
//! journal notes instead of errors, so partial protection is recorded rather
//! than discarded.

use crate::journal::{ActionId, Journal, JournalError, NewAction};
use crate::registry::Registry;
use crate::resolver::{Ctx, Severity, resolve};
use crate::snapshot::{Limits, SnapshotError, Store};
use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum HookError {
    #[error("event parse: {0}")]
    Parse(String),
    #[error("event is for tool {0}, not Bash")]
    NotBash(String),
    #[error(transparent)]
    Journal(#[from] JournalError),
    #[error(transparent)]
    Snapshot(#[from] SnapshotError),
    #[error("registry: {0}")]
    Registry(#[from] crate::registry::RegistryError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnknownPolicy {
    /// Snapshot the working directory (bounded by limits) when any part of
    /// the command escaped full accounting. The default.
    SnapshotCwd,
    /// Journal the gap loudly but snapshot nothing.
    Passthrough,
}

pub struct HookConfig {
    /// Store + journal + user registry overlay live here (default ~/.doover).
    pub doover_home: PathBuf,
    /// The user's home, for tilde resolution (the hook process's own $HOME).
    pub home: PathBuf,
    /// Applied to EVERY snapshot — known-destructive scopes included
    /// (carried-forward requirement; `rm -rf huge/` must not stall the hook
    /// unboundedly).
    pub limits: Limits,
    pub unknown_policy: UnknownPolicy,
}

impl HookConfig {
    /// Environment-driven config for the binary: DOOVER_HOME,
    /// DOOVER_MAX_FILES, DOOVER_MAX_BYTES, DOOVER_UNKNOWN_POLICY.
    pub fn from_env() -> Self {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/"));
        let doover_home = std::env::var_os("DOOVER_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".doover"));
        let env_u64 = |k: &str, default: u64| {
            std::env::var(k)
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(default)
        };
        let unknown_policy = match std::env::var("DOOVER_UNKNOWN_POLICY").as_deref() {
            Ok("passthrough") => UnknownPolicy::Passthrough,
            _ => UnknownPolicy::SnapshotCwd,
        };
        Self {
            doover_home,
            home,
            limits: Limits {
                max_files: env_u64("DOOVER_MAX_FILES", 100_000),
                max_bytes: env_u64("DOOVER_MAX_BYTES", 5 * 1024 * 1024 * 1024),
            },
            unknown_policy,
        }
    }
}

#[derive(Debug)]
pub struct PreEvent {
    pub session_id: String,
    pub tool_use_id: String,
    pub tool_name: String,
    pub cwd: PathBuf,
    pub command: String,
}

#[derive(Debug)]
pub struct PostEvent {
    pub session_id: String,
    pub tool_use_id: String,
    pub tool_name: String,
    pub duration_ms: i64,
}

mod wire {
    #[derive(serde::Deserialize)]
    pub struct ToolInput {
        pub command: Option<String>,
    }
    #[derive(serde::Deserialize)]
    pub struct Event {
        pub session_id: String,
        pub cwd: String,
        pub tool_name: String,
        pub tool_use_id: String,
        pub tool_input: Option<ToolInput>,
        pub duration_ms: Option<i64>,
    }
}

pub fn parse_pre_event(json: &str) -> Result<PreEvent, HookError> {
    let e: wire::Event = serde_json::from_str(json).map_err(|e| HookError::Parse(e.to_string()))?;
    if e.tool_name != "Bash" {
        return Err(HookError::NotBash(e.tool_name));
    }
    let command = e
        .tool_input
        .and_then(|t| t.command)
        .ok_or_else(|| HookError::Parse("missing tool_input.command".into()))?;
    Ok(PreEvent {
        session_id: e.session_id,
        tool_use_id: e.tool_use_id,
        tool_name: "Bash".into(),
        cwd: PathBuf::from(e.cwd),
        command,
    })
}

pub fn parse_post_event(json: &str) -> Result<PostEvent, HookError> {
    let e: wire::Event = serde_json::from_str(json).map_err(|e| HookError::Parse(e.to_string()))?;
    if e.tool_name != "Bash" {
        return Err(HookError::NotBash(e.tool_name));
    }
    Ok(PostEvent {
        session_id: e.session_id,
        tool_use_id: e.tool_use_id,
        tool_name: "Bash".into(),
        // the contract has no exit code; duration is the only post metric
        duration_ms: e.duration_ms.unwrap_or(0),
    })
}

/// Outcome summary for logging and, crucially, for the binary's runtime
/// warning. `gaps` holds the loud protection-gap messages (snapshot failures,
/// truncations) — non-empty means coverage is incomplete. The binary warns
/// when a destructive+ action has gaps, so "I ran but couldn't fully protect
/// you" is never silent (audit round 9).
pub struct PreOutcome {
    pub action_id: ActionId,
    pub manifests_attached: usize,
    pub severity: Severity,
    pub gaps: Vec<String>,
}

impl PreOutcome {
    /// True when the binary should emit a loud (but non-blocking) warning: a
    /// destructive-or-worse action whose protection is incomplete.
    pub fn needs_warning(&self) -> bool {
        self.severity >= Severity::Destructive && !self.gaps.is_empty()
    }
}

fn open_journal(cfg: &HookConfig) -> Result<Journal, HookError> {
    std::fs::create_dir_all(&cfg.doover_home).map_err(|e| {
        HookError::Parse(format!(
            "cannot create doover home {}: {e}",
            cfg.doover_home.display()
        ))
    })?;
    Ok(Journal::open(&cfg.doover_home.join("journal.db"))?)
}

pub fn handle_pre(cfg: &HookConfig, ev: &PreEvent) -> Result<PreOutcome, HookError> {
    let journal = open_journal(cfg)?;
    journal.begin_session(&ev.session_id, "claude-code", &ev.cwd.to_string_lossy())?;

    let (registry, overlay_warnings) = Registry::with_overlay(&cfg.doover_home.join("registry.d"))?;
    for w in &overlay_warnings {
        eprintln!("doover: registry overlay: {w}");
    }

    let ctx = Ctx {
        cwd: &ev.cwd,
        home: &cfg.home,
    };
    let r = resolve(&ev.command, &registry, &ctx);

    let action = journal.start_action(&NewAction {
        session_id: &ev.session_id,
        tool_use_id: Some(&ev.tool_use_id),
        raw_command: &ev.command,
        effect: r.severity.as_str(),
        rule_id: r.rule_id.as_deref(),
        has_unknown: r.has_unknown,
    })?;

    // snapshot destructive+ scopes; the unknown policy adds a bounded cwd
    // snapshot when anything escaped accounting
    let mut targets: Vec<PathBuf> = Vec::new();
    if r.severity >= Severity::Destructive {
        targets.extend(r.paths.iter().cloned());
    }
    if r.has_unknown && cfg.unknown_policy == UnknownPolicy::SnapshotCwd {
        let cwd = crate::resolver::normalize_lexical(&ev.cwd);
        if !targets.contains(&cwd) {
            targets.push(cwd);
        }
    }

    let mut attached = 0usize;
    let mut gaps: Vec<String> = Vec::new();
    // record a gap both in the journal (for `log`) and in the outcome (for
    // the binary's runtime warning)
    let mut note_gap = |journal: &Journal, msg: String| -> Result<(), HookError> {
        journal.add_note(action, &msg)?;
        gaps.push(msg);
        Ok(())
    };
    if !targets.is_empty() {
        let store = Store::open(cfg.doover_home.join("store"))?;
        for path in &targets {
            // once the action exists, per-path failures become loud gaps,
            // never lost protection for the OTHER paths — and never silent
            match store.snapshot(path, Some(&cfg.limits)) {
                Ok(manifest) => {
                    if manifest.truncated {
                        note_gap(
                            &journal,
                            format!(
                                "UNPROTECTED: snapshot of {} truncated at limits ({} files skipped)",
                                path.display(),
                                manifest.skipped
                            ),
                        )?;
                    }
                    if !manifest.warnings.is_empty() {
                        note_gap(
                            &journal,
                            format!(
                                "PARTIAL: snapshot gaps at {}: {}",
                                path.display(),
                                manifest.warnings.join("; ")
                            ),
                        )?;
                    }
                    journal.attach_manifest(action, &manifest)?;
                    attached += 1;
                }
                Err(e) => {
                    note_gap(
                        &journal,
                        format!("UNPROTECTED: snapshot of {} failed: {e}", path.display()),
                    )?;
                }
            }
        }
    }

    Ok(PreOutcome {
        action_id: action,
        manifests_attached: attached,
        severity: r.severity,
        gaps,
    })
}

pub fn handle_post(cfg: &HookConfig, ev: &PostEvent) -> Result<ActionId, HookError> {
    let journal = open_journal(cfg)?;
    Ok(journal.complete_by_tool_use(&ev.session_id, &ev.tool_use_id, ev.duration_ms)?)
}
