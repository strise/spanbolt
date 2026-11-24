use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use clap::{Args, Parser, Subcommand};
use spanbolt_server::Emulator;

/// mimalloc instead of the system allocator. glibc's malloc slows down as the heap grows under
/// spanbolt's pattern (many small allocations from the blocking pool's threads, MVCC versions kept
/// alive): a 10,000-row insert commit doubled in time between 10k and 1M rows in the table, with
/// most of the growth in work that never touches the table (decoding, DEFAULT evaluation).
/// `--example insert_bench` measures it.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// A fast drop-in replacement for the Cloud Spanner emulator.
#[derive(Parser)]
#[command(name = "spanbolt", version, args_conflicts_with_subcommands = true)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    #[command(flatten)]
    serve: ServeArgs,
}

#[derive(Subcommand)]
enum Command {
    /// Ask a running spanbolt to write all instances and databases to a snapshot file.
    Snapshot(SnapshotArgs),
}

#[derive(Args)]
struct ServeArgs {
    /// Address for the gRPC server.
    #[arg(long, default_value = "0.0.0.0:9010")]
    host_port: String,
    /// Port for the REST gateway, on the gRPC server's host (the emulator uses 9020).
    #[arg(long, env = "SPANBOLT_REST_PORT")]
    rest_port: Option<u16>,
    /// Snapshot file to load at startup.
    #[arg(long, env = "SPANBOLT_SNAPSHOT")]
    snapshot: Option<PathBuf>,
    /// Follow the official emulator where it differs from production Spanner: honour its
    /// `spanner_emulator.disable_query_null_filtered_index_check` hint, reject window functions and
    /// NULLS FIRST/LAST in the analyser, use its error codes and messages for reads, and use its
    /// full-text search (byte-wise rquery parser, SEARCH anywhere and without a usable search
    /// index, SCORE without SEARCH, its SNIPPET JSON).
    #[arg(long)]
    emulator_compat: bool,
}

#[derive(Args)]
struct SnapshotArgs {
    /// gRPC address of the running spanbolt.
    #[arg(long, env = "SPANNER_EMULATOR_HOST", default_value = "127.0.0.1:9010")]
    target: String,
    /// Where the server writes the snapshot: a path on the server's filesystem.
    path: PathBuf,
}

fn main() -> std::process::ExitCode {
    let started = Instant::now();
    let cli = Cli::parse();
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        // Every call's handler runs on the blocking pool (`offload`), and a streaming call also
        // holds a producer thread there while its client reads. Past the cap, calls queue for a
        // thread; tokio's default of 512 is reachable with a few hundred concurrent streams.
        .max_blocking_threads(4096)
        .build()
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("spanbolt: cannot start runtime: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    match cli.command {
        Some(Command::Snapshot(args)) => runtime.block_on(save_snapshot(args)),
        None => runtime.block_on(serve(cli.serve, started)),
    }
}

async fn save_snapshot(args: SnapshotArgs) -> std::process::ExitCode {
    use spanbolt_proto::control::SaveSnapshotRequest;
    use spanbolt_proto::control::control_client::ControlClient;
    let url = if args.target.contains("://") {
        args.target.clone()
    } else {
        format!("http://{}", args.target)
    };
    let mut client = match ControlClient::connect(url).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("spanbolt: cannot connect to {}: {e}", args.target);
            return std::process::ExitCode::FAILURE;
        }
    };
    let started = Instant::now();
    let path = args.path.to_string_lossy().into_owned();
    match client
        .save_snapshot(SaveSnapshotRequest { path: path.clone() })
        .await
    {
        Ok(resp) => {
            eprintln!(
                "spanbolt: saved {path} ({} bytes) in {:.1} ms",
                resp.into_inner().bytes,
                started.elapsed().as_secs_f64() * 1000.0
            );
            std::process::ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("spanbolt: SaveSnapshot failed: {}", e.message());
            std::process::ExitCode::FAILURE
        }
    }
}

/// Resolves on SIGINT or SIGTERM (`docker stop`; PID 1 ignores an unhandled SIGTERM).
async fn termination() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        if let Ok(mut term) = signal(SignalKind::terminate()) {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
            }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}

async fn serve(args: ServeArgs, started: Instant) -> std::process::ExitCode {
    let mut emu = Emulator::new(spanbolt_server::sql::default_engine(args.emulator_compat));
    emu.emulator_compat = args.emulator_compat;
    let emu = Arc::new(emu);
    if let Some(path) = &args.snapshot {
        let loading = Instant::now();
        match spanbolt_storage::snapshot::load(path) {
            Ok(images) => {
                let (i, d) = emu.restore(images);
                eprintln!(
                    "spanbolt: loaded {} ({i} instances, {d} databases) in {:.1} ms",
                    path.display(),
                    loading.elapsed().as_secs_f64() * 1000.0
                );
            }
            Err(e) => {
                eprintln!(
                    "spanbolt: cannot load snapshot {}: {}",
                    path.display(),
                    e.message
                );
                return std::process::ExitCode::FAILURE;
            }
        }
    }
    let listener = match tokio::net::TcpListener::bind(&args.host_port).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("spanbolt: cannot listen on {}: {e}", args.host_port);
            return std::process::ExitCode::FAILURE;
        }
    };
    let addr = listener
        .local_addr()
        .map(|a| a.to_string())
        .unwrap_or_else(|_| args.host_port.clone());
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let rest = match args.rest_port {
        Some(port) => {
            let host = args
                .host_port
                .rsplit_once(':')
                .map_or("0.0.0.0", |(h, _)| h);
            let rest_addr = format!("{host}:{port}");
            let rest_listener = match tokio::net::TcpListener::bind(&rest_addr).await {
                Ok(l) => l,
                Err(e) => {
                    eprintln!("spanbolt: cannot listen on {rest_addr}: {e}");
                    return std::process::ExitCode::FAILURE;
                }
            };
            let bound = rest_listener
                .local_addr()
                .map(|a| a.to_string())
                .unwrap_or(rest_addr);
            let mut rx = stop_rx.clone();
            let task = tokio::spawn(spanbolt_server::rest::serve(
                rest_listener,
                emu.clone(),
                async move {
                    let _ = rx.wait_for(|s| *s).await;
                },
            ));
            Some((bound, task))
        }
        None => None,
    };
    // Testcontainers' SpannerEmulatorContainer waits for ".*Cloud Spanner emulator running\..*".
    eprintln!(
        "spanbolt: Cloud Spanner emulator running. gRPC on {addr}{} (ready in {:.1} ms)",
        rest.as_ref()
            .map(|(a, _)| format!(", REST on {a}"))
            .unwrap_or_default(),
        started.elapsed().as_secs_f64() * 1000.0
    );
    let shutdown = async move {
        termination().await;
        spanbolt_server::timing::flush();
        let _ = stop_tx.send(true);
    };
    let result = spanbolt_server::serve(listener, emu, shutdown).await;
    if let Some((_, task)) = rest {
        match task.await {
            Ok(Err(e)) => eprintln!("spanbolt: REST gateway error: {e}"),
            Err(e) => eprintln!("spanbolt: REST gateway task failed: {e}"),
            Ok(Ok(())) => {}
        }
    }
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("spanbolt: server error: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}
