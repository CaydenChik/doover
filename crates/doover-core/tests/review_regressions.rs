//! Adversarial-review regressions (2026-07-14, round 2).
//!
//! The 0.2.0 trial fixes went through a multi-lens adversarial review before
//! shipping. It found 15 verified defects — 8 of them in the `readonly.yaml`
//! that had been added ONE HOUR earlier to reduce over-snapshotting. The file
//! let in a whole family of commands that read by default but WRITE A FILE via
//! a flag: the exact "looks harmless, isn't" class its own header claimed to
//! exclude. Every one is reproduced here.
//!
//! The lesson stacks on the trial's: a `safe` rule is a promise not to
//! snapshot, so admitting one is as dangerous as any code change, and "I read
//! the man page" is not the same as "I ran it on both BSD and GNU". These were
//! confirmed by actually executing the commands in throwaway dirs.

use doover_core::redact::redact;
use doover_core::registry::Registry;
use doover_core::resolver::{Ctx, Severity, resolve};
use std::path::Path;

fn classify(cmd: &str) -> (Severity, bool) {
    let reg = Registry::builtin().unwrap();
    let ctx = Ctx {
        cwd: Path::new("/proj"),
        home: Path::new("/home/u"),
    };
    let r = resolve(cmd, &reg, &ctx);
    (r.severity, r.has_unknown)
}

/// A command that can destroy data must be snapshotted — either precisely
/// (severity >= Destructive with a captured path) or defensively (has_unknown
/// routes it to the cwd snapshot). What it must NOT be is `Safe`, which means
/// "do not snapshot" and has no recovery path.
fn assert_protected(cmd: &str) {
    let (s, unknown) = classify(cmd);
    assert!(
        s != Severity::Safe,
        "`{cmd}` classified SAFE — no snapshot, unrecoverable data loss"
    );
    assert!(
        s >= Severity::Destructive || unknown,
        "`{cmd}` is not snapshotted (severity={s:?}, has_unknown={unknown})"
    );
}

fn assert_safe(cmd: &str) {
    let (s, unknown) = classify(cmd);
    assert_eq!(s, Severity::Safe, "`{cmd}` should stay safe, got {s:?}");
    assert!(!unknown, "`{cmd}` should not trigger a defensive snapshot");
}

fn resolved_paths(cmd: &str) -> Vec<String> {
    let reg = Registry::builtin().unwrap();
    let ctx = Ctx {
        cwd: Path::new("/work"),
        home: Path::new("/home/u"),
    };
    let r = resolve(cmd, &reg, &ctx);
    let mut p: Vec<String> = r.paths.iter().map(|p| p.display().to_string()).collect();
    p.sort();
    p
}

// --- ROUND 2 (DATA-LOSS): a flag-value target that itself looks like a flag ---
//
// The resolver's path-flag consume was "sticky": `--output -weird` sets a
// pending Path-consume, but the flag-shaped token `-weird` was not claimed by
// it, so the consume latched onto a LATER positional and captured the WRONG
// file — git then truncates `-weird` while doover snapshotted something else,
// and because a (bogus) path WAS captured the cwd fallback did not engage, so
// the real write target went unprotected (round-2 audit F4a). A dash-named
// output file is unusual but legal, and the write is unrecoverable.
#[test]
fn a_flag_shaped_output_filename_is_captured_not_a_later_positional() {
    // the value of --output is `-weird`, even though it looks like a flag
    assert_eq!(
        resolved_paths("git log --output -weird laterpositional"),
        vec!["/work/-weird".to_string()],
        "the file literally named -weird must be captured, not a later arg"
    );
    // the sort companion has the same shape; it also captures positional inputs
    // (paths: positional), harmless over-capture — the point is -dashfile IS in
    // the set, not latched away onto in.txt alone.
    assert_eq!(
        resolved_paths("sort --output -dashfile in.txt"),
        vec!["/work/-dashfile".to_string(), "/work/in.txt".to_string()],
    );
    // and the ordinary forms still capture correctly
    assert_eq!(
        resolved_paths("git log --output out.txt"),
        vec!["/work/out.txt".to_string()],
    );
    assert_eq!(
        resolved_paths("git log --output=out.txt"),
        vec!["/work/out.txt".to_string()],
    );
}

