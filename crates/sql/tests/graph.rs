//! Spanner Graph: property graph DDL, INFORMATION_SCHEMA.PROPERTY_GRAPHS and GQL queries, using
//! the shape of the entity graph and the queries agents write against it.

use std::sync::Arc;
use std::time::{Duration, Instant};

use spanbolt_ddl::{Schema, parse_statement};
use spanbolt_proto::spanner as pb;
use spanbolt_sql::{Engine, SchemaExprs, Txn};
use spanbolt_storage::{Code, DbState};
use spanbolt_txn::{Clock, Database, ReadOnlyTxn, ReadWriteTxn};

const TABLES: &[&str] = &[
    "CREATE TABLE eg_company (id STRING(MAX) NOT NULL, country STRING(MAX), kind STRING(MAX), name STRING(MAX), registration_number STRING(MAX), status STRING(MAX), is_bankrupt BOOL) PRIMARY KEY (id)",
    "CREATE TABLE eg_person (id STRING(MAX) NOT NULL, country STRING(MAX), kind STRING(MAX), name STRING(MAX), birth_year INT64) PRIMARY KEY (id)",
    "CREATE TABLE eg_address (id STRING(MAX) NOT NULL, address_line STRING(MAX), current_companies INT64) PRIMARY KEY (id)",
    "CREATE TABLE eg_owns (id STRING(MAX) NOT NULL, src_id STRING(MAX), dst_id STRING(MAX), is_active BOOL, share_percentage FLOAT64) PRIMARY KEY (id)",
    "CREATE TABLE eg_role (id STRING(MAX) NOT NULL, src_id STRING(MAX), dst_id STRING(MAX), is_active BOOL, role_title STRING(MAX)) PRIMARY KEY (id)",
    "CREATE TABLE eg_ubo (id STRING(MAX) NOT NULL, src_id STRING(MAX), dst_id STRING(MAX), is_active BOOL, reason_kind STRING(MAX)) PRIMARY KEY (id)",
    "CREATE TABLE eg_located_at (id STRING(MAX) NOT NULL, src_id STRING(MAX), dst_id STRING(MAX), is_current BOOL, address_kind STRING(MAX)) PRIMARY KEY (id)",
    "CREATE TABLE portfolio (entity_id STRING(MAX) NOT NULL) PRIMARY KEY (entity_id)",
];

const GRAPH: &str = "CREATE PROPERTY GRAPH entity_graph
  NODE TABLES (
    eg_company KEY (id) LABEL Company PROPERTIES ALL COLUMNS LABEL Entity PROPERTIES (id, country, kind, name),
    eg_person KEY (id) LABEL Person PROPERTIES ALL COLUMNS LABEL Entity PROPERTIES (id, country, kind, name),
    eg_address KEY (id) LABEL Address PROPERTIES ALL COLUMNS
  )
  EDGE TABLES (
    eg_role AS role_person KEY (id) SOURCE KEY (src_id) REFERENCES eg_person (id) DESTINATION KEY (dst_id) REFERENCES eg_company (id) LABEL HAS_ROLE PROPERTIES ALL COLUMNS,
    eg_role AS role_company KEY (id) SOURCE KEY (src_id) REFERENCES eg_company (id) DESTINATION KEY (dst_id) REFERENCES eg_company (id) LABEL HAS_ROLE PROPERTIES ALL COLUMNS,
    eg_owns AS owns_person KEY (id) SOURCE KEY (src_id) REFERENCES eg_person (id) DESTINATION KEY (dst_id) REFERENCES eg_company (id) LABEL OWNS PROPERTIES ALL COLUMNS,
    eg_owns AS owns_company KEY (id) SOURCE KEY (src_id) REFERENCES eg_company (id) DESTINATION KEY (dst_id) REFERENCES eg_company (id) LABEL OWNS PROPERTIES ALL COLUMNS,
    eg_ubo AS ubo_person KEY (id) SOURCE KEY (src_id) REFERENCES eg_person (id) DESTINATION KEY (dst_id) REFERENCES eg_company (id) LABEL UBO_OF PROPERTIES ALL COLUMNS,
    eg_located_at AS company_located_at KEY (id) SOURCE KEY (src_id) REFERENCES eg_company (id) DESTINATION KEY (dst_id) REFERENCES eg_address (id) LABEL LOCATED_AT PROPERTIES ALL COLUMNS
  )";

