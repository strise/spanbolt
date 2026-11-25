//! The `.case` file format: parsing, and rewriting expectation blocks in place.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::Value as Json;

use crate::observe::{MessageRule, Observation};

#[derive(Debug, Clone, PartialEq)]
pub enum Verb {
    Query,
    Dml,
    Pdml,
    Batch,
    Write(WriteKind),
    Delete,
    Read,
    Begin,
    Commit,
    Rollback,
    Ddl,
    GetDdl,
    GetDatabase,
    ListSessions,
    CreateSession,
    GetSession,
    SessionLastUse,
    DeleteSession,
    GetOperation,
    ListDatabaseOperations,
    ListOperations,
    ListDatabaseRoles,
    DropDatabase,
    CompareTs,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WriteKind {
    Insert,
    Update,
    InsertOrUpdate,
    Replace,
}

#[derive(Debug, Clone)]
pub struct Param {
    pub name: String,
    pub type_text: String,
    pub value: Json,
}

#[derive(Debug, Clone)]
pub struct Step {
    pub verb: Verb,
    /// Table for writes, deletes and reads.
    pub table: Option<String>,
    pub opts: BTreeMap<String, String>,
    pub flags: Vec<String>,
    pub body: Vec<String>,
    pub params: Vec<Param>,
    pub expect: Option<Observation>,
    /// 0-based line of the header.
    pub line: usize,
    /// Lines `[start, end)` of the `expect:` block, if present.
    pub expect_span: Option<(usize, usize)>,
    /// The `expect@spanner:` block: what real Cloud Spanner returned.
    pub expect_spanner: Option<SpannerExpect>,
    /// Lines `[start, end)` of the `expect@spanner:` block, if present.
    pub expect_spanner_span: Option<(usize, usize)>,
    /// Index of the line after the step's last line (body/params), where a new `expect:` goes.
    pub insert_at: usize,
    /// `ddl protos` / `ddl protos=NAME`: the `proto_descriptors` sent with the statements.
    pub descriptors: Option<Arc<Vec<u8>>>,
}

/// A recorded real-Spanner outcome.
#[derive(Debug, Clone)]
pub enum SpannerExpect {
    /// `same`: Spanner matched the emulator expectation (or, without one, succeeded silently).
    Same,
    /// Spanner returned something else.
    Differs(Observation),
}

/// Which recorded expectation a run is checked against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Oracle {
    /// `expect:`, recorded from the official emulator.
    Emulator,
    /// `expect@spanner:` where recorded, otherwise `expect:`.
    Spanner,
}

impl Step {
    /// The expectation for `oracle`.
    pub fn expectation(&self, oracle: Oracle) -> Option<&Observation> {
        match (oracle, &self.expect_spanner) {
            (Oracle::Spanner, Some(SpannerExpect::Differs(o))) => Some(o),
            _ => self.expect.as_ref(),
        }
    }

    /// Where a new `expect@spanner:` block goes: after the `expect:` block, else after the step.
    pub fn spanner_insert_at(&self) -> usize {
        self.expect_span.map_or(self.insert_at, |(_, end)| end)
    }

    pub fn txn(&self) -> Option<&str> {
        self.opts.get("txn").map(String::as_str)
    }
    pub fn sql(&self) -> String {
        self.body.join("\n")
    }
    pub fn has_flag(&self, f: &str) -> bool {
        self.flags.iter().any(|x| x == f)
    }
}

#[derive(Debug, Clone)]
pub struct Case {
    /// `<relative file path without .case>/<name>`.
    pub name: String,
    pub tags: Vec<String>,
    pub schema: Arc<Vec<String>>,
    /// The `proto:` block in effect: `proto_descriptors` for `CreateDatabase`.
    pub descriptors: Option<Arc<Vec<u8>>>,
    pub prelude: Arc<Vec<Step>>,
    pub steps: Vec<Step>,
    pub skip: Option<String>,
    /// `skip@emulator:`: run only against Cloud Spanner (`--oracle spanner`), for statements the
    /// emulator can't answer (it aborts on them).
    pub skip_emulator: Option<String>,
    pub file: usize,
    pub line: usize,
}