// --- FINDING 1 (CRITICAL): git grep -O executes an arbitrary command --------
//
// `git grep -O<pager> needle` opens matched files in <pager>, which is run as
// a shell command. `git grep -O'sh -c "rm -rf ."' needle` deletes the tree.
// It is a command WRAPPER, exactly like env/xargs/sudo, and marking it safe
// snapshots nothing. Reproduced: victim.txt overwritten, a.txt deleted, exit 0.
#[test]
fn git_grep_open_pager_is_never_safe() {
    assert_protected("git grep -O'sh -c \"rm -rf .\"' needle");
    assert_protected("git grep -Osh needle");
    assert_protected("git grep --open-files-in-pager=sh needle");
    // the common, genuinely read-only form must still be free
    assert_safe("git grep needle");
    assert_safe("git grep -n -i needle src/");
}

// --- FINDING 2 (CRITICAL): base64 -o / --output truncates a file ------------
//
// BSD/macOS base64 (the binary this ships against) writes its output to the
// file named by -o/--output. `echo x | base64 -o important.txt` truncated a
// file of user data. Same output-positional/flag class as sort -o.
#[test]
fn base64_output_flag_is_never_safe() {
    assert_protected("base64 -o important.txt in.bin");
    assert_protected("base64 --output important.txt in.bin");
    assert_protected("base64 --output=important.txt in.bin");
    assert_protected("base64 -oimportant.txt in.bin");
    // plain decode/encode to stdout stays free
    assert_safe("base64 in.bin");
    assert_safe("base64 -d in.b64");
}

// --- FINDINGS 3-7 (CRITICAL/HIGH): git <diff-cmd> --output=<file> -----------
//
// `--output=<file>` is a git diff-generation option honored by every command
// that produces diff output. It truncates the named file. Confirmed: `git log
// --output=out.txt -p` overwrote the file; `git diff --output=out.txt HEAD`
// emptied it. This includes the pre-existing git.log / git.diff / git.show
// safe rules in git.yaml (finding 7), so the bug was already latent and the
// new readonly.yaml git rules widened it.
#[test]
fn git_output_flag_is_never_safe() {
    for sub in [
        "log",
        "diff",
        "show",
        "blame",
        "diff-tree",
        "rev-list",
        "shortlog",
    ] {
        assert_protected(&format!("git {sub} --output=important.txt"));
        assert_protected(&format!("git {sub} --output important.txt"));
    }
    // the read-only forms without --output stay free
    assert_safe("git log -p");
    assert_safe("git log --oneline -20");
    assert_safe("git diff HEAD~1");
    assert_safe("git show HEAD");
    assert_safe("git blame src/main.rs");
    assert_safe("git shortlog -sn");
}