const ROWS: &[&str] = &[
    "INSERT INTO eg_company (id, country, kind, name, registration_number, status, is_bankrupt) VALUES
       ('c1', 'NO', 'company', 'Nordvik Eiendom AS', '917968861', 'active', false),
       ('c2', 'NO', 'company', 'Holding AS', '1', 'active', false),
       ('c3', 'NO', 'company', 'Top Invest AS', '2', 'active', false),
       ('c4', 'NO', 'company', 'Bankrupt AS', '3', 'bankrupt', true),
       ('c5', 'NO', 'company', 'Neighbour AS', '4', 'active', false)",
    "INSERT INTO eg_person (id, country, kind, name, birth_year) VALUES
       ('p1', 'NO', 'person', 'Ola', 1970), ('p2', 'NO', 'person', 'Kari', 1980)",
    "INSERT INTO eg_address (id, address_line, current_companies) VALUES ('a1', 'Storgata 1', 2)",
    // Persons and companies own through one base table; o5 is inactive and o9 dangles.
    "INSERT INTO eg_owns (id, src_id, dst_id, is_active, share_percentage) VALUES
       ('o1', 'c2', 'c1', true, 60), ('o2', 'p1', 'c1', true, 40), ('o3', 'c3', 'c2', true, 100),
       ('o4', 'p2', 'c3', true, 100), ('o5', 'c4', 'c1', false, 10), ('o6', 'c4', 'c5', true, 51),
       ('o9', 'gone', 'c1', true, 1)",
    "INSERT INTO eg_role (id, src_id, dst_id, is_active, role_title) VALUES
       ('r1', 'p1', 'c1', true, 'CEO'), ('r2', 'p1', 'c4', true, 'board'), ('r3', 'p2', 'c5', false, 'board')",
    "INSERT INTO eg_ubo (id, src_id, dst_id, is_active, reason_kind) VALUES ('u1', 'p2', 'c1', true, 'indirect')",
    "INSERT INTO eg_located_at (id, src_id, dst_id, is_current, address_kind) VALUES
       ('l1', 'c1', 'a1', true, 'registered'), ('l2', 'c5', 'a1', true, 'registered')",
    "INSERT INTO portfolio (entity_id) VALUES ('c1'), ('c5')",
];

struct Db {
    engine: Engine,
    db: Arc<Database>,
}

fn schema(ddl: &[&str]) -> Schema {
    let mut schema = Schema::default();
    for s in ddl {
        schema
            .apply(&parse_statement(s).expect("parse"))
            .expect("apply");
    }
    schema
}

fn open(ddl: &[&str], rows: &[String]) -> Db {
    spanbolt_storage::expr::set_expr_evaluator(Some(Arc::new(SchemaExprs::new().expect("exprs"))));
    let db = Arc::new(Database::new(
        Arc::new(Clock::default()),
        DbState::new(schema(ddl)).expect("state"),
    ));
    let engine = Engine::new(spanbolt_sql::Options::default());
    let txn = Arc::new(ReadWriteTxn::new(db.clone()));
    for s in rows {
        let req = pb::ExecuteSqlRequest {
            sql: s.clone(),
            ..Default::default()
        };
        engine
            .execute_sql(&db, Txn::ReadWrite(&txn), &req)
            .expect("dml");
    }
    txn.commit(&txn.catalog(), &[]).expect("commit");
    Db { engine, db }
}

fn entity_graph() -> Db {
    let mut ddl = TABLES.to_vec();
    ddl.push(GRAPH);
    open(
        &ddl,
        &ROWS.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
    )
}

