//! T2 — registry test suite (doover-implementation-plan.md §3).
//! Written before the registry module exists; drives its design.

use doover_core::registry::{Effect, Registry, UndoStrategy};

// --- shipped data validity ---------------------------------------------------

#[test]
fn shipped_registry_parses_with_at_least_20_rules() {
    let r = Registry::builtin().expect("shipped registry must always be valid");
    assert!(r.len() >= 20, "shipped registry has only {} rules", r.len());
}

#[test]
fn shipped_ids_are_namespaced() {
    let r = Registry::builtin().unwrap();
    for rule in r.rules() {
        assert!(
            rule.id.contains('.'),
            "rule id `{}` must be namespaced like `coreutils.rm`",
            rule.id
        );
    }
}

#[test]
fn every_destructive_or_irreversible_rule_has_an_undo_strategy() {
    let r = Registry::builtin().unwrap();
    for rule in r.rules() {
        if matches!(rule.effect, Effect::Destructive | Effect::Irreversible) {
            assert_eq!(
                rule.undo,
                UndoStrategy::SnapshotRestore,
                "rule `{}` destroys data but declares no snapshot strategy",
                rule.id
            );
        }
    }
}

// --- schema validation -------------------------------------------------------

#[test]
fn missing_required_field_fails_loudly() {
    let doc = "rules:\n  - id: bad.rule\n    match: { command: foo }\n    effect: destructive\n";
    let err = Registry::parse_rules("inline.yaml", doc).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("undo"),
        "error must name the missing field, got: {msg}"
    );
}

#[test]
fn match_must_have_exactly_one_of_command_or_redirect() {
    let both = "rules:\n  - id: bad.both\n    match: { command: foo, redirect: \">\" }\n    effect: safe\n    undo: none\n";
    assert!(Registry::parse_rules("inline.yaml", both).is_err());
    let neither = "rules:\n  - id: bad.neither\n    match: {}\n    effect: safe\n    undo: none\n";
    assert!(Registry::parse_rules("inline.yaml", neither).is_err());
}

#[test]
fn duplicate_ids_are_rejected() {
    let doc = "rules:\n  - id: dup.x\n    match: { command: a }\n    effect: safe\n    undo: none\n  - id: dup.x\n    match: { command: b }\n    effect: safe\n    undo: none\n";
    let rules = Registry::parse_rules("inline.yaml", doc).unwrap();
    assert!(
        Registry::from_rules(rules).is_err(),
        "duplicate ids must be rejected"
    );
}

// --- lookup ------------------------------------------------------------------

#[test]
fn lookup_specificity_prefers_subcommand_and_flags() {
    let r = Registry::builtin().unwrap();
    let push = r
        .lookup_command("git", Some("push"), &[])
        .expect("git.push");
    assert_eq!(push.id, "git.push");
    assert_eq!(push.effect, Effect::Externalizing);

    let hard = r
        .lookup_command("git", Some("reset"), &["--hard".into()])
        .expect("git.reset-hard");
    assert_eq!(hard.id, "git.reset-hard");
    assert_eq!(hard.effect, Effect::Destructive);

    // plain `git reset` (no --hard) matches nothing shipped → caller treats as unknown
    assert!(r.lookup_command("git", Some("reset"), &[]).is_none());
}

#[test]
fn lookup_unknown_command_is_none() {
    let r = Registry::builtin().unwrap();
    assert!(r.lookup_command("frobnicate", None, &[]).is_none());
}

#[test]
fn eq_attached_long_flag_matches_flags_any() {
    // audit finding: `sort --output=out.txt` must match posix.sort-o even
    // though the flag token carries its value
    let r = Registry::builtin().unwrap();
    let rule = r
        .lookup_command("sort", None, &["--output=out.txt".into()])
        .expect("--output=x must match");
    assert_eq!(rule.id, "posix.sort-o");
    assert_eq!(rule.effect, Effect::Destructive);
}

#[test]
fn redirect_lookup_distinguishes_truncate_from_append() {
    let r = Registry::builtin().unwrap();
    assert_eq!(r.lookup_redirect(">").unwrap().effect, Effect::Destructive);
    assert_eq!(r.lookup_redirect(">>").unwrap().effect, Effect::Mutating);
}

