//! Regression inputs from the 2026-07-06 fuzz-hunt. These segfaulted the
//! process under the original tree-sitter-bash backend (heap-layout-dependent
//! memory corruption in its C error recovery, glibc-manifesting; 7-byte
//! minimized repro below). The parser was replaced with pure-Rust
//! brush-parser; these inputs must now resolve — conservatively — on a
//! default-sized caller stack, forever.

/// The delta-minimized trigger: two single quotes, an open brace, and one
/// astral-plane character.
const MINIMIZED: &str = "''{\u{cd4b6}";

/// Exact last-printed inputs of crashed CI fuzz jobs.
const CRASHERS: &[&str] = &[
    MINIMIZED,
    // seed 1, INPUT 3
    "Z#`|.u` d=?M}M\nu&c?-Ii\\'~{r]&& *'\u{62fe7}~z.\u{bc6a7}(\u{104f28}'>?}Wf)3e]$f@mcf}{\u{cd4b6} ?[)mE*.d{>>)6\n$\u{f109c}dr~Ujc\"-Ca=(*`>vz𪽉}~)&oa=KC)\n\"$`$\u{98975}v`\u{6fd66}!/v\u{e001e}f",
    // seed 2, INPUT 32
    "o\u{f4bd9}; {\u{d5a22}7A>d?`j?}#%:& \u{4c87d}mi\n:\n#(\ncp\u{cb57c}.~>[.dap%(}Ua{mV\n \u{37940}i.`W&/>\u{16433}Y\u{338b7};\u{b7f02}s?*J#g/麼{v/r\\\n\\]\u{8feed}.\u{d0d78}c&G\u{34a43}L<[&3*imB\n5\\P ;z\n;J\u{aadbd}{(.\u{f17e8}9!.r|\"C$a=q]f[-&m?iZvHd\\..Hm./rm&k[\n~#\u{8f442}y[J'p\n",
];

/// Raw parse of the crashers must complete (Ok or Err, never a crash).
#[test]
fn raw_parse_survives_crashers() {
    let options = brush_parser::ParserOptions::default();
    for input in CRASHERS {
        if let Ok(tokens) = brush_parser::tokenize_str(input) {
            let _ = brush_parser::parse_tokens(&tokens, &options);
        }
    }
}

/// The public contract: `resolve()` survives the crashers on any caller
/// thread and classifies garbage conservatively.
#[test]
fn resolve_survives_crashers_on_default_stack() {
    use doover_core::registry::Registry;
    use doover_core::resolver::{Ctx, resolve};
    let registry = Registry::builtin().unwrap();
    let jail = tempfile::tempdir().unwrap();
    let home = jail.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let ctx = Ctx {
        cwd: jail.path(),
        home: &home,
    };
    for input in CRASHERS {
        let r = resolve(input, &registry, &ctx);
        // garbage in, conservative classification out
        assert!(
            r.has_unknown,
            "crasher input must classify with has_unknown"
        );
    }
}
