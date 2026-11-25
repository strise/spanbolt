//! Spanner's query size limits (https://cloud.google.com/spanner/quotas#query_limits), checked on
//! the analyser's serialized resolved AST like the emulator's `QuerySizeLimitsChecker`
//! (cloud-spanner-emulator `query_size_limits_checker.cc`, Apache-2.0).
//!
//! The AST is walked at the protobuf wire level, guided by the descriptors of `resolved_ast.proto`,
//! so every node kind is covered without decoding into the prost types. A message is a resolved
//! node when it is `ResolvedNodeProto` or its `parent` field (the superclass) is one; `AnyResolved*`
//! wrappers are transparent. A node's depth is its distance from the statement in resolved nodes,
//! which is what the emulator's breadth-first walk counts "nested" occurrences by: the number of
//! distinct depths a node kind appears at.

use std::collections::{BTreeSet, HashMap};
use std::sync::LazyLock;

use googlesql_sys::proto::{FunctionRefProto, ResolvedParameterProto, TypeProto};
use prost::Message;
use prost_types::field_descriptor_proto::Type;
use spanbolt_storage::{Error, Result};

/// Longest query string, in bytes.
pub const MAX_QUERY_STRING_SIZE: usize = 1 << 20;
const MAX_JOINS: usize = 20;
const MAX_NESTED_FUNCTION_NODES: usize = 75;
const MAX_NESTED_SUBQUERY_EXPRESSIONS: usize = 25;
const MAX_NESTED_SUBSELECTS: usize = 60;
const MAX_NESTED_GROUP_BY: usize = 35;
const MAX_UNIONS_IN_QUERY: usize = 200;
const MAX_SUBQUERY_EXPRESSION_CHILDREN: usize = 500;
const MAX_FUNCTION_NODES: usize = 1000;
const MAX_COLUMNS_IN_GROUP_BY: usize = 1000;
const MAX_PARAMETERS: usize = 950;
const MAX_STRUCT_FIELDS: usize = 1000;
const MAX_NESTED_STRUCT_DEPTH: usize = 15;
/// Production only; the emulator has no IN-list limit.
const MAX_IN_LIST: usize = 10_000;

/// Cloud Spanner's analyser runs out of stack on deeply nested expressions, before any limit is
/// checked. Measured on nested function calls in the resolved AST: an operator (`$add`, ...) level
/// fits about 925 times, a named function call about 875 times while resolving the query and about
/// 825 times once its signature is resolved (`conformance/cases/sql/stack_depth.case`). Production
/// only: the emulator's analyser has the stack for any query the limits allow.
const STACK_OPERATORS: f64 = 925.0;
const STACK_CALLS_QUERY: f64 = 875.0;
const STACK_CALLS_FUNCTION: f64 = 825.0;
/// How deep the walk goes in resolved nodes. Deeper, a query already breaks the function limits
/// (and production's stack), and walking on would overflow this thread's stack.
const MAX_WALK_DEPTH: usize = 2000;

/// The node kinds the limits count, and the list fields some of them bound.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Kind {
    JoinScan,
    ProjectScan,
    SubqueryExpr,
    AggregateScan,
    FunctionCall,
    Parameter,
    SetOperationScan,
    MakeStruct,
}

impl Kind {
    fn of(message: &str) -> Option<Kind> {
        Some(match message {
            "ResolvedJoinScanProto" => Kind::JoinScan,
            "ResolvedProjectScanProto" => Kind::ProjectScan,
            "ResolvedSubqueryExprProto" => Kind::SubqueryExpr,
            "ResolvedAggregateScanProto" => Kind::AggregateScan,
            "ResolvedFunctionCallProto" => Kind::FunctionCall,
            "ResolvedParameterProto" => Kind::Parameter,
            "ResolvedSetOperationScanProto" => Kind::SetOperationScan,
            "ResolvedMakeStructProto" => Kind::MakeStruct,
            _ => return None,
        })
    }

