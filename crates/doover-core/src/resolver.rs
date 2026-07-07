//! Bash command parsing and affected-path scope resolution.
//!
//! Parsing uses `brush-parser` (pure Rust). The original tree-sitter-bash
//! backend was removed after fuzzing found glibc-only segfaults in its C
//! error recovery (heap-layout-dependent; 7-byte minimized repro) — a safety
//! tool whose input is adversarial by definition cannot keep a memory-unsafe
//! parser in the trust path. With pure Rust, worst-case failures are panics,
//! which the thread wrapper in [`resolve`] converts into a conservative
//! Unknown resolution.
//!
//! Quote-context correctness: every word is carried as *segments* with an
//! expandability mask (unquoted vs quoted/escaped), because bash only
//! brace-expands, globs, and tilde-expands unquoted spans. `'{a,b}'{c,d}`
//! expands only the second group; `'*'x` never globs; `'~/x'` is a literal.
//! The bash differential oracle (tests/resolver_oracle.rs) enforces this
//! against real bash.
//!
//! Design invariants (property-tested):
//! - never panics or crashes the caller on any input;
//! - anything the resolver cannot fully account for — opaque constructs
//!   (`eval`, `$( )`, `sh -c`, `xargs`, wrappers like `sudo`), unresolvable
//!   variables, parse errors, control-flow compounds, or a destructive rule
//!   that yielded zero concrete paths — sets `has_unknown`, which routes the
//!   action through the engine's unknown policy instead of silently
//!   under-protecting;
//! - resolution is deterministic and purely lexical except for glob expansion
//!   and git-repo-root discovery, which read (never write) the filesystem.

use crate::registry::{Effect, PathSource, Registry, Rule};
use brush_parser::ast;
use brush_parser::word::{TildeExpr, WordPiece, WordPieceWithSource};
use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};

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

/// Nesting cap for subshells/brace groups and for literal reconstruction; no
/// legitimate agent command comes close, and capping keeps stack usage bounded
/// on adversarial input. Fail safe (Unknown) beyond it.
const MAX_WALK_DEPTH: usize = 256;
const MAX_LITERAL_DEPTH: usize = 64;

/// Cap on brace-expansion fan-out; beyond it the scope is treated as unknown
/// rather than materializing a huge path list.
const MAX_BRACE_EXPANSION: usize = 1024;

/// Generous stack for the recursive-descent parser on pathological inputs;
/// panics anywhere in resolution degrade to a conservative Unknown.
const RESOLVER_STACK_BYTES: usize = 32 * 1024 * 1024;

pub fn resolve(command: &str, registry: &Registry, ctx: &Ctx) -> Resolution {
    std::thread::scope(|scope| {
        let handle = std::thread::Builder::new()
            .name("doover-resolver".into())
            .stack_size(RESOLVER_STACK_BYTES)
            .spawn_scoped(scope, || resolve_inner(command, registry, ctx));
        match handle.map(std::thread::ScopedJoinHandle::join) {
            Ok(Ok(resolution)) => resolution,
            _ => Resolution {
                severity: Severity::Unknown,
                paths: Vec::new(),
                rule_id: None,
                has_unknown: true,
            },
        }
    })
}

fn resolve_inner(command: &str, registry: &Registry, ctx: &Ctx) -> Resolution {
    let mut walker = Walker {
        registry,
        ctx,
        options: brush_parser::ParserOptions::default(),
        out: Out::default(),
    };
    let mut cur = Some(normalize_lexical(ctx.cwd));
    match brush_parser::tokenize_str(command) {
        Ok(tokens) => match brush_parser::parse_tokens(&tokens, &walker.options) {
            Ok(program) => {
                for complete_command in &program.complete_commands {
                    walker.walk_compound_list(complete_command, &mut cur, 0);
                }
            }
            Err(_) => walker.out.mark_unknown(),
        },
        Err(_) => walker.out.mark_unknown(),
    }
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

/// A word as quote-aware segments: `(text, expandable)`. Only expandable
/// (unquoted, unescaped) spans participate in brace/glob expansion.
/// `tilde_home` records an unquoted leading `~` (brush emits a tilde piece
/// only where bash would actually expand one).
#[derive(Clone, Debug)]
struct Word {
    segs: Vec<(String, bool)>,
    tilde_home: bool,
}

impl Word {
    fn text(&self) -> String {
        self.segs.iter().map(|(s, _)| s.as_str()).collect()
    }

    fn masked_chars(&self) -> Vec<(char, bool)> {
        self.segs
            .iter()
            .flat_map(|(s, exp)| s.chars().map(move |c| (c, *exp)))
            .collect()
    }

    fn has_expandable(&self, ch: char) -> bool {
        self.segs.iter().any(|(s, exp)| *exp && s.contains(ch))
    }

    /// Split at a byte offset of `text()`, preserving masks; used to peel the
    /// value off attached flag forms (`-ovalue`, `--output=value`).
    fn split_off_bytes(&self, at: usize) -> Word {
        let mut consumed = 0usize;
        let mut segs = Vec::new();
        for (s, exp) in &self.segs {
            let end = consumed + s.len();
            if end > at {
                let start = at.saturating_sub(consumed);
                segs.push((s[start..].to_string(), *exp));
            }
            consumed = end;
        }
        Word {
            segs,
            tilde_home: false,
        }
    }
}

enum ArgVal {
    Lit(Word),
    Opaque,
}

enum Tok {
    Flag(Word),
    Pos(Word),
}

/// What the next positional token should become, given the preceding flag.
enum Consume {
    No,
    Drop,
    Path,
}

struct Walker<'a> {
    registry: &'a Registry,
    ctx: &'a Ctx<'a>,
    options: brush_parser::ParserOptions,
    out: Out,
}

