//! What a step produced, how it is written as an expectation, and the comparison rules.

use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value as Json;
use spanbolt_proto::spanner::{Type, TypeCode};

#[derive(Debug, Clone)]
pub enum MessageRule {
    Exact(String),
    Ignore,
    Regex(Regex),
}

/// Expected (parsed from `expect:`) or actual (observed) outcome of a step. All fields empty = ok.
#[derive(Debug, Clone, Default)]
pub struct Observation {
    /// gRPC code name, e.g. `INVALID_ARGUMENT`.
    pub error: Option<String>,
    pub message: Option<MessageRule>,
    pub columns: Option<String>,
    pub rows: Vec<Json>,
    pub count: Option<i64>,
    pub counts: Option<Vec<i64>>,
    pub statements: Option<Vec<String>>,
    /// Column types of `rows` (actual observations only), for placeholder detection.
    pub row_types: Vec<Type>,
}

pub const NOW: &str = "<now>";
pub const ANY: &str = "<any>";

impl Observation {
    pub fn from_status(s: &tonic::Status) -> Self {
        Self::error(code_name(s.code()), s.message())
    }

    pub fn error(code: &str, message: &str) -> Self {
        Self {
            error: Some(code.to_string()),
            message: Some(MessageRule::Exact(normalise_message(message))),
            ..Default::default()
        }
    }

    pub fn is_ok(&self) -> bool {
        self.error.is_none()
            && self.columns.is_none()
            && self.rows.is_empty()
            && self.count.is_none()
            && self.counts.is_none()
            && self.statements.is_none()
    }

    pub fn error_code(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// The `expect:` body.
    pub fn to_lines(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(c) = &self.counts {
            out.push(format!(
                "counts: {}",
                serde_json::to_string(c).unwrap_or_default()
            ));
        }
        if let Some(e) = &self.error {
            out.push(format!("error: {e}"));
            match &self.message {
                Some(MessageRule::Exact(m)) => out.push(format!("message: {m}")),
                Some(MessageRule::Ignore) => out.push("message: ignore".into()),
                Some(MessageRule::Regex(r)) => out.push(format!("message~: {}", r.as_str())),
                None => {}
            }
        }
        if let Some(c) = self.count {
            out.push(format!("count: {c}"));
        }
        if let Some(c) = &self.columns {
            out.push(format!("columns: {c}"));
        }
        out.extend(self.rows.iter().map(Json::to_string));
        if let Some(st) = &self.statements {
            out.push("statements:".into());
            out.extend(st.iter().map(|s| Json::String(s.clone()).to_string()));
        }
        if out.is_empty() {
            out.push("ok".into());
        }
        out
    }

    /// Replaces volatile values (timestamps/dates near `now_secs`) with `<now>` for recording.
    pub fn with_placeholders(mut self, now_secs: i64) -> Self {
        let types = std::mem::take(&mut self.row_types);
        for row in &mut self.rows {
            if let Json::Array(vals) = row {
                for (v, t) in vals.iter_mut().zip(&types) {
                    placeholder(v, t, now_secs);
                }
            }
        }
        self
    }

    /// For `record`: keeps hand-written rules from the previous expectation where they still apply.
    pub fn merge_manual(mut self, old: Option<&Observation>) -> Self {
        let Some(old) = old else { return self };
        if old.error.is_some()
            && old.error == self.error
            && let Some(rule @ (MessageRule::Ignore | MessageRule::Regex(_))) = &old.message
        {
            let keep = match (rule, &self.message) {
                (MessageRule::Regex(r), Some(MessageRule::Exact(m))) => r.is_match(m),
                _ => true,
            };
            if keep {
                self.message = Some(rule.clone());
            }
        }
        if old.rows.len() == self.rows.len() {
            for (new_row, old_row) in self.rows.iter_mut().zip(&old.rows) {
                keep_placeholders(new_row, old_row);
            }
        }
        self
    }
}

fn keep_placeholders(new: &mut Json, old: &Json) {
    match (new, old) {
        (n, Json::String(s)) if s == ANY || s == NOW => *n = Json::String(s.clone()),
        (Json::Array(n), Json::Array(o)) if n.len() == o.len() => {
            for (a, b) in n.iter_mut().zip(o) {
                keep_placeholders(a, b);
            }
        }
        _ => {}
    }
}

fn placeholder(v: &mut Json, t: &Type, now: i64) {
    let code = TypeCode::try_from(t.code).unwrap_or(TypeCode::Unspecified);
    match (code, v) {
        (TypeCode::Timestamp, v @ Json::String(_)) => {
            if let Some(secs) = v.as_str().and_then(crate::value::parse_timestamp_secs)
                && (secs - now).abs() < 600
            {
                *v = Json::String(NOW.into());
            }
        }
        (TypeCode::Date, v @ Json::String(_)) => {
            if let Some(days) = v.as_str().and_then(crate::value::parse_date_days)
                && (days - now.div_euclid(86_400)).abs() <= 1
            {
                *v = Json::String(NOW.into());
            }
        }
        (TypeCode::Array, Json::Array(items)) => {
            if let Some(e) = &t.array_element_type {
                for i in items {
                    placeholder(i, e, now);
                }
            }
        }
        (TypeCode::Struct, Json::Array(items)) => {
            if let Some(st) = &t.struct_type {
                for (i, f) in items.iter_mut().zip(&st.fields) {
                    if let Some(ft) = &f.r#type {
                        placeholder(i, ft, now);
                    }
                }
            }
        }
        _ => {}
    }
}

