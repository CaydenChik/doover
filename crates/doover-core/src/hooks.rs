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

use crate::journal::{ActionId, Journal, JournalError, ManifestRole, NewAction};
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

/// Default wall-clock budget for a single snapshot. Sized to finish, wrap up,
/// and journal comfortably inside the harness hook timeout (installed at 20s),
/// so the loud PARTIAL-coverage gap always wins the race against a SIGKILL
/// rather than losing it (bench D1).
const DEFAULT_SNAPSHOT_MS: u64 = 5_000;

fn snapshot_budget() -> Option<std::time::Duration> {
    parse_snapshot_budget(std::env::var("DOOVER_MAX_SNAPSHOT_MS").ok().as_deref())
}

/// The snapshot time budget the hook binary will actually run with —
/// exposed for `doover doctor`, which cross-checks it against the installed
/// hook timeout (a budget at/above the timeout re-creates the SIGKILL blind
/// spot the budget exists to close).
pub fn effective_snapshot_budget() -> Option<std::time::Duration> {
    snapshot_budget()
}

/// Parse `DOOVER_MAX_SNAPSHOT_MS`, fail-safe. Unset or unparseable → the 5s
/// default; an explicit `0` → no budget (unlimited, the documented opt-out).
/// Garbage never silently reduces protection to nothing.
fn parse_snapshot_budget(v: Option<&str>) -> Option<std::time::Duration> {
    let default = std::time::Duration::from_millis(DEFAULT_SNAPSHOT_MS);
    match v {
        None => Some(default),
        Some(s) => match s.trim().parse::<u64>() {
            Ok(0) => None,
            Ok(ms) => Some(std::time::Duration::from_millis(ms)),
            Err(_) => Some(default),
        },
    }
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
    /// Store budgets + the automatic gc cadence (D2). The post hook runs gc
    /// when the cadence or free-space floor fires, so the store self-bounds
    /// without a cron.
    pub maintenance: crate::maintenance::MaintenanceBudget,
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
                max_duration: snapshot_budget(),
            },
            unknown_policy,
            maintenance: crate::maintenance::MaintenanceBudget::from_env(),
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

/// One wall-clock deadline shared by EVERY snapshot in a single hook
/// invocation (round 19): N targets must never stack N budgets past the
/// harness timeout — that would re-create the SIGKILL blind spot the D1
/// budget exists to close. Each call gets the time remaining; at/after the
/// deadline a zero budget makes the snapshot truncate immediately, which the
/// existing machinery journals as a loud gap. A `None` base budget (explicit
/// opt-out) stays unlimited.
fn slice_limits(base: &Limits, deadline: Option<std::time::Instant>) -> Limits {
    let mut l = *base;
    if let Some(dl) = deadline {
        l.max_duration = Some(dl.saturating_duration_since(std::time::Instant::now()));
    }
    l
}

