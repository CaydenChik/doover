use clap::{Parser, Subcommand};
use std::io::Read;

/// Exit code for commands that exist in the CLI surface but are not yet
/// implemented. Distinct from 1 (runtime error) and 2 (hook block decision)
/// so tests and scripts can tell "not built yet" from "failed".
const EXIT_NOT_IMPLEMENTED: i32 = 64;

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
    /// Set up the snapshot store and install harness hooks
    Init,
    /// List recent journaled agent actions
    Log {
        /// How many actions to show
        #[arg(short = 'n', long, default_value_t = 20)]
        limit: i64,
    },
    /// Show one action's snapshot manifest and diff
    Show,
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
    Diff,
    /// Store and session health summary
    Status,
    /// Apply the retention policy to the snapshot store
    Gc,
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
    let (name, step) = match cli.command {
        Command::Hook(kind) => {
            run_hook_fail_open(kind);
            return;
        }
        Command::Undo { id, force, dry_run } => {
            run_undo_redo(Verb::Undo, id, force, dry_run);
            return;
        }
        Command::Redo { id, force, dry_run } => {
            run_undo_redo(Verb::Redo, id, force, dry_run);
            return;
        }
        Command::Log { limit } => {
            run_log(limit);
            return;
        }
        Command::Init => ("init", 7),
        Command::Show => ("show", 6),
        Command::Diff => ("diff", 6),
        Command::Status => ("status", 7),
        Command::Gc => ("gc", 7),
        Command::Doctor => ("doctor", 7),
    };
    eprintln!(
        "doover {name}: not implemented yet (arrives in build step {step}; see doover-implementation-plan.md)"
    );
    std::process::exit(EXIT_NOT_IMPLEMENTED);
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
        let mut cmd = a.raw_command.replace('\n', " ");
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
