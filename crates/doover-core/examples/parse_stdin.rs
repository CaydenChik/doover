//! Diagnostic helper (fuzz-hunt): parse stdin bytes with tree-sitter-bash.
//! Exit 0 on success; a segfault (139) marks the input as a crasher.

use std::io::Read;

fn main() {
    let mut input = Vec::new();
    std::io::stdin().read_to_end(&mut input).unwrap();
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_bash::LANGUAGE.into())
        .unwrap();
    let tree = parser.parse(&input, None);
    println!("parsed: {}", tree.is_some());
}
