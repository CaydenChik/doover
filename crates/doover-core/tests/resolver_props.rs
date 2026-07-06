//! T1 property tests: P1 never panics, P2 opaque constructs are never safe,
//! P3 resolution is deterministic.

use doover_core::registry::Registry;
use doover_core::resolver::{Ctx, resolve};
use proptest::prelude::*;

fn ctx_dirs() -> (tempfile::TempDir, std::path::PathBuf) {
    let jail = tempfile::tempdir().unwrap();
    let home = jail.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    (jail, home)
}

proptest! {
    /// P1 — arbitrary input (including newlines, quotes, unicode) never panics.
    #[test]
    fn p1_never_panics(input in ".{0,160}") {
        let registry = Registry::builtin().unwrap();
        let (jail, home) = ctx_dirs();
        let ctx = Ctx { cwd: jail.path(), home: &home };
        let _ = resolve(&input, &registry, &ctx);
    }

    /// P2 — a statement containing an opaque construct must set has_unknown,
    /// wherever it sits in the line.
    #[test]
    fn p2_opaque_is_never_silently_safe(
        prefix in prop::sample::select(vec!["", "ls; ", "rm -rf ./a && ", "echo hi | "]),
        construct in prop::sample::select(vec![
            "eval \"$X\"",
            "$(rm -rf ./x)",
            "`rm -rf ./x`",
            "bash -c 'rm -rf ./x'",
            "sh -c \"rm x\"",
            "zsh -c 'rm x'",
            "source ./setup.sh",
            ". ./env.sh",
            "xargs rm",
            "sudo rm -rf ./x",
            "rm -rf $TARGET",
            "$CLEANER ./data",
        ]),
        suffix in prop::sample::select(vec!["", " ; ls", " && echo done"]),
    ) {
        let registry = Registry::builtin().unwrap();
        let (jail, home) = ctx_dirs();
        let ctx = Ctx { cwd: jail.path(), home: &home };
        let input = format!("{prefix}{construct}{suffix}");
        let r = resolve(&input, &registry, &ctx);
        prop_assert!(
            r.has_unknown,
            "opaque construct classified without unknown flag: {input:?} → {r:?}"
        );
    }

    /// P3 — same input, same context ⇒ identical resolution.
    #[test]
    fn p3_deterministic(input in ".{0,120}") {
        let registry = Registry::builtin().unwrap();
        let (jail, home) = ctx_dirs();
        let ctx = Ctx { cwd: jail.path(), home: &home };
        let a = resolve(&input, &registry, &ctx);
        let b = resolve(&input, &registry, &ctx);
        prop_assert_eq!(a, b);
    }
}