    /// The repeated field whose length the kind's local check bounds, with its limit and error.
    fn bounded_list(self) -> Option<(&'static str, usize, &'static str)> {
        Some(match self {
            Kind::AggregateScan => (
                "group_by_list",
                MAX_COLUMNS_IN_GROUP_BY,
                "Number of grouping columns exceeds the maximum allowed limit of",
            ),
            Kind::SetOperationScan => (
                "input_item_list",
                MAX_UNIONS_IN_QUERY,
                "Number of UNION operations exceeds the maximum allowed limit of",
            ),
            Kind::MakeStruct => (
                "field_list",
                MAX_STRUCT_FIELDS,
                "Number of struct fields exceeds the maximum allowed limit of",
            ),
            Kind::ProjectScan => (
                "expr_list",
                MAX_SUBQUERY_EXPRESSION_CHILDREN,
                "Number of child subqueries exceeds the maximum allowed limit of",
            ),
            _ => return None,
        })
    }
}

/// What a message-typed field of a resolved node leads to.
#[derive(Clone, Copy)]
enum Edge {
    /// `parent`: the node's own superclass fields, at the same depth.
    Parent(usize),
    /// A child node, directly or through `AnyResolved*` wrappers.
    Child(usize),
}

struct Field {
    edge: Option<Edge>,
    /// Matched against the list a local check bounds (see [`Kind::bounded_list`]).
    name: String,
}

struct Msg {
    name: String,
    /// `AnyResolved*`: a oneof of node (or wrapper) types, not a node itself.
    wrapper: bool,
    fields: HashMap<u32, Field>,
}

/// `resolved_ast.proto`'s messages, indexed; `None` on docs.rs (no descriptors).
struct Schema {
    msgs: Vec<Msg>,
    root: usize,
}

static SCHEMA: LazyLock<Option<Schema>> = LazyLock::new(load);

fn load() -> Option<Schema> {
    let fds =
        prost_types::FileDescriptorSet::decode(googlesql_sys::RESOLVED_AST_DESCRIPTORS).ok()?;
    let mut index: HashMap<String, usize> = HashMap::new();
    let mut raw = Vec::new();
    for file in &fds.file {
        let package = file.package();
        for m in &file.message_type {
            let full = format!(".{package}.{}", m.name());
            index.insert(full, raw.len());
            raw.push(m);
        }
    }
    let parent_of = |m: &prost_types::DescriptorProto| -> Option<usize> {
        m.field
            .iter()
            .find(|f| f.name() == "parent" && f.r#type() == Type::Message)
            .and_then(|f| index.get(f.type_name()).copied())
    };
    let mut is_node = vec![None::<bool>; raw.len()];
    fn node(
        i: usize,
        raw: &[&prost_types::DescriptorProto],
        memo: &mut [Option<bool>],
        parent_of: &dyn Fn(&prost_types::DescriptorProto) -> Option<usize>,
    ) -> bool {
        if let Some(v) = memo[i] {
            return v;
        }
        memo[i] = Some(false);
        let v = raw[i].name() == "ResolvedNodeProto"
            || parent_of(raw[i]).is_some_and(|p| node(p, raw, memo, parent_of));
        memo[i] = Some(v);
        v
    }
    for i in 0..raw.len() {
        node(i, &raw, &mut is_node, &parent_of);
    }
    let wrapper = |i: usize| raw[i].name().starts_with("AnyResolved");
    let msgs = raw
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let mut fields = HashMap::new();
            if is_node[i] == Some(true) || wrapper(i) {
                for f in &m.field {
                    let target = (f.r#type() == Type::Message)
                        .then(|| index.get(f.type_name()).copied())
                        .flatten();
                    let edge = target.and_then(|t| {
                        if f.name() == "parent" && is_node[t] == Some(true) {
                            Some(Edge::Parent(t))
                        } else if is_node[t] == Some(true) || wrapper(t) {
                            Some(Edge::Child(t))
                        } else {
                            None
                        }
                    });
                    fields.insert(
                        f.number() as u32,
                        Field {
                            edge,
                            name: f.name().to_string(),
                        },
                    );
                }
            }
            Msg {
                name: m.name().to_string(),
                wrapper: wrapper(i),
                fields,
            }
        })
        .collect();
    let root = *index.get(".googlesql.AnyResolvedStatementProto")?;
    Some(Schema { msgs, root })
}