// --- DATA-LOSS: -t/--target-directory write target across cp/ln/mv/install ---
//
// A `-t DIR`/`--target-directory=DIR` moves the destination into a FLAG value
// while the positionals become SOURCES. cp/ln (`positional-last`) captured a
// source and missed DIR entirely; mv/install (`paths: positional`) captured the
// SEPARATE `-t DIR` form but missed the ATTACHED `--target-directory=DIR` form.
// Every case overwrites files in DIR with no snapshot. Companion rules capture
// DIR in every form (rounds 3-4).
#[test]
fn target_directory_flag_captures_the_destination_for_cp_ln_mv_install() {
    for (cmd, want) in [
        ("cp -t /dest src.txt", "/dest"),
        ("cp --target-directory=/dest a b", "/dest"),
        ("ln -t /dest a b", "/dest"),
        ("ln -sf -t /dest a", "/dest"),
        ("ln --target-directory=/dest a", "/dest"),
        ("mv -t /dest a b", "/dest"),
        ("mv --target-directory=/dest a", "/dest"),
        ("install -t /dest src", "/dest"),
        ("install --target-directory=/dest src", "/dest"),
    ] {
        assert!(
            resolved_paths(cmd).contains(&want.to_string()),
            "`{cmd}` must capture the target dir {want}, got {:?}",
            resolved_paths(cmd)
        );
    }
    // ordinary two-arg forms still capture the destination
    assert!(resolved_paths("cp a b").contains(&"/work/b".to_string()));
    assert!(resolved_paths("mv a b").contains(&"/work/b".to_string()));
    // mv still captures the removed sources too (paths: positional)
    assert!(resolved_paths("mv -t /dest a b").contains(&"/work/a".to_string()));
}

// --- ROUND 4 (audit-loss): the round-3 `basic` scheme word over-masked prose --
//
// Adding `basic` to the bearer rule masked any `basic <word>`, corrupting the
// audit record for common phrases (`--mode basic production`). `basic`/`digest`
// are too common to anchor on; the header rules already cover
// `Authorization: Basic …`. Removed from the bearer rule.
#[test]
fn redaction_does_not_over_mask_the_word_basic() {
    assert!(
        redact("npm run build --mode basic production").contains("production"),
        "over-masked prose after 'basic'"
    );
    assert!(redact("echo basic auth flow").contains("auth"));
    // but a real Authorization: Basic header is still masked
    let a = redact("curl -H \"Authorization: Basic dXNlcjpwYXNz\" https://x");
    assert!(
        !a.contains("dXNlcjpwYXNz"),
        "auth basic must still be masked: {a}"
    );
}

// --- ROUND 3 (secret-leak): Proxy-Authorization / Basic scheme ---------------
#[test]
fn redaction_masks_proxy_authorization_and_basic() {
    let a = redact("curl -H \"Proxy-Authorization: Basic dXNlcjpwYXNzMTIz\" https://x");
    assert!(
        !a.contains("dXNlcjpwYXNzMTIz"),
        "leaked proxy-auth basic: {a}"
    );
    let b = redact("curl -H Authorization:Basic\\ dXNlcjpTM2Nu https://x");
    assert!(!b.contains("dXNlcjpTM2Nu"), "leaked unquoted basic: {b}");
}

// --- ROUND 2 (CRITICAL): find's write predicates truncate a file ------------
//
// Found by the round-2 exhaustive re-audit, not the review: `find`'s
// -fprint/-fprint0/-fprintf/-fls predicates each take a FILE argument and
// TRUNCATE it. Confirmed on BSD find: `find . -fprint victim.txt` overwrote a
// file of user data with the listing. The bare `posix.find` rule is safe (no
// snapshot), and only -delete/-exec were companioned, so these writes were
// silent and unrecoverable — the exact write-via-flag class the review found
// across readonly.yaml, hiding in a pre-existing rule.
#[test]
fn find_write_predicates_are_never_safe() {
    assert_protected("find . -fprint victim.txt");
    assert_protected("find . -fprint0 victim.txt");
    assert_protected("find . -fprintf victim.txt '%p\\n'");
    assert_protected("find . -fls victim.txt");
    // bare find and read-only predicates stay free
    assert_safe("find . -name '*.rs'");
    assert_safe("find . -type f -print");
}

// --- ROUND 3 (HIGH): find's exec predicates must ALL be covered --------------
//
// The companion listed -exec/-execdir/-ok but MISSED -okdir (the -execdir
// variant that prompts). `find . -okdir rm {} ;` executes rm over computed
// targets and was classifying safe -> no snapshot. Same exec class.
#[test]
fn find_exec_predicates_including_okdir_are_never_safe() {
    assert_protected("find . -exec rm {} ;");
    assert_protected("find . -execdir rm {} ;");
    assert_protected("find . -ok rm {} ;");
    assert_protected("find . -okdir rm {} ;");
}

