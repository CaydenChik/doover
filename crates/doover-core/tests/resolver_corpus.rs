//! T1 — data-driven scope-resolver corpus (doover-implementation-plan.md §3).
//! Cases live in tests/corpus/parser/*.yaml at the workspace root so non-Rust
//! contributors can extend them. Step-2 gate: ≥80 cases, all green.

use doover_core::registry::Registry;
use doover_core::resolver::{Ctx, Resolution, Severity, normalize_lexical, resolve};
use serde::Deserialize;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

#[derive(Deserialize)]
struct CorpusFile {
    cases: Vec<Case>,
}

#[derive(Deserialize)]
struct Case {
    name: String,
    cmd: String,
    #[serde(default)]
    cwd_fixture: Vec<String>,
    expect: Expect,
}

#[derive(Deserialize)]
struct Expect {
    effect: Severity,
    #[serde(default)]
    paths: Option<Vec<String>>,
    #[serde(default)]
    unknown: Option<bool>,
    #[serde(default)]
    rule: Option<String>,
}

fn corpus_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/corpus/parser")
}

fn build_fixture(jail: &Path, entries: &[String]) {
    for entry in entries {
        let path = jail.join(entry);
        if entry.ends_with('/') {
            std::fs::create_dir_all(&path).unwrap();
        } else {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, "fixture").unwrap();
        }
    }
}

/// Corpus path conventions: "~/x" → fixture home, "/x" → absolute literal,
/// "." → the jail cwd, anything else → jail-relative.
fn expected_path(raw: &str, cwd: &Path, home: &Path) -> PathBuf {
    let joined = if raw == "~" {
        home.to_path_buf()
    } else if let Some(rest) = raw.strip_prefix("~/") {
        home.join(rest)
    } else if raw.starts_with('/') {
        PathBuf::from(raw)
    } else {
        cwd.join(raw)
    };
    normalize_lexical(&joined)
}

fn run_case(registry: &Registry, case: &Case) -> Result<(), String> {
    let jail = tempfile::tempdir().unwrap();
    let cwd = normalize_lexical(jail.path());
    let home = cwd.join("home");
    std::fs::create_dir_all(&home).unwrap();
    build_fixture(&cwd, &case.cwd_fixture);

    let ctx = Ctx {
        cwd: &cwd,
        home: &home,
    };
    let r: Resolution = resolve(&case.cmd, registry, &ctx);

    // invariant: an Unknown classification always carries the unknown flag
    if r.severity == Severity::Unknown && !r.has_unknown {
        return Err("severity is unknown but has_unknown is false".into());
    }
    if r.severity != case.expect.effect {
        return Err(format!(
            "effect: expected {:?}, got {:?} (rule {:?}, unknown {})",
            case.expect.effect, r.severity, r.rule_id, r.has_unknown
        ));
    }
    if let Some(expected) = &case.expect.paths {
        let want: BTreeSet<PathBuf> = expected
            .iter()
            .map(|p| expected_path(p, &cwd, &home))
            .collect();
        let got: BTreeSet<PathBuf> = r.paths.iter().cloned().collect();
        if want != got {
            return Err(format!("paths: expected {want:?}, got {got:?}"));
        }
    }
    // strict by default: omitting `unknown` asserts it is false unless the
    // expected effect is itself unknown (audit finding: a spuriously-cleared
    // unknown flag was previously invisible)
    let expected_unknown = case
        .expect
        .unknown
        .unwrap_or(case.expect.effect == Severity::Unknown);
    if r.has_unknown != expected_unknown {
        return Err(format!(
            "unknown flag: expected {expected_unknown}, got {}",
            r.has_unknown
        ));
    }
    if let Some(rule) = &case.expect.rule {
        if r.rule_id.as_deref() != Some(rule.as_str()) {
            return Err(format!("rule: expected {rule:?}, got {:?}", r.rule_id));
        }
    }
    Ok(())
}

#[test]
fn corpus() {
    let registry = Registry::builtin().unwrap();
    let mut files: Vec<PathBuf> = std::fs::read_dir(corpus_dir())
        .expect("corpus dir must exist")
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "yaml"))
        .collect();
    files.sort();
    assert!(!files.is_empty(), "no corpus files found");

    let mut total = 0usize;
    let mut names = BTreeSet::new();
    let mut failures = Vec::new();
    for file in &files {
        let text = std::fs::read_to_string(file).unwrap();
        let doc: CorpusFile = serde_yaml::from_str(&text)
            .unwrap_or_else(|e| panic!("{} is not valid corpus YAML: {e}", file.display()));
        for case in &doc.cases {
            total += 1;
            assert!(
                names.insert(case.name.clone()),
                "duplicate corpus case name `{}`",
                case.name
            );
            if let Err(msg) = run_case(&registry, case) {
                failures.push(format!("[{}] {}", case.name, msg));
            }
        }
    }
    assert!(
        total >= 80,
        "corpus has {total} cases; the step-2 gate is ≥80"
    );
    assert!(
        failures.is_empty(),
        "{}/{} corpus cases failed:\n{}",
        failures.len(),
        total,
        failures.join("\n")
    );
}
