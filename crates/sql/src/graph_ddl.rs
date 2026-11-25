//! `CREATE PROPERTY GRAPH` as Cloud Spanner keeps it. Spanner prints a graph's definition in its
//! own form before analysing it, at creation too: every element table with its KEY, every
//! REFERENCES with its columns, every label with its property list (`PROPERTIES ALL COLUMNS` and a
//! missing PROPERTIES clause written out as the columns there are now), element tables, labels
//! and properties sorted by name. GetDatabaseDdl returns that text, and analysis errors point
//! into it. The emulator analyses the statement as written, so `--emulator-compat` keeps the
//! declaration order (but expands `ALL COLUMNS` once, too).

use spanbolt_ddl::{SqlToken, quote_ident, quote_path, sql_tokens};

/// A parsed `CREATE PROPERTY GRAPH`.
pub struct GraphDdl {
    pub name: String,
    nodes: Vec<Element>,
    edges: Vec<Element>,
}

struct Element {
    table: String,
    alias: Option<String>,
    key: Option<Vec<String>>,
    source: Option<Reference>,
    destination: Option<Reference>,
    labels: Vec<LabelDef>,
}

struct Reference {
    columns: Vec<String>,
    node: String,
    node_columns: Option<Vec<String>>,
}

struct LabelDef {
    /// `None` for `DEFAULT LABEL`, which is named after the element table.
    name: Option<String>,
    properties: Properties,
}

enum Properties {
    AllColumns {
        except: Vec<String>,
    },
    /// Expression text as written, with its `AS` name.
    List(Vec<(String, Option<String>)>),
    None,
}

/// The columns of a table or view an element table reads, and its primary key (empty for a view).
pub struct Input {
    pub columns: Vec<String>,
    pub key: Vec<String>,
}

impl Element {
    fn name(&self) -> &str {
        self.alias.as_deref().unwrap_or(&self.table)
    }
}

struct Parser<'a> {
    sql: &'a str,
    toks: Vec<(SqlToken, usize)>,
    at: usize,
}

