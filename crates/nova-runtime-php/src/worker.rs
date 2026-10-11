//! Worker mode, HTTP side: requests go to long-running PHP workers that
//! keep the application booted between requests (Laravel Octane, Symfony
//! Runtime, Spiral, ... through their RoadRunner integration, or
//! FrankenPHP-style workers on NOVA's PHP extension).
//!
//! NOVA listens on a per-site socket; the site's worker manager (`nova
//! php-workers`, running as the site inside its sandbox) connects a few
//! times. Requests are multiplexed over those connections, each frame
//! tagged with the request id (second goridge option): NOVA sends a
//! request the moment it arrives, the manager queues it and hands it to
//! the next free PHP worker without another round trip. NOVA's timeout
//! sends `{"cancel":true}`, which drops the request from the queue or
//! replaces the worker stuck on it.

use crate::goridge::{self, Frame};
use crate::{PhpError, PhpResponse};
use bytes::Bytes;
use http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::UnixListener;
use tokio::net::unix::OwnedWriteHalf;
use tokio::sync::{Notify, mpsc, oneshot};
use tokio::time::Instant;

/// Largest response frame accepted from PHP (bigger bodies should stream).
const MAX_FRAME: usize = 256 << 20;

pub struct WorkerPool {
    socket: PathBuf,
    /// Only connections from these uids are workers (empty: anyone who
    /// can reach the socket).
    peer_uids: Vec<u32>,
    expose_errors: bool,
    /// RoadRunner workers expect urlencoded forms parsed by the server;
    /// NOVA's own workers parse them in PHP and keep the raw body.
    parse_urlencoded: bool,
    /// The worker uses NOVA's PHP extension (`frankenphp_handle_request`):
    /// CGI parameters in, CGI response out.
    fastcgi: bool,
    site: String,
    links: Mutex<Vec<Arc<Link>>>,
    /// Signalled when a manager connects.
    linked: Notify,
    next_id: AtomicU32,
    next_link: AtomicUsize,
    in_flight: AtomicUsize,
    listener: Mutex<Option<tokio::task::JoinHandle<()>>>,
    pub served: AtomicU64,
}

/// One connection from the manager.
struct Link {
    wr: tokio::sync::Mutex<OwnedWriteHalf>,
    /// Requests sent on this link, by id: where their frames go.
    pending: Mutex<HashMap<u32, mpsc::UnboundedSender<Frame>>>,
    closed: AtomicBool,
}

impl Link {
    async fn send(&self, f: &Frame) -> io::Result<()> {
        f.write_to(&mut *self.wr.lock().await).await
    }
}

impl std::fmt::Debug for WorkerPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkerPool")
            .field("socket", &self.socket)
            .field("in_flight", &self.in_flight())
            .finish()
    }
}

/// One request for a worker, independent of the HTTP stack.
pub struct WorkerRequest<'a> {
    pub method: &'a str,
    /// `https://host/path?query`
    pub uri: String,
    pub query: &'a str,
    pub protocol: &'a str,
    pub remote_addr: String,
    pub headers: &'a HeaderMap,
    pub body: Bytes,
    /// Time allowed for the complete response.
    pub timeout: Duration,
    /// The CGI parameters FPM would get (`$_SERVER`), sent as `server`
    /// for NOVA's own workers; RoadRunner workers ignore the field.
    pub server: Option<Vec<(Vec<u8>, Vec<u8>)>>,
}

/// Removes a request from its link's table when its exchange ends.
struct Registered {
    link: Arc<Link>,
    id: u32,
    pool: Arc<WorkerPool>,
}

impl Drop for Registered {
    fn drop(&mut self) {
        self.link.pending.lock().unwrap().remove(&self.id);
        self.pool.in_flight.fetch_sub(1, Ordering::Relaxed);
    }
}

