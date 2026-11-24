//! Recursive-descent parser for Spanner GoogleSQL DDL.

use crate::DdlError;
use crate::lexer::{Tok, Token, is_reserved, syntax, tokenize};
use crate::schema::*;

pub(crate) struct Parser<'a> {
    src: &'a str,
    toks: Vec<Token>,
    pos: usize,
    /// Follow the official emulator's grammar where it differs from production Spanner.
    emulator: bool,
    /// Set by `column_rest_in` when the column carries `PRIMARY KEY`.
    column_pk: bool,
}

type R<T> = Result<T, DdlError>;

pub fn parse_statement(sql: &str) -> R<Statement> {
    parse(sql, false)
}

/// `parse_statement` with the official emulator's grammar (`--emulator-compat`), which lacks
/// some production statements (ALTER SEARCH INDEX) and words some errors differently.
pub fn parse_statement_emulator_compat(sql: &str) -> R<Statement> {
    parse(sql, true)
}

fn parse(sql: &str, emulator: bool) -> R<Statement> {
    let mut p = Parser::new(sql)?;
    p.emulator = emulator;
    let stmt = p.statement()?;
    p.expect_eof()?;
    Ok(stmt)
}

/// True if the statement starts with a DDL keyword (CREATE, ALTER, DROP, RENAME, GRANT, REVOKE, ANALYZE).
pub fn is_ddl(sql: &str) -> bool {
    let Ok(toks) = tokenize(sql) else {
        return false;
    };
    match toks.first().map(|t| &t.tok) {
        Some(Tok::Ident(w)) => [
            "CREATE", "ALTER", "DROP", "RENAME", "GRANT", "REVOKE", "ANALYZE",
        ]
        .iter()
        .any(|k| k.eq_ignore_ascii_case(w)),
        _ => false,
    }
}

impl<'a> Parser<'a> {
    pub(crate) fn new(src: &'a str) -> R<Self> {
        Ok(Parser {
            src,
            toks: tokenize(src)?,
            pos: 0,
            emulator: false,
            column_pk: false,
        })
    }

    fn tok(&self) -> &Tok {
        &self.toks[self.pos].tok
    }

    fn tok_at(&self, n: usize) -> &Tok {
        let i = (self.pos + n).min(self.toks.len() - 1);
        &self.toks[i].tok
    }

    fn bump(&mut self) {
        if self.pos < self.toks.len() - 1 {
            self.pos += 1;
        }
    }

    fn kw_at(&self, n: usize, kw: &str) -> bool {
        matches!(self.tok_at(n), Tok::Ident(w) if w.eq_ignore_ascii_case(kw))
    }

    fn is_kw(&self, kw: &str) -> bool {
        self.kw_at(0, kw)
    }

