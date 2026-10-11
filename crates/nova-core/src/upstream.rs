//! Reverse proxy to a site's application server (`[site.proxy]`).
//!
//! HTTP/1.1 to the upstream over a shared keep-alive pool, request and
//! response bodies streamed, WebSocket upgrades tunneled. The upstream
//! sees the client NOVA already validated (`X-Forwarded-For/-Proto/-Host`
//! are replaced, never passed through from the client), and hop-by-hop
//! headers stay on their own hop.

use crate::client::Client as Peer;
use http::{
    HeaderMap, HeaderName, HeaderValue, Request, Response, StatusCode, Uri, Version, header,
};
use http_body_util::BodyExt;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::{TokioExecutor, TokioIo};
use nova_http::{Body, ReqBody};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

pub type HttpClient = Client<HttpConnector, ReqBody>;

/// One pool for all sites (keyed by upstream host:port internally).
pub fn http_client() -> HttpClient {
    let mut connector = HttpConnector::new();
    connector.set_nodelay(true);
    connector.set_connect_timeout(Some(Duration::from_secs(5)));
    Client::builder(TokioExecutor::new())
        .pool_idle_timeout(Duration::from_secs(30))
        .pool_max_idle_per_host(64)
        .build(connector)
}

#[derive(Debug)]
pub enum ProxyError {
    /// Connection refused, reset, invalid response...
    Unavailable(String),
    Timeout,
}

/// Headers that describe one hop and must not be forwarded.
const HOP_BY_HOP: [&str; 8] = [
    "connection",
    "keep-alive",
    "proxy-connection",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
];

fn strip_hop_by_hop(h: &mut HeaderMap, keep_upgrade: bool) {
    // Plus every header the Connection header names.
    let named: Vec<HeaderName> = h
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .filter_map(|n| HeaderName::from_bytes(n.trim().as_bytes()).ok())
        .collect();
    for n in named {
        if !(keep_upgrade && n == header::UPGRADE) {
            h.remove(n);
        }
    }
    for n in HOP_BY_HOP {
        h.remove(n);
    }
    if !keep_upgrade {
        h.remove(header::UPGRADE);
    }
}

fn is_websocket<B>(req: &Request<B>) -> bool {
    req.version() <= Version::HTTP_11
        && req
            .headers()
            .get(header::UPGRADE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.eq_ignore_ascii_case("websocket"))
}

/// The request as the upstream should see it.
fn upstream_request(
    base: &str,
    req: Request<ReqBody>,
    peer: Peer,
    websocket: bool,
) -> Result<Request<ReqBody>, ProxyError> {
    let (parts, body) = req.into_parts();
    let path = parts
        .uri
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or("/");
    let uri: Uri = format!("{base}{path}")
        .parse()
        .map_err(|e| ProxyError::Unavailable(format!("bad upstream URI: {e}")))?;
    let mut headers = parts.headers;
    strip_hop_by_hop(&mut headers, websocket);
    // Apps build absolute URLs from Host: keep the client's (HTTP/2 and
    // HTTP/3 carry it as :authority).
    let host = headers.get(header::HOST).cloned().or_else(|| {
        parts
            .uri
            .authority()
            .and_then(|a| HeaderValue::from_str(a.as_str()).ok())
    });
    for n in [
        "forwarded",
        "x-forwarded-for",
        "x-forwarded-proto",
        "x-forwarded-host",
        "x-real-ip",
    ] {
        headers.remove(n);
    }
    let ip = HeaderValue::from_str(&peer.ip.to_string()).expect("an IP is a valid header");
    headers.insert("x-forwarded-for", ip.clone());
    headers.insert("x-real-ip", ip);
    headers.insert(
        "x-forwarded-proto",
        HeaderValue::from_static(if peer.https { "https" } else { "http" }),
    );
    if let Some(h) = host {
        headers.insert("x-forwarded-host", h.clone());
        headers.insert(header::HOST, h);
    }
    if websocket {
        headers.insert(header::CONNECTION, HeaderValue::from_static("upgrade"));
        headers.insert(header::UPGRADE, HeaderValue::from_static("websocket"));
    }
    let mut out = Request::new(body);
    *out.method_mut() = parts.method;
    *out.uri_mut() = uri;
    *out.version_mut() = Version::HTTP_11;
    *out.headers_mut() = headers;
    Ok(out)
}

