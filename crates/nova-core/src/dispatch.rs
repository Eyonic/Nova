//! The request lifecycle:
//!
//! 1. internal endpoints (`/_nova/...`)
//! 2. site lookup by Host
//! 3. lexical path validation (traversal, hidden files)
//! 4. target resolution: file → directory index → `/script.php/path-info`
//!    → front controller → 404, with canonical containment checks
//! 5. execution: optimized image, static file, or PHP via FastCGI
//! 6. response headers, metrics and the access log line

use crate::live::{self, LiveHub};
use crate::metrics::{Kind, Metrics};
use crate::sites::{Site, SitePhp, Sites, strip_port};
use bytes::Bytes;
use futures_util::StreamExt;
use http::{HeaderValue, Method, Request, Response, StatusCode, Version, header};
use http_body_util::{BodyExt, BodyStream, Limited, StreamBody};
use hyper::body::{Body as _, Frame, Incoming};
use nova_config::Mode;
use nova_http::path::{self, PathError, SafePath};
use nova_http::static_files::{self, FileResponse};
use nova_http::{Body, empty, full};
use nova_optimize::Optimizer;
use nova_optimize::manifest::Format;
use nova_runtime_php::{CgiRequest, PhpError, PhpRequest};
use std::fs::Metadata;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::watch;

pub struct App {
    pub mode: Mode,
    pub sites: Sites,
    pub optimizer: Option<Arc<Optimizer>>,
    pub metrics: Metrics,
    pub max_body: u64,
    pub server_port: u16,
    pub metrics_enabled: bool,
    /// `None` when no site uses PHP.
    pub php_ready: Option<watch::Receiver<bool>>,
    /// Database endpoints checked by the readiness probe: (service, host, port).
    pub databases: Vec<(String, String, u16)>,
    pub shutting_down: AtomicBool,
    pub live: Arc<LiveHub>,
    boot: u32,
    seq: AtomicU64,
}

impl App {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        mode: Mode,
        sites: Sites,
        optimizer: Option<Arc<Optimizer>>,
        max_body: u64,
        server_port: u16,
        metrics_enabled: bool,
        php_ready: Option<watch::Receiver<bool>>,
        databases: Vec<(String, String, u16)>,
        live: Arc<LiveHub>,
    ) -> Self {
        let boot = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos() ^ (d.as_secs() as u32))
            .unwrap_or(0);
        Self {
            mode,
            sites,
            optimizer,
            metrics: Metrics::default(),
            max_body,
            server_port,
            metrics_enabled,
            php_ready,
            databases,
            shutting_down: AtomicBool::new(false),
            live,
            boot,
            seq: AtomicU64::new(0),
        }
    }

    fn next_request_id(&self) -> String {
        format!(
            "{:08x}{:08x}",
            self.boot,
            self.seq.fetch_add(1, Ordering::Relaxed)
        )
    }
}

/// Where a request ends up after resolution.
#[derive(Debug)]
enum Target {
    Static(PathBuf, Metadata),
    Php {
        script: PathBuf,
        script_name: String,
        path_info: String,
    },
    Redirect(String),
    NotFound,
}

impl nova_http::Handler for App {
    async fn handle(&self, req: Request<Incoming>, peer: SocketAddr) -> Response<Body> {
        let start = Instant::now();
        let id = self.next_request_id();
        let method = req.method().clone();
        let uri_path = req.uri().path().to_owned();
        let (mut resp, kind, site) = self.route(req, peer, &id).await;

        let h = resp.headers_mut();
        h.insert(header::SERVER, HeaderValue::from_static("nova"));
        if let Ok(v) = HeaderValue::from_str(&id) {
            h.insert("x-request-id", v);
        }
        let status = resp.status().as_u16();
        self.metrics.record(kind, status);
        if kind != Kind::Internal {
            tracing::info!(
                target: "nova::access",
                request_id = %id,
                site = site.unwrap_or("-"),
                %peer,
                method = %method,
                path = %uri_path,
                status,
                kind = kind.as_str(),
                duration_ms = start.elapsed().as_secs_f64() * 1e3,
            );
        }
        resp
    }
}