impl Case {
    /// Why the case doesn't run against `oracle`'s expectations, if it doesn't.
    pub fn skip_for(&self, oracle: Oracle) -> Option<&str> {
        match oracle {
            Oracle::Emulator => self.skip.as_deref().or(self.skip_emulator.as_deref()),
            Oracle::Spanner => self.skip.as_deref(),
        }
    }

    /// Identity of the starting state (schema + prelude), for template databases.
    pub fn template_key(&self) -> String {
        let mut key = self.schema.join(";\n");
        if let Some(d) = &self.descriptors {
            key.push_str(&format!("\n{d:?}"));
        }
        for s in self.prelude.iter() {
            key.push('\n');
            key.push_str(&format!(
                "{:?}{:?}{:?}{:?}",
                s.verb, s.table, s.opts, s.body
            ));
        }
        key
    }
}

#[derive(Debug)]
pub struct CaseFile {
    pub path: PathBuf,
    /// Path relative to the corpus root without `.case`: the prefix of its case names.
    pub rel: String,
    pub lines: Vec<String>,
}

#[derive(Debug)]
pub struct Corpus {
    pub files: Vec<CaseFile>,
    pub cases: Vec<Case>,
}

/// Loads every `*.case` under `root`.
pub fn load(root: &Path) -> Result<Corpus, String> {
    let mut paths = Vec::new();
    walk(root, &mut paths).map_err(|e| format!("{}: {e}", root.display()))?;
    paths.sort();
    let mut corpus = Corpus {
        files: Vec::new(),
        cases: Vec::new(),
    };
    let mut errors = Vec::new();
    for path in paths {
        let text =
            std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .with_extension("")
            .to_string_lossy()
            .replace('\\', "/");
        let idx = corpus.files.len();
        match parse(&text, &rel, idx) {
            Ok(cases) => corpus.cases.extend(cases),
            Err(e) => errors.push(format!("{}:{e}", path.display())),
        }
        corpus.files.push(CaseFile {
            path,
            rel,
            lines: text.lines().map(str::to_string).collect(),
        });
    }
    if !errors.is_empty() {
        return Err(errors.join("\n"));
    }
    let mut seen = std::collections::HashSet::new();
    for c in &corpus.cases {
        if !seen.insert(c.name.clone()) {
            return Err(format!("duplicate case name {}", c.name));
        }
    }
    Ok(corpus)
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            walk(&path, out)?;
        } else if path.extension().is_some_and(|e| e == "case") {
            out.push(path);
        }
    }
    Ok(())
}

/// Splits DDL / batch bodies into statements at `;` line ends.
pub fn split_statements(body: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = Vec::new();
    for line in body {
        let trimmed = line.trim_end();
        if let Some(stripped) = trimmed.strip_suffix(';') {
            cur.push(stripped.to_string());
            push_stmt(&mut out, &mut cur);
        } else {
            cur.push(line.clone());
        }
    }
    push_stmt(&mut out, &mut cur);
    out
}

fn push_stmt(out: &mut Vec<String>, cur: &mut Vec<String>) {
    let s = cur.join("\n").trim().to_string();
    cur.clear();
    if !s.is_empty() {
        out.push(s);
    }
}

struct Block {
    header: String,
    body: Vec<String>,
    line: usize,
    /// One past the last non-blank line of the block.
    end: usize,
}

fn blocks(text: &str) -> Result<Vec<Block>, String> {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if line.trim().is_empty() || line.starts_with('#') {
            i += 1;
            continue;
        }
        if line.starts_with(' ') || line.starts_with('\t') {
            return Err(format!("{}: indented line outside a block", i + 1));
        }
        let start = i;
        i += 1;
        let mut body = Vec::new();
        let mut end = i;
        while i < lines.len() {
            let l = lines[i];
            if l.trim().is_empty() {
                body.push(String::new());
                i += 1;
                continue;
            }
            if let Some(b) = l.strip_prefix("  ") {
                body.push(b.to_string());
                i += 1;
                end = i;
            } else {
                break;
            }
        }
        body.truncate(end - start - 1);
        out.push(Block {
            header: line.trim_end().to_string(),
            body,
            line: start,
            end,
        });
        i = end;
    }
    Ok(out)
}

