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

/// A word reduced to its literal text. `glob_ok`/`brace_ok` are true only when
/// the respective metacharacters appeared *unquoted* — `rm '*.bak'` must not
/// glob and `rm '{a,b}'` must not brace-expand.
struct Lit {
    text: String,
    glob_ok: bool,
    brace_ok: bool,
}

#[derive(Clone)]
struct PosArg {
    text: String,
    glob_ok: bool,
    brace_ok: bool,
}

enum ArgVal {
    Lit(Lit),
    Opaque,
}

enum Tok {
    Flag(String),
    Pos(PosArg),
}

/// What the next positional token should become, given the preceding flag.
enum Consume {
    No,
    Drop,
    Path,
}

/// Cap on brace-expansion fan-out; beyond it the scope is treated as unknown
/// rather than materializing a huge path list.
const MAX_BRACE_EXPANSION: usize = 1024;

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
        let name = name_lit.text;

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
                ArgVal::Lit(lit) => {
                    if !after_double_dash && lit.text == "--" {
                        after_double_dash = true;
                    } else if !after_double_dash && lit.text.len() > 1 && lit.text.starts_with('-')
                    {
                        tokens.push(Tok::Flag(lit.text.clone()));
                    } else {
                        tokens.push(Tok::Pos(PosArg {
                            text: lit.text.clone(),
                            glob_ok: lit.glob_ok,
                            brace_ok: lit.brace_ok,
                        }));
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
            Tok::Pos(p) => Some(p.text.as_str()),
            Tok::Flag(_) => None,
        });
        let Some(rule) = self.registry.lookup_command(&name, sub, &flags) else {
            self.out.mark_unknown();
            return;
        };
        let sev = Severity::from(rule.effect);
        self.out.contribute(sev, &rule.id);

        // separate positionals from flag-carried values. `flag_args` flags
        // consume a non-path value (dropped); `path_flags` flags consume a
        // path value (captured), in separate or attached (`=`) form.
        let empty: &[String] = &[];
        let (flag_args, path_flags) = rule
            .scope
            .as_ref()
            .map(|s| (s.flag_args.as_slice(), s.path_flags.as_slice()))
            .unwrap_or((empty, empty));
        let mut positionals: Vec<PosArg> = Vec::new();
        let mut flag_paths: Vec<PosArg> = Vec::new();
        let mut consume: Consume = Consume::No;
        for tok in &tokens {
            match tok {
                Tok::Flag(f) => {
                    if let Some((name, val)) = f.split_once('=') {
                        // attached form: only path_flags carry a path
                        if path_flags.iter().any(|pf| pf == name) {
                            flag_paths.push(PosArg {
                                text: val.to_string(),
                                glob_ok: val.contains(GLOB_CHARS),
                                brace_ok: val.contains('{'),
                            });
                        }
                    } else if path_flags.iter().any(|pf| pf == f) {
                        consume = Consume::Path;
                    } else if flag_args.iter().any(|fa| fa == f) {
                        consume = Consume::Drop;
                    }
                }
                Tok::Pos(p) => match std::mem::replace(&mut consume, Consume::No) {
                    Consume::Path => flag_paths.push(p.clone()),
                    Consume::Drop => {}
                    Consume::No => positionals.push(p.clone()),
                },
            }
        }

        let mut contributed = self.extract_scope(rule, &positionals, cur);
        if let Some(scope) = &rule.scope {
            for pv in &flag_paths {
                contributed += self.add_path(pv, scope.globs, cur);
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

    fn extract_scope(
        &mut self,
        rule: &Rule,
        positionals: &[PosArg],
        cur: &Option<PathBuf>,
    ) -> usize {
        let Some(scope) = &rule.scope else { return 0 };
        let skip = usize::from(rule.matcher.subcommand.is_some()) + scope.skip;
        let path_args: Vec<&PosArg> = positionals.iter().skip(skip).collect();
        let mut contributed = 0usize;
        match scope.paths {
            PathSource::Positional => {
                for pa in path_args.iter().copied() {
                    contributed += self.add_path(pa, scope.globs, cur);
                }
            }
            PathSource::PositionalLast => {
                if let Some(pa) = path_args.last().copied() {
                    contributed += self.add_path(pa, scope.globs, cur);
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

    /// Resolve one path argument, applying bash expansion order: brace, then
    /// tilde/normalize, then glob. Returns how many paths were added.
    fn add_path(&mut self, pa: &PosArg, globs: bool, cur: &Option<PathBuf>) -> usize {
        if pa.text.is_empty() {
            return 0;
        }
        // brace expansion happens before all other expansions in bash
        let words = if pa.brace_ok && pa.text.contains('{') {
            match expand_braces(&pa.text, MAX_BRACE_EXPANSION) {
                Some(list) => list,
                None => {
                    // fan-out too large: scope is unresolvable
                    self.out.mark_unknown();
                    return 0;
                }
            }
        } else {
            vec![pa.text.clone()]
        };
        let mut total = 0;
        for word in words {
            total += self.add_single(&word, pa.glob_ok, globs, cur);
        }
        total
    }

    /// One fully brace-expanded word → resolved path(s), with optional globbing.
    fn add_single(
        &mut self,
        text: &str,
        glob_ok: bool,
        globs: bool,
        cur: &Option<PathBuf>,
    ) -> usize {
        if text.is_empty() {
            return 0;
        }
        let Some(resolved) = self.resolve_path(text, cur) else {
            return 0;
        };
        if glob_ok && globs {
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
            ArgVal::Lit(lit) => {
                !(lit.text.len() > 1 && lit.text.starts_with('-')) && lit.text != "--"
            }
            ArgVal::Opaque => true,
        });
        match target {
            None => *cur = Some(normalize_lexical(self.ctx.home)),
            Some(ArgVal::Lit(lit)) if lit.text == "-" => {
                // previous directory is untracked
                self.out.mark_unknown();
                *cur = None;
            }
            Some(ArgVal::Lit(lit)) => match self.resolve_path(&lit.text, cur) {
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
        let Some(lit) = self.extract_word(raw) else {
            self.out.mark_unknown();
            return;
        };
        // numeric target is an fd, not a file
        if lit.text.is_empty() || lit.text.chars().all(|ch| ch.is_ascii_digit()) {
            return;
        }
        let Some(rule) = self.registry.lookup_redirect(op) else {
            self.out.mark_unknown();
            return;
        };
        self.out.contribute(Severity::from(rule.effect), &rule.id);
        if let Some(p) = self.resolve_path(&lit.text, cur) {
            self.out.paths.insert(p);
        }
    }

    fn extract_word(&self, raw: &str) -> Option<Lit> {
        let pieces = brush_parser::word::parse(raw, &self.options).ok()?;
        pieces_to_literal(&pieces, 0)
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

fn pieces_to_literal(pieces: &[WordPieceWithSource], depth: usize) -> Option<Lit> {
    if depth > MAX_LITERAL_DEPTH {
        return None;
    }
    let mut text = String::new();
    let mut glob_ok = false;
    let mut brace_ok = false;
    for pw in pieces {
        match &pw.piece {
            WordPiece::Text(s) => {
                if s.contains(GLOB_CHARS) {
                    glob_ok = true;
                }
                if s.contains('{') {
                    brace_ok = true;
                }
                text.push_str(s);
            }
            WordPiece::SingleQuotedText(s) | WordPiece::AnsiCQuotedText(s) => text.push_str(s),
            WordPiece::DoubleQuotedSequence(inner)
            | WordPiece::GettextDoubleQuotedSequence(inner) => {
                // quoted metacharacters don't expand: inner flags discarded
                let lit = pieces_to_literal(inner, depth + 1)?;
                text.push_str(&lit.text);
            }
            WordPiece::EscapeSequence(s) => {
                text.push_str(s.strip_prefix('\\').unwrap_or(s));
            }
            WordPiece::TildeExpansion(TildeExpr::Home) => text.push('~'),
            // ~user / ~+ / ~- and all expansions are unresolvable statically
            _ => return None,
        }
    }
    Some(Lit {
        text,
        glob_ok,
        brace_ok,
    })
}

/// Bash-style brace expansion. `Some(list)` on success — a word with no
/// expandable brace group yields `[input]`. `None` if the fan-out would exceed
/// `limit`, so the caller treats the scope as unknown.
fn expand_braces(text: &str, limit: usize) -> Option<Vec<String>> {
    let mut out = Vec::new();
    if expand_braces_rec(text, &mut out, limit) {
        Some(out)
    } else {
        None
    }
}

fn expand_braces_rec(text: &str, out: &mut Vec<String>, limit: usize) -> bool {
    if out.len() > limit {
        return false;
    }
    let chars: Vec<char> = text.chars().collect();
    for i in 0..chars.len() {
        if chars[i] != '{' {
            continue;
        }
        match parse_brace(&chars, i, limit) {
            BraceParse::Expand(close, options) => {
                let pre: String = chars[..i].iter().collect();
                let post: String = chars[close + 1..].iter().collect();
                for opt in options {
                    if !expand_braces_rec(&format!("{pre}{opt}{post}"), out, limit) {
                        return false;
                    }
                }
                return true;
            }
            BraceParse::Overflow => return false,
            BraceParse::NotHere => {}
        }
    }
    out.push(text.to_string());
    out.len() <= limit
}

enum BraceParse {
    Expand(usize, Vec<String>),
    Overflow,
    NotHere,
}

fn parse_brace(chars: &[char], open: usize, limit: usize) -> BraceParse {
    let mut depth = 0usize;
    let mut close = None;
    let mut commas = Vec::new();
    for (i, &c) in chars.iter().enumerate().skip(open) {
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
            options.push(chars[start..c].iter().collect::<String>());
            start = c + 1;
        }
        options.push(chars[start..close].iter().collect::<String>());
        return BraceParse::Expand(close, options);
    }
    let inner: String = chars[open + 1..close].iter().collect();
    match parse_range(&inner, limit) {
        Some(Some(options)) => BraceParse::Expand(close, options),
        Some(None) => BraceParse::Overflow,
        None => BraceParse::NotHere, // {literal}
    }
}

/// `{m..n}` / `{a..c}` with optional `..step`. Returns `Some(Some(list))` on a
/// valid range, `Some(None)` when it exceeds `limit`, `None` when the content
/// is not a range at all.
fn parse_range(inner: &str, limit: usize) -> Option<Option<Vec<String>>> {
    let parts: Vec<&str> = inner.split("..").collect();
    if parts.len() != 2 && parts.len() != 3 {
        return None;
    }
    let step: i64 = if parts.len() == 3 {
        let s: i64 = parts[2].parse().ok()?;
        if s == 0 { return None } else { s.abs() }
    } else {
        1
    };

    // numeric range (with simple zero-pad support)
    if let (Ok(a), Ok(b)) = (parts[0].parse::<i64>(), parts[1].parse::<i64>()) {
        let count = ((a - b).unsigned_abs() as usize) / (step as usize) + 1;
        if count > limit {
            return Some(None);
        }
        let width = if (parts[0].starts_with('0') && parts[0].len() > 1)
            || (parts[1].starts_with('0') && parts[1].len() > 1)
        {
            parts[0].len().max(parts[1].len())
        } else {
            0
        };
        let mut v = Vec::with_capacity(count);
        let mut cur = a;
        while (a <= b && cur <= b) || (a > b && cur >= b) {
            v.push(if width > 0 {
                format!("{cur:0width$}")
            } else {
                cur.to_string()
            });
            cur += if a <= b { step } else { -step };
        }
        return Some(Some(v));
    }

    // single-character alphabetic range
    let (sc, ec) = (
        parts[0].chars().collect::<Vec<_>>(),
        parts[1].chars().collect::<Vec<_>>(),
    );
    if sc.len() == 1 && ec.len() == 1 && sc[0].is_ascii_alphabetic() && ec[0].is_ascii_alphabetic()
    {
        let (a, b) = (sc[0] as i64, ec[0] as i64);
        let count = ((a - b).unsigned_abs() as usize) / (step as usize) + 1;
        if count > limit {
            return Some(None);
        }
        let mut v = Vec::with_capacity(count);
        let mut cur = a;
        while (a <= b && cur <= b) || (a > b && cur >= b) {
            v.push(((cur as u8) as char).to_string());
            cur += if a <= b { step } else { -step };
        }
        return Some(Some(v));
    }
    None
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