fn cell(v: &prost_types::Value) -> String {
    use prost_types::value::Kind;
    match &v.kind {
        Some(Kind::NullValue(_)) | None => "NULL".into(),
        Some(Kind::StringValue(s)) => s.clone(),
        Some(Kind::BoolValue(b)) => b.to_string(),
        Some(Kind::NumberValue(n)) => n.to_string(),
        Some(Kind::ListValue(l)) => format!(
            "[{}]",
            l.values.iter().map(cell).collect::<Vec<_>>().join(",")
        ),
        Some(Kind::StructValue(s)) => format!("{s:?}"),
    }
}

impl Db {
    fn try_query(&self, sql: &str) -> Result<Vec<Vec<String>>, spanbolt_storage::Error> {
        let req = pb::ExecuteSqlRequest {
            sql: sql.into(),
            ..Default::default()
        };
        let txn = ReadOnlyTxn {
            version: self.db.latest(),
        };
        let rs = self
            .engine
            .execute_sql(&self.db, Txn::ReadOnly(&txn), &req)?;
        Ok(rs
            .rows
            .iter()
            .map(|r| r.values.iter().map(cell).collect())
            .collect())
    }

    fn query(&self, sql: &str) -> Vec<Vec<String>> {
        self.try_query(sql)
            .unwrap_or_else(|e| panic!("{sql}\n{e:?}"))
    }

    /// Rows sorted, for queries without ORDER BY.
    fn rows(&self, sql: &str) -> Vec<Vec<String>> {
        let mut r = self.query(sql);
        r.sort();
        r
    }

    fn plan(&self, sql: &str) -> Result<String, spanbolt_storage::Error> {
        self.engine.explain(
            &self.db.latest().state,
            &pb::ExecuteSqlRequest {
                sql: sql.into(),
                ..Default::default()
            },
        )
    }
}

fn strs(rows: &[&[&str]]) -> Vec<Vec<String>> {
    rows.iter()
        .map(|r| r.iter().map(|s| s.to_string()).collect())
        .collect()
}

// ------------------------------------------------------------------ DDL and metadata

#[test]
fn create_property_graph_is_listed_with_every_edge_definition() {
    let db = entity_graph();
    let rows = db.query(
        "SELECT PROPERTY_GRAPH_CATALOG, PROPERTY_GRAPH_SCHEMA, PROPERTY_GRAPH_NAME FROM INFORMATION_SCHEMA.PROPERTY_GRAPHS",
    );
    assert_eq!(rows, strs(&[&["", "", "entity_graph"]]));
    let meta = &db.query(
        "SELECT PROPERTY_GRAPH_METADATA_JSON FROM INFORMATION_SCHEMA.PROPERTY_GRAPHS WHERE PROPERTY_GRAPH_NAME = 'entity_graph'",
    )[0][0];
    let m: serde_json::Value = serde_json::from_str(meta).expect("json");
    let names = |k: &str| -> Vec<(String, String)> {
        m[k].as_array()
            .expect(k)
            .iter()
            .map(|t| {
                (
                    t["name"].as_str().unwrap_or("").to_string(),
                    t["baseTableName"].as_str().unwrap_or("").to_string(),
                )
            })
            .collect()
    };
    assert_eq!(
        names("nodeTables"),
        vec![
            ("eg_address".into(), "eg_address".into()),
            ("eg_company".into(), "eg_company".into()),
            ("eg_person".into(), "eg_person".into()),
        ]
    );
    assert_eq!(
        names("edgeTables"),
        vec![
            ("company_located_at".into(), "eg_located_at".into()),
            ("owns_company".into(), "eg_owns".into()),
            ("owns_person".into(), "eg_owns".into()),
            ("role_company".into(), "eg_role".into()),
            ("role_person".into(), "eg_role".into()),
            ("ubo_person".into(), "eg_ubo".into()),
        ]
    );
    let owns = &m["edgeTables"][2];
    assert_eq!(owns["kind"], "EDGE");
    assert_eq!(owns["keyColumns"], serde_json::json!(["id"]));
    assert_eq!(owns["labelNames"], serde_json::json!(["OWNS"]));
    assert_eq!(
        owns["sourceNodeTable"],
        serde_json::json!({"nodeTableName": "eg_person", "edgeTableColumns": ["src_id"], "nodeTableColumns": ["id"]})
    );
    assert_eq!(
        owns["destinationNodeTable"]["nodeTableName"],
        serde_json::json!("eg_company")
    );
    let company = &m["nodeTables"][1];
    assert_eq!(company["kind"], "NODE");
    assert_eq!(
        company["labelNames"],
        serde_json::json!(["Company", "Entity"])
    );
    let defs: Vec<&str> = company["propertyDefinitions"]
        .as_array()
        .expect("defs")
        .iter()
        .map(|d| d["propertyDeclarationName"].as_str().unwrap_or(""))
        .collect();
    assert!(
        defs.contains(&"is_bankrupt") && defs.contains(&"registration_number"),
        "{defs:?}"
    );
    let entity = m["labels"]
        .as_array()
        .expect("labels")
        .iter()
        .find(|l| l["name"] == "Entity")
        .expect("Entity label");
    assert_eq!(
        entity["propertyDeclarationNames"],
        serde_json::json!(["country", "id", "kind", "name"])
    );
    let decl = m["propertyDeclarations"]
        .as_array()
        .expect("decls")
        .iter()
        .find(|d| d["name"] == "share_percentage")
        .expect("share_percentage");
    assert_eq!(decl["type"], "FLOAT64");
}