pub fn parse(text: &str, rel: &str, file: usize) -> Result<Vec<Case>, String> {
    let top = rel.split('/').next().unwrap_or(rel).to_string();
    let mut file_tags = vec![top];
    let mut file_schema: Vec<String> = Vec::new();
    let mut prelude: Vec<Step> = Vec::new();
    let mut prelude_arc: Option<Arc<Vec<Step>>> = None;
    let mut schema_arc: Option<Arc<Vec<String>>> = None;
    // `proto NAME:` blocks by name (`""` for `proto:`); a case's own blocks extend the file's.
    let mut file_protos: BTreeMap<String, Arc<Vec<u8>>> = BTreeMap::new();
    let mut case_protos: Vec<BTreeMap<String, Arc<Vec<u8>>>> = Vec::new();
    let mut cases: Vec<Case> = Vec::new();
    for b in blocks(text)? {
        let at = |e: String| format!("{}: {e}", b.line + 1);
        if let Some(name) = b.header.strip_prefix("case ") {
            let name = name.trim();
            if name.is_empty() || name.contains(char::is_whitespace) {
                return Err(at(format!("bad case name `{name}`")));
            }
            let schema = schema_arc
                .get_or_insert_with(|| Arc::new(file_schema.clone()))
                .clone();
            if prelude_arc.is_none() {
                for step in &mut prelude {
                    resolve_protos(step, &file_protos)
                        .map_err(|e| format!("{}: {e}", step.line + 1))?;
                }
            }
            let pre = prelude_arc
                .get_or_insert_with(|| Arc::new(prelude.clone()))
                .clone();
            case_protos.push(file_protos.clone());
            cases.push(Case {
                name: format!("{rel}/{name}"),
                tags: file_tags.clone(),
                schema,
                descriptors: None,
                prelude: pre,
                steps: Vec::new(),
                skip: None,
                skip_emulator: None,
                file,
                line: b.line,
            });
            continue;
        }
        if let Some(t) = b.header.strip_prefix("tags:") {
            let tags = t.split_whitespace().map(str::to_string);
            match cases.last_mut() {
                Some(c) => c.tags.extend(tags),
                None => file_tags.extend(tags),
            }
            continue;
        }
        if let Some(reason) = b.header.strip_prefix("skip@emulator:") {
            let c = cases
                .last_mut()
                .ok_or_else(|| at("skip@emulator: outside a case".into()))?;
            c.skip_emulator = Some(reason.trim().to_string());
            continue;
        }
        if let Some(reason) = b.header.strip_prefix("skip:") {
            let c = cases
                .last_mut()
                .ok_or_else(|| at("skip: outside a case".into()))?;
            c.skip = Some(reason.trim().to_string());
            continue;
        }
        if b.header == "proto:" || b.header.starts_with("proto ") {
            let name = b.header.strip_prefix("proto").unwrap_or_default();
            let name = name
                .strip_suffix(':')
                .ok_or_else(|| at("`proto NAME:` needs a body".into()))?;
            let d = crate::protos::descriptors(&b.body).map_err(at)?;
            match case_protos.last_mut() {
                Some(m) => m.insert(name.trim().to_string(), d),
                None => file_protos.insert(name.trim().to_string(), d),
            };
            continue;
        }
        if b.header == "schema:" {
            let stmts = split_statements(&b.body);
            match cases.last_mut() {
                Some(c) => c.schema = Arc::new(stmts),
                None => file_schema = stmts,
            }
            continue;
        }
        let in_prelude = cases.is_empty();
        let steps: &mut Vec<Step> = match cases.last_mut() {
            Some(c) => &mut c.steps,
            None => &mut prelude,
        };
        if b.header == "params:" {
            let step = steps
                .last_mut()
                .ok_or_else(|| at("params: without a step".into()))?;
            for l in b.body.iter().filter(|l| !l.trim().is_empty()) {
                step.params.push(parse_param(l).map_err(at)?);
            }
            step.insert_at = b.end;
            continue;
        }
        if b.header == "expect:" {
            if in_prelude {
                return Err(at("expect: in the prelude".into()));
            }
            let step = steps
                .last_mut()
                .ok_or_else(|| at("expect: without a step".into()))?;
            step.expect = Some(parse_expect(&b.body).map_err(at)?);
            step.expect_span = Some((b.line, b.end));
            continue;
        }
        if b.header == "expect@spanner:" {
            if in_prelude {
                return Err(at("expect@spanner: in the prelude".into()));
            }
            let step = steps
                .last_mut()
                .ok_or_else(|| at("expect@spanner: without a step".into()))?;
            let body: Vec<&str> = b
                .body
                .iter()
                .map(|l| l.trim())
                .filter(|l| !l.is_empty() && !l.starts_with('#'))
                .collect();
            step.expect_spanner = Some(if body == ["same"] {
                SpannerExpect::Same
            } else {
                SpannerExpect::Differs(parse_expect(&b.body).map_err(at)?)
            });
            step.expect_spanner_span = Some((b.line, b.end));
            continue;
        }
        steps.push(parse_step(&b).map_err(at)?);
    }
    for (c, protos) in cases.iter_mut().zip(&case_protos) {
        let mut seen = std::collections::HashSet::new();
        c.tags.retain(|t| seen.insert(t.clone()));
        c.descriptors = protos.get("").cloned();
        for step in &mut c.steps {
            resolve_protos(step, protos).map_err(|e| format!("{}: {e}", step.line + 1))?;
        }
    }
    Ok(cases)
}

