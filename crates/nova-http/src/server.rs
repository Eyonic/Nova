//! Accept loop, per-connection serving and graceful shutdown.

use crate::Body;
use hyper::body::Incoming;
use hyper::{Request, Response};
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto;
use hyper_util::server::graceful::GracefulShutdown;
use std::convert::Infallible;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::Semaphore;

/// Application entry point for every request.
pub trait Handler: Send + Sync + 'static {
    fn handle(
        &self,
        req: Request<Incoming>,
        peer: SocketAddr,
    ) -> impl Future<Output = Response<Body>> + Send;
}

#[derive(Debug, Clone)]
pub struct ServerOptions {
    pub max_connections: usize,
    pub header_read_timeout: Duration,
    pub shutdown_grace: Duration,
}

/// Serve until `shutdown` resolves, then stop accepting and give in-flight
/// connections `shutdown_grace` to finish.
pub async fn serve<H: Handler>(
    listener: TcpListener,
    opts: ServerOptions,
    handler: Arc<H>,
    shutdown: impl Future<Output = ()>,
) -> std::io::Result<()> {
    let mut builder = auto::Builder::new(TokioExecutor::new());
    builder
        .http1()
        .timer(TokioTimer::new())
        .header_read_timeout(opts.header_read_timeout)
        .keep_alive(true);
    builder
        .http2()
        .timer(TokioTimer::new())
        .max_concurrent_streams(256);
    let builder = Arc::new(builder);
    let graceful = GracefulShutdown::new();
    let limit = Arc::new(Semaphore::new(opts.max_connections));
    tokio::pin!(shutdown);

    loop {
        let permit = tokio::select! {
            _ = &mut shutdown => break,
            p = Arc::clone(&limit).acquire_owned() => p.expect("semaphore never closed"),
        };
        let (stream, peer) = tokio::select! {
            _ = &mut shutdown => break,
            accepted = listener.accept() => match accepted {
                Ok(a) => a,
                Err(e) => {
                    // EMFILE and friends: back off instead of spinning.
                    tracing::warn!(error = %e, "accept failed");
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                }
            },
        };
        let _ = stream.set_nodelay(true);
        let h = Arc::clone(&handler);
        let svc = hyper::service::service_fn(move |req| {
            let h = Arc::clone(&h);
            async move { Ok::<_, Infallible>(h.handle(req, peer).await) }
        });
        let conn = builder
            .serve_connection_with_upgrades(TokioIo::new(stream), svc)
            .into_owned();
        let conn = graceful.watch(conn);
        tokio::spawn(async move {
            if let Err(e) = conn.await {
                tracing::debug!(%peer, error = %e, "connection ended with error");
            }
            drop(permit);
        });
    }

    drop(listener);
    tracing::info!(
        grace_secs = opts.shutdown_grace.as_secs(),
        "stopped accepting; draining connections"
    );
    match tokio::time::timeout(opts.shutdown_grace, graceful.shutdown()).await {
        Ok(()) => tracing::info!("all connections closed"),
        Err(_) => tracing::warn!("grace period elapsed; dropping remaining connections"),
    }
    Ok(())
}
