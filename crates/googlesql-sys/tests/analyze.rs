//! Analyses representative production-style statements against the generated schema fixture.

use std::sync::LazyLock;
use std::time::Instant;

use googlesql_sys::{AnalyzeError, Catalog};

const SCHEMA: &str = include_str!("../../../fixtures/schema.json");

/// Spike-only port of native/googlesql/tools/ddl_to_catalog.py.
fn catalog_json(copies: usize, extra_features: &[&str]) -> String {
    let ddl: serde_json::Value = serde_json::from_str(SCHEMA).expect("fixture");
    let mut tables = Vec::new();
    for stmt in ddl["ddl"].as_array().expect("ddl array") {
        let s = stmt.as_str().expect("string");
        let Some(rest) = s.strip_prefix("CREATE TABLE ") else {
            continue;
        };
        let open = rest.find('(').expect("(");
        let name = rest[..open].trim().to_string();
        let end = rest.rfind(") PRIMARY KEY").expect("pk");
        let mut cols = Vec::new();
        for part in split_top(&rest[open + 1..end]) {
            let up = part.to_ascii_uppercase();
            if up.starts_with("CONSTRAINT")
                || up.starts_with("FOREIGN KEY")
                || up.starts_with("CHECK")
            {
                continue;
            }
            let (col, ty) = part.split_once(' ').expect("col type");
            let ty = ty.trim();
            let upper = ty.to_ascii_uppercase();
            let cut = [
                " NOT NULL",
                " AS (",
                " AS(",
                " DEFAULT",
                " OPTIONS",
                " HIDDEN",
            ]
            .iter()
            .filter_map(|k| upper.find(k))
            .min()
            .unwrap_or(ty.len());
            cols.push(serde_json::json!({"name": col.trim_matches('`'), "type": ty[..cut].trim()}));
        }
        for i in 0..copies {
            let n = if i == 0 {
                name.clone()
            } else {
                format!("{name}_copy{i}")
            };
            tables.push(serde_json::json!({"name": n, "columns": cols}));
        }
    }
    serde_json::json!({"tables": tables, "extra_features": extra_features}).to_string()
}

fn split_top(body: &str) -> Vec<String> {
    let (mut parts, mut depth, mut cur) = (Vec::new(), 0i32, String::new());
    for ch in body.chars() {
        match ch {
            '(' | '<' => depth += 1,
            ')' | '>' => depth -= 1,
            _ => {}
        }
        if ch == ',' && depth == 0 {
            parts.push(std::mem::take(&mut cur));
        } else {
            cur.push(ch);
        }
    }
    parts.push(cur);
    parts
        .into_iter()
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect()
}

static EMULATOR: LazyLock<Catalog> =
    LazyLock::new(|| Catalog::new(&catalog_json(1, &[])).expect("catalog"));

/// The emulator's feature set plus what the probe queries need beyond it.
static EXTENDED: LazyLock<Catalog> = LazyLock::new(|| {
    Catalog::new(&catalog_json(
        1,
        &[
            "FEATURE_ANALYTIC_FUNCTIONS",
            "FEATURE_QUALIFY",
            "FEATURE_NULLS_FIRST_LAST_IN_ORDER_BY",
        ],
    ))
    .expect("catalog")
});

struct Case {
    name: &'static str,
    sql: &'static str,
    params: &'static str,
}