/// Sets a `ddl` step's descriptors from its `protos` flag (the unnamed `proto:`) or
/// `protos=NAME` option.
fn resolve_protos(step: &mut Step, protos: &BTreeMap<String, Arc<Vec<u8>>>) -> Result<(), String> {
    let name = match (step.has_flag("protos"), step.opts.get("protos")) {
        (true, _) => "",
        (false, Some(n)) => n.as_str(),
        (false, None) => return Ok(()),
    };
    if step.verb != Verb::Ddl {
        return Err("`protos` only applies to `ddl`".into());
    }
    let d = protos
        .get(name)
        .ok_or_else(|| format!("no `proto {name}:` block for `protos`"))?;
    step.descriptors = Some(d.clone());
    Ok(())
}

fn parse_step(b: &Block) -> Result<Step, String> {
    let (head, has_body) = match b.header.strip_suffix(':') {
        Some(h) => (h, true),
        None => (b.header.as_str(), false),
    };
    let mut tokens = head.split_whitespace();
    let verb_text = tokens.next().unwrap_or_default();
    let verb = match verb_text {
        "query" => Verb::Query,
        "dml" => Verb::Dml,
        "pdml" => Verb::Pdml,
        "batch" => Verb::Batch,
        "insert" => Verb::Write(WriteKind::Insert),
        "update" => Verb::Write(WriteKind::Update),
        "insert_or_update" => Verb::Write(WriteKind::InsertOrUpdate),
        "replace" => Verb::Write(WriteKind::Replace),
        "delete" => Verb::Delete,
        "read" => Verb::Read,
        "begin" => Verb::Begin,
        "commit" => Verb::Commit,
        "rollback" => Verb::Rollback,
        "ddl" => Verb::Ddl,
        "get_ddl" => Verb::GetDdl,
        "get_database" => Verb::GetDatabase,
        "list_sessions" => Verb::ListSessions,
        "create_session" => Verb::CreateSession,
        "get_session" => Verb::GetSession,
        "session_last_use" => Verb::SessionLastUse,
        "delete_session" => Verb::DeleteSession,
        "get_operation" => Verb::GetOperation,
        "list_database_operations" => Verb::ListDatabaseOperations,
        "list_operations" => Verb::ListOperations,
        "list_database_roles" => Verb::ListDatabaseRoles,
        "drop_database" => Verb::DropDatabase,
        "compare_ts" => Verb::CompareTs,
        other => return Err(format!("unknown directive `{other}`")),
    };
    let needs_table = matches!(verb, Verb::Write(_) | Verb::Delete | Verb::Read);
    let table = if needs_table {
        Some(
            tokens
                .next()
                .ok_or_else(|| format!("`{verb_text}` needs a table"))?
                .to_string(),
        )
    } else {
        None
    };
    let mut opts = BTreeMap::new();
    let mut flags = Vec::new();
    for t in tokens {
        match t.split_once('=') {
            Some((k, v)) => {
                opts.insert(k.to_string(), v.to_string());
            }
            None => flags.push(t.to_string()),
        }
    }
    let wants_body = !matches!(
        verb,
        Verb::Begin
            | Verb::Commit
            | Verb::Rollback
            | Verb::GetDdl
            | Verb::GetDatabase
            | Verb::ListSessions
            | Verb::CreateSession
            | Verb::GetSession
            | Verb::SessionLastUse
            | Verb::DeleteSession
            | Verb::GetOperation
            | Verb::ListDatabaseOperations
            | Verb::ListOperations
            | Verb::ListDatabaseRoles
            | Verb::DropDatabase
            | Verb::CompareTs
    );
    if wants_body && (!has_body || b.body.iter().all(|l| l.trim().is_empty())) {
        return Err(format!("`{verb_text}` needs a body"));
    }
    if !wants_body && b.body.iter().any(|l| !l.trim().is_empty()) {
        return Err(format!("`{verb_text}` takes no body"));
    }
    let names_session = matches!(
        verb,
        Verb::CreateSession | Verb::GetSession | Verb::SessionLastUse
    );
    if verb == Verb::CompareTs && flags.len() != 2 {
        return Err("`compare_ts` takes two saved timestamps: `compare_ts A B`".into());
    }
    if names_session && !opts.contains_key("session") {
        return Err(format!("`{verb_text}` needs session="));
    }
    if verb == Verb::DeleteSession && opts.contains_key("session") == opts.contains_key("name") {
        return Err(format!("`{verb_text}` needs session= or name="));
    }
    if verb == Verb::GetOperation && !opts.contains_key("op") {
        return Err(format!("`{verb_text}` needs op="));
    }
    if matches!(verb, Verb::Write(_) | Verb::Read) && !opts.contains_key("cols") {
        return Err(format!("`{verb_text}` needs cols="));
    }
    Ok(Step {
        verb,
        table,
        opts,
        flags,
        body: b.body.clone(),
        params: Vec::new(),
        expect: None,
        line: b.line,
        expect_span: None,
        expect_spanner: None,
        expect_spanner_span: None,
        insert_at: b.end,
        descriptors: None,
    })
}

