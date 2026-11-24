//! `record`: turns a case run into `expect:` / `expect@spanner:` blocks and saves them as each case
//! finishes, so a run that is interrupted (time limit, lost access grant, Ctrl-C) keeps the cases it
//! got through.
//!
//! Every save re-reads the case file under a file lock and finds the case's steps in what is on disk
//! now, so recorders running at the same time (say `record` and `record --oracle spanner`, or two
//! `--filter` runs) don't overwrite each other's blocks in a shared file.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;

use crate::case::{self, Case, Corpus, Oracle, SpannerExpect};
use crate::observe;
use crate::report;
use crate::runner::CaseRun;

/// New block bodies for one case: step index within the case, block lines (unindented).
pub type Updates = Vec<(usize, Vec<String>)>;

/// One step where real Spanner and the emulator expectation disagree.
pub struct Disagreement {
    pub case: String,
    pub line: usize,
    pub sql: String,
    pub emulator: Vec<String>,
    pub spanner: Vec<String>,
}

impl Disagreement {
    pub fn markdown(&self) -> String {
        let block = |lines: &[String]| {
            if lines.is_empty() {
                "(no expectation: must succeed)".to_string()
            } else {
                lines.join("\n")
            }
        };
        format!(
            "### `{}` (line {})\n\n```sql\n{}\n```\n\nEmulator:\n\n```text\n{}\n```\n\nSpanner:\n\n```text\n{}\n```\n",
            self.case,
            self.line,
            self.sql.trim_end(),
            block(&self.emulator),
            block(&self.spanner),
        )
    }
}

/// Records finished cases into their files, one case at a time.
pub struct Recorder<'a> {
    corpus: &'a Corpus,
    oracle: Oracle,
    /// Recording clock (unix seconds), for `<now>` placeholders.
    now: i64,
    pub recorded: usize,
    pub problems: usize,
    pub disagreements: Vec<Disagreement>,
}

impl<'a> Recorder<'a> {
    pub fn new(corpus: &'a Corpus, oracle: Oracle, now: i64) -> Self {
        Self {
            corpus,
            oracle,
            now,
            recorded: 0,
            problems: 0,
            disagreements: Vec::new(),
        }
    }

    /// Saves `case`'s results to its file now. Problems are printed and counted, not returned: one
    /// case's trouble must not stop the others from being saved.
    pub fn finish(&mut self, case: &Case, run: &CaseRun) {
        if let Err(e) = &run.setup {
            eprintln!("SETUP {}: {}", case.name, e.message);
            self.problems += 1;
            return;
        }
        let built = match self.oracle {
            Oracle::Emulator => emulator_updates(case, run, self.now).map(|u| (u, Vec::new())),
            Oracle::Spanner => spanner_updates(case, run, self.now),
        };
        let Some((updates, found)) = built else {
            self.problems += 1;
            return;
        };
        if let Err(e) = save_case(self.corpus, case, &updates, self.oracle) {
            eprintln!("SAVE {}: {e}", case.name);
            self.problems += 1;
            return;
        }
        self.recorded += 1;
        self.disagreements.extend(found);
    }
}

/// `expect:` bodies for a run against the emulator; `None` (after printing why) on a harness error.
fn emulator_updates(case: &Case, run: &CaseRun, now: i64) -> Option<Updates> {
    let mut updates = Vec::new();
    let mut ok = true;
    for (i, (step, obs)) in case.steps.iter().zip(&run.steps).enumerate() {
        match obs {
            Err(e) => {
                eprintln!("ERROR {} line {}: {e}", case.name, step.line + 1);
                ok = false;
            }
            Ok(obs) => {
                if obs.is_ok() && step.expect.is_none() {
                    continue;
                }
                let obs = obs
                    .clone()
                    .with_placeholders(now)
                    .merge_manual(step.expect.as_ref());
                updates.push((i, obs.to_lines()));
            }
        }
    }
    ok.then_some(updates)
}

