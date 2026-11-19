//! Judging runs against expectations, and console / JUnit / JSON reports.

use std::collections::BTreeMap;

use serde_json::json;

use crate::case::{Case, Oracle, Verb};
use crate::observe::{self, Observation};
use crate::runner::CaseRun;

#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    Pass,
    Fail(String),
    Unimplemented(String),
    Error(String),
    Skipped(String),
}

impl Status {
    pub fn label(&self) -> &'static str {
        match self {
            Status::Pass => "pass",
            Status::Fail(_) => "fail",
            Status::Unimplemented(_) => "unimplemented",
            Status::Error(_) => "error",
            Status::Skipped(_) => "skipped",
        }
    }
    pub fn detail(&self) -> Option<&str> {
        match self {
            Status::Pass => None,
            Status::Fail(d) | Status::Unimplemented(d) | Status::Error(d) | Status::Skipped(d) => {
                Some(d)
            }
        }
    }
}

pub fn ordered(step: &crate::case::Step) -> bool {
    if step.has_flag("ordered") || step.verb == Verb::Read {
        return true;
    }
    if step.has_flag("unordered") {
        return false;
    }
    observe::has_top_level_order_by(&step.sql())
}

fn describe(o: &Observation) -> String {
    let msg = match &o.message {
        Some(observe::MessageRule::Exact(m)) => m.as_str(),
        _ => "",
    };
    format!("{}: {msg}", o.error.as_deref().unwrap_or("OK"))
}

/// Judges one run against the case's expectations.
pub fn judge(case: &Case, run: &CaseRun, oracle: Oracle) -> Status {
    if let Err(e) = &run.setup {
        return match e.code {
            None => Status::Error(format!("setup: {}", e.message)),
            Some(tonic::Code::Unimplemented) => {
                Status::Unimplemented(format!("setup: {}", e.message))
            }
            Some(_) => Status::Fail(format!("setup: {}", e.message)),
        };
    }
    let mut diffs = Vec::new();
    for (step, result) in case.steps.iter().zip(&run.steps) {
        let at = format!("line {}", step.line + 1);
        let actual = match result {
            Ok(o) => o,
            Err(e) => return Status::Error(format!("{at}: {e}")),
        };
        let expected_unimpl =
            step.expectation(oracle).and_then(|e| e.error.as_deref()) == Some("UNIMPLEMENTED");
        if actual.error.as_deref() == Some("UNIMPLEMENTED") && !expected_unimpl {
            return Status::Unimplemented(format!("{at}: {}", describe(actual)));
        }
        match step.expectation(oracle) {
            None => {
                if actual.error.is_some() {
                    diffs.push(format!("{at}: expected success, got {}", describe(actual)));
                }
            }
            Some(exp) => {
                if let Err(d) = observe::compare(exp, actual, ordered(step)) {
                    diffs.push(format!("{at}:\n{}", indent(&d)));
                }
            }
        }
    }
    if diffs.is_empty() {
        Status::Pass
    } else {
        Status::Fail(diffs.join("\n"))
    }
}

