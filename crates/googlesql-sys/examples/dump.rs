//! Prints the decoded resolved AST of one statement: `cargo run --release --example dump -- '<sql>'`.
fn main() {
    let sql = std::env::args().nth(1).unwrap_or_else(|| "SELECT 1".into());
    let params = std::env::args().nth(2).unwrap_or_else(|| "{}".into());
    let catalog = googlesql_sys::Catalog::new(
        r#"{"tables":[{"name":"monitoring.alerts","columns":[{"name":"id","type":"STRING(36)"},{"name":"team","type":"STRING(36)"}]}]}"#,
    )
    .expect("catalog");
    match catalog.analyze(&sql, &params) {
        Ok(stmt) => println!("{stmt:#?}"),
        Err(e) => eprintln!("{e}"),
    }
}