#[test]
fn property_graph_ddl_validates_and_drops() {
    let mut s = schema(TABLES);
    let bad = parse_statement(
        "CREATE PROPERTY GRAPH g NODE TABLES (no_such_table KEY (id) LABEL X PROPERTIES ALL COLUMNS)",
    )
    .expect("parse");
    s.apply(&bad).expect("apply is syntactic");
    let engine = Engine::new(spanbolt_sql::Options::default());
    let err = engine
        .validate_property_graph(&Arc::new(s), "g")
        .expect_err("unknown table");
    assert_eq!(err.code, Code::InvalidArgument, "{err:?}");

    let mut ddl = TABLES.to_vec();
    ddl.push(GRAPH);
    ddl.push(
        "CREATE OR REPLACE PROPERTY GRAPH entity_graph NODE TABLES (eg_person KEY (id) LABEL Person PROPERTIES (id, name))",
    );
    let db = open(&ddl, &[]);
    let m = &db
        .query("SELECT PROPERTY_GRAPH_METADATA_JSON FROM INFORMATION_SCHEMA.PROPERTY_GRAPHS")[0][0];
    let m: serde_json::Value = serde_json::from_str(m).expect("json");
    assert_eq!(m["nodeTables"].as_array().map(Vec::len), Some(1));
    assert_eq!(m["edgeTables"].as_array().map(Vec::len), Some(0));

    ddl.push("DROP PROPERTY GRAPH entity_graph");
    let db = open(&ddl, &[]);
    assert!(
        db.query("SELECT * FROM INFORMATION_SCHEMA.PROPERTY_GRAPHS")
            .is_empty()
    );
}

#[test]
fn key_defaults_to_the_primary_key() {
    let mut ddl = TABLES.to_vec();
    ddl.push("CREATE PROPERTY GRAPH g NODE TABLES (eg_person LABEL Person PROPERTIES (id, name))");
    let db = open(&ddl, &[]);
    let m = &db
        .query("SELECT PROPERTY_GRAPH_METADATA_JSON FROM INFORMATION_SCHEMA.PROPERTY_GRAPHS")[0][0];
    let m: serde_json::Value = serde_json::from_str(m).expect("json");
    assert_eq!(m["nodeTables"][0]["keyColumns"], serde_json::json!(["id"]));
}

// ------------------------------------------------------------------ the agent guide's queries

#[test]
fn match_with_node_filter_order_by_and_limit() {
    let db = entity_graph();
    let rows = db.query(
        "GRAPH entity_graph
         MATCH (c:Company WHERE LOWER(c.name) LIKE '%nordvik eiendom%' OR c.registration_number = '2')
         RETURN c.id AS src_id, 'Company' AS src_label, c.name AS src_name, c.is_bankrupt AS is_bankrupt
         ORDER BY LOWER(c.name) = 'nordvik eiendom as' DESC, c.is_bankrupt, c.name
         LIMIT 20",
    );
    assert_eq!(
        rows,
        strs(&[
            &["c1", "Company", "Nordvik Eiendom AS", "false"],
            &["c3", "Company", "Top Invest AS", "false"],
        ])
    );
}