impl Parser<'_> {
    fn peek_kw(&self, kw: &str) -> bool {
        matches!(self.toks.get(self.at), Some((SqlToken::Ident { text, quoted: false }, _)) if text.eq_ignore_ascii_case(kw))
    }

    fn kw(&mut self, kw: &str) -> bool {
        let yes = self.peek_kw(kw);
        if yes {
            self.at += 1;
        }
        yes
    }

    fn expect_kw(&mut self, kw: &str) -> Option<()> {
        self.kw(kw).then_some(())
    }

    fn peek_sym(&self, s: &str) -> bool {
        matches!(self.toks.get(self.at), Some((SqlToken::Sym(t), _)) if *t == s)
    }

    fn sym(&mut self, s: &str) -> bool {
        let yes = self.peek_sym(s);
        if yes {
            self.at += 1;
        }
        yes
    }

    fn ident(&mut self) -> Option<String> {
        match self.toks.get(self.at) {
            Some((SqlToken::Ident { text, .. }, _)) => {
                self.at += 1;
                Some(text.clone())
            }
            _ => None,
        }
    }

    /// A possibly dotted name.
    fn path(&mut self) -> Option<String> {
        let mut name = self.ident()?;
        while self.sym(".") {
            name.push('.');
            name.push_str(&self.ident()?);
        }
        Some(name)
    }

    fn ident_list(&mut self) -> Option<Vec<String>> {
        self.sym("(").then_some(())?;
        let mut out = vec![self.ident()?];
        while self.sym(",") {
            out.push(self.ident()?);
        }
        self.sym(")").then_some(out)
    }

    fn offset(&self) -> usize {
        self.toks.get(self.at).map_or(self.sql.len(), |t| t.1)
    }

    /// `(expr [AS name], ...)`, each expression as its source text.
    fn property_list(&mut self) -> Option<Vec<(String, Option<String>)>> {
        self.sym("(").then_some(())?;
        let mut out = Vec::new();
        loop {
            let start = self.offset();
            let mut depth = 0usize;
            let mut alias_at: Option<usize> = None;
            loop {
                match self.toks.get(self.at)? {
                    (SqlToken::Sym("(" | "["), _) => depth += 1,
                    (SqlToken::Sym(")" | "]"), _) if depth > 0 => depth -= 1,
                    (SqlToken::Sym(")" | ","), _) if depth == 0 => break,
                    (
                        SqlToken::Ident {
                            text,
                            quoted: false,
                        },
                        _,
                    ) if depth == 0 && text.eq_ignore_ascii_case("AS") => {
                        alias_at = Some(self.at);
                    }
                    _ => {}
                }
                self.at += 1;
            }
            let end = self.offset();
            let item = match alias_at {
                Some(i) if i + 2 == self.at => {
                    let SqlToken::Ident { text, .. } = &self.toks[i + 1].0 else {
                        return None;
                    };
                    let expr = self.sql[start..self.toks[i].1].trim();
                    (expr.to_string(), Some(text.clone()))
                }
                Some(_) => return None,
                None => (self.sql[start..end].trim().to_string(), None),
            };
            if item.0.is_empty() {
                return None;
            }
            out.push(item);
            if self.sym(")") {
                return Some(out);
            }
            self.sym(",").then_some(())?;
        }
    }

    fn properties(&mut self) -> Option<Properties> {
        if self.kw("NO") {
            self.expect_kw("PROPERTIES")?;
            return Some(Properties::None);
        }
        self.expect_kw("PROPERTIES")?;
        self.kw("ARE");
        if self.kw("ALL") {
            self.expect_kw("COLUMNS")?;
            let except = if self.kw("EXCEPT") {
                self.ident_list()?
            } else {
                Vec::new()
            };
            return Some(Properties::AllColumns { except });
        }
        Some(Properties::List(self.property_list()?))
    }

    fn reference(&mut self, what: &str) -> Option<Reference> {
        self.expect_kw(what)?;
        self.expect_kw("KEY")?;
        let columns = self.ident_list()?;
        self.expect_kw("REFERENCES")?;
        let node = self.ident()?;
        let node_columns = if self.peek_sym("(") {
            Some(self.ident_list()?)
        } else {
            None
        };
        Some(Reference {
            columns,
            node,
            node_columns,
        })
    }

    fn element(&mut self, edge: bool) -> Option<Element> {
        let table = self.path()?;
        let alias = if self.kw("AS") {
            Some(self.ident()?)
        } else {
            None
        };
        let key = if self.kw("KEY") {
            Some(self.ident_list()?)
        } else {
            None
        };
        let (source, destination) = if edge {
            (
                Some(self.reference("SOURCE")?),
                Some(self.reference("DESTINATION")?),
            )
        } else {
            (None, None)
        };
        let mut labels = Vec::new();
        loop {
            if self.kw("LABEL") {
                let name = Some(self.ident()?);
                let properties = if self.peek_kw("PROPERTIES") || self.peek_kw("NO") {
                    self.properties()?
                } else {
                    Properties::AllColumns { except: Vec::new() }
                };
                labels.push(LabelDef { name, properties });
            } else if self.kw("DEFAULT") {
                self.expect_kw("LABEL")?;
                let properties = if self.peek_kw("PROPERTIES") || self.peek_kw("NO") {
                    self.properties()?
                } else {
                    Properties::AllColumns { except: Vec::new() }
                };
                labels.push(LabelDef {
                    name: None,
                    properties,
                });
            } else if labels.is_empty() && (self.peek_kw("PROPERTIES") || self.peek_kw("NO")) {
                let properties = self.properties()?;
                labels.push(LabelDef {
                    name: None,
                    properties,
                });
                break;
            } else {
                break;
            }
        }
        if labels.is_empty() {
            labels.push(LabelDef {
                name: None,
                properties: Properties::AllColumns { except: Vec::new() },
            });
        }
        Some(Element {
            table,
            alias,
            key,
            source,
            destination,
            labels,
        })
    }

    fn elements(&mut self, edge: bool) -> Option<Vec<Element>> {
        self.expect_kw("TABLES")?;
        self.sym("(").then_some(())?;
        let mut out = vec![self.element(edge)?];
        while self.sym(",") {
            out.push(self.element(edge)?);
        }
        self.sym(")").then_some(out)
    }
}