pub fn code_name(c: tonic::Code) -> &'static str {
    use tonic::Code::*;
    match c {
        Ok => "OK",
        Cancelled => "CANCELLED",
        Unknown => "UNKNOWN",
        InvalidArgument => "INVALID_ARGUMENT",
        DeadlineExceeded => "DEADLINE_EXCEEDED",
        NotFound => "NOT_FOUND",
        AlreadyExists => "ALREADY_EXISTS",
        PermissionDenied => "PERMISSION_DENIED",
        ResourceExhausted => "RESOURCE_EXHAUSTED",
        FailedPrecondition => "FAILED_PRECONDITION",
        Aborted => "ABORTED",
        OutOfRange => "OUT_OF_RANGE",
        Unimplemented => "UNIMPLEMENTED",
        Internal => "INTERNAL",
        Unavailable => "UNAVAILABLE",
        DataLoss => "DATA_LOSS",
        Unauthenticated => "UNAUTHENTICATED",
    }
}

/// The emulator prefixes protobuf debug strings it embeds in messages with this marker.
static DEBUG_ONLY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"goo\.gle/debug\w*").unwrap_or_else(|e| panic!("{e}")));
/// The emulator echoes the statement and a caret after the `[at L:C]` position.
static STATEMENT_ECHO: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)(\[at \d+:\d+\]).*$").unwrap_or_else(|e| panic!("{e}")));
static WS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").unwrap_or_else(|e| panic!("{e}")));

/// Collapses whitespace and strips known emulator noise from an error message.
pub fn normalise_message(m: &str) -> String {
    let m = DEBUG_ONLY.replace_all(m, "");
    let m = STATEMENT_ECHO.replace(&m, "$1");
    WS.replace_all(m.trim(), " ").into_owned()
}