fn parse_param(line: &str) -> Result<Param, String> {
    let line = line.trim();
    let rest = line
        .strip_prefix('@')
        .ok_or_else(|| format!("param line must start with @: `{line}`"))?;
    let (decl, value) = rest
        .split_once(" = ")
        .ok_or_else(|| format!("param needs ` = `: `{line}`"))?;
    let (name, type_text) = decl
        .trim()
        .split_once(char::is_whitespace)
        .ok_or_else(|| format!("param needs a type: `{line}`"))?;
    crate::value::parse_type(type_text.trim())?;
    let value: Json =
        serde_json::from_str(value.trim()).map_err(|e| format!("param value `{value}`: {e}"))?;
    Ok(Param {
        name: name.to_string(),
        type_text: type_text.trim().to_string(),
        value,
    })
}

fn parse_expect(body: &[String]) -> Result<Observation, String> {
    let mut obs = Observation::default();
    let mut in_statements = false;
    for raw in body {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if in_statements {
            let s: String =
                serde_json::from_str(line).map_err(|e| format!("statement `{line}`: {e}"))?;
            obs.statements.get_or_insert_with(Vec::new).push(s);
            continue;
        }
        if line == "ok" {
            continue;
        }
        if line.starts_with('[') {
            let row: Json = serde_json::from_str(line).map_err(|e| format!("row `{line}`: {e}"))?;
            obs.rows.push(row);
            continue;
        }
        if let Some(v) = line.strip_prefix("error:") {
            obs.error = Some(v.trim().to_string());
        } else if let Some(v) = line.strip_prefix("message~:") {
            let re = regex::Regex::new(v.trim()).map_err(|e| format!("message~: {e}"))?;
            obs.message = Some(MessageRule::Regex(re));
        } else if let Some(v) = line.strip_prefix("message:") {
            let v = v.trim();
            obs.message = Some(if v == "ignore" {
                MessageRule::Ignore
            } else {
                MessageRule::Exact(v.to_string())
            });
        } else if let Some(v) = line.strip_prefix("columns:") {
            obs.columns = Some(v.trim().to_string());
        } else if let Some(v) = line.strip_prefix("counts:") {
            let c: Vec<i64> = serde_json::from_str(v.trim()).map_err(|e| format!("counts: {e}"))?;
            obs.counts = Some(c);
        } else if let Some(v) = line.strip_prefix("count:") {
            obs.count = Some(v.trim().parse().map_err(|e| format!("count: {e}"))?);
        } else if line == "statements:" {
            in_statements = true;
            obs.statements = Some(Vec::new());
        } else {
            return Err(format!("unknown expectation line `{line}`"));
        }
    }
    Ok(obs)
}

