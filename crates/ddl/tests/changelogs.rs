//! Every DDL statement in a set of Liquibase SQL changelogs must parse and round-trip.
//! `SPANBOLT_CHANGELOGS` lists directories (searched for `*.sql`) and files, separated by `:`;
//! skipped when it is unset or names no SQL files.

use spanbolt_ddl::{is_ddl, parse_statement, print_statement, split_statements};
use std::path::{Path, PathBuf};

fn sql_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            sql_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "sql") {
            out.push(p);
        }
    }
}

fn substitute(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut rest = sql;
    while let Some(i) = rest.find("${") {
        out.push_str(&rest[..i]);
        match rest[i..].find('}') {
            Some(j) => {
                out.push_str("placeholder");
                rest = &rest[i + j + 1..];
            }
            None => {
                out.push_str(&rest[i..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

#[test]
fn changelog_ddl_parses() {
    let Ok(paths) = std::env::var("SPANBOLT_CHANGELOGS") else {
        eprintln!("SPANBOLT_CHANGELOGS not set; skipping");
        return;
    };
    let mut files = Vec::new();
    for p in std::env::split_paths(&paths) {
        if p.is_file() {
            files.push(p);
        } else {
            sql_files(&p, &mut files);
        }
    }
    if files.is_empty() {
        eprintln!("no .sql files under SPANBOLT_CHANGELOGS={paths}; skipping");
        return;
    }
    let (mut ddl, mut other, mut failures) = (0, 0, Vec::new());
    for f in &files {
        let src = substitute(&std::fs::read_to_string(f).unwrap());
        // A `--changeset` header also ends the previous statement.
        let mut chunks = vec![String::new()];
        for line in src.lines() {
            if line.trim_start().starts_with("--changeset") {
                chunks.push(String::new());
            }
            let last = chunks.last_mut().unwrap();
            last.push_str(line);
            last.push('\n');
        }
        for stmt in chunks.iter().flat_map(|c| split_statements(c)) {
            if !is_ddl(&stmt) {
                other += 1;
                continue;
            }
            ddl += 1;
            match parse_statement(&stmt) {
                Ok(parsed) => {
                    let printed = print_statement(&parsed);
                    match parse_statement(&printed) {
                        Ok(again) if again == parsed => {}
                        Ok(_) => failures.push(format!("round trip differs: {printed}")),
                        Err(e) => failures.push(format!("reparse: {e}")),
                    }
                }
                Err(e) => failures.push(format!("{}: {e}", f.display())),
            }
        }
    }
    eprintln!(
        "changelogs: {} files, {ddl} DDL statements parsed, {other} non-DDL skipped",
        files.len()
    );
    assert!(
        failures.is_empty(),
        "{} failures:\n{}",
        failures.len(),
        failures.join("\n")
    );
    assert!(ddl > 0, "no DDL statements found");
}