impl Walker<'_> {
    /// `cur` is the tracked working directory; `None` means a `cd` made it
    /// unresolvable and every later relative path is unknown.
    fn walk_compound_list(
        &mut self,
        list: &ast::CompoundList,
        cur: &mut Option<PathBuf>,
        depth: usize,
    ) {
        if depth > MAX_WALK_DEPTH {
            self.out.mark_unknown();
            return;
        }
        for ast::CompoundListItem(and_or, _sep) in &list.0 {
            self.walk_pipeline(&and_or.first, cur, depth);
            for branch in &and_or.additional {
                let (ast::AndOr::And(p) | ast::AndOr::Or(p)) = branch;
                self.walk_pipeline(p, cur, depth);
            }
        }
    }

    fn walk_pipeline(&mut self, pipeline: &ast::Pipeline, cur: &mut Option<PathBuf>, depth: usize) {
        if pipeline.seq.len() == 1 {
            self.walk_command(&pipeline.seq[0], cur, depth);
        } else {
            // each segment runs in its own subshell: isolate cwd changes
            for command in &pipeline.seq {
                let mut seg_cur = cur.clone();
                self.walk_command(command, &mut seg_cur, depth);
            }
        }
    }

    fn walk_command(&mut self, command: &ast::Command, cur: &mut Option<PathBuf>, depth: usize) {
        match command {
            ast::Command::Simple(simple) => self.handle_simple(simple, cur),
            ast::Command::Compound(compound, redirects) => {
                match compound {
                    ast::CompoundCommand::Subshell(subshell) => {
                        let mut sub_cur = cur.clone();
                        self.walk_compound_list(&subshell.list, &mut sub_cur, depth + 1);
                    }
                    ast::CompoundCommand::BraceGroup(group) => {
                        self.walk_compound_list(&group.list, cur, depth + 1);
                    }
                    // control flow (if/for/while/case/arith/coproc) executes
                    // data-dependent bodies: conservative until refined
                    _ => self.out.mark_unknown(),
                }
                if let Some(list) = redirects {
                    for redirect in &list.0 {
                        self.handle_redirect(redirect, cur);
                    }
                }
            }
            // a function *definition* executes nothing; calling it later hits
            // the unregistered-command path and classifies unknown
            ast::Command::Function(_) => {}
            // [[ ... ]] evaluates without filesystem writes
            ast::Command::ExtendedTest(_, redirects) => {
                if let Some(list) = redirects {
                    for redirect in &list.0 {
                        self.handle_redirect(redirect, cur);
                    }
                }
            }
        }
    }

    fn handle_simple(&mut self, simple: &ast::SimpleCommand, cur: &mut Option<PathBuf>) {
        if let Some(prefix) = &simple.prefix {
            self.handle_prefix_suffix_items(&prefix.0, None, cur);
        }
        // suffix first so redirects are honored even when the command name is
        // opaque (`$CMD > file` must still protect file)
        let mut args: Vec<ArgVal> = Vec::new();
        if let Some(suffix) = &simple.suffix {
            self.handle_prefix_suffix_items(&suffix.0, Some(&mut args), cur);
        }

        let Some(name_word) = &simple.word_or_name else {
            return; // bare assignments / redirects only
        };
        let Some(name_lit) = self.extract_word(&name_word.value) else {
            self.out.mark_unknown();
            return;
        };
        if name_lit.tilde_home {
            self.out.mark_unknown();
            return;
        }
        let name = name_lit.text();

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
                ArgVal::Lit(word) => {
                    let text = word.text();
                    if !after_double_dash && text == "--" {
                        after_double_dash = true;
                    } else if !after_double_dash && text.len() > 1 && text.starts_with('-') {
                        tokens.push(Tok::Flag(word.clone()));
                    } else {
                        tokens.push(Tok::Pos(word.clone()));
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
                Tok::Flag(w) => Some(w.text()),
                Tok::Pos(_) => None,
            })
            .collect();
        let subs: Vec<String> = tokens
            .iter()
            .filter_map(|t| match t {
                Tok::Pos(w) => Some(w.text()),
                Tok::Flag(_) => None,
            })
            .collect();
        let Some(rule) =
            self.registry
                .lookup_command(&name, subs.first().map(String::as_str), &flags)
        else {
            self.out.mark_unknown();
            return;
        };
        let sev = Severity::from(rule.effect);
        self.out.contribute(sev, &rule.id);

        // separate positionals from flag-carried values. `flag_args` flags
        // consume a non-path value (dropped); `path_flags` flags carry a path
        // value (captured) — in separate (`-o v`), attached-short (`-ov`), or
        // attached-long (`--out=v`) form.
        let empty: &[String] = &[];
        let (flag_args, path_flags) = rule
            .scope
            .as_ref()
            .map(|s| (s.flag_args.as_slice(), s.path_flags.as_slice()))
            .unwrap_or((empty, empty));
        let is_short = |f: &str| f.len() == 2 && f.starts_with('-') && !f.starts_with("--");
        let mut positionals: Vec<Word> = Vec::new();
        let mut flag_paths: Vec<Word> = Vec::new();
        let mut consume: Consume = Consume::No;
        for tok in &tokens {
            match tok {
                Tok::Flag(w) => {
                    let t = w.text();
                    if let Some(eq) = t.find('=') {
                        if path_flags.iter().any(|pf| pf.as_str() == &t[..eq]) {
                            flag_paths.push(w.split_off_bytes(eq + 1));
                        }
                    } else if path_flags.iter().any(|pf| pf.as_str() == t) {
                        consume = Consume::Path;
                    } else if flag_args.iter().any(|fa| fa.as_str() == t) {
                        consume = Consume::Drop;
                    } else if let Some(pf) = path_flags
                        .iter()
                        .find(|pf| is_short(pf) && t.starts_with(pf.as_str()) && t.len() > 2)
                    {
                        flag_paths.push(w.split_off_bytes(pf.len()));
                    } else if flag_args
                        .iter()
                        .any(|fa| is_short(fa) && t.starts_with(fa.as_str()) && t.len() > 2)
                    {
                        // attached non-path value: nothing to capture or drop
                    }
                }
                Tok::Pos(w) => match std::mem::replace(&mut consume, Consume::No) {
                    Consume::Path => flag_paths.push(w.clone()),
                    Consume::Drop => {}
                    Consume::No => positionals.push(w.clone()),
                },
            }
        }

        let mut contributed = self.extract_scope(rule, &positionals, cur);
        if let Some(scope) = &rule.scope {
            for pv in &flag_paths {
                // path-flag values (e.g. `sort -o out`) are write targets:
                // a symlink there is written through
                contributed += self.add_path(pv, scope.globs, cur, true);
            }
        }
        if sev >= Severity::Destructive && contributed == 0 {
            // destructive action with no pre-snapshottable paths: the engine
            // must fall back to the unknown policy rather than claim coverage
            self.out.mark_unknown();
        }
    }

    fn handle_prefix_suffix_items(
        &mut self,
        items: &[ast::CommandPrefixOrSuffixItem],
        mut args: Option<&mut Vec<ArgVal>>,
        cur: &Option<PathBuf>,
    ) {
        for item in items {
            match item {
                ast::CommandPrefixOrSuffixItem::Word(word) => {
                    if let Some(args) = args.as_deref_mut() {
                        args.push(match self.extract_word(&word.value) {
                            Some(lit) => ArgVal::Lit(lit),
                            None => ArgVal::Opaque,
                        });
                    }
                }
                ast::CommandPrefixOrSuffixItem::IoRedirect(redirect) => {
                    self.handle_redirect(redirect, cur);
                }
                ast::CommandPrefixOrSuffixItem::AssignmentWord(_assignment, word) => {
                    // FOO=bar is inert, but FOO=$(cmd) — including nesting like
                    // FOO=${X:-$(cmd)} — executes
                    if self.word_can_execute(&word.value) {
                        self.out.mark_unknown();
                    }
                }
                ast::CommandPrefixOrSuffixItem::ProcessSubstitution(..) => {
                    self.out.mark_unknown();
                }
            }
        }
    }

    fn extract_scope(&mut self, rule: &Rule, positionals: &[Word], cur: &Option<PathBuf>) -> usize {
        let Some(scope) = &rule.scope else { return 0 };
        let skip = usize::from(rule.matcher.subcommand.is_some()) + scope.skip;
        let path_args: Vec<&Word> = positionals.iter().skip(skip).collect();
        let mut contributed = 0usize;
        match scope.paths {
            PathSource::Positional => {
                for word in path_args {
                    contributed += self.add_path(word, scope.globs, cur, false);
                }
            }
            PathSource::PositionalLast => {
                if let Some(word) = path_args.last() {
                    contributed += self.add_path(word, scope.globs, cur, false);
                }
            }
            PathSource::Repo => {
                if let Some(root) = cur.as_deref().and_then(find_repo_root) {
                    self.insert_scoped(root, false);
                    contributed += 1;
                }
            }
            PathSource::RedirectTarget | PathSource::None => {}
        }
        contributed
    }

    /// Resolve one path word, applying bash expansion order — brace, then
    /// tilde/normalize, then glob — each step honoring the quote mask.
    /// Returns how many paths were added.
    /// `write_target` marks a value that will be WRITTEN through (redirect and
    /// path-flag targets), so a symlink there is always dereferenced. For
    /// ordinary positional args, dereferencing is decided per variant by a
    /// trailing `/` or `/.` (see [`Self::add_single`]).
    fn add_path(
        &mut self,
        word: &Word,
        globs: bool,
        cur: &Option<PathBuf>,
        write_target: bool,
    ) -> usize {
        if word.segs.is_empty() && !word.tilde_home {
            return 0;
        }
        let chars = word.masked_chars();
        let expanded: Vec<Vec<(char, bool)>> = if word.has_expandable('{') {
            match expand_braces_masked(&chars, MAX_BRACE_EXPANSION) {
                Some(list) => list,
                None => {
                    // fan-out too large: scope is unresolvable
                    self.out.mark_unknown();
                    return 0;
                }
            }
        } else {
            vec![chars]
        };
        let mut total = 0;
        for variant in expanded {
            total += self.add_single(&variant, word.tilde_home, globs, cur, write_target);
        }
        total
    }

    /// One fully brace-expanded masked word → resolved path(s), globbing only
    /// on unquoted glob characters.
    fn add_single(
        &mut self,
        chars: &[(char, bool)],
        tilde_home: bool,
        globs: bool,
        cur: &Option<PathBuf>,
        write_target: bool,
    ) -> usize {
        let text: String = chars.iter().map(|(c, _)| c).collect();
        if text.is_empty() && !tilde_home {
            return 0;
        }
        // `link/..` addresses the target's PARENT (deref then up): unbounded
        // scope we can't safely enumerate — fail to unknown
        if text.ends_with("/..") {
            self.out.mark_unknown();
            return 0;
        }

        // base directory + word remainder
        let (base, rem): (Option<PathBuf>, &[(char, bool)]) = if tilde_home {
            let mut start = 0;
            while start < chars.len() && chars[start].0 == '/' {
                start += 1;
            }
            (Some(self.ctx.home.to_path_buf()), &chars[start..])
        } else if Path::new(&text).is_absolute() {
            (None, chars)
        } else {
            match cur {
                Some(dir) => (Some(dir.clone()), chars),
                None => {
                    self.out.mark_unknown();
                    return 0;
                }
            }
        };
        let rem_text: String = rem.iter().map(|(c, _)| c).collect();
        let literal = match &base {
            Some(b) => normalize_lexical(&b.join(&rem_text)),
            None => normalize_lexical(Path::new(&rem_text)),
        };

        let active_glob = rem.iter().any(|(c, exp)| *exp && GLOB_CHARS.contains(c));
        if !(globs && active_glob) {
            // dereference a symlink only when the usage actually goes through
            // it: a write target, or a trailing `/` or `/.` (operates on the
            // target dir). A bare `rm link` scopes only the link (round 8).
            let derefs = write_target || rem_text.ends_with('/') || rem_text.ends_with("/.");
            self.insert_scoped(literal, derefs);
            return 1;
        }

        // glob pattern: escape the base entirely and any QUOTED metacharacters
        // in the word — only unquoted ones keep their pattern meaning
        let mut pattern = String::new();
        if let Some(b) = &base {
            pattern.push_str(&glob::Pattern::escape(&b.to_string_lossy()));
            pattern.push('/');
        }
        for (c, exp) in rem {
            if !exp && GLOB_CHARS.contains(c) {
                pattern.push_str(&glob::Pattern::escape(&c.to_string()));
            } else {
                pattern.push(*c);
            }
        }
        // bash parity for leading dots (audit rounds 5 & 6): a `*`/`?`/`[`
        // matches a hidden component only when the *corresponding pattern
        // component* literally begins with `.` — and this holds for EVERY
        // component, not just the last (`*/f.txt` must not descend `.hidden/`).
        // The glob crate's require_literal_leading_dot is unreliable (misses
        // `.h*`), so we glob permissively and filter per-component ourselves.
        let base_prefix = base.as_ref().map(|b| format!("{}/", b.to_string_lossy()));
        let pat_components: Vec<&str> = rem_text.trim_matches('/').split('/').collect();
        match glob::glob(&pattern) {
            Ok(matches) => {
                let mut n = 0usize;
                for m in matches.take(10_000).flatten() {
                    let m_str = m.to_string_lossy();
                    let relative = match &base_prefix {
                        Some(p) => m_str.strip_prefix(p.as_str()).unwrap_or(&m_str),
                        None => m_str.trim_start_matches('/'),
                    };
                    if glob_match_allowed(relative, &pat_components) {
                        // a glob match is a concrete entry; rm removes the link
                        // itself, not its target — no deref
                        self.insert_scoped(normalize_lexical(&m), false);
                        n += 1;
                    }
                }
                n
            }
            Err(_) => {
                self.insert_scoped(literal, false);
                1
            }
        }
    }

    /// Record a scoped path. When `deref` and the path is a symlink, also scope
    /// its resolved target chain — operations that go THROUGH a link act on the
    /// target: `> linkfile` clobbers the target's content, `rm -rf link/` (or
    /// `link/.`) deletes the target directory. `deref` is false for a bare
    /// `rm link`, which touches only the link (round 8: unconditional deref
    /// pulled whole target trees into the store). `read_link` (not
    /// canonicalize) so dangling links scope their would-be-created target as
    /// an absent-marker. Chain hops are capped; loops simply stop contributing.
    fn insert_scoped(&mut self, path: PathBuf, deref: bool) {
        let mut hops = 0;
        let mut cur = path;
        loop {
            let is_link = deref && cur.symlink_metadata().is_ok_and(|m| m.is_symlink());
            self.out.paths.insert(cur.clone());
            if !is_link || hops >= 8 {
                return;
            }
            let Ok(target) = std::fs::read_link(&cur) else {
                return;
            };
            let resolved = if target.is_absolute() {
                normalize_lexical(&target)
            } else {
                match cur.parent() {
                    Some(parent) => normalize_lexical(&parent.join(target)),
                    None => return,
                }
            };
            if self.out.paths.contains(&resolved) {
                return; // loop or already scoped
            }
            cur = resolved;
            hops += 1;
        }
    }

    /// Literal resolution for words that never glob or brace-expand (cd
    /// targets, redirect targets).
    fn resolve_literal(&mut self, word: &Word, cur: &Option<PathBuf>) -> Option<PathBuf> {
        let text = word.text();
        if word.tilde_home {
            return Some(normalize_lexical(
                &self.ctx.home.join(text.trim_start_matches('/')),
            ));
        }
        if Path::new(&text).is_absolute() {
            return Some(normalize_lexical(Path::new(&text)));
        }
        match cur {
            Some(dir) => Some(normalize_lexical(&dir.join(&text))),
            None => {
                self.out.mark_unknown();
                None
            }
        }
    }

    fn handle_cd(&mut self, args: &[ArgVal], cur: &mut Option<PathBuf>) {
        let target = args.iter().find(|a| match a {
            ArgVal::Lit(w) => {
                let t = w.text();
                !(t.len() > 1 && t.starts_with('-')) && t != "--"
            }
            ArgVal::Opaque => true,
        });
        match target {
            None => *cur = Some(normalize_lexical(self.ctx.home)),
            Some(ArgVal::Lit(w)) if w.text() == "-" => {
                // previous directory is untracked
                self.out.mark_unknown();
                *cur = None;
            }
            Some(ArgVal::Lit(w)) => match self.resolve_literal(w, cur) {
                Some(p) => *cur = Some(p),
                None => *cur = None,
            },
            Some(ArgVal::Opaque) => {
                self.out.mark_unknown();
                *cur = None;
            }
        }
    }

    fn handle_redirect(&mut self, redirect: &ast::IoRedirect, cur: &Option<PathBuf>) {
        use ast::{IoFileRedirectKind as Kind, IoFileRedirectTarget as Target, IoRedirect as R};
        match redirect {
            R::File(_fd, kind, target) => {
                let op = match kind {
                    Kind::Write | Kind::Clobber | Kind::ReadAndWrite => ">",
                    Kind::Append => ">>",
                    // input redirects don't write a file, but the target word
                    // itself can execute (`< $(cmd)`), which must not pass as
                    // safe
                    Kind::Read | Kind::DuplicateInput | Kind::DuplicateOutput => {
                        if let Target::Filename(word) = target {
                            if self.word_can_execute(&word.value) {
                                self.out.mark_unknown();
                            }
                        }
                        return;
                    }
                };
                match target {
                    Target::Filename(word) => self.redirect_to(op, &word.value, cur),
                    Target::Fd(_) | Target::Duplicate(_) => {}
                    Target::ProcessSubstitution(..) => self.out.mark_unknown(),
                }
            }
            // &> / &>>
            R::OutputAndError(word, append) => {
                let op = if *append { ">>" } else { ">" };
                self.redirect_to(op, &word.value, cur);
            }
            // here-string: `<<< $(cmd)` executes the substitution
            R::HereString(_fd, word) => {
                if self.word_can_execute(&word.value) {
                    self.out.mark_unknown();
                }
            }
            // heredoc body is expanded unless the delimiter was quoted
            R::HereDocument(_fd, here) => {
                if here.requires_expansion && self.word_can_execute(&here.doc.value) {
                    self.out.mark_unknown();
                }
            }
        }
    }

    fn redirect_to(&mut self, op: &str, raw: &str, cur: &Option<PathBuf>) {
        let Some(word) = self.extract_word(raw) else {
            self.out.mark_unknown();
            return;
        };
        let text = word.text();
        // numeric target is an fd, not a file
        if (text.is_empty() && !word.tilde_home) || text.chars().all(|ch| ch.is_ascii_digit()) {
            return;
        }
        let Some(rule) = self.registry.lookup_redirect(op) else {
            self.out.mark_unknown();
            return;
        };
        self.out.contribute(Severity::from(rule.effect), &rule.id);
        if let Some(p) = self.resolve_literal(&word, cur) {
            // redirects WRITE THROUGH symlinks: deref so the clobbered target
            // content is snapshotted
            self.insert_scoped(p, true);
        }
    }

    fn extract_word(&self, raw: &str) -> Option<Word> {
        let pieces = brush_parser::word::parse(raw, &self.options).ok()?;
        pieces_to_word(&pieces, 0)
    }

    /// True if the word can execute a command when expanded: a direct command
    /// substitution, a backquote, or a command substitution nested inside a
    /// parameter/arithmetic expansion (e.g. `${X:-$(cmd)}`). Single-quoted text
    /// is inert. Fail-closed: unparseable ⇒ true.
    fn word_can_execute(&self, raw: &str) -> bool {
        fn scan(pieces: &[WordPieceWithSource], raw: &str, depth: usize) -> bool {
            depth > MAX_LITERAL_DEPTH
                || pieces.iter().any(|pw| match &pw.piece {
                    WordPiece::CommandSubstitution(_)
                    | WordPiece::BackquotedCommandSubstitution(_) => true,
                    WordPiece::DoubleQuotedSequence(inner)
                    | WordPiece::GettextDoubleQuotedSequence(inner) => scan(inner, raw, depth + 1),
                    // parameter/arithmetic expansions carry their operands as
                    // opaque strings; a nested $( ) or backquote there still
                    // executes. Conservatively flag if the word contains one.
                    WordPiece::ParameterExpansion(_) | WordPiece::ArithmeticExpression(_) => {
                        raw.contains("$(") || raw.contains('`')
                    }
                    _ => false,
                })
        }
        match brush_parser::word::parse(raw, &self.options) {
            Ok(pieces) => scan(&pieces, raw, 0),
            Err(_) => true,
        }
    }
}