const CASES: &[Case] = &[
    Case {
        name: "join_force_index",
        sql: "SELECT a.id, a.status, c.name
              FROM sales.orders@{FORCE_INDEX=idx_orders_account_item_placed_at} AS a
              JOIN catalog_items AS c ON c.id = a.item_id
              WHERE a.account_id = @account AND a.item_kind = 'PHYSICAL'
              ORDER BY a.placed_at DESC LIMIT 50",
        params: r#"{"account":"STRING"}"#,
    },
    Case {
        name: "unnest_param",
        sql: "SELECT id, name FROM catalog_items WHERE id IN UNNEST(@ids) AND account_id = @account",
        params: r#"{"ids":"ARRAY<STRING>","account":"STRING"}"#,
    },
    Case {
        name: "array_select_as_struct",
        sql: "SELECT e.item_id,
                     ARRAY(SELECT AS STRUCT a.id, a.status, a.change_kind
                           FROM sales.orders a WHERE a.item_id = e.item_id
                           ORDER BY a.placed_at DESC LIMIT 5) AS recent_orders
              FROM item_revisions e WHERE e.account_id = @account",
        params: r#"{"account":"STRING"}"#,
    },
    Case {
        name: "json_value_query",
        sql: "SELECT id, JSON_VALUE(details, '$.displayName') AS display_name,
                     JSON_QUERY(attributes, '$.dimensions') AS dimensions
              FROM catalog_items
              WHERE JSON_VALUE(details, '$.status') = @status",
        params: r#"{"status":"STRING"}"#,
    },
    Case {
        name: "window_function",
        sql: "SELECT id, ROW_NUMBER() OVER (PARTITION BY account_id ORDER BY placed_at DESC) AS rn
              FROM sales.orders",
        params: "{}",
    },
    Case {
        name: "window_qualify",
        sql: "SELECT item_id, id, placed_at
              FROM sales.orders
              WHERE account_id = @account
              QUALIFY ROW_NUMBER() OVER (PARTITION BY item_id ORDER BY placed_at DESC) = 1",
        params: r#"{"account":"STRING"}"#,
    },
    Case {
        name: "with_cte",
        sql: "WITH latest AS (
                SELECT item_id, MAX(placed_at) AS latest_at FROM sales.orders
                WHERE account_id = @account GROUP BY item_id)
              SELECT a.* FROM sales.orders a JOIN latest l
                ON a.item_id = l.item_id AND a.placed_at = l.latest_at",
        params: r#"{"account":"STRING"}"#,
    },
    Case {
        name: "insert_then_return",
        sql: "INSERT INTO item_revisions (item_id, account_id, created_at, updated_at, updated_by,
                                              latest_revision_id, details, attributes, revision_ids)
              VALUES (@id, @account, PENDING_COMMIT_TIMESTAMP(), PENDING_COMMIT_TIMESTAMP(), @user,
                      @rev, JSON '{}', NULL, [@rev])
              THEN RETURN item_id, account_id",
        params: r#"{"id":"STRING","account":"STRING","user":"STRING","rev":"STRING"}"#,
    },
    Case {
        name: "update_then_return",
        sql: "UPDATE sales.orders SET status = @state, updated_at = PENDING_COMMIT_TIMESTAMP()
              WHERE id = @id THEN RETURN id, status",
        params: r#"{"state":"STRING","id":"STRING"}"#,
    },
    Case {
        name: "delete",
        sql: "DELETE FROM sales.orders WHERE account_id = @account AND id IN UNNEST(@ids)",
        params: r#"{"account":"STRING","ids":"ARRAY<STRING>"}"#,
    },
    Case {
        name: "search_score_snippet",
        sql: "SELECT id, SCORE(name_tokens, @q) AS score, SNIPPET(name, @q) AS snip
              FROM catalog_items
              WHERE SEARCH(name_tokens, @q) AND account_id = @account
              ORDER BY score DESC LIMIT 10",
        params: r#"{"q":"STRING","account":"STRING"}"#,
    },
    Case {
        name: "is_distinct_from",
        sql: "SELECT id FROM sales.orders WHERE status IS DISTINCT FROM @state",
        params: r#"{"state":"STRING"}"#,
    },
    Case {
        name: "nulls_last",
        sql: "SELECT id FROM sales.orders ORDER BY notified_by_email_at DESC NULLS LAST",
        params: "{}",
    },
];

fn kind(stmt: &googlesql_sys::proto::AnyResolvedStatementProto) -> String {
    let dbg = format!("{:?}", stmt.node);
    dbg.split('(').take(2).collect::<Vec<_>>().join("(")
}

fn run(catalog: &Catalog, label: &str) -> Vec<(&'static str, Result<String, AnalyzeError>)> {
    let mut out = Vec::new();
    for c in CASES {
        let cold = Instant::now();
        let r = catalog.analyze(c.sql, c.params);
        let cold = cold.elapsed();
        let warm_iters = 200u32;
        let warm = Instant::now();
        for _ in 0..warm_iters {
            let _ = catalog.analyze_raw(c.sql, c.params);
        }
        let warm = warm.elapsed() / warm_iters;
        match &r {
            Ok(stmt) => println!(
                "[{label}] {:<24} OK   cold {:>8.2?} warm {:>8.2?}  {}",
                c.name,
                cold,
                warm,
                kind(stmt)
            ),
            Err(e) => println!(
                "[{label}] {:<24} ERR  code {} {}",
                c.name,
                e.code,
                e.message.lines().next().unwrap_or_default()
            ),
        }
        out.push((c.name, r.map(|s| kind(&s))));
    }
    out
}

