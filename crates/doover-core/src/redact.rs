//! Display-time secret redaction for journaled commands (step 8).
//!
//! The journal stores `raw_command` verbatim — the undo engine and audit
//! trail need ground truth. But `log`/`show` print those strings, and agent
//! commands routinely embed credentials (`curl -H "Authorization: …"`,
//! `--password=…`, `TOKEN=… cmd`). Everything user-facing goes through
//! [`redact`] so credentials bound the journal's disk exposure (gc prunes
//! rows by age) AND never reach a terminal or paste buffer.
//!
//! Deliberately pattern-based and conservative: mask what is very likely a
//! credential, never rewrite anything else. This is hygiene, not a DLP
//! guarantee — an exotic secret shape will get through, and that limit
//! belongs in user docs alongside the other safety-net caveats.

use std::sync::OnceLock;

const MASK: &str = "[redacted]";

/// (pattern, replacement) pairs, applied in order.
fn rules() -> &'static Vec<(regex::Regex, String)> {
    static RULES: OnceLock<Vec<(regex::Regex, String)>> = OnceLock::new();
    RULES.get_or_init(|| {
        [
            // Authorization/auth headers: everything after the colon up to a
            // closing quote (or end) is the credential, scheme included.
            (
                r#"(?i)\b(authorization\s*:\s*)[^"'\\]+"#,
                format!("${{1}}{MASK}"),
            ),
            // bare bearer tokens outside a header context
            (r"(?i)\b(bearer\s+)[A-Za-z0-9._~+/=-]+", format!("${{1}}{MASK}")),
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
        .map(|(p, r)| (regex::Regex::new(p).expect("static pattern"), r))
        .collect()
    })
}

/// Mask likely credentials in a command string for display.
pub fn redact(cmd: &str) -> String {
    let mut out = cmd.to_string();
    for (re, replacement) in rules() {
        out = re.replace_all(&out, replacement.as_str()).into_owned();
    }
    out
}