/// Forward `req` to `base` (`http://host:port`).
pub async fn forward(
    client: &HttpClient,
    base: &str,
    mut req: Request<ReqBody>,
    peer: Peer,
    timeout: Duration,
) -> Result<Response<Body>, ProxyError> {
    let websocket = is_websocket(&req);
    let downstream = websocket.then(|| hyper::upgrade::on(&mut req));
    let out = upstream_request(base, req, peer, websocket)?;
    let mut resp = match tokio::time::timeout(timeout, client.request(out)).await {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => return Err(ProxyError::Unavailable(e.to_string())),
        Err(_) => return Err(ProxyError::Timeout),
    };
    if let Some(downstream) = downstream
        && resp.status() == StatusCode::SWITCHING_PROTOCOLS
    {
        let upstream = hyper::upgrade::on(&mut resp);
        tokio::spawn(async move {
            if let (Ok(d), Ok(u)) = tokio::join!(downstream, upstream) {
                let (mut d, mut u) = (TokioIo::new(d), TokioIo::new(u));
                let _ = tokio::io::copy_bidirectional(&mut d, &mut u).await;
            }
        });
        let (mut parts, _) = resp.into_parts();
        strip_hop_by_hop(&mut parts.headers, true);
        parts
            .headers
            .insert(header::CONNECTION, HeaderValue::from_static("upgrade"));
        return Ok(Response::from_parts(parts, nova_http::empty()));
    }
    let (mut parts, body) = resp.into_parts();
    strip_hop_by_hop(&mut parts.headers, false);
    Ok(Response::from_parts(
        parts,
        body.map_err(std::io::Error::other).boxed_unsync(),
    ))
}

/// The upstreams of one site's `[site.proxy]` and their health.
#[derive(Debug)]
pub struct Balancer {
    backends: Vec<Backend>,
    balance: nova_config::Balance,
    fail_timeout: Duration,
    next: AtomicUsize,
}

#[derive(Debug)]
pub struct Backend {
    pub base: String,
    in_flight: AtomicUsize,
    /// Not chosen before this instant (failed connection or health check).
    down_until: Mutex<Option<Instant>>,
}

impl Backend {
    /// Not resting after a failure or a failed health check.
    pub fn is_up(&self) -> bool {
        Balancer::up(self, Instant::now())
    }

    pub fn in_flight(&self) -> usize {
        self.in_flight.load(Ordering::Relaxed)
    }
}

/// A chosen upstream; counts as in flight until dropped.
pub struct Lease<'a> {
    pub backend: &'a Backend,
    balancer: &'a Balancer,
}

impl Drop for Lease<'_> {
    fn drop(&mut self) {
        self.backend.in_flight.fetch_sub(1, Ordering::Relaxed);
    }
}

impl Lease<'_> {
    /// The upstream could not be reached: rest it for `fail_timeout`.
    pub fn failed(&self) {
        self.balancer
            .mark_down(self.backend, self.balancer.fail_timeout);
    }
}

impl Balancer {
    pub fn new(p: &nova_config::SiteProxy) -> Self {
        Balancer {
            backends: p
                .upstream_list()
                .into_iter()
                .map(|u| Backend {
                    base: u.trim_end_matches('/').to_string(),
                    in_flight: AtomicUsize::new(0),
                    down_until: Mutex::new(None),
                })
                .collect(),
            balance: p.balance,
            fail_timeout: Duration::from_secs(p.fail_timeout_secs.max(1)),
            next: AtomicUsize::new(0),
        }
    }

    pub fn backends(&self) -> &[Backend] {
        &self.backends
    }

    fn up(b: &Backend, now: Instant) -> bool {
        b.down_until.lock().unwrap().is_none_or(|t| now >= t)
    }

    fn mark_down(&self, b: &Backend, for_: Duration) {
        *b.down_until.lock().unwrap() = Some(Instant::now() + for_);
    }

    /// Bring an upstream back (its health check passed).
    pub fn mark_up(&self, b: &Backend) {
        *b.down_until.lock().unwrap() = None;
    }