#[test]
fn representative_queries() {
    let emulator = run(&EMULATOR, "emulator");
    let extended = run(&EXTENDED, "extended");
    for (name, r) in &extended {
        assert!(
            r.is_ok(),
            "{name} failed with the extended feature set: {r:?}"
        );
    }
    // The emulator's own LanguageOptions do not enable these.
    let failing: Vec<_> = emulator
        .iter()
        .filter(|(_, r)| r.is_err())
        .map(|(n, _)| *n)
        .collect();
    println!("rejected under emulator options: {failing:?}");
}

#[test]
fn errors_are_reported() {
    let e = EMULATOR
        .analyze("SELECT nope FROM catalog_items", "{}")
        .expect_err("unknown column");
    assert_eq!(e.code, 3, "{e}");
    assert!(e.message.contains("Unrecognized name: nope"), "{e}");
}

#[test]
fn catalog_construction_500_tables() {
    let json = catalog_json(2, &[]);
    let n = serde_json::from_str::<serde_json::Value>(&json).expect("json")["tables"]
        .as_array()
        .map_or(0, Vec::len);
    let t = Instant::now();
    let cat = Catalog::new(&json).expect("catalog");
    println!("catalog with {n} tables built in {:?}", t.elapsed());
    let t = Instant::now();
    cat.analyze(
        "SELECT id FROM catalog_items_copy1 WHERE id = @id",
        r#"{"id":"STRING"}"#,
    )
    .expect("analyze");
    println!("first analyse on it: {:?}", t.elapsed());
}

#[test]
fn concurrent_analysis() {
    let t = Instant::now();
    std::thread::scope(|s| {
        for _ in 0..8 {
            s.spawn(|| {
                for _ in 0..100 {
                    for c in CASES.iter().filter(|c| {
                        !matches!(c.name, "window_function" | "window_qualify" | "nulls_last")
                    }) {
                        EMULATOR.analyze_raw(c.sql, c.params).expect(c.name);
                    }
                }
            });
        }
    });
    println!("8 threads x 100 x 10 statements: {:?}", t.elapsed());
}

/// `cargo test --release -p googlesql-sys -- --ignored --nocapture bench`
#[test]
#[ignore]
fn bench_latency() {
    let cases: Vec<_> = CASES
        .iter()
        .filter(|c| !matches!(c.name, "window_function" | "window_qualify" | "nulls_last"))
        .collect();
    let mut all = Vec::new();
    for c in &cases {
        let mut v: Vec<_> = (0..2000)
            .map(|_| {
                let t = Instant::now();
                EMULATOR.analyze_raw(c.sql, c.params).expect(c.name);
                t.elapsed()
            })
            .collect();
        v.sort();
        println!(
            "{:<24} p50 {:>9.1?} p99 {:>9.1?}",
            c.name,
            v[v.len() / 2],
            v[v.len() * 99 / 100]
        );
        all.extend(v);
    }
    all.sort();
    println!(
        "all p50 {:?} p99 {:?}",
        all[all.len() / 2],
        all[all.len() * 99 / 100]
    );
    for threads in [1usize, 2, 4, 8] {
        let n = 400;
        let t = Instant::now();
        std::thread::scope(|s| {
            for _ in 0..threads {
                s.spawn(|| {
                    for _ in 0..n {
                        for c in &cases {
                            EMULATOR.analyze_raw(c.sql, c.params).expect(c.name);
                        }
                    }
                });
            }
        });
        let total = (threads * n * cases.len()) as f64;
        println!(
            "{threads} threads: {:.0} analyses/s",
            total / t.elapsed().as_secs_f64()
        );
    }
}

