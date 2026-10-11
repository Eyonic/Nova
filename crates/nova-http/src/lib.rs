//! NOVA HTTP layer: connection handling on top of hyper (protocol parsing),
//! plus the primitives the core dispatcher builds on — safe URL path
//! resolution and static file responses.

pub mod compress;
mod h3;
pub mod path;
pub mod proxy;
pub mod sendfile;
pub mod server;
pub mod static_files;

use bytes::Bytes;
use http_body_util::{
    BodyExt, Empty, Full,
    combinators::{BoxBody, UnsyncBoxBody},
};

/// Response body type used throughout NOVA.
pub type Body = UnsyncBoxBody<Bytes, std::io::Error>;

/// Request body type handed to the [`Handler`] (HTTP/1.1, HTTP/2 and HTTP/3).
pub type ReqBody = BoxBody<Bytes, std::io::Error>;

/// Sends `103 Early Hints` before the final response, found in the
/// extensions of HTTP/2 and HTTP/3 requests (browsers ignore 103 on
/// HTTP/1.1). `send` returns `false` once the final response is out.
#[derive(Clone)]
pub struct EarlyHints(std::sync::Arc<dyn Fn(http::HeaderMap) -> bool + Send + Sync>);

impl EarlyHints {
    pub fn new(f: impl Fn(http::HeaderMap) -> bool + Send + Sync + 'static) -> Self {
        Self(std::sync::Arc::new(f))
    }

    pub fn send(&self, headers: http::HeaderMap) -> bool {
        (self.0)(headers)
    }
}

impl std::fmt::Debug for EarlyHints {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("EarlyHints")
    }
}

fn early_hints_response(headers: http::HeaderMap) -> http::Response<()> {
    let mut r = http::Response::new(());
    *r.status_mut() = http::StatusCode::EARLY_HINTS;
    *r.headers_mut() = headers;
    r
}

pub fn full(data: impl Into<Bytes>) -> Body {
    Full::new(data.into())
        .map_err(|never| match never {})
        .boxed_unsync()
}

pub fn empty() -> Body {
    Empty::new().map_err(|never| match never {}).boxed_unsync()
}

pub use server::{ConnInfo, Handler, IpPredicate, Listener, ServerOptions, TlsSettings, serve};
pub use {quinn, rustls};
