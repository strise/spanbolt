//! Times parse + apply (+ print) of the generated schema fixture (`fixtures/schema.json`, or `FIXTURE=<path>`). Run with `--release`.
use spanbolt_ddl::{Schema, parse_statement};
use std::time::Instant;

fn main() {
    let path = std::env::var("FIXTURE").unwrap_or_else(|_| {
        concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/schema.json").to_string()
    });
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).expect("fixture")).expect("json");
    let stmts: Vec<&str> = v["ddl"]
        .as_array()
        .expect("ddl array")
        .iter()
        .filter_map(|s| s.as_str())
        .collect();
    let mut best = f64::MAX;
    let mut best_print = f64::MAX;
    for _ in 0..20 {
        let t = Instant::now();
        let mut schema = Schema::default();
        for s in &stmts {
            schema
                .apply(&parse_statement(s).expect("parse"))
                .expect("apply");
        }
        best = best.min(t.elapsed().as_secs_f64() * 1e3);
        let t = Instant::now();
        let out = schema.to_ddl();
        best_print = best_print.min(t.elapsed().as_secs_f64() * 1e3);
        assert_eq!(out.len(), stmts.len());
    }
    println!(
        "{} statements: parse+apply {best:.2} ms, to_ddl {best_print:.2} ms (best of 20)",
        stmts.len()
    );
}
