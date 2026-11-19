//! Per-RPC timing log, for profiling where a client's time goes. Off by default.
//!
//! Set `SPANBOLT_TIMING=<path>` and every gRPC call appends one JSON line to `<path>` when its
//! response body ends (the last byte of a stream, or the error):
//!
//! ```json
//! {"t":1763560000123.4,"m":"ExecuteStreamingSql","us":812,"code":0,"sql":"SELECT ..."}
//! ```
//!
//! `t` is the start in Unix milliseconds, `us` the duration until the response body ended (for a
//! stream this includes the client reading it), `code` the gRPC status. Handlers add what the
//! RPC ran through [`note`]: `sql` (a statement, the statements of a batch joined by `;\n`, or
//! the DDL statements), `n` (statements or mutations). `scripts/timing-report.py` turns the log
//! into totals, counts and p99 per RPC and per statement fingerprint.
//!
//! Disabled, the layer forwards each call and its body untouched apart from one branch.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use http_body::{Body, Frame, SizeHint};
use parking_lot::Mutex;
use tonic::codegen::http;

/// The open log, if `SPANBOLT_TIMING` named one.
fn log() -> Option<&'static Mutex<BufWriter<File>>> {
    static LOG: OnceLock<Option<Mutex<BufWriter<File>>>> = OnceLock::new();
    LOG.get_or_init(|| {
        let path = std::env::var_os("SPANBOLT_TIMING").filter(|p| !p.is_empty())?;
        match File::create(&path) {
            Ok(f) => {
                eprintln!("spanbolt: timing every RPC into {}", path.to_string_lossy());
                Some(Mutex::new(BufWriter::with_capacity(1 << 20, f)))
            }
            Err(e) => {
                eprintln!(
                    "spanbolt: cannot open timing log {}: {e}",
                    path.to_string_lossy()
                );
                None
            }
        }
    })
    .as_ref()
}

/// Whether the timing log is on.
pub fn enabled() -> bool {
    log().is_some()
}

/// Writes buffered lines to the file.
pub fn flush() {
    if let Some(l) = log() {
        let _ = l.lock().flush();
    }
}

/// What a handler adds to its call's line.
#[derive(Default)]
struct Detail {
    sql: Option<String>,
    n: Option<u64>,
}

/// Shared between the layer and the handler, through the request's extensions.
#[derive(Clone, Default)]
struct Note(Arc<Mutex<Detail>>);

/// Adds the statement text (and a count) to the current call's line; a no-op when timing is
/// off.
pub fn note(ext: &http::Extensions, sql: impl FnOnce() -> String, n: Option<u64>) {
    if let Some(Note(d)) = ext.get::<Note>() {
        let mut d = d.lock();
        d.sql = Some(sql());
        d.n = n;
    }
}

/// One call in flight; writes its line when dropped.
struct Record {
    method: String,
    wall: f64,
    start: Instant,
    code: Option<i32>,
    note: Note,
}

impl Drop for Record {
    fn drop(&mut self) {
        let Some(l) = log() else { return };
        let us = self.start.elapsed().as_micros() as u64;
        let mut line = serde_json::json!({
            "t": self.wall,
            "m": self.method,
            "us": us,
            // No status seen: the client went away before the body ended.
            "code": self.code.unwrap_or(-1),
        });
        let d = self.note.0.lock();
        if let Some(s) = &d.sql {
            line["sql"] = s.as_str().into();
        }
        if let Some(n) = d.n {
            line["n"] = n.into();
        }
        drop(d);
        let mut w = l.lock();
        let _ = serde_json::to_writer(&mut *w, &line);
        let _ = w.write_all(b"\n");
    }
}

/// The tower layer that times every call.
#[derive(Clone, Copy, Default)]
pub struct TimingLayer;

impl<S> tower_layer::Layer<S> for TimingLayer {
    type Service = Timed<S>;

    fn layer(&self, inner: S) -> Timed<S> {
        Timed { inner }
    }
}

#[derive(Clone)]
pub struct Timed<S> {
    inner: S,
}

impl<S, B, R> tonic::codegen::Service<http::Request<B>> for Timed<S>
where
    S: tonic::codegen::Service<http::Request<B>, Response = http::Response<R>>,
    S::Future: Send + 'static,
{
    type Response = http::Response<TimedBody<R>>;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, S::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), S::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: http::Request<B>) -> Self::Future {
        let record = enabled().then(|| {
            let note = Note::default();
            req.extensions_mut().insert(note.clone());
            let path = req.uri().path();
            Record {
                method: path.rsplit('/').next().unwrap_or(path).to_string(),
                wall: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_or(0.0, |d| d.as_secs_f64() * 1000.0),
                start: Instant::now(),
                code: None,
                note,
            }
        });
        let fut = self.inner.call(req);
        Box::pin(async move {
            let resp = fut.await?;
            let mut record = record;
            if let Some(r) = record.as_mut() {
                r.code = grpc_status(resp.headers());
            }
            Ok(resp.map(|body| TimedBody { body, record }))
        })
    }
}

fn grpc_status(h: &http::HeaderMap) -> Option<i32> {
    h.get("grpc-status")?.to_str().ok()?.parse().ok()
}

/// A response body that writes its call's line when it ends or is dropped.
pub struct TimedBody<B> {
    body: B,
    record: Option<Record>,
}

impl<B: Body + Unpin> Body for TimedBody<B> {
    type Data = B::Data;
    type Error = B::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<B::Data>, B::Error>>> {
        let this = &mut *self;
        let out = Pin::new(&mut this.body).poll_frame(cx);
        if let Poll::Ready(ref f) = out {
            if let (Some(Ok(frame)), Some(r)) = (f, this.record.as_mut())
                && let Some(t) = frame.trailers_ref()
            {
                r.code = grpc_status(t).or(r.code);
            }
            if this.body.is_end_stream() || f.is_none() {
                this.record = None;
            }
        }
        out
    }

    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.body.size_hint()
    }
}
