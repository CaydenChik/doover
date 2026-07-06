//! Bash command parsing and affected-path scope resolution.
//!
//! Design invariants (property-tested):
//! - never panics on any input;
//! - anything the resolver cannot fully account for — opaque constructs
//!   (`eval`, `$( )`, `sh -c`, `xargs`, wrappers like `sudo`), unresolvable
//!   variables, parse errors, or a destructive rule that yielded zero concrete
//!   paths — sets `has_unknown`, which routes the action through the engine's
//!   unknown policy instead of silently under-protecting;
//! - resolution is deterministic and purely lexical except for glob expansion
//!   and git-repo-root discovery, which read (never write) the filesystem.

use crate::registry::{Effect, PathSource, Registry, Rule};
use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};
use tree_sitter::Node;

/// Classification severity. Extends registry [`Effect`] with `Unknown`,
/// ordered so that a destructive-with-known-scope part still dominates an
/// unknown part in reporting (the `has_unknown` flag carries the safety
/// obligation separately).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    #[default]
    Safe,
    Mutating,
    Externalizing,
    Unknown,
    Destructive,
    Irreversible,
}

impl From<Effect> for Severity {
    fn from(e: Effect) -> Self {
        match e {
            Effect::Safe => Severity::Safe,
            Effect::Mutating => Severity::Mutating,
            Effect::Externalizing => Severity::Externalizing,
            Effect::Destructive => Severity::Destructive,
            Effect::Irreversible => Severity::Irreversible,
        }
    }
}

pub struct Ctx<'a> {
    pub cwd: &'a Path,
    pub home: &'a Path,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolution {
    pub severity: Severity,
    /// Affected paths, absolute, lexically normalized, sorted, deduplicated.
    pub paths: Vec<PathBuf>,
    /// Registry rule of the highest-severity contribution.
    pub rule_id: Option<String>,
    /// True when any part of the line escaped full accounting.
    pub has_unknown: bool,
}

/// Commands that execute other commands or otherwise defeat static scoping.
/// Wrapper stripping (sudo, env, …) is a future refinement; opaque is safe.
const OPAQUE_COMMANDS: &[&str] = &[
    "eval", "source", ".", "exec", "sh", "bash", "zsh", "dash", "ksh", "fish", "xargs", "sudo",
    "doas", "env", "nohup", "nice", "time", "timeout", "command", "builtin", "script", "watch",
];

const GLOB_CHARS: &[char] = &['*', '?', '['];

pub fn resolve(command: &str, registry: &Registry, ctx: &Ctx) -> Resolution {
    let mut out = Out::default();

    let mut parser = tree_sitter::Parser::new();
    if parser
        .set_language(&tree_sitter_bash::LANGUAGE.into())
        .is_err()
    {
        out.mark_unknown();
        return out.finish();
    }
    let Some(tree) = parser.parse(command, None) else {
        out.mark_unknown();
        return out.finish();
    };
    let root = tree.root_node();
    if root.has_error() {
        out.mark_unknown();
    }

    let mut walker = Walker {
        registry,
        ctx,
        src: command.as_bytes(),
        out,
    };
    let mut cur = Some(normalize_lexical(ctx.cwd));
    walker.walk(root, &mut cur);
    walker.out.finish()
}

#[derive(Default)]
struct Out {
    severity: Severity,
    paths: BTreeSet<PathBuf>,
    rule_id: Option<String>,
    rule_sev: Severity,
    has_unknown: bool,
}

impl Out {
    fn mark_unknown(&mut self) {
        self.severity = self.severity.max(Severity::Unknown);
        self.has_unknown = true;
    }

    fn contribute(&mut self, sev: Severity, rule_id: &str) {
        self.severity = self.severity.max(sev);
        if self.rule_id.is_none() || sev > self.rule_sev {
            self.rule_sev = sev;
            self.rule_id = Some(rule_id.to_string());
        }
    }

    fn finish(self) -> Resolution {
        Resolution {
            severity: self.severity,
            paths: self.paths.into_iter().collect(),
            rule_id: self.rule_id,
            has_unknown: self.has_unknown,
        }
    }
}

enum ArgVal {
    Lit { text: String, quoted: bool },
    Opaque,
}

