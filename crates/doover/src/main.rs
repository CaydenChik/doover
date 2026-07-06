use clap::{Parser, Subcommand};

/// Exit code for commands that exist in the CLI surface but are not yet
/// implemented. Distinct from 1 (runtime error) and 2 (hook block decision)
/// so tests and scripts can tell "not built yet" from "failed".
const EXIT_NOT_IMPLEMENTED: i32 = 64;

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
    /// PreToolUse: classify, snapshot, decide
    Pre,
    /// PostToolUse: record outcome, post-hashes
    Post,
}

fn main() {
    let cli = Cli::parse();
    let (name, step) = match cli.command {
        Command::Init => ("init", 7),
        Command::Log => ("log", 6),
        Command::Show => ("show", 6),
        Command::Undo => ("undo", 6),
        Command::Redo => ("redo", 6),
        Command::Diff => ("diff", 6),
        Command::Status => ("status", 7),
        Command::Gc => ("gc", 7),
        Command::Doctor => ("doctor", 7),
        Command::Hook(HookCommand::Pre) => ("hook pre", 5),
        Command::Hook(HookCommand::Post) => ("hook post", 5),
    };
    eprintln!(
        "doover {name}: not implemented yet (arrives in build step {step}; see doover-implementation-plan.md)"
    );
    std::process::exit(EXIT_NOT_IMPLEMENTED);
}
