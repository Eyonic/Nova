//! Executes one PHP request over FastCGI and turns the CGI response into
//! an HTTP status, headers and a streamed body.

use crate::fastcgi::{self, EndRequest, RecordReader, RecordType};
use crate::pool::{Pool, Slot};
use bytes::{Bytes, BytesMut};
use futures_util::{Stream, StreamExt};
use http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use tokio::sync::mpsc;

/// Upper bound for the CGI header block PHP may emit.
const MAX_HEADER_BYTES: usize = 64 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum PhpError {
    #[error("PHP runtime unavailable: {0}")]
    Unavailable(io::Error),
    #[error("PHP did not respond within {0:?}")]
    Timeout(Duration),
    #[error("invalid response from PHP: {0}")]
    Protocol(String),
    #[error("request body error: {0}")]
    Body(io::Error),
}

pub struct PhpResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    /// Remaining body chunks; closed when PHP finishes.
    pub body: mpsc::Receiver<io::Result<Bytes>>,
}

/// Everything needed to run one request, independent of the HTTP stack.
pub struct PhpRequest<B> {
    pub socket: std::path::PathBuf,
    pub params: Vec<(Vec<u8>, Vec<u8>)>,
    pub body: B,
    /// Time allowed until PHP has produced the complete header block.
    pub header_timeout: Duration,
    /// Used to tag PHP stderr output in logs.
    pub site: String,
}

/// Aborts the task when dropped (unless it was taken out first).
struct AbortOnDrop<T>(Option<tokio::task::JoinHandle<T>>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        if let Some(h) = &self.0 {
            h.abort();
        }
    }
}

/// Run one request on a new connection that FPM closes afterwards.
pub async fn execute<B>(req: PhpRequest<B>) -> Result<PhpResponse, PhpError>
where
    B: Stream<Item = io::Result<Bytes>> + Send + Unpin + 'static,
{
    let stream = connect(&req.socket).await?;
    run(stream, None, req).await
}

/// Run one request on a kept-alive connection from `pool` (`req.socket` is
/// ignored); the connection goes back to the pool after a clean response.
pub async fn execute_pooled<B>(
    req: PhpRequest<B>,
    pool: &Arc<Pool>,
) -> Result<PhpResponse, PhpError>
where
    B: Stream<Item = io::Result<Bytes>> + Send + Unpin + 'static,
{
    let (stream, slot) = pool.get().await?.into_parts();
    run(stream, Some(slot), req).await
}

