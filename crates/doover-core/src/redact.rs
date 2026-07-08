//! Display-time secret redaction for journaled commands (step 8).
//!
//! The journal stores `raw_command` verbatim — the undo engine and audit
//! trail need ground truth. But `log`/`show` print those strings, and agent
//! commands routinely embed credentials (`curl -H "Authorization: …"`,
//! `--password=…`, `TOKEN=… cmd`, `-u user:pass`, `https://u:p@host`).
//! Everything user-facing goes through [`redact`] so credentials bound the
//! journal's disk exposure (gc prunes rows by age) AND never reach a terminal
//! or paste buffer.
//!
//! Deliberately pattern-based and conservative: mask what is very likely a
//! credential, and — the mirror-image failure (audit round 13) — never
//! rewrite something that only LOOKS like one (uid:gid, port maps, prose).
//! This is hygiene, not a DLP guarantee: an exotic secret shape will get
//! through, and that limit belongs in user docs alongside the other
//! safety-net caveats.

use regex::{Captures, Regex};
use std::sync::OnceLock;

const MASK: &str = "[redacted]";

/// Simple (pattern, replacement) rules, applied in order.
fn simple_rules() -> &'static Vec<(Regex, String)> {
    static RULES: OnceLock<Vec<(Regex, String)>> = OnceLock::new();
    RULES.get_or_init(|| {
        [
            // Authorization + API-key/token style headers: everything after
            // the colon up to a closing quote (or end) is the credential.
            (
                r#"(?i)\b(authorization\s*:\s*)[^"'\\]+"#,
                format!("${{1}}{MASK}"),
            ),
            (
                r#"(?i)\b(x-(?:api-key|auth-token|access-token)\s*:\s*)[^"'\\]+"#,
                format!("${{1}}{MASK}"),
            ),
            // bare bearer tokens outside a header context
            (
                r"(?i)\b(bearer\s+)[A-Za-z0-9._~+/=-]+",
                format!("${{1}}{MASK}"),
            ),
            // secret-bearing flags: --password=x, --token x, --api-key=x …
            (
                r#"(?i)(--?(?:password|passwd|token|api[-_]?key|secret|access[-_]?key)[=\s]+)("[^"]*"|'[^']*'|\S+)"#,
                format!("${{1}}{MASK}"),
            ),
            // env-style assignments whose NAME says credential
            (
                r#"\b([A-Za-z_][A-Za-z0-9_]*(?:SECRET|TOKEN|PASSWORD|PASSWD|API_?KEY|CREDENTIALS?)[A-Za-z0-9_]*)=("[^"]*"|'[^']*'|\S+)"#,
                format!("${{1}}={MASK}"),
            ),
        ]
        .into_iter()
        .map(|(p, r)| (Regex::new(p).expect("static pattern"), r))
        .collect()
    })
}

/// `-u user:pass` / `--user user:pass` (curl-style basic auth). Masks the
/// password half only when the value looks like a credential — NOT when it is
/// a `uid:gid` pair (docker `-u 1000:1000`) or a plain value (`ls -u file`).
fn basic_auth_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)(^|\s)(-u|--user)(\s+)(\S+)").expect("static pattern"))
}

/// `scheme://user:pass@host` — mask the password, keep scheme/user/host.
fn url_userinfo_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"([a-zA-Z][a-zA-Z0-9+.\-]*://[^/\s:@]+):([^/\s@]+)@").expect("static pattern")
    })
}

fn all_digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

/// Mask likely credentials in a command string for display.
pub fn redact(cmd: &str) -> String {
    let mut out = cmd.to_string();

    // URL userinfo: keep everything but the password half.
    out = url_userinfo_re()
        .replace_all(&out, |c: &Captures| format!("{}:{MASK}@", &c[1]))
        .into_owned();

    // Basic-auth flag, with uid:gid / plain-value discrimination.
    out = basic_auth_re()
        .replace_all(&out, |c: &Captures| {
            let (lead, flag, gap, val) = (&c[1], &c[2], &c[3], &c[4]);
            match val.split_once(':') {
                // user:pass, but not uid:gid → mask the password half
                Some((user, pass)) if !(all_digits(user) && all_digits(pass)) => {
                    format!("{lead}{flag}{gap}{user}:{MASK}")
                }
                // uid:gid or no colon at all → leave untouched
                _ => format!("{lead}{flag}{gap}{val}"),
            }
        })
        .into_owned();

    for (re, replacement) in simple_rules() {
        out = re.replace_all(&out, replacement.as_str()).into_owned();
    }
    out
}
