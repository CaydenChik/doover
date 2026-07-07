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
    /// List journaled agent actions
    Log,
    /// Show one action's snapshot manifest and diff
    Show,
    /// Restore pre-action state (session-scoped, selective)
    Undo,
    /// Revert an undo
    Redo,
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
        Command::Init => ("init", 7),
        Command::Log => ("log", 6),
        Command::Show => ("show", 6),
        Command::Undo => ("undo", 6),
        Command::Redo => ("redo", 6),
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
            doover_core::hooks::handle_pre(&cfg, &ev).map_err(|e| e.to_string())?;
        }
        HookCommand::Post => {
            let ev = doover_core::hooks::parse_post_event(&input).map_err(|e| e.to_string())?;
            doover_core::hooks::handle_post(&cfg, &ev).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}
