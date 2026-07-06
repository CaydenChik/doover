//! Bash differential oracle: real bash is the ground truth for word expansion
//! (brace/tilde/glob/quoting). Expansion via `printf '%s\n'` executes nothing.
//!
//! Contract per word W:
//! - bash:     cd <jail>; HOME=<fixture-home>; printf '%s\n' W
//! - resolver: resolve("rm -- W")  →  paths
//! - if the resolver sets has_unknown, strict comparison is skipped (claiming
//!   uncertainty is allowed); otherwise path sets must be identical.

use doover_core::registry::Registry;
use doover_core::resolver::{Ctx, normalize_lexical, resolve};
use serde::Deserialize;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Deserialize)]
struct OracleFile {
    words: Vec<OracleWord>,
}

#[derive(Deserialize)]
struct OracleWord {
    word: String,
    #[serde(default)]
    cwd_fixture: Vec<String>,
}

fn bash_expand(word: &str, cwd: &Path, home: &Path) -> Vec<String> {
    let output = Command::new("bash")
        .arg("--noprofile")
        .arg("--norc")
        .arg("-c")
        .arg(format!("printf '%s\\n' {word}"))
        .current_dir(cwd)
        .env_clear()
        .env("HOME", home)
        .env("PATH", "/usr/bin:/bin")
        .output()
        .expect("bash must be runnable");
    assert!(
        output.status.success(),
        "bash failed on {word:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

fn to_abs(raw: &str, cwd: &Path) -> PathBuf {
    let p = Path::new(raw);
    if p.is_absolute() {
        normalize_lexical(p)
    } else {
        normalize_lexical(&cwd.join(raw))
    }
}

#[test]
fn resolver_expansion_matches_bash() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/corpus/oracle/words.yaml");
    let doc: OracleFile = serde_yaml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert!(doc.words.len() >= 25, "oracle word list has shrunk");

    let registry = Registry::builtin().unwrap();
    let mut failures = Vec::new();
    let mut compared = 0usize;

    for entry in &doc.words {
        let jail = tempfile::tempdir().unwrap();
        let cwd = normalize_lexical(jail.path());
        let home = cwd.join("home");
        std::fs::create_dir_all(&home).unwrap();
        for fx in &entry.cwd_fixture {
            let p = cwd.join(fx);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, "fixture").unwrap();
        }

        let bash: BTreeSet<PathBuf> = bash_expand(&entry.word, &cwd, &home)
            .iter()
            .map(|l| to_abs(l, &cwd))
            .collect();

        let ctx = Ctx {
            cwd: &cwd,
            home: &home,
        };
        let r = resolve(&format!("rm -- {}", entry.word), &registry, &ctx);
        if r.has_unknown {
            continue; // uncertainty is always an acceptable answer
        }
        compared += 1;
        let ours: BTreeSet<PathBuf> = r.paths.iter().cloned().collect();
        if ours != bash {
            failures.push(format!(
                "word {:?}\n    bash: {:?}\n    ours: {:?}",
                entry.word,
                bash.iter()
                    .map(|p| p.strip_prefix(&cwd).unwrap_or(p))
                    .collect::<Vec<_>>(),
                ours.iter()
                    .map(|p| p.strip_prefix(&cwd).unwrap_or(p))
                    .collect::<Vec<_>>(),
            ));
        }
    }

    assert!(
        compared >= 20,
        "oracle only strictly compared {compared} words — too many hid behind has_unknown"
    );
    assert!(
        failures.is_empty(),
        "{} word(s) diverge from bash:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
