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
use spanbolt_storage::{Catalog as StorageCatalog, Error, Result, TableId};

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
    pub table_id: TableId,
    /// Key columns, as positions in the base table.
    pub key: Vec<usize>,
    pub labels: Vec<String>,
    pub properties: Vec<PropertyDef>,
    /// Column ids of the definition's table scan, by base table position (-1 if unused), so
    /// property expressions compile against a base table row.
    pub layout: Vec<i64>,
    pub source: Option<NodeRef>,
    pub destination: Option<NodeRef>,
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

fn positions(
    storage: &StorageCatalog,
    table: TableId,
    exprs: &[AnyResolvedExprProto],
) -> Result<Vec<usize>> {
    let tm = storage.table(table);
    exprs
        .iter()
        .map(|e| {
            let name = column_name(e)?;
            tm.column(name)
                .ok_or_else(|| Error::internal(format!("column {name} not in {}", tm.name)))
        })
        .collect()
}

fn element_table(
    storage: &StorageCatalog,
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
    let table_id = storage.table_id(&table).ok_or_else(|| {
        unimplemented(format!(
            "Property graph element tables over views ({table})"
        ))
    })?;
    let width = storage.table(table_id).columns.len();
    let mut layout = vec![-1; width];
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
    Ok(ElementTable {
        name: t.alias.clone().unwrap_or_else(|| table.clone()),
        key: positions(storage, table_id, &t.key_list)?,
        table,
        table_id,
        labels: t.label_name_list.clone(),
        properties,
        layout,
        source: None,
        destination: None,
    })
}

fn node_ref(
    storage: &StorageCatalog,
    nodes: &[ElementTable],
    edge: TableId,
    r: &ResolvedGraphNodeTableReferenceProto,
) -> Result<NodeRef> {
    let name = r.node_table_identifier.as_deref().unwrap_or("");
    let node = nodes
        .iter()
        .position(|n| n.name.eq_ignore_ascii_case(name))
        .ok_or_else(|| Error::internal(format!("node table {name} not found")))?;
    Ok(NodeRef {
        node,
        edge_columns: positions(storage, edge, &r.edge_table_column_list)?,
        node_columns: positions(storage, nodes[node].table_id, &r.node_table_column_list)?,
    })
}

fn from_stmt(
    storage: &StorageCatalog,
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
        .map(|t| element_table(storage, t))
        .collect::<Result<Vec<_>>>()?;
    let mut edges = Vec::new();
    for t in &s.edge_table_list {
        let mut e = element_table(storage, t)?;
        let (Some(src), Some(dst)) = (&t.source_node_reference, &t.dest_node_reference) else {
            return Err(Error::internal("edge table without node references"));
        };
        e.source = Some(node_ref(storage, &nodes, e.table_id, src)?);
        e.destination = Some(node_ref(storage, &nodes, e.table_id, dst)?);
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

/// Analyses a `CREATE PROPERTY GRAPH` statement against the catalog's tables.
pub fn analyse(
    gsql: &googlesql_sys::Catalog,
    storage: &StorageCatalog,
    ddl: &str,
) -> Result<GraphDef> {
    let stmt = gsql
        .analyze_property_graph_ddl(ddl)
        .map_err(crate::analyze::map_error)?;
    match stmt.node {
        Some(StmtNode::ResolvedCreateStatementNode(c)) => match c.node {
            Some(CreateNode::ResolvedCreatePropertyGraphStmtNode(s)) => from_stmt(storage, &s),
            _ => Err(Error::internal("not a CREATE PROPERTY GRAPH statement")),
        },
        _ => Err(Error::internal("not a CREATE PROPERTY GRAPH statement")),
    }
}

/// The graph JSON of `gsql_catalog_add_property_graph`.
pub fn shim_json(g: &GraphDef, storage: &StorageCatalog) -> String {
    let col = |t: TableId, i: usize| storage.table(t).columns[i].name.clone();
    let cols = |t: TableId, is: &[usize]| is.iter().map(|&i| col(t, i)).collect::<Vec<_>>();
    let element = |e: &ElementTable| {
        let mut v = json!({
            "name": e.name,
            "table": e.table,
            "key": cols(e.table_id, &e.key),
            "labels": e.labels,
            "properties": e.properties.iter()
                .map(|p| json!({"name": p.name, "sql": p.sql}))
                .collect::<Vec<_>>(),
        });
        let reference = |r: &NodeRef| {
            json!({
                "node": g.nodes[r.node].name,
                "edge_columns": cols(e.table_id, &r.edge_columns),
                "node_columns": cols(g.nodes[r.node].table_id, &r.node_columns),
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

pub fn metadata_json(g: &GraphDef, storage: &StorageCatalog) -> Json {
    let col = |t: TableId, i: usize| storage.table(t).columns[i].name.clone();
    let cols = |t: TableId, is: &[usize]| is.iter().map(|&i| col(t, i)).collect::<Vec<_>>();
    let element = |e: &ElementTable, kind: &str| {
        let mut v = json!({
            "name": e.name,
            "kind": kind,
            "baseCatalogName": "",
            "baseSchemaName": "",
            "baseTableName": e.table,
            "keyColumns": cols(e.table_id, &e.key),
            "labelNames": e.labels,
            "propertyDefinitions": e.properties.iter()
                .map(|p| json!({
                    "propertyDeclarationName": p.name,
                    "valueExpressionSql": p.sql,
                }))
                .collect::<Vec<_>>(),
        });
        let reference = |r: &NodeRef| {
            json!({
                "nodeTableName": g.nodes[r.node].name,
                "edgeTableColumns": cols(e.table_id, &r.edge_columns),
                "nodeTableColumns": cols(g.nodes[r.node].table_id, &r.node_columns),
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
        "propertyDeclarations": g.properties.iter()
            .map(|p| json!({"name": p.name, "type": types::spanner_name(&p.ty)}))
            .collect::<Vec<_>>(),
    })
}