impl App {
    async fn route<'a>(
        &'a self,
        req: Request<Incoming>,
        peer: SocketAddr,
        id: &str,
    ) -> (Response<Body>, Kind, Option<&'a str>) {
        // HTTP/2 carries the host in :authority, HTTP/1.1 in Host.
        let host = req
            .headers()
            .get(header::HOST)
            .and_then(|v| v.to_str().ok())
            .or_else(|| req.uri().authority().map(|a| a.as_str()));

        if req.uri().path().starts_with("/_nova/live") && self.live.enabled() {
            if req.method() != Method::GET && req.method() != Method::HEAD {
                return (
                    self.error(StatusCode::METHOD_NOT_ALLOWED, "Method not allowed.", None),
                    Kind::Error,
                    None,
                );
            }
            match req.uri().path() {
                "/_nova/live.js" => {
                    return (
                        LiveHub::runtime_response(req.uri().query()),
                        Kind::Internal,
                        None,
                    );
                }
                "/_nova/live/events" => {
                    let Some(site) = self.sites.lookup(host) else {
                        return (self.not_found(), Kind::Error, None);
                    };
                    let channels = query_param(req.uri().query(), "channels")
                        .map(|c| {
                            percent_encoding::percent_decode_str(c)
                                .decode_utf8_lossy()
                                .into_owned()
                        })
                        .unwrap_or_default();
                    let resp = match self.live.subscribe(&site.name, &channels, peer.ip()) {
                        Ok(r) => r,
                        Err(e) => LiveHub::error_response(e),
                    };
                    return (resp, Kind::Internal, Some(site.name.as_str()));
                }
                _ => {}
            }
        }
        if req.uri().path().starts_with("/_nova/") {
            return (self.internal(&req).await, Kind::Internal, None);
        }

        let Some(site) = self.sites.lookup(host) else {
            return (
                self.error(
                    StatusCode::MISDIRECTED_REQUEST,
                    "No site is configured for this host.",
                    None,
                ),
                Kind::Error,
                None,
            );
        };
        let name = Some(site.name.as_str());

        let safe = match path::resolve(req.uri().path()) {
            Ok(p) => p,
            Err(PathError::BadRequest) => {
                return (
                    self.error(StatusCode::BAD_REQUEST, "Malformed request path.", None),
                    Kind::Error,
                    name,
                );
            }
            Err(PathError::Hidden) => return (self.not_found(), Kind::Error, name),
        };

        match resolve_target(site, &safe, req.uri().query()).await {
            Target::Redirect(location) => {
                let resp = Response::builder()
                    .status(StatusCode::MOVED_PERMANENTLY)
                    .header(header::LOCATION, location)
                    .body(empty())
                    .unwrap();
                (resp, Kind::Static, name)
            }
            Target::NotFound => (self.not_found(), Kind::Error, name),
            Target::Php {
                script,
                script_name,
                path_info,
            } => match &site.php {
                Some(php) => {
                    let resp = self
                        .run_php(site, php, req, peer, id, &script, &script_name, &path_info)
                        .await;
                    (resp, Kind::Php, name)
                }
                // PHP disabled: never leak source code.
                None => (self.not_found(), Kind::Error, name),
            },
            Target::Static(file, meta) => {
                if req.method() != Method::GET && req.method() != Method::HEAD {
                    let mut resp = self.error(
                        StatusCode::METHOD_NOT_ALLOWED,
                        "Method not allowed for static files.",
                        None,
                    );
                    resp.headers_mut()
                        .insert(header::ALLOW, HeaderValue::from_static("GET, HEAD"));
                    return (resp, Kind::Error, name);
                }
                self.serve_static(site, &req, &file, &meta)
                    .await
                    .map_name(name)
            }
        }
    }

    async fn serve_static(
        &self,
        site: &Site,
        req: &Request<Incoming>,
        file: &Path,
        meta: &Metadata,
    ) -> (Response<Body>, Kind) {
        let cache = if self.mode.is_dev() {
            "no-cache"
        } else {
            "public, max-age=0, must-revalidate"
        };
        let optimizable = Format::from_path(file).is_some();

        if optimizable
            && site.optimize
            && let Some(opt) = &self.optimizer
            && let Ok(rel) = file.strip_prefix(&site.root)
            && let Some(rel) = rel.to_str()
        {
            let accept = req
                .headers()
                .get(header::ACCEPT)
                .and_then(|v| v.to_str().ok());
            let width = query_param(req.uri().query(), "w").and_then(|w| w.parse::<u32>().ok());
            if let Some(sel) = opt.select(&site.name, rel, meta, accept, width)
                && let Ok(vmeta) = tokio::fs::metadata(&sel.path).await
            {
                let fr = FileResponse {
                    path: &sel.path,
                    meta: &vmeta,
                    content_type: Some(sel.format.mime()),
                    cache_control: cache,
                };
                let mut resp = static_files::respond(fr, req.method(), req.headers()).await;
                resp.headers_mut()
                    .insert(header::VARY, HeaderValue::from_static("Accept"));
                if resp.status() == StatusCode::OK {
                    self.metrics
                        .image_saved(meta.len().saturating_sub(vmeta.len()));
                }
                return (resp, Kind::Image);
            }
        }

        let fr = FileResponse {
            path: file,
            meta,
            content_type: None,
            cache_control: cache,
        };
        let mut resp = static_files::respond(fr, req.method(), req.headers()).await;
        if optimizable {
            resp.headers_mut()
                .insert(header::VARY, HeaderValue::from_static("Accept"));
        }
        (resp, Kind::Static)
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_php(
        &self,
        site: &Site,
        php: &SitePhp,
        req: Request<Incoming>,
        peer: SocketAddr,
        id: &str,
        script: &Path,
        script_name: &str,
        path_info: &str,
    ) -> Response<Body> {
        let started = Instant::now();
        let (parts, body) = req.into_parts();
        let declared = parts
            .headers
            .get(header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok());
        if declared.is_some_and(|l| l > self.max_body) {
            return self.error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "Request body too large.",
                None,
            );
        }

        // PHP needs CONTENT_LENGTH up front. With a declared length the body
        // is streamed; otherwise (chunked) it is buffered up to the limit.
        type BodyStreamBox =
            std::pin::Pin<Box<dyn futures_util::Stream<Item = io::Result<Bytes>> + Send>>;
        let (stream, content_length): (BodyStreamBox, Option<u64>) =
            if declared.is_some() || body.is_end_stream() {
                let s = BodyStream::new(body).filter_map(|frame| async move {
                    match frame {
                        Ok(f) => f.into_data().ok().map(Ok),
                        Err(e) => Some(Err(io::Error::other(e))),
                    }
                });
                (Box::pin(s), declared)
            } else {
                match Limited::new(body, self.max_body as usize).collect().await {
                    Ok(c) => {
                        let bytes = c.to_bytes();
                        let len = bytes.len() as u64;
                        (Box::pin(futures_util::stream::iter([Ok(bytes)])), Some(len))
                    }
                    Err(e)
                        if e.downcast_ref::<http_body_util::LengthLimitError>()
                            .is_some() =>
                    {
                        return self.error(
                            StatusCode::PAYLOAD_TOO_LARGE,
                            "Request body too large.",
                            None,
                        );
                    }
                    Err(_) => {
                        return self.error(
                            StatusCode::BAD_REQUEST,
                            "Could not read request body.",
                            None,
                        );
                    }
                }
            };

        let host = parts
            .headers
            .get(header::HOST)
            .and_then(|v| v.to_str().ok())
            .or_else(|| parts.uri.authority().map(|a| a.as_str()))
            .unwrap_or("");
        let request_uri = parts
            .uri
            .path_and_query()
            .map(|p| p.as_str())
            .unwrap_or("/");
        let protocol = match parts.version {
            Version::HTTP_10 => "HTTP/1.0",
            Version::HTTP_2 => "HTTP/2.0",
            Version::HTTP_3 => "HTTP/3.0",
            _ => "HTTP/1.1",
        };
        let params = nova_runtime_php::build_params(&CgiRequest {
            method: parts.method.as_str(),
            request_uri,
            query: parts.uri.query().unwrap_or(""),
            script_name,
            script_filename: script,
            path_info,
            document_root: &site.root,
            server_name: strip_port(host),
            server_port: self.server_port,
            server_protocol: protocol,
            remote_addr: peer,
            https: false,
            headers: &parts.headers,
            content_length,
            request_id: id,
        });

        let result = nova_runtime_php::execute(PhpRequest {
            socket: php.socket.clone(),
            params,
            body: stream,
            header_timeout: php.timeout + Duration::from_secs(5),
            site: site.name.clone(),
        })
        .await;
        let micros = started.elapsed().as_micros() as u64;

        let r = match result {
            Ok(r) => r,
            Err(e) => {
                self.metrics.php_done(micros, false);
                tracing::warn!(request_id = id, site = site.name, error = %e, "PHP request failed");
                let detail = Some(e.to_string());
                return match e {
                    PhpError::Unavailable(_) => self.error(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "The PHP runtime is not available.",
                        detail,
                    ),
                    PhpError::Timeout(_) => self.error(
                        StatusCode::GATEWAY_TIMEOUT,
                        "The PHP script did not respond in time.",
                        detail,
                    ),
                    PhpError::Protocol(_) => self.error(
                        StatusCode::BAD_GATEWAY,
                        "The PHP runtime returned an invalid response.",
                        detail,
                    ),
                    PhpError::Body(_) => self.error(
                        StatusCode::BAD_REQUEST,
                        "Could not read request body.",
                        detail,
                    ),
                };
            }
        };
        self.metrics.php_done(micros, r.status.is_server_error());

        let body = futures_util::stream::unfold(r.body, |mut rx| async move {
            rx.recv().await.map(|item| (item.map(Frame::data), rx))
        });
        let mut resp = Response::new(StreamBody::new(body).boxed_unsync());
        *resp.status_mut() = r.status;
        let headers = resp.headers_mut();
        for (name, value) in r.headers.iter() {
            // Hop-by-hop headers are owned by the HTTP layer.
            if matches!(
                name.as_str(),
                "connection" | "transfer-encoding" | "keep-alive" | "upgrade" | "trailer"
            ) {
                continue;
            }
            // NOVA Live publish requests are for NOVA, never for the client.
            if name.as_str() == live::PUBLISH_HEADER {
                if r.status.as_u16() < 400 {
                    for channel in live::parse_channels(value.to_str().unwrap_or("")) {
                        self.live.publish(&site.name, &channel);
                    }
                }
                continue;
            }
            headers.append(name.clone(), value.clone());
        }
        // PHP may answer NOVA Live requests with a fragment: caches must
        // keep fragments and full pages apart.
        headers.append(
            header::VARY,
            HeaderValue::from_static("Nova-Live, Nova-Target"),
        );
        resp
    }

    async fn internal(&self, req: &Request<Incoming>) -> Response<Body> {
        let json = |status: StatusCode, v: serde_json::Value| {
            Response::builder()
                .status(status)
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::CACHE_CONTROL, "no-store")
                .body(full(v.to_string()))
                .unwrap()
        };
        match req.uri().path() {
            "/_nova/health/live" => json(StatusCode::OK, serde_json::json!({ "status": "ok" })),
            "/_nova/health/ready" => {
                let (ok, checks) = self.readiness().await;
                let status = if ok {
                    StatusCode::OK
                } else {
                    StatusCode::SERVICE_UNAVAILABLE
                };
                json(
                    status,
                    serde_json::json!({ "status": if ok { "ready" } else { "not_ready" }, "checks": checks }),
                )
            }
            "/_nova/metrics" if self.metrics_enabled => {
                let (mut assets, mut variants, mut errors) = (0u64, 0u64, 0u64);
                if let Some(opt) = &self.optimizer {
                    for site in self.sites.iter() {
                        if let Some(m) = opt.manifest(&site.name) {
                            assets += m.assets.len() as u64;
                            variants += m
                                .assets
                                .values()
                                .map(|a| a.variants.len() as u64)
                                .sum::<u64>();
                            errors += m.errors.len() as u64;
                        }
                    }
                }
                let php_ready = self
                    .php_ready
                    .as_ref()
                    .map(|r| *r.borrow() as u64)
                    .unwrap_or(0);
                let body = self.metrics.render(&[
                    ("nova_optimize_assets", "Optimized source images.", assets),
                    (
                        "nova_optimize_variants",
                        "Generated image variants.",
                        variants,
                    ),
                    (
                        "nova_optimize_errors",
                        "Images that failed optimization.",
                        errors,
                    ),
                    ("nova_php_ready", "1 when all PHP pools answer.", php_ready),
                    (
                        "nova_live_connections",
                        "Open NOVA Live event streams.",
                        self.live.connections() as u64,
                    ),
                    (
                        "nova_live_published_total",
                        "NOVA Live invalidations published.",
                        self.live.published(),
                    ),
                ]);
                Response::builder()
                    .header(header::CONTENT_TYPE, "text/plain; version=0.0.4")
                    .header(header::CACHE_CONTROL, "no-store")
                    .body(full(body))
                    .unwrap()
            }
            "/_nova/optimize/status" if self.metrics_enabled => {
                let mut out = serde_json::Map::new();
                if let Some(opt) = &self.optimizer {
                    for site in self.sites.iter() {
                        let v = match opt.manifest(&site.name) {
                            Some(m) => serde_json::json!({
                                "generated_at": m.generated_at,
                                "assets": m.assets.len(),
                                "variants": m.assets.values().map(|a| a.variants.len()).sum::<usize>(),
                                "errors": m.errors.len(),
                                "warnings": m.warnings,
                            }),
                            None => serde_json::json!({ "assets": 0, "pending": true }),
                        };
                        out.insert(site.name.clone(), v);
                    }
                }
                json(StatusCode::OK, serde_json::Value::Object(out))
            }
            _ => self.not_found(),
        }
    }

    pub async fn readiness(&self) -> (bool, serde_json::Value) {
        let mut ok = true;
        let mut checks = serde_json::Map::new();
        let draining = self.shutting_down.load(Ordering::Relaxed);
        ok &= !draining;
        checks.insert("accepting".into(), (!draining).into());
        if let Some(r) = &self.php_ready {
            let ready = *r.borrow();
            ok &= ready;
            checks.insert("php".into(), ready.into());
        }
        for (name, host, port) in &self.databases {
            let up = matches!(
                tokio::time::timeout(
                    Duration::from_secs(1),
                    tokio::net::TcpStream::connect((host.as_str(), *port))
                )
                .await,
                Ok(Ok(_))
            );
            ok &= up;
            checks.insert(format!("database:{name}"), up.into());
        }
        (ok, serde_json::Value::Object(checks))
    }

    fn not_found(&self) -> Response<Body> {
        self.error(
            StatusCode::NOT_FOUND,
            "The requested resource was not found.",
            None,
        )
    }

    /// Error page. Internal details are shown only in development mode.
    fn error(&self, status: StatusCode, message: &str, detail: Option<String>) -> Response<Body> {
        let detail = detail
            .filter(|_| self.mode.is_dev())
            .map(|d| format!("<pre>{}</pre>", html_escape(&d)))
            .unwrap_or_default();
        let reason = status.canonical_reason().unwrap_or("");
        let body = format!(
            "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><title>{code} {reason}</title>\
             <style>body{{font:16px/1.5 system-ui,sans-serif;max-width:40rem;margin:4rem auto;padding:0 1rem;color:#222}}\
             pre{{background:#f4f4f4;padding:1rem;overflow:auto}}</style></head>\
             <body><h1>{code} {reason}</h1><p>{msg}</p>{detail}<hr><small>NOVA</small></body></html>",
            code = status.as_u16(),
            msg = html_escape(message),
        );
        Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
            .header(header::CACHE_CONTROL, "no-store")
            .body(full(body))
            .unwrap()
    }
}