// --- user overlay ------------------------------------------------------------

fn overlay_dir(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for (name, content) in files {
        std::fs::write(dir.path().join(name), content).unwrap();
    }
    dir
}

#[test]
fn overlay_adds_new_rules_and_overrides_by_id() {
    let dir = overlay_dir(&[(
        "custom.yaml",
        "rules:\n  - id: my.tool\n    match: { command: mytool }\n    effect: destructive\n    scope: { paths: positional }\n    undo: snapshot-restore\n  - id: net.curl\n    match: { command: curl }\n    effect: irreversible\n    undo: none\n",
    )]);
    let (r, warnings) = Registry::with_overlay(dir.path()).unwrap();
    assert!(
        warnings.is_empty(),
        "clean overlay must not warn: {warnings:?}"
    );
    assert_eq!(r.lookup_command("mytool", None, &[]).unwrap().id, "my.tool");
    // upgrade (externalizing → irreversible) is allowed
    assert_eq!(
        r.lookup_command("curl", None, &[]).unwrap().effect,
        Effect::Irreversible
    );
}

#[test]
fn overlay_cannot_silently_downgrade_a_shipped_destructive_rule() {
    let dir = overlay_dir(&[(
        "custom.yaml",
        "rules:\n  - id: coreutils.rm\n    match: { command: rm }\n    effect: safe\n    undo: none\n",
    )]);
    let (r, warnings) = Registry::with_overlay(dir.path()).unwrap();
    // shipped classification survives…
    assert_eq!(
        r.lookup_command("rm", None, &[]).unwrap().effect,
        Effect::Destructive
    );
    // …and the rejection is loud.
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("downgrade") && w.contains("coreutils.rm")),
        "expected a downgrade warning naming the rule, got: {warnings:?}"
    );
}

#[test]
fn overlay_cannot_downgrade_externalizing_either() {
    // audit finding: exfiltration-relevant classifications deserve the same
    // downgrade guard as destructive ones
    let dir = overlay_dir(&[(
        "custom.yaml",
        "rules:\n  - id: git.push\n    match: { command: git, subcommand: push }\n    effect: safe\n    undo: none\n",
    )]);
    let (r, warnings) = Registry::with_overlay(dir.path()).unwrap();
    assert_eq!(
        r.lookup_command("git", Some("push"), &[]).unwrap().effect,
        Effect::Externalizing
    );
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("downgrade") && w.contains("git.push")),
        "expected downgrade warning, got: {warnings:?}"
    );
}

#[test]
fn malformed_overlay_file_is_skipped_with_warning_not_a_crash() {
    let dir = overlay_dir(&[
        ("broken.yaml", "rules: [ this is not valid yaml ::: }"),
        (
            "good.yaml",
            "rules:\n  - id: my.ok\n    match: { command: oktool }\n    effect: safe\n    undo: none\n",
        ),
    ]);
    let (r, warnings) = Registry::with_overlay(dir.path()).unwrap();
    assert!(warnings.iter().any(|w| w.contains("broken.yaml")));
    assert!(
        r.lookup_command("oktool", None, &[]).is_some(),
        "valid file still loads"
    );
    assert!(
        r.lookup_command("rm", None, &[]).is_some(),
        "builtin rules intact"
    );
}

#[test]
fn missing_overlay_dir_is_fine() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("does-not-exist");
    let (r, warnings) = Registry::with_overlay(&missing).unwrap();
    assert!(warnings.is_empty());
    assert_eq!(r.len(), Registry::builtin().unwrap().len());
}

// --- golden classification matrix (insta) ------------------------------------

#[test]
fn classification_matrix_snapshot() {
    let r = Registry::builtin().unwrap();
    let mut lines: Vec<String> = r
        .rules()
        .map(|rule| format!("{} → {:?} | {:?}", rule.id, rule.effect, rule.undo))
        .collect();
    lines.sort();
    insta::assert_snapshot!(lines.join("\n"));
}