#[test]
fn label_disjunction_labels_function_and_anchor_on_the_last_node() {
    let db = entity_graph();
    let rows = db.rows(
        "GRAPH entity_graph
         MATCH (h)-[e:HAS_ROLE|OWNS|UBO_OF WHERE e.is_active = true]->(c:Company WHERE c.id = 'c1')
         RETURN h.id AS src_id, IF('Person' IN UNNEST(LABELS(h)), 'Person', 'Company') AS src_label,
           h.name AS src_name, LABELS(e)[OFFSET(0)] AS edge, c.id AS dst_id, e.role_title AS role_code,
           e.share_percentage AS share_percentage, e.reason_kind AS reason_kind",
    );
    assert_eq!(
        rows,
        strs(&[
            &[
                "c2",
                "Company",
                "Holding AS",
                "OWNS",
                "c1",
                "NULL",
                "60",
                "NULL"
            ],
            &[
                "p1", "Person", "Ola", "HAS_ROLE", "c1", "CEO", "NULL", "NULL"
            ],
            &["p1", "Person", "Ola", "OWNS", "c1", "NULL", "40", "NULL"],
            &[
                "p2", "Person", "Kari", "UBO_OF", "c1", "NULL", "NULL", "indirect"
            ],
        ])
    );
}

#[test]
fn quantified_path_for_and_next() {
    let db = entity_graph();
    let rows = db.rows(
        "GRAPH entity_graph
         MATCH (c:Company WHERE c.id = 'c1')<-[e:OWNS WHERE e.is_active = true]-{1,6}(top)
         FOR x IN e
         RETURN DISTINCT x.id AS owns_id
         NEXT
         MATCH (s)-[o:OWNS]->(d:Company)
         WHERE o.id = owns_id
         RETURN s.id AS src_id, IF('Person' IN UNNEST(LABELS(s)), 'Person', 'Company') AS src_label,
           'OWNS' AS edge, d.id AS dst_id, o.share_percentage AS share_percentage",
    );
    assert_eq!(
        rows,
        strs(&[
            &["c2", "Company", "OWNS", "c1", "60"],
            &["c3", "Company", "OWNS", "c2", "100"],
            &["p1", "Person", "OWNS", "c1", "40"],
            &["p2", "Person", "OWNS", "c3", "100"],
        ])
    );
}

#[test]
fn quantified_path_bounds_and_group_arrays() {
    let db = entity_graph();
    let rows = db.rows(
        "GRAPH entity_graph
         MATCH (c:Company WHERE c.id = 'c1')<-[e:OWNS WHERE e.is_active]-{2,3}(top)
         RETURN top.id AS top, ARRAY_LENGTH(e) AS hops, ARRAY_TO_STRING(ARRAY(SELECT x.id FROM UNNEST(e) x WITH OFFSET i ORDER BY i), '>') AS chain",
    );
    assert_eq!(
        rows,
        strs(&[&["c3", "2", "o1>o3"], &["p2", "3", "o1>o3>o4"]])
    );
}

#[test]
fn multiple_paths_sharing_a_variable() {
    let db = entity_graph();
    let rows = db.rows(
        "GRAPH entity_graph
         MATCH (p:Person)-[r:HAS_ROLE WHERE r.is_active = true]->(c:Company WHERE c.id = 'c1'),
               (p)-[r2:HAS_ROLE]->(b:Company WHERE b.is_bankrupt = true)
         FOR pair IN [STRUCT(c.id AS dst_id, r.role_title AS role_code), STRUCT(b.id AS dst_id, r2.role_title AS role_code)]
         RETURN DISTINCT p.id AS src_id, pair.dst_id AS dst_id, pair.role_code AS role_code",
    );
    assert_eq!(rows, strs(&[&["p1", "c1", "CEO"], &["p1", "c4", "board"]]));
}