fn indent(s: &str) -> String {
    s.lines()
        .map(|l| format!("  {l}"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[derive(Default, Clone, Copy)]
pub struct Counts {
    pub total: usize,
    pub pass: usize,
    pub fail: usize,
    pub unimplemented: usize,
    pub error: usize,
    pub skipped: usize,
}

impl Counts {
    fn add(&mut self, s: &Status) {
        self.total += 1;
        match s {
            Status::Pass => self.pass += 1,
            Status::Fail(_) => self.fail += 1,
            Status::Unimplemented(_) => self.unimplemented += 1,
            Status::Error(_) => self.error += 1,
            Status::Skipped(_) => self.skipped += 1,
        }
    }
    fn json(&self) -> serde_json::Value {
        json!({
            "total": self.total, "pass": self.pass, "fail": self.fail,
            "unimplemented": self.unimplemented, "error": self.error, "skipped": self.skipped,
        })
    }
}

pub struct Report<'a> {
    pub results: Vec<(&'a Case, Status)>,
}

impl Report<'_> {
    pub fn totals(&self) -> Counts {
        let mut c = Counts::default();
        for (_, s) in &self.results {
            c.add(s);
        }
        c
    }

    pub fn by_tag(&self) -> BTreeMap<String, Counts> {
        let mut m: BTreeMap<String, Counts> = BTreeMap::new();
        for (case, s) in &self.results {
            for t in &case.tags {
                m.entry(t.clone()).or_default().add(s);
            }
        }
        m
    }

    pub fn print(&self, verbose: bool) {
        let mut shown = 0;
        for (case, s) in &self.results {
            let show = matches!(s, Status::Fail(_) | Status::Error(_))
                || (verbose && matches!(s, Status::Unimplemented(_)));
            if !show {
                continue;
            }
            shown += 1;
            if !verbose && shown > 40 {
                continue;
            }
            println!(
                "{} {}\n{}\n",
                s.label().to_uppercase(),
                case.name,
                indent(s.detail().unwrap_or_default())
            );
        }
        if !verbose && shown > 40 {
            println!("... {} more failing cases (use -v)\n", shown - 40);
        }
        println!(
            "{:<24} {:>6} {:>6} {:>6} {:>8} {:>6} {:>6}",
            "tag", "total", "pass", "fail", "unimpl", "error", "skip"
        );
        let row = |name: &str, c: &Counts| {
            println!(
                "{name:<24} {:>6} {:>6} {:>6} {:>8} {:>6} {:>6}",
                c.total, c.pass, c.fail, c.unimplemented, c.error, c.skipped
            )
        };
        for (tag, c) in self.by_tag() {
            row(&tag, &c);
        }
        row("TOTAL", &self.totals());
    }

    pub fn json(&self, target: &str, unimplemented_fails: bool) -> serde_json::Value {
        let cases: Vec<_> = self
            .results
            .iter()
            .map(|(c, s)| json!({"name": c.name, "tags": c.tags, "status": s.label(), "detail": s.detail()}))
            .collect();
        let tags: serde_json::Map<String, serde_json::Value> = self
            .by_tag()
            .into_iter()
            .map(|(k, v)| (k, v.json()))
            .collect();
        json!({
            "target": target,
            "unimplemented": if unimplemented_fails { "fail" } else { "skip" },
            "totals": self.totals().json(),
            "by_tag": tags,
            "cases": cases,
        })
    }

    /// JUnit XML; `unimplemented_fails` reports UNIMPLEMENTED as failures instead of skips.
    pub fn junit(&self, unimplemented_fails: bool) -> String {
        let t = self.totals();
        let failures = t.fail
            + if unimplemented_fails {
                t.unimplemented
            } else {
                0
            };
        let skipped = t.skipped
            + if unimplemented_fails {
                0
            } else {
                t.unimplemented
            };
        let mut out = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<testsuites tests=\"{}\" failures=\"{failures}\" errors=\"{}\" skipped=\"{skipped}\">\n<testsuite name=\"spanbolt-conformance\" tests=\"{}\" failures=\"{failures}\" errors=\"{}\" skipped=\"{skipped}\">\n",
            t.total, t.error, t.total, t.error
        );
        for (case, s) in &self.results {
            let (class, name) = case.name.rsplit_once('/').unwrap_or(("", &case.name));
            out.push_str(&format!(
                "  <testcase classname=\"{}\" name=\"{}\"",
                xml_attr(&class.replace('/', ".")),
                xml_attr(name)
            ));
            let child = match s {
                Status::Pass => None,
                Status::Fail(d) => Some(("failure", "mismatch", d)),
                Status::Unimplemented(d) if unimplemented_fails => {
                    Some(("failure", "UNIMPLEMENTED", d))
                }
                Status::Unimplemented(d) => Some(("skipped", "UNIMPLEMENTED", d)),
                Status::Error(d) => Some(("error", "harness error", d)),
                Status::Skipped(d) => Some(("skipped", "skipped", d)),
            };
            match child {
                None => out.push_str("/>\n"),
                Some((tag, msg, detail)) => out.push_str(&format!(
                    ">\n    <{tag} message=\"{}\"><![CDATA[{}]]></{tag}>\n  </testcase>\n",
                    xml_attr(msg),
                    detail.replace("]]>", "]]]]><![CDATA[>")
                )),
            }
        }
        out.push_str("</testsuite>\n</testsuites>\n");
        out
    }
}

fn xml_attr(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::{CaseRun, SetupError};

    const FILE: &str = "case q
query:
  SELECT K FROM T ORDER BY K
expect:
  columns: K INT64
  [\"1\"]
  [\"2\"]
read T cols=K:
  all
expect:
  error: NOT_FOUND
  message: ignore
";

    fn case() -> Case {
        let mut cases = crate::case::parse(FILE, "sql/t", 0).unwrap_or_else(|e| panic!("{e}"));
        cases.remove(0)
    }

    fn rows(rs: &[&str]) -> Observation {
        Observation {
            columns: Some("K INT64".into()),
            rows: rs
                .iter()
                .map(|r| serde_json::from_str(r).unwrap_or_else(|e| panic!("{e}")))
                .collect(),
            ..Default::default()
        }
    }

    fn run(steps: Vec<Observation>) -> CaseRun {
        CaseRun {
            setup: Ok(()),
            steps: steps.into_iter().map(Ok).collect(),
        }
    }

    #[test]
    fn order_by_makes_rows_ordered() {
        let c = case();
        let nf = Observation::error("NOT_FOUND", "Table not found");
        assert_eq!(
            judge(
                &c,
                &run(vec![rows(&[r#"["1"]"#, r#"["2"]"#]), nf.clone()]),
                Oracle::Emulator
            ),
            Status::Pass
        );
        let swapped = judge(
            &c,
            &run(vec![rows(&[r#"["2"]"#, r#"["1"]"#]), nf]),
            Oracle::Emulator,
        );
        assert!(matches!(swapped, Status::Fail(_)), "{swapped:?}");
    }

    #[test]
    fn unimplemented_is_its_own_status() {
        let c = case();
        let un = Observation::error("UNIMPLEMENTED", "no SQL");
        let s = judge(&c, &run(vec![un, Observation::default()]), Oracle::Emulator);
        assert!(matches!(s, Status::Unimplemented(_)), "{s:?}");
        let setup = CaseRun {
            setup: Err(SetupError {
                code: Some(tonic::Code::Unimplemented),
                message: "prelude".into(),
            }),
            steps: Vec::new(),
        };
        assert!(matches!(
            judge(&c, &setup, Oracle::Emulator),
            Status::Unimplemented(_)
        ));
        let report = Report {
            results: vec![(&c, Status::Unimplemented("x".into())), (&c, Status::Pass)],
        };
        assert!(
            report
                .junit(false)
                .contains("<skipped message=\"UNIMPLEMENTED\"")
        );
        assert!(
            report
                .junit(true)
                .contains("<failure message=\"UNIMPLEMENTED\"")
        );
        let t = report.totals();
        assert_eq!((t.pass, t.fail, t.unimplemented), (1, 0, 1));
    }

    #[test]
    fn wrong_status_fails() {
        let c = case();
        let s = judge(
            &c,
            &run(vec![
                rows(&[r#"["1"]"#, r#"["2"]"#]),
                Observation::default(),
            ]),
            Oracle::Emulator,
        );
        let Status::Fail(d) = s else { panic!("{s:?}") };
        assert!(d.contains("status: expected NOT_FOUND, got OK"), "{d}");
    }
}