/// Reduce parsed word pieces to quote-aware segments. Only a *leading*
/// unquoted `~` piece becomes tilde expansion, matching where brush emits it.
fn pieces_to_word(pieces: &[WordPieceWithSource], depth: usize) -> Option<Word> {
    if depth > MAX_LITERAL_DEPTH {
        return None;
    }
    let mut word = Word {
        segs: Vec::new(),
        tilde_home: false,
    };
    for (i, pw) in pieces.iter().enumerate() {
        match &pw.piece {
            WordPiece::Text(s) => word.segs.push((s.clone(), true)),
            WordPiece::SingleQuotedText(s) | WordPiece::AnsiCQuotedText(s) => {
                word.segs.push((s.clone(), false));
            }
            WordPiece::DoubleQuotedSequence(inner)
            | WordPiece::GettextDoubleQuotedSequence(inner) => {
                // quoted metacharacters don't expand: the whole sequence is a
                // non-expandable segment
                let lit = pieces_to_word(inner, depth + 1)?;
                if lit.tilde_home {
                    return None; // "~" quoted-with-tilde-piece: shouldn't occur
                }
                word.segs.push((lit.text(), false));
            }
            WordPiece::EscapeSequence(s) => {
                word.segs
                    .push((s.strip_prefix('\\').unwrap_or(s).to_string(), false));
            }
            WordPiece::TildeExpansion(TildeExpr::Home) if i == 0 => {
                word.tilde_home = true;
            }
            // ~user / ~+ / ~- / mid-word tilde pieces and all expansions are
            // unresolvable statically
            _ => return None,
        }
    }
    Some(word)
}

