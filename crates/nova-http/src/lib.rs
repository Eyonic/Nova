//! NOVA HTTP layer: connection handling on top of hyper (protocol parsing),
//! plus the primitives the core dispatcher builds on — safe URL path
//! resolution and static file responses.

pub mod compress;
mod h3;
pub mod path;
pub mod proxy;
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
