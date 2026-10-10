//! HTTP/3 over QUIC (quinn + h3), feeding the same [`Handler`] as TCP.

use crate::server::{ConnInfo, Handler, PerIp, ServerOptions};
use bytes::{Buf, Bytes};
use http::{Request, Response};
use http_body_util::BodyExt;
use hyper::body::Frame;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Semaphore, watch};
use tokio_util::task::TaskTracker;

/// Largest request header section accepted (hyper's HTTP/2 default is 16 KiB).
const MAX_FIELD_SECTION: u64 = 64 * 1024;

/// The connection limits shared with the TCP listeners.
pub(crate) struct Limits {
    pub total: Arc<Semaphore>,
    pub per_ip: Arc<PerIp>,
    pub opts: ServerOptions,
}

/// Accept QUIC connections until `stop` flips, then send GOAWAY and wait
/// (up to the shutdown grace) for open requests.
pub(crate) async fn serve<H: Handler>(
    endpoint: quinn::Endpoint,
    handler: Arc<H>,
    limits: Limits,
    mut stop: watch::Receiver<bool>,
) {
    let tracker = TaskTracker::new();
    let opts = &limits.opts;
    loop {
        let incoming = tokio::select! {
            _ = stop.changed() => break,
            i = endpoint.accept() => match i {
                Some(i) => i,
                None => break,
            },
        };
        let ip = incoming.remote_address().ip();
        let max = opts.max_connections_per_ip;
        let exempt = max == 0 || (opts.per_ip_exempt)(ip);
        // A UDP source address is trivially spoofed. Once an address holds
        // half its quota, make it prove it is real (a stateless Retry round
        // trip) before it may take more, so forged packets cannot lock a
        // victim address out.
        if !exempt && !incoming.remote_address_validated() && limits.per_ip.count(ip) >= max / 2 {
            if let Err(e) = incoming.retry() {
                e.into_incoming().refuse();
            }
            continue;
        }
        let guard = if exempt {
            None
        } else {
            match limits.per_ip.acquire(ip, max) {
                Some(g) => Some(g),
                None => {
                    tracing::debug!(peer = %incoming.remote_address(), "per-IP connection limit reached (QUIC)");
                    incoming.refuse();
                    continue;
                }
            }
        };
        let Ok(permit) = Arc::clone(&limits.total).try_acquire_owned() else {
            incoming.refuse();
            continue;
        };
        let handler = Arc::clone(&handler);
        let stop = stop.clone();
        let handshake_timeout = opts.header_read_timeout;
        tracker.spawn(async move {
            connection(incoming, handler, stop, handshake_timeout).await;
            drop(guard);
            drop(permit);
        });
    }
    tracker.close();
    let _ = tokio::time::timeout(opts.shutdown_grace, tracker.wait()).await;
    endpoint.close(0u32.into(), b"shutting down");
    let _ = tokio::time::timeout(Duration::from_secs(1), endpoint.wait_idle()).await;
}

async fn connection<H: Handler>(
    incoming: quinn::Incoming,
    handler: Arc<H>,
    mut stop: watch::Receiver<bool>,
    handshake_timeout: Duration,
) {
    // Same budget as a TLS handshake on TCP (`header_read_timeout`).
    let setup = async {
        let conn = incoming.await.map_err(|e| e.to_string())?;
        let peer = conn.remote_address();
        let mut builder = ::h3::server::builder();
        builder.max_field_section_size(MAX_FIELD_SECTION);
        let h3c = builder
            .build::<_, Bytes>(h3_quinn::Connection::new(conn))
            .await
            .map_err(|e| e.to_string())?;
        Ok::<_, String>((peer, h3c))
    };
    let (peer, mut h3c) = match tokio::time::timeout(handshake_timeout, setup).await {
        Ok(Ok(c)) => c,
        Ok(Err(e)) => {
            tracing::debug!(error = %e, "QUIC/HTTP/3 setup failed");
            return;
        }
        Err(_) => {
            tracing::debug!("QUIC handshake timed out");
            return;
        }
    };
    let requests = TaskTracker::new();
    let mut draining = *stop.borrow();
    if draining {
        let _ = h3c.shutdown(0).await;
    }
    loop {
        let accepted = tokio::select! {
            changed = stop.changed(), if !draining => {
                draining = true;
                if changed.is_ok() {
                    // GOAWAY: finish open requests, accept no new ones.
                    let _ = h3c.shutdown(0).await;
                }
                continue;
            }
            a = h3c.accept() => a,
        };
        match accepted {
            Ok(Some(resolver)) => {
                let handler = Arc::clone(&handler);
                requests.spawn(async move {
                    match resolver.resolve_request().await {
                        Ok((req, stream)) => request(req, stream, handler, peer).await,
                        Err(e) => tracing::debug!(%peer, error = %e, "bad HTTP/3 request"),
                    }
                });
            }
            Ok(None) => break,
            Err(e) => {
                if !e.is_h3_no_error() {
                    tracing::debug!(%peer, error = %e, "HTTP/3 connection ended");
                }
                break;
            }
        }
    }
    requests.close();
    requests.wait().await;
}

async fn request<H: Handler>(
    req: Request<()>,
    stream: ::h3::server::RequestStream<h3_quinn::BidiStream<Bytes>, Bytes>,
    handler: Arc<H>,
    peer: SocketAddr,
) {
    let (mut send, mut recv) = stream.split();
    // The request body is pumped through a channel so the handler gets a
    // `Sync` body like on TCP.
    let (tx, rx) = tokio::sync::mpsc::channel::<std::io::Result<Bytes>>(8);
    tokio::spawn(async move {
        loop {
            let item = match recv.recv_data().await {
                Ok(Some(mut buf)) => Ok(buf.copy_to_bytes(buf.remaining())),
                Ok(None) => return,
                Err(e) => Err(std::io::Error::other(e)),
            };
            let failed = item.is_err();
            if tx.send(item).await.is_err() || failed {
                return;
            }
        }
    });
    let req = req.map(|()| ChannelBody(rx).boxed());
    let info = ConnInfo {
        peer,
        socket_peer: peer,
        tls: true,
        http3: true,
    };
    let resp = handler.handle(req, info).await;
    let (mut parts, mut body) = resp.into_parts();
    // Connection-specific headers are not allowed in HTTP/3 (RFC 9114 §4.2).
    for h in [
        "connection",
        "keep-alive",
        "transfer-encoding",
        "upgrade",
        "proxy-connection",
    ] {
        parts.headers.remove(h);
    }
    if let Err(e) = send.send_response(Response::from_parts(parts, ())).await {
        tracing::debug!(%peer, error = %e, "HTTP/3 response failed");
        return;
    }
    while let Some(frame) = body.frame().await {
        match frame {
            Ok(f) => {
                if let Ok(data) = f.into_data()
                    && !data.is_empty()
                    && send.send_data(data).await.is_err()
                {
                    return;
                }
            }
            Err(e) => {
                tracing::debug!(%peer, error = %e, "response body failed");
                send.stop_stream(::h3::error::Code::H3_INTERNAL_ERROR);
                return;
            }
        }
    }
    let _ = send.finish().await;
}

/// Request body fed by the stream-reading task.
struct ChannelBody(tokio::sync::mpsc::Receiver<std::io::Result<Bytes>>);

impl hyper::body::Body for ChannelBody {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<Frame<Bytes>, std::io::Error>>> {
        self.0
            .poll_recv(cx)
            .map(|item| item.map(|r| r.map(Frame::data)))
    }
}