trait WithName<'a> {
    fn map_name(self, name: Option<&'a str>) -> (Response<Body>, Kind, Option<&'a str>);
}

impl<'a> WithName<'a> for (Response<Body>, Kind) {
    fn map_name(self, name: Option<&'a str>) -> (Response<Body>, Kind, Option<&'a str>) {
        (self.0, self.1, name)
    }
}

fn is_php(p: &Path) -> bool {
    p.extension().is_some_and(|e| e.eq_ignore_ascii_case("php"))
}

/// Canonicalize and require the result to stay inside the document root.
async fn contained(site: &Site, p: &Path) -> Option<PathBuf> {
    let c = tokio::fs::canonicalize(p).await.ok()?;
    c.starts_with(&site.root).then_some(c)
}

async fn file_meta(p: &Path) -> Option<Metadata> {
    tokio::fs::metadata(p).await.ok().filter(|m| m.is_file())
}

async fn resolve_target(site: &Site, safe: &SafePath, query: Option<&str>) -> Target {
    let full = site.root.join(safe.to_path());
    match tokio::fs::metadata(&full).await {
        Ok(m) if m.is_file() => {
            let Some(c) = contained(site, &full).await else {
                return Target::NotFound;
            };
            if is_php(&c) {
                return Target::Php {
                    script: c,
                    script_name: safe.url(),
                    path_info: String::new(),
                };
            }
            return Target::Static(c, m);
        }
        Ok(m) if m.is_dir() => {
            if !safe.segments.is_empty() && !safe.trailing_slash {
                let q = query.map(|q| format!("?{q}")).unwrap_or_default();
                return Target::Redirect(format!("{}/{q}", safe.url()));
            }
            let dir_url = if safe.segments.is_empty() {
                "/".to_string()
            } else {
                format!("{}/", safe.url())
            };
            let indexes: &[&str] = if site.php.is_some() {
                &["index.html", "index.htm", "index.php"]
            } else {
                &["index.html", "index.htm"]
            };
            for index in indexes {
                let f = full.join(index);
                if let Some(meta) = file_meta(&f).await
                    && let Some(c) = contained(site, &f).await
                {
                    if is_php(&c) {
                        return Target::Php {
                            script: c,
                            script_name: format!("{dir_url}{index}"),
                            path_info: String::new(),
                        };
                    }
                    return Target::Static(c, meta);
                }
            }
        }
        _ => {
            // `/index.php/some/path`: the first existing .php segment is the script.
            if site.php.is_some() && safe.segments.len() > 1 {
                for i in 0..safe.segments.len() - 1 {
                    if !safe.segments[i].to_ascii_lowercase().ends_with(".php") {
                        continue;
                    }
                    let script: PathBuf = site
                        .root
                        .join(safe.segments[..=i].iter().collect::<PathBuf>());
                    if file_meta(&script).await.is_some()
                        && let Some(c) = contained(site, &script).await
                    {
                        let mut path_info = format!("/{}", safe.segments[i + 1..].join("/"));
                        if safe.trailing_slash {
                            path_info.push('/');
                        }
                        return Target::Php {
                            script: c,
                            script_name: format!("/{}", safe.segments[..=i].join("/")),
                            path_info,
                        };
                    }
                    break;
                }
            }
        }
    }

    // Pretty URLs: hand everything else to the front controller.
    if let Some(fc) = site
        .php
        .as_ref()
        .and_then(|p| p.front_controller.as_deref())
    {
        let f = site.root.join(fc.trim_start_matches('/'));
        if file_meta(&f).await.is_some()
            && let Some(c) = contained(site, &f).await
        {
            return Target::Php {
                script: c,
                script_name: fc.to_string(),
                path_info: String::new(),
            };
        }
    }
    Target::NotFound
}

