//! spanbolt's gRPC server: Spanner, admin, long-running operations and test controls.

pub mod convert;
pub mod emulator;
pub mod names;
pub mod offload;
mod page;
pub mod rest;
mod services;
pub mod sql;
pub mod status;
mod stream;
pub mod timing;
mod wire;

use std::sync::Arc;
use std::time::Duration;

use spanbolt_proto::control::control_server::ControlServer;
use spanbolt_proto::database_admin::database_admin_server::DatabaseAdminServer;
use spanbolt_proto::instance_admin::instance_admin_server::InstanceAdminServer;
use spanbolt_proto::longrunning::operations_server::OperationsServer;
use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;

pub use emulator::Emulator;
pub use sql::{NoSql, SqlEngine, SqlTxn};

/// Messages up to 100 MiB, like the official emulator accepts.
const MAX_MESSAGE: usize = 100 << 20;

/// Serves every service on `listener` until `shutdown` resolves.
pub async fn serve(
    listener: TcpListener,
    emu: Arc<Emulator>,
    shutdown: impl std::future::Future<Output = ()> + Send,
) -> Result<(), tonic::transport::Error> {
    let sweeper = {
        let emu = emu.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(60));
            loop {
                tick.tick().await;
                emu.sweep();
            }
        })
    };
    let flusher = timing::enabled().then(|| {
        tokio::spawn(async {
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            loop {
                tick.tick().await;
                timing::flush();
            }
        })
    });
    let result = tonic::transport::Server::builder()
        .tcp_nodelay(true)
        .layer(timing::TimingLayer)
        .layer(offload::OffloadLayer)
        .add_service(wire::SpannerRouter::new(
            services::spanner::SpannerService { emu: emu.clone() },
            MAX_MESSAGE,
        ))
        .add_service(
            DatabaseAdminServer::new(services::database_admin::DatabaseAdminService {
                emu: emu.clone(),
            })
            .max_decoding_message_size(MAX_MESSAGE),
        )
        .add_service(InstanceAdminServer::new(
            services::instance_admin::InstanceAdminService { emu: emu.clone() },
        ))
        .add_service(OperationsServer::new(
            services::operations::OperationsService { emu: emu.clone() },
        ))
        .add_service(ControlServer::new(services::control::ControlService {
            emu,
        }))
        .serve_with_incoming_shutdown(TcpListenerStream::new(listener), shutdown)
        .await;
    sweeper.abort();
    if let Some(f) = flusher {
        f.abort();
    }
    timing::flush();
    result
}

/// A server running on a background task; dropping the handle shuts it down.
pub struct Running {
    pub addr: std::net::SocketAddr,
    pub emulator: Arc<Emulator>,
    _shutdown: tokio::sync::oneshot::Sender<()>,
}

/// Starts a server on `addr` (use port 0 for an ephemeral port) inside the current runtime.
pub async fn spawn(addr: &str, emu: Arc<Emulator>) -> std::io::Result<Running> {
    let listener = TcpListener::bind(addr).await?;
    let local = listener.local_addr()?;
    let (tx, rx) = tokio::sync::oneshot::channel();
    tokio::spawn(serve(listener, emu.clone(), async {
        let _ = rx.await;
    }));
    Ok(Running {
        addr: local,
        emulator: emu,
        _shutdown: tx,
    })
}
