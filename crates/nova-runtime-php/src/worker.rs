//! Worker mode, HTTP side: requests go to long-running PHP workers that
//! keep the application booted between requests (Laravel Octane, Symfony
//! Runtime, Spiral, ... through their RoadRunner integration).
//!
//! NOVA listens on a per-site socket; the site's worker manager (`nova
//! php-workers`, running as the site inside its sandbox) connects once per
//! PHP worker. A connection is one worker: NOVA writes a request frame and
//! reads the response frames, one request at a time. The manager kills a
//! worker whose connection closes mid-request (timeout), and says goodbye
//! with a control frame before closing an idle one, so a request is never
//! lost in between.

use crate::goridge::{self, Frame};
use crate::{PhpError, PhpResponse};
use bytes::Bytes;
use http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Semaphore, mpsc, oneshot};
use tokio::time::Instant;

/// Largest response frame accepted from PHP (bigger bodies should stream).
const MAX_FRAME: usize = 256 << 20;

pub struct WorkerPool {
    socket: PathBuf,
    /// Only connections from these uids are workers (empty: anyone who
    /// can reach the socket).
    peer_uids: Vec<u32>,
    max_requests: u32,
    expose_errors: bool,
    /// RoadRunner workers expect urlencoded forms parsed by the server;
    /// NOVA's own workers parse them in PHP and keep the raw body.
    parse_urlencoded: bool,
    site: String,
    idle: Mutex<Vec<WorkerConn>>,
    /// One permit per idle connection.
    ready: Semaphore,
    listener: Mutex<Option<tokio::task::JoinHandle<()>>>,
    pub served: AtomicU64,
}

struct WorkerConn {
    stream: UnixStream,
    requests: u32,
}

impl std::fmt::Debug for WorkerPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkerPool")
            .field("socket", &self.socket)
            .field("idle", &self.ready.available_permits())
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