#[test]
fn where_after_match_compares_variables() {
    let db = entity_graph();
    let rows = db.rows(
        "GRAPH entity_graph
         MATCH (c:Company WHERE c.id = 'c1')-[l:LOCATED_AT WHERE l.is_current = true AND l.address_kind = 'registered']->(a:Address WHERE a.current_companies <= 200),
               (other:Company)-[l2:LOCATED_AT WHERE l2.is_current = true]->(a)
         WHERE other.id != c.id
         FOR pair IN [STRUCT(c.id AS src_id, c.name AS src_name), STRUCT(other.id AS src_id, other.name AS src_name)]
         RETURN DISTINCT pair.src_id AS src_id, pair.src_name AS src_name, a.id AS dst_id, a.address_line AS dst_name
         LIMIT 200",
    );
    assert_eq!(
        rows,
        strs(&[
            &["c1", "Nordvik Eiendom AS", "a1", "Storgata 1"],
            &["c5", "Neighbour AS", "a1", "Storgata 1"],
        ])
    );
}

#[test]
fn subquery_in_a_node_filter() {
    let db = entity_graph();
    let rows = db.rows(
        "GRAPH entity_graph
         MATCH (c:Company WHERE c.id IN (SELECT entity_id FROM portfolio))<-[e:OWNS WHERE e.is_active = true]-(o:Company WHERE o.is_bankrupt = true)
         RETURN o.id AS src_id, c.id AS dst_id, e.share_percentage AS share",
    );
    assert_eq!(rows, strs(&[&["c4", "c5", "51"]]));
    let rows = db.rows(
        "GRAPH entity_graph
         MATCH (c:Company WHERE c.id IN (SELECT entity_id FROM portfolio))-[l:LOCATED_AT WHERE l.is_current = true AND l.address_kind = 'registered']->(a:Address WHERE a.current_companies <= 200)
               <-[l2:LOCATED_AT WHERE l2.is_current = true AND l2.address_kind = 'registered']-(other:Company WHERE other.id IN (SELECT entity_id FROM portfolio))
         WHERE other.id != c.id
         RETURN DISTINCT c.id AS src_id, a.id AS dst_id",
    );
    assert_eq!(rows, strs(&[&["c1", "a1"], &["c5", "a1"]]));
}

// ------------------------------------------------------------------ more GQL

#[test]
fn filter_statement() {
    let db = entity_graph();
    assert_eq!(
        db.rows("GRAPH entity_graph MATCH (c:Company) FILTER c.is_bankrupt RETURN c.id"),
        strs(&[&["c4"]])
    );
    assert_eq!(
        db.rows(
            "GRAPH entity_graph MATCH (c:Company) FILTER WHERE c.status = 'bankrupt' RETURN c.id"
        ),
        strs(&[&["c4"]])
    );
}

#[test]
fn labels_of_a_node() {
    let db = entity_graph();
    assert_eq!(
        db.rows(
            "GRAPH entity_graph MATCH (p:Person WHERE p.id = 'p1')
             RETURN ARRAY_TO_STRING(ARRAY(SELECT l FROM UNNEST(LABELS(p)) l ORDER BY l), ',') AS labels"
        ),
        strs(&[&["Entity,Person"]])
    );
    // A label shared by node tables matches every one of them.
    assert_eq!(
        db.rows("GRAPH entity_graph MATCH (n:Entity WHERE n.country = 'NO') RETURN COUNT(*) AS n"),
        strs(&[&["7"]])
    );
}

#[test]
fn optional_match() {
    let db = entity_graph();
    assert_eq!(
        db.rows(
            "GRAPH entity_graph
             MATCH (c:Company WHERE c.id IN ('c4', 'c5'))
             OPTIONAL MATCH (c)<-[:OWNS]-(x)
             RETURN c.id AS company, x.id AS owner"
        ),
        strs(&[&["c4", "NULL"], &["c5", "c4"]])
    );
}

#[test]
fn any_direction_edges() {
    let db = entity_graph();
    assert_eq!(
        db.rows(
            "GRAPH entity_graph MATCH (c:Company WHERE c.id = 'c2')-[o:OWNS]-(x) RETURN x.id AS x_id, o.id AS o_id"
        ),
        strs(&[&["c1", "o1"], &["c3", "o3"]])
    );
}

