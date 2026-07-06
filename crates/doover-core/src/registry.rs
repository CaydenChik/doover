//! Reversibility registry: classifies commands and shell constructs by effect,
//! affected-path scope, and undo strategy. Data lives in `registry/*.yaml`
//! (CC0); a user overlay directory can add rules or *upgrade* severity, but a
//! shipped destructive classification can never be silently downgraded.

use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

/// Effect classes ordered by severity: later variants are strictly more
/// dangerous. `Ord` is load-bearing — the overlay downgrade check relies on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effect {
    Safe,
    Mutating,
    Externalizing,
    Destructive,
    Irreversible,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum UndoStrategy {
    SnapshotRestore,
    None,
    Recompute,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PathSource {
    Positional,
    PositionalLast,
    RedirectTarget,
    Repo,
    None,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScopeSpec {
    pub paths: PathSource,
    #[serde(default = "default_true")]
    pub globs: bool,
    /// Leading positional arguments that are not paths (sed scripts, chmod
    /// modes) and must be dropped before scope extraction.
    #[serde(default)]
    pub skip: usize,
    /// Flags that consume the following argument (`truncate -s 0`): that
    /// argument is not a path and must not enter the scope.
    #[serde(default)]
    pub flag_args: Vec<String>,
    /// Flags whose value *is* a target path to snapshot, in either the
    /// separate (`-o out.txt`) or attached (`--output=out.txt`) form.
    #[serde(default)]
    pub path_flags: Vec<String>,
    #[serde(default)]
    pub recursive_flags: Vec<String>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MatchSpec {
    pub command: Option<String>,
    pub subcommand: Option<String>,
    pub flags_any: Option<Vec<String>>,
    pub redirect: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    pub id: String,
    #[serde(rename = "match")]
    pub matcher: MatchSpec,
    pub effect: Effect,
    #[serde(default)]
    pub scope: Option<ScopeSpec>,
    pub undo: UndoStrategy,
    #[serde(default)]
    pub notes: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RegistryFile {
    rules: Vec<Rule>,
}

#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("failed to parse {file}: {source}")]
    Parse {
        file: String,
        #[source]
        source: serde_yaml::Error,
    },
    #[error("invalid rule `{id}` in {file}: {reason}")]
    InvalidRule {
        file: String,
        id: String,
        reason: String,
    },
    #[error("duplicate rule id `{id}`")]
    DuplicateId { id: String },
    #[error("failed to read {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

/// Shipped registry data, embedded at compile time.
const SHIPPED: &[(&str, &str)] = &[
    ("coreutils.yaml", include_str!("../registry/coreutils.yaml")),
    ("shell.yaml", include_str!("../registry/shell.yaml")),
    ("git.yaml", include_str!("../registry/git.yaml")),
    ("net.yaml", include_str!("../registry/net.yaml")),
    ("posix.yaml", include_str!("../registry/posix.yaml")),
];

pub struct Registry {
    rules: Vec<Rule>,
    index: HashMap<String, usize>,
}

impl Registry {
    /// Parse and validate the rules of one YAML document. Public so tests and
    /// future tooling (registry linters, the doctor command) can reuse it.
    pub fn parse_rules(file: &str, contents: &str) -> Result<Vec<Rule>, RegistryError> {
        let parsed: RegistryFile =
            serde_yaml::from_str(contents).map_err(|source| RegistryError::Parse {
                file: file.to_string(),
                source,
            })?;
        for rule in &parsed.rules {
            validate(file, rule)?;
        }
        Ok(parsed.rules)
    }

    /// Build a registry from already-parsed rules, rejecting duplicate ids.
    pub fn from_rules(rules: Vec<Rule>) -> Result<Self, RegistryError> {
        let mut index = HashMap::with_capacity(rules.len());
        for (i, rule) in rules.iter().enumerate() {
            if index.insert(rule.id.clone(), i).is_some() {
                return Err(RegistryError::DuplicateId {
                    id: rule.id.clone(),
                });
            }
        }
        Ok(Self { rules, index })
    }

    /// The shipped registry. Must always be valid — a failure here is a bug
    /// caught by the T2 suite, never a runtime condition.
    pub fn builtin() -> Result<Self, RegistryError> {
        let mut rules = Vec::new();
        for (file, contents) in SHIPPED {
            rules.extend(Self::parse_rules(file, contents)?);
        }
        Self::from_rules(rules)
    }

    /// Builtin rules plus a user overlay directory (`*.yaml`, lexical order).
    /// Overlay problems are warnings, never failures: a broken user file must
    /// not take the safety net down with it. Severity downgrades of shipped
    /// destructive/irreversible rules are rejected loudly.
    pub fn with_overlay(dir: &Path) -> Result<(Self, Vec<String>), RegistryError> {
        let mut registry = Self::builtin()?;
        let mut warnings = Vec::new();

        if !dir.is_dir() {
            return Ok((registry, warnings));
        }
        let mut entries: Vec<_> = std::fs::read_dir(dir)
            .map_err(|source| RegistryError::Io {
                path: dir.display().to_string(),
                source,
            })?
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| {
                p.extension()
                    .is_some_and(|ext| ext == "yaml" || ext == "yml")
            })
            .collect();
        entries.sort();

        for path in entries {
            let file = path.display().to_string();
            let contents = match std::fs::read_to_string(&path) {
                Ok(c) => c,
                Err(e) => {
                    warnings.push(format!("skipping {file}: {e}"));
                    continue;
                }
            };
            let rules = match Self::parse_rules(&file, &contents) {
                Ok(r) => r,
                Err(e) => {
                    warnings.push(format!("skipping {file}: {e}"));
                    continue;
                }
            };
            for rule in rules {
                registry.insert_overlay(rule, &mut warnings);
            }
        }
        Ok((registry, warnings))
    }

    fn insert_overlay(&mut self, rule: Rule, warnings: &mut Vec<String>) {
        match self.index.get(&rule.id) {
            Some(&i) => {
                let shipped = &self.rules[i];
                // never let a user overlay quietly weaken a safety-relevant
                // shipped classification. Both data-loss (destructive/
                // irreversible) and exfiltration (externalizing) qualify —
                // downgrading either could disable a protection the user relies
                // on without them noticing.
                if is_protected(shipped.effect) && rule.effect < shipped.effect {
                    warnings.push(format!(
                        "refusing to downgrade `{}` from {:?} to {:?}; overlay rule ignored",
                        rule.id, shipped.effect, rule.effect
                    ));
                    return;
                }
                self.rules[i] = rule;
            }
            None => {
                self.index.insert(rule.id.clone(), self.rules.len());
                self.rules.push(rule);
            }
        }
    }

    /// Most-specific match for a simple command: a rule with a matching
    /// subcommand outranks a bare-command rule, and a matching `flags_any`
    /// outranks both. Rules with a subcommand or flags requirement that the
    /// invocation doesn't satisfy don't match at all.
    pub fn lookup_command(
        &self,
        command: &str,
        subcommand: Option<&str>,
        flags: &[String],
    ) -> Option<&Rule> {
        self.rules
            .iter()
            .filter_map(|rule| {
                let m = &rule.matcher;
                if m.command.as_deref() != Some(command) {
                    return None;
                }
                let mut score = 1usize;
                if let Some(want) = m.subcommand.as_deref() {
                    if subcommand != Some(want) {
                        return None;
                    }
                    score += 2;
                }
                if let Some(want_any) = &m.flags_any {
                    if !want_any
                        .iter()
                        .any(|w| flags.iter().any(|f| flag_matches(f, w)))
                    {
                        return None;
                    }
                    score += 4;
                }
                Some((score, rule))
            })
            .max_by(|(sa, ra), (sb, rb)| sa.cmp(sb).then_with(|| rb.id.cmp(&ra.id)))
            .map(|(_, rule)| rule)
    }

    pub fn lookup_redirect(&self, op: &str) -> Option<&Rule> {
        self.rules
            .iter()
            .find(|rule| rule.matcher.redirect.as_deref() == Some(op))
    }

    pub fn rules(&self) -> impl Iterator<Item = &Rule> {
        self.rules.iter()
    }

    pub fn len(&self) -> usize {
        self.rules.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }
}

/// Whether an observed flag token satisfies a wanted flag. Exact match, or the
/// `--long=value` form of a `--long` want (`--output=x` matches `--output`).
fn flag_matches(observed: &str, want: &str) -> bool {
    observed == want
        || (want.starts_with("--")
            && observed
                .split_once('=')
                .is_some_and(|(name, _)| name == want))
}

/// Safety-relevant classifications an overlay may not silently weaken:
/// externalizing (exfiltration) and above (data loss).
fn is_protected(effect: Effect) -> bool {
    effect >= Effect::Externalizing
}

fn validate(file: &str, rule: &Rule) -> Result<(), RegistryError> {
    let fail = |reason: &str| {
        Err(RegistryError::InvalidRule {
            file: file.to_string(),
            id: rule.id.clone(),
            reason: reason.to_string(),
        })
    };
    if rule.id.trim().is_empty() {
        return fail("empty id");
    }
    let m = &rule.matcher;
    match (&m.command, &m.redirect) {
        (Some(_), Some(_)) => {
            return fail("match must have exactly one of `command` or `redirect`, not both");
        }
        (None, None) => return fail("match must have one of `command` or `redirect`"),
        (None, Some(_)) if m.subcommand.is_some() || m.flags_any.is_some() => {
            return fail("`subcommand`/`flags_any` require `command`");
        }
        _ => {}
    }
    Ok(())
}
