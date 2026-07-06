//! Diagnostic (temporary, fuzz-hunt branch): parse crashers on constrained and
//! generous stacks to test the stack-overflow hypothesis.

const CRASHER: &str = "Z#`|.u` d=?M}M\nu&c?-Ii\\'~{r]&& *'\u{62fe7}~z.\u{bc6a7}(\u{104f28}'>?}Wf)3e]$f@mcf}{\u{cd4b6} ?[)mE*.d{>>)6\n$\u{f109c}dr~Ujc\"-Ca=(*`>vz𪽉}~)&oa=KC)\n\"$`$\u{98975}v`\u{6fd66}!/v\u{e001e}f";

fn parse_on_stack(stack: usize) {
    std::thread::Builder::new()
        .stack_size(stack)
        .spawn(|| {
            let mut parser = tree_sitter::Parser::new();
            parser.set_language(&tree_sitter_bash::LANGUAGE.into()).unwrap();
            let _ = parser.parse(CRASHER, None);
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn probe_small_stack_128k() {
    parse_on_stack(128 * 1024);
}

#[test]
fn probe_big_stack_64m() {
    parse_on_stack(64 * 1024 * 1024);
}
