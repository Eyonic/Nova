//! NOVA PHP runtime: runs real PHP through a supervised PHP-FPM, spoken to
//! with NOVA's own FastCGI client.
//!
//! By default each request gets the classic PHP lifecycle (fresh request
//! state in an FPM worker). Sites that opt into worker mode
//! (`[site.php] worker = ...`) instead keep their application booted in
//! long-running workers speaking the RoadRunner protocol; see [`worker`].

pub mod cgi;
pub mod client;
pub mod fastcgi;
pub mod form;
pub mod fpm;
pub mod goridge;
pub mod manager;
pub mod pool;
pub mod worker;

pub use cgi::{CgiRequest, build_params};
pub use client::{PhpError, PhpRequest, PhpResponse, execute, execute_pooled};
pub use fpm::{Fpm, FpmConfig, Launch, PoolSpec};
pub use pool::Pool;
pub use worker::{WorkerPool, WorkerRequest};