/// Bash-style brace expansion over quote-masked characters: only expandable
/// braces/commas have structural meaning. `Some(list)` on success (a word with
/// no expandable group yields itself); `None` when fan-out exceeds `limit`.
fn expand_braces_masked(chars: &[(char, bool)], limit: usize) -> Option<Vec<Vec<(char, bool)>>> {
    let mut out = Vec::new();
    if expand_rec(chars, &mut out, limit) {
        Some(out)
    } else {
        None
    }
}

fn expand_rec(chars: &[(char, bool)], out: &mut Vec<Vec<(char, bool)>>, limit: usize) -> bool {
    if out.len() > limit {
        return false;
    }
    for i in 0..chars.len() {
        if chars[i] != ('{', true) {
            continue;
        }
        match parse_brace(chars, i, limit) {
            BraceParse::Expand(close, options) => {
                for opt in options {
                    let mut next = chars[..i].to_vec();
                    next.extend(opt);
                    next.extend_from_slice(&chars[close + 1..]);
                    if !expand_rec(&next, out, limit) {
                        return false;
                    }
                }
                return true;
            }
            BraceParse::Overflow => return false,
            BraceParse::NotHere => {}
        }
    }
    out.push(chars.to_vec());
    out.len() <= limit
}

enum BraceParse {
    Expand(usize, Vec<Vec<(char, bool)>>),
    Overflow,
    NotHere,
}

