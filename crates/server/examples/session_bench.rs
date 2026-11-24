//! Session registry cost: memory per session, BatchCreateSessions latency as sessions pile up, and
//! ListSessions over many sessions.
//!
//! ```text
//! cargo run --release -p spanbolt-server --example session_bench -- [grpc|registry] [sessions]
//! ```
//!
//! `grpc` (default) runs an in-process server and creates sessions with BatchCreateSessions of 100,
//! over a new connection every `SESSION_BENCH_RECONNECT` batches (default 1, like a client that is
//! opened and closed per request; raise it when repeated runs run out of local ports), then times
//! the first 20 ListSessions pages of 1000. `registry` calls the registry directly, without the
//! server, and times two pages. Every 100 batches it prints the sessions so far, the mean and max
//! batch time in microseconds, and the live heap (macOS: all malloc zones; Linux: glibc
//! `mallinfo2`). At the end: live heap per session.

use std::sync::Arc;
use std::time::{Duration, Instant};

use spanbolt_proto::spanner as pb;
use spanbolt_proto::spanner::spanner_client::SpannerClient;
use spanbolt_server::Emulator;
use spanbolt_server::emulator::{InstanceMeta, Session};
use tonic::transport::Channel;

type BoxErr = Box<dyn std::error::Error + Send + Sync>;

const INSTANCE: &str = "projects/test/instances/instance0";
const DB: &str = "projects/test/instances/instance0/databases/database0";
const BATCH: usize = 100;