async fn run<B>(
    stream: UnixStream,
    slot: Option<Slot>,
    req: PhpRequest<B>,
) -> Result<PhpResponse, PhpError>
where
    B: Stream<Item = io::Result<Bytes>> + Send + Unpin + 'static,
{
    let PhpRequest {
        socket: _,
        params,
        body,
        header_timeout,
        site,
    } = req;
    let keep = slot.is_some();
    let (rd, mut wr) = stream.into_split();

    // Writer: BEGIN_REQUEST, PARAMS, then STDIN streamed from the client.
    // Runs concurrently with the reader so a script that writes output
    // before consuming php://input cannot deadlock against us.
    let (body_err_tx, mut body_err_rx) = mpsc::channel::<io::Error>(1);
    // Aborted on every exit path, including this future being dropped
    // (client gone): FPM closes a connection only after reading EOF, so a
    // write half left open would hold a PHP worker forever.
    let mut writer = AbortOnDrop(Some(tokio::spawn(async move {
        let mut buf = BytesMut::with_capacity(8192);
        fastcgi::put_begin_request(&mut buf, keep);
        let encoded = fastcgi::encode_params(params.iter().map(|(k, v)| (&k[..], &v[..])));
        fastcgi::put_stream(&mut buf, RecordType::Params, &encoded);
        fastcgi::put_record(&mut buf, RecordType::Params, &[]);
        wr.write_all(&buf).await?;
        let mut body = body;
        while let Some(chunk) = body.next().await {
            let chunk = match chunk {
                Ok(c) => c,
                Err(e) => {
                    // Closing without the final STDIN record makes FPM abort the script.
                    let _ = body_err_tx.send(e).await;
                    return Ok(None);
                }
            };
            buf.clear();
            fastcgi::put_stream(&mut buf, RecordType::Stdin, &chunk);
            wr.write_all(&buf).await?;
        }
        buf.clear();
        fastcgi::put_record(&mut buf, RecordType::Stdin, &[]);
        wr.write_all(&buf).await?;
        if keep {
            // Handed back for reuse once the response is complete.
            return Ok(Some(wr));
        }
        // Dropping the write half would half-close the socket; keep it open
        // until the reader has consumed the response and aborts this task.
        std::future::pending::<()>().await;
        Ok::<_, io::Error>(None)
    })));

    let mut reader = RecordReader::new(rd);
    let header_phase = async {
        let mut head = BytesMut::new();
        loop {
            tokio::select! {
                biased;
                Some(e) = body_err_rx.recv() => return Err(PhpError::Body(e)),
                rec = reader.next() => {
                    let rec = rec.map_err(|e| PhpError::Protocol(e.to_string()))?;
                    let Some(rec) = rec else {
                        return Err(PhpError::Protocol("connection closed before response headers".into()));
                    };
                    match rec.ty {
                        RecordType::Stdout => {
                            head.extend_from_slice(&rec.content);
                            if let Some((end, skip)) = find_header_end(&head) {
                                let rest = head.split_off(end + skip).freeze();
                                head.truncate(end);
                                return Ok((head.freeze(), rest, false));
                            }
                            if head.len() > MAX_HEADER_BYTES {
                                return Err(PhpError::Protocol("response header block too large".into()));
                            }
                        }
                        RecordType::Stderr => log_stderr(&site, &rec.content),
                        RecordType::EndRequest => {
                            // Response without a blank line: everything is headers.
                            return Ok((head.freeze(), Bytes::new(), true));
                        }
                        other => return Err(PhpError::Protocol(format!("unexpected record {other:?}"))),
                    }
                }
            }
        }
    };
    let (head, rest, finished) = match tokio::time::timeout(header_timeout, header_phase).await {
        Ok(r) => r?,
        Err(_) => return Err(PhpError::Timeout(header_timeout)),
    };
    let (status, headers) = parse_cgi_headers(&head)?;

    let (tx, rx) = mpsc::channel(16);
    tokio::spawn(async move {
        // Reusable only after a clean END_REQUEST with the request fully sent.
        let mut clean = finished;
        if !rest.is_empty() && tx.send(Ok(rest)).await.is_err() {
            return;
        }
        if !finished {
            loop {
                let next = tokio::select! {
                    // Client gone: close now instead of waiting for output.
                    _ = tx.closed() => break,
                    next = reader.next() => next,
                };
                match next {
                    Ok(Some(rec)) => match rec.ty {
                        RecordType::Stdout if !rec.content.is_empty() => {
                            if tx.send(Ok(rec.content)).await.is_err() {
                                break; // client went away; dropping the socket aborts the script
                            }
                        }
                        RecordType::Stderr => log_stderr(&site, &rec.content),
                        RecordType::EndRequest => {
                            match EndRequest::parse(&rec.content) {
                                Ok(end) if end.protocol_status != 0 => tracing::warn!(
                                    site,
                                    protocol_status = end.protocol_status,
                                    "FastCGI request not completed"
                                ),
                                Ok(_) => clean = true,
                                Err(_) => {}
                            }
                            break;
                        }
                        _ => {}
                    },
                    Ok(None) => break,
                    Err(e) => {
                        let _ = tx.send(Err(e)).await;
                        break;
                    }
                }
            }
        }
        if let Some(slot) = slot
            && clean
            && let Some(handle) = writer.0.take_if(|h| h.is_finished())
            && let Ok(Ok(Some(wr))) = handle.await
            && let Ok(stream) = reader.into_inner().reunite(wr)
        {
            slot.recycle(stream);
        }
    });

    Ok(PhpResponse {
        status,
        headers,
        body: rx,
    })
}

/// Connect to the pool. A missing or refusing socket is retried for a few
/// seconds: that is a pool being (re)started, e.g. during a config reload,
/// and the client should not see it.
pub(crate) async fn connect(socket: &Path) -> Result<UnixStream, PhpError> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        match tokio::time::timeout(Duration::from_secs(5), UnixStream::connect(socket)).await {
            Ok(Ok(s)) => return Ok(s),
            Ok(Err(e))
                if matches!(
                    e.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                ) && tokio::time::Instant::now() < deadline =>
            {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Ok(Err(e)) => return Err(PhpError::Unavailable(e)),
            Err(_) => {
                return Err(PhpError::Unavailable(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "connect timed out",
                )));
            }
        }
    }
}

fn log_stderr(site: &str, content: &[u8]) {
    for line in String::from_utf8_lossy(content)
        .lines()
        .filter(|l| !l.trim().is_empty())
    {
        tracing::warn!(target: "nova::php", site, "{line}");
    }
}

/// Position of the blank line ending the header block and its length.
fn find_header_end(buf: &[u8]) -> Option<(usize, usize)> {
    let crlf = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|p| (p, 4));
    let lf = buf.windows(2).position(|w| w == b"\n\n").map(|p| (p, 2));
    match (crlf, lf) {
        (Some(a), Some(b)) => Some(if a.0 <= b.0 { a } else { b }),
        (a, b) => a.or(b),
    }
}

