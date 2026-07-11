use clap::{Parser, Subcommand};
use std::io::Read;

/// Hook stdin is harness-controlled JSON; cap reads defensively.
const MAX_EVENT_BYTES: u64 = 10 * 1024 * 1024;

#[derive(Parser)]
#[command(name = "doover", version, about = "Every agent deserves a do-over.")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Install the Claude Code Bash hooks and create the store
    Init {
        /// Write into the project's .claude/settings.json instead of the
        /// user-global ~/.claude/settings.json
        #[arg(long)]
        project: bool,
        /// Print what would change without writing
        #[arg(long)]
        dry_run: bool,
    },
    /// List recent journaled agent actions
    Log {
        /// How many actions to show
        #[arg(short = 'n', long, default_value_t = 20)]
        limit: i64,
    },
    /// Show one action's details and snapshot manifests
    Show {
        /// Journal action id (see `doover log`)
        id: i64,
    },
    /// Restore the state from before an action (latest undoable by default)
    Undo {
        /// Journal action id (defaults to the latest undoable action)
        id: Option<i64>,
        /// Restore even if the touched paths changed since the action
        #[arg(long)]
        force: bool,
        /// Print the restore plan without changing anything
        #[arg(long)]
        dry_run: bool,
    },
    /// Revert an undo, re-applying the action's effect
    Redo {
        /// Journal id of the undo action (defaults to the latest undo)
        id: Option<i64>,
        /// Re-apply even if the touched paths changed since the undo
        #[arg(long)]
        force: bool,
        /// Print the plan without changing anything
        #[arg(long)]
        dry_run: bool,
    },
    /// Diff an action's pre-state against the current filesystem
    Diff {
        /// Journal action id (see `doover log`)
        id: i64,
    },
    /// Store and session health summary
    Status,
    /// Prune old snapshots and journal rows (journal-relative retention)
    Gc {
        /// Keep everything newer than this many days before the newest action
        #[arg(long, default_value_t = 7)]
        keep_days: i64,
        /// Show what would be removed without deleting
        #[arg(long)]
        dry_run: bool,
    },
    /// Check hooks, store, and platform capabilities
    Doctor,
    /// Harness-facing hook entrypoints (stdin JSON)
    #[command(subcommand)]
    Hook(HookCommand),
}

/// Exit code for a refused undo/redo due to conflicts (see CLAUDE.md).
const EXIT_CONFLICT: i32 = 3;

#[derive(Subcommand)]
enum HookCommand {
    /// PreToolUse: classify, snapshot, journal
    Pre,
    /// PostToolUse: correlate and complete
    Post,
}

fn main() {
    let cli = Cli::parse();
    match cli.command {
        Command::Hook(kind) => run_hook_fail_open(kind),
        Command::Undo { id, force, dry_run } => run_undo_redo(Verb::Undo, id, force, dry_run),
        Command::Redo { id, force, dry_run } => run_undo_redo(Verb::Redo, id, force, dry_run),
        Command::Log { limit } => run_log(limit),
        Command::Init { project, dry_run } => std::process::exit(run_init(project, dry_run)),
        Command::Gc { keep_days, dry_run } => std::process::exit(run_gc(keep_days, dry_run)),
        Command::Status => std::process::exit(run_status()),
        Command::Doctor => std::process::exit(run_doctor()),
        Command::Show { id } => std::process::exit(run_show(id)),
        Command::Diff { id } => std::process::exit(run_diff(id)),
    }
}

enum Verb {
    Undo,
    Redo,
}

/// Open the journal, creating DOOVER_HOME first so a fresh install reads as an
/// empty history (friendly messages) rather than an open error.
fn open_journal_or_exit(cfg: &doover_core::hooks::HookConfig) -> doover_core::journal::Journal {
    if let Err(e) = std::fs::create_dir_all(&cfg.doover_home) {
        eprintln!("doover: cannot create {}: {e}", cfg.doover_home.display());
        std::process::exit(1);
    }
    match doover_core::journal::Journal::open(&cfg.doover_home.join("journal.db")) {
        Ok(j) => j,
        Err(e) => {
            eprintln!("doover: cannot open journal: {e}");
            std::process::exit(1);
        }
    }
}