fn limit_error(text: &str, limit: usize) -> Error {
    Error::invalid_argument(format!("{text} {limit}."))
}

/// Rejects a query string longer than Spanner allows (checked before analysis).
pub fn check_query_string(sql: &str) -> Result<()> {
    if sql.len() > MAX_QUERY_STRING_SIZE {
        return Err(Error::invalid_argument(format!(
            "Query string length of {} exceeds maximum allowed length of {MAX_QUERY_STRING_SIZE}",
            sql.len()
        )));
    }
    Ok(())
}

#[derive(Default)]
struct Counts {
    total: usize,
    depths: BTreeSet<usize>,
}

struct Walk<'s> {
    schema: &'s Schema,
    /// `--emulator-compat`: the emulator's limits; otherwise production's.
    compat: bool,
    counts: HashMap<Kind, Counts>,
    /// The first limit a single node breaks (the list a node kind bounds, a struct parameter).
    local: Option<Error>,
    /// The longest `IN` list.
    in_list: usize,
    /// The stack the function calls above the current node take, as a fraction of Spanner's, while
    /// resolving the query and while resolving a function's signature, and the most of each.
    stack: (f64, f64),
    deepest: (f64, f64),
    /// Nodes past [`MAX_WALK_DEPTH`] were not walked.
    truncated: bool,
}

/// One protobuf field: its number and, for a length-delimited one, its bytes.
fn fields(mut b: &[u8]) -> impl Iterator<Item = (u32, Option<&[u8]>)> {
    std::iter::from_fn(move || {
        if b.is_empty() {
            return None;
        }
        let key = prost::encoding::decode_varint(&mut b).ok()?;
        let (number, wire) = ((key >> 3) as u32, key & 7);
        match wire {
            0 => {
                prost::encoding::decode_varint(&mut b).ok()?;
                Some((number, None))
            }
            1 | 5 => {
                let n = if wire == 1 { 8 } else { 4 };
                b = b.get(n..)?;
                Some((number, None))
            }
            2 => {
                let len = prost::encoding::decode_varint(&mut b).ok()? as usize;
                let (v, rest) = (b.get(..len)?, b.get(len..)?);
                b = rest;
                Some((number, Some(v)))
            }
            // Groups don't occur in resolved_ast.proto; stop rather than misread.
            _ => None,
        }
    })
}

