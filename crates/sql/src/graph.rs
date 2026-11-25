//! Property graphs: the model built from a `CREATE PROPERTY GRAPH` statement (analysed by
//! GoogleSQL), the graph the analyser resolves graph queries against, and
//! `INFORMATION_SCHEMA.PROPERTY_GRAPHS` metadata.

use googlesql_sys::proto::any_resolved_create_statement_proto::Node as CreateNode;
use googlesql_sys::proto::any_resolved_expr_proto::Node as E;
use googlesql_sys::proto::any_resolved_scan_proto::Node as S;
use googlesql_sys::proto::any_resolved_statement_proto::Node as StmtNode;
use googlesql_sys::proto::{
    AnyResolvedExprProto, ResolvedCreatePropertyGraphStmtProto, ResolvedGraphElementTableProto,
    ResolvedGraphNodeTableReferenceProto,
};
use serde_json::{Value as Json, json};
use std::sync::Arc;

use spanbolt_storage::{Catalog as StorageCatalog, Error, Result, TableId};

use crate::catalog::ViewDef;
use crate::types::{self, Type, unimplemented};

pub struct GraphDef {
    pub name: String,
    pub properties: Vec<PropertyDecl>,
    pub labels: Vec<Label>,
    pub nodes: Vec<ElementTable>,
    pub edges: Vec<ElementTable>,
}

pub struct PropertyDecl {
    pub name: String,
    pub ty: Type,
}

pub struct Label {
    pub name: String,
    pub properties: Vec<String>,
}

pub struct ElementTable {
    /// The element table's name: its alias, or the base table's name.
    pub name: String,
    pub table: String,
    /// What the element table reads: a stored table or a view.
    pub input: ElementInput,
    /// The input's column names, by position.
    pub columns: Vec<String>,
    /// Key columns, as positions in the input's row.
    pub key: Vec<usize>,
    pub labels: Vec<String>,
    pub properties: Vec<PropertyDef>,
    /// Column ids of the definition's table scan, by input position (-1 if unused), so
    /// property expressions compile against an input row.
    pub layout: Vec<i64>,
    pub source: Option<NodeRef>,
    pub destination: Option<NodeRef>,
}

#[derive(Clone)]
pub enum ElementInput {
    Table(TableId),
    /// The view's rows, computed by its analysed query. Its columns are the query's output.
    View(Arc<ViewDef>),
}

pub struct PropertyDef {
    pub name: String,
    pub sql: String,
    pub expr: AnyResolvedExprProto,
}

pub struct NodeRef {
    /// Index into `GraphDef::nodes`.
    pub node: usize,
    pub edge_columns: Vec<usize>,
    pub node_columns: Vec<usize>,
}

impl GraphDef {
    pub fn node_or_edge(&self, name: &str) -> Option<(bool, usize)> {
        if let Some(i) = self
            .nodes
            .iter()
            .position(|t| t.name.eq_ignore_ascii_case(name))
        {
            return Some((false, i));
        }
        self.edges
            .iter()
            .position(|t| t.name.eq_ignore_ascii_case(name))
            .map(|i| (true, i))
    }

    pub fn property(&self, name: &str) -> Option<&PropertyDecl> {
        self.properties
            .iter()
            .find(|p| p.name.eq_ignore_ascii_case(name))
    }
}

fn column_name(e: &AnyResolvedExprProto) -> Result<&str> {
    match e.node.as_ref() {
        Some(E::ResolvedColumnRefNode(c)) => c
            .column
            .as_ref()
            .and_then(|c| c.name.as_deref())
            .ok_or_else(|| Error::internal("column reference without column")),
        Some(E::ResolvedCatalogColumnRefNode(c)) => c
            .column
            .as_ref()
            .and_then(|c| c.name.as_deref())
            .ok_or_else(|| Error::internal("catalog column without name")),
        _ => Err(unimplemented("A graph key that is not a column")),
    }
}

fn positions(et: &ElementTable, exprs: &[AnyResolvedExprProto]) -> Result<Vec<usize>> {
    exprs
        .iter()
        .map(|e| {
            let name = column_name(e)?;
            et.columns
                .iter()
                .position(|c| c.eq_ignore_ascii_case(name))
                .ok_or_else(|| Error::internal(format!("column {name} not in {}", et.table)))
        })
        .collect()
}

/// Finds the view a catalog name refers to, if it names one.
pub type ViewLookup<'v> = dyn Fn(&str) -> Option<Arc<ViewDef>> + 'v;