/// True if the statement has an `ORDER BY` outside any parentheses, strings and comments.
pub fn has_top_level_order_by(sql: &str) -> bool {
    let b = sql.as_bytes();
    let mut i = 0;
    let mut depth = 0i32;
    let mut words: Vec<String> = Vec::new();
    while i < b.len() {
        let c = b[i];
        match c {
            b'\'' | b'"' | b'`' => {
                let triple = b.len() >= i + 3 && b[i + 1] == c && b[i + 2] == c;
                let q = if triple { 3 } else { 1 };
                i += q;
                while i < b.len() {
                    if b[i] == b'\\' {
                        i += 2;
                        continue;
                    }
                    if b[i] == c
                        && (!triple || (b.len() >= i + 3 && b[i + 1] == c && b[i + 2] == c))
                    {
                        i += q;
                        break;
                    }
                    i += 1;
                }
                words.clear();
                continue;
            }
            b'-' if b.get(i + 1) == Some(&b'-') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            b'#' => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                    i += 1;
                }
                i += 2;
                continue;
            }
            b'(' => {
                depth += 1;
                words.clear();
            }
            b')' => {
                depth -= 1;
                words.clear();
            }
            c if c.is_ascii_alphanumeric() || c == b'_' => {
                let start = i;
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                    i += 1;
                }
                if depth == 0 {
                    let w = sql[start..i].to_ascii_uppercase();
                    if w == "BY" && words.last().is_some_and(|p| p == "ORDER") {
                        return true;
                    }
                    words.push(w);
                }
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    false
}

/// Value equality under the conformance rules.
pub fn values_match(expected: &Json, actual: &Json) -> bool {
    match (expected, actual) {
        (Json::String(e), _) if e == ANY => true,
        (Json::String(e), Json::String(_)) if e == NOW => true,
        (Json::Number(e), Json::Number(a)) => match (e.as_f64(), a.as_f64()) {
            (Some(x), Some(y)) => x.to_bits() == y.to_bits() || (x.is_nan() && y.is_nan()),
            _ => e == a,
        },
        (Json::Array(e), Json::Array(a)) => {
            e.len() == a.len() && e.iter().zip(a).all(|(x, y)| values_match(x, y))
        }
        (e, a) => e == a,
    }
}

/// Compares an observation with its expectation; `Err` carries a readable diff.
pub fn compare(expected: &Observation, actual: &Observation, ordered: bool) -> Result<(), String> {
    let mut diffs = Vec::new();
    if expected.error != actual.error {
        diffs.push(format!(
            "status: expected {}, got {}",
            expected.error.as_deref().unwrap_or("OK"),
            actual.error.as_deref().unwrap_or("OK"),
        ));
    } else if expected.error.is_some() {
        let got = match &actual.message {
            Some(MessageRule::Exact(m)) => m.as_str(),
            _ => "",
        };
        let ok = match &expected.message {
            None | Some(MessageRule::Ignore) => true,
            Some(MessageRule::Exact(m)) => m == got,
            Some(MessageRule::Regex(r)) => r.is_match(got),
        };
        if !ok {
            let want = match &expected.message {
                Some(MessageRule::Exact(m)) => m.clone(),
                Some(MessageRule::Regex(r)) => format!("~ {}", r.as_str()),
                _ => String::new(),
            };
            diffs.push(format!("message:\n  - {want}\n  + {got}"));
        }
    }
    if expected.count != actual.count {
        diffs.push(format!(
            "count: expected {:?}, got {:?}",
            expected.count, actual.count
        ));
    }
    if expected.counts != actual.counts {
        diffs.push(format!(
            "counts: expected {:?}, got {:?}",
            expected.counts, actual.counts
        ));
    }
    if expected.columns != actual.columns {
        diffs.push(format!(
            "columns:\n  - {}\n  + {}",
            expected.columns.as_deref().unwrap_or("(none)"),
            actual.columns.as_deref().unwrap_or("(none)")
        ));
    }
    if expected.statements != actual.statements {
        let fmt = |s: &Option<Vec<String>>| {
            s.as_ref()
                .map(|v| v.join("\n    "))
                .unwrap_or_else(|| "(none)".into())
        };
        diffs.push(format!(
            "statements:\n  - {}\n  + {}",
            fmt(&expected.statements),
            fmt(&actual.statements)
        ));
    }
    if let Some(d) = rows_diff(&expected.rows, &actual.rows, ordered) {
        diffs.push(d);
    }
    if diffs.is_empty() {
        Ok(())
    } else {
        Err(diffs.join("\n"))
    }
}

