//! Runs RPC handlers on tokio's blocking pool instead of the runtime's workers.
//!
//! Handlers do their work synchronously: analysing and running SQL, applying DDL, committing. On a
//! worker thread a long query holds that worker, and a few of them at once hold them all. Then
//! nothing else on the runtime runs: not cheap RPCs, not the accept loop, not the HTTP/2 handshake
//! of a new connection, which waits for the server's SETTINGS frame. Clients see that as a
//! connection that never becomes ready. On the blocking pool every call has a thread of its own,
//! so a long query delays only itself and the workers stay free for I/O.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::runtime::Handle;
use tonic::codegen::http;

/// Runs `fut` to completion on a blocking-pool thread. Dropping the returned future drops `fut`
/// at its next await, as it would have been dropped on a worker.
pub async fn run<F>(fut: F) -> F::Output
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    let handle = Handle::current();
    // Dropped with this future, which wakes `cancelled` and drops `fut`.
    let (_alive, cancelled) = tokio::sync::oneshot::channel::<()>();
    let task = tokio::task::spawn_blocking(move || {
        handle.block_on(async move {
            tokio::select! {
                biased;
                out = fut => Some(out),
                _ = cancelled => None,
            }
        })
    });
    match task.await {
        Ok(Some(out)) => out,
        Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
        // The runtime is shutting down (the task never started) or this future is being
        // dropped: either way nobody is waiting for the result.
        Ok(None) | Err(_) => std::future::pending().await,
    }
}

/// The tower layer that runs every call's handler through [`run`]. The response body is still
/// polled on the workers; streaming bodies are fed from blocking threads already (`stream`).
#[derive(Clone, Copy, Default)]
pub struct OffloadLayer;

impl<S> tower_layer::Layer<S> for OffloadLayer {
    type Service = Offload<S>;

    fn layer(&self, inner: S) -> Offload<S> {
        Offload { inner }
    }
}

#[derive(Clone)]
pub struct Offload<S> {
    inner: S,
}

impl<S, B> tonic::codegen::Service<http::Request<B>> for Offload<S>
where
    S: tonic::codegen::Service<http::Request<B>>,
    S::Future: Send + 'static,
    S::Response: Send + 'static,
    S::Error: Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<S::Response, S::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), S::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: http::Request<B>) -> Self::Future {
        Box::pin(run(self.inner.call(req)))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    use super::run;

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn a_blocking_call_leaves_the_worker_free() {
        let slow = tokio::spawn(run(async {
            std::thread::sleep(Duration::from_millis(500));
        }));
        // With one worker, this would wait for the sleep if the call ran on the worker.
        tokio::task::yield_now().await;
        let t = Instant::now();
        tokio::spawn(async {}).await.unwrap();
        assert!(
            t.elapsed() < Duration::from_millis(250),
            "{:?}",
            t.elapsed()
        );
        slow.await.unwrap();
    }

    #[tokio::test]
    async fn dropping_the_call_drops_the_handler() {
        struct Flag(Arc<AtomicBool>);
        impl Drop for Flag {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let dropped = Arc::new(AtomicBool::new(false));
        let flag = Flag(dropped.clone());
        let call = tokio::spawn(run(async move {
            let _flag = flag;
            std::future::pending::<()>().await;
        }));
        tokio::time::sleep(Duration::from_millis(50)).await;
        call.abort();
        let t = Instant::now();
        while !dropped.load(Ordering::SeqCst) {
            assert!(
                t.elapsed() < Duration::from_secs(5),
                "handler never dropped"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn returns_the_output() {
        assert_eq!(run(async { 7 }).await, 7);
    }
}