fn run_undo_redo(verb: Verb, id: Option<i64>, force: bool, dry_run: bool) {
    use doover_core::undo::{Selector, UndoEngine, UndoError};
    let cfg = doover_core::hooks::HookConfig::from_env();
    let journal = open_journal_or_exit(&cfg);
    let store = match doover_core::snapshot::Store::open(cfg.doover_home.join("store")) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("doover: cannot open store: {e}");
            std::process::exit(1);
        }
    };
    let engine = UndoEngine::new(&journal, &store);
    let sel = id.map(Selector::Action).unwrap_or(Selector::Latest);
    let (verb_str, result) = match verb {
        Verb::Undo => ("undo", engine.undo(sel, force, dry_run)),
        Verb::Redo => ("redo", engine.redo(sel, force, dry_run)),
    };
    match result {
        Ok(report) => {
            if report.dry_run {
                println!("would {verb_str} action #{}:", report.target_action);
            } else {
                println!(
                    "{verb_str} of action #{} complete — {} path(s) restored{}",
                    report.target_action,
                    report.paths_restored,
                    if report.forced { " (forced)" } else { "" }
                );
            }
            for line in &report.plan {
                println!("  {line}");
            }
            for w in &report.warnings {
                eprintln!("doover: warning: {w}");
            }
        }
        Err(e @ UndoError::Conflicts(_)) => {
            eprintln!("doover: {e}");
            std::process::exit(EXIT_CONFLICT);
        }
        Err(e) => {
            eprintln!("doover: {e}");
            std::process::exit(1);
        }
    }
}

fn run_log(limit: i64) {
    let cfg = doover_core::hooks::HookConfig::from_env();
    let journal = open_journal_or_exit(&cfg);
    let actions = match journal.recent_actions(limit) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("doover: {e}");
            std::process::exit(1);
        }
    };
    if actions.is_empty() {
        println!("no journaled actions yet");
        return;
    }
    for a in &actions {
        use doover_core::journal::ActionStatus;
        let status = match a.status {
            ActionStatus::Pending => "pending  ",
            ActionStatus::Completed => "completed",
            ActionStatus::Abandoned => "abandoned",
            ActionStatus::Undone => "undone   ",
        };
        let mut cmd = doover_core::redact::redact(&a.raw_command).replace('\n', " ");
        if cmd.chars().count() > 60 {
            cmd = format!("{}…", cmd.chars().take(59).collect::<String>());
        }
        let flags = match (a.has_unknown, a.note.is_some()) {
            (true, true) => " [unknown, notes]",
            (true, false) => " [unknown]",
            (false, true) => " [notes]",
            (false, false) => "",
        };
        println!("#{:<5} {status}  {:<13} {cmd}{flags}", a.id, a.effect);
    }
}

/// Install the two Bash hooks into a Claude Code settings.json, MERGING with
/// any existing hooks rather than clobbering them. Idempotent: re-running does
/// not duplicate doover's entries. Returns a process exit code.
fn run_init(project: bool, dry_run: bool) -> i32 {
    let settings_path = if project {
        std::path::PathBuf::from(".claude/settings.json")
    } else {
        let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
        match home {
            Some(h) => h.join(".claude/settings.json"),
            None => {
                eprintln!("doover: HOME is not set; use --project or set HOME");
                return 1;
            }
        }
    };

    let existing = std::fs::read_to_string(&settings_path).unwrap_or_default();
    let mut root: serde_json::Value = if existing.trim().is_empty() {
        serde_json::json!({})
    } else {
        match serde_json::from_str(&existing) {
            Ok(v) => v,
            Err(e) => {
                eprintln!(
                    "doover: {} is not valid JSON ({e}); refusing to overwrite it",
                    settings_path.display()
                );
                return 1;
            }
        }
    };
    if !root.is_object() {
        eprintln!("doover: {} is not a JSON object", settings_path.display());
        return 1;
    }

    // shape errors are loud: "valid JSON we cannot merge into" must never
    // read as success (audit round 12 — the dual of the malformed-JSON check)
    let added = match install_bash_hooks(&mut root) {
        Ok(a) => a,
        Err(why) => {
            eprintln!(
                "doover: cannot merge hooks into {}: {why}; fix the file and re-run doover init",
                settings_path.display()
            );
            return 1;
        }
    };
    if !added {
        println!(
            "doover hooks already installed in {}",
            settings_path.display()
        );
        return 0;
    }
    let rendered = match serde_json::to_string_pretty(&root) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("doover: cannot serialize settings: {e}");
            return 1;
        }
    };
    if dry_run {
        println!("would write {} :\n{rendered}", settings_path.display());
        return 0;
    }
    if let Some(parent) = settings_path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            eprintln!("doover: cannot create {}: {e}", parent.display());
            return 1;
        }
    }
    // atomic replace: settings.json is the USER'S config — a crash mid-write
    // must never leave it torn (write temp in the same dir, then rename)
    if let Err(e) = write_atomic(&settings_path, &(rendered + "\n")) {
        eprintln!("doover: cannot write {}: {e}", settings_path.display());
        return 1;
    }
    println!(
        "installed doover Bash hooks into {}\nrun `doover doctor` to verify",
        settings_path.display()
    );
    0
}

