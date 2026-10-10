//! Response compression: `Accept-Encoding` negotiation and on-the-fly
//! brotli / zstd / gzip encoding of compressible responses.
//!
//! Responses are left alone when they are already encoded, partial (206),
//! bodiless (204/304), marked `Cache-Control: no-transform`, an event stream,
//! or smaller than the configured minimum. A compressed response carries a
//! weak ETag (it is a different representation than the identity bytes) and
//! drops `Content-Length` and `Accept-Ranges`.

use crate::Body;
use async_compression::Level;
use async_compression::tokio::bufread::{BrotliEncoder, GzipEncoder, ZstdEncoder};
use futures_util::TryStreamExt;
use http::{HeaderMap, HeaderValue, Method, Response, StatusCode, header};
use http_body_util::{BodyExt, StreamBody};
use hyper::body::Frame;
use std::pin::Pin;
use tokio::io::AsyncRead;
use tokio_util::io::{ReaderStream, StreamReader};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    Brotli,
    Zstd,
    Gzip,
}

impl Encoding {
    /// Server preference when the client rates several encodings equally.
    pub const PREFERENCE: [Encoding; 3] = [Encoding::Brotli, Encoding::Zstd, Encoding::Gzip];

    /// The `Content-Encoding` token.
    pub fn token(self) -> &'static str {
        match self {
            Encoding::Brotli => "br",
            Encoding::Zstd => "zstd",
            Encoding::Gzip => "gzip",
        }
    }

    /// File extension of a precompressed sibling (`app.css.br`).
    pub fn extension(self) -> &'static str {
        match self {
            Encoding::Brotli => "br",
            Encoding::Zstd => "zst",
            Encoding::Gzip => "gz",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Options {
    /// Responses with a known length below this are sent uncompressed.
    pub min_size: u64,
}

/// Encodings the client accepts, best first: by q-value, then by
/// [`Encoding::PREFERENCE`]. `*` covers encodings not listed explicitly.
pub fn accepted(accept_encoding: Option<&str>) -> Vec<Encoding> {
    let Some(value) = accept_encoding else {
        return Vec::new();
    };
    let mut explicit: Vec<(String, f32)> = Vec::new();
    for item in value.split(',') {
        let mut parts = item.split(';');
        let token = parts.next().unwrap_or("").trim().to_ascii_lowercase();
        if token.is_empty() {
            continue;
        }
        let q = parts
            .filter_map(|p| p.trim().strip_prefix("q=").or(p.trim().strip_prefix("Q=")))
            .find_map(|q| q.trim().parse::<f32>().ok())
            .unwrap_or(1.0);
        explicit.push((token, q));
    }
    let q_of = |name: &str| explicit.iter().find(|(t, _)| t == name).map(|(_, q)| *q);
    let star = q_of("*").unwrap_or(0.0);
    let mut out: Vec<(Encoding, f32)> = Encoding::PREFERENCE
        .iter()
        .map(|&e| {
            let q = match e {
                // `x-gzip` is an old alias browsers may still send.
                Encoding::Gzip => q_of("gzip").or_else(|| q_of("x-gzip")),
                _ => q_of(e.token()),
            };
            (e, q.unwrap_or(star))
        })
        .filter(|(_, q)| *q > 0.0)
        .collect();
    // Stable sort keeps the server preference among equal q-values.
    out.sort_by(|a, b| b.1.total_cmp(&a.1));
    out.into_iter().map(|(e, _)| e).collect()
}

/// Whether a media type benefits from compression. Images (other than SVG
/// and icons), audio, video, archives and WOFF fonts are already compressed.
pub fn is_compressible(content_type: &str) -> bool {
    let essence = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if essence == "text/event-stream" {
        // Event streams must reach the client as soon as they are written.
        return false;
    }
    essence.starts_with("text/")
        || essence.ends_with("+json")
        || essence.ends_with("+xml")
        || matches!(
            essence.as_str(),
            "application/javascript"
                | "application/x-javascript"
                | "application/ecmascript"
                | "application/json"
                | "application/xml"
                | "application/wasm"
                | "application/x-httpd-php"
                | "application/vnd.ms-fontobject"
                | "image/x-icon"
                | "image/vnd.microsoft.icon"
                | "image/bmp"
                | "font/ttf"
                | "font/otf"
                | "font/collection"
        )
}

/// Add `token` to the `Vary` header unless it is already covered.
pub fn add_vary(headers: &mut HeaderMap, token: &'static str) {
    let covered = headers.get_all(header::VARY).iter().any(|v| {
        v.to_str().is_ok_and(|s| {
            s.split(',')
                .map(str::trim)
                .any(|t| t == "*" || t.eq_ignore_ascii_case(token))
        })
    });
    if !covered {
        headers.append(header::VARY, HeaderValue::from_static(token));
    }
}

fn weaken_etag(headers: &mut HeaderMap) {
    if let Some(tag) = headers.get(header::ETAG).and_then(|v| v.to_str().ok())
        && tag.starts_with('"')
        && let Ok(weak) = HeaderValue::from_str(&format!("W/{tag}"))
    {
        headers.insert(header::ETAG, weak);
    }
}

/// Compress `resp` for a client that sent `accept_encoding`, when worthwhile.
pub fn apply(
    mut resp: Response<Body>,
    method: &Method,
    accept_encoding: Option<&str>,
    opts: &Options,
) -> Response<Body> {
    let headers = resp.headers();
    let compressible = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(is_compressible);
    if !compressible || headers.contains_key(header::CONTENT_ENCODING) {
        return resp;
    }
    let no_transform = headers
        .get_all(header::CACHE_CONTROL)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .any(|v| v.to_ascii_lowercase().contains("no-transform"));
    if no_transform {
        return resp;
    }
    // The representation depends on Accept-Encoding from here on, even when
    // this particular response stays uncompressed.
    add_vary(resp.headers_mut(), "Accept-Encoding");

    let status = resp.status();
    let Some(encoding) = accepted(accept_encoding).first().copied() else {
        return resp;
    };
    if status == StatusCode::NOT_MODIFIED {
        // Keep the validator identical to the one the 200 carried.
        weaken_etag(resp.headers_mut());
        return resp;
    }
    let length = resp
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    if status.is_informational()
        || status == StatusCode::NO_CONTENT
        || status == StatusCode::PARTIAL_CONTENT
        || resp.headers().contains_key(header::CONTENT_RANGE)
        || length.is_some_and(|l| l < opts.min_size)
    {
        return resp;
    }

    let h = resp.headers_mut();
    h.remove(header::CONTENT_LENGTH);
    h.remove(header::ACCEPT_RANGES);
    h.insert(
        header::CONTENT_ENCODING,
        HeaderValue::from_static(encoding.token()),
    );
    weaken_etag(h);
    if method == Method::HEAD {
        return resp;
    }
    // The encoded body replaces the file's bytes: never send the file raw.
    resp.extensions_mut().remove::<hyper::ext::SendFile>();
    resp.map(|body| encode(body, encoding))
}

fn encode(body: Body, encoding: Encoding) -> Body {
    let reader = StreamReader::new(body.into_data_stream());
    // Levels tuned for on-the-fly use: close to the best ratio per CPU cycle.
    let encoder: Pin<Box<dyn AsyncRead + Send>> = match encoding {
        Encoding::Brotli => Box::pin(BrotliEncoder::with_quality(reader, Level::Precise(5))),
        Encoding::Zstd => Box::pin(ZstdEncoder::with_quality(reader, Level::Precise(3))),
        Encoding::Gzip => Box::pin(GzipEncoder::with_quality(reader, Level::Precise(6))),
    };
    let stream = ReaderStream::with_capacity(encoder, 32 * 1024).map_ok(Frame::data);
    StreamBody::new(stream).boxed_unsync()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::full;
    use async_compression::tokio::bufread::{BrotliDecoder, GzipDecoder, ZstdDecoder};
    use tokio::io::AsyncReadExt;

    const OPTS: Options = Options { min_size: 1024 };

    fn page(len: usize) -> Response<Body> {
        Response::builder()
            .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
            .header(header::CONTENT_LENGTH, len)
            .header(header::ETAG, "\"abc\"")
            .header(header::ACCEPT_RANGES, "bytes")
            .body(full(
                "<p>hello nova</p>".repeat(len / 17 + 1)[..len].to_string(),
            ))
            .unwrap()
    }

    async fn bytes(resp: Response<Body>) -> Vec<u8> {
        resp.into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec()
    }

    #[test]
    fn negotiation() {
        use Encoding::*;
        assert_eq!(accepted(None), vec![]);
        assert_eq!(
            accepted(Some("gzip, deflate, br, zstd")),
            vec![Brotli, Zstd, Gzip]
        );
        assert_eq!(accepted(Some("gzip;q=1, br;q=0.5")), vec![Gzip, Brotli]);
        assert_eq!(accepted(Some("br;q=0, *")), vec![Zstd, Gzip]);
        assert_eq!(accepted(Some("identity")), vec![]);
        assert_eq!(accepted(Some("x-gzip")), vec![Gzip]);
        assert_eq!(accepted(Some("*;q=0")), vec![]);
    }

    #[test]
    fn media_types() {
        assert!(is_compressible("text/html; charset=utf-8"));
        assert!(is_compressible("application/javascript; charset=utf-8"));
        assert!(is_compressible("image/svg+xml"));
        assert!(is_compressible("application/manifest+json"));
        assert!(!is_compressible("text/event-stream; charset=utf-8"));
        assert!(!is_compressible("image/avif"));
        assert!(!is_compressible("font/woff2"));
        assert!(!is_compressible("application/zip"));
    }

    #[tokio::test]
    async fn round_trips_every_encoding() {
        let original = bytes(page(5000)).await;
        for (ae, enc) in [("br", "br"), ("zstd", "zstd"), ("gzip", "gzip")] {
            let resp = apply(page(5000), &Method::GET, Some(ae), &OPTS);
            let h = resp.headers();
            assert_eq!(h[header::CONTENT_ENCODING], enc);
            assert_eq!(h[header::VARY], "Accept-Encoding");
            assert_eq!(h[header::ETAG], "W/\"abc\"");
            assert!(!h.contains_key(header::CONTENT_LENGTH));
            assert!(!h.contains_key(header::ACCEPT_RANGES));
            let packed = bytes(resp).await;
            assert!(packed.len() < original.len() / 4, "{enc}: {}", packed.len());
            let mut out = Vec::new();
            match enc {
                "br" => BrotliDecoder::new(&packed[..]).read_to_end(&mut out).await,
                "zstd" => ZstdDecoder::new(&packed[..]).read_to_end(&mut out).await,
                _ => GzipDecoder::new(&packed[..]).read_to_end(&mut out).await,
            }
            .unwrap();
            assert_eq!(out, original);
        }
    }

    #[tokio::test]
    async fn leaves_ineligible_responses_alone() {
        let small = apply(page(100), &Method::GET, Some("br"), &OPTS);
        assert!(!small.headers().contains_key(header::CONTENT_ENCODING));
        assert_eq!(small.headers()[header::VARY], "Accept-Encoding");

        let none = apply(page(5000), &Method::GET, None, &OPTS);
        assert!(!none.headers().contains_key(header::CONTENT_ENCODING));
        assert_eq!(none.headers()[header::ETAG], "\"abc\"");

        let mut partial = page(5000);
        *partial.status_mut() = StatusCode::PARTIAL_CONTENT;
        let partial = apply(partial, &Method::GET, Some("br"), &OPTS);
        assert!(!partial.headers().contains_key(header::CONTENT_ENCODING));

        let mut encoded = page(5000);
        encoded
            .headers_mut()
            .insert(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"));
        let encoded = apply(encoded, &Method::GET, Some("br"), &OPTS);
        assert_eq!(encoded.headers()[header::CONTENT_ENCODING], "gzip");

        let mut opt_out = page(5000);
        opt_out.headers_mut().insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static("no-transform"),
        );
        let opt_out = apply(opt_out, &Method::GET, Some("br"), &OPTS);
        assert!(!opt_out.headers().contains_key(header::CONTENT_ENCODING));

        let mut not_modified = page(0);
        *not_modified.status_mut() = StatusCode::NOT_MODIFIED;
        let not_modified = apply(not_modified, &Method::GET, Some("br"), &OPTS);
        assert!(
            !not_modified
                .headers()
                .contains_key(header::CONTENT_ENCODING)
        );
        assert_eq!(not_modified.headers()[header::ETAG], "W/\"abc\"");
    }

    #[test]
    fn vary_is_merged() {
        let mut h = HeaderMap::new();
        h.insert(
            header::VARY,
            HeaderValue::from_static("Accept, accept-encoding"),
        );
        add_vary(&mut h, "Accept-Encoding");
        assert_eq!(h.get_all(header::VARY).iter().count(), 1);
        add_vary(&mut h, "Nova-Live");
        assert_eq!(h.get_all(header::VARY).iter().count(), 2);
    }
}