impl WorkerPool {
    pub fn new(
        socket: impl Into<PathBuf>,
        peer_uids: Vec<u32>,
        max_requests: u32,
        expose_errors: bool,
        parse_urlencoded: bool,
        site: &str,
    ) -> Arc<Self> {
        Arc::new(Self {
            socket: socket.into(),
            peer_uids,
            max_requests: max_requests.max(1),
            expose_errors,
            parse_urlencoded,
            site: site.to_string(),
            idle: Mutex::new(Vec::new()),
            ready: Semaphore::new(0),
            listener: Mutex::new(None),
            served: AtomicU64::new(0),
        })
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// Start accepting worker connections (idempotent). Replaces a socket
    /// left by a previous NOVA process; its workers move over as their
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
                pool.put(WorkerConn {
                    stream,
                    requests: 0,
                });
            }
        }));
        Ok(())
    }

    fn put(&self, conn: WorkerConn) {
        self.idle.lock().unwrap().push(conn);
        self.ready.add_permits(1);
    }

    /// Connected workers waiting for a request.
    pub fn idle_count(&self) -> usize {
        self.ready.available_permits()
    }

    /// The most recently used live worker (warm caches), waiting up to
    /// `wait` for one to become free.
    async fn take(&self, wait: Duration) -> Result<WorkerConn, PhpError> {
        let deadline = Instant::now() + wait;
        loop {
            let permit = tokio::time::timeout_at(deadline, self.ready.acquire())
                .await
                .map_err(|_| PhpError::Timeout(wait))?
                .expect("never closed");
            permit.forget();
            let conn = self.idle.lock().unwrap().pop().expect("permit per conn");
            if crate::pool::alive(&conn.stream) {
                return Ok(conn);
            }
            // Worker exited or the manager said goodbye: take the next one.
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
        let frame = encode_request(&req, self.parse_urlencoded);
        let conn = self.take(timeout).await?;
        let (head_tx, head_rx) = oneshot::channel();
        tokio::spawn(Arc::clone(self).exchange(conn, frame, deadline, timeout, head_tx));
        head_rx
            .await
            .unwrap_or_else(|_| Err(PhpError::Protocol("worker task ended".into())))
    }

    async fn exchange(
        self: Arc<Self>,
        mut conn: WorkerConn,
        frame: Frame,
        deadline: Instant,
        timeout: Duration,
        head_tx: oneshot::Sender<Result<PhpResponse, PhpError>>,
    ) {
        let mut head_tx = Some(head_tx);
        let mut result;
        let mut tries = 0;
        loop {
            result = tokio::time::timeout_at(
                deadline,
                self.converse(&mut conn.stream, frame.clone(), &mut head_tx),
            )
            .await;
            // The manager closed this worker's connection before reading
            // the request: it never ran, so another worker may take it.
            let unsent = matches!(&result, Ok(Err(PhpError::Unavailable(e))) if e.kind() == io::ErrorKind::ConnectionAborted);
            tries += 1;
            if !unsent || tries == 3 {
                break;
            }
            match self
                .take(deadline.saturating_duration_since(Instant::now()))
                .await
            {
                Ok(c) => conn = c,
                Err(e) => {
                    result = Ok(Err(e));
                    break;
                }
            }
        }
        let fail = |e: PhpError, head_tx: &mut Option<oneshot::Sender<_>>| {
            if let Some(tx) = head_tx.take() {
                let _ = tx.send(Err(e));
            }
        };
        match result {
            Ok(Ok(reusable)) => {
                self.served.fetch_add(1, Ordering::Relaxed);
                conn.requests += 1;
                if !reusable {
                    return;
                }
                if conn.requests >= self.max_requests {
                    // The manager starts a fresh worker in its place.
                    let _ = Frame::control(r#"{"stop":true}"#)
                        .write_to(&mut conn.stream)
                        .await;
                    return;
                }
                self.put(conn);
            }
            // The connection is dropped: the manager kills the worker.
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
                fail(PhpError::Timeout(timeout), &mut head_tx)
            }
        }
    }

    /// Send the request, deliver the response. `Ok(false)`: the worker is
    /// stopping and must not get another request.
    async fn converse(
        &self,
        stream: &mut UnixStream,
        frame: Frame,
        head_tx: &mut Option<oneshot::Sender<Result<PhpResponse, PhpError>>>,
    ) -> Result<bool, PhpError> {
        frame.write_to(stream).await.map_err(|e| {
            PhpError::Unavailable(io::Error::new(io::ErrorKind::ConnectionAborted, e))
        })?;
        let mut body_tx: Option<mpsc::Sender<io::Result<Bytes>>> = None;
        let mut stopped = false;
        loop {
            let f = goridge::read_frame(stream, MAX_FRAME)
                .await
                .map_err(|e| PhpError::Protocol(format!("reading from PHP worker: {e}")))?;
            if f.flags & goridge::CONTROL != 0 {
                if head_tx.is_some() && f.payload.windows(4).any(|w| w == b"stop") {
                    // Goodbye from the manager, sent before it read the request.
                    return Err(PhpError::Unavailable(io::Error::new(
                        io::ErrorKind::ConnectionAborted,
                        "worker left before the request",
                    )));
                }
                // e.g. a pid announcement; nothing to do.
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
                return Ok(true);
            }
            let (head, body) = f.split().map_err(|e| PhpError::Protocol(e.to_string()))?;
            let more = f.byte10 & goridge::STREAM != 0;
            if let Some(tx) = head_tx.take() {
                let Some((status, headers)) = parse_head(&head)? else {
                    // `{"stop":true}`: the worker is quitting instead of answering.
                    let _ = tx.send(Err(PhpError::Protocol("PHP worker stopped".into())));
                    return Ok(false);
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
                    ..Default::default()
                };
                pong.write_to(stream).await.map_err(PhpError::Unavailable)?;
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
                        ..Default::default()
                    };
                    stop.write_to(stream).await.map_err(PhpError::Unavailable)?;
                }
            }
            if !more {
                return Ok(true);
            }
        }
    }
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
            json_str(&mut h, &String::from_utf8_lossy(v.as_bytes()));
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
            json_str(&mut h, &String::from_utf8_lossy(k));
            h.push(':');
            json_str(&mut h, &String::from_utf8_lossy(v));
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
    async fn exchange_with_a_fake_worker() {
        let dir = std::env::temp_dir().join(format!("nova-wk-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let pool = WorkerPool::new(dir.join("w.sock"), vec![], 2, false, true, "t");
        pool.listen().unwrap();
        // A worker that streams two chunks, then answers plainly.
        let mut php = UnixStream::connect(pool.socket()).await.unwrap();
        let fake = tokio::spawn(async move {
            for n in 0..2 {
                let req = goridge::read_frame(&mut php, 1 << 20).await.unwrap();
                let (h, _) = req.split().unwrap();
                assert!(
                    std::str::from_utf8(&h)
                        .unwrap()
                        .contains("\"method\":\"GET\"")
                );
                if n == 0 {
                    let mut a = Frame::message(br#"{"status":200,"headers":{}}"#, b"one,");
                    a.byte10 = goridge::STREAM;
                    a.write_to(&mut php).await.unwrap();
                    Frame::message(br#"{"status":200,"headers":{}}"#, b"two")
                        .write_to(&mut php)
                        .await
                        .unwrap();
                } else {
                    Frame::message(br#"{"status":404,"headers":{}}"#, b"")
                        .write_to(&mut php)
                        .await
                        .unwrap();
                }
            }
            // max_requests = 2: NOVA asks this worker to stop.
            let stop = goridge::read_frame(&mut php, 1 << 20).await.unwrap();
            assert_eq!(stop.flags & goridge::CONTROL, goridge::CONTROL);
        });
        let headers = HeaderMap::new();
        let req = || WorkerRequest {
            method: "GET",
            uri: "http://t/".into(),
            query: "",
            protocol: "HTTP/1.1",
            remote_addr: "127.0.0.1".into(),
            headers: &headers,
            body: Bytes::new(),
            timeout: Duration::from_secs(5),
            server: None,
        };
        let mut r = pool.execute(req()).await.unwrap();
        assert_eq!(r.status, 200);
        let mut body = Vec::new();
        while let Some(c) = r.body.recv().await {
            body.extend_from_slice(&c.unwrap());
        }
        assert_eq!(body, b"one,two");
        let r = pool.execute(req()).await.unwrap();
        assert_eq!(r.status, 404);
        fake.await.unwrap();
        assert!(matches!(
            pool.take(Duration::from_millis(50)).await,
            Err(PhpError::Timeout(_))
        ));
        let _ = std::fs::remove_dir_all(dir);
    }
}
