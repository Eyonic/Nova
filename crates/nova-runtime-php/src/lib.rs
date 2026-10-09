//! NOVA PHP runtime: runs real PHP through a supervised PHP-FPM, spoken to
//! with NOVA's own FastCGI client.
//!
//! Each request gets the classic PHP lifecycle (fresh request state in an
//! FPM worker); NOVA never keeps application state alive between requests.

pub mod cgi;
pub mod client;
pub mod fastcgi;
pub mod fpm;

pub use cgi::{CgiRequest, build_params};
pub use client::{PhpError, PhpRequest, PhpResponse, execute};
pub use fpm::{Fpm, FpmConfig, Launch, PoolSpec};