impl Walk<'_> {
    /// Walks the message `m` (bytes `b`) found at `depth`. `own` is the concrete node whose fields
    /// (its own or a superclass's) `b` holds, if any.
    fn message(&mut self, m: usize, b: &[u8], depth: usize, own: Option<Kind>) -> Result<()> {
        let msg = &self.schema.msgs[m];
        if msg.wrapper {
            // A wrapper holds exactly one node (or nested wrapper) at this same depth.
            for (n, v) in fields(b) {
                if let (
                    Some(Field {
                        edge: Some(Edge::Child(t)),
                        ..
                    }),
                    Some(v),
                ) = (msg.fields.get(&n), v)
                {
                    self.message(*t, v, depth, None)?;
                }
            }
            return Ok(());
        }
        let own = match own {
            Some(k) => Some(k),
            None => {
                let kind = Kind::of(&msg.name);
                if let Some(k) = kind {
                    let c = self.counts.entry(k).or_default();
                    c.total += 1;
                    c.depths.insert(depth);
                    if k == Kind::Parameter
                        && self.local.is_none()
                        && let Err(e) = check_struct_parameter(b)
                    {
                        self.local = Some(e);
                    }
                }
                kind
            }
        };
        // Production allows more than 500 select-list items.
        let bounded = own
            .and_then(Kind::bounded_list)
            .filter(|_| self.compat || own != Some(Kind::ProjectScan));
        let mut listed = 0;
        let (mut is_in, mut arguments) = (false, 0usize);
        let mut pushed: Option<(f64, f64)> = None;
        for (n, v) in fields(b) {
            let Some(f) = msg.fields.get(&n) else {
                continue;
            };
            if let Some((list, _, _)) = bounded
                && f.name == list
            {
                listed += 1;
            }
            if own == Some(Kind::FunctionCall) {
                match (f.name.as_str(), v) {
                    ("argument_list", _) => arguments += 1,
                    ("function", Some(v)) => {
                        let name = FunctionRefProto::decode(v)
                            .map(|r| r.name().to_string())
                            .unwrap_or_default();
                        is_in = name.ends_with(":$in");
                        let operator = name.rsplit(':').next().is_some_and(|n| n.starts_with('$'));
                        let cost = if operator {
                            (1.0 / STACK_OPERATORS, 1.0 / STACK_OPERATORS)
                        } else {
                            (1.0 / STACK_CALLS_QUERY, 1.0 / STACK_CALLS_FUNCTION)
                        };
                        self.stack.0 += cost.0;
                        self.stack.1 += cost.1;
                        self.deepest.0 = self.deepest.0.max(self.stack.0);
                        self.deepest.1 = self.deepest.1.max(self.stack.1);
                        pushed = Some(cost);
                    }
                    _ => {}
                }
            }
            let Some(v) = v else { continue };
            match f.edge {
                Some(Edge::Parent(t)) => self.message(t, v, depth, own)?,
                Some(Edge::Child(_)) if depth >= MAX_WALK_DEPTH => self.truncated = true,
                Some(Edge::Child(t)) => self.message(t, v, depth + 1, None)?,
                None => {}
            }
        }
        if let Some(cost) = pushed {
            self.stack.0 -= cost.0;
            self.stack.1 -= cost.1;
        }
        if is_in {
            // `x IN (a, b)` is `$in(x, a, b)`.
            self.in_list = self.in_list.max(arguments.saturating_sub(1));
        }
        if let Some((_, limit, text)) = bounded
            && listed > limit
            && self.local.is_none()
        {
            self.local = Some(limit_error(text, limit));
        }
        Ok(())
    }
}

/// A STRUCT (or ARRAY<STRUCT>) parameter: at most 1000 fields per struct and 15 levels.
fn check_struct_parameter(b: &[u8]) -> Result<()> {
    let Ok(p) = ResolvedParameterProto::decode(b) else {
        return Ok(());
    };
    let Some(ty) = p.parent.and_then(|e| e.r#type) else {
        return Ok(());
    };
    let elem = |t: &TypeProto| -> Option<TypeProto> {
        match &t.array_type {
            Some(a) => a.element_type.as_deref().cloned(),
            None => Some(t.clone()),
        }
    };
    let Some(top) = elem(&ty).filter(|t| t.struct_type.is_some()) else {
        return Ok(());
    };
    let mut level = vec![top];
    let mut depth = 1;
    while !level.is_empty() {
        let mut next = Vec::new();
        for t in &level {
            let fields = t.struct_type.as_ref().map_or(&[][..], |s| &s.field[..]);
            if fields.len() > MAX_STRUCT_FIELDS {
                return Err(limit_error(
                    "Number of struct fields exceeds the maximum allowed limit of",
                    MAX_STRUCT_FIELDS,
                ));
            }
            next.extend(
                fields
                    .iter()
                    .filter_map(|f| f.field_type.as_ref().and_then(elem))
                    .filter(|t| t.struct_type.is_some()),
            );
        }
        if !next.is_empty() {
            depth += 1;
            if depth > MAX_NESTED_STRUCT_DEPTH {
                return Err(limit_error(
                    "Number of nested struct types exceeds the maximum allowed limit of",
                    MAX_NESTED_STRUCT_DEPTH,
                ));
            }
        }
        level = next;
    }
    Ok(())
}