    /// Choose an upstream, skipping `exclude` (an index already tried).
    /// When none is up, the one that went down first is tried anyway:
    /// better a request that may fail than none at all.
    pub fn pick(&self, exclude: Option<usize>) -> Option<(usize, Lease<'_>)> {
        let n = self.backends.len();
        let now = Instant::now();
        let start = self.next.fetch_add(1, Ordering::Relaxed);
        let candidates = (0..n)
            .map(|i| (start + i) % n)
            .filter(|&i| Some(i) != exclude);
        let up: Vec<usize> = candidates
            .clone()
            .filter(|&i| Self::up(&self.backends[i], now))
            .collect();
        let chosen = if up.is_empty() {
            candidates.min_by_key(|&i| *self.backends[i].down_until.lock().unwrap())?
        } else {
            match self.balance {
                nova_config::Balance::RoundRobin => up[0],
                // Fewest in flight; ties go round-robin (iteration order).
                nova_config::Balance::LeastConn => *up
                    .iter()
                    .min_by_key(|&&i| self.backends[i].in_flight.load(Ordering::Relaxed))
                    .unwrap(),
            }
        };
        let backend = &self.backends[chosen];
        backend.in_flight.fetch_add(1, Ordering::Relaxed);
        Some((
            chosen,
            Lease {
                backend,
                balancer: self,
            },
        ))
    }