fn query_param<'a>(query: Option<&'a str>, key: &str) -> Option<&'a str> {
    query?.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
        (k == key).then_some(v)
    })
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn site(root: &Path, php: bool, fc: Option<&str>) -> Site {
        Site {
            name: "t".into(),
            root: std::fs::canonicalize(root).unwrap(),
            project_dir: root.to_owned(),
            php: php.then(|| SitePhp {
                socket: "/nonexistent".into(),
                front_controller: fc.map(String::from),
                timeout: Duration::from_secs(1),
            }),
            optimize: false,
        }
    }

    async fn target(s: &Site, p: &str) -> Target {
        let (path, query) = p
            .split_once('?')
            .map(|(a, b)| (a, Some(b)))
            .unwrap_or((p, None));
        resolve_target(s, &path::resolve(path).unwrap(), query).await
    }

    #[tokio::test]
    async fn resolution_order() {
        let base = std::env::temp_dir().join(format!("nova-dispatch-{}", std::process::id()));
        let root = base.join("public");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(root.join("docs")).unwrap();
        std::fs::create_dir_all(root.join("app")).unwrap();
        std::fs::write(root.join("index.html"), "home").unwrap();
        std::fs::write(root.join("docs/index.php"), "<?php").unwrap();
        std::fs::write(root.join("app/index.php"), "<?php").unwrap();
        std::fs::write(root.join("router.php"), "<?php").unwrap();
        std::fs::write(base.join("secret.txt"), "secret").unwrap();
        std::os::unix::fs::symlink(base.join("secret.txt"), root.join("leak.txt")).unwrap();

        let s = site(&root, true, Some("/router.php"));
        assert!(matches!(target(&s, "/").await, Target::Static(p, _) if p.ends_with("index.html")));
        assert!(matches!(target(&s, "/docs?x=1").await, Target::Redirect(l) if l == "/docs/?x=1"));
        assert!(
            matches!(target(&s, "/docs/").await, Target::Php { script_name, .. } if script_name == "/docs/index.php")
        );
        assert!(matches!(target(&s, "/app/index.php/users/7").await,
            Target::Php { script_name, path_info, .. } if script_name == "/app/index.php" && path_info == "/users/7"));
        assert!(
            matches!(target(&s, "/pretty/url").await, Target::Php { script_name, .. } if script_name == "/router.php")
        );
        // A symlink escaping the document root is a hard 404, even with a front controller.
        assert!(matches!(target(&s, "/leak.txt").await, Target::NotFound));

        let static_only = site(&root, false, None);
        assert!(matches!(
            target(&static_only, "/missing").await,
            Target::NotFound
        ));
        assert!(matches!(
            target(&static_only, "/leak.txt").await,
            Target::NotFound
        ));
        assert!(matches!(
            target(&static_only, "/docs/").await,
            Target::NotFound
        ));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn query_params() {
        assert_eq!(query_param(Some("a=1&w=640"), "w"), Some("640"));
        assert_eq!(query_param(Some("w"), "w"), Some(""));
        assert_eq!(query_param(None, "w"), None);
    }
}
