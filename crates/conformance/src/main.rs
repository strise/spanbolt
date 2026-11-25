use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use clap::{Args, Parser, Subcommand, ValueEnum};
use spanbolt_conformance::case::{self, Case, Corpus, Oracle, SpannerExpect};
use spanbolt_conformance::record::{Disagreement, Recorder};
use spanbolt_conformance::report::{self, Report, Status};
use spanbolt_conformance::runner::{CaseRun, SpannerOracle, Target};

/// Differential conformance harness for Spanner gRPC endpoints.
#[derive(Parser)]
#[command(name = "spanbolt-conformance", version)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the cases against the oracle and write the observed outcomes into the case files.
    Record(RecordArgs),
    /// Run the cases and compare with the recorded expectations.
    Check(CheckArgs),
    /// List the steps where the recorded real-Spanner outcome differs from the emulator's.
    DiffOracles(DiffArgs),
}

#[derive(Clone, Copy, PartialEq, ValueEnum)]
enum OracleArg {
    /// The official emulator: `expect:` blocks.
    Emulator,
    /// Real Cloud Spanner: `expect@spanner:` blocks.
    Spanner,
}

impl OracleArg {
    fn oracle(self) -> Oracle {
        match self {
            Self::Emulator => Oracle::Emulator,
            Self::Spanner => Oracle::Spanner,
        }
    }
}

#[derive(Args)]
struct RecordArgs {
    #[command(flatten)]
    common: Common,
    /// Where the expectations come from. `spanner` connects to real Cloud Spanner with the
    /// application-default credentials and writes `expect@spanner:` blocks (`same` when Spanner
    /// agrees with the emulator expectation); `--target` is then ignored.
    #[arg(long, value_enum, default_value_t = OracleArg::Emulator)]
    oracle: OracleArg,
    /// Spanner project (with `--oracle spanner`).
    #[arg(long)]
    spanner_project: Option<String>,
    /// Existing Spanner instance (with `--oracle spanner`). The harness creates, uses and drops
    /// databases named `spanbolt-oracle-<mmdd><run>-<n>` there and touches nothing else.
    #[arg(long)]
    spanner_instance: Option<String>,
    /// Log of every Spanner database created and dropped.
    #[arg(long, default_value = "target/conformance/spanner-oracle-dbs.log")]
    db_log: PathBuf,
    /// Markdown report of the steps where Spanner disagrees with the emulator expectation.
    #[arg(long)]
    report: Option<PathBuf>,
}

#[derive(Args)]
struct DiffArgs {
    /// Root directory of the case files.
    #[arg(long, default_value = "conformance/cases")]
    cases: PathBuf,
    /// Write the list as Markdown here instead of printing it.
    #[arg(long)]
    out: Option<PathBuf>,
}

#[derive(Args)]
struct Common {
    /// Endpoint speaking the Spanner gRPC API (`host:port`).
    #[arg(long, default_value = "")]
    target: String,
    /// Root directory of the case files.
    #[arg(long, default_value = "conformance/cases")]
    cases: PathBuf,
    /// Only cases whose name contains this substring (repeatable).
    #[arg(long)]
    filter: Vec<String>,
    /// Only cases with this tag (repeatable).
    #[arg(long)]
    tag: Vec<String>,
    /// Cases run in parallel.
    #[arg(long, default_value_t = 8)]
    jobs: usize,
}

#[derive(Clone, Copy, PartialEq, ValueEnum)]
enum Unimplemented {
    Skip,
    Fail,
}

#[derive(Args)]
struct CheckArgs {
    #[command(flatten)]
    common: Common,
    /// JUnit XML output path.
    #[arg(long)]
    junit: Option<PathBuf>,
    /// JSON summary output path.
    #[arg(long)]
    json: Option<PathBuf>,
    /// Whether a step returning UNIMPLEMENTED counts as skipped or failed.
    #[arg(long, value_enum, default_value_t = Unimplemented::Skip)]
    unimplemented: Unimplemented,
    /// Print every failing diff and every UNIMPLEMENTED case.
    #[arg(short, long)]
    verbose: bool,
    /// Which expectations to check against: `spanner` uses `expect@spanner:` where recorded and
    /// falls back to `expect:`.
    #[arg(long, value_enum, default_value_t = OracleArg::Emulator)]
    oracle: OracleArg,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(spanbolt_conformance::deep::STACK)
        .build()
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("cannot start runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    let result = runtime.block_on(async move {
        match cli.cmd {
            Cmd::Record(c) => record(c).await,
            Cmd::Check(c) => check(c).await,
            Cmd::DiffOracles(d) => diff_oracles(d),
        }
    });
    match result {
        Ok(code) => code,
        Err(e) => {
            eprintln!("spanbolt-conformance: {e}");
            ExitCode::from(2)
        }
    }
}

