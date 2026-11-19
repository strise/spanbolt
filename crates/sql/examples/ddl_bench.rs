//! The first SQL statement after a DDL on a large schema, in process:
//! `source scripts/googlesql-env.sh && DDL=<schema dump> cargo run --release -p spanbolt-sql --example ddl_bench`
//!
//! Loads a schema dump (DDL statements separated by `;` at line ends, `;;` between
//! UpdateDatabaseDdl batches) and replays its first private-person Liquibase migration (the
//! batches from `CREATE TABLE DATABASECHANGELOG_pp_<uuid>` to the next person) `PERSONS` times
//! (default 10) under fresh ids. After each batch it times the DDL apply and the first statement,
//! Liquibase's `INSERT INTO DATABASECHANGELOG_pp_<uuid>`, then the same INSERT again (warm).

use std::sync::Arc;
use std::time::{Duration, Instant};

use spanbolt_ddl::{Schema, parse_statement};
use spanbolt_proto::spanner as pb;
use spanbolt_sql::{Engine, Txn};
use spanbolt_storage::DbState;
use spanbolt_txn::{Clock, Database, ReadWriteTxn};

fn split(text: &str) -> Vec<String> {
    text.split(";\n")
        .map(|s| s.trim().trim_end_matches(';').trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1e3
}

fn summary(label: &str, v: &mut [Duration]) {
    v.sort();
    let mean = v.iter().sum::<Duration>() / v.len() as u32;
    println!(
        "{label:<22} n={:<4} mean={:>7.2} ms  p50={:>7.2} ms  p90={:>7.2} ms  max={:>7.2} ms",
        v.len(),
        ms(mean),
        ms(v[v.len() / 2]),
        ms(v[v.len() * 9 / 10]),
        ms(v[v.len() - 1]),
    );
}

fn main() {
    let path = std::env::var("DDL").expect("DDL=<schema dump>");
    let text = std::fs::read_to_string(&path).expect("read DDL");
    let persons: usize = std::env::var("PERSONS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(10);

    let mut schema = Schema::default();
    let (mut ok, mut failed) = (0, 0);
    for ddl in split(&text) {
        match parse_statement(&ddl).map(|st| schema.apply(&st)) {
            Ok(Ok(_)) => ok += 1,
            _ => failed += 1,
        }
    }
    println!(
        "{ok} DDL statements applied, {failed} skipped, {} tables",
        schema.tables.len()
    );

    // The first private-person migration in the dump is the template.
    let marker = "CREATE TABLE DATABASECHANGELOG_pp_";
    let start = text
        .find(marker)
        .expect("no private-person migration in the dump");
    let uuid_start = start + marker.len();
    let uuid: String = text[uuid_start..]
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();
    let end = text[uuid_start..]
        .find(marker)
        .map_or(text.len(), |i| uuid_start + i);
    let template: Vec<String> = text[start..end]
        .split(";;")
        .map(str::to_string)
        .filter(|b| !b.trim().is_empty())
        .collect();
    println!("template: {} DDL batches per person", template.len());

    let db = Arc::new(Database::new(
        Arc::new(Clock::default()),
        DbState::new(schema).expect("state"),
    ));
    let engine = Engine::default();
    // Warm the engine: the first catalog of the process pays one-time costs.
    let rw = Arc::new(ReadWriteTxn::new(db.clone()));
    let req = |sql: String| pb::ExecuteSqlRequest {
        sql,
        ..Default::default()
    };
    engine
        .execute_sql(&db, Txn::ReadWrite(&rw), &req("SELECT 1".into()))
        .expect("warm");
    drop(rw);

    let (mut ddl_t, mut first_t, mut warm_t) = (Vec::new(), Vec::new(), Vec::new());
    for p in 0..persons {
        let id = format!("{:08x}_0000_4000_8000_{:012x}", 0x5b047 + p, p);
        for (n, batch) in template.iter().enumerate() {
            let batch = batch.replace(&uuid, &id);
            let stmts = split(&batch);
            let t = Instant::now();
            db.update_schema(|s| {
                let mut schema = (*s.schema).clone();
                for st in &stmts {
                    let parsed = parse_statement(st)
                        .map_err(|e| spanbolt_storage::Error::internal(format!("{e:?}: {st}")))?;
                    schema
                        .apply(&parsed)
                        .map_err(|e| spanbolt_storage::Error::internal(format!("{e:?}")))?;
                }
                Ok(schema)
            })
            .expect("DDL");
            ddl_t.push(t.elapsed());
            let sql = format!(
                "INSERT INTO DATABASECHANGELOG_pp_{id} (ID, AUTHOR, FILENAME, DATEEXECUTED, \
                 ORDEREXECUTED, MD5SUM, DESCRIPTION, COMMENTS, EXECTYPE, CONTEXTS, LABELS, \
                 LIQUIBASE, DEPLOYMENT_ID) VALUES ('{n}', 'a', 'f.sql', CURRENT_TIMESTAMP(), {n}, \
                 'x', 'sql', '', 'EXECUTED', NULL, NULL, '4.29.2', '1')"
            );
            for out in [&mut first_t, &mut warm_t] {
                let rw = Arc::new(ReadWriteTxn::new(db.clone()));
                let t = Instant::now();
                engine
                    .execute_sql(&db, Txn::ReadWrite(&rw), &req(sql.clone()))
                    .expect("insert");
                out.push(t.elapsed());
            }
        }
    }
    summary("DDL apply", &mut ddl_t);
    summary("first SQL after DDL", &mut first_t);
    summary("same SQL again", &mut warm_t);
}