    fn eat_kw(&mut self, kw: &str) -> bool {
        if self.is_kw(kw) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn eat_kws(&mut self, kws: &[&str]) -> bool {
        if kws.iter().enumerate().all(|(i, k)| self.kw_at(i, k)) {
            for _ in kws {
                self.bump();
            }
            true
        } else {
            false
        }
    }

    fn expect_kw(&mut self, kw: &str) -> R<()> {
        if self.eat_kw(kw) {
            Ok(())
        } else {
            Err(self.expecting(kw))
        }
    }

    fn is_sym(&self, s: &str) -> bool {
        matches!(self.tok(), Tok::Sym(x) if *x == s)
    }

    fn eat_sym(&mut self, s: &str) -> bool {
        if self.is_sym(s) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn expect_sym(&mut self, s: &str) -> R<()> {
        if self.eat_sym(s) {
            Ok(())
        } else {
            Err(self.expecting(s))
        }
    }

    /// Consumes a `>` even when the lexer produced `>>`.
    fn expect_gt(&mut self) -> R<()> {
        if self.is_sym(">>") {
            let t = &mut self.toks[self.pos];
            t.tok = Tok::Sym(">");
            t.start += 1;
            return Ok(());
        }
        self.expect_sym(">")
    }

    fn expect_eof(&self) -> R<()> {
        if matches!(self.tok(), Tok::Eof) {
            Ok(())
        } else {
            Err(self.expecting("EOF"))
        }
    }

    fn found(&self) -> String {
        let t = &self.toks[self.pos];
        match &t.tok {
            Tok::Eof => "EOF".to_string(),
            _ => self.src[t.start..t.end].to_string(),
        }
    }

    fn expecting(&self, what: &str) -> DdlError {
        syntax(
            self.src,
            self.toks[self.pos].start,
            &format!("Expecting '{}' but found '{}'", what, self.found()),
        )
    }

    /// An error about the whole statement, in the emulator's wording.
    fn statement_error(&self, msg: &str) -> DdlError {
        DdlError::Syntax(format!(
            "Error parsing Spanner DDL statement: {} : {msg}",
            self.src
        ))
    }

    fn pk_once(&self) -> DdlError {
        self.statement_error("Can only specify PRIMARY KEY once.")
    }

    fn encountered(&self, what: &str) -> DdlError {
        syntax(
            self.src,
            self.toks[self.pos].start,
            &format!("Encountered '{}' while parsing: {}", self.found(), what),
        )
    }

    /// Spanner DDL that spanbolt doesn't model: UNIMPLEMENTED rather than a syntax error, so a
    /// client can tell "valid but unsupported" from "invalid".
    fn unimplemented(&self, what: &str) -> DdlError {
        DdlError::Unimplemented(format!("{what} is not supported by spanbolt yet."))
    }

    /// After CREATE/ALTER/DROP: the object kinds Spanner has and spanbolt doesn't support.
    fn unsupported_kind(&self, verb: &str) -> Option<DdlError> {
        const KINDS: &[(&[&str], &str)] = &[
            (&["PROTO", "BUNDLE"], "PROTO BUNDLE"),
            (&["VECTOR", "INDEX"], "VECTOR INDEX"),
            (&["QUEUE"], "QUEUE"),
            (&["PLACEMENT"], "PLACEMENT"),
            (&["FUNCTION"], "FUNCTION"),
        ];
        KINDS
            .iter()
            .find(|(kws, _)| kws.iter().enumerate().all(|(i, k)| self.kw_at(i, k)))
            .map(|(_, kind)| self.unimplemented(&format!("{verb} {kind}")))
    }

    fn unsupported(&self, what: &str) -> DdlError {
        DdlError::Syntax(format!(
            "Error parsing Spanner DDL statement: {} : {what} is not supported",
            self.src
        ))
    }

    /// One identifier part.
    fn ident(&mut self) -> R<String> {
        match self.tok().clone() {
            Tok::Ident(w) if !is_reserved(&w) => {
                self.bump();
                Ok(w)
            }
            Tok::QuotedIdent(w) => {
                self.bump();
                Ok(w)
            }
            _ => Err(self.encountered("identifier")),
        }
    }

    /// Identifier part after a `.`; reserved words are allowed there.
    fn path_part(&mut self) -> R<String> {
        match self.tok().clone() {
            Tok::Ident(w) | Tok::QuotedIdent(w) => {
                self.bump();
                Ok(w)
            }
            _ => Err(self.encountered("identifier")),
        }
    }

    /// Dotted name, e.g. `schema.table`.
    fn path(&mut self) -> R<String> {
        let mut name = self.ident()?;
        while self.is_sym(".") {
            self.bump();
            name.push('.');
            name.push_str(&self.path_part()?);
        }
        Ok(name)
    }

    fn ident_list(&mut self) -> R<Vec<String>> {
        self.expect_sym("(")?;
        if self.eat_sym(")") {
            return Ok(Vec::new());
        }
        self.ident_list_rest(Vec::new())
    }

    /// Non-empty `( a, b )`.
    fn ident_list1(&mut self) -> R<Vec<String>> {
        self.expect_sym("(")?;
        self.ident_list_rest(Vec::new())
    }

    fn ident_list_rest(&mut self, mut out: Vec<String>) -> R<Vec<String>> {
        loop {
            out.push(self.ident()?);
            if self.eat_sym(")") {
                return Ok(out);
            }
            self.expect_sym(",")?;
        }
    }

    fn key_parts(&mut self) -> R<Vec<KeyPart>> {
        self.expect_sym("(")?;
        let mut out = Vec::new();
        if self.eat_sym(")") {
            return Ok(out);
        }
        loop {
            out.push(self.key_part()?);
            if self.eat_sym(")") {
                return Ok(out);
            }
            self.expect_sym(",")?;
        }
    }

    fn key_part(&mut self) -> R<KeyPart> {
        let column = self.ident()?;
        let order = if self.eat_kw("DESC") {
            SortOrder::Desc
        } else {
            self.eat_kw("ASC");
            SortOrder::Asc
        };
        Ok(KeyPart { column, order })
    }

    fn uint(&mut self) -> R<u64> {
        if let Tok::Int(s) = self.tok().clone() {
            let v = if let Some(h) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
                u64::from_str_radix(h, 16).ok()
            } else {
                s.parse().ok()
            };
            if let Some(v) = v {
                self.bump();
                return Ok(v);
            }
        }
        Err(self.encountered("integer literal"))
    }

    /// `( ... )` with balanced parentheses; returns the trimmed inner source text.
    fn paren_text(&mut self) -> R<String> {
        if !self.is_sym("(") {
            return Err(self.expecting("("));
        }
        let open = self.toks[self.pos].end;
        let mut depth = 0usize;
        loop {
            match self.tok() {
                Tok::Sym("(") => depth += 1,
                Tok::Sym(")") => {
                    depth -= 1;
                    if depth == 0 {
                        let close = self.toks[self.pos].start;
                        self.bump();
                        return Ok(self.src[open..close].trim().to_string());
                    }
                }
                Tok::Eof => return Err(self.expecting(")")),
                _ => {}
            }
            self.bump();
        }
    }

    /// Source text from the current token up to (not including) a top-level token where `stop`
    /// returns true, or EOF.
    fn text_until(&mut self, stop: impl Fn(&Self) -> bool) -> R<String> {
        let start = self.toks[self.pos].start;
        let mut end = start;
        let mut depth = 0usize;
        loop {
            match self.tok() {
                Tok::Eof => break,
                Tok::Sym("(") => depth += 1,
                Tok::Sym(")") if depth == 0 => break,
                Tok::Sym(")") => depth -= 1,
                _ if depth == 0 && stop(self) => break,
                _ => {}
            }
            end = self.toks[self.pos].end;
            self.bump();
        }
        if end == start {
            return Err(self.encountered("expression"));
        }
        Ok(self.src[start..end].trim().to_string())
    }

    fn rest_text(&mut self) -> String {
        let start = self.toks[self.pos].start;
        self.pos = self.toks.len() - 1;
        self.src[start..].trim().to_string()
    }

    /// `OPTIONS ( k = v, ... )` with values normalised to SQL literal text.
    fn options(&mut self) -> R<Vec<(String, String)>> {
        self.expect_kw("OPTIONS")?;
        self.expect_sym("(")?;
        let mut out = Vec::new();
        if self.eat_sym(")") {
            return Ok(out);
        }
        loop {
            let key = self.ident()?;
            self.expect_sym("=")?;
            let value = self.option_value()?;
            out.push((key, value));
            if self.eat_sym(")") {
                return Ok(out);
            }
            self.expect_sym(",")?;
        }
    }

    fn option_value(&mut self) -> R<String> {
        let neg = self.eat_sym("-");
        let v = match self.tok().clone() {
            Tok::Str(s) => quote_string(&s),
            Tok::Int(s) | Tok::Float(s) => s,
            Tok::Ident(w) if w.eq_ignore_ascii_case("true") => "true".into(),
            Tok::Ident(w) if w.eq_ignore_ascii_case("false") => "false".into(),
            Tok::Ident(w) if w.eq_ignore_ascii_case("null") => "NULL".into(),
            Tok::Sym("[") => {
                let start = self.toks[self.pos].start;
                while !self.is_sym("]") && !matches!(self.tok(), Tok::Eof) {
                    self.bump();
                }
                let end = self.toks[self.pos].end;
                self.expect_sym("]")?;
                return Ok(self.src[start..end].to_string());
            }
            _ => return Err(self.encountered("option value")),
        };
        self.bump();
        Ok(if neg { format!("-{v}") } else { v })
    }

    // ---- statements ----

    pub(crate) fn statement(&mut self) -> R<Statement> {
        if self.eat_kw("CREATE") {
            self.create()
        } else if self.eat_kw("ALTER") {
            self.alter()
        } else if self.eat_kw("DROP") {
            self.drop()
        } else if self.eat_kw("RENAME") {
            self.rename()
        } else if self.is_kw("GRANT") || self.is_kw("REVOKE") {
            let kind = self.found().to_ascii_uppercase();
            let ddl = self.rest_text();
            Ok(opaque(&kind, "", ddl, false))
        } else if self.is_kw("ANALYZE") {
            self.bump();
            Ok(opaque("ANALYZE", "", "ANALYZE".into(), false))
        } else {
            Err(self.encountered("ddl statement"))
        }
    }

    fn if_not_exists(&mut self) -> bool {
        self.eat_kws(&["IF", "NOT", "EXISTS"])
    }

    fn if_exists(&mut self) -> bool {
        self.eat_kws(&["IF", "EXISTS"])
    }

    fn create(&mut self) -> R<Statement> {
        let head = self.toks[self.pos - 1].start;
        let or_replace = self.eat_kws(&["OR", "REPLACE"]);
        if self.eat_kw("SCHEMA") {
            let if_not_exists = self.if_not_exists();
            let name = self.ident()?;
            return Ok(Statement::CreateSchema {
                name,
                if_not_exists,
            });
        }
        if self.eat_kw("TABLE") {
            return self.create_table();
        }
        if self.is_kw("UNIQUE") || self.is_kw("NULL_FILTERED") || self.is_kw("INDEX") {
            let unique = self.eat_kw("UNIQUE");
            let null_filtered = self.eat_kw("NULL_FILTERED");
            self.expect_kw("INDEX")?;
            return self.create_index(unique, null_filtered);
        }
        if self.eat_kws(&["SEARCH", "INDEX"]) {
            return self.create_search_index();
        }
        if self.eat_kw("SEQUENCE") {
            let if_not_exists = self.if_not_exists();
            let name = self.path()?;
            let options = if self.is_kw("OPTIONS") {
                self.options()?
            } else {
                Vec::new()
            };
            return Ok(Statement::CreateSequence {
                sequence: Sequence { name, options },
                if_not_exists,
            });
        }
        if self.eat_kw("VIEW") {
            let name = self.path()?;
            if !self.eat_kws(&["SQL", "SECURITY"]) {
                return Err(DdlError::Syntax(format!(
                    "View `{name}` is missing the SQL SECURITY clause."
                )));
            }
            let sql_security_invoker = if self.eat_kw("INVOKER") {
                true
            } else if self.eat_kw("DEFINER") {
                false
            } else {
                return Err(self.expecting("INVOKER"));
            };
            self.expect_kw("AS")?;
            let sql = self.rest_text();
            if sql.is_empty() {
                return Err(self.encountered("query"));
            }
            return Ok(Statement::CreateView {
                view: View {
                    name,
                    sql,
                    sql_security_invoker,
                },
                or_replace,
            });
        }
        if self.eat_kws(&["CHANGE", "STREAM"]) {
            let if_not_exists = self.if_not_exists();
            let name = self.path()?;
            let cs = self.change_stream_body(ChangeStream::new(name))?;
            let mut ddl = cs.to_ddl();
            if if_not_exists {
                // `apply` reads IF NOT EXISTS back from the text and stores the canonical form.
                ddl = ddl.replacen("CHANGE STREAM ", "CHANGE STREAM IF NOT EXISTS ", 1);
            }
            return Ok(opaque("CHANGE STREAM", &cs.name.clone(), ddl, false));
        }
        if self.eat_kw("ROLE") {
            let name = self.ident()?;
            return Ok(opaque(
                "ROLE",
                &name,
                format!("CREATE ROLE {}", quote_ident(&name)),
                false,
            ));
        }
        for (kws, kind) in [
            (&["PROPERTY", "GRAPH"][..], "PROPERTY GRAPH"),
            (&["MODEL"][..], "MODEL"),
        ] {
            if self.eat_kws(kws) {
                self.if_not_exists();
                let name = self.path()?;
                // Keep the source text; `apply` reads OR REPLACE / IF NOT EXISTS from it.
                let ddl = self.src[head..].trim().to_string();
                self.rest_text();
                return Ok(opaque(kind, &name, ddl, false));
            }
        }
        if let Some(e) = self.unsupported_kind("CREATE") {
            return Err(e);
        }
        if or_replace {
            return Err(self.expecting("VIEW"));
        }
        Err(self.encountered("create_statement"))
    }

    fn create_table(&mut self) -> R<Statement> {
        let if_not_exists = self.if_not_exists();
        let name = self.path()?;
        let mut table = Table {
            name,
            columns: Vec::new(),
            primary_key: Vec::new(),
            interleave: None,
            foreign_keys: Vec::new(),
            checks: Vec::new(),
            row_deletion_policy: None,
            synonym: None,
        };
        // A PRIMARY KEY given inside the parentheses, on a column or as an element.
        let mut inline_pk: Option<Vec<KeyPart>> = None;
        let mut pk_column = false;
        self.expect_sym("(")?;
        while !self.eat_sym(")") {
            if self.is_kw("PRIMARY") && self.kw_at(1, "KEY") {
                self.bump();
                self.bump();
                let key = self.key_parts()?;
                if inline_pk.is_some() {
                    return Err(self.pk_once());
                }
                inline_pk = Some(key);
            } else if self.is_kw("CONSTRAINT")
                || (self.is_kw("FOREIGN") && self.kw_at(1, "KEY"))
                || (self.is_kw("CHECK") && matches!(self.tok_at(1), Tok::Sym("(")))
            {
                match self.constraint()? {
                    Constraint::Fk(fk) => table.foreign_keys.push(fk),
                    Constraint::Check(c) => table.checks.push(c),
                }
            } else if self.is_kw("SYNONYM") && matches!(self.tok_at(1), Tok::Sym("(")) {
                self.bump();
                self.expect_sym("(")?;
                table.synonym = Some(self.path()?);
                self.expect_sym(")")?;
            } else {
                let col = self.column_def()?;
                if std::mem::take(&mut self.column_pk) {
                    if pk_column {
                        return Err(
                            self.statement_error("Multiple columns declared as PRIMARY KEY")
                        );
                    }
                    if inline_pk.is_some() {
                        return Err(self.pk_once());
                    }
                    pk_column = true;
                    inline_pk = Some(vec![KeyPart {
                        column: col.name.clone(),
                        order: SortOrder::Asc,
                    }]);
                }
                table.columns.push(col);
            }
            if !self.eat_sym(",") {
                self.expect_sym(")")?;
                break;
            }
        }
        if let Some(key) = inline_pk {
            if self.is_kw("PRIMARY") {
                return Err(self.pk_once());
            }
            table.primary_key = key;
        } else if matches!(self.tok(), Tok::Eof) {
            // No PRIMARY KEY: the emulator keys the table on a hidden identity column.
            table.columns.push(Column {
                name: ROWID_COLUMN.into(),
                ty: ColumnType::Int64,
                not_null: true,
                default: None,
                on_update: None,
                generated: Some(Generated::Identity {
                    options: Some("BIT_REVERSED_POSITIVE".into()),
                }),
                allow_commit_timestamp: false,
                hidden: true,
                vector_length: None,
            });
            table.primary_key = vec![KeyPart {
                column: ROWID_COLUMN.into(),
                order: SortOrder::Asc,
            }];
            return Ok(Statement::CreateTable {
                table,
                if_not_exists,
            });
        } else {
            self.expect_kw("PRIMARY")?;
            self.expect_kw("KEY")?;
            table.primary_key = self.key_parts()?;
        }
        while self.eat_sym(",") {
            if self.eat_kw("INTERLEAVE") {
                self.expect_kw("IN")?;
                let in_parent = self.eat_kw("PARENT");
                let parent = self.path()?;
                let on_delete = self.on_delete()?;
                if on_delete.is_some() && !in_parent {
                    // Rows of an INTERLEAVE IN table have no parent row to follow.
                    let t = &table.name;
                    return Err(DdlError::Syntax(if self.emulator {
                        format!("Cannot SET ON DELETE on an INTERLEAVE IN table {t}.")
                    } else {
                        format!(
                            "table {t} uses INTERLEAVE IN and ON DELETE, which is not allowed since there is no parent/child relationship between rows of the two tables. Did you mean to use INTERLEAVE IN PARENT?"
                        )
                    }));
                }
                table.interleave = Some(Interleave {
                    parent,
                    on_delete: on_delete.unwrap_or(OnDelete::NoAction),
                    in_parent,
                });
            } else if self.is_kw("ROW") {
                table.row_deletion_policy = Some(self.row_deletion_policy()?);
            } else {
                return Err(self.expecting("INTERLEAVE"));
            }
        }
        Ok(Statement::CreateTable {
            table,
            if_not_exists,
        })
    }

    fn on_delete(&mut self) -> R<Option<OnDelete>> {
        if !self.eat_kws(&["ON", "DELETE"]) {
            return Ok(None);
        }
        if self.eat_kw("CASCADE") {
            Ok(Some(OnDelete::Cascade))
        } else if self.eat_kws(&["NO", "ACTION"]) {
            Ok(Some(OnDelete::NoAction))
        } else {
            Err(self.expecting("CASCADE"))
        }
    }

    fn row_deletion_policy(&mut self) -> R<RowDeletionPolicy> {
        self.expect_kw("ROW")?;
        self.expect_kw("DELETION")?;
        self.expect_kw("POLICY")?;
        self.expect_sym("(")?;
        self.expect_kw("OLDER_THAN")?;
        self.expect_sym("(")?;
        let column = self.ident()?;
        self.expect_sym(",")?;
        self.expect_kw("INTERVAL")?;
        let days = self.uint()?;
        self.expect_kw("DAY")?;
        self.expect_sym(")")?;
        self.expect_sym(")")?;
        Ok(RowDeletionPolicy { column, days })
    }

    fn constraint(&mut self) -> R<Constraint> {
        let name = if self.eat_kw("CONSTRAINT") {
            Some(self.ident()?)
        } else {
            None
        };
        if self.eat_kw("CHECK") {
            let expr = self.paren_text()?;
            let generated_name = name.is_none();
            let name = name.unwrap_or_default();
            return Ok(Constraint::Check(CheckConstraint {
                name,
                expr,
                generated_name,
            }));
        }
        self.expect_kw("FOREIGN")?;
        self.expect_kw("KEY")?;
        let columns = self.ident_list1()?;
        self.expect_kw("REFERENCES")?;
        let ref_table = self.path()?;
        let ref_columns = self.ident_list1()?;
        let on_delete = self.on_delete()?;
        let enforced = if self.eat_kws(&["NOT", "ENFORCED"]) {
            false
        } else {
            self.eat_kw("ENFORCED");
            true
        };
        if on_delete.is_some() && !enforced {
            return Err(DdlError::Syntax(format!(
                "Error parsing Spanner DDL statement: {} : ON DELETE actions are not supported for NOT ENFORCED foreign keys.",
                self.src
            )));
        }
        let generated_name = name.is_none();
        let name = name.unwrap_or_default();
        Ok(Constraint::Fk(ForeignKey {
            name,
            columns,
            ref_table,
            ref_columns,
            on_delete: on_delete.clone().unwrap_or(OnDelete::NoAction),
            enforced,
            generated_name,
            on_delete_explicit: on_delete.is_some(),
        }))
    }

    fn column_def(&mut self) -> R<Column> {
        let name = self.ident()?;
        self.column_rest(name)
    }

    fn column_rest(&mut self, name: String) -> R<Column> {
        self.column_rest_in(name, "column_def")
    }

    /// `ctx` names the grammar rule in the emulator's syntax errors.
    fn column_rest_in(&mut self, name: String, ctx: &str) -> R<Column> {
        let (ty, vector_length) = self.column_type()?;
        let mut col = Column {
            name,
            ty,
            not_null: false,
            default: None,
            on_update: None,
            generated: None,
            allow_commit_timestamp: false,
            hidden: false,
            vector_length,
        };
        // ON UPDATE can't follow HIDDEN or OPTIONS.
        let mut options_seen = false;
        loop {
            if self.eat_kws(&["NOT", "NULL"]) {
                col.not_null = true;
            } else if self.eat_kw("DEFAULT") {
                col.default = Some(self.paren_text()?);
            } else if self.is_kw("ON")
                && self.kw_at(1, "UPDATE")
                && col.on_update.is_none()
                && col.generated.is_none()
                && !options_seen
            {
                // ON UPDATE goes with a DEFAULT, before or after it.
                let on = self.pos;
                self.bump();
                self.bump();
                col.on_update = Some(self.paren_text()?);
                if col.default.is_none() && !self.is_kw("DEFAULT") {
                    self.pos = on;
                    return Err(self.encountered(ctx));
                }
            } else if self.is_kw("AS") && col.default.is_some() {
                return Err(self.expecting(")"));
            } else if self.eat_kw("AS") {
                let expr = self.paren_text()?;
                let stored = self.eat_kw("STORED");
                col.generated = Some(Generated::Expr { expr, stored });
            } else if self.is_kw("PRIMARY") && self.kw_at(1, "KEY") {
                self.bump();
                self.bump();
                self.column_pk = true;
            } else if self.eat_kw("HIDDEN") {
                col.hidden = true;
                options_seen = true;
            } else if self.is_kw("OPTIONS") {
                options_seen = true;
                for (k, v) in self.options()? {
                    if k.eq_ignore_ascii_case("allow_commit_timestamp") {
                        col.allow_commit_timestamp = v == "true";
                    } else {
                        return Err(DdlError::Syntax(format!(
                            "Error parsing Spanner DDL statement: {} : Option: {k} is unknown.",
                            self.src
                        )));
                    }
                }
            } else if self.is_kw("GENERATED")
                && self.kw_at(1, "BY")
                && self.kw_at(2, "DEFAULT")
                && self.kw_at(3, "AS")
                && self.kw_at(4, "IDENTITY")
            {
                for _ in 0..5 {
                    self.bump();
                }
                let options = if self.is_sym("(") {
                    Some(self.paren_text()?)
                } else {
                    None
                };
                col.generated = Some(Generated::Identity { options });
            } else if self.is_kw("GENERATED") || self.is_kw("AUTO_INCREMENT") {
                return Err(self.unsupported("Identity column"));
            } else {
                return Ok(col);
            }
        }
    }

    fn column_type(&mut self) -> R<(ColumnType, Option<u64>)> {
        let ty = self.scalar_or_compound_type()?;
        let mut vector_length = None;
        if matches!(ty, ColumnType::Array(_)) && self.is_sym("(") && self.kw_at(1, "vector_length")
        {
            self.bump();
            self.bump();
            self.expect_sym("=>")?;
            vector_length = Some(self.uint()?);
            self.expect_sym(")")?;
        }
        Ok((ty, vector_length))
    }

    fn scalar_or_compound_type(&mut self) -> R<ColumnType> {
        let word = match self.tok() {
            Tok::Ident(w) => w.to_ascii_uppercase(),
            _ => return Err(self.encountered("type")),
        };
        self.bump();
        let ty = match word.as_str() {
            "BOOL" => ColumnType::Bool,
            "INT64" => ColumnType::Int64,
            "FLOAT32" => ColumnType::Float32,
            "FLOAT64" => ColumnType::Float64,
            "NUMERIC" => ColumnType::Numeric,
            "DATE" => ColumnType::Date,
            "TIMESTAMP" => ColumnType::Timestamp,
            "JSON" => ColumnType::Json,
            "INTERVAL" => ColumnType::Interval,
            "UUID" => ColumnType::Uuid,
            "TOKENLIST" => ColumnType::TokenList,
            "STRING" | "BYTES" => {
                self.expect_sym("(")?;
                let len = if self.eat_kw("MAX") {
                    None
                } else {
                    Some(self.uint()?)
                };
                self.expect_sym(")")?;
                if word == "STRING" {
                    ColumnType::String(len)
                } else {
                    ColumnType::Bytes(len)
                }
            }
            "ARRAY" => {
                self.expect_sym("<")?;
                let inner = self.scalar_or_compound_type()?;
                self.expect_gt()?;
                ColumnType::Array(Box::new(inner))
            }
            "STRUCT" => {
                self.expect_sym("<")?;
                let mut fields = Vec::new();
                if !self.is_sym(">") && !self.is_sym(">>") {
                    loop {
                        let name = if matches!(self.tok_at(1), Tok::Ident(_))
                            || matches!(self.tok_at(1), Tok::QuotedIdent(_))
                        {
                            Some(self.ident()?)
                        } else {
                            None
                        };
                        fields.push((name, self.scalar_or_compound_type()?));
                        if !self.eat_sym(",") {
                            break;
                        }
                    }
                }
                self.expect_gt()?;
                ColumnType::Struct(fields)
            }
            _ => {
                self.pos -= 1;
                return Err(self.encountered("type"));
            }
        };
        Ok(ty)
    }

    fn create_index(&mut self, unique: bool, null_filtered: bool) -> R<Statement> {
        let if_not_exists = self.if_not_exists();
        let name = self.path()?;
        self.expect_kw("ON")?;
        let table = self.path()?;
        // An expression key, `ON T((expr), ...)`: Spanner indexes it through a hidden generated
        // column, which spanbolt doesn't model.
        let mut depth = 0usize;
        for i in 0.. {
            match self.tok_at(i) {
                Tok::Sym("(")
                    if depth == 1 && matches!(self.tok_at(i - 1), Tok::Sym("(" | ",")) =>
                {
                    return Err(self.unimplemented("An index key expression"));
                }
                Tok::Sym("(") => depth += 1,
                Tok::Sym(")") if depth <= 1 => break,
                Tok::Sym(")") => depth -= 1,
                Tok::Eof => break,
                _ => {}
            }
        }
        let key = self.key_parts()?;
        let mut index = Index {
            name,
            table,
            key,
            storing: Vec::new(),
            unique,
            null_filtered,
            interleave_in: None,
            filter: None,
        };
        loop {
            if self.eat_kw("STORING") {
                index.storing = self.ident_list1()?;
            } else if self.eat_kw("WHERE") {
                let cols = self.null_filter()?;
                index.filter = Some(null_filter_sql(&cols));
            } else if self.is_sym(",") && self.kw_at(1, "INTERLEAVE") {
                self.bump();
                self.bump();
                self.expect_kw("IN")?;
                index.interleave_in = Some(self.path()?);
            } else {
                break;
            }
        }
        Ok(Statement::CreateIndex {
            index,
            if_not_exists,
        })
    }

    /// A filtered index's `WHERE col IS NOT NULL [AND col IS NOT NULL ...]`: the only form Spanner
    /// accepts. Returns the column names.
    fn null_filter(&mut self) -> R<Vec<String>> {
        let mut cols = Vec::new();
        loop {
            cols.push(self.ident()?);
            self.expect_kw("IS")?;
            self.expect_kw("NOT")?;
            self.expect_kw("NULL")?;
            if !self.eat_kw("AND") {
                return Ok(cols);
            }
        }
    }

    fn filter_text(&mut self) -> R<String> {
        self.text_until(|p| (p.is_sym(",") && p.kw_at(1, "INTERLEAVE")) || p.is_kw("OPTIONS"))
    }

    fn create_search_index(&mut self) -> R<Statement> {
        let name = self.path()?;
        self.expect_kw("ON")?;
        let table = self.path()?;
        let columns = self.ident_list()?;
        let mut index = SearchIndex {
            name,
            table,
            columns,
            storing: Vec::new(),
            partition_by: Vec::new(),
            order_by: Vec::new(),
            options: Vec::new(),
            filter: None,
            interleave_in: None,
        };
        if self.eat_kw("STORING") {
            index.storing = self.ident_list1()?;
        }
        if self.eat_kws(&["PARTITION", "BY"]) {
            loop {
                index.partition_by.push(self.ident()?);
                if !self.eat_sym(",") {
                    break;
                }
            }
        }
        if self.eat_kws(&["ORDER", "BY"]) {
            loop {
                index.order_by.push(self.key_part()?);
                if !self.eat_sym(",") {
                    break;
                }
            }
        }
        if self.eat_kw("WHERE") {
            index.filter = Some(self.filter_text()?);
        }
        if self.is_sym(",") && self.kw_at(1, "INTERLEAVE") {
            self.bump();
            self.bump();
            self.expect_kw("IN")?;
            index.interleave_in = Some(self.path()?);
        }
        if self.is_kw("OPTIONS") {
            index.options = self.options()?;
        }
        Ok(Statement::CreateSearchIndex { index })
    }

    fn change_stream_body(&mut self, mut cs: ChangeStream) -> R<ChangeStream> {
        if self.eat_kw("FOR") {
            cs.target = self.change_stream_target()?;
        }
        if self.is_kw("OPTIONS") {
            cs.options = self.options()?;
        }
        Ok(cs)
    }

    fn change_stream_target(&mut self) -> R<CsTarget> {
        if self.eat_kw("ALL") {
            return Ok(CsTarget::All);
        }
        let mut tables = Vec::new();
        loop {
            let t = self.path()?;
            let cols = if self.is_sym("(") {
                Some(self.ident_list()?)
            } else {
                None
            };
            tables.push((t, cols));
            if !self.eat_sym(",") {
                return Ok(CsTarget::Tables(tables));
            }
        }
    }

    fn alter(&mut self) -> R<Statement> {
        if self.eat_kw("TABLE") {
            let if_exists = self.if_exists();
            let table = self.path()?;
            let action = self.alter_table_action()?;
            return Ok(Statement::AlterTable {
                table,
                action,
                if_exists,
            });
        }
        // The emulator's grammar has no ALTER SEARCH INDEX.
        if self.is_kw("SEARCH") && self.kw_at(1, "INDEX") && !self.emulator {
            self.bump();
            self.bump();
            let name = self.path()?;
            let at = self.pos;
            let add = if self.eat_kw("ADD") {
                true
            } else if self.eat_kw("DROP") {
                false
            } else {
                return Err(self.encountered("alter_search_index_statement"));
            };
            let stored = self.eat_kw("STORED");
            if !self.eat_kw("COLUMN") {
                self.pos = at;
                return Err(self.encountered("alter_search_index_statement"));
            }
            let column = self.ident()?;
            let action = match (add, stored) {
                (true, false) => AlterSearchIndex::AddColumn(column),
                (false, false) => AlterSearchIndex::DropColumn(column),
                (true, true) => AlterSearchIndex::AddStoredColumn(column),
                (false, true) => AlterSearchIndex::DropStoredColumn(column),
            };
            return Ok(Statement::AlterSearchIndex { name, action });
        }
        if self.eat_kw("INDEX") {
            let name = self.path()?;
            let add = if self.eat_kw("ADD") {
                true
            } else if self.eat_kw("DROP") {
                false
            } else {
                return Err(self.expecting("ADD"));
            };
            self.expect_kw("STORED")?;
            self.expect_kw("COLUMN")?;
            let col = self.ident()?;
            let action = if add {
                AlterIndex::AddStoredColumn(col)
            } else {
                AlterIndex::DropStoredColumn(col)
            };
            return Ok(Statement::AlterIndex { name, action });
        }
        if self.eat_kw("DATABASE") {
            let name = self.ident()?;
            self.expect_kw("SET")?;
            let options = self.options()?;
            for (k, v) in &options {
                if v == "NULL" && k.eq_ignore_ascii_case("default_leader") {
                    return Err(DdlError::Syntax(format!(
                        "Error parsing Spanner DDL statement: {} : Option: {k} is null.",
                        self.src
                    )));
                }
            }
            return Ok(Statement::AlterDatabase { options, name });
        }
        if self.eat_kw("SEQUENCE") {
            let name = self.path()?;
            self.expect_kw("SET")?;
            let options = self.options()?;
            return Ok(Statement::AlterSequence { name, options });
        }
        if self.eat_kws(&["CHANGE", "STREAM"]) {
            let name = self.path()?;
            let ddl = self.src.trim().to_string();
            // Validate the shape now; apply re-parses it against the stored stream.
            if self.eat_kw("SET") {
                if self.eat_kw("FOR") {
                    self.change_stream_target()?;
                } else {
                    self.options()?;
                }
            } else {
                self.expect_kw("DROP")?;
                self.expect_kw("FOR")?;
                self.expect_kw("ALL")?;
            }
            return Ok(opaque("ALTER CHANGE STREAM", &name, ddl, false));
        }
        if let Some(e) = self.unsupported_kind("ALTER") {
            return Err(e);
        }
        Err(self.encountered("alter_statement"))
    }

    /// A column definition in ALTER TABLE, where PRIMARY KEY can't go.
    fn alter_column_def(&mut self, column: R<Column>) -> R<Column> {
        let column = column?;
        if std::mem::take(&mut self.column_pk) {
            return Err(self.statement_error("Unexpected PRIMARY KEY clause"));
        }
        Ok(column)
    }

    fn alter_table_action(&mut self) -> R<AlterTable> {
        if self.eat_kw("ADD") {
            // COLUMN is optional: anything that isn't another kind of ADD is a column.
            if self.eat_kw("COLUMN") {
                let if_not_exists = self.if_not_exists();
                let def = self.column_def();
                let column = self.alter_column_def(def)?;
                return Ok(AlterTable::AddColumn {
                    column,
                    if_not_exists,
                });
            }
            if self.is_kw("ROW") && self.kw_at(1, "DELETION") {
                return Ok(AlterTable::AddRowDeletionPolicy(
                    self.row_deletion_policy()?,
                ));
            }
            if self.eat_kw("SYNONYM") {
                return Ok(AlterTable::AddSynonym(self.path()?));
            }
            if self.is_kw("CONSTRAINT")
                || (self.is_kw("FOREIGN") && self.kw_at(1, "KEY"))
                || (self.is_kw("CHECK") && matches!(self.tok_at(1), Tok::Sym("(")))
            {
                return Ok(match self.constraint()? {
                    Constraint::Fk(fk) => AlterTable::AddForeignKey(fk),
                    Constraint::Check(c) => AlterTable::AddCheck(c),
                });
            }
            let def = self.column_def();
            let column = self.alter_column_def(def)?;
            return Ok(AlterTable::AddColumn {
                column,
                if_not_exists: false,
            });
        }
        if self.eat_kw("DROP") {
            if self.eat_kw("COLUMN") {
                return Ok(AlterTable::DropColumn {
                    name: self.ident()?,
                });
            }
            if self.eat_kw("CONSTRAINT") {
                return Ok(AlterTable::DropConstraint {
                    name: self.ident()?,
                });
            }
            if self.eat_kws(&["ROW", "DELETION", "POLICY"]) {
                return Ok(AlterTable::DropRowDeletionPolicy);
            }
            if self.eat_kw("SYNONYM") {
                return Ok(AlterTable::DropSynonym(self.path()?));
            }
            return Ok(AlterTable::DropColumn {
                name: self.ident()?,
            });
        }
        if self.eat_kw("REPLACE") {
            return Ok(AlterTable::ReplaceRowDeletionPolicy(
                self.row_deletion_policy()?,
            ));
        }
        if self.eat_kw("ALTER") {
            self.eat_kw("COLUMN");
            let name = self.ident()?;
            if self.eat_kw("SET") {
                if self.eat_kw("DEFAULT") {
                    let default = Some(self.paren_text()?);
                    return Ok(AlterTable::SetColumnDefault { name, default });
                }
                if self.eat_kws(&["ON", "UPDATE"]) {
                    let on_update = Some(self.paren_text()?);
                    return Ok(AlterTable::SetColumnOnUpdate { name, on_update });
                }
                let opts = self.options()?;
                let mut act = None;
                for (k, v) in opts {
                    if !k.eq_ignore_ascii_case("allow_commit_timestamp") {
                        return Err(DdlError::Syntax(format!(
                            "Error parsing Spanner DDL statement: {} : Option: {k} is unknown.",
                            self.src
                        )));
                    }
                    act = Some(v == "true");
                }
                return Ok(AlterTable::SetColumnOptions {
                    name,
                    allow_commit_timestamp: act,
                });
            }
            if self.eat_kws(&["DROP", "DEFAULT"]) {
                return Ok(AlterTable::SetColumnDefault {
                    name,
                    default: None,
                });
            }
            if self.eat_kws(&["DROP", "ON", "UPDATE"]) {
                return Ok(AlterTable::SetColumnOnUpdate {
                    name,
                    on_update: None,
                });
            }
            let def = self.column_rest_in(name, "column_def_alter_attrs");
            let column = self.alter_column_def(def)?;
            return Ok(AlterTable::AlterColumn { column });
        }
        if self.eat_kw("SET") {
            self.expect_kw("ON")?;
            self.expect_kw("DELETE")?;
            if self.eat_kw("CASCADE") {
                return Ok(AlterTable::SetOnDelete(OnDelete::Cascade));
            }
            self.expect_kw("NO")?;
            self.expect_kw("ACTION")?;
            return Ok(AlterTable::SetOnDelete(OnDelete::NoAction));
        }
        if self.eat_kw("RENAME") {
            self.expect_kw("TO")?;
            let to = self.path()?;
            if self.eat_sym(",") {
                self.expect_kw("ADD")?;
                self.expect_kw("SYNONYM")?;
                let synonym = self.path()?;
                return Ok(AlterTable::RenameWithSynonym { to, synonym });
            }
            return Ok(AlterTable::Rename { to });
        }
        Err(self.encountered("ALTER TABLE action"))
    }

    fn drop(&mut self) -> R<Statement> {
        if self.eat_kw("TABLE") {
            let if_exists = self.if_exists();
            return Ok(Statement::DropTable {
                name: self.path()?,
                if_exists,
            });
        }
        if self.eat_kw("INDEX") {
            let if_exists = self.if_exists();
            return Ok(Statement::DropIndex {
                name: self.path()?,
                if_exists,
            });
        }
        if self.eat_kws(&["SEARCH", "INDEX"]) {
            let if_exists = self.if_exists();
            return Ok(Statement::DropSearchIndex {
                name: self.path()?,
                if_exists,
            });
        }
        if self.eat_kw("SEQUENCE") {
            let if_exists = self.if_exists();
            return Ok(Statement::DropSequence {
                name: self.path()?,
                if_exists,
            });
        }
        if self.eat_kw("VIEW") {
            let if_exists = self.if_exists();
            return Ok(Statement::DropView {
                name: self.path()?,
                if_exists,
            });
        }
        if self.eat_kw("SCHEMA") {
            return Ok(Statement::DropSchema {
                name: self.ident()?,
            });
        }
        for (kws, kind) in [
            (&["CHANGE", "STREAM"][..], "CHANGE STREAM"),
            (&["ROLE"][..], "ROLE"),
            (&["PROPERTY", "GRAPH"][..], "PROPERTY GRAPH"),
            (&["MODEL"][..], "MODEL"),
        ] {
            if self.eat_kws(kws) {
                let if_exists = self.if_exists();
                let name = self.path()?;
                let ddl = if if_exists { "IF EXISTS" } else { "" };
                return Ok(opaque(kind, &name, ddl.to_string(), true));
            }
        }
        if let Some(e) = self.unsupported_kind("DROP") {
            return Err(e);
        }
        Err(self.encountered("drop_statement"))
    }

    fn rename(&mut self) -> R<Statement> {
        self.expect_kw("TABLE")?;
        let mut renames = Vec::new();
        loop {
            let from = self.path()?;
            self.expect_kw("TO")?;
            let to = self.path()?;
            renames.push((from, to));
            if !self.eat_sym(",") {
                break;
            }
        }
        if renames.len() == 1 {
            let (table, to) = renames.remove(0);
            return Ok(Statement::AlterTable {
                table,
                action: AlterTable::Rename { to },
                if_exists: false,
            });
        }
        Ok(Statement::RenameTables { renames })
    }
}

enum Constraint {
    Fk(ForeignKey),
    Check(CheckConstraint),
}

fn opaque(kind: &str, name: &str, ddl: String, drop: bool) -> Statement {
    Statement::Opaque {
        object: OpaqueObject {
            kind: kind.to_string(),
            name: name.to_string(),
            ddl,
        },
        drop,
    }
}

// ---- change streams (stored as opaque objects with canonical DDL) ----

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CsTarget {
    None,
    All,
    Tables(Vec<(String, Option<Vec<String>>)>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ChangeStream {
    pub name: String,
    pub target: CsTarget,
    pub options: Vec<(String, String)>,
}

impl ChangeStream {
    fn new(name: String) -> Self {
        ChangeStream {
            name,
            target: CsTarget::None,
            options: Vec::new(),
        }
    }

    pub(crate) fn to_ddl(&self) -> String {
        let mut s = format!("CREATE CHANGE STREAM {}", quote_path(&self.name));
        match &self.target {
            CsTarget::None => {}
            CsTarget::All => s.push_str(" FOR ALL"),
            CsTarget::Tables(ts) => {
                s.push_str(" FOR ");
                let parts: Vec<String> = ts
                    .iter()
                    .map(|(t, cols)| match cols {
                        None => quote_path(t),
                        Some(c) => format!("{}({})", quote_path(t), join_idents(c)),
                    })
                    .collect();
                s.push_str(&parts.join(", "));
            }
        }
        if !self.options.is_empty() {
            let opts: Vec<String> = self
                .options
                .iter()
                .map(|(k, v)| format!("{k} = {v}"))
                .collect();
            s.push_str(&format!(" OPTIONS ( {} )", opts.join(", ")));
        }
        s
    }

    /// Parses a canonical `CREATE CHANGE STREAM` statement.
    pub(crate) fn parse(ddl: &str) -> R<ChangeStream> {
        let mut p = Parser::new(ddl)?;
        p.expect_kw("CREATE")?;
        p.expect_kw("CHANGE")?;
        p.expect_kw("STREAM")?;
        let name = p.path()?;
        p.change_stream_body(ChangeStream::new(name))
    }

    /// Applies an `ALTER CHANGE STREAM name ...` statement.
    pub(crate) fn alter(&mut self, ddl: &str) -> R<()> {
        let mut p = Parser::new(ddl)?;
        p.expect_kw("ALTER")?;
        p.expect_kw("CHANGE")?;
        p.expect_kw("STREAM")?;
        p.path()?;
        if p.eat_kw("SET") {
            if p.eat_kw("FOR") {
                self.target = p.change_stream_target()?;
            } else {
                // The emulator replaces the whole option set.
                self.options = p.options()?;
                self.options.retain(|(_, v)| v != "NULL");
            }
        } else {
            self.target = CsTarget::None;
        }
        Ok(())
    }
}

// ---- quoting helpers ----

pub fn quote_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        match c {
            '\'' => out.push_str("\\'"),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('\'');
    out
}

fn is_plain_ident(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// How GetDatabaseDdl prints a filtered index's WHERE clause (without `WHERE`).
pub(crate) fn null_filter_sql(cols: &[String]) -> String {
    cols.iter()
        .map(|c| format!("{} IS NOT NULL", quote_ident(c)))
        .collect::<Vec<_>>()
        .join(" AND ")
}

/// The columns of a filtered index's WHERE clause as `null_filter_sql` printed it.
pub fn null_filter_columns(filter: &str) -> R<Vec<String>> {
    let mut p = Parser::new(filter)?;
    let cols = p.null_filter()?;
    p.expect_eof()?;
    Ok(cols)
}

/// Backquotes an identifier when it is reserved or not a plain identifier.
pub fn quote_ident(s: &str) -> String {
    if is_plain_ident(s) && !is_reserved(s) {
        s.to_string()
    } else {
        format!("`{}`", s.replace('\\', "\\\\").replace('`', "\\`"))
    }
}

/// Quotes each part of a dotted name.
pub fn quote_path(s: &str) -> String {
    s.split('.').map(quote_ident).collect::<Vec<_>>().join(".")
}

pub(crate) fn join_idents(cols: &[String]) -> String {
    cols.iter()
        .map(|c| quote_ident(c))
        .collect::<Vec<_>>()
        .join(", ")
}