/// Checks a serialized `AnyResolvedStatementProto` against Spanner's query size limits: the
/// emulator's with `compat`, otherwise production's, which allow more parameters and select-list
/// items, limit IN lists, and report the whole-query counts before a single node's.
pub fn check(statement: &[u8], compat: bool) -> Result<()> {
    let Some(schema) = SCHEMA.as_ref() else {
        return Ok(());
    };
    let mut w = Walk {
        schema,
        compat,
        counts: HashMap::new(),
        local: None,
        in_list: 0,
        stack: (0.0, 0.0),
        deepest: (0.0, 0.0),
        truncated: false,
    };
    w.message(schema.root, statement, 0, None)?;
    if compat && let Some(e) = w.local.take() {
        return Err(e);
    }
    let out_of_stack = |phase: &str| {
        Error::new(
            spanbolt_storage::Code::ResourceExhausted,
            format!(
                "Out of stack space due to deeply nested query expression during {phase} resolution"
            ),
        )
    };
    if !compat && (w.deepest.0 > 1.0 || w.deepest.1 > 1.0) {
        return Err(out_of_stack(if w.deepest.0 > 1.0 {
            "query"
        } else {
            "function"
        }));
    }
    let total = |k: Kind| w.counts.get(&k).map_or(0, |c| c.total);
    let nested = |k: Kind| w.counts.get(&k).map_or(0, |c| c.depths.len());
    let functions = (
        total(Kind::FunctionCall),
        MAX_FUNCTION_NODES,
        "Number of functions exceeds the maximum allowed limit of",
    );
    let nested_functions = (
        nested(Kind::FunctionCall),
        MAX_NESTED_FUNCTION_NODES,
        "Number of nested boolean predicates exceeds the maximum allowed limit of",
    );
    // Production reports the nesting before the count.
    let (first, second) = if compat {
        (functions, nested_functions)
    } else {
        (nested_functions, functions)
    };
    let checks = [
        first,
        second,
        (
            total(Kind::JoinScan),
            MAX_JOINS,
            "Number of joins exceeds the maximum allowed limit of",
        ),
        (
            nested(Kind::SubqueryExpr),
            MAX_NESTED_SUBQUERY_EXPRESSIONS,
            "Number of nested subqueries exceeds the maximum allowed limit of",
        ),
        (
            nested(Kind::ProjectScan),
            MAX_NESTED_SUBSELECTS,
            "Number of nested subselects exceeds the maximum allowed limit of",
        ),
        (
            nested(Kind::AggregateScan),
            MAX_NESTED_GROUP_BY,
            "Number of nested aggregations exceeds the maximum allowed limit of",
        ),
        (
            total(Kind::Parameter),
            MAX_PARAMETERS,
            "Number of parameters in query exceeds the maximum allowed limit of",
        ),
    ];
    for (i, (n, limit, text)) in checks.into_iter().enumerate() {
        let parameters = i == checks.len() - 1;
        if n > limit && (compat || !parameters) {
            return Err(limit_error(text, limit));
        }
    }
    if !compat && w.in_list > MAX_IN_LIST {
        return Err(limit_error(
            "The number of elements in an IN list exceeds the maximum allowed limit of",
            MAX_IN_LIST,
        ));
    }
    match w.local {
        Some(e) => Err(e),
        // Not checked to the bottom, and too deep for the planner's recursion.
        None if w.truncated => Err(out_of_stack("query")),
        None => Ok(()),
    }
}