enum Tok {
    Flag(String),
    Pos(String, bool),
}

struct Walker<'a> {
    registry: &'a Registry,
    ctx: &'a Ctx<'a>,
    src: &'a [u8],
    out: Out,
}

impl Walker<'_> {
    /// `cur` is the tracked working directory; `None` means a `cd` made it
    /// unresolvable and every later relative path is unknown.
    fn walk(&mut self, node: Node, cur: &mut Option<PathBuf>) {
        match node.kind() {
            "comment" => {}
            "variable_assignment" => {
                // FOO=bar is inert, but FOO=$(cmd) executes.
                if contains_kind(node, &["command_substitution"]) {
                    self.out.mark_unknown();
                }
            }
            "command" => self.handle_command(node, cur),
            "redirected_statement" => {
                if let Some(body) = node.child_by_field_name("body") {
                    self.walk(body, cur);
                }
                let mut c = node.walk();
                for child in node.named_children(&mut c) {
                    if child.kind() == "file_redirect" {
                        self.handle_redirect(child, cur);
                    }
                }
            }
            "pipeline" => {
                // each segment runs in its own subshell: isolate cwd changes
                let mut c = node.walk();
                for child in node.named_children(&mut c) {
                    let mut seg_cur = cur.clone();
                    self.walk(child, &mut seg_cur);
                }
            }
            "subshell" => {
                let mut sub_cur = cur.clone();
                let mut c = node.walk();
                for child in node.named_children(&mut c) {
                    self.walk(child, &mut sub_cur);
                }
            }
            _ => {
                if node.is_error() {
                    self.out.mark_unknown();
                    return;
                }
                // generic descent in source order (program, list,
                // negated_command, compound statements, …): commands anywhere
                // below still route through handle_command.
                let mut c = node.walk();
                for child in node.named_children(&mut c) {
                    self.walk(child, cur);
                }
            }
        }
    }

    fn handle_command(&mut self, node: Node, cur: &mut Option<PathBuf>) {
        let Some(name_node) = node.child_by_field_name("name") else {
            self.out.mark_unknown();
            return;
        };
        let Some(name) = self.command_name_text(name_node) else {
            self.out.mark_unknown();
            return;
        };

        // collect argument values (everything after the name node)
        let mut args = Vec::new();
        let mut c = node.walk();
        for child in node.named_children(&mut c) {
            if child.start_byte() < name_node.end_byte() {
                continue;
            }
            args.push(self.extract_arg(child));
        }

        if name == "cd" {
            self.handle_cd(&args, cur);
            return;
        }
        if OPAQUE_COMMANDS.contains(&name.as_str()) {
            self.out.mark_unknown();
            return;
        }

        let mut any_opaque = false;
        let mut tokens: Vec<Tok> = Vec::new();
        let mut after_double_dash = false;
        for arg in &args {
            match arg {
                ArgVal::Opaque => any_opaque = true,
                ArgVal::Lit { text, quoted } => {
                    if !after_double_dash && text == "--" {
                        after_double_dash = true;
                    } else if !after_double_dash && text.len() > 1 && text.starts_with('-') {
                        tokens.push(Tok::Flag(text.clone()));
                    } else {
                        tokens.push(Tok::Pos(text.clone(), *quoted));
                    }
                }
            }
        }
        if any_opaque {
            self.out.mark_unknown();
        }

        let flags: Vec<String> = tokens
            .iter()
            .filter_map(|t| match t {
                Tok::Flag(f) => Some(f.clone()),
                Tok::Pos(..) => None,
            })
            .collect();
        let sub = tokens.iter().find_map(|t| match t {
            Tok::Pos(text, _) => Some(text.as_str()),
            Tok::Flag(_) => None,
        });
        let Some(rule) = self.registry.lookup_command(&name, sub, &flags) else {
            self.out.mark_unknown();
            return;
        };
        let sev = Severity::from(rule.effect);
        self.out.contribute(sev, &rule.id);

        // positionals, minus arguments consumed by value-taking flags
        let flag_args: &[String] = rule
            .scope
            .as_ref()
            .map(|s| s.flag_args.as_slice())
            .unwrap_or(&[]);
        let mut positionals: Vec<(String, bool)> = Vec::new();
        let mut consume_next = false;
        for tok in &tokens {
            match tok {
                Tok::Flag(f) => {
                    if flag_args.iter().any(|fa| fa == f) {
                        consume_next = true;
                    }
                }
                Tok::Pos(text, quoted) => {
                    if consume_next {
                        consume_next = false;
                    } else {
                        positionals.push((text.clone(), *quoted));
                    }
                }
            }
        }

        let contributed = self.extract_scope(rule, &positionals, cur);
        if sev >= Severity::Destructive && contributed == 0 {
            // destructive action with no pre-snapshottable paths: the engine
            // must fall back to the unknown policy rather than claim coverage
            self.out.mark_unknown();
        }
    }

    fn extract_scope(
        &mut self,
        rule: &Rule,
        positionals: &[(String, bool)],
        cur: &Option<PathBuf>,
    ) -> usize {
        let Some(scope) = &rule.scope else { return 0 };
        let skip = usize::from(rule.matcher.subcommand.is_some()) + scope.skip;
        let path_args: Vec<&(String, bool)> = positionals.iter().skip(skip).collect();
        let mut contributed = 0usize;
        match scope.paths {
            PathSource::Positional => {
                for (text, quoted) in path_args.iter().copied() {
                    contributed += self.add_path(text, *quoted, scope.globs, cur);
                }
            }
            PathSource::PositionalLast => {
                if let Some((text, quoted)) = path_args.last().copied() {
                    contributed += self.add_path(text, *quoted, scope.globs, cur);
                }
            }
            PathSource::Repo => {
                if let Some(root) = cur.as_deref().and_then(find_repo_root) {
                    self.out.paths.insert(root);
                    contributed += 1;
                }
            }
            PathSource::RedirectTarget | PathSource::None => {}
        }
        contributed
    }

    /// Resolve one path argument (tilde, cwd join, normalize, optional glob
    /// expansion) and record the results. Returns how many paths were added.
    fn add_path(&mut self, text: &str, quoted: bool, globs: bool, cur: &Option<PathBuf>) -> usize {
        let Some(resolved) = self.resolve_path(text, cur) else {
            return 0;
        };
        if !quoted && globs && text.contains(GLOB_CHARS) {
            let pattern = resolved.to_string_lossy().into_owned();
            match glob::glob(&pattern) {
                Ok(matches) => {
                    let mut n = 0usize;
                    for m in matches.take(10_000).flatten() {
                        self.out.paths.insert(normalize_lexical(&m));
                        n += 1;
                    }
                    n
                }
                Err(_) => {
                    self.out.paths.insert(resolved);
                    1
                }
            }
        } else {
            self.out.paths.insert(resolved);
            1
        }
    }

    fn resolve_path(&mut self, text: &str, cur: &Option<PathBuf>) -> Option<PathBuf> {
        let joined = if text == "~" {
            self.ctx.home.to_path_buf()
        } else if let Some(rest) = text.strip_prefix("~/") {
            self.ctx.home.join(rest)
        } else if text.starts_with('~') {
            // ~otheruser — unsupported
            self.out.mark_unknown();
            return None;
        } else if Path::new(text).is_absolute() {
            PathBuf::from(text)
        } else {
            match cur {
                Some(dir) => dir.join(text),
                None => {
                    self.out.mark_unknown();
                    return None;
                }
            }
        };
        Some(normalize_lexical(&joined))
    }

    fn handle_cd(&mut self, args: &[ArgVal], cur: &mut Option<PathBuf>) {
        let target = args.iter().find(|a| match a {
            ArgVal::Lit { text, .. } => !(text.len() > 1 && text.starts_with('-')) && text != "--",
            ArgVal::Opaque => true,
        });
        match target {
            None => *cur = Some(normalize_lexical(self.ctx.home)),
            Some(ArgVal::Lit { text, .. }) if text == "-" => {
                // previous directory is untracked
                self.out.mark_unknown();
                *cur = None;
            }
            Some(ArgVal::Lit { text, .. }) => match self.resolve_path(text, cur) {
                Some(p) => *cur = Some(p),
                None => *cur = None,
            },
            Some(ArgVal::Opaque) => {
                self.out.mark_unknown();
                *cur = None;
            }
        }
    }

    fn handle_redirect(&mut self, node: Node, cur: &Option<PathBuf>) {
        // operator is an unnamed token child; input-only redirects are safe
        let mut op: Option<String> = None;
        let mut c = node.walk();
        for child in node.children(&mut c) {
            if !child.is_named() && child.kind().contains('>') {
                op = Some(child.kind().to_string());
                break;
            }
        }
        let Some(op) = op else { return };

        let dest = node
            .child_by_field_name("destination")
            .or_else(|| node.named_children(&mut node.walk()).last());
        let Some(dest) = dest else {
            self.out.mark_unknown();
            return;
        };
        // fd targets (>&2) have no path
        if matches!(dest.kind(), "number" | "file_descriptor") {
            return;
        }
        let text = match self.extract_arg(dest) {
            ArgVal::Lit { text, .. } => text,
            ArgVal::Opaque => {
                self.out.mark_unknown();
                return;
            }
        };
        if text.chars().all(|ch| ch.is_ascii_digit()) {
            return;
        }

        let rule = self.registry.lookup_redirect(&op).or_else(|| {
            // fallback for &>, >|, &>> …: append-family keeps append semantics
            let fallback = if op.contains(">>") { ">>" } else { ">" };
            self.registry.lookup_redirect(fallback)
        });
        let Some(rule) = rule else {
            self.out.mark_unknown();
            return;
        };
        self.out.contribute(Severity::from(rule.effect), &rule.id);
        if let Some(p) = self.resolve_path(&text, cur) {
            self.out.paths.insert(p);
        }
    }

    fn command_name_text(&mut self, name_node: Node) -> Option<String> {
        let inner = name_node.named_child(0).unwrap_or(name_node);
        match inner.kind() {
            "word" | "number" => self.node_text(inner).map(|t| unescape_word(&t)),
            _ => None,
        }
    }

    fn extract_arg(&mut self, node: Node) -> ArgVal {
        match self.extract_literal(node) {
            Some((text, quoted)) => ArgVal::Lit { text, quoted },
            None => ArgVal::Opaque,
        }
    }

    fn extract_literal(&mut self, node: Node) -> Option<(String, bool)> {
        match node.kind() {
            "word" => Some((unescape_word(&self.node_text(node)?), false)),
            "number" => Some((self.node_text(node)?, false)),
            "raw_string" => {
                let t = self.node_text(node)?;
                Some((t.trim_matches('\'').to_string(), true))
            }
            "string" => {
                let mut result = String::new();
                let mut c = node.walk();
                for child in node.named_children(&mut c) {
                    match child.kind() {
                        "string_content" => result.push_str(&self.node_text(child)?),
                        "escape_sequence" => {
                            let t = self.node_text(child)?;
                            result.push_str(t.strip_prefix('\\').unwrap_or(&t));
                        }
                        _ => return None, // expansion, command_substitution, …
                    }
                }
                Some((result, true))
            }
            "concatenation" => {
                let mut result = String::new();
                let mut all_quoted = true;
                let mut c = node.walk();
                for child in node.named_children(&mut c) {
                    let (part, quoted) = self.extract_literal(child)?;
                    result.push_str(&part);
                    all_quoted &= quoted;
                }
                Some((result, all_quoted))
            }
            _ => None,
        }
    }

    fn node_text(&self, node: Node) -> Option<String> {
        node.utf8_text(self.src).ok().map(str::to_string)
    }
}

fn contains_kind(node: Node, kinds: &[&str]) -> bool {
    if kinds.contains(&node.kind()) {
        return true;
    }
    let mut c = node.walk();
    node.children(&mut c).any(|ch| contains_kind(ch, kinds))
}

fn find_repo_root(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .find(|p| p.join(".git").exists())
        .map(normalize_lexical)
}

fn unescape_word(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            if let Some(next) = chars.next() {
                result.push(next);
            }
        } else {
            result.push(ch);
        }
    }
    result
}

/// Lexical normalization: resolves `.` and `..` without touching the
/// filesystem (no symlink resolution — the snapshot engine handles links).
pub fn normalize_lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}