/// `expect@spanner:` bodies for a run against Cloud Spanner, with the steps that differ from the
/// emulator expectation.
fn spanner_updates(case: &Case, run: &CaseRun, now: i64) -> Option<(Updates, Vec<Disagreement>)> {
    let mut updates = Vec::new();
    let mut found = Vec::new();
    let mut ok = true;
    for (i, (step, obs)) in case.steps.iter().zip(&run.steps).enumerate() {
        let obs = match obs {
            Err(e) => {
                eprintln!("ERROR {} line {}: {e}", case.name, step.line + 1);
                ok = false;
                continue;
            }
            Ok(obs) => obs,
        };
        let agrees = match &step.expect {
            None => obs.is_ok(),
            Some(exp) => observe::compare(exp, obs, report::ordered(step)).is_ok(),
        };
        if agrees {
            if step.expect.is_some() || step.expect_spanner.is_some() {
                updates.push((i, vec!["same".to_string()]));
            }
            continue;
        }
        let previous = match &step.expect_spanner {
            Some(SpannerExpect::Differs(o)) => Some(o),
            _ => step.expect.as_ref(),
        };
        let lines = obs
            .clone()
            .with_placeholders(now)
            .merge_manual(previous)
            .to_lines();
        found.push(Disagreement {
            case: case.name.clone(),
            line: step.line + 1,
            sql: step.body.join("\n"),
            emulator: step
                .expect
                .as_ref()
                .map(|e| e.to_lines())
                .unwrap_or_default(),
            spanner: lines.clone(),
        });
        updates.push((i, lines));
    }
    ok.then_some((updates, found))
}

/// Writes `updates` into `case`'s file as it is on disk now, under an exclusive lock on the file.
/// Fails, writing nothing, if the case is gone or its steps changed since the corpus was loaded.
pub fn save_case(
    corpus: &Corpus,
    case: &Case,
    updates: &[(usize, Vec<String>)],
    oracle: Oracle,
) -> Result<(), String> {
    if updates.is_empty() {
        return Ok(());
    }
    let file = &corpus.files[case.file];
    let path = &file.path;
    let err = |e: &dyn std::fmt::Display| format!("{}: {e}", path.display());
    let _lock = lock(path).map_err(|e| err(&e))?;
    let text = std::fs::read_to_string(path).map_err(|e| err(&e))?;
    let current = case::parse(&text, &file.rel, case.file).map_err(|e| err(&e))?;
    let fresh = current
        .iter()
        .find(|c| c.name == case.name)
        .ok_or_else(|| err(&"case no longer in the file; not saved"))?;
    let same_steps = fresh.steps.len() == case.steps.len()
        && fresh
            .steps
            .iter()
            .zip(&case.steps)
            .all(|(a, b)| a.verb == b.verb && a.opts == b.opts && a.body == b.body);
    if !same_steps {
        return Err(err(&"case steps changed on disk during the run; not saved"));
    }
    let steps: Vec<_> = updates
        .iter()
        .map(|(i, body)| (&fresh.steps[*i], body.clone()))
        .collect();
    let lines: Vec<String> = text.lines().map(str::to_string).collect();
    let mut out = match oracle {
        Oracle::Emulator => case::rewrite(&lines, &steps),
        Oracle::Spanner => case::rewrite_spanner(&lines, &steps),
    }
    .join("\n");
    out.push('\n');
    // Write a sibling and rename it over the file, so a run killed mid-write leaves the old file.
    let tmp = path.with_extension(format!("case.tmp-{}", std::process::id()));
    let mut f = File::create(&tmp).map_err(|e| err(&e))?;
    f.write_all(out.as_bytes())
        .and_then(|()| f.sync_all())
        .and_then(|()| std::fs::rename(&tmp, path))
        .map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            err(&e)
        })
}