fn parse_brace(chars: &[(char, bool)], open: usize, limit: usize) -> BraceParse {
    let mut depth = 0usize;
    let mut close = None;
    let mut commas = Vec::new();
    for (i, &(c, exp)) in chars.iter().enumerate().skip(open) {
        if !exp {
            continue; // quoted braces/commas are content, not structure
        }
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    close = Some(i);
                    break;
                }
            }
            ',' if depth == 1 => commas.push(i),
            _ => {}
        }
    }
    let Some(close) = close else {
        return BraceParse::NotHere;
    };
    if !commas.is_empty() {
        let mut options = Vec::new();
        let mut start = open + 1;
        for &c in &commas {
            options.push(chars[start..c].to_vec());
            start = c + 1;
        }
        options.push(chars[start..close].to_vec());
        return BraceParse::Expand(close, options);
    }
    // range form: only when the whole interior is unquoted
    let interior = &chars[open + 1..close];
    if interior.is_empty() || interior.iter().any(|(_, exp)| !exp) {
        return BraceParse::NotHere;
    }
    let inner: String = interior.iter().map(|(c, _)| c).collect();
    match parse_range(&inner, limit) {
        Some(Some(options)) => BraceParse::Expand(
            close,
            options
                .into_iter()
                .map(|s| s.chars().map(|c| (c, true)).collect())
                .collect(),
        ),
        Some(None) => BraceParse::Overflow,
        None => BraceParse::NotHere, // {literal}
    }
}