fn select<'a>(corpus: &'a Corpus, c: &Common) -> Vec<&'a Case> {
    corpus
        .cases
        .iter()
        .filter(|case| c.filter.is_empty() || c.filter.iter().any(|f| case.name.contains(f)))
        .filter(|case| c.tag.is_empty() || c.tag.iter().any(|t| case.tags.contains(t)))
        .collect()
}

/// Runs the non-skipped cases in parallel; results keep the input order. `on_done` sees each case as
/// it finishes (on this task, one at a time), so `record` can save it before the run ends.
async fn run_all(
    target: Arc<Target>,
    cases: &[&Case],
    oracle: Oracle,
    jobs: usize,
    mut on_done: impl FnMut(&Case, &CaseRun),
) -> Vec<Option<CaseRun>> {
    let sem = Arc::new(tokio::sync::Semaphore::new(jobs.max(1)));
    let mut set = tokio::task::JoinSet::new();
    for (i, case) in cases.iter().enumerate() {
        if case.skip_for(oracle).is_some() {
            continue;
        }
        let case = (*case).clone();
        let target = target.clone();
        let sem = sem.clone();
        set.spawn(async move {
            let _permit = sem.acquire_owned().await;
            (i, target.run_case(&case).await)
        });
    }
    let mut out: Vec<Option<CaseRun>> = (0..cases.len()).map(|_| None).collect();
    let mut done = 0;
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);
    loop {
        let r = tokio::select! {
            r = set.join_next() => r,
            _ = &mut ctrl_c => {
                eprintln!("interrupted: dropping template databases (Ctrl-C again to skip)");
                exit_on_ctrl_c();
                set.abort_all();
                target.drop_templates().await;
                std::process::exit(130);
            }
        };
        let Some(r) = r else { break };
        match r {
            Ok((i, run)) => {
                on_done(cases[i], &run);
                out[i] = Some(run);
            }
            Err(e) => eprintln!("case task failed: {e}"),
        }
        done += 1;
        if done % 50 == 0 {
            eprintln!("  {done} cases run");
        }
    }
    // Tokio keeps the SIGINT handler installed for the rest of the process; restore "Ctrl-C
    // exits" for whatever runs after this.
    exit_on_ctrl_c();
    // Every case cloned and dropped its own database; the templates are all that is left.
    target.drop_templates().await;
    out
}

fn exit_on_ctrl_c() {
    tokio::spawn(async {
        if tokio::signal::ctrl_c().await.is_ok() {
            std::process::exit(130);
        }
    });
}

async fn prepare(c: &Common) -> Result<(Corpus, Arc<Target>), String> {
    if c.target.is_empty() {
        return Err("--target is required".into());
    }
    let target = Target::connect(&c.target).await?;
    load_for(c, target)
}

fn load_for(c: &Common, target: Target) -> Result<(Corpus, Arc<Target>), String> {
    let corpus = case::load(&c.cases)?;
    eprintln!(
        "{} cases loaded from {}; target {} ({})",
        corpus.cases.len(),
        c.cases.display(),
        if target.is_spanner() {
            "Cloud Spanner"
        } else {
            &c.target
        },
        if target.can_clone {
            "CloneDatabase templates"
        } else {
            "one CreateDatabase per case"
        }
    );
    Ok((corpus, Arc::new(target)))
}

