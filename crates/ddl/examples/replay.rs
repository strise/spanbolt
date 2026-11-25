//! Applies `;;`-separated statements and prints results like the emulator oracle script.
use spanbolt_ddl::{DdlError, Schema, parse_statement};

fn main() {
    let path = std::env::args().nth(1).expect("usage: replay <file>");
    let src = std::fs::read_to_string(path).expect("read input");
    let mut schema = Schema::default();
    for s in src.split(";;").map(str::trim).filter(|s| !s.is_empty()) {
        let head: String = s.lines().next().unwrap_or("").chars().take(80).collect();
        match parse_statement(s).and_then(|st| schema.apply(&st)) {
            Ok(()) => println!("OK  {head}"),
            Err(e) => {
                let code = match e {
                    DdlError::Syntax(_) => 3,
                    DdlError::NotFound(_) => 5,
                    DdlError::Exists(_) => 6,
                    DdlError::Unimplemented(_) => 12,
                    DdlError::Internal(_) => 13,
                    DdlError::Precondition(_) | DdlError::AlreadyExists(_) => 9,
                };
                println!("ERR{code} {head} => {e}")
            }
        }
    }
    println!("=== DDL");
    for s in schema.to_ddl() {
        println!("{s}\n;;");
    }
}