    /// Active health checks (`health_path`) until the process ends.
    pub async fn check_health(self: std::sync::Arc<Self>, path: String, every: Duration) {
        let client = http_client();
        loop {
            for b in &self.backends {
                let ok = async {
                    let req = Request::get(format!("{}{path}", b.base))
                        .header(header::USER_AGENT, "nova-health-check")
                        .body(http_body_util::Empty::new().map_err(|n| match n {}).boxed())
                        .ok()?;
                    let resp = tokio::time::timeout(Duration::from_secs(2), client.request(req))
                        .await
                        .ok()?
                        .ok()?;
                    Some(!resp.status().is_server_error())
                }
                .await
                .unwrap_or(false);
                let was_up = Self::up(b, Instant::now());
                if ok {
                    self.mark_up(b);
                } else {
                    // Down until the next check that passes.
                    self.mark_down(b, Duration::from_secs(3600));
                }
                if ok != was_up {
                    tracing::warn!(upstream = b.base, healthy = ok, "upstream health changed");
                }
            }
            tokio::time::sleep(every).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use http_body_util::Full;

    fn peer() -> Peer {
        Peer {
            ip: "198.51.100.7".parse().unwrap(),
            https: true,
            proxied: false,
        }
    }

    fn req(headers: &[(&str, &str)]) -> Request<ReqBody> {
        let mut b = Request::builder().uri("/api/items?x=1");
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        b.body(
            Full::new(Bytes::new())
                .map_err(|never| match never {})
                .boxed(),
        )
        .unwrap()
    }

    #[test]
    fn rewrites_forwarding_and_hop_headers() {
        let r = req(&[
            ("host", "shop.example.com"),
            ("x-forwarded-for", "6.6.6.6"),
            ("forwarded", "for=6.6.6.6"),
            ("connection", "keep-alive, x-secret"),
            ("x-secret", "drop me"),
            ("te", "trailers"),
            ("accept", "text/html"),
        ]);
        let out = upstream_request("http://127.0.0.1:3000", r, peer(), false).unwrap();
        assert_eq!(out.uri(), "http://127.0.0.1:3000/api/items?x=1");
        let h = out.headers();
        assert_eq!(h["host"], "shop.example.com");
        assert_eq!(h["x-forwarded-for"], "198.51.100.7");
        assert_eq!(h["x-forwarded-proto"], "https");
        assert_eq!(h["x-forwarded-host"], "shop.example.com");
        assert_eq!(h["accept"], "text/html");
        for gone in ["forwarded", "connection", "x-secret", "te"] {
            assert!(!h.contains_key(gone), "{gone} forwarded");
        }
    }

    #[test]
    fn websocket_upgrade_headers_survive() {
        let r = req(&[
            ("host", "a"),
            ("connection", "Upgrade"),
            ("upgrade", "websocket"),
            ("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ=="),
        ]);
        assert!(is_websocket(&r));
        let out = upstream_request("http://u:1", r, peer(), true).unwrap();
        assert_eq!(out.headers()["upgrade"], "websocket");
        assert_eq!(out.headers()["connection"], "upgrade");
        assert!(out.headers().contains_key("sec-websocket-key"));
    }

    /// End to end against a real (in-process) upstream: status, headers,
    /// streamed body, hop-by-hop response headers removed, and errors.
    #[tokio::test]
    async fn forwards_to_a_live_upstream() {
        use hyper::service::service_fn;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let (s, _) = listener.accept().await.unwrap();
                tokio::spawn(async move {
                    let svc = service_fn(|r: Request<hyper::body::Incoming>| async move {
                        let echo = format!(
                            "{} {} xff={}",
                            r.method(),
                            r.uri(),
                            r.headers()["x-forwarded-for"].to_str().unwrap()
                        );
                        Ok::<_, std::convert::Infallible>(
                            Response::builder()
                                .status(201)
                                .header("x-app", "node")
                                .header("keep-alive", "timeout=5")
                                .body(Full::new(Bytes::from(echo)))
                                .unwrap(),
                        )
                    });
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(s), svc)
                        .await;
                });
            }
        });
        let client = http_client();
        let base = format!("http://{addr}");
        let resp = forward(
            &client,
            &base,
            req(&[("host", "a")]),
            peer(),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        assert_eq!(resp.status(), 201);
        assert_eq!(resp.headers()["x-app"], "node");
        assert!(!resp.headers().contains_key("keep-alive"));
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(body, "GET /api/items?x=1 xff=198.51.100.7");

        // Nothing listening: Unavailable, not a hang.
        let closed = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dead = format!("http://{}", closed.local_addr().unwrap());
        drop(closed);
        let err = forward(&client, &dead, req(&[]), peer(), Duration::from_secs(5)).await;
        assert!(matches!(err, Err(ProxyError::Unavailable(_))));
    }

    fn proxy(ups: &[&str], balance: nova_config::Balance) -> nova_config::SiteProxy {
        nova_config::SiteProxy {
            upstream: String::new(),
            upstreams: ups.iter().map(|u| u.to_string()).collect(),
            balance,
            health_path: None,
            health_interval_secs: 5,
            fail_timeout_secs: 10,
            paths: vec!["/**".into()],
            static_first: true,
            timeout_secs: 60,
        }
    }

    #[test]
    fn balancing_and_failover() {
        use nova_config::Balance;
        let b = Balancer::new(&proxy(
            &["http://a:1", "http://b:1", "http://c:1"],
            Balance::LeastConn,
        ));
        // Three requests in flight land on three different upstreams.
        let l1 = b.pick(None).unwrap();
        let l2 = b.pick(None).unwrap();
        let l3 = b.pick(None).unwrap();
        let mut used = [l1.0, l2.0, l3.0];
        used.sort();
        assert_eq!(used, [0, 1, 2]);
        // b is busier: the next request avoids it.
        drop(l1);
        drop(l3);
        let next = b.pick(None).unwrap();
        assert_ne!(next.0, l2.0);
        drop((l2, next));

        // A failed upstream is skipped, and never chosen for a retry.
        let (i, lease) = b.pick(None).unwrap();
        lease.failed();
        drop(lease);
        for _ in 0..10 {
            let (j, _l) = b.pick(None).unwrap();
            assert_ne!(i, j);
        }
        let (k, _l) = b.pick(Some((i + 1) % 3)).unwrap();
        assert!(k != i && k != (i + 1) % 3);

        // All down: still try one (the earliest to fail) instead of none.
        for bk in b.backends() {
            b.mark_down(bk, Duration::from_secs(60));
        }
        assert!(b.pick(None).is_some());

        // Round robin cycles through all.
        let rr = Balancer::new(&proxy(&["http://a:1", "http://b:1"], Balance::RoundRobin));
        let a = rr.pick(None).unwrap().0;
        let c = rr.pick(None).unwrap().0;
        assert_ne!(a, c);
    }
}