#[test]
fn dangling_edges_do_not_match() {
    let db = entity_graph();
    assert_eq!(
        db.rows("GRAPH entity_graph MATCH ()-[o:OWNS]->(c WHERE c.id = 'c1') RETURN o.id"),
        strs(&[&["o1"], &["o2"], &["o5"]])
    );
}

#[test]
fn graph_table_in_sql() {
    let db = entity_graph();
    assert_eq!(
        db.query(
            "SELECT src, dst FROM GRAPH_TABLE(entity_graph
               MATCH (a)-[o:OWNS]->(b:Company WHERE b.id = 'c1')
               COLUMNS (a.id AS src, b.id AS dst)) g
             ORDER BY src"
        ),
        strs(&[&["c2", "c1"], &["c4", "c1"], &["p1", "c1"]])
    );
    assert_eq!(
        db.query(
            "SELECT owner, COUNT(*) AS n FROM GRAPH_TABLE(entity_graph
               MATCH (a)-[o:OWNS WHERE o.is_active]->(b:Company)
               COLUMNS (a.id AS owner)) GROUP BY owner HAVING COUNT(*) > 1 ORDER BY owner"
        ),
        Vec::<Vec<String>>::new()
    );
}

/// Element tables over views (the graph-with-views how-to): views as node and edge tables, mixed
/// with a base table, with columns in another order than the base table and a computed column.
fn graph_over_views() -> Db {
    let mut ddl = TABLES.to_vec();
    ddl.extend([
        "CREATE VIEW active_company SQL SECURITY INVOKER AS
           SELECT c.name AS name, c.id AS id, UPPER(c.country) AS country FROM eg_company c WHERE NOT c.is_bankrupt",
        "CREATE VIEW active_owns SQL SECURITY INVOKER AS
           SELECT o.id, o.src_id, o.dst_id, o.share_percentage FROM eg_owns o WHERE o.is_active",
        "CREATE PROPERTY GRAPH g
           NODE TABLES (
             active_company KEY (id) LABEL Company PROPERTIES (id, name, country),
             eg_person KEY (id) LABEL Person PROPERTIES (id, name)
           )
           EDGE TABLES (
             active_owns AS owns_company KEY (id) SOURCE KEY (src_id) REFERENCES active_company (id) DESTINATION KEY (dst_id) REFERENCES active_company (id) LABEL OWNS PROPERTIES (id, share_percentage AS share),
             active_owns AS owns_person KEY (id) SOURCE KEY (src_id) REFERENCES eg_person (id) DESTINATION KEY (dst_id) REFERENCES active_company (id) LABEL OWNS PROPERTIES (id, share_percentage AS share)
           )",
    ]);
    open(
        &ddl,
        &ROWS.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
    )
}

#[test]
fn graph_over_views_scans_and_seeks_the_views() {
    let db = graph_over_views();
    // c4 is bankrupt, so not a node; o5 is inactive, o6 starts at c4 and o9 dangles.
    assert_eq!(
        db.rows("GRAPH g MATCH (c:Company) RETURN c.id, c.name, c.country"),
        strs(&[
            &["c1", "Nordvik Eiendom AS", "NO"],
            &["c2", "Holding AS", "NO"],
            &["c3", "Top Invest AS", "NO"],
            &["c5", "Neighbour AS", "NO"],
        ])
    );
    assert_eq!(
        db.rows("GRAPH g MATCH (a)-[o:OWNS]->(b) RETURN a.id AS a, o.id AS o, o.share AS share, b.id AS b"),
        strs(&[
            &["c2", "o1", "60", "c1"],
            &["c3", "o3", "100", "c2"],
            &["p1", "o2", "40", "c1"],
            &["p2", "o4", "100", "c3"],
        ])
    );
    // Anchored on a view node by key, then walked both ways through view edges.
    assert_eq!(
        db.rows(
            "GRAPH g MATCH (c:Company WHERE c.id = 'c2')-[o:OWNS]-(x) RETURN x.id AS x, o.id AS o"
        ),
        strs(&[&["c1", "o1"], &["c3", "o3"]])
    );
    assert_eq!(
        db.rows(
            "GRAPH g MATCH (c:Company WHERE c.id = 'c1')<-[e:OWNS]-{1,3}(top) RETURN top.id AS top, ARRAY_LENGTH(e) AS hops"
        ),
        strs(&[&["c2", "1"], &["c3", "2"], &["p1", "1"], &["p2", "3"]])
    );
    assert_eq!(
        db.query("GRAPH g MATCH (p:Person) RETURN p.id AS id ORDER BY id"),
        strs(&[&["p1"], &["p2"]])
    );
}