/// Parse a CGI header block (RFC 3875 §6.3) into an HTTP status and headers.
pub fn parse_cgi_headers(head: &[u8]) -> Result<(StatusCode, HeaderMap), PhpError> {
    let text = std::str::from_utf8(head)
        .map_err(|_| PhpError::Protocol("non UTF-8 response headers".into()))?;
    let mut headers = HeaderMap::new();
    let mut status = None;
    for line in text
        .split('\n')
        .map(|l| l.trim_end_matches('\r'))
        .filter(|l| !l.is_empty())
    {
        let Some((name, value)) = line.split_once(':') else {
            return Err(PhpError::Protocol(format!(
                "malformed header line {line:?}"
            )));
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("status") {
            let code = value.split_whitespace().next().unwrap_or("");
            status = Some(
                code.parse::<u16>()
                    .ok()
                    .and_then(|c| StatusCode::from_u16(c).ok())
                    .ok_or_else(|| {
                        PhpError::Protocol(format!("invalid Status header {value:?}"))
                    })?,
            );
            continue;
        }
        let (Ok(n), Ok(v)) = (
            HeaderName::from_bytes(name.trim().as_bytes()),
            HeaderValue::from_str(value),
        ) else {
            tracing::warn!("dropping invalid header from PHP: {line:?}");
            continue;
        };
        headers.append(n, v);
    }
    let status = status.unwrap_or(if headers.contains_key(http::header::LOCATION) {
        StatusCode::FOUND
    } else {
        StatusCode::OK
    });
    Ok((status, headers))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_status_and_headers() {
        let (s, h) = parse_cgi_headers(b"Status: 404 Not Found\r\nContent-type: text/html\r\nSet-Cookie: a=1\r\nSet-Cookie: b=2").unwrap();
        assert_eq!(s, StatusCode::NOT_FOUND);
        assert_eq!(h["content-type"], "text/html");
        assert_eq!(h.get_all("set-cookie").iter().count(), 2);
    }

    #[test]
    fn location_without_status_is_302() {
        let (s, _) = parse_cgi_headers(b"Location: /next").unwrap();
        assert_eq!(s, StatusCode::FOUND);
    }

    #[test]
    fn header_end_detection() {
        assert_eq!(find_header_end(b"A: b\r\n\r\nbody"), Some((4, 4)));
        assert_eq!(find_header_end(b"A: b\n\nbody"), Some((4, 2)));
        assert_eq!(find_header_end(b"A: b\r\n"), None);
    }

    /// A fake PHP-FPM that, like `fcgi_close`, answers and then waits for
    /// the web server to close the connection. Returns whether it saw EOF
    /// within two seconds after answering.
    async fn fake_fpm(listener: tokio::net::UnixListener, delay: Duration) -> bool {
        use tokio::io::AsyncReadExt;
        let (mut s, _) = listener.accept().await.unwrap();
        // Read until the empty STDIN record that ends the request.
        let mut got = Vec::new();
        let mut buf = [0u8; 4096];
        let end_of_stdin = [1u8, RecordType::Stdin as u8];
        while !got
            .windows(8)
            .any(|w| w[..2] == end_of_stdin && w[4..6] == [0, 0])
        {
            let n = s.read(&mut buf).await.unwrap();
            assert!(n > 0, "request incomplete");
            got.extend_from_slice(&buf[..n]);
        }
        tokio::time::sleep(delay).await;
        let mut out = BytesMut::new();
        fastcgi::put_stream(
            &mut out,
            RecordType::Stdout,
            b"Content-type: text/plain\r\n\r\nhi",
        );
        fastcgi::put_record(&mut out, RecordType::Stdout, &[]);
        fastcgi::put_record(&mut out, RecordType::EndRequest, &[0; 8]);
        let _ = s.write_all(&out).await;
        let _ = s.shutdown().await;
        let eof = async {
            loop {
                match s.read(&mut buf).await {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {}
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(2), eof)
            .await
            .is_ok()
    }

    fn request(socket: &Path) -> PhpRequest<futures_util::stream::Empty<io::Result<Bytes>>> {
        PhpRequest {
            socket: socket.to_path_buf(),
            params: vec![(b"REQUEST_METHOD".to_vec(), b"GET".to_vec())],
            body: futures_util::stream::empty(),
            header_timeout: Duration::from_secs(5),
            site: "test".into(),
        }
    }

    /// The client gives up while PHP is still working (closed tab, proxy
    /// timeout): the FastCGI connection must still be closed, or FPM keeps
    /// that worker waiting forever and the pool runs dry.
    #[tokio::test]
    async fn abandoned_request_releases_the_php_worker() {
        let dir = std::env::temp_dir().join(format!("nova-fcgi-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("abandon.sock");
        let _ = std::fs::remove_file(&sock);
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        let fpm = tokio::spawn(fake_fpm(listener, Duration::from_millis(300)));
        let call = tokio::spawn(execute(request(&sock)));
        tokio::time::sleep(Duration::from_millis(100)).await;
        call.abort(); // the HTTP request future is dropped mid-flight
        assert!(
            fpm.await.unwrap(),
            "PHP worker never saw the connection close"
        );

        // And a completed request closes too.
        let listener = tokio::net::UnixListener::bind(dir.join("done.sock")).unwrap();
        let fpm = tokio::spawn(fake_fpm(listener, Duration::ZERO));
        let mut resp = execute(request(&dir.join("done.sock"))).await.unwrap();
        while resp.body.recv().await.is_some() {}
        assert!(
            fpm.await.unwrap(),
            "PHP worker never saw the connection close"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A fake PHP-FPM that honours FCGI_KEEP_CONN: serves requests on each
    /// connection until the client closes it. Returns the accept counter
    /// and the number of connections open at the same time (high water).
    fn keepalive_fpm(
        listener: tokio::net::UnixListener,
        delay: Duration,
    ) -> (
        Arc<std::sync::atomic::AtomicUsize>,
        Arc<std::sync::atomic::AtomicUsize>,
    ) {
        use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
        use tokio::io::AsyncReadExt;
        let accepts = Arc::new(AtomicUsize::new(0));
        let open = Arc::new(AtomicUsize::new(0));
        let high = Arc::new(AtomicUsize::new(0));
        let (a, h) = (Arc::clone(&accepts), Arc::clone(&high));
        tokio::spawn(async move {
            loop {
                let (mut s, _) = listener.accept().await.unwrap();
                a.fetch_add(1, SeqCst);
                let now = open.fetch_add(1, SeqCst) + 1;
                h.fetch_max(now, SeqCst);
                let open = Arc::clone(&open);
                tokio::spawn(async move {
                    let mut got = Vec::new();
                    let mut buf = [0u8; 4096];
                    let end_of_stdin = [1u8, RecordType::Stdin as u8];
                    loop {
                        // One request: read until the empty STDIN record.
                        while !got
                            .windows(8)
                            .any(|w| w[..2] == end_of_stdin && w[4..6] == [0, 0])
                        {
                            match s.read(&mut buf).await {
                                Ok(0) | Err(_) => {
                                    open.fetch_sub(1, SeqCst);
                                    return;
                                }
                                Ok(n) => got.extend_from_slice(&buf[..n]),
                            }
                        }
                        got.clear();
                        tokio::time::sleep(delay).await;
                        let mut out = BytesMut::new();
                        fastcgi::put_stream(
                            &mut out,
                            RecordType::Stdout,
                            b"Content-type: text/plain\r\n\r\nok",
                        );
                        fastcgi::put_record(&mut out, RecordType::Stdout, &[]);
                        fastcgi::put_record(&mut out, RecordType::EndRequest, &[0; 8]);
                        if s.write_all(&out).await.is_err() {
                            open.fetch_sub(1, SeqCst);
                            return;
                        }
                    }
                });
            }
        });
        (accepts, high)
    }

    async fn drain(mut r: PhpResponse) -> Bytes {
        let mut b = BytesMut::new();
        while let Some(c) = r.body.recv().await {
            b.extend_from_slice(&c.unwrap());
        }
        b.freeze()
    }

    #[tokio::test]
    async fn pooled_connections_are_reused_and_capped() {
        use std::sync::atomic::Ordering::SeqCst;
        let dir = std::env::temp_dir().join(format!("nova-pool-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("keep.sock");
        let _ = std::fs::remove_file(&sock);
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        let (accepts, high) = keepalive_fpm(listener, Duration::from_millis(20));
        // max_children 3 -> at most 2 connections.
        let pool = Pool::new(&sock, 3);

        for _ in 0..5 {
            let r = execute_pooled(request(&sock), &pool).await.unwrap();
            assert_eq!(drain(r).await, "ok");
            tokio::time::sleep(Duration::from_millis(5)).await; // reader task recycles
        }
        assert_eq!(
            accepts.load(SeqCst),
            1,
            "sequential requests share one connection"
        );
        assert_eq!(pool.idle_count(), 1);

        // 10 concurrent requests never open more than 2 connections.
        let jobs: Vec<_> = (0..10)
            .map(|_| {
                let pool = Arc::clone(&pool);
                let sock = sock.clone();
                tokio::spawn(async move {
                    drain(execute_pooled(request(&sock), &pool).await.unwrap()).await
                })
            })
            .collect();
        for j in jobs {
            assert_eq!(j.await.unwrap(), "ok");
        }
        assert!(high.load(SeqCst) <= 2, "pool exceeded max_children - 1");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