async fn record(r: RecordArgs) -> Result<ExitCode, String> {
    if r.oracle == OracleArg::Spanner {
        return record_spanner(r).await;
    }
    let c = r.common;
    let (corpus, target) = prepare(&c).await?;
    let cases = select(&corpus, &c);
    let mut rec = Recorder::new(&corpus, Oracle::Emulator, unix_now());
    run_all(target, &cases, Oracle::Emulator, c.jobs, |case, run| {
        rec.finish(case, run)
    })
    .await;
    let (recorded, problems) = (rec.recorded, rec.problems);
    eprintln!("recorded {recorded} cases, {problems} with setup or harness problems");
    Ok(if problems == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

async fn check(a: CheckArgs) -> Result<ExitCode, String> {
    let (corpus, target) = prepare(&a.common).await?;
    let cases = select(&corpus, &a.common);
    let oracle = a.oracle.oracle();
    let runs = run_all(target, &cases, oracle, a.common.jobs, |_, _| {}).await;
    let results = cases
        .iter()
        .zip(&runs)
        .map(|(case, run)| {
            let status = match (run, case.skip_for(oracle)) {
                (_, Some(reason)) => Status::Skipped(reason.to_string()),
                (Some(run), None) => report::judge(case, run, oracle),
                (None, None) => Status::Error("case task panicked".into()),
            };
            (*case, status)
        })
        .collect();
    let report = Report { results };
    report.print(a.verbose);
    let fails_unimpl = a.unimplemented == Unimplemented::Fail;
    if let Some(p) = &a.junit {
        write(p, &report.junit(fails_unimpl))?;
    }
    if let Some(p) = &a.json {
        let j = report.json(&a.common.target, fails_unimpl);
        write(p, &serde_json::to_string_pretty(&j).unwrap_or_default())?;
    }
    let t = report.totals();
    let failed = t.fail + t.error + if fails_unimpl { t.unimplemented } else { 0 };
    Ok(if failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

fn disagreement_report(title: &str, items: &[Disagreement]) -> String {
    let mut out = format!("# {title}\n\n{} steps disagree.\n\n", items.len());
    for d in items {
        out.push_str(&d.markdown());
        out.push('\n');
    }
    out
}

async fn record_spanner(r: RecordArgs) -> Result<ExitCode, String> {
    let (Some(project), Some(instance)) = (r.spanner_project.clone(), r.spanner_instance.clone())
    else {
        return Err("--oracle spanner needs --spanner-project and --spanner-instance".into());
    };
    if let Some(dir) = r.db_log.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let target = Target::connect_spanner(SpannerOracle {
        project,
        instance,
        db_log: r.db_log.clone(),
    })
    .await?;
    let c = &r.common;
    let (corpus, target) = load_for(c, target)?;
    let cases = select(&corpus, c);
    eprintln!(
        "recording {} cases from Cloud Spanner; databases logged to {}",
        cases
            .iter()
            .filter(|c| c.skip_for(Oracle::Spanner).is_none())
            .count(),
        r.db_log.display()
    );
    let mut rec = Recorder::new(&corpus, Oracle::Spanner, unix_now());
    run_all(target, &cases, Oracle::Spanner, c.jobs, |case, run| {
        rec.finish(case, run)
    })
    .await;
    let (recorded, problems, disagreements) = (rec.recorded, rec.problems, rec.disagreements);
    for d in &disagreements {
        eprintln!("DIFFERS {} line {}", d.case, d.line);
    }
    if let Some(p) = &r.report {
        write(
            p,
            &disagreement_report("Cloud Spanner vs emulator", &disagreements),
        )?;
    }
    eprintln!(
        "recorded {recorded} cases from Cloud Spanner ({} steps differ from the emulator), {problems} with setup or harness problems",
        disagreements.len()
    );
    Ok(if problems == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

fn diff_oracles(d: DiffArgs) -> Result<ExitCode, String> {
    let corpus = case::load(&d.cases)?;
    let mut items = Vec::new();
    let mut recorded = 0;
    for case in &corpus.cases {
        for step in &case.steps {
            match &step.expect_spanner {
                None => {}
                Some(SpannerExpect::Same) => recorded += 1,
                Some(SpannerExpect::Differs(o)) => {
                    recorded += 1;
                    items.push(Disagreement {
                        case: case.name.clone(),
                        line: step.line + 1,
                        sql: step.body.join("\n"),
                        emulator: step
                            .expect
                            .as_ref()
                            .map(|e| e.to_lines())
                            .unwrap_or_default(),
                        spanner: o.to_lines(),
                    });
                }
            }
        }
    }
    let text = disagreement_report(
        &format!("Cloud Spanner vs emulator ({recorded} steps recorded from Spanner)"),
        &items,
    );
    match &d.out {
        Some(p) => write(p, &text)?,
        None => print!("{text}"),
    }
    Ok(ExitCode::SUCCESS)
}

fn write(path: &PathBuf, text: &str) -> Result<(), String> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    std::fs::write(path, text).map_err(|e| format!("{}: {e}", path.display()))
}
