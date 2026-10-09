//! NOVA HTTP layer: connection handling on top of hyper (protocol parsing),
//! plus the primitives the core dispatcher builds on — safe URL path
//! resolution and static file responses.

pub mod path;
pub mod server;
pub mod static_files;

use bytes::Bytes;
use http_body_util::{BodyExt, Empty, Full, combinators::UnsyncBoxBody};

/// Response body type used throughout NOVA.
pub type Body = UnsyncBoxBody<Bytes, std::io::Error>;

pub fn full(data: impl Into<Bytes>) -> Body {
    Full::new(data.into())
        .map_err(|never| match never {})
        .boxed_unsync()
}

pub fn empty() -> Body {
    Empty::new().map_err(|never| match never {}).boxed_unsync()
}

pub use server::{Handler, ServerOptions, serve};
