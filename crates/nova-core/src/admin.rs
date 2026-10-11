//! Admin API (`/_nova/admin/...`): runtime state, the effective
//! configuration, validation, zero-downtime reload and cache purges.
//!
//! Only clients in `server.admin_allow` reach it, and when
//! `server.admin_token_env` is set they must also send
//! `Authorization: Bearer <token>`. Requests that change something are
//! `POST` and must carry `X-Nova-Admin: 1`, which a browser cannot add to a
//! cross-site form or image request (CSRF from a page an admin visits).
//!
//! | | |
//! |---|---|
//! | `GET /_nova/admin/status` | version, uptime, sites (PHP mode, worker pools, upstream health) |
//! | `GET /_nova/admin/config` | the configuration this process runs, as JSON |
//! | `POST /_nova/admin/config/validate` | check a TOML configuration (body) without applying it |
//! | `POST /_nova/admin/reload` | re-read the configuration file and swap workers without downtime |
//! | `POST /_nova/admin/purge?site=<name>[&tags=a,b]` | drop micro-cached pages (all, or by tag) |

use crate::client::Client;
use crate::dispatch::App;
use http::{Method, Request, Response, StatusCode, header};
use http_body_util::BodyExt;
use nova_config::{Cidr, Config};
use nova_http::{Body, ReqBody, full};
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Largest configuration accepted by `config/validate`.
const MAX_CONFIG: usize = 1 << 20;

pub struct Admin {
    config: Arc<Config>,
    token: Option<String>,
    /// Write end of the supervisor's control pipe (strict isolation).
    control: Option<Mutex<std::fs::File>>,
    started: Instant,
}

impl Admin {
    pub fn new(cfg: &Config, control: Option<std::fs::File>) -> Self {
        Admin {
            config: Arc::new(cfg.clone()),
            token: cfg
                .server
                .admin_token_env
                .as_ref()
                .and_then(|v| std::env::var(v).ok())
                .filter(|t| !t.is_empty()),
            control: control.map(Mutex::new),
            started: Instant::now(),
        }
    }

    pub async fn handle(&self, app: &App, req: Request<ReqBody>, client: Client) -> Response<Body> {
        if !Cidr::any_contains(&app.http.admin_allow, client.ip) {
            return app.not_found();
        }
        if let Some(token) = &self.token {
            let given = req
                .headers()
                .get(header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer "));
            if !given.is_some_and(|g| constant_time_eq(g.as_bytes(), token.as_bytes())) {
                return json(
                    StatusCode::UNAUTHORIZED,
                    serde_json::json!({ "error": "token required" }),
                );
            }
        }
        let post = req.method() == Method::POST;
        if post && req.headers().get("x-nova-admin").is_none() {
            return json(
                StatusCode::FORBIDDEN,
                serde_json::json!({ "error": "send X-Nova-Admin: 1 with admin POST requests" }),
            );
        }
        let path = req.uri().path().to_string();
        match (req.method().clone(), path.as_str()) {
            (Method::GET, "/_nova/admin/status") => json(StatusCode::OK, self.status(app)),
            (Method::GET, "/_nova/admin/config") => json(
                StatusCode::OK,
                serde_json::to_value(&*self.config).unwrap_or_default(),
            ),
            (Method::POST, "/_nova/admin/config/validate") => self.validate(req).await,
            (Method::POST, "/_nova/admin/reload") => self.reload(),
            (Method::POST, "/_nova/admin/purge") => purge(app, req.uri().query().unwrap_or("")),
            _ => app.not_found(),
        }
    }

    fn status(&self, app: &App) -> serde_json::Value {
        let sites: Vec<serde_json::Value> = app
            .sites
            .iter()
            .map(|s| {
                let mut v = serde_json::json!({ "name": s.name, "waf": format!("{:?}", s.waf).to_lowercase() });
                if let Some(php) = &s.php {
                    v["php"] = serde_json::json!({
                        "mode": if php.worker.is_some() { "worker" } else { "fpm" },
                        "idle_workers": php.worker.as_ref().map(|w| w.idle_count()),
                        "micro_cache_secs": php.micro_cache.map(|d| d.as_secs()),
                    });
                }
                if let Some(b) = &s.balancer {
                    v["upstreams"] = b
                        .backends()
                        .iter()
                        .map(|u| serde_json::json!({ "url": u.base, "up": u.is_up(), "in_flight": u.in_flight() }))
                        .collect();
                }
                v
            })
            .collect();
        serde_json::json!({
            "version": env!("CARGO_PKG_VERSION"),
            "mode": self.config.mode.to_string(),
            "uptime_secs": self.started.elapsed().as_secs(),
            "reload": if self.control.is_some() { "available" } else { "needs strict isolation (restart instead)" },
            "sites": sites,
        })
    }

    async fn validate(&self, req: Request<ReqBody>) -> Response<Body> {
        let body = match http_body_util::Limited::new(req.into_body(), MAX_CONFIG)
            .collect()
            .await
        {
            Ok(b) => b.to_bytes(),
            Err(_) => {
                return json(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    serde_json::json!({ "error": "configuration too large or unreadable" }),
                );
            }
        };
        let text = String::from_utf8_lossy(&body);
        match Config::from_toml(&text).and_then(|c| c.validate().map(|()| c)) {
            Ok(c) => json(
                StatusCode::OK,
                serde_json::json!({ "valid": true, "sites": c.sites.len() }),
            ),
            Err(e) => json(
                StatusCode::UNPROCESSABLE_ENTITY,
                serde_json::json!({ "valid": false, "errors": e.to_string().lines().collect::<Vec<_>>() }),
            ),
        }
    }

    fn reload(&self) -> Response<Body> {
        let Some(control) = &self.control else {
            return json(
                StatusCode::CONFLICT,
                serde_json::json!({ "error": "live reload needs strict isolation (start NOVA as root); restart instead" }),
            );
        };
        match control.lock().unwrap().write_all(b"R") {
            // The supervisor validates the file first; an invalid one changes
            // nothing (see the log). This worker is replaced when it succeeds.
            Ok(()) => json(
                StatusCode::ACCEPTED,
                serde_json::json!({ "status": "reloading" }),
            ),
            Err(e) => json(
                StatusCode::INTERNAL_SERVER_ERROR,
                serde_json::json!({ "error": e.to_string() }),
            ),
        }
    }
}

fn purge(app: &App, query: &str) -> Response<Body> {
    let mut site = None;
    let mut tags = None;
    for pair in query.split('&') {
        match pair.split_once('=') {
            Some(("site", v)) => site = Some(v.to_string()),
            Some(("tags", v)) => tags = Some(crate::microcache::parse_tags(&v.replace("%2C", ","))),
            _ => {}
        }
    }
    let Some(site) = site.filter(|s| app.sites.iter().any(|x| x.name == *s)) else {
        return json(
            StatusCode::BAD_REQUEST,
            serde_json::json!({ "error": "unknown or missing site" }),
        );
    };
    match &tags {
        Some(t) => app.micro.purge_tags(&site, t, false),
        None => app.micro.purge_site(&site),
    }
    json(
        StatusCode::OK,
        serde_json::json!({ "purged": site, "tags": tags }),
    )
}

fn json(status: StatusCode, v: serde_json::Value) -> Response<Body> {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::CACHE_CONTROL, "no-store")
        .body(full(v.to_string()))
        .unwrap()
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}