/// Write via temp-file-then-rename so the target is never observed torn.
/// Cleans up the temp file if the rename fails.
fn write_atomic(path: &std::path::Path, data: &str) -> std::io::Result<()> {
    let dir = path.parent().filter(|p| !p.as_os_str().is_empty());
    let tmp = dir
        .unwrap_or_else(|| std::path::Path::new("."))
        .join(format!(".doover-init.{}.tmp", std::process::id()));
    std::fs::write(&tmp, data)?;
    let renamed = std::fs::rename(&tmp, path);
    if renamed.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    renamed
}

/// Merge doover's PreToolUse/PostToolUse Bash hooks into a settings object.
/// Ok(true) = added, Ok(false) = both already present (idempotent re-run),
/// Err = an existing value has a shape we refuse to guess at. All-or-
/// nothing: a shape error anywhere means NOTHING was modified.
fn install_bash_hooks(root: &mut serde_json::Value) -> Result<bool, String> {
    use serde_json::{Value, json};
    let obj = root.as_object_mut().expect("checked object");

    // validate every shape we will touch BEFORE mutating anything
    if let Some(h) = obj.get("hooks") {
        if !h.is_object() {
            return Err("the \"hooks\" key is not an object".into());
        }
        for event in ["PreToolUse", "PostToolUse"] {
            if let Some(v) = h.get(event) {
                if !v.is_array() {
                    return Err(format!("hooks.{event} is not an array"));
                }
            }
        }
    }

    let hooks = obj
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .expect("validated above");

    let mut changed = false;
    for (event, cmd) in [
        ("PreToolUse", "doover hook pre"),
        ("PostToolUse", "doover hook post"),
    ] {
        let arr = hooks
            .entry(event)
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .expect("validated above");
        // already present? contains-match so a hand-edited absolute path
        // ("/usr/local/bin/doover hook pre") is not duplicated
        let present = arr.iter().any(|entry| {
            entry
                .get("hooks")
                .and_then(Value::as_array)
                .is_some_and(|hs| {
                    hs.iter().any(|h| {
                        h.get("command")
                            .and_then(Value::as_str)
                            .is_some_and(|c| c.contains(cmd))
                    })
                })
        });
        if !present {
            arr.push(json!({
                "matcher": "Bash",
                "hooks": [{ "type": "command", "command": cmd, "timeout": 20 }]
            }));
            changed = true;
        }
    }
    Ok(changed)
}

/// Fetch action `id` or exit 1 with a "not found" message.
fn action_or_exit(
    journal: &doover_core::journal::Journal,
    id: i64,
) -> doover_core::journal::ActionRecord {
    match journal.action(id) {
        Ok(a) => a,
        Err(doover_core::journal::JournalError::ActionNotFound(_)) => {
            eprintln!("doover: action #{id} not found (see `doover log`)");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("doover: {e}");
            std::process::exit(1);
        }
    }
}