/// `{m..n}` / `{a..c}` ranges, using i128 internally so extreme i64 bounds
/// cannot overflow. `Some(Some(list))` on a valid range, `Some(None)` when it
/// exceeds `limit` OR is version-divergent, `None` when not a range at all.
///
/// Version portability (bash-oracle finding): stepped ranges (`{1..9..2}`)
/// and zero-padded ranges (`{01..03}`) behave differently between bash 3.2
/// (macOS default) and bash 4+. We cannot know which bash executes the
/// command, so those forms are "cannot safely expand" → unknown policy,
/// never a confidently-wrong path list.
fn parse_range(inner: &str, limit: usize) -> Option<Option<Vec<String>>> {
    let parts: Vec<&str> = inner.split("..").collect();
    if parts.len() != 2 && parts.len() != 3 {
        return None;
    }
    if parts.len() == 3 {
        // stepped range: bash-version-divergent (3.2 leaves it literal)
        let looks_like_range = parts[2].parse::<i128>().is_ok();
        return if looks_like_range { Some(None) } else { None };
    }
    let step: i128 = 1;

    let make = |a: i128, b: i128, fmt: &dyn Fn(i128) -> String| -> Option<Vec<String>> {
        let count = (a - b).unsigned_abs() / step.unsigned_abs() + 1;
        if count > limit as u128 {
            return None;
        }
        let dir: i128 = if a <= b { step } else { -step };
        let mut v = Vec::with_capacity(count as usize);
        let mut cur = a;
        while (dir > 0 && cur <= b) || (dir < 0 && cur >= b) {
            v.push(fmt(cur));
            cur += dir;
        }
        Some(v)
    };

    // numeric range
    if let (Ok(a), Ok(b)) = (parts[0].parse::<i128>(), parts[1].parse::<i128>()) {
        let zero_padded = (parts[0].starts_with('0') && parts[0].len() > 1)
            || (parts[1].starts_with('0') && parts[1].len() > 1)
            || parts[0].starts_with("-0")
            || parts[1].starts_with("-0");
        if zero_padded {
            // bash-version-divergent (3.2 strips padding, 4+ pads)
            return Some(None);
        }
        return Some(make(a, b, &|n| n.to_string()));
    }

    // single-character alphabetic range
    let (sc, ec) = (
        parts[0].chars().collect::<Vec<_>>(),
        parts[1].chars().collect::<Vec<_>>(),
    );
    if sc.len() == 1 && ec.len() == 1 && sc[0].is_ascii_alphabetic() && ec[0].is_ascii_alphabetic()
    {
        return Some(make(sc[0] as i128, ec[0] as i128, &|n| {
            ((n as u8) as char).to_string()
        }));
    }
    None
}