fn hook_deadline(limits: &Limits) -> Option<std::time::Instant> {
    limits.max_duration.map(|d| std::time::Instant::now() + d)
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
    /// True when the binary should emit a loud (but non-blocking) warning: any
    /// protection gap at all. `gaps` is only ever populated when a snapshot was
    /// ATTEMPTED (a destructive scope, or the defensive cwd snapshot for an
    /// unknown command) and it failed or truncated — so a non-empty `gaps`
    /// always means "we tried to protect you and couldn't fully." Gating this
    /// on `severity >= Destructive` (as the first cut did) wrongly silenced the
    /// unknown path, which is exactly where we defend BECAUSE the command might
    /// be destructive. Safe/mutating commands never snapshot, so never warn.
    pub fn needs_warning(&self) -> bool {
        !self.gaps.is_empty()
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
        let deadline = hook_deadline(&cfg.limits);
        for path in &targets {
            // once the action exists, per-path failures become loud gaps,
            // never lost protection for the OTHER paths — and never silent
            match store.snapshot(path, Some(&slice_limits(&cfg.limits, deadline))) {
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
                    journal.attach_manifest(action, &manifest, ManifestRole::Pre)?;
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
    let action = journal.complete_by_tool_use(&ev.session_id, &ev.tool_use_id, ev.duration_ms)?;

    // capture POST state for every path we pre-snapshotted: it is what redo
    // restores, and the conflict oracle for undo ("is the world still as the
    // action left it?"). Failures degrade to journal notes — undo still works
    // from the pre-manifests, just without conflict verification.
    let pre = journal.manifests_by_role(action, ManifestRole::Pre)?;
    if !pre.is_empty() {
        let store = Store::open(cfg.doover_home.join("store"))?;
        let deadline = hook_deadline(&cfg.limits);
        for m in &pre {
            match store.snapshot(&m.path, Some(&slice_limits(&cfg.limits, deadline))) {
                Ok(post) => journal.attach_manifest(action, &post, ManifestRole::Post)?,
                Err(e) => journal.add_note(
                    action,
                    &format!(
                        "post-state snapshot of {} failed: {e} (redo/conflict checks unavailable)",
                        m.path.display()
                    ),
                )?,
            }
        }
    }
    maybe_gc(cfg, &journal, action);
    Ok(action)
}

/// How long a free-space breach waits between triggered passes. A low disk
/// that doover cannot fix (someone else's data, CoW-shared blocks) must not
/// re-run a full gc on every single action.
const FREE_LOW_RETRIGGER_SECS: u64 = 600;

/// Automatic gc from the post hook (D2): the store must self-bound without a
/// cron. STRICTLY fail-open — the action already completed; no maintenance
/// failure may surface to the harness.
///
/// Scope is deliberately narrower than manual `doover gc` (D2 review):
/// - `gc_every == 0` disables ALL automatic gc, free-space path included;
/// - the free-space floor triggers a retention+cap pass (rate-limited) and a
///   loud warning, but NEVER deficit-driven eviction — destroying history
///   over disk pressure that is usually not doover's fault (and frees ~0
///   physical bytes on CoW) is a decision only an explicit `doover gc` makes;
/// - the pass carries a wall-clock budget so it stays off the critical path;
/// - anything evicted (or an unmeetable budget) is journaled on the
///   triggering action and warned to stderr — never silent.
fn maybe_gc(cfg: &HookConfig, journal: &Journal, action: ActionId) {
    let b = cfg.maintenance;
    if b.gc_every == 0 {
        return; // full opt-out of automatic maintenance
    }
    let cadence_due = action % (b.gc_every as i64).max(1) == 0;
    let free_low = b
        .min_free_bytes
        .zip(crate::snapshot::free_bytes(&cfg.doover_home))
        .is_some_and(|(floor, free)| free < floor);
    let free_low_due = free_low && free_low_retrigger_elapsed(cfg);
    if !cadence_due && !free_low_due {
        return;
    }
    let Ok(store) = Store::open(cfg.doover_home.join("store")) else {
        return;
    };
    let report = crate::maintenance::gc(
        journal,
        &store,
        &cfg.doover_home,
        &crate::maintenance::auto_gc_options(&b),
    );
    touch_gc_marker(cfg);
    let Ok(report) = report else { return };

    // visibility: history removal is never silent (D2 review, critical)
    if report.cap_evicted_actions > 0 || report.still_over_budget {
        let note = format!(
            "auto-gc: evicted {} old action(s) to satisfy the store size cap{}",
            report.cap_evicted_actions,
            if report.still_over_budget {
                "; STILL over budget (bounded by pins/recent actions — run `doover gc`)"
            } else {
                ""
            }
        );
        let _ = journal.add_note(action, &note);
        eprintln!("doover: {note}");
    }
    if free_low {
        eprintln!(
            "doover: disk space is low (below the DOOVER_MIN_FREE_BYTES floor); \
             doover's store is bounded by its size cap — run `doover gc` to review \
             or evict more history"
        );
    }
}

/// Rate limit for the free-space trigger, via the mtime of a marker file.
/// Fail-open in the triggering direction: no marker (first run) or an
/// unreadable one means "due".
fn free_low_retrigger_elapsed(cfg: &HookConfig) -> bool {
    let marker = cfg.doover_home.join(".last-auto-gc");
    match std::fs::metadata(&marker).and_then(|m| m.modified()) {
        Ok(at) => std::time::SystemTime::now()
            .duration_since(at)
            .map(|age| age.as_secs() >= FREE_LOW_RETRIGGER_SECS)
            .unwrap_or(true),
        Err(_) => true,
    }
}

fn touch_gc_marker(cfg: &HookConfig) {
    let _ = std::fs::write(cfg.doover_home.join(".last-auto-gc"), b"");
}

#[cfg(test)]
mod budget_tests {
    use super::{DEFAULT_SNAPSHOT_MS, parse_snapshot_budget};
    use std::time::Duration;

    #[test]
    fn budget_parse_is_fail_safe() {
        let default = Some(Duration::from_millis(DEFAULT_SNAPSHOT_MS));
        // unset and garbage both fall to the safe default — never off
        assert_eq!(parse_snapshot_budget(None), default);
        assert_eq!(parse_snapshot_budget(Some("nonsense")), default);
        assert_eq!(parse_snapshot_budget(Some("-5")), default);
        assert_eq!(parse_snapshot_budget(Some("")), default);
        // explicit 0 is the documented "no budget" opt-out
        assert_eq!(parse_snapshot_budget(Some("0")), None);
        // a real value is honored (whitespace tolerated)
        assert_eq!(
            parse_snapshot_budget(Some("2500")),
            Some(Duration::from_millis(2500))
        );
        assert_eq!(
            parse_snapshot_budget(Some("  8000 ")),
            Some(Duration::from_millis(8000))
        );
    }
}