fn run_show(id: i64) -> i32 {
    use doover_core::journal::ManifestRole;
    let cfg = doover_core::hooks::HookConfig::from_env();
    let journal = open_journal_or_exit(&cfg);
    let a = action_or_exit(&journal, id);

    let status = format!("{:?}", a.status).to_lowercase();
    println!("action #{}: {status}", a.id);
    println!("  session:  {} (seq {})", a.session_id, a.seq);
    // display-time redaction: the journal keeps raw_command verbatim for the
    // audit trail; credentials must never reach a terminal
    println!(
        "  command:  {}",
        doover_core::redact::redact(&a.raw_command)
    );
    print!("  effect:   {}", a.effect);
    match &a.rule_id {
        Some(r) => println!(" [{r}]"),
        None => println!(),
    }
    if let Some(t) = a.target_action_id {
        println!("  undoes:   action #{t}");
    }
    if a.pinned {
        println!("  pinned:   yes (gc keeps it)");
    }

    for (role, label) in [(ManifestRole::Pre, "pre"), (ManifestRole::Post, "post")] {
        let manifests = match journal.manifests_by_role(a.id, role) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("doover: cannot read manifests: {e}");
                return 1;
            }
        };
        for m in &manifests {
            println!(
                "  {label} snapshot: {} ({} entr{}{}{})",
                m.path.display(),
                m.entries.len(),
                if m.entries.len() == 1 { "y" } else { "ies" },
                if m.root == doover_core::snapshot::Root::Absent {
                    ", did not exist"
                } else {
                    ""
                },
                if m.truncated { ", TRUNCATED" } else { "" },
            );
            for w in &m.warnings {
                println!("    warning: {w}");
            }
            for e in m.entries.iter().take(20) {
                let kind = match &e.kind {
                    doover_core::snapshot::EntryKind::File { len, .. } => {
                        format!("file {len} B")
                    }
                    doover_core::snapshot::EntryKind::Dir { .. } => "dir".into(),
                    doover_core::snapshot::EntryKind::Symlink { target } => {
                        format!("symlink -> {}", target.display())
                    }
                    doover_core::snapshot::EntryKind::Fifo { .. } => "fifo".into(),
                };
                let rel = if e.rel.as_os_str().is_empty() {
                    ".".into()
                } else {
                    e.rel.display().to_string()
                };
                println!("    {rel}  ({kind})");
            }
            if m.entries.len() > 20 {
                println!(
                    "    … {} more (see `doover diff {id}`)",
                    m.entries.len() - 20
                );
            }
        }
    }
    0
}

fn run_diff(id: i64) -> i32 {
    use doover_core::journal::ManifestRole;
    let cfg = doover_core::hooks::HookConfig::from_env();
    let journal = open_journal_or_exit(&cfg);
    let a = action_or_exit(&journal, id);

    let manifests = match journal.manifests_by_role(a.id, ManifestRole::Pre) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("doover: cannot read manifests: {e}");
            return 1;
        }
    };
    if manifests.is_empty() {
        println!("action #{id} recorded no pre-state (nothing to compare)");
        return 0;
    }
    let mut changed = 0u64;
    let mut total = 0u64;
    let mut partial = false;
    for m in &manifests {
        let report = match doover_core::inspect::diff_manifest(m) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("doover: diff failed for {}: {e}", m.path.display());
                return 1;
            }
        };
        partial |= report.partial;
        for line in report.lines {
            total += 1;
            if line.status != doover_core::inspect::PathStatus::Unchanged {
                changed += 1;
            }
            println!("  {:<12} {}", line.status.as_str(), line.path.display());
        }
    }
    if partial {
        // the recorded snapshot was truncated at limits: this comparison is
        // incomplete, and so is what undo could restore — say so plainly
        println!("  note: the recorded snapshot was truncated; this diff is PARTIAL");
    }
    println!(
        "{changed} of {total} path(s) differ from the pre-state of action #{id}{}",
        if changed > 0 {
            " (`doover undo` restores it)"
        } else {
            ""
        }
    );
    0
}

fn cfg_journal_store() -> Option<(
    doover_core::journal::Journal,
    doover_core::snapshot::Store,
    std::path::PathBuf,
)> {
    let cfg = doover_core::hooks::HookConfig::from_env();
    if std::fs::create_dir_all(&cfg.doover_home).is_err() {
        return None;
    }
    let j = doover_core::journal::Journal::open(&cfg.doover_home.join("journal.db")).ok()?;
    let s = doover_core::snapshot::Store::open(cfg.doover_home.join("store")).ok()?;
    Some((j, s, cfg.doover_home))
}