/// Bash leading-dot rule per component: a matched path component that begins
/// with `.` is allowed only when the aligned pattern component also begins
/// with a literal `.`. `.` and `..` pseudo-entries are always rejected (rm
/// can't act on them; `..` would scope a parent tree).
fn glob_match_allowed(relative: &str, pat_components: &[&str]) -> bool {
    // empty components (from `//` in pattern or path) carry no matching
    // meaning and would misalign the index comparison
    let pats: Vec<&str> = pat_components
        .iter()
        .copied()
        .filter(|p| !p.is_empty())
        .collect();
    for (i, comp) in relative.split('/').filter(|c| !c.is_empty()).enumerate() {
        if comp == "." || comp == ".." {
            return false;
        }
        if comp.starts_with('.') {
            let pat_dotted = pats.get(i).is_some_and(|p| p.starts_with('.'));
            if !pat_dotted {
                return false;
            }
        }
    }
    true
}

fn find_repo_root(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .find(|p| p.join(".git").exists())
        .map(normalize_lexical)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn masked(parts: &[(&str, bool)]) -> Vec<(char, bool)> {
        parts
            .iter()
            .flat_map(|(s, e)| s.chars().map(move |c| (c, *e)))
            .collect()
    }

    fn texts(result: Option<Vec<Vec<(char, bool)>>>) -> Option<Vec<String>> {
        result.map(|list| {
            list.into_iter()
                .map(|cs| cs.into_iter().map(|(c, _)| c).collect())
                .collect()
        })
    }

    #[test]
    fn unquoted_group_expands() {
        let r = texts(expand_braces_masked(&masked(&[("{a,b}", true)]), 64));
        assert_eq!(r, Some(vec!["a".into(), "b".into()]));
    }

    #[test]
    fn quoted_group_is_inert() {
        let r = texts(expand_braces_masked(&masked(&[("{a,b}", false)]), 64));
        assert_eq!(r, Some(vec!["{a,b}".into()]));
    }

    #[test]
    fn quoted_prefix_with_unquoted_group() {
        // the audit-2 bug: '{a,b}'{c,d} must expand ONLY {c,d}
        let r = texts(expand_braces_masked(
            &masked(&[("{a,b}", false), ("{c,d}", true)]),
            64,
        ));
        assert_eq!(r, Some(vec!["{a,b}c".into(), "{a,b}d".into()]));
    }

    #[test]
    fn cartesian_product() {
        let r = texts(expand_braces_masked(&masked(&[("{a,b}{1,2}", true)]), 64));
        assert_eq!(
            r,
            Some(vec!["a1".into(), "a2".into(), "b1".into(), "b2".into()])
        );
    }

    #[test]
    fn nested_groups() {
        let r = texts(expand_braces_masked(&masked(&[("{a,b{1,2}}", true)]), 64));
        assert_eq!(r, Some(vec!["a".into(), "b1".into(), "b2".into()]));
    }

    #[test]
    fn comma_free_brace_is_literal() {
        let r = texts(expand_braces_masked(&masked(&[("x{alone}y", true)]), 64));
        assert_eq!(r, Some(vec!["x{alone}y".into()]));
    }

    #[test]
    fn portable_ranges_expand() {
        assert_eq!(
            texts(expand_braces_masked(&masked(&[("{1..3}", true)]), 64)),
            Some(vec!["1".into(), "2".into(), "3".into()])
        );
        assert_eq!(
            texts(expand_braces_masked(&masked(&[("{c..a}", true)]), 64)),
            Some(vec!["c".into(), "b".into(), "a".into()])
        );
    }

    #[test]
    fn version_divergent_ranges_are_unknown_not_guessed() {
        // bash 3.2 (macOS) vs 4+ disagree on steps and zero-padding: we must
        // never emit a confidently-wrong path list (bash-oracle finding)
        assert_eq!(
            texts(expand_braces_masked(&masked(&[("{1..9..4}", true)]), 64)),
            None
        );
        assert_eq!(
            texts(expand_braces_masked(&masked(&[("{01..03}", true)]), 64)),
            None
        );
    }

    #[test]
    fn quoted_range_interior_is_literal() {
        // {'1..3'} — interior quoted: not a range
        let r = texts(expand_braces_masked(
            &masked(&[("{", true), ("1..3", false), ("}", true)]),
            64,
        ));
        assert_eq!(r, Some(vec!["{1..3}".into()]));
    }

    #[test]
    fn fanout_cap_returns_none() {
        assert_eq!(
            texts(expand_braces_masked(&masked(&[("{1..2000}", true)]), 1024)),
            None
        );
    }

    #[test]
    fn extreme_i64_range_no_panic_no_overflow() {
        // would overflow i64 subtraction; must cleanly report over-limit
        let r = expand_braces_masked(
            &masked(&[("{-9223372036854775808..9223372036854775807}", true)]),
            1024,
        );
        assert!(r.is_none());
    }

    #[test]
    fn identity_without_braces() {
        let r = texts(expand_braces_masked(&masked(&[("plain.txt", true)]), 64));
        assert_eq!(r, Some(vec!["plain.txt".into()]));
    }
}