/// Parses a `CREATE PROPERTY GRAPH` statement; `None` for anything this doesn't model (dynamic
/// labels, options, ...), which is then kept as written.
pub fn parse(sql: &str) -> Option<GraphDdl> {
    let toks = sql_tokens(sql)?;
    let mut p = Parser { sql, toks, at: 0 };
    p.expect_kw("CREATE")?;
    if p.kw("OR") {
        p.expect_kw("REPLACE")?;
    }
    p.expect_kw("PROPERTY")?;
    p.expect_kw("GRAPH")?;
    if p.kw("IF") {
        p.expect_kw("NOT")?;
        p.expect_kw("EXISTS")?;
    }
    let name = p.path()?;
    p.expect_kw("NODE")?;
    let nodes = p.elements(false)?;
    let edges = if p.kw("EDGE") {
        p.elements(true)?
    } else {
        Vec::new()
    };
    (p.at == p.toks.len()).then_some(GraphDdl { name, nodes, edges })
}

impl GraphDdl {
    /// The statement as Cloud Spanner prints it (`sorted`), or in declaration order. `input`
    /// gives the columns of a table or view; for one it doesn't know, `ALL COLUMNS` and a missing
    /// KEY stay as written, so the analyser reports the missing table.
    pub fn print(&self, sorted: bool, input: &dyn Fn(&str) -> Option<Input>) -> String {
        let key_of = |e: &Element| -> Option<Vec<String>> {
            e.key
                .clone()
                .or_else(|| input(&e.table).map(|i| i.key).filter(|k| !k.is_empty()))
        };
        let element = |e: &Element| -> String {
            let q = quote_ident;
            let mut lines = vec![match &e.alias {
                Some(a) => format!("    {} AS {}", quote_path(&e.table), q(a)),
                None => format!("    {}", quote_path(&e.table)),
            }];
            if let Some(key) = key_of(e) {
                lines.push(format!("      KEY({})", join_idents(&key)));
            }
            for (what, r) in [("SOURCE", &e.source), ("DESTINATION", &e.destination)] {
                let Some(r) = r else { continue };
                let node_cols = r.node_columns.clone().or_else(|| {
                    self.nodes
                        .iter()
                        .find(|n| n.name().eq_ignore_ascii_case(&r.node))
                        .and_then(key_of)
                });
                let target = match node_cols {
                    Some(c) => format!("{}({})", q(&r.node), join_idents(&c)),
                    None => q(&r.node),
                };
                lines.push(format!(
                    "      {what} KEY({}) REFERENCES {target}",
                    join_idents(&r.columns)
                ));
            }
            let mut labels: Vec<(&str, &Properties)> = e
                .labels
                .iter()
                .map(|l| (l.name.as_deref().unwrap_or(e.name()), &l.properties))
                .collect();
            if sorted {
                labels.sort_by(|a, b| a.0.cmp(b.0));
            }
            for (name, props) in labels {
                let items: Option<Vec<(String, String)>> = match props {
                    Properties::None => {
                        lines.push(format!("      LABEL {} NO PROPERTIES", q(name)));
                        continue;
                    }
                    Properties::AllColumns { except } => input(&e.table).map(|i| {
                        i.columns
                            .into_iter()
                            .filter(|c| !except.iter().any(|x| x.eq_ignore_ascii_case(c)))
                            .map(|c| (c.clone(), q(&c)))
                            .collect()
                    }),
                    Properties::List(list) => Some(
                        list.iter()
                            .map(|(expr, alias)| match alias {
                                Some(a) => (a.clone(), format!("{expr} AS {}", q(a))),
                                None => (expr.trim_matches('`').to_string(), expr.clone()),
                            })
                            .collect(),
                    ),
                };
                let Some(mut items) = items else {
                    let except = match props {
                        Properties::AllColumns { except } if !except.is_empty() => {
                            format!(" EXCEPT ({})", join_idents(except))
                        }
                        _ => String::new(),
                    };
                    lines.push(format!(
                        "      LABEL {} PROPERTIES ALL COLUMNS{except}",
                        q(name)
                    ));
                    continue;
                };
                if sorted {
                    items.sort_by(|a, b| a.0.cmp(&b.0));
                }
                let body = items
                    .iter()
                    .map(|(_, text)| format!("        {text}"))
                    .collect::<Vec<_>>()
                    .join(",\n");
                lines.push(format!("      LABEL {} PROPERTIES(\n{body})", q(name)));
            }
            lines.join("\n")
        };
        let block = |what: &str, tables: &[Element]| -> String {
            let mut ts: Vec<&Element> = tables.iter().collect();
            if sorted {
                ts.sort_by(|a, b| a.name().cmp(b.name()));
            }
            let body = ts
                .into_iter()
                .map(element)
                .collect::<Vec<_>>()
                .join(",\n\n");
            format!("\n  {what} TABLES(\n{body}\n  )")
        };
        let mut out = format!("CREATE PROPERTY GRAPH {}", quote_path(&self.name));
        out.push_str(&block("NODE", &self.nodes));
        if !self.edges.is_empty() {
            out.push_str(&block("EDGE", &self.edges));
        }
        out
    }
}

