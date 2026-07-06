//! Regression inputs for the Linux-only SIGSEGV found by fuzz-hunt on
//! 2026-07-06 (glibc-only manifestation; macOS unaffected). Each string is the
//! exact last-printed input of a crashed CI fuzz job.

const CRASHERS: &[&str] = &[
    // seed 1, INPUT 3
    "Z#`|.u` d=?M}M\nu&c?-Ii\\'~{r]&& *'\u{62fe7}~z.\u{bc6a7}(\u{104f28}'>?}Wf)3e]$f@mcf}{\u{cd4b6} ?[)mE*.d{>>)6\n$\u{f109c}dr~Ujc\"-Ca=(*`>vz𪽉}~)&oa=KC)\n\"$`$\u{98975}v`\u{6fd66}!/v\u{e001e}f",
    // seed 2, INPUT 32
    "o\u{f4bd9}; {\u{d5a22}7A>d?`j?}#%:& \u{4c87d}mi\n:\n#(\ncp\u{cb57c}.~>[.dap%(}Ua{mV\n \u{37940}i.`W&/>\u{16433}Y\u{338b7};\u{b7f02}s?*J#g/麼{v/r\\\n\\]\u{8feed}.\u{d0d78}c&G\u{34a43}L<[&3*imB\n5\\P ;z\n;J\u{aadbd}{(.\u{f17e8}9!.r|\"C$a=q]f[-&m?iZvHd\\..Hm./rm&k[\n~#\u{8f442}y[J'p\n",
];

/// Stage 1: is the crash inside tree-sitter's parse itself?
#[test]
fn parse_only_survives_crashers() {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_bash::LANGUAGE.into())
        .unwrap();
    for (i, input) in CRASHERS.iter().enumerate() {
        let tree = parser.parse(input, None);
        assert!(tree.is_some(), "parse returned None for crasher {i}");
    }
}

/// Stage 2: full resolution over the same inputs.
#[test]
fn full_resolve_survives_crashers() {
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
        let _ = resolve(input, &registry, &ctx);
    }
}
