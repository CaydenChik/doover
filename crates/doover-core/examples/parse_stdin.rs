//! Diagnostic helper: parse stdin with brush-parser (used by fuzz minimization).
//! Exit 0 always unless the parser itself crashes the process.

use std::io::Read;

fn main() {
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input).unwrap_or(0);
    let options = brush_parser::ParserOptions::default();
    match brush_parser::tokenize_str(&input) {
        Ok(tokens) => match brush_parser::parse_tokens(&tokens, &options) {
            Ok(_) => println!("parsed: ok"),
            Err(e) => println!("parse error: {e}"),
        },
        Err(e) => println!("tokenize error: {e}"),
    }
}
