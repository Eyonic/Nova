//! Static file responses: MIME types, validators (ETag / Last-Modified),
//! conditional requests, single byte ranges and HEAD.

use crate::{Body, empty, full};
use futures_util::TryStreamExt;
use http::{HeaderMap, HeaderValue, Method, Response, StatusCode, header};
use http_body_util::{BodyExt, StreamBody};
use hyper::body::Frame;
use std::fs::Metadata;
use std::io::SeekFrom;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;

pub struct FileResponse<'a> {
    pub path: &'a Path,
    pub meta: &'a Metadata,
    /// Overrides the MIME type guessed from the extension.
    pub content_type: Option<&'a str>,
    pub cache_control: &'a str,
}

pub fn guess_mime(path: &Path) -> String {
    let mime = mime_guess::from_path(path).first_or_octet_stream();
    match (mime.type_(), mime.subtype()) {
        (mime_guess::mime::TEXT, _)
        | (mime_guess::mime::APPLICATION, mime_guess::mime::JAVASCRIPT) => {
            format!("{}; charset=utf-8", mime.essence_str())
        }
        (_, sub) if sub == "json" || sub == "xml" || sub == "svg" => {
            format!("{}; charset=utf-8", mime.essence_str())
        }
        _ => mime.essence_str().to_string(),
    }
}

fn etag(meta: &Metadata) -> String {
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("\"{:x}-{:x}\"", meta.len(), mtime)
}

fn not_modified(req: &HeaderMap, etag: &str, modified: Option<SystemTime>) -> bool {
    if let Some(inm) = req.get(header::IF_NONE_MATCH).and_then(|v| v.to_str().ok()) {
        // Weak comparison per RFC 9110 §13.1.2.
        return inm
            .split(',')
            .map(str::trim)
            .any(|t| t == "*" || t.trim_start_matches("W/") == etag);
    }
    if let (Some(ims), Some(modified)) = (
        req.get(header::IF_MODIFIED_SINCE)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| httpdate::parse_http_date(v).ok()),
        modified,
    ) {
        let secs = |t: SystemTime| {
            t.duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0)
        };
        return secs(modified) <= secs(ims);
    }
    false
}

#[derive(Debug, PartialEq, Eq)]
pub enum RangeSpec {
    Full,
    Partial(u64, u64), // inclusive
    Unsatisfiable,
}

/// Single-range support; multi-range requests are answered with the full body.
pub fn parse_range(value: Option<&str>, len: u64) -> RangeSpec {
    let Some(spec) = value.and_then(|v| v.trim().strip_prefix("bytes=")) else {
        return RangeSpec::Full;
    };
    if spec.contains(',') {
        return RangeSpec::Full;
    }
    let Some((start, end)) = spec.trim().split_once('-') else {
        return RangeSpec::Full;
    };
    let parsed = match (start.trim(), end.trim()) {
        ("", "") => return RangeSpec::Full,
        ("", n) => match n.parse::<u64>() {
            Ok(0) => return RangeSpec::Unsatisfiable,
            Ok(n) => (len.saturating_sub(n), len.saturating_sub(1)),
            Err(_) => return RangeSpec::Full,
        },
        (s, "") => match s.parse::<u64>() {
            Ok(s) => (s, len.saturating_sub(1)),
            Err(_) => return RangeSpec::Full,
        },
        (s, e) => match (s.parse::<u64>(), e.parse::<u64>()) {
            (Ok(s), Ok(e)) if s <= e => (s, e.min(len.saturating_sub(1))),
            _ => return RangeSpec::Full,
        },
    };
    if len == 0 || parsed.0 >= len {
        RangeSpec::Unsatisfiable
    } else {
        RangeSpec::Partial(parsed.0, parsed.1)
    }
}