#[test]
fn deep_expressions_analyse_on_a_small_stack() {
    // GoogleSQL takes about 2 KiB of stack a level; 5000 levels overflowed a 2 MiB thread and
    // aborted the process before long statements moved to a thread sized for them.
    std::thread::Builder::new()
        .stack_size(2 << 20)
        .spawn(|| {
            let cat = Catalog::new(r#"{"tables":[],"extra_features":[]}"#).expect("catalog");
            let nested = format!("SELECT {}1{}", "ABS(".repeat(5000), ")".repeat(5000));
            assert!(cat.analyze_raw(&nested, "{}").is_ok());
            let chain = format!("SELECT {}", vec!["1"; 5000].join(" + "));
            assert!(cat.analyze_raw(&chain, "{}").is_ok());
        })
        .expect("spawn")
        .join()
        .expect("analysis");
}

#[test]
fn views_analyse_in_strict_mode_with_exact_table_names() {
    let cat = Catalog::new(
        r#"{"tables":[{"name":"Account","columns":[{"name":"Id","type":"INT64"}]}],
            "extra_features":[]}"#,
    )
    .expect("catalog");
    let view = |sql: &str, exact: &str| {
        cat.analyze_view(sql, exact)
            .map(|_| ())
            .map_err(|e| e.message)
    };
    assert!(view("SELECT a.Id FROM Account AS a", r#"["Account"]"#).is_ok());
    let e = view("SELECT Id FROM Account", "").unwrap_err();
    assert!(
        e.contains("cannot be used without a qualifier in strict name resolution mode"),
        "{e}"
    );
    // Case-insensitive unless the table is listed.
    assert!(view("SELECT a.Id FROM ACCOUNT AS a", "").is_ok());
    let e = view(
        "SELECT a.Id FROM Account AS a WHERE EXISTS (SELECT 1 FROM ACCOUNT AS b)",
        r#"["Account"]"#,
    )
    .unwrap_err();
    assert!(e.starts_with("Table not found: ACCOUNT [at 1:59]"), "{e}");
}

#[test]
fn deep_expressions_decode() {
    // prost stops at 100 nested messages and a call is four of them: 34 nested calls didn't
    // decode. Expressions and scans now decode up to `deep::MAX`, deep ones on a thread of their
    // own.
    std::thread::Builder::new()
        .stack_size(2 << 20)
        .spawn(|| {
            let cat = Catalog::new(r#"{"tables":[],"extra_features":[]}"#).expect("catalog");
            for n in [10, 75, 250] {
                let nested = format!("SELECT {}1{}", "ABS(".repeat(n), ")".repeat(n));
                assert!(cat.analyze(&nested, "{}").is_ok(), "{n} nested calls");
            }
            let chain = format!("SELECT {}", vec!["1"; 250].join(" + "));
            assert!(cat.analyze(&chain, "{}").is_ok());
            // Deeper than that is the emulator's and Spanner's out-of-stack error, not a stack
            // overflow.
            let nested = format!("SELECT {}1{}", "ABS(".repeat(5000), ")".repeat(5000));
            let e = cat.analyze(&nested, "{}").expect_err("too deep");
            assert_eq!(e.code, 8);
            assert!(e.message.starts_with("Out of stack space"), "{}", e.message);
        })
        .expect("spawn")
        .join()
        .expect("decoding");
}

#[test]
fn deep_struct_types_decode() {
    // A STRUCT level of a `TypeProto` is three messages: prost stopped at 32 levels. The emulator
    // answers thousands; past `deep::MAX` it runs out of stack like the emulator, without
    // overflowing ours.
    std::thread::Builder::new()
        .stack_size(2 << 20)
        .spawn(|| {
            let cat = Catalog::new(r#"{"tables":[],"extra_features":[]}"#).expect("catalog");
            let cast = |n: usize| {
                let ty = format!("{}INT64{}", "STRUCT<f ".repeat(n), ">".repeat(n));
                cat.analyze(&format!("SELECT CAST(NULL AS {ty}) IS NULL"), "{}")
            };
            for n in [10, 33, 100, 1000, 2000] {
                let r = cast(n);
                assert!(r.is_ok(), "{n} nested STRUCTs: {:?}", r.err());
            }
            let e = cast(2001).expect_err("too deep");
            assert_eq!(e.code, 8, "{}", e.message);
            assert!(e.message.starts_with("Out of stack space"), "{}", e.message);
            // An ARRAY level counts too.
            let ty = format!(
                "{}INT64{}",
                "ARRAY<STRUCT<f ".repeat(1001),
                ">>".repeat(1001)
            );
            let sql = format!("SELECT CAST(NULL AS {ty}) IS NULL");
            assert_eq!(cat.analyze(&sql, "{}").expect_err("too deep").code, 8);
            // STRUCT literals nest a value with their type.
            let ctor = format!("SELECT {}1{}", "STRUCT(".repeat(200), " AS f)".repeat(200));
            assert!(cat.analyze(&ctor, "{}").is_ok());
        })
        .expect("spawn")
        .join()
        .expect("decoding");
}