// --- FINDING 11 (MEDIUM): file -C writes a compiled magic file --------------
//
// `file -C -m <name>` compiles and writes `<name>.mgc`, overwriting it.
// Confirmed: a 752-byte binary landed on a file of user data.
#[test]
fn file_compile_flag_is_never_safe() {
    assert_protected("file -C -m important");
    assert_protected("file -C");
    // plain type detection stays free
    assert_safe("file important.txt");
    assert_safe("file -b -i photo.jpg");
}

// --- FINDING 10 (HIGH): write-time redaction must not eat the command -------
//
// The Authorization / X-API-Key rules matched `[^"'\\]+` — up to the next
// quote or END OF STRING. For an UNQUOTED header value (legal curl) that is
// the whole rest of the command, so redacting at write time stored a truncated
// row and destroyed the only copy of the destructive tail. The redacted form
// must never cross whitespace into a following argument or command separator.
#[test]
fn redaction_never_swallows_the_rest_of_the_command() {
    // the exact scenario from the finding: unquoted header, then `&& rm -rf`
    let r = redact("curl -H Authorization:Bearer_TOK https://x/y && rm -rf ./build");
    assert!(
        r.contains("rm -rf ./build"),
        "redaction ate the destructive tail: {r}"
    );
    assert!(
        !r.contains("Bearer_TOK"),
        "the token must still be masked: {r}"
    );

    let r2 = redact("curl -H X-API-Key:sk-live-abc https://api.example.com/v1 -o report.json");
    assert!(
        r2.contains("-o report.json"),
        "redaction ate the output flag: {r2}"
    );
    assert!(!r2.contains("sk-live-abc"), "the key must be masked: {r2}");

    // a bearer token with the scheme word (a space in the value) is still fully
    // masked, and still does not cross into the next argument
    let r3 = redact("curl -H \"Authorization: Bearer sk-XYZ\" https://x && rm f");
    assert!(
        !r3.contains("sk-XYZ"),
        "quoted bearer token must be masked: {r3}"
    );
    assert!(
        r3.contains("rm f"),
        "must not cross the closing quote: {r3}"
    );
}

/// ROUND 2: the finding-10 fix had unbounded siblings. Every credential-value
/// capture must stop at shell metacharacters (so a `&&rm`/`|tee`/`;rm` is never
/// eaten from the audit record) and count backslash-escapes as value (so a
/// token after `\ ` cannot leak). Recovery is never at stake here (resolve runs
/// on the raw command), but a mangled audit record and a persisted secret are.
#[test]
fn credential_masking_never_eats_a_command_or_leaks_past_a_backslash() {
    // F2a: secret flag glued to a chained command
    let a = redact("tool --password=abc&&rm -rf x");
    assert!(a.contains("rm -rf x"), "ate the chained rm: {a}");
    assert!(!a.contains("abc"), "password not masked: {a}");

    // F2b: -u basic auth glued to a pipe / separator
    let b = redact("curl -u a:b|tee /etc/passwd");
    assert!(b.contains("|tee /etc/passwd"), "ate the pipe-to-tee: {b}");
    let b2 = redact("curl -u user:pass;rm -rf y");
    assert!(b2.contains("rm -rf y"), "ate the chained rm: {b2}");

    // F2c: bearer token after a backslash-escaped space (realistic one-arg header)
    let c = redact("curl -H Authorization:Bearer\\ sk-LEAKC https://x");
    assert!(
        !c.contains("sk-LEAKC"),
        "leaked token past the backslash: {c}"
    );
    assert!(c.contains("https://x"), "ate the URL: {c}");

    // F2e: credential in a URL query string
    let e = redact("curl https://api.example.com/v1?api_key=sk-LEAKE&x=1");
    assert!(!e.contains("sk-LEAKE"), "leaked URL-query api_key: {e}");
    assert!(e.contains("x=1"), "ate the rest of the query: {e}");
}