pub async fn respond(f: FileResponse<'_>, method: &Method, req: &HeaderMap) -> Response<Body> {
    let len = f.meta.len();
    let tag = etag(f.meta);
    let modified = f.meta.modified().ok();
    let ctype = f
        .content_type
        .map(str::to_owned)
        .unwrap_or_else(|| guess_mime(f.path));

    let mut builder = Response::builder()
        .header(header::ETAG, &tag)
        .header(header::CACHE_CONTROL, f.cache_control)
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff");
    if let Some(m) = modified {
        builder = builder.header(header::LAST_MODIFIED, httpdate::fmt_http_date(m));
    }

    if not_modified(req, &tag, modified) {
        return builder
            .status(StatusCode::NOT_MODIFIED)
            .body(empty())
            .unwrap();
    }

    // If-Range: only honor Range when the validator still matches.
    let range_hdr = req.get(header::RANGE).and_then(|v| v.to_str().ok());
    let range_ok = req
        .get(header::IF_RANGE)
        .and_then(|v| v.to_str().ok())
        .is_none_or(|v| v == tag);
    let range = if range_ok {
        parse_range(range_hdr, len)
    } else {
        RangeSpec::Full
    };

    let (status, start, count) = match range {
        RangeSpec::Full => (StatusCode::OK, 0, len),
        RangeSpec::Partial(s, e) => {
            builder = builder.header(header::CONTENT_RANGE, format!("bytes {s}-{e}/{len}"));
            (StatusCode::PARTIAL_CONTENT, s, e - s + 1)
        }
        RangeSpec::Unsatisfiable => {
            return builder
                .status(StatusCode::RANGE_NOT_SATISFIABLE)
                .header(header::CONTENT_RANGE, format!("bytes */{len}"))
                .body(empty())
                .unwrap();
        }
    };
    builder = builder
        .status(status)
        .header(
            header::CONTENT_TYPE,
            HeaderValue::from_str(&ctype)
                .unwrap_or(HeaderValue::from_static("application/octet-stream")),
        )
        .header(header::CONTENT_LENGTH, count);
    if method == Method::HEAD {
        return builder.body(empty()).unwrap();
    }

    let mut file = match tokio::fs::File::open(f.path).await {
        Ok(file) => file,
        Err(e) => {
            tracing::warn!(path = %f.path.display(), error = %e, "cannot open file");
            return Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(full("internal error"))
                .unwrap();
        }
    };
    if start > 0 && file.seek(SeekFrom::Start(start)).await.is_err() {
        return Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .body(full("internal error"))
            .unwrap();
    }
    let stream = ReaderStream::with_capacity(file.take(count), 64 * 1024).map_ok(Frame::data);
    builder
        .body(StreamBody::new(stream).boxed_unsync())
        .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges() {
        assert_eq!(parse_range(None, 100), RangeSpec::Full);
        assert_eq!(
            parse_range(Some("bytes=0-9"), 100),
            RangeSpec::Partial(0, 9)
        );
        assert_eq!(
            parse_range(Some("bytes=90-"), 100),
            RangeSpec::Partial(90, 99)
        );
        assert_eq!(
            parse_range(Some("bytes=-10"), 100),
            RangeSpec::Partial(90, 99)
        );
        assert_eq!(
            parse_range(Some("bytes=50-500"), 100),
            RangeSpec::Partial(50, 99)
        );
        assert_eq!(
            parse_range(Some("bytes=100-"), 100),
            RangeSpec::Unsatisfiable
        );
        assert_eq!(parse_range(Some("bytes=0-1,5-6"), 100), RangeSpec::Full);
        assert_eq!(parse_range(Some("items=0-1"), 100), RangeSpec::Full);
    }

    #[test]
    fn mime_types() {
        assert_eq!(guess_mime(Path::new("a.html")), "text/html; charset=utf-8");
        assert_eq!(guess_mime(Path::new("a.css")), "text/css; charset=utf-8");
        assert_eq!(guess_mime(Path::new("a.avif")), "image/avif");
        assert_eq!(
            guess_mime(Path::new("a.unknownext")),
            "application/octet-stream"
        );
    }

    #[test]
    fn conditional() {
        let mut h = HeaderMap::new();
        h.insert(header::IF_NONE_MATCH, "W/\"a-b\", \"c-d\"".parse().unwrap());
        assert!(not_modified(&h, "\"c-d\"", None));
        assert!(!not_modified(&h, "\"x\"", None));
    }
}
