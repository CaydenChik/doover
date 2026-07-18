//! Secret redaction for journaled commands (step 8; moved to write-time after
//! the user-#1 trial).
//!
//! Agent commands routinely embed credentials (`curl -H "Authorization: …"`,
//! `--password=…`, `TOKEN=… cmd`, `-u user:pass`, `https://u:p@host`).
//!
//! [`redact`] runs at WRITE time, in `hooks::handle_pre`, before the command
//! string is ever handed to the journal. It used to run only at display time,
//! on the theory that the journal should keep ground truth for audit. The trial
//! showed why that was wrong: `doover show` printed `Authorization: [redacted]`
//! while `journal.db` held the bearer token in plaintext, findable with
//! `strings`. It was documented — and documenting it did not help, because a
//! mask on screen is a promise. Showing one while keeping the secret buys false
//! confidence, which is worse than no redaction at all.
//!
//! Nothing functional reads the command back: undo restores from manifests, and
//! the resolver has already run by the time the row is written. The stored
//! string is display and audit metadata, so redacting it costs nothing real.
//!
//! Display still redacts too. [`redact`] is idempotent, journals written by
//! older versions still contain raw commands, and any future user-facing print
//! of `raw_command` MUST go through it.
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

/// An unquoted credential VALUE: a run of characters that are neither
/// whitespace, a quote, nor a shell metacharacter, with backslash-escaped
/// characters (`\ `, `\;`) counted as part of the value.
///
/// Bounding every mask with this guarantees redaction can neither leak a token
/// past a backslash-escaped space (`Authorization:Bearer\ TOK` used to mask only
/// `Bearer`) nor swallow a following command / redirect / separator into the
/// mask (`--password=x&&rm`, `-u a:b|tee`, `X-API-Key:k;rm` used to eat the
/// tail). Both were found by the round-2 audit as siblings of the finding-10 fix.
const CRED_VALUE: &str = r#"(?:\\.|[^\s"'\\$&|;<>()`])+"#;

/// Simple (pattern, replacement) rules, applied in order.
fn simple_rules() -> &'static Vec<(Regex, String)> {
    static RULES: OnceLock<Vec<(Regex, String)>> = OnceLock::new();
    RULES.get_or_init(|| {
        let v = CRED_VALUE;
        // Credential-bearing HTTP header names: Authorization, Proxy-
        // Authorization, Cookie, and any `X-…-Token`/`Key`/`Secret` (X-Api-Key,
        // X-Auth-Token, X-Amz-Security-Token, X-Vault-Token, …). No `\b` before
        // it, so the `-H`-glued form `-HAuthorization:…` (no space) is caught
        // too (round-3 audit found proxy-authorization leaking).
        let h = r"(?:(?:proxy-)?authorization|cookie|x-[a-z0-9-]*(?:key|token|secret))";
        let rules: Vec<(String, String)> = vec![
            // QUOTED header: the value runs to the closing quote and may contain
            // spaces, so mask from the header name to that quote. Covers
            // non-standard schemes (`AWS4-HMAC-SHA256 Credential=…`) and
            // multi-value cookies (`Cookie: a=1; b=2`) with no leak.
            (
                format!(r#"(?i)(["'])({h}\s*:\s*)[^"'\n]*"#),
                format!("${{1}}${{2}}{MASK}"),
            ),
            // Bearer token anywhere. Runs BEFORE the unquoted header rule so an
            // unquoted `Authorization: Bearer TOK` (real space) has its token
            // masked before the header rule shortens the value to the scheme
            // word (round-2 audit: that ordering was the leak). "bearer" is a
            // specific-enough word to anchor on; "basic"/"digest" are NOT (round-3
            // audit: `basic X` over-masked prose like `--mode basic production`),
            // and the header rules already cover `Authorization: Basic …` (quoted
            // → whole value; unquoted → CRED_VALUE incl. the escaped scheme space).
            (
                r"(?i)\b(bearer\s+)[A-Za-z0-9._~+/=-]+".to_string(),
                format!("${{1}}{MASK}"),
            ),
            // UNQUOTED header: mask the value token, bounded by CRED_VALUE so an
            // escaped space cannot leak and a `&&`/`;`/`|` cannot be eaten.
            (
                format!(r"(?i)({h}\s*:\s*){v}"),
                format!("${{1}}{MASK}"),
            ),
            // secret-bearing flags: --password=x, --token x, --api-key=x …
            (
                format!(r#"(?i)(--?(?:password|passwd|token|api[-_]?key|secret|access[-_]?key)[=\s]+)("[^"]*"|'[^']*'|{v})"#),
                format!("${{1}}{MASK}"),
            ),
            // credential in a URL query string: ?api_key=… / &token=… . Common
            // when an agent calls an API by URL; the value never reaches the
            // userinfo rule (which only handles user:pass@host).
            (
                format!(r"(?i)([?&](?:api[-_]?key|apikey|access[-_]?token|auth[-_]?token|client[-_]?secret|token|secret|password|passwd|sig|signature)=){v}"),
                format!("${{1}}{MASK}"),
            ),
            // env-style assignment whose NAME says credential.
            (
                format!(r#"\b([A-Za-z_][A-Za-z0-9_]*(?:SECRET|TOKEN|PASSWORD|PASSWD|API_?KEY|CREDENTIALS?)[A-Za-z0-9_]*)=("[^"]*"|'[^']*'|{v})"#),
                format!("${{1}}={MASK}"),
            ),
        ];
        rules
            .into_iter()
            .map(|(p, r)| (Regex::new(&p).expect("static pattern"), r))
            .collect()
    })
}

/// `-u user:pass` / `--user user:pass` (curl-style basic auth). Masks the
/// password half only when the value looks like a credential — NOT when it is
/// a `uid:gid` pair (docker `-u 1000:1000`) or a plain value (`ls -u file`).
fn basic_auth_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    // The value is bounded by CRED_VALUE, not `\S+`: `-u a:b|tee` used to fold
    // `|tee` (a pipe that overwrites a file) into the masked password half,
    // erasing it from the stored audit record (round-2 audit F2b).
    RE.get_or_init(|| {
        Regex::new(&format!(r"(?i)(^|\s)(-u|--user)(\s+)({CRED_VALUE})")).expect("static pattern")
    })
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