#[test]
fn graph_over_views_reads_the_snapshot_and_reports_the_view() {
    let db = graph_over_views();
    let m = &db
        .query("SELECT PROPERTY_GRAPH_METADATA_JSON FROM INFORMATION_SCHEMA.PROPERTY_GRAPHS")[0][0];
    let m: serde_json::Value = serde_json::from_str(m).expect("json");
    let nodes = m["nodeTables"].as_array().expect("nodes");
    assert!(
        nodes.iter().any(|n| n["baseTableName"] == "active_company"
            && n["keyColumns"] == serde_json::json!(["id"])),
        "{m}"
    );
    assert_eq!(
        db.query(
            "SELECT owner FROM GRAPH_TABLE(g MATCH (a)-[o:OWNS]->(b:Company WHERE b.name = 'Nordvik Eiendom AS') COLUMNS (a.id AS owner)) ORDER BY owner"
        ),
        strs(&[&["c2"], &["p1"]])
    );
}

#[test]
fn analyze_plan_of_a_graph_query() {
    let db = entity_graph();
    let plan = db
        .plan(
            "GRAPH entity_graph MATCH (c:Company WHERE c.id = 'c1')<-[e:OWNS]-{1,6}(top) RETURN top.id",
        )
        .expect("plan");
    assert!(plan.contains("Graph"), "{plan}");
}

#[test]
fn unsupported_graph_features_are_unimplemented() {
    let db = entity_graph();
    let err = db
        .try_query(
            "GRAPH entity_graph MATCH p = (a:Person)-[o:OWNS]->(b) RETURN PATH_LENGTH(p) AS n",
        )
        .expect_err("path variables");
    assert_eq!(err.code, Code::Unimplemented, "{err:?}");
}

// ------------------------------------------------------------------ performance

#[test]
fn six_level_quantified_path_is_fast() {
    // 3000 companies, each owned by three others: 729 ownership chains of six hops from one company.
    let mut rows = Vec::new();
    let n = 3000;
    let mut companies = Vec::new();
    for i in 0..n {
        companies.push(format!(
            "('c{i}', 'NO', 'company', 'C{i}', '{i}', 'active', false)"
        ));
    }
    rows.push(format!(
        "INSERT INTO eg_company (id, country, kind, name, registration_number, status, is_bankrupt) VALUES {}",
        companies.join(",")
    ));
    let mut owns = Vec::new();
    for i in 0..n {
        for k in 1..=3 {
            let owner = (i * 3 + k) % n;
            owns.push(format!("('o{i}_{k}', 'c{owner}', 'c{i}', true, 10)"));
        }
    }
    for chunk in owns.chunks(1000) {
        rows.push(format!(
            "INSERT INTO eg_owns (id, src_id, dst_id, is_active, share_percentage) VALUES {}",
            chunk.join(",")
        ));
    }
    let mut ddl = TABLES.to_vec();
    ddl.push(GRAPH);
    let db = open(&ddl, &rows);
    let q = "GRAPH entity_graph
         MATCH (c:Company WHERE c.id = 'c0')<-[e:OWNS WHERE e.is_active = true]-{1,6}(top)
         FOR x IN e
         RETURN DISTINCT x.id AS owns_id
         NEXT
         MATCH (s)-[o:OWNS]->(d:Company)
         WHERE o.id = owns_id
         RETURN COUNT(*) AS n";
    let start = Instant::now();
    let out = db.query(q);
    let took = start.elapsed();
    assert_eq!(out, strs(&[&["1092"]]));
    assert!(took < Duration::from_secs(3), "took {took:?}");
}