/// A regression I introduced fixing finding 10 and then caught by asking "did I
/// make it worse": a scheme allow-list masked only the scheme word of a QUOTED
/// header and leaked the token of any non-listed scheme — and at write time
/// that leak is now permanent. A quoted header value must be masked in full
/// regardless of scheme.
/// ROUND 3: broaden which credential-bearing headers are masked. Pre-existing
/// gaps (not regressions), but common: the `-H`-glued form, AWS's
/// X-Amz-Security-Token, and Cookie headers all persisted a secret at write
/// time. Redaction is hygiene not DLP, but these shapes are what agents produce.
#[test]
fn redaction_masks_glued_headers_amz_tokens_and_cookies() {
    for (cmd, secret) in [
        // -H glued to the header name with no space
        (
            "curl -HAuthorization:Bearer\\ sk-GLUED https://x",
            "sk-GLUED",
        ),
        // AWS temporary-credential header
        (
            "curl -H \"X-Amz-Security-Token: sk-AMZTOKEN\" https://s3",
            "sk-AMZTOKEN",
        ),
        // a session cookie
        (
            "curl -H \"Cookie: session=sk-COOKIE; theme=dark\" https://x",
            "sk-COOKIE",
        ),
        // an unquoted single-value cookie
        ("curl -H Cookie:sk-BARE https://x", "sk-BARE"),
    ] {
        let out = redact(cmd);
        assert!(
            !out.contains(secret),
            "leaked `{secret}`:\n  in:  {cmd}\n  out: {out}"
        );
    }
    // and a non-secret X- header is left alone (no over-mask of routing headers)
    let keep = redact("curl -H \"X-Request-Id: abc-123\" https://x");
    assert!(
        keep.contains("abc-123"),
        "over-masked a non-credential header: {keep}"
    );
}

#[test]
fn redaction_does_not_leak_nonstandard_auth_schemes() {
    for (cmd, secret) in [
        (
            "curl -H \"Authorization: AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE\" https://s3",
            "AKIAIOSFODNN7EXAMPLE",
        ),
        (
            "curl -H 'Authorization: Custom myrealsecrettoken12345' https://x",
            "myrealsecrettoken12345",
        ),
        (
            "curl -H \"X-API-Key: multi word key value\" https://x",
            "word key value",
        ),
    ] {
        let out = redact(cmd);
        assert!(
            !out.contains(secret),
            "leaked `{secret}` from a quoted header:\n  in:  {cmd}\n  out: {out}"
        );
    }
}

// --- FINDING 14 (LOW): redact() must be idempotent --------------------------
//
// The design redacts at write time AND again at display time, so redact must
// be a fixed point: redact(redact(x)) == redact(x). If not, `doover show`
// prints something different from the stored row — exactly the kind of
// screen-vs-disk mismatch that hid the original plaintext-secret bug.
#[test]
fn redact_is_idempotent_on_representative_commands() {
    for cmd in [
        "curl -H Authorization:Bearer_TOK https://x/y && rm -rf ./build",
        "curl -H \"Authorization: Bearer sk-XYZ\" https://x",
        "curl -H X-API-Key:sk-live-abc https://api -o out.json",
        "curl -u user:hunter2 https://x",
        "psql postgres://user:secret@host/db",
        "AWS_SECRET_ACCESS_KEY=abc123 aws s3 ls",
        "git push origin main",
        "docker run -u 1000:1000 img",
    ] {
        let once = redact(cmd);
        let twice = redact(&once);
        assert_eq!(
            once, twice,
            "redact not idempotent on `{cmd}`:\n  1x: {once}\n  2x: {twice}"
        );
    }
}