/// Takes an exclusive lock on the file at `path`, held until the returned handle drops. A writer
/// replaces the file by rename while holding the lock, so a waiter that then gets the lock on the
/// old inode retries on the new one.
fn lock(path: &Path) -> std::io::Result<File> {
    loop {
        let f = OpenOptions::new().read(true).open(path)?;
        f.lock()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if f.metadata()?.ino() != std::fs::metadata(path)?.ino() {
                continue;
            }
        }
        return Ok(f);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observe::Observation;
    use std::path::PathBuf;

    const FILE: &str = "tags: sql

case one
query:
  SELECT 1

case two
query:
  SELECT 2
expect:
  columns: INT64
  [\"2\"]
";

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "spanbolt-conformance-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sql")).unwrap_or_else(|e| panic!("{e}"));
        std::fs::write(dir.join("sql/basic.case"), FILE).unwrap_or_else(|e| panic!("{e}"));
        dir
    }

    fn run_with(obs: Observation) -> CaseRun {
        CaseRun {
            setup: Ok(()),
            steps: vec![Ok(obs)],
        }
    }

    fn error(msg: &str) -> Observation {
        Observation::error("INVALID_ARGUMENT", msg)
    }

    fn read(dir: &Path) -> String {
        std::fs::read_to_string(dir.join("sql/basic.case")).unwrap_or_else(|e| panic!("{e}"))
    }

    /// A run that stops after its first case (time limit, lost grant) has already saved that case,
    /// and left the cases it never reached alone.
    #[test]
    fn interrupted_run_keeps_finished_cases() {
        let dir = scratch("interrupted");
        let corpus = case::load(&dir).unwrap_or_else(|e| panic!("{e}"));
        let mut rec = Recorder::new(&corpus, Oracle::Emulator, 0);
        rec.finish(&corpus.cases[0], &run_with(error("one")));
        // The run dies here: case two never finishes, nothing else gets called.
        assert_eq!((rec.recorded, rec.problems), (1, 0));
        let text = read(&dir);
        assert!(
            text.contains("  SELECT 1\nexpect:\n  error: INVALID_ARGUMENT\n  message: one\n"),
            "{text}"
        );
        assert!(text.ends_with("  SELECT 2\nexpect:\n  columns: INT64\n  [\"2\"]\n"));
        let again = case::load(&dir).unwrap_or_else(|e| panic!("{e}"));
        assert!(again.cases[0].steps[0].expect.is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Two recorders that loaded the file before either wrote it keep each other's blocks: each
    /// save applies to the file as it is on disk, not as it was at load time.
    #[test]
    fn concurrent_recorders_keep_each_others_blocks() {
        let dir = scratch("concurrent");
        let a = case::load(&dir).unwrap_or_else(|e| panic!("{e}"));
        let b = case::load(&dir).unwrap_or_else(|e| panic!("{e}"));
        // Recorder A (emulator) re-records case two; recorder B (Spanner) records case one, then
        // case two, from its stale view of the file.
        let mut ra = Recorder::new(&a, Oracle::Emulator, 0);
        ra.finish(&a.cases[1], &run_with(error("two")));
        let mut rb = Recorder::new(&b, Oracle::Spanner, 0);
        rb.finish(&b.cases[0], &run_with(error("spanner one")));
        rb.finish(&b.cases[1], &run_with(error("spanner two")));
        assert_eq!((ra.problems, rb.problems), (0, 0));
        let text = read(&dir);
        let cases = case::parse(&text, "sql/basic", 0).unwrap_or_else(|e| panic!("{e}"));
        let msg = |o: Option<&Observation>| o.map(|o| o.to_lines().join("|"));
        assert_eq!(
            msg(cases[0].steps[0].expectation(Oracle::Spanner)),
            Some("error: INVALID_ARGUMENT|message: spanner one".into()),
            "{text}"
        );
        assert_eq!(
            msg(cases[1].steps[0].expectation(Oracle::Emulator)),
            Some("error: INVALID_ARGUMENT|message: two".into()),
            "{text}"
        );
        assert_eq!(
            msg(cases[1].steps[0].expectation(Oracle::Spanner)),
            Some("error: INVALID_ARGUMENT|message: spanner two".into()),
            "{text}"
        );
        assert_eq!(rb.disagreements.len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A case edited during the run (steps added or changed) is not saved over the edit.
    #[test]
    fn edited_case_is_not_overwritten() {
        let dir = scratch("edited");
        let corpus = case::load(&dir).unwrap_or_else(|e| panic!("{e}"));
        let edited = FILE.replace("SELECT 1", "SELECT 10");
        std::fs::write(dir.join("sql/basic.case"), &edited).unwrap_or_else(|e| panic!("{e}"));
        let mut rec = Recorder::new(&corpus, Oracle::Emulator, 0);
        rec.finish(&corpus.cases[0], &run_with(error("one")));
        assert_eq!((rec.recorded, rec.problems), (0, 1));
        assert_eq!(read(&dir), edited);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