fn run_gc(keep_days: i64, dry_run: bool) -> i32 {
    let Some((journal, store, dh)) = cfg_journal_store() else {
        eprintln!("doover: cannot open journal/store");
        return 1;
    };
    // manual gc enforces the same env-driven budgets as the automatic trigger
    let budget = doover_core::maintenance::MaintenanceBudget::from_env();
    match doover_core::maintenance::gc(
        &journal,
        &store,
        &dh,
        &doover_core::maintenance::GcOptions {
            keep_days,
            dry_run,
            cap_bytes: budget.cap_bytes,
            // manual gc is the ONE place deficit-driven eviction may run —
            // the user is looking at the report
            min_free_bytes: budget.min_free_bytes,
            time_budget: None,
        },
    ) {
        Ok(r) => {
            let verb = if r.dry_run { "would free" } else { "freed" };
            println!(
                "{verb} {} object(s), {} KiB; pruned {} action(s), {} session(s); {} tmp",
                r.objects_removed,
                r.bytes_freed / 1024,
                r.actions_pruned,
                r.sessions_pruned,
                r.tmp_removed,
            );
            if r.over_cap_bytes_before > 0 {
                println!(
                    "store over its size cap by {} KiB before this pass",
                    r.over_cap_bytes_before / 1024
                );
            }
            if r.free_deficit_bytes_before > 0 {
                println!(
                    "free space {} KiB below the floor before this pass",
                    r.free_deficit_bytes_before / 1024
                );
            }
            if r.cap_evicted_actions > 0 {
                println!(
                    "evicted {} old action(s) to satisfy the store budget",
                    r.cap_evicted_actions
                );
            }
            // dry-run cannot simulate the iterative eviction pass — say so
            // instead of letting "would free" read as the whole story
            if r.dry_run && (r.over_cap_bytes_before > 0 || r.free_deficit_bytes_before > 0) {
                println!(
                    "note: a real gc would ALSO evict oldest unpinned actions until within \
                     budget (eviction is not simulated in dry-run)"
                );
            }
            if r.still_over_budget {
                println!(
                    "warning: still over budget — the rest is pinned or too recent to evict \
                     (raise DOOVER_MAX_STORE_BYTES, unpin, or free disk space)"
                );
            }
            0
        }
        Err(e) => {
            eprintln!("doover: gc failed: {e}");
            1
        }
    }
}