impl WorkerPool {
    pub fn new(
        socket: impl Into<PathBuf>,
        peer_uids: Vec<u32>,
        expose_errors: bool,
        parse_urlencoded: bool,
        fastcgi: bool,
        site: &str,
    ) -> Arc<Self> {
        Arc::new(Self {
            socket: socket.into(),
            peer_uids,
            expose_errors,
            parse_urlencoded,
            fastcgi,
            site: site.to_string(),
            links: Mutex::new(Vec::new()),
            linked: Notify::new(),
            next_id: AtomicU32::new(1),
            next_link: AtomicUsize::new(0),
            in_flight: AtomicUsize::new(0),
            listener: Mutex::new(None),
            served: AtomicU64::new(0),
        })
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// Start accepting manager connections (idempotent). Replaces a socket
    /// left by a previous NOVA process; the manager moves over as its
    /// connections to the old process end.
    pub fn listen(self: &Arc<Self>) -> io::Result<()> {
        let mut slot = self.listener.lock().unwrap();
        if slot.is_some() {
            return Ok(());
        }
        let _ = std::fs::remove_file(&self.socket);
        let listener = UnixListener::bind(&self.socket)?;
        // The directory (site owner, worker group, 0730) is the boundary;
        // the peer uid check below the authentication.
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&self.socket, std::fs::Permissions::from_mode(0o666))?;
        let pool = Arc::downgrade(self);
        *slot = Some(tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                };
                let Some(pool) = pool.upgrade() else { return };
                if !pool.peer_uids.is_empty() {
                    match stream.peer_cred() {
                        Ok(c) if pool.peer_uids.contains(&c.uid()) => {}
                        other => {
                            tracing::warn!(site = pool.site, peer = ?other.map(|c| c.uid()), "rejected PHP worker connection from another user");
                            continue;
                        }
                    }
                }
                let (rd, wr) = stream.into_split();
                let link = Arc::new(Link {
                    wr: tokio::sync::Mutex::new(wr),
                    pending: Mutex::new(HashMap::new()),
                    closed: AtomicBool::new(false),
                });
                pool.links.lock().unwrap().push(Arc::clone(&link));
                pool.linked.notify_waiters();
                tokio::spawn(route(Arc::downgrade(&pool), link, rd));
            }
        }));
        Ok(())
    }

    /// Manager connections.
    pub fn links(&self) -> usize {
        self.links.lock().unwrap().len()
    }

    /// Requests sent to the workers and not yet answered (queued or running).
    pub fn in_flight(&self) -> usize {
        self.in_flight.load(Ordering::Relaxed)
    }

    /// A live link (round robin), waiting until `deadline` for the
    /// manager to connect.
    async fn link(&self, deadline: Instant, wait: Duration) -> Result<Arc<Link>, PhpError> {
        loop {
            let notified = self.linked.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let links = self.links.lock().unwrap();
                if !links.is_empty() {
                    let n = self.next_link.fetch_add(1, Ordering::Relaxed);
                    return Ok(Arc::clone(&links[n % links.len()]));
                }
            }
            tokio::time::timeout_at(deadline, notified)
                .await
                .map_err(|_| PhpError::Timeout(wait))?;
        }
    }

    /// Run one request on a worker. The exchange continues in its own task,
    /// so a client that goes away does not cost a booted worker.
    pub async fn execute(
        self: &Arc<Self>,
        req: WorkerRequest<'_>,
    ) -> Result<PhpResponse, PhpError> {
        let timeout = req.timeout;
        let deadline = Instant::now() + timeout;
        let mut frame = match &req.server {
            Some(params) if self.fastcgi => encode_fastcgi(params, &req.body),
            _ => encode_request(&req, self.parse_urlencoded),
        };
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        frame.options.push(id);
        let mut tries = 0;
        let (link, rx) = loop {
            let link = self.link(deadline, timeout).await?;
            let (tx, rx) = mpsc::unbounded_channel();
            link.pending.lock().unwrap().insert(id, tx);
            let sent = if link.closed.load(Ordering::Relaxed) {
                Err(io::ErrorKind::BrokenPipe.into())
            } else {
                link.send(&frame).await
            };
            match sent {
                Ok(()) => break (link, rx),
                Err(e) => {
                    // The manager went away: the request did not reach it.
                    link.pending.lock().unwrap().remove(&id);
                    tries += 1;
                    if tries == 3 {
                        return Err(PhpError::Unavailable(e));
                    }
                    tokio::task::yield_now().await;
                }
            }
        };
        self.in_flight.fetch_add(1, Ordering::Relaxed);
        let reg = Registered {
            link,
            id,
            pool: Arc::clone(self),
        };
        let (head_tx, head_rx) = oneshot::channel();
        tokio::spawn(Arc::clone(self).exchange(reg, rx, deadline, timeout, head_tx));
        head_rx
            .await
            .unwrap_or_else(|_| Err(PhpError::Protocol("worker task ended".into())))
    }

    async fn exchange(
        self: Arc<Self>,
        reg: Registered,
        mut rx: mpsc::UnboundedReceiver<Frame>,
        deadline: Instant,
        timeout: Duration,
        head_tx: oneshot::Sender<Result<PhpResponse, PhpError>>,
    ) {
        let mut head_tx = Some(head_tx);
        let result =
            tokio::time::timeout_at(deadline, self.converse(&reg, &mut rx, &mut head_tx)).await;
        let fail = |e: PhpError, head_tx: &mut Option<oneshot::Sender<_>>| {
            if let Some(tx) = head_tx.take() {
                let _ = tx.send(Err(e));
            }
        };
        match result {
            Ok(Ok(())) => {
                self.served.fetch_add(1, Ordering::Relaxed);
            }
            Ok(Err(e)) => {
                tracing::warn!(site = self.site, error = %e, "PHP worker failed");
                fail(e, &mut head_tx)
            }
            Err(_) => {
                tracing::warn!(
                    site = self.site,
                    ?timeout,
                    "PHP worker timed out; restarting it"
                );
                let mut cancel = Frame::control(r#"{"cancel":true}"#);
                cancel.options = vec![0, reg.id];
                let _ = reg.link.send(&cancel).await;
                fail(PhpError::Timeout(timeout), &mut head_tx)
            }
        }
    }

    /// Deliver the response frames of one request.
    async fn converse(
        &self,
        reg: &Registered,
        rx: &mut mpsc::UnboundedReceiver<Frame>,
        head_tx: &mut Option<oneshot::Sender<Result<PhpResponse, PhpError>>>,
    ) -> Result<(), PhpError> {
        let mut body_tx: Option<mpsc::Sender<io::Result<Bytes>>> = None;
        let mut stopped = false;
        loop {
            let Some(f) = rx.recv().await else {
                return Err(PhpError::Unavailable(io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    "PHP worker manager went away",
                )));
            };
            if f.flags & goridge::CONTROL != 0 {
                if f.payload.windows(6).any(|w| w == b"failed") {
                    return Err(PhpError::Protocol(
                        "PHP worker exited during the request".into(),
                    ));
                }
                continue;
            }
            if f.flags & goridge::ERROR != 0 {
                let msg = String::from_utf8_lossy(&f.payload).into_owned();
                tracing::error!(target: "nova::php", site = self.site, "{msg}");
                if let Some(tx) = head_tx.take() {
                    let body = if self.expose_errors {
                        msg
                    } else {
                        String::new()
                    };
                    let _ = tx.send(Ok(simple(StatusCode::INTERNAL_SERVER_ERROR, body)));
                }
                return Ok(());
            }
            let (head, body) = f.split().map_err(|e| PhpError::Protocol(e.to_string()))?;
            let more = f.byte10 & goridge::STREAM != 0;
            if let Some(tx) = head_tx.take() {
                let parsed = if f.byte11 & goridge::NOVA_CGI_HEAD != 0 {
                    crate::client::parse_cgi_headers(&head).map(Some)?
                } else {
                    parse_head(&head)?
                };
                let Some((status, headers)) = parsed else {
                    let _ = tx.send(Err(PhpError::Protocol("PHP worker stopped".into())));
                    return Ok(());
                };
                let (btx, brx) = mpsc::channel(8);
                let _ = tx.send(Ok(PhpResponse {
                    status,
                    headers,
                    body: brx,
                }));
                body_tx = Some(btx);
            }
            if f.byte10 & goridge::PING != 0 {
                let pong = Frame {
                    byte10: goridge::PONG,
                    options: vec![0, reg.id],
                    ..Default::default()
                };
                reg.link.send(&pong).await.map_err(PhpError::Unavailable)?;
            }
            if !body.is_empty() {
                let gone = match &body_tx {
                    Some(tx) => tx.send(Ok(body)).await.is_err(),
                    None => true,
                };
                if gone && more && !stopped {
                    // Client left: ask the stream generator to stop.
                    stopped = true;
                    let stop = Frame {
                        byte10: goridge::STOP,
                        options: vec![0, reg.id],
                        ..Default::default()
                    };
                    reg.link.send(&stop).await.map_err(PhpError::Unavailable)?;
                }
            }
            if !more {
                return Ok(());
            }
        }
    }
}