fn rows_diff(expected: &[Json], actual: &[Json], ordered: bool) -> Option<String> {
    let mut missing = Vec::new();
    let mut extra = Vec::new();
    if ordered {
        let same = expected.len() == actual.len()
            && expected.iter().zip(actual).all(|(e, a)| values_match(e, a));
        if same {
            return None;
        }
        let mut out = vec![format!(
            "rows (ordered): expected {}, got {}",
            expected.len(),
            actual.len()
        )];
        for i in 0..expected.len().max(actual.len()) {
            match (expected.get(i), actual.get(i)) {
                (Some(e), Some(a)) if values_match(e, a) => out.push(format!("    {e}")),
                (e, a) => {
                    if let Some(e) = e {
                        out.push(format!("  - {e}"));
                    }
                    if let Some(a) = a {
                        out.push(format!("  + {a}"));
                    }
                }
            }
        }
        return Some(out.join("\n"));
    }
    let mut used = vec![false; actual.len()];
    for e in expected {
        match (0..actual.len()).find(|&i| !used[i] && values_match(e, &actual[i])) {
            Some(i) => used[i] = true,
            None => missing.push(e),
        }
    }
    for (i, a) in actual.iter().enumerate() {
        if !used[i] {
            extra.push(a);
        }
    }
    if missing.is_empty() && extra.is_empty() {
        return None;
    }
    let mut out = vec![format!(
        "rows (unordered): expected {}, got {}",
        expected.len(),
        actual.len()
    )];
    out.extend(missing.iter().map(|e| format!("  - {e}")));
    out.extend(extra.iter().map(|a| format!("  + {a}")));
    Some(out.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn j(s: &str) -> Json {
        serde_json::from_str(s).unwrap_or_else(|e| panic!("{e}"))
    }

    fn rows(rs: &[&str]) -> Observation {
        Observation {
            columns: Some("K INT64".into()),
            rows: rs.iter().map(|r| j(r)).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn unordered_rows_compare_as_multisets() {
        let e = rows(&[r#"["1"]"#, r#"["2"]"#, r#"["2"]"#]);
        let a = rows(&[r#"["2"]"#, r#"["1"]"#, r#"["2"]"#]);
        assert!(compare(&e, &a, false).is_ok());
        assert!(compare(&e, &a, true).is_err());
        let a2 = rows(&[r#"["2"]"#, r#"["1"]"#, r#"["1"]"#]);
        let d = compare(&e, &a2, false).err().unwrap_or_default();
        assert!(d.contains(r#"- ["2"]"#) && d.contains(r#"+ ["1"]"#), "{d}");
    }

    #[test]
    fn floats_compare_by_bits() {
        assert!(values_match(&j("0.1"), &j("0.1")));
        assert!(!values_match(&j("0.0"), &j("-0.0")));
        assert!(!values_match(&j("0.30000000000000004"), &j("0.3")));
        // Integral floats keep their float form through the wire mapping.
        assert!(values_match(&j("1.0"), &j("1.0")));
        assert!(values_match(&j(r#""NaN""#), &j(r#""NaN""#)));
        let nan = Json::String("<number NaN>".into());
        assert!(values_match(&nan, &nan));
    }

    #[test]
    fn placeholders() {
        assert!(values_match(
            &j(r#""<now>""#),
            &j(r#""2026-01-01T00:00:00Z""#)
        ));
        assert!(!values_match(&j(r#""<now>""#), &j("null")));
        assert!(values_match(&j(r#""<any>""#), &j("null")));
        let ts = Type {
            code: TypeCode::Timestamp as i32,
            ..Default::default()
        };
        let obs = Observation {
            rows: vec![j(r#"["1970-01-01T00:01:40Z", "2000-01-01T00:00:00Z"]"#)],
            row_types: vec![ts.clone(), ts],
            ..Default::default()
        }
        .with_placeholders(100);
        assert_eq!(obs.rows[0], j(r#"["<now>", "2000-01-01T00:00:00Z"]"#));
    }

    #[test]
    fn messages_are_normalised() {
        assert_eq!(
            normalise_message("Table not found:   Foo\n  [at 1:15]"),
            "Table not found: Foo [at 1:15]"
        );
        assert_eq!(
            normalise_message(
                "Table not found: Nope [at 1:15]\nSELECT * FROM Nope\n              ^"
            ),
            "Table not found: Nope [at 1:15]"
        );
        assert_eq!(
            normalise_message("Could not parse goo.gle/debugonly    \nbool_value: true\n as INT64"),
            "Could not parse bool_value: true as INT64"
        );
        assert_eq!(
            normalise_message("Could not parse goo.gle/debugstr number_value: 1.5 as NUMERIC"),
            "Could not parse number_value: 1.5 as NUMERIC"
        );
        let e = Observation {
            error: Some("NOT_FOUND".into()),
            message: Some(MessageRule::Ignore),
            ..Default::default()
        };
        assert!(compare(&e, &Observation::error("NOT_FOUND", "whatever"), false).is_ok());
        assert!(compare(&e, &Observation::error("INVALID_ARGUMENT", "x"), false).is_err());
        let re = Observation {
            error: Some("NOT_FOUND".into()),
            message: Regex::new("^Table").ok().map(MessageRule::Regex),
            ..Default::default()
        };
        assert!(compare(&re, &Observation::error("NOT_FOUND", "Table x"), false).is_ok());
        assert!(compare(&re, &Observation::error("NOT_FOUND", "Column x"), false).is_err());
    }

    #[test]
    fn order_by_detection() {
        assert!(has_top_level_order_by("SELECT a FROM t ORDER BY a"));
        assert!(has_top_level_order_by("select a from t\norder   by a desc"));
        assert!(!has_top_level_order_by(
            "SELECT ARRAY_AGG(a ORDER BY a) FROM t"
        ));
        assert!(!has_top_level_order_by(
            "SELECT a FROM (SELECT a FROM t ORDER BY a)"
        ));
        assert!(!has_top_level_order_by("SELECT 'ORDER BY' FROM t"));
        assert!(!has_top_level_order_by("SELECT a FROM t -- ORDER BY a"));
        assert!(has_top_level_order_by(
            "WITH x AS (SELECT 1 AS a) SELECT a FROM x ORDER BY a"
        ));
    }

    #[test]
    fn merge_keeps_manual_rules() {
        let old = Observation {
            error: Some("NOT_FOUND".into()),
            message: Some(MessageRule::Ignore),
            ..Default::default()
        };
        let new = Observation::error("NOT_FOUND", "Table not found: X").merge_manual(Some(&old));
        assert!(matches!(new.message, Some(MessageRule::Ignore)));
        let new = Observation::error("ABORTED", "x").merge_manual(Some(&old));
        assert!(matches!(new.message, Some(MessageRule::Exact(_))));
        let old_rows = rows(&[r#"["<any>", "1"]"#]);
        let new_rows = rows(&[r#"["abc", "2"]"#]).merge_manual(Some(&old_rows));
        assert_eq!(new_rows.rows[0], j(r#"["<any>", "2"]"#));
    }

    #[test]
    fn expectation_lines() {
        let o = Observation {
            count: Some(2),
            columns: Some("K INT64".into()),
            rows: vec![j(r#"["1"]"#)],
            ..Default::default()
        };
        assert_eq!(
            o.to_lines(),
            vec!["count: 2", "columns: K INT64", r#"["1"]"#]
        );
        assert_eq!(Observation::default().to_lines(), vec!["ok"]);
    }
}
