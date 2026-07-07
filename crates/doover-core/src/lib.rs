//! doover-core — snapshot, journal, and undo primitives for AI agent shell actions.
//!
//! Module map (built strictly test-first; see doover-implementation-plan.md):
//! - `registry` (step 1): reversibility classification of commands
//! - `parser` (step 2): bash parsing + affected-path scope resolution
//! - `snapshot` (step 3): content-addressed CoW snapshot store
//! - `journal` (step 4): SQLite action journal
//! - `hooks` (step 5): harness adapters (Claude Code first)
//! - `undo` (step 6): restore engine with conflict detection

pub mod journal;
pub mod registry;
pub mod resolver;
pub mod snapshot;

/// Crate version, single source of truth for the CLI `--version` output.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    #[test]
    fn version_matches_cargo() {
        assert_eq!(super::VERSION, env!("CARGO_PKG_VERSION"));
        assert!(!super::VERSION.is_empty());
    }

    /// Honest-failure canary. With `DOOVER_CI_CANARY=1` this test MUST fail;
    /// the CI job `honesty-canary` runs it that way and passes only if it fails.
    /// This proves, on every CI run, that test failures are actually reported —
    /// the precondition for the project rule "no claim of completion without
    /// green tests" (see CLAUDE.md).
    #[test]
    fn ci_canary() {
        if std::env::var_os("DOOVER_CI_CANARY").is_some() {
            panic!(
                "canary tripped: failure reporting verified (this is the expected outcome under DOOVER_CI_CANARY=1)"
            );
        }
    }
}