fn run_status() -> i32 {
    let Some((journal, store, dh)) = cfg_journal_store() else {
        eprintln!("doover: cannot open journal/store");
        return 1;
    };
    let (sessions, per_status) = match journal.stats() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("doover: {e}");
            return 1;
        }
    };
    // never report an unreadable store as "0 objects" — say so
    let objects = match store.object_count() {
        Ok(n) => n.to_string(),
        Err(e) => format!("unreadable ({e})"),
    };
    println!("doover home:  {}", dh.display());
    println!("sessions:     {sessions}");
    print!("actions:      ");
    if per_status.is_empty() {
        println!("none");
    } else {
        println!(
            "{}",
            per_status
                .iter()
                .map(|(s, n)| format!("{n} {s}"))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    println!("store objects: {objects}");
    0
}

fn run_doctor() -> i32 {
    let cfg = doover_core::hooks::HookConfig::from_env();
    let mut problems = 0;
    println!("doover doctor");

    // 1. doover home writable
    match std::fs::create_dir_all(&cfg.doover_home) {
        Ok(_) => println!(
            "  [ok]   doover home writable: {}",
            cfg.doover_home.display()
        ),
        Err(e) => {
            println!("  [FAIL] doover home {}: {e}", cfg.doover_home.display());
            problems += 1;
        }
    }

    // 2. journal opens / integrity
    match doover_core::journal::Journal::open(&cfg.doover_home.join("journal.db")) {
        Ok(j) => match j.integrity_check() {
            Ok(true) => println!("  [ok]   journal integrity"),
            Ok(false) => {
                println!("  [FAIL] journal integrity check reported problems");
                problems += 1;
            }
            Err(e) => {
                println!("  [FAIL] journal integrity: {e}");
                problems += 1;
            }
        },
        Err(e) => {
            println!("  [FAIL] cannot open journal: {e}");
            problems += 1;
        }
    }

    // 3. store + copy-on-write capability
    match doover_core::snapshot::Store::open(cfg.doover_home.join("store")) {
        Ok(s) => {
            if s.supports_reflink() {
                println!("  [ok]   store supports copy-on-write (fast snapshots)");
            } else {
                println!("  [warn] store filesystem has no reflink; snapshots use full copies");
            }
            // orphaned staging from an interrupted restore
            match doover_core::snapshot::orphaned_staging(&cfg.doover_home.join("store")) {
                Ok(orphans) if !orphans.is_empty() => {
                    println!(
                        "  [warn] {} orphaned restore-staging dir(s) found",
                        orphans.len()
                    );
                }
                _ => {}
            }
        }
        Err(e) => {
            println!("  [FAIL] cannot open store: {e}");
            problems += 1;
        }
    }

    // 4. hooks installed? check the project settings AND the global ones —
    // `init --project` is a first-class install (audit round 12)
    let mut candidates = vec![std::path::PathBuf::from(".claude/settings.json")];
    if let Some(h) = std::env::var_os("HOME") {
        candidates.push(std::path::PathBuf::from(h).join(".claude/settings.json"));
    }
    let installed_at = candidates
        .iter()
        .find(|p| std::fs::read_to_string(p).is_ok_and(|text| text.contains("doover hook pre")));
    match installed_at {
        Some(p) => println!("  [ok]   Claude Code hooks installed ({})", p.display()),
        None => println!("  [warn] Claude Code hooks not found — run `doover init`"),
    }

    if problems == 0 {
        println!("all good");
        0
    } else {
        println!("{problems} problem(s) found");
        1
    }
}

/// PRIME DIRECTIVE OF THE HOOK PATH: never block the agent. Any failure —
/// parse error, journal error, snapshot error, even a panic — degrades to a
/// stderr warning and exit 0. The harness treats a non-(0|2) exit as a
/// non-blocking error anyway; we make the same guarantee deliberately and
/// loudly. Protection gaps are journaled where possible; the one thing
/// doover must never do is turn a safety net into a blocker.
fn run_hook_fail_open(kind: HookCommand) {
    let result = std::panic::catch_unwind(|| run_hook(kind));
    match result {
        Ok(Ok(())) => std::process::exit(0),
        Ok(Err(msg)) => {
            eprintln!("doover: hook error (fail-open, agent not blocked): {msg}");
            std::process::exit(0);
        }
        Err(_) => {
            eprintln!("doover: hook panicked (fail-open, agent not blocked)");
            std::process::exit(0);
        }
    }
}

fn run_hook(kind: HookCommand) -> Result<(), String> {
    // test hook for the S8 fail-open e2e: prove even a panic cannot block
    if std::env::var_os("DOOVER_TEST_PANIC").is_some() {
        panic!("DOOVER_TEST_PANIC");
    }

    let mut input = String::new();
    std::io::stdin()
        .take(MAX_EVENT_BYTES)
        .read_to_string(&mut input)
        .map_err(|e| format!("reading event: {e}"))?;

    let cfg = doover_core::hooks::HookConfig::from_env();
    match kind {
        HookCommand::Pre => {
            let ev = doover_core::hooks::parse_pre_event(&input).map_err(|e| e.to_string())?;
            let outcome = doover_core::hooks::handle_pre(&cfg, &ev).map_err(|e| e.to_string())?;
            // Never block (exit 0), but never stay silent about a destructive
            // action we could not fully protect (audit round 9): the whole
            // point is the safety net — a hole in it must be loud.
            if outcome.needs_warning() {
                eprintln!(
                    "doover: PROTECTION INCOMPLETE for a {:?} action — undo may not fully recover:",
                    outcome.severity
                );
                for gap in &outcome.gaps {
                    eprintln!("doover:   {gap}");
                }
            }
        }
        HookCommand::Post => {
            let ev = doover_core::hooks::parse_post_event(&input).map_err(|e| e.to_string())?;
            doover_core::hooks::handle_post(&cfg, &ev).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}