fn element_table(
    storage: &StorageCatalog,
    views: &ViewLookup,
    t: &ResolvedGraphElementTableProto,
) -> Result<ElementTable> {
    if t.dynamic_label.is_some() || t.dynamic_properties.is_some() {
        return Err(unimplemented("Dynamic labels and properties"));
    }
    let scan = match t.input_scan.as_ref().and_then(|s| s.node.as_ref()) {
        Some(S::ResolvedTableScanNode(s)) => s,
        _ => return Err(Error::internal("graph element table without a table scan")),
    };
    let table = scan
        .table
        .as_ref()
        .and_then(|t| t.full_name.clone().or_else(|| t.name.clone()))
        .ok_or_else(|| Error::internal("graph element table without a table"))?;
    // The analyser has resolved the name, so a scan that isn't a stored table reads a view.
    let (input, columns) = match storage.table_id(&table) {
        Some(id) => (
            ElementInput::Table(id),
            storage
                .table(id)
                .columns
                .iter()
                .map(|c| c.name.clone())
                .collect::<Vec<_>>(),
        ),
        None => {
            let v = views(&table)
                .ok_or_else(|| Error::internal(format!("graph element table {table} not found")))?;
            let cols = v
                .query
                .output_column_list
                .iter()
                .map(|o| o.name.clone().unwrap_or_default())
                .collect();
            (ElementInput::View(v), cols)
        }
    };
    let mut layout = vec![-1; columns.len()];
    let cols = scan
        .parent
        .as_ref()
        .map(|p| &p.column_list[..])
        .unwrap_or(&[]);
    for (c, &i) in cols.iter().zip(&scan.column_index_list) {
        if let Some(slot) = layout.get_mut(i as usize) {
            *slot = c.column_id.unwrap_or(-1);
        }
    }
    let properties = t
        .property_definition_list
        .iter()
        .map(|p| {
            Ok(PropertyDef {
                name: p.property_declaration_name.clone().unwrap_or_default(),
                sql: p.sql.clone().unwrap_or_default(),
                expr: p
                    .expr
                    .clone()
                    .ok_or_else(|| Error::internal("property definition without expr"))?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let mut et = ElementTable {
        name: t.alias.clone().unwrap_or_else(|| table.clone()),
        key: Vec::new(),
        table,
        input,
        columns,
        labels: t.label_name_list.clone(),
        properties,
        layout,
        source: None,
        destination: None,
    };
    et.key = positions(&et, &t.key_list)?;
    Ok(et)
}

fn node_ref(
    nodes: &[ElementTable],
    edge: &ElementTable,
    r: &ResolvedGraphNodeTableReferenceProto,
) -> Result<NodeRef> {
    let name = r.node_table_identifier.as_deref().unwrap_or("");
    let node = nodes
        .iter()
        .position(|n| n.name.eq_ignore_ascii_case(name))
        .ok_or_else(|| Error::internal(format!("node table {name} not found")))?;
    Ok(NodeRef {
        node,
        edge_columns: positions(edge, &r.edge_table_column_list)?,
        node_columns: positions(&nodes[node], &r.node_table_column_list)?,
    })
}

fn from_stmt(
    storage: &StorageCatalog,
    views: &ViewLookup,
    s: &ResolvedCreatePropertyGraphStmtProto,
) -> Result<GraphDef> {
    let name = s
        .parent
        .as_ref()
        .map(|p| p.name_path.join("."))
        .filter(|n| !n.is_empty())
        .ok_or_else(|| Error::internal("property graph without a name"))?;
    let nodes = s
        .node_table_list
        .iter()
        .map(|t| element_table(storage, views, t))
        .collect::<Result<Vec<_>>>()?;
    let mut edges = Vec::new();
    for t in &s.edge_table_list {
        let mut e = element_table(storage, views, t)?;
        let (Some(src), Some(dst)) = (&t.source_node_reference, &t.dest_node_reference) else {
            return Err(Error::internal("edge table without node references"));
        };
        e.source = Some(node_ref(&nodes, &e, src)?);
        e.destination = Some(node_ref(&nodes, &e, dst)?);
        edges.push(e);
    }
    let properties = s
        .property_declaration_list
        .iter()
        .map(|p| {
            let ty = p
                .r#type
                .as_ref()
                .ok_or_else(|| Error::internal("property without type"))?;
            Ok(PropertyDecl {
                name: p.name.clone().unwrap_or_default(),
                ty: types::from_proto(ty)?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let labels = s
        .label_list
        .iter()
        .map(|l| Label {
            name: l.name.clone().unwrap_or_default(),
            properties: l.property_declaration_name_list.clone(),
        })
        .collect();
    Ok(GraphDef {
        name,
        properties,
        labels,
        nodes,
        edges,
    })
}

/// Analyses a `CREATE PROPERTY GRAPH` statement against the catalog's tables and views.
pub fn analyse(
    gsql: &googlesql_sys::Catalog,
    storage: &StorageCatalog,
    views: &ViewLookup,
    ddl: &str,
) -> Result<GraphDef> {
    let stmt = gsql
        .analyze_property_graph_ddl(ddl)
        .map_err(crate::analyze::map_error)?;
    match stmt.node {
        Some(StmtNode::ResolvedCreateStatementNode(c)) => match c.node {
            Some(CreateNode::ResolvedCreatePropertyGraphStmtNode(s)) => {
                from_stmt(storage, views, &s)
            }
            _ => Err(Error::internal("not a CREATE PROPERTY GRAPH statement")),
        },
        _ => Err(Error::internal("not a CREATE PROPERTY GRAPH statement")),
    }
}

/// The graph JSON of `gsql_catalog_add_property_graph`.
fn cols(e: &ElementTable, is: &[usize]) -> Vec<String> {
    is.iter().map(|&i| e.columns[i].clone()).collect()
}

pub fn shim_json(g: &GraphDef) -> String {
    let element = |e: &ElementTable| {
        let mut v = json!({
            "name": e.name,
            "table": e.table,
            "key": cols(e, &e.key),
            "labels": e.labels,
            "properties": e.properties.iter()
                .map(|p| json!({"name": p.name, "sql": p.sql}))
                .collect::<Vec<_>>(),
        });
        let reference = |r: &NodeRef| {
            json!({
                "node": g.nodes[r.node].name,
                "edge_columns": cols(e, &r.edge_columns),
                "node_columns": cols(&g.nodes[r.node], &r.node_columns),
            })
        };
        if let (Some(s), Some(d)) = (&e.source, &e.destination) {
            v["source"] = reference(s);
            v["destination"] = reference(d);
        }
        v
    };
    json!({
        "name": g.name,
        "properties": g.properties.iter()
            .map(|p| json!({"name": p.name, "type": types::gsql_name(&p.ty)}))
            .collect::<Vec<_>>(),
        "labels": g.labels.iter()
            .map(|l| json!({"name": l.name, "properties": l.properties}))
            .collect::<Vec<_>>(),
        "nodes": g.nodes.iter().map(element).collect::<Vec<_>>(),
        "edges": g.edges.iter().map(element).collect::<Vec<_>>(),
    })
    .to_string()
}

/// `PROPERTY_GRAPH_METADATA_JSON`: Spanner's `PropertyGraph` proto as JSON. Unlike the official
/// emulator, `baseTableName` is the base table and every element table is listed, also several
/// over one base table.
/// Element tables are listed by name, as the emulator does.
fn by_name(mut v: Vec<Json>) -> Vec<Json> {
    v.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    v
}

/// `INFORMATION_SCHEMA.PROPERTY_GRAPHS.PROPERTY_GRAPH_METADATA_JSON`. Cloud Spanner (`sorted`)
/// lists property definitions and declarations by name; the emulator in declaration order.
pub fn metadata_json(g: &GraphDef, sorted: bool) -> Json {
    let sort = |mut v: Vec<Json>, key: &str| {
        if sorted {
            v.sort_by(|a, b| a[key].as_str().cmp(&b[key].as_str()));
        }
        v
    };
    let element = |e: &ElementTable, kind: &str| {
        let mut v = json!({
            "name": e.name,
            "kind": kind,
            "baseCatalogName": "",
            "baseSchemaName": "",
            "baseTableName": e.table,
            "keyColumns": cols(e, &e.key),
            "labelNames": e.labels,
            "propertyDefinitions": sort(e.properties.iter()
                .map(|p| json!({
                    "propertyDeclarationName": p.name,
                    "valueExpressionSql": p.sql,
                }))
                .collect(), "propertyDeclarationName"),
        });
        let reference = |r: &NodeRef| {
            json!({
                "nodeTableName": g.nodes[r.node].name,
                "edgeTableColumns": cols(e, &r.edge_columns),
                "nodeTableColumns": cols(&g.nodes[r.node], &r.node_columns),
            })
        };
        if let (Some(s), Some(d)) = (&e.source, &e.destination) {
            v["sourceNodeTable"] = reference(s);
            v["destinationNodeTable"] = reference(d);
        }
        v
    };
    let mut labels: Vec<Json> = g
        .labels
        .iter()
        .map(|l| {
            let mut props = l.properties.clone();
            props.sort();
            json!({"name": l.name, "propertyDeclarationNames": props})
        })
        .collect();
    labels.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    json!({
        "catalog": "",
        "schema": "",
        "name": g.name,
        "nodeTables": by_name(g.nodes.iter().map(|e| element(e, "NODE")).collect()),
        "edgeTables": by_name(g.edges.iter().map(|e| element(e, "EDGE")).collect()),
        "labels": labels,
        "propertyDeclarations": sort(g.properties.iter()
            .map(|p| json!({"name": p.name, "type": types::spanner_name(&p.ty)}))
            .collect(), "name"),
    })
}

/// The properties element table `e` exposes under `label`, in the order `e` defines them.
fn label_properties<'g>(g: &'g GraphDef, e: &'g ElementTable, label: &str) -> Vec<&'g PropertyDef> {
    let Some(l) = g.labels.iter().find(|l| l.name.eq_ignore_ascii_case(label)) else {
        return Vec::new();
    };
    e.properties
        .iter()
        .filter(|p| l.properties.iter().any(|n| n.eq_ignore_ascii_case(&p.name)))
        .collect()
}

fn element_head(e: &ElementTable) -> String {
    let q = spanbolt_ddl::quote_ident;
    if e.name.eq_ignore_ascii_case(&e.table) {
        spanbolt_ddl::quote_path(&e.table)
    } else {
        format!("{} AS {}", spanbolt_ddl::quote_path(&e.table), q(&e.name))
    }
}

fn names(e: &ElementTable, is: &[usize]) -> String {
    is.iter()
        .map(|&i| spanbolt_ddl::quote_ident(&e.columns[i]))
        .collect::<Vec<_>>()
        .join(", ")
}

/// `SOURCE KEY(..) REFERENCES N(..)`-style clauses of an edge table, one per entry.
fn references(g: &GraphDef, e: &ElementTable) -> Vec<String> {
    [("SOURCE", &e.source), ("DESTINATION", &e.destination)]
        .into_iter()
        .filter_map(|(what, r)| {
            let r = r.as_ref()?;
            let node = &g.nodes[r.node];
            Some(format!(
                "{what} KEY({}) REFERENCES {}({})",
                names(e, &r.edge_columns),
                spanbolt_ddl::quote_ident(&node.name),
                names(node, &r.node_columns)
            ))
        })
        .collect()
}

/// The graph as emulator 1.5.59's GetDatabaseDdl prints it: element tables by name, each label
/// with only its property names, sorted (expressions are left out).
pub fn emulator_ddl(g: &GraphDef) -> String {
    let table = |e: &ElementTable| {
        let mut out = format!("{} KEY({})", element_head(e), names(e, &e.key));
        for r in references(g, e) {
            out.push(' ');
            out.push_str(&r);
        }
        let labels = e
            .labels
            .iter()
            .map(|label| {
                let mut props: Vec<&str> = label_properties(g, e, label)
                    .iter()
                    .map(|p| p.name.as_str())
                    .collect();
                props.sort_unstable();
                if props.is_empty() {
                    format!("LABEL {label}")
                } else {
                    format!("LABEL {label} PROPERTIES(\n{})", props.join(",\n"))
                }
            })
            .collect::<Vec<_>>()
            .join(", ");
        format!("{out} {labels}")
    };
    let block = |what: &str, tables: &[ElementTable]| {
        let mut ts: Vec<&ElementTable> = tables.iter().collect();
        ts.sort_by(|a, b| a.name.cmp(&b.name));
        let body = ts.into_iter().map(table).collect::<Vec<_>>().join(",\n");
        format!("\n{what} TABLES(\n  {body}\n)")
    };
    let mut out = format!("CREATE PROPERTY GRAPH {}", g.name);
    out.push_str(&block("NODE", &g.nodes));
    if !g.edges.is_empty() {
        out.push_str(&block("EDGE", &g.edges));
    }
    out
}