/// Read a link's frames and route them to their requests. When the link
/// closes, its requests fail (their channels close).
async fn route(
    pool: std::sync::Weak<WorkerPool>,
    link: Arc<Link>,
    rd: tokio::net::unix::OwnedReadHalf,
) {
    let mut r = tokio::io::BufReader::with_capacity(64 << 10, rd);
    while let Ok(f) = goridge::read_frame(&mut r, MAX_FRAME).await {
        let Some(id) = f.options.get(1).copied() else {
            continue;
        };
        let tx = link.pending.lock().unwrap().get(&id).cloned();
        if let Some(tx) = tx {
            let _ = tx.send(f);
        }
    }
    link.closed.store(true, Ordering::Relaxed);
    if let Some(pool) = pool.upgrade() {
        pool.links
            .lock()
            .unwrap()
            .retain(|l| !Arc::ptr_eq(l, &link));
    }
    link.pending.lock().unwrap().clear();
}

impl Drop for WorkerPool {
    fn drop(&mut self) {
        if let Some(h) = self.listener.lock().unwrap().take() {
            h.abort();
        }
    }
}

fn simple(status: StatusCode, body: String) -> PhpResponse {
    let (tx, rx) = mpsc::channel(1);
    if !body.is_empty() {
        let _ = tx.try_send(Ok(Bytes::from(body)));
    }
    let mut headers = HeaderMap::new();
    headers.insert(
        http::header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    PhpResponse {
        status,
        headers,
        body: rx,
    }
}

/// `{"status":200,"headers":{"Name":["v"]}}`; `None` for `{"stop":true}`.
fn parse_head(head: &[u8]) -> Result<Option<(StatusCode, HeaderMap)>, PhpError> {
    #[derive(serde::Deserialize)]
    struct Head {
        status: Option<u16>,
        #[serde(default)]
        headers: Headers,
        #[serde(default)]
        stop: bool,
    }
    #[derive(serde::Deserialize, Default)]
    #[serde(untagged)]
    enum Headers {
        Map(std::collections::BTreeMap<String, Values>),
        #[default]
        #[allow(dead_code)]
        Empty,
        #[allow(dead_code)]
        List(Vec<serde_json::Value>),
    }
    #[derive(serde::Deserialize)]
    #[serde(untagged)]
    enum Values {
        Many(Vec<String>),
        One(String),
    }
    let h: Head = serde_json::from_slice(head)
        .map_err(|e| PhpError::Protocol(format!("invalid response header from PHP worker: {e}")))?;
    if h.stop && h.status.is_none() {
        return Ok(None);
    }
    let status = StatusCode::from_u16(h.status.unwrap_or(200))
        .map_err(|_| PhpError::Protocol("invalid status from PHP worker".into()))?;
    let mut headers = HeaderMap::new();
    if let Headers::Map(map) = h.headers {
        for (name, values) in map {
            let Ok(name) = HeaderName::from_bytes(name.as_bytes()) else {
                continue;
            };
            let values = match values {
                Values::Many(v) => v,
                Values::One(v) => vec![v],
            };
            for v in values {
                if let Ok(v) = HeaderValue::from_str(&v) {
                    headers.append(name.clone(), v);
                }
            }
        }
    }
    Ok(Some((status, headers)))
}

/// Request frame for NOVA's PHP extension: FastCGI-encoded parameters
/// (exactly what PHP-FPM would get), then the raw body.
pub fn encode_fastcgi(params: &[(Vec<u8>, Vec<u8>)], body: &[u8]) -> Frame {
    let encoded = crate::fastcgi::encode_params(params.iter().map(|(k, v)| (&k[..], &v[..])));
    let mut f = Frame::message(&encoded, body);
    f.byte11 = goridge::NOVA_FASTCGI;
    f
}

/// Request frame: RoadRunner's JSON context, then the raw body. Form
/// bodies are flagged for the manager, which parses them in the sandbox.
pub fn encode_request(req: &WorkerRequest<'_>, parse_urlencoded: bool) -> Frame {
    use crate::form::json_str;
    let mut h = String::with_capacity(512);
    h.push_str("{\"remoteAddr\":");
    json_str(&mut h, &req.remote_addr);
    h.push_str(",\"protocol\":");
    json_str(&mut h, req.protocol);
    h.push_str(",\"method\":");
    json_str(&mut h, req.method);
    h.push_str(",\"uri\":");
    json_str(&mut h, &req.uri);
    h.push_str(",\"rawQuery\":");
    json_str(&mut h, req.query);
    h.push_str(",\"headers\":{");
    let mut first = true;
    for name in req.headers.keys() {
        // NOVA's own workers read headers from `server`; only the content
        // type is needed here (the manager parses uploads with it).
        if req.server.is_some() && name != http::header::CONTENT_TYPE {
            continue;
        }
        if !first {
            h.push(',');
        }
        first = false;
        json_str(&mut h, &canonical(name.as_str()));
        h.push_str(":[");
        for (i, v) in req.headers.get_all(name).iter().enumerate() {
            if i > 0 {
                h.push(',');
            }
            crate::form::json_bytes(&mut h, v.as_bytes());
        }
        h.push(']');
    }
    h.push_str("},\"cookies\":{");
    let mut seen = Vec::new();
    for v in req.headers.get_all(http::header::COOKIE) {
        for pair in v.as_bytes().split(|&b| b == b';') {
            let pair = pair.trim_ascii();
            let Some(eq) = pair.iter().position(|&b| b == b'=') else {
                continue;
            };
            let name = crate::form::url_decode(&pair[..eq]);
            // PHP keeps the first of duplicate names.
            if name.is_empty() || seen.contains(&name) {
                continue;
            }
            if !seen.is_empty() {
                h.push(',');
            }
            json_str(&mut h, &name);
            h.push(':');
            json_str(&mut h, &crate::form::url_decode(&pair[eq + 1..]));
            seen.push(name);
        }
    }
    h.push_str("},\"attributes\":{}");
    if let Some(server) = &req.server {
        h.push_str(",\"server\":{");
        for (i, (k, v)) in server.iter().enumerate() {
            if i > 0 {
                h.push(',');
            }
            crate::form::json_bytes(&mut h, k);
            h.push(':');
            crate::form::json_bytes(&mut h, v);
        }
        h.push('}');
    }
    h.push_str(crate::form::UNPARSED_TAIL);
    let mut f = Frame::message(h.as_bytes(), &req.body);
    let ct = req
        .headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !req.body.is_empty() && crate::form::needs_parsing(ct, parse_urlencoded) {
        f.byte11 = goridge::NOVA_PARSE_FORM;
    }
    f
}

/// `content-type` -> `Content-Type`, the form RoadRunner (Go) passes.
fn canonical(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut up = true;
    for c in name.chars() {
        out.push(if up { c.to_ascii_uppercase() } else { c });
        up = c == '-';
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_context() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "content-type",
            "application/x-www-form-urlencoded".parse().unwrap(),
        );
        headers.append("x-multi", "a".parse().unwrap());
        headers.append("x-multi", "b\"c".parse().unwrap());
        headers.insert("cookie", "s=a%3Db; s=second; t=1".parse().unwrap());
        let f = encode_request(
            &WorkerRequest {
                method: "POST",
                uri: "https://example.test/x?y=1".into(),
                query: "y=1",
                protocol: "HTTP/2.0",
                remote_addr: "192.0.2.1".into(),
                headers: &headers,
                body: Bytes::from_static(b"a=1"),
                timeout: Duration::from_secs(1),
                server: None,
            },
            true,
        );
        assert_eq!(f.byte11, goridge::NOVA_PARSE_FORM);
        let (h, b) = f.split().unwrap();
        assert_eq!(&b[..], b"a=1");
        let v: serde_json::Value = serde_json::from_slice(&h).unwrap();
        assert_eq!(v["uri"], "https://example.test/x?y=1");
        assert_eq!(v["headers"]["X-Multi"][1], "b\"c");
        assert_eq!(
            v["headers"]["Content-Type"][0],
            "application/x-www-form-urlencoded"
        );
        assert_eq!(v["cookies"]["s"], "a=b");
        assert_eq!(v["cookies"]["t"], "1");
        assert_eq!(v["parsed"], false);
    }

    #[test]
    fn response_head() {
        let (s, h) =
            parse_head(br#"{"status":201,"headers":{"Set-Cookie":["a=1","b=2"],"X":"y"}}"#)
                .unwrap()
                .unwrap();
        assert_eq!(s, 201);
        assert_eq!(h.get_all("set-cookie").iter().count(), 2);
        assert_eq!(h["x"], "y");
        let (s, h) = parse_head(br#"{"status":204,"headers":[]}"#)
            .unwrap()
            .unwrap();
        assert_eq!((s.as_u16(), h.len()), (204, 0));
        assert!(parse_head(br#"{"stop":true}"#).unwrap().is_none());
    }

    #[tokio::test]
    async fn exchange_with_a_fake_manager() {
        let dir = std::env::temp_dir().join(format!("nova-wk-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let pool = WorkerPool::new(dir.join("w.sock"), vec![], false, true, false, "t");
        pool.listen().unwrap();
        // A manager that takes two requests, then answers the second one
        // first: the first streams two chunks.
        let mut m = tokio::net::UnixStream::connect(pool.socket())
            .await
            .unwrap();
        let fake = tokio::spawn(async move {
            let a = goridge::read_frame(&mut m, 1 << 20).await.unwrap();
            let b = goridge::read_frame(&mut m, 1 << 20).await.unwrap();
            let (h, _) = a.split().unwrap();
            assert!(
                std::str::from_utf8(&h)
                    .unwrap()
                    .contains("\"method\":\"GET\"")
            );
            let (ida, idb) = (a.options[1], b.options[1]);
            assert_ne!(ida, idb);
            let mut r = Frame::message(br#"{"status":404,"headers":{}}"#, b"");
            r.options.push(idb);
            r.write_to(&mut m).await.unwrap();
            let mut one = Frame::message(br#"{"status":200,"headers":{}}"#, b"one,");
            one.byte10 = goridge::STREAM;
            one.options.push(ida);
            one.write_to(&mut m).await.unwrap();
            let mut two = Frame::message(br#"{"status":200,"headers":{}}"#, b"two");
            two.options.push(ida);
            two.write_to(&mut m).await.unwrap();
            // A third request times out: NOVA cancels it.
            let c = goridge::read_frame(&mut m, 1 << 20).await.unwrap();
            let cancel = goridge::read_frame(&mut m, 1 << 20).await.unwrap();
            assert_eq!(cancel.flags & goridge::CONTROL, goridge::CONTROL);
            assert_eq!(cancel.options[1], c.options[1]);
        });
        let headers = HeaderMap::new();
        let req = |timeout| WorkerRequest {
            method: "GET",
            uri: "http://t/".into(),
            query: "",
            protocol: "HTTP/1.1",
            remote_addr: "127.0.0.1".into(),
            headers: &headers,
            body: Bytes::new(),
            timeout,
            server: None,
        };
        let s = Duration::from_secs(5);
        let (a, b) = tokio::join!(pool.execute(req(s)), async {
            tokio::time::sleep(Duration::from_millis(20)).await;
            pool.execute(req(s)).await
        });
        assert_eq!(b.unwrap().status, 404);
        let mut a = a.unwrap();
        assert_eq!(a.status, 200);
        let mut body = Vec::new();
        while let Some(c) = a.body.recv().await {
            body.extend_from_slice(&c.unwrap());
        }
        assert_eq!(body, b"one,two");
        assert!(matches!(
            pool.execute(req(Duration::from_millis(100))).await,
            Err(PhpError::Timeout(_))
        ));
        fake.await.unwrap();
        assert_eq!(pool.in_flight(), 0);
        let _ = std::fs::remove_dir_all(dir);
    }
}