fn join_idents(names: &[String]) -> String {
    names
        .iter()
        .map(|n| quote_ident(n))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(t: &str) -> Option<Input> {
        let s = |v: &[&str]| v.iter().map(|c| c.to_string()).collect();
        match t {
            "Person" => Some(Input {
                columns: s(&["Id", "Name", "Extra"]),
                key: s(&["Id"]),
            }),
            "Knows" => Some(Input {
                columns: s(&["Id", "Src", "Dst", "W"]),
                key: s(&["Id"]),
            }),
            _ => None,
        }
    }

    fn print(sql: &str, sorted: bool) -> String {
        parse(sql).expect("parses").print(sorted, &input)
    }

    #[test]
    fn prints_like_cloud_spanner() {
        assert_eq!(
            print(
                "CREATE PROPERTY GRAPH G NODE TABLES (Person KEY (Id) LABEL P PROPERTIES (Id, Name)) EDGE TABLES (Knows KEY (Id) SOURCE KEY (Src) REFERENCES Person (Id) DESTINATION KEY (Dst) REFERENCES Person (Id) LABEL K PROPERTIES (Id))",
                true
            ),
            "CREATE PROPERTY GRAPH G\n  NODE TABLES(\n    Person\n      KEY(Id)\n      LABEL P PROPERTIES(\n        Id,\n        Name)\n  )\n  EDGE TABLES(\n    Knows\n      KEY(Id)\n      SOURCE KEY(Src) REFERENCES Person(Id)\n      DESTINATION KEY(Dst) REFERENCES Person(Id)\n      LABEL K PROPERTIES(\n        Id)\n  )"
        );
        assert_eq!(
            print(
                "CREATE PROPERTY GRAPH GI NODE TABLES (Person, Knows AS Friend KEY (Id) DEFAULT LABEL PROPERTIES ALL COLUMNS EXCEPT (W))",
                true
            ),
            "CREATE PROPERTY GRAPH GI\n  NODE TABLES(\n    Knows AS Friend\n      KEY(Id)\n      LABEL Friend PROPERTIES(\n        Dst,\n        Id,\n        Src),\n\n    Person\n      KEY(Id)\n      LABEL Person PROPERTIES(\n        Extra,\n        Id,\n        Name)\n  )"
        );
        assert_eq!(
            print(
                "CREATE PROPERTY GRAPH GX NODE TABLES (Person KEY (Id) LABEL P PROPERTIES (Id, UPPER(Name) AS Upper, Extra + 1 AS Next) LABEL Q NO PROPERTIES)",
                true
            ),
            "CREATE PROPERTY GRAPH GX\n  NODE TABLES(\n    Person\n      KEY(Id)\n      LABEL P PROPERTIES(\n        Id,\n        Extra + 1 AS Next,\n        UPPER(Name) AS Upper)\n      LABEL Q NO PROPERTIES\n  )"
        );
    }

    #[test]
    fn declaration_order_and_unknown_tables() {
        assert_eq!(
            print(
                "CREATE OR REPLACE PROPERTY GRAPH G NODE TABLES (Person PROPERTIES ALL COLUMNS)",
                false
            ),
            "CREATE PROPERTY GRAPH G\n  NODE TABLES(\n    Person\n      KEY(Id)\n      LABEL Person PROPERTIES(\n        Id,\n        Name,\n        Extra)\n  )"
        );
        assert_eq!(
            print(
                "CREATE PROPERTY GRAPH Bad NODE TABLES (Missing KEY (Id) LABEL X PROPERTIES ALL COLUMNS)",
                true
            ),
            "CREATE PROPERTY GRAPH Bad\n  NODE TABLES(\n    Missing\n      KEY(Id)\n      LABEL X PROPERTIES ALL COLUMNS\n  )"
        );
        assert!(
            parse("CREATE PROPERTY GRAPH G NODE TABLES (Person DYNAMIC LABEL (Name))").is_none()
        );
    }
}
