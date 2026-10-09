//! Accept loops, per-connection serving and graceful shutdown.
//!
//! One server drives any number of TCP listeners (plain HTTP or TLS, each
//! optionally behind the PROXY protocol) plus an optional QUIC endpoint for
//! HTTP/3. Every request reaches the same [`Handler`] together with a
//! [`ConnInfo`] describing how it arrived.

use crate::{Body, ReqBody, h3, proxy};
use http_body_util::BodyExt;
use hyper::body::Incoming;
use hyper::{Request, Response};
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto;
use hyper_util::server::graceful::{GracefulShutdown, Watcher};
use std::collections::HashMap;
use std::convert::Infallible;
use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Semaphore, watch};

/// How a request reached NOVA.
#[derive(Debug, Clone, Copy)]
pub struct ConnInfo {
    /// The client as seen by NOVA (the PROXY protocol header, when used).
    pub peer: SocketAddr,
    /// The TCP or UDP peer itself (a load balancer behind the PROXY protocol).
    pub socket_peer: SocketAddr,
    pub tls: bool,
    pub http3: bool,
}

/// Application entry point for every request.
pub trait Handler: Send + Sync + 'static {
    fn handle(
        &self,
        req: Request<ReqBody>,
        conn: ConnInfo,
    ) -> impl Future<Output = Response<Body>> + Send;
}

/// TLS settings of a listener.
#[derive(Clone)]
pub struct TlsSettings {
    /// Regular connections (ALPN h2 / http/1.1).
    pub config: Arc<rustls::ServerConfig>,
    /// ACME TLS-ALPN-01 validation handshakes (`acme-tls/1`), when ACME is on.
    pub challenge: Option<Arc<rustls::ServerConfig>>,
}

pub struct Listener {
    pub tcp: TcpListener,
    pub tls: Option<TlsSettings>,
    /// Read a PROXY protocol v1/v2 header before anything else.
    pub proxy_protocol: bool,
}

pub type IpPredicate = Arc<dyn Fn(IpAddr) -> bool + Send + Sync>;

#[derive(Clone)]
pub struct ServerOptions {
    pub max_connections: usize,
    /// Open connections per TCP peer address; 0 = unlimited.
    pub max_connections_per_ip: usize,
    /// Peers never subject to `max_connections_per_ip` (proxies, monitoring).
    pub per_ip_exempt: IpPredicate,
    /// Time allowed for the PROXY header, the TLS handshake and request headers.
    pub header_read_timeout: Duration,
    pub shutdown_grace: Duration,
}

/// Open connections per peer address.
#[derive(Default)]
struct PerIp(Mutex<HashMap<IpAddr, usize>>);

struct PerIpGuard {
    map: Arc<PerIp>,
    ip: IpAddr,
}

impl PerIp {
    fn acquire(self: &Arc<Self>, ip: IpAddr, max: usize) -> Option<PerIpGuard> {
        let mut m = self.0.lock().unwrap();
        let n = m.entry(ip).or_insert(0);
        if max > 0 && *n >= max {
            return None;
        }
        *n += 1;
        Some(PerIpGuard {
            map: Arc::clone(self),
            ip,
        })
    }
}

impl Drop for PerIpGuard {
    fn drop(&mut self) {
        let mut m = self.map.0.lock().unwrap();
        if let Some(n) = m.get_mut(&self.ip) {
            *n -= 1;
            if *n == 0 {
                m.remove(&self.ip);
            }
        }
    }
}

struct Shared<H> {
    handler: Arc<H>,
    builder: auto::Builder<TokioExecutor>,
    limit: Arc<Semaphore>,
    per_ip: Arc<PerIp>,
    opts: ServerOptions,
}

/// Serve until `shutdown` resolves, then stop accepting and give in-flight
/// connections `shutdown_grace` to finish.
pub async fn serve<H: Handler>(
    listeners: Vec<Listener>,
    quic: Option<quinn::Endpoint>,
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
        .max_concurrent_streams(256)
        .keep_alive_interval(Duration::from_secs(30));
    let shared = Arc::new(Shared {
        handler: Arc::clone(&handler),
        builder,
        limit: Arc::new(Semaphore::new(opts.max_connections)),
        per_ip: Arc::new(PerIp::default()),
        opts: opts.clone(),
    });
    let graceful = GracefulShutdown::new();
    let (stop_tx, stop_rx) = watch::channel(false);
    let h3_task = quic.map(|ep| {
        tokio::spawn(h3::serve(
            ep,
            Arc::clone(&handler),
            Arc::clone(&shared.limit),
            stop_rx.clone(),
            opts.shutdown_grace,
        ))
    });

    // Accept loops borrow `graceful` (each connection gets a watcher) and
    // end once `stop` flips.
    let loops = futures_util::future::join_all(
        listeners
            .into_iter()
            .map(|l| accept_loop(l, &shared, &graceful, stop_rx.clone())),
    );
    tokio::join!(
        async {
            shutdown.await;
            let _ = stop_tx.send(true);
        },
        loops
    );
    tracing::info!(
        grace_secs = opts.shutdown_grace.as_secs(),
        "stopped accepting; draining connections"
    );
    let drain = async {
        graceful.shutdown().await;
        if let Some(t) = h3_task {
            let _ = t.await;
        }
    };
    match tokio::time::timeout(opts.shutdown_grace, drain).await {
        Ok(()) => tracing::info!("all connections closed"),
        Err(_) => tracing::warn!("grace period elapsed; dropping remaining connections"),
    }
    Ok(())
}

