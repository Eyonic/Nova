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
use std::time::Duration;

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
}
