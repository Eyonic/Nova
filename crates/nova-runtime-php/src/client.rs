//! Executes one PHP request over FastCGI and turns the CGI response into
//! an HTTP status, headers and a streamed body.

use crate::fastcgi::{self, EndRequest, RecordReader, RecordType};
use bytes::{Bytes, BytesMut};
use futures_util::{Stream, StreamExt};
use http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use std::io;
use std::path::Path;
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

pub async fn execute<B>(req: PhpRequest<B>) -> Result<PhpResponse, PhpError>
where
    B: Stream<Item = io::Result<Bytes>> + Send + Unpin + 'static,
{
    let PhpRequest {
        socket,
        params,
        body,
        header_timeout,
        site,
    } = req;
    let stream = connect(&socket).await?;
    let (rd, mut wr) = stream.into_split();

    // Writer: BEGIN_REQUEST, PARAMS, then STDIN streamed from the client.
    // Runs concurrently with the reader so a script that writes output
    // before consuming php://input cannot deadlock against us.
    let (body_err_tx, mut body_err_rx) = mpsc::channel::<io::Error>(1);
    let writer = tokio::spawn(async move {
        let mut buf = BytesMut::with_capacity(8192);
        fastcgi::put_begin_request(&mut buf);
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
                    return Ok(());
                }
            };
            buf.clear();
            fastcgi::put_stream(&mut buf, RecordType::Stdin, &chunk);
            wr.write_all(&buf).await?;
        }
        buf.clear();
        fastcgi::put_record(&mut buf, RecordType::Stdin, &[]);
        wr.write_all(&buf).await?;
        // Dropping the write half would half-close the socket; keep it open
        // until the reader has consumed the response and aborts this task.
        std::future::pending::<()>().await;
        Ok::<_, io::Error>(())
    });

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
        Ok(r) => r.inspect_err(|_| writer.abort())?,
        Err(_) => {
            writer.abort();
            return Err(PhpError::Timeout(header_timeout));
        }
    };
    let (status, headers) = parse_cgi_headers(&head)?;

    let (tx, rx) = mpsc::channel(16);
    tokio::spawn(async move {
        if !rest.is_empty() && tx.send(Ok(rest)).await.is_err() {
            writer.abort();
            return;
        }
        if !finished {
            loop {
                match reader.next().await {
                    Ok(Some(rec)) => match rec.ty {
                        RecordType::Stdout if !rec.content.is_empty() => {
                            if tx.send(Ok(rec.content)).await.is_err() {
                                break; // client went away; dropping the socket aborts the script
                            }
                        }
                        RecordType::Stderr => log_stderr(&site, &rec.content),
                        RecordType::EndRequest => {
                            if let Ok(end) = EndRequest::parse(&rec.content)
                                && end.protocol_status != 0
                            {
                                tracing::warn!(
                                    site,
                                    protocol_status = end.protocol_status,
                                    "FastCGI request not completed"
                                );
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
        writer.abort();
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
async fn connect(socket: &Path) -> Result<UnixStream, PhpError> {
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
}