async fn accept_loop<H: Handler>(
    listener: Listener,
    shared: &Arc<Shared<H>>,
    graceful: &GracefulShutdown,
    mut stop: watch::Receiver<bool>,
) {
    loop {
        let permit = tokio::select! {
            _ = stop.changed() => break,
            p = Arc::clone(&shared.limit).acquire_owned() => p.expect("semaphore never closed"),
        };
        let (stream, peer) = tokio::select! {
            _ = stop.changed() => break,
            accepted = listener.tcp.accept() => match accepted {
                Ok(a) => a,
                Err(e) => {
                    // EMFILE and friends: back off instead of spinning.
                    tracing::warn!(error = %e, "accept failed");
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                }
            },
        };
        let guard = if (shared.opts.per_ip_exempt)(peer.ip()) {
            None
        } else {
            match shared
                .per_ip
                .acquire(peer.ip(), shared.opts.max_connections_per_ip)
            {
                Some(g) => Some(g),
                None => {
                    tracing::debug!(%peer, "per-IP connection limit reached");
                    continue;
                }
            }
        };
        let _ = stream.set_nodelay(true);
        let shared2 = Arc::clone(shared);
        let tls = listener.tls.clone();
        let proxy_protocol = listener.proxy_protocol;
        let watcher = graceful.watcher();
        tokio::spawn(async move {
            handle_connection(stream, peer, tls, proxy_protocol, shared2, watcher).await;
            drop(guard);
            drop(permit);
        });
    }
}

async fn handle_connection<H: Handler>(
    mut stream: TcpStream,
    socket_peer: SocketAddr,
    tls: Option<TlsSettings>,
    proxy_protocol: bool,
    shared: Arc<Shared<H>>,
    watcher: Watcher,
) {
    let timeout = shared.opts.header_read_timeout;
    let mut peer = socket_peer;
    if proxy_protocol {
        match tokio::time::timeout(timeout, proxy::read_header(&mut stream)).await {
            Ok(Ok(Some(addr))) => peer = addr,
            Ok(Ok(None)) => {}
            Ok(Err(e)) => {
                tracing::debug!(%socket_peer, error = %e, "invalid PROXY protocol header");
                return;
            }
            Err(_) => return,
        }
    }
    let Some(tls) = tls else {
        let info = ConnInfo {
            peer,
            socket_peer,
            tls: false,
            http3: false,
        };
        serve_io(stream, info, shared, watcher).await;
        return;
    };

    let handshake = async {
        let start =
            tokio_rustls::LazyConfigAcceptor::new(rustls::server::Acceptor::default(), stream)
                .await?;
        let challenge = start
            .client_hello()
            .alpn()
            .is_some_and(|mut a| a.next() == Some(ACME_TLS_ALPN) && a.next().is_none());
        if challenge && let Some(cfg) = &tls.challenge {
            // ACME validation: complete the handshake, then hang up.
            let mut s = start.into_stream(Arc::clone(cfg)).await?;
            let _ = tokio::io::AsyncWriteExt::shutdown(&mut s).await;
            return Ok(None);
        }
        start.into_stream(Arc::clone(&tls.config)).await.map(Some)
    };
    match tokio::time::timeout(timeout, handshake).await {
        Ok(Ok(Some(s))) => {
            let info = ConnInfo {
                peer,
                socket_peer,
                tls: true,
                http3: false,
            };
            serve_io(s, info, shared, watcher).await;
        }
        Ok(Ok(None)) => {}
        Ok(Err(e)) => tracing::debug!(%peer, error = %e, "TLS handshake failed"),
        Err(_) => tracing::debug!(%peer, "TLS handshake timed out"),
    }
}

const ACME_TLS_ALPN: &[u8] = b"acme-tls/1";

async fn serve_io<H, I>(io: I, info: ConnInfo, shared: Arc<Shared<H>>, watcher: Watcher)
where
    H: Handler,
    I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let h = Arc::clone(&shared.handler);
    let svc = hyper::service::service_fn(move |req: Request<Incoming>| {
        let h = Arc::clone(&h);
        async move {
            let req = req.map(|b| b.map_err(std::io::Error::other).boxed());
            Ok::<_, Infallible>(h.handle(req, info).await)
        }
    });
    let conn = shared
        .builder
        .serve_connection_with_upgrades(TokioIo::new(io), svc)
        .into_owned();
    let conn = watcher.watch(conn);
    if let Err(e) = conn.await {
        tracing::debug!(peer = %info.peer, error = %e, "connection ended with error");
    }
}
