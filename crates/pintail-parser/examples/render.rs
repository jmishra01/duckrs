//! Prints the AST rendering of each SQL line read from stdin (or an error).
//!
//! `cargo run -p pintail-parser --example render < queries.sql`

use std::io::BufRead;

fn main() {
    for line in std::io::stdin().lock().lines() {
        let line = line.expect("stdin is readable");
        if line.trim().is_empty() {
            continue;
        }
        match pintail_parser::parse_ast(&line) {
            Ok(statements) => {
                for s in statements {
                    println!("{s}");
                }
            }
            Err(e) => println!("ERROR: {e}"),
        }
    }
}