/// Rewrites `lines`, replacing or inserting `expect:` blocks. `updates` maps a step to its new
/// `expect:` body lines (unindented).
pub fn rewrite(lines: &[String], steps: &[(&Step, Vec<String>)]) -> Vec<String> {
    rewrite_blocks(lines, steps, Oracle::Emulator)
}

/// Like [`rewrite`], for the `expect@spanner:` blocks. Leaves `expect:` blocks alone.
pub fn rewrite_spanner(lines: &[String], steps: &[(&Step, Vec<String>)]) -> Vec<String> {
    rewrite_blocks(lines, steps, Oracle::Spanner)
}

fn rewrite_blocks(lines: &[String], steps: &[(&Step, Vec<String>)], oracle: Oracle) -> Vec<String> {
    let header = match oracle {
        Oracle::Emulator => "expect:",
        Oracle::Spanner => "expect@spanner:",
    };
    let mut replace: BTreeMap<usize, (usize, &Vec<String>)> = BTreeMap::new();
    let mut insert: BTreeMap<usize, &Vec<String>> = BTreeMap::new();
    for (step, body) in steps {
        let (span, at) = match oracle {
            Oracle::Emulator => (step.expect_span, step.insert_at),
            Oracle::Spanner => (step.expect_spanner_span, step.spanner_insert_at()),
        };
        match span {
            Some((s, e)) => {
                replace.insert(s, (e, body));
            }
            None => {
                insert.insert(at, body);
            }
        }
    }
    let mut out = Vec::with_capacity(lines.len());
    let mut i = 0;
    let emit = |out: &mut Vec<String>, body: &Vec<String>| {
        out.push(header.to_string());
        out.extend(body.iter().map(|l| format!("  {l}")));
    };
    while i <= lines.len() {
        if let Some(body) = insert.get(&i) {
            emit(&mut out, body);
        }
        if i == lines.len() {
            break;
        }
        if let Some((end, body)) = replace.get(&i) {
            emit(&mut out, body);
            i = *end;
            continue;
        }
        out.push(lines[i].clone());
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILE: &str = "tags: sql
schema:
  CREATE TABLE T (
    K INT64 NOT NULL,
  ) PRIMARY KEY (K);
  CREATE INDEX I ON T(K)
insert T cols=K:
  [1]

case one
query:
  SELECT K FROM T

  ORDER BY K
params:
  @p ARRAY<STRUCT<a INT64>> = [[1]]
# trailing comment

case two
tags: extra
begin txn=a
read T cols=K index=I limit=1 txn=a:
  all
expect:
  columns: K INT64
  [\"1\"]
commit txn=a
";

    #[test]
    fn parses_cases_and_prelude() {
        let cases = parse(FILE, "sql/basic", 0).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(cases.len(), 2);
        let one = &cases[0];
        assert_eq!(one.name, "sql/basic/one");
        assert_eq!(one.schema.len(), 2);
        assert_eq!(one.prelude.len(), 1);
        assert_eq!(one.steps[0].sql(), "SELECT K FROM T\n\nORDER BY K");
        assert_eq!(one.steps[0].params[0].type_text, "ARRAY<STRUCT<a INT64>>");
        let two = &cases[1];
        assert_eq!(two.tags, vec!["sql", "extra"]);
        assert_eq!(two.steps.len(), 3);
        assert_eq!(
            two.steps[1].opts.get("index").map(String::as_str),
            Some("I")
        );
        assert_eq!(two.steps[1].expect.as_ref().map(|e| e.rows.len()), Some(1));
    }

    #[test]
    fn rewrite_inserts_and_replaces() {
        let cases = parse(FILE, "sql/basic", 0).unwrap_or_else(|e| panic!("{e}"));
        let lines: Vec<String> = FILE.lines().map(str::to_string).collect();
        let a = vec!["columns: K INT64".to_string(), "[\"1\"]".to_string()];
        let b = vec!["ok".to_string()];
        let out = rewrite(
            &lines,
            &[(&cases[0].steps[0], a.clone()), (&cases[1].steps[1], b)],
        );
        let text = out.join("\n");
        assert!(text.contains("  @p ARRAY<STRUCT<a INT64>> = [[1]]\nexpect:\n  columns: K INT64\n  [\"1\"]\n# trailing comment"));
        assert!(text.contains("  all\nexpect:\n  ok\ncommit txn=a"));
        let again = parse(&text, "sql/basic", 0).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            again[0].steps[0].expect.as_ref().map(|e| e.rows.len()),
            Some(1)
        );
    }

    #[test]
    fn spanner_blocks_parse_and_rewrite() {
        let cases = parse(FILE, "sql/basic", 0).unwrap_or_else(|e| panic!("{e}"));
        let lines: Vec<String> = FILE.lines().map(str::to_string).collect();
        let same = vec!["same".to_string()];
        let differs = vec![
            "error: INVALID_ARGUMENT".to_string(),
            "message: no".to_string(),
        ];
        let out = rewrite_spanner(
            &lines,
            &[(&cases[0].steps[0], differs), (&cases[1].steps[1], same)],
        );
        let text = out.join("\n");
        assert!(text.contains("[[1]]\nexpect@spanner:\n  error: INVALID_ARGUMENT"));
        assert!(text.contains("  [\"1\"]\nexpect@spanner:\n  same\ncommit txn=a"));
        let again = parse(&text, "sql/basic", 0).unwrap_or_else(|e| panic!("{e}"));
        let q = &again[0].steps[0];
        assert!(q.expect.is_none());
        assert_eq!(
            q.expectation(Oracle::Spanner)
                .and_then(|o| o.error.as_deref()),
            Some("INVALID_ARGUMENT")
        );
        assert!(q.expectation(Oracle::Emulator).is_none());
        let r = &again[1].steps[1];
        assert!(matches!(r.expect_spanner, Some(SpannerExpect::Same)));
        assert_eq!(
            r.expectation(Oracle::Spanner).map(|e| e.rows.len()),
            Some(1)
        );
        // Re-recording the emulator keeps the Spanner block and vice versa.
        let ok = vec!["ok".to_string()];
        let text2 = rewrite(&out, &[(&again[0].steps[0], ok)]).join("\n");
        assert!(text2.contains("[[1]]\nexpect:\n  ok\nexpect@spanner:\n  error: INVALID_ARGUMENT"));
        let again2 = parse(&text2, "sql/basic", 0).unwrap_or_else(|e| panic!("{e}"));
        let out3 = rewrite_spanner(
            &text2.lines().map(str::to_string).collect::<Vec<_>>(),
            &[(&again2[0].steps[0], vec!["same".to_string()])],
        )
        .join("\n");
        assert!(out3.contains("[[1]]\nexpect:\n  ok\nexpect@spanner:\n  same\n# trailing"));
    }

    #[test]
    fn statements_split_at_semicolon_line_ends() {
        let body: Vec<String> = [
            "UPDATE T SET a = ';' WHERE true;",
            "",
            "DELETE FROM T",
            "WHERE true",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(
            split_statements(&body),
            vec![
                "UPDATE T SET a = ';' WHERE true",
                "DELETE FROM T\nWHERE true"
            ]
        );
    }
}