/// Live malloc bytes.
#[cfg(target_os = "macos")]
fn heap() -> u64 {
    #[repr(C)]
    #[derive(Default)]
    struct MallocStats {
        blocks_in_use: u32,
        size_in_use: usize,
        max_size_in_use: usize,
        size_allocated: usize,
    }
    unsafe extern "C" {
        fn malloc_zone_statistics(zone: *mut std::ffi::c_void, stats: *mut MallocStats);
    }
    let mut ms = MallocStats::default();
    // SAFETY: a null zone means all zones; `ms` matches malloc_statistics_t.
    unsafe { malloc_zone_statistics(std::ptr::null_mut(), &mut ms) };
    ms.size_in_use as u64
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn heap() -> u64 {
    #[repr(C)]
    #[derive(Default)]
    struct Mallinfo2 {
        arena: usize,
        ordblks: usize,
        smblks: usize,
        hblks: usize,
        hblkhd: usize,
        usmblks: usize,
        fsmblks: usize,
        uordblks: usize,
        fordblks: usize,
        keepcost: usize,
    }
    unsafe extern "C" {
        fn mallinfo2() -> Mallinfo2;
    }
    // SAFETY: glibc >= 2.33.
    let m = unsafe { mallinfo2() };
    (m.uordblks + m.hblkhd) as u64
}

#[cfg(not(any(target_os = "macos", all(target_os = "linux", target_env = "gnu"))))]
fn heap() -> u64 {
    0
}

fn mb(b: u64) -> f64 {
    b as f64 / 1048576.0
}

struct Report {
    batches: usize,
    sum: Duration,
    max: Duration,
    base: u64,
}

impl Report {
    fn add(&mut self, sessions: usize, took: Duration) {
        self.batches += 1;
        self.sum += took;
        self.max = self.max.max(took);
        if self.batches.is_multiple_of(100) {
            let h = heap();
            println!(
                "{sessions},{:.0},{:.0},{:.1},{:.0}",
                self.sum.as_secs_f64() * 1e6 / 100.0,
                self.max.as_secs_f64() * 1e6,
                mb(h),
                (h.saturating_sub(self.base)) as f64 / sessions as f64,
            );
            self.sum = Duration::ZERO;
            self.max = Duration::ZERO;
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), BoxErr> {
    let mut args = std::env::args().skip(1);
    let mode = args.next().unwrap_or_else(|| "grpc".into());
    let total: usize = args.next().map_or(Ok(1_000_000), |s| s.parse())?;
    let reconnect: usize = std::env::var("SESSION_BENCH_RECONNECT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);

    let emu = Arc::new(Emulator::new(Arc::new(spanbolt_server::NoSql)));
    emu.create_instance(
        INSTANCE.into(),
        InstanceMeta {
            config: "emulator-config".into(),
            display_name: "bench".into(),
            node_count: 1,
            processing_units: 1000,
            labels: Default::default(),
            create_time: emu.clock.now(),
            update_time: emu.clock.now(),
        },
    )?;
    let entry = emu.add_database(DB, || emu.new_database(Default::default()))?;
    println!(
        "# mode={mode} sessions={total} size_of::<Session>={}",
        std::mem::size_of::<Session>()
    );
    println!("sessions,mean_us,max_us,heap_mb,heap_bytes_per_session");
    let base = heap();
    let mut rep = Report {
        batches: 0,
        sum: Duration::ZERO,
        max: Duration::ZERO,
        base,
    };
    let start = Instant::now();
    let mut made = 0;
    match mode.as_str() {
        "registry" => {
            while made < total {
                let t = Instant::now();
                for _ in 0..BATCH {
                    emu.add_session(Session {
                        name: format!("{DB}/sessions/{}", emu.opaque_id()),
                        database: entry.name.clone(),
                        db: entry.db.clone(),
                        multiplexed: false,
                        create_time: emu.clock.now(),
                        labels: Default::default(),
                        creator_role: String::new(),
                        current_txn: parking_lot::Mutex::new(None),
                        last_used: Default::default(),
                    });
                }
                made += BATCH;
                rep.add(made, t.elapsed());
            }
            let t = Instant::now();
            let (page, next) = emu.sessions_page(DB, 1000, "");
            println!(
                "# sessions_page: {} sessions in {:.1} ms",
                page.len(),
                t.elapsed().as_secs_f64() * 1e3
            );
            let t = Instant::now();
            let (page, _) = emu.sessions_page(DB, 1000, &next);
            println!(
                "# sessions_page (2nd): {} sessions in {:.1} ms",
                page.len(),
                t.elapsed().as_secs_f64() * 1e3
            );
        }
        "grpc" => {
            let server = spanbolt_server::spawn("127.0.0.1:0", emu.clone()).await?;
            let url = format!("http://{}", server.addr);
            let mut client: Option<SpannerClient<Channel>> = None;
            let mut batches = 0;
            while made < total {
                let t = Instant::now();
                if batches % reconnect == 0 {
                    client = None;
                }
                let c = match &mut client {
                    Some(c) => c,
                    None => client.insert(SpannerClient::new(
                        Channel::from_shared(url.clone())?.connect().await?,
                    )),
                };
                let got = c
                    .batch_create_sessions(pb::BatchCreateSessionsRequest {
                        database: DB.into(),
                        session_template: None,
                        session_count: BATCH as i32,
                    })
                    .await?
                    .into_inner()
                    .session
                    .len();
                made += got;
                batches += 1;
                rep.add(made, t.elapsed());
            }
            // ListSessions: the first pages of 1000, as a client counting sessions pages them.
            let mut c = SpannerClient::new(Channel::from_shared(url.clone())?.connect().await?);
            let (mut token, mut listed) = (String::new(), 0);
            let t = Instant::now();
            let pages = 20;
            for _ in 0..pages {
                let r = c
                    .list_sessions(pb::ListSessionsRequest {
                        database: DB.into(),
                        page_size: 1000,
                        page_token: token.clone(),
                        filter: String::new(),
                    })
                    .await?
                    .into_inner();
                listed += r.sessions.len();
                token = r.next_page_token;
            }
            println!(
                "# ListSessions: {pages} pages ({listed} sessions) in {:.0} ms, {:.1} ms per page; heap {:.1} MB",
                t.elapsed().as_secs_f64() * 1e3,
                t.elapsed().as_secs_f64() * 1e3 / pages as f64,
                mb(heap())
            );
            drop(client);
            drop(server);
        }
        m => return Err(format!("unknown mode {m}: grpc or registry").into()),
    }
    let h = heap();
    println!(
        "# {made} sessions in {:.1}s; heap {:.1} MB -> {:.1} MB, {:.0} bytes per session",
        start.elapsed().as_secs_f64(),
        mb(base),
        mb(h),
        h.saturating_sub(base) as f64 / made as f64
    );
    Ok(())
}
