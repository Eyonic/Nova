//! The request lifecycle:
//!
//! 1. internal endpoints (`/_nova/...`)
//! 2. site lookup by Host
//! 3. lexical path validation (traversal, hidden files)
//! 4. target resolution: file → directory index → `/script.php/path-info`
//!    → front controller → 404, with canonical containment checks
//! 5. execution: optimized image, static file, or PHP via FastCGI
//! 6. response headers, metrics and the access log line

use crate::client::{self, Client};
use crate::live::{self, LiveHub};
use crate::metrics::{Kind, Metrics};
use crate::ratelimit::RateLimiter;
use crate::sites::{Site, SitePhp, Sites, strip_port};
use bytes::Bytes;
use futures_util::StreamExt;
use http::{HeaderMap, HeaderValue, Method, Request, Response, StatusCode, Version, header};
use http_body_util::{BodyExt, BodyStream, StreamBody};
use hyper::body::{Body as _, Frame};
use nova_config::{Cidr, Mode};
use nova_http::compress::{self, Encoding};
use nova_http::path::{self, PathError, SafePath};
use nova_http::static_files::{self, FileResponse};
use nova_http::{Body, ConnInfo, ReqBody, empty, full};
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
    /// `None` disables response compression.
    pub compression: Option<compress::Options>,
    /// Serve precompressed siblings (`app.css.br`) of static files.
    pub precompressed: bool,
    pub http: HttpSettings,
    boot: u32,
    seq: AtomicU64,
    /// `[site.php] micro_cache_secs` storage, shared by all sites.
    pub micro: Arc<crate::microcache::MicroCache>,
    /// Remembered asset versions for compression dictionaries (RFC 9842).
    dictionaries: crate::dictionaries::Dictionaries,
    /// Keep-alive pool for `[site.proxy]` upstreams.
    upstream: crate::upstream::HttpClient,
    /// ACME HTTP-01 key authorizations (`acme_challenge = "http-01"`).
    pub acme_http01: std::sync::OnceLock<Arc<rustls_acme::ResolvesServerCertAcme>>,
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
        PROBE_CACHING.store(mode == Mode::Production, Ordering::Relaxed);
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
            compression: None,
            precompressed: false,
            http: HttpSettings::default(),
            boot,
            seq: AtomicU64::new(0),
            micro: Arc::default(),
            dictionaries: Default::default(),
            upstream: crate::upstream::http_client(),
            acme_http01: std::sync::OnceLock::new(),
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

/// Server-wide HTTP behaviour (proxies, limits, TLS facts for headers).
pub struct HttpSettings {
    pub trusted_proxies: Vec<Cidr>,
    pub forwarded_header: nova_config::ForwardedHeader,
    /// Clients allowed to read metrics and optimizer status.
    pub admin_allow: Vec<Cidr>,
    pub rate_limit: Option<RateLimiter>,
    /// Clients the rate limit never applies to.
    pub rate_exempt: Vec<Cidr>,
    /// Abort a request body that sends nothing for this long.
    pub body_timeout: Duration,
    pub access_log: bool,
    pub tls: Option<TlsPublic>,
}

impl Default for HttpSettings {
    fn default() -> Self {
        Self {
            trusted_proxies: Vec::new(),
            forwarded_header: nova_config::ForwardedHeader::default(),
            admin_allow: Cidr::private_ranges(),
            rate_limit: None,
            rate_exempt: Vec::new(),
            body_timeout: Duration::from_secs(60),
            access_log: true,
            tls: None,
        }
    }
}

/// What clients need to know about NOVA's HTTPS endpoint.
#[derive(Debug, Clone)]
pub struct TlsPublic {
    /// Port in `https://` redirects and `Alt-Svc`.
    pub port: u16,
    pub hsts_max_age: u64,
    pub http3: bool,
    /// Publicly trusted certificates exist (ACME or configured files), so
    /// public hosts are redirected to HTTPS and get HSTS by default.
    pub trusted_certs: bool,
}

impl TlsPublic {
    /// Only hosts with a publicly trusted certificate: HSTS or a forced
    /// redirect on a self-signed `localhost` would lock browsers out.
    fn trusted(&self, host: &str) -> bool {
        self.trusted_certs && crate::tls::acme_eligible(host)
    }
}

/// Marks error responses NOVA generated itself (eligible for site error pages).
#[derive(Debug, Clone, Copy)]
struct NovaError;

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
    async fn handle(&self, req: Request<ReqBody>, conn: ConnInfo) -> Response<Body> {
        let start = Instant::now();
        let id = self.next_request_id();
        let method = req.method().clone();
        let version = req.version();
        let uri_path = req.uri().path().to_owned();
        let client = client::resolve(
            req.headers(),
            &conn,
            &self.http.trusted_proxies,
            self.http.forwarded_header,
        );
        let (accept_encoding, host, user_agent, referer) = {
            let header = |name: header::HeaderName| {
                req.headers()
                    .get(name)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_owned)
            };
            (
                header(header::ACCEPT_ENCODING),
                request_host(&req).map(str::to_owned),
                header(header::USER_AGENT),
                header(header::REFERER),
            )
        };
        let (mut resp, kind, site) = self.route(req, client, &id).await;

        if let Some(site) = site {
            if resp.extensions().get::<NovaError>().is_some()
                && let Some(page) = site.rules.error_pages.get(&resp.status().as_u16())
            {
                resp = error_page(site, page, resp).await;
            }
            let bare = host.as_deref().map(strip_port).unwrap_or("");
            let hsts = self
                .http
                .tls
                .as_ref()
                .filter(|t| t.trusted(&bare.to_ascii_lowercase()))
                .map_or(0, |t| t.hsts_max_age);
            site.rules
                .apply_headers(resp.headers_mut(), client.https, hsts);
        }
        if let Some(tls) = &self.http.tls
            && tls.http3
            && conn.tls
            && let Ok(v) = HeaderValue::from_str(&format!("h3=\":{}\"; ma=86400", tls.port))
        {
            resp.headers_mut().insert(header::ALT_SVC, v);
        }
        if let Some(site) = site
            && (site.html_rewrite || site.speculation_rules)
            && method == Method::GET
            && resp.status() == StatusCode::OK
            && !resp.headers().contains_key(header::CONTENT_ENCODING)
            && resp
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.starts_with("text/html"))
        {
            resp = self.rewrite_html(site, resp, &uri_path);
        }
        if let Some(opts) = &self.compression {
            resp = compress::apply(resp, &method, accept_encoding.as_deref(), opts);
        }

        let h = resp.headers_mut();
        h.insert(header::SERVER, HeaderValue::from_static("nova"));
        if let Ok(v) = HeaderValue::from_str(&id) {
            h.insert("x-request-id", v);
        }
        let status = resp.status().as_u16();
        self.metrics.record(kind, status);
        if kind != Kind::Internal && self.http.access_log {
            let bytes = resp
                .headers()
                .get(header::CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok());
            let record = crate::access::Record {
                request_id: &id,
                site: site.map_or("-", |s| s.name.as_str()),
                client: client.ip,
                peer: conn.peer,
                host: host.as_deref().unwrap_or("-"),
                method: method.as_str(),
                path: &uri_path,
                protocol: if conn.http3 {
                    "HTTP/3"
                } else {
                    protocol_name(version)
                },
                tls: client.https,
                status,
                bytes,
                kind: kind.as_str(),
                encoding: resp
                    .headers()
                    .get(header::CONTENT_ENCODING)
                    .and_then(|v| v.to_str().ok()),
                referer: referer.as_deref(),
                user_agent: user_agent.as_deref(),
                duration_ms: start.elapsed().as_secs_f64() * 1e3,
            };
            if tracing::enabled!(target: "nova::access", tracing::Level::INFO)
                && !crate::access::write(&record)
            {
                tracing::info!(
                    target: "nova::access",
                    request_id = record.request_id,
                    site = record.site,
                    client = %record.client,
                    peer = %record.peer,
                    host = record.host,
                    method = record.method,
                    path = record.path,
                    protocol = record.protocol,
                    tls = record.tls,
                    status = record.status,
                    bytes = record.bytes,
                    kind = record.kind,
                    encoding = record.encoding,
                    referer = record.referer,
                    user_agent = record.user_agent,
                    duration_ms = record.duration_ms,
                );
            }
        }
        resp
    }
}

/// `Host` (HTTP/1.1) or `:authority` (HTTP/2, HTTP/3).
fn request_host<B>(req: &Request<B>) -> Option<&str> {
    req.headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .or_else(|| req.uri().authority().map(|a| a.as_str()))
}

fn protocol_name(v: Version) -> &'static str {
    match v {
        Version::HTTP_09 => "HTTP/0.9",
        Version::HTTP_10 => "HTTP/1.0",
        Version::HTTP_2 => "HTTP/2.0",
        Version::HTTP_3 => "HTTP/3.0",
        _ => "HTTP/1.1",
    }
}

/// Replace NOVA's built-in error body with the site's own page.
async fn error_page(site: &Site, page: &str, resp: Response<Body>) -> Response<Body> {
    let path = site.root.join(page.trim_start_matches('/'));
    let Some(file) = contained(site, &path).await else {
        return resp;
    };
    match tokio::fs::read(&file).await {
        Ok(bytes) if bytes.len() <= 1 << 20 => {
            let (mut parts, _) = resp.into_parts();
            if let Ok(ct) = HeaderValue::from_str(&static_files::guess_mime(&file)) {
                parts.headers.insert(header::CONTENT_TYPE, ct);
            }
            parts
                .headers
                .insert(header::CONTENT_LENGTH, bytes.len().into());
            Response::from_parts(parts, full(bytes))
        }
        _ => resp,
    }
}

impl App {
    async fn route<'a>(
        &'a self,
        req: Request<ReqBody>,
        client: Client,
        id: &str,
    ) -> (Response<Body>, Kind, Option<&'a Site>) {
        let host = request_host(&req);

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
                    let resp = match self.live.subscribe(&site.name, &channels, client.ip) {
                        Ok(r) => r,
                        Err(e) => LiveHub::error_response(e),
                    };
                    return (resp, Kind::Internal, Some(site));
                }
                _ => {}
            }
        }
        // ACME HTTP-01 validation: before redirects, auth and rate limits.
        if let Some(token) = req
            .uri()
            .path()
            .strip_prefix("/.well-known/acme-challenge/")
            && let Some(acme) = self.acme_http01.get()
        {
            let resp = match acme.get_http_01_key_auth(token) {
                Some(key_auth) => Response::builder()
                    .header(header::CONTENT_TYPE, "application/octet-stream")
                    .body(full(key_auth))
                    .unwrap(),
                None => self.not_found(),
            };
            return (resp, Kind::Internal, None);
        }
        if req.uri().path().starts_with("/_nova/") {
            return (self.internal(&req, client).await, Kind::Internal, None);
        }

        if let Some(rl) = &self.http.rate_limit
            && !Cidr::any_contains(&self.http.rate_exempt, client.ip)
            && let Err(wait) = rl.check(client.ip)
        {
            let mut resp = self.error(
                StatusCode::TOO_MANY_REQUESTS,
                "Too many requests. Please slow down.",
                None,
            );
            resp.headers_mut()
                .insert(header::RETRY_AFTER, (wait.as_secs() + 1).into());
            return (resp, Kind::Error, None);
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
        let name = Some(site);

        if let Some(resp) = self.site_redirect(site, &req, client) {
            return (resp, Kind::Static, name);
        }
        if let Some(auth) = &site.rules.auth
            && auth.applies(req.uri().path())
        {
            let given = req
                .headers()
                .get(header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok());
            if !auth.check(given).await {
                let mut resp =
                    self.error(StatusCode::UNAUTHORIZED, "Authentication required.", None);
                if let Ok(v) = HeaderValue::from_str(&format!(
                    "Basic realm=\"{}\", charset=\"UTF-8\"",
                    auth.realm
                )) {
                    resp.headers_mut().insert(header::WWW_AUTHENTICATE, v);
                }
                return (resp, Kind::Error, name);
            }
        }

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

        // Reverse proxy: matching paths go to the application server, except
        // files that exist in the document root when `static_first` is set.
        let proxied = site.proxy.as_ref().filter(|p| p.matches(req.uri().path()));
        let target = match proxied {
            Some(p) if !p.static_first => {
                return (self.proxy(p, req, client).await, Kind::Proxy, name);
            }
            _ => resolve_target(site, &safe, req.uri().query()).await,
        };
        if let Some(p) = proxied
            && !matches!(target, Target::Static(..))
        {
            return (self.proxy(p, req, client).await, Kind::Proxy, name);
        }
        match target {
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
                        .run_php(
                            site,
                            php,
                            req,
                            client,
                            id,
                            &script,
                            &script_name,
                            &path_info,
                        )
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
                let (resp, kind) = self.serve_static(site, &req, &file, &meta).await;
                (resp, kind, name)
            }
        }
    }

    async fn serve_static(
        &self,
        site: &Site,
        req: &Request<ReqBody>,
        file: &Path,
        meta: &Metadata,
    ) -> (Response<Body>, Kind) {
        let rel = file
            .strip_prefix(&site.root)
            .ok()
            .and_then(|r| r.to_str())
            .unwrap_or("");
        let cache_control =
            site.rules
                .cache
                .cache_control(rel, req.uri().query(), self.mode.is_dev());
        let cache = cache_control.as_str();
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
                && let Some(vmeta) = variant_meta(&sel.path).await
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

        // Script Optimizer output: minified and precompressed in the background.
        if site.optimize
            && !rel.is_empty()
            && !req.headers().contains_key(header::RANGE)
            && let Some(opt) = &self.optimizer
            && nova_optimize::text::TextKind::from_path(file).is_some()
        {
            let accepted = if self.compression.is_some() {
                compress::accepted(
                    req.headers()
                        .get(header::ACCEPT_ENCODING)
                        .and_then(|v| v.to_str().ok()),
                )
            } else {
                Vec::new()
            };
            let tokens: Vec<&str> = accepted.iter().map(|e| e.token()).collect();
            if let Some(sel) = opt.select_text(&site.name, rel, meta, &tokens)
                && let Some(vmeta) = variant_meta(&sel.path).await
            {
                let ctype = static_files::guess_mime(file);
                let fr = FileResponse {
                    path: &sel.path,
                    meta: &vmeta,
                    content_type: Some(&ctype),
                    cache_control: cache,
                };
                let mut resp = static_files::respond(fr, req.method(), req.headers()).await;
                let h = resp.headers_mut();
                h.remove(header::ACCEPT_RANGES);
                if let Some(enc) = sel.encoding {
                    h.insert(header::CONTENT_ENCODING, HeaderValue::from_static(enc));
                }
                compress::add_vary(h, "Accept-Encoding");
                if resp.status() == StatusCode::OK {
                    self.metrics
                        .text_saved(meta.len().saturating_sub(vmeta.len()));
                    if req.method() == Method::GET
                        && let Some(delta) = self
                            .dictionary_response(site, req, rel, meta, vmeta.len(), &mut resp)
                            .await
                    {
                        return (delta, Kind::Static);
                    }
                }
                return (resp, Kind::Static);
            }
        }

        if let Some((enc, path, pmeta)) = self.precompressed_sibling(site, req, file, meta).await {
            let ctype = static_files::guess_mime(file);
            let fr = FileResponse {
                path: &path,
                meta: &pmeta,
                content_type: Some(&ctype),
                cache_control: cache,
            };
            let mut resp = static_files::respond(fr, req.method(), req.headers()).await;
            let h = resp.headers_mut();
            // Byte ranges would address the encoded file, not the original.
            h.remove(header::ACCEPT_RANGES);
            h.insert(
                header::CONTENT_ENCODING,
                HeaderValue::from_static(enc.token()),
            );
            compress::add_vary(h, "Accept-Encoding");
            return (resp, Kind::Static);
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

    /// A precompressed sibling (`app.css.br`) produced by the site's build,
    /// in the client's preferred encoding. It must be at least as new as the
    /// original so a stale build artifact is never served.
    async fn precompressed_sibling(
        &self,
        site: &Site,
        req: &Request<ReqBody>,
        file: &Path,
        meta: &Metadata,
    ) -> Option<(Encoding, PathBuf, Metadata)> {
        if !self.precompressed
            || self.compression.is_none()
            || req.headers().contains_key(header::RANGE)
            || !compress::is_compressible(&static_files::guess_mime(file))
        {
            return None;
        }
        let accept = req
            .headers()
            .get(header::ACCEPT_ENCODING)
            .and_then(|v| v.to_str().ok());
        let modified = meta.modified().ok()?;
        for enc in compress::accepted(accept) {
            let mut name = file.as_os_str().to_owned();
            name.push(".");
            name.push(enc.extension());
            let sibling = PathBuf::from(name);
            if let Probe::File(c, m) = probe_optional(site, &sibling).await
                && m.modified().is_ok_and(|t| t >= modified)
            {
                return Some((enc, c, m));
            }
        }
        None
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_php(
        &self,
        site: &Site,
        php: &SitePhp,
        req: Request<ReqBody>,
        client: Client,
        id: &str,
        script: &Path,
        script_name: &str,
        path_info: &str,
    ) -> Response<Body> {
        let started = Instant::now();
        let (parts, body) = req.into_parts();
        let cache_key = php.micro_cache.and_then(|_| {
            crate::microcache::key(
                &site.name,
                &parts.method,
                &parts.uri,
                &parts.headers,
                client.https,
            )
        });
        // Held until this response is stored (or found uncacheable), so
        // concurrent misses for the same page wait instead of all running PHP.
        let mut _lead = None;
        // An expired entry this request refreshes; served instead of an error.
        let mut stale = None;
        if let Some(k) = &cache_key {
            use crate::microcache::Lookup;
            match self.micro.lookup(k) {
                Lookup::Fresh(e) => return self.micro_hit(e, &parts.headers).await,
                // Grace: one request refreshes, everyone else gets the
                // previous version at once instead of waiting.
                Lookup::Stale(e) => match self.micro.try_lead(k) {
                    Some(lead) => {
                        _lead = Some(lead);
                        stale = Some(e);
                    }
                    None => return self.micro_stale(e, &parts.headers).await,
                },
                Lookup::Miss => match self.micro.lead_or_wait(k, php.timeout).await {
                    Some(lead) => _lead = Some(lead),
                    None => {
                        if let Some(e) = self.micro.get(k) {
                            return self.micro_hit(e, &parts.headers).await;
                        }
                    }
                },
            }
        }
        let req_headers = parts.headers.clone();
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
        // Either way a client that stalls mid-upload is cut off.
        type BodyStreamBox =
            std::pin::Pin<Box<dyn futures_util::Stream<Item = io::Result<Bytes>> + Send>>;
        let end_stream = body.is_end_stream();
        let frames = BodyStream::new(body).filter_map(|frame| async move {
            match frame {
                Ok(f) => f.into_data().ok().map(Ok),
                Err(e) => Some(Err(e)),
            }
        });
        let mut timed: BodyStreamBox = Box::pin(idle_timeout(frames, self.http.body_timeout));
        let (stream, content_length): (BodyStreamBox, Option<u64>) =
            if declared.is_some() || end_stream {
                (timed, declared)
            } else {
                let mut buf = bytes::BytesMut::new();
                while let Some(chunk) = timed.next().await {
                    match chunk {
                        Ok(c) if (buf.len() + c.len()) as u64 > self.max_body => {
                            return self.error(
                                StatusCode::PAYLOAD_TOO_LARGE,
                                "Request body too large.",
                                None,
                            );
                        }
                        Ok(c) => buf.extend_from_slice(&c),
                        Err(e) if e.kind() == io::ErrorKind::TimedOut => {
                            return self.error(
                                StatusCode::REQUEST_TIMEOUT,
                                "The request body was not received in time.",
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
                }
                let bytes = buf.freeze();
                let len = bytes.len() as u64;
                (Box::pin(futures_util::stream::iter([Ok(bytes)])), Some(len))
            };

        let host = parts
            .headers
            .get(header::HOST)
            .and_then(|v| v.to_str().ok())
            .or_else(|| parts.uri.authority().map(|a| a.as_str()))
            .unwrap_or("");
        // The port the client used: explicit in Host, else the scheme's default.
        let server_port = host
            .rsplit_once(':')
            .filter(|(h, _)| !h.ends_with(']') || host.starts_with('['))
            .and_then(|(_, p)| p.parse::<u16>().ok())
            .unwrap_or(if client.https { 443 } else { 80 });
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
            server_port,
            server_protocol: protocol,
            remote_addr: SocketAddr::new(client.ip, 0),
            https: client.https,
            headers: &parts.headers,
            content_length,
            request_id: id,
        });

        let result = nova_runtime_php::execute_pooled(
            PhpRequest {
                socket: php.socket.clone(),
                params,
                body: stream,
                header_timeout: php.timeout + Duration::from_secs(5),
                site: site.name.clone(),
            },
            &php.pool,
        )
        .await;
        let micros = started.elapsed().as_micros() as u64;

        let r = match result {
            // stale-if-error: a failing PHP keeps the last good page online.
            Ok(r) if r.status.is_server_error() && stale.is_some() => {
                self.metrics.php_done(micros, false);
                return self.micro_stale(stale.take().unwrap(), &req_headers).await;
            }
            Err(_) if stale.is_some() => {
                self.metrics.php_done(micros, false);
                return self.micro_stale(stale.take().unwrap(), &req_headers).await;
            }
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

        let mut headers = HeaderMap::new();
        let mut published = false;
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
                published = true;
                if r.status.as_u16() < 400 {
                    // The site's data changed: cached pages are stale.
                    self.micro.purge_site(&site.name);
                    for channel in live::parse_channels(value.to_str().unwrap_or("")) {
                        self.live.publish(&site.name, &channel);
                    }
                }
                continue;
            }
            headers.append(name.clone(), value.clone());
        }
        let store = cache_key
            .zip(php.micro_cache)
            .filter(|_| !published && crate::microcache::cacheable(r.status, &headers));
        // PHP may answer NOVA Live requests with a fragment: caches must
        // keep fragments and full pages apart.
        headers.append(
            header::VARY,
            HeaderValue::from_static("Nova-Live, Nova-Target"),
        );

        let mut rx = r.body;
        // Cacheable: buffer up to MAX_ENTRY; anything larger (or failing)
        // is streamed on from where buffering stopped, and not stored.
        let mut prefix: Vec<io::Result<Bytes>> = Vec::new();
        if let Some((key, ttl)) = store {
            let mut size = 0;
            loop {
                match rx.recv().await {
                    Some(Ok(b)) => {
                        size += b.len();
                        prefix.push(Ok(b));
                        if size > crate::microcache::MAX_ENTRY {
                            break;
                        }
                    }
                    Some(Err(e)) => {
                        prefix.push(Err(e));
                        break;
                    }
                    None => {
                        let mut body = bytes::BytesMut::with_capacity(size);
                        for b in prefix.iter().flatten() {
                            body.extend_from_slice(b);
                        }
                        let body = body.freeze();
                        self.micro.put(
                            key,
                            r.status,
                            headers.clone(),
                            body.clone(),
                            ttl,
                            php.micro_cache_grace,
                        );
                        let mut resp = Response::new(full(body));
                        *resp.status_mut() = r.status;
                        *resp.headers_mut() = headers;
                        crate::microcache::mark(resp.headers_mut(), None);
                        return resp;
                    }
                }
            }
        }
        // Without buffering this is the whole body; after a partial buffer
        // it continues where buffering stopped.
        let rest = futures_util::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|item| (item, rx))
        });
        let body = futures_util::stream::iter(prefix)
            .chain(rest)
            .map(|item| item.map(Frame::data));
        let mut resp = Response::new(StreamBody::new(body).boxed_unsync());
        *resp.status_mut() = r.status;
        *resp.headers_mut() = headers;
        resp
    }

    /// `html_rewrite`: stream the page through the `<img>` rewriter with the
    /// site's image metadata from the Optimizer.
    fn rewrite_html(&self, site: &Site, resp: Response<Body>, path: &str) -> Response<Body> {
        let manifest = self.optimizer.as_ref().and_then(|o| o.manifest(&site.name));
        let lookup = move |p: &str| {
            let asset = manifest.as_ref()?.assets.get(p.trim_start_matches('/'))?;
            let mut widths: Vec<u32> = asset
                .variants
                .iter()
                .map(|v| v.width)
                .filter(|w| *w < asset.source.width)
                .collect();
            widths.sort_unstable();
            widths.dedup();
            Some(crate::htmlrewrite::ImageInfo {
                width: asset.source.width,
                height: asset.source.height,
                widths,
            })
        };
        let (mut parts, body) = resp.into_parts();
        // The body changes: its length and strong validator no longer apply.
        parts.headers.remove(header::CONTENT_LENGTH);
        parts.headers.remove(header::ETAG);
        if site.speculation_rules && !parts.headers.contains_key("no-vary-search") {
            // Prefetched pages stay usable when the link carries tracking parameters.
            parts.headers.insert(
                "no-vary-search",
                HeaderValue::from_static(
                    r#"params=("utm_source" "utm_medium" "utm_campaign" "utm_term" "utm_content" "gclid" "fbclid")"#,
                ),
            );
        }
        let opts = crate::htmlrewrite::Options {
            images: site.html_rewrite,
            speculation: site.speculation_rules,
        };
        let body = crate::htmlrewrite::rewrite(body, path.to_string(), opts, lookup);
        Response::from_parts(parts, body)
    }

    /// Compression dictionaries for an immutable, fingerprinted JS/CSS
    /// response: mark it as a dictionary for later versions, and when the
    /// browser offers an older version NOVA knows, answer with the `dcz`
    /// delta instead (when it beats `normal_len`, the regular encoding).
    async fn dictionary_response(
        &self,
        site: &Site,
        req: &Request<ReqBody>,
        rel: &str,
        meta: &Metadata,
        normal_len: u64,
        resp: &mut Response<Body>,
    ) -> Option<Response<Body>> {
        let path = req.uri().path();
        let immutable = resp
            .headers()
            .get(header::CACHE_CONTROL)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.contains("immutable"));
        let pattern = crate::dictionaries::pattern_for(path)?;
        let offered = req.headers().contains_key("available-dictionary");
        // Offer a dictionary only on responses browsers keep (immutable);
        // use one whenever the browser announces a version NOVA knows.
        if !immutable && !offered {
            return None;
        }
        let opt = self.optimizer.as_ref()?;
        // The decoded bytes the browser keeps: the minified version, or the
        // original when the Optimizer left it as is (already minified).
        let identity = opt
            .select_text(&site.name, rel, meta, &[])
            .map(|sel| sel.path)
            .unwrap_or_else(|| site.root.join(rel));
        let content = Bytes::from(tokio::fs::read(&identity).await.ok()?);
        if content.len() > crate::dictionaries::MAX_DICTIONARY {
            return None;
        }
        let hash = crate::dictionaries::sha256(&content);
        if immutable {
            self.dictionaries.remember(hash, &pattern, content.clone());
            if let Ok(v) = HeaderValue::from_str(&format!("match=\"{pattern}\"")) {
                resp.headers_mut().insert("use-as-dictionary", v);
            }
        }
        let h = req.headers();
        let accepts_dcz = h
            .get_all(header::ACCEPT_ENCODING)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(','))
            .any(|t| t.split(';').next().is_some_and(|t| t.trim() == "dcz"));
        let have = h
            .get("available-dictionary")
            .and_then(|v| v.to_str().ok())
            .and_then(crate::dictionaries::parse_available)?;
        if !accepts_dcz {
            return None;
        }
        let delta = self.dictionaries.delta(&have, path, hash, &content)?;
        if delta.len() as u64 >= normal_len {
            return None;
        }
        let mut out = Response::new(full(delta));
        for name in [
            header::CONTENT_TYPE,
            header::CACHE_CONTROL,
            header::LAST_MODIFIED,
        ] {
            if let Some(v) = resp.headers().get(&name) {
                out.headers_mut().insert(name, v.clone());
            }
        }
        let oh = out.headers_mut();
        oh.insert(header::CONTENT_ENCODING, HeaderValue::from_static("dcz"));
        oh.insert(
            header::VARY,
            HeaderValue::from_static("Accept-Encoding, Available-Dictionary"),
        );
        if let Some(v) = resp.headers().get("use-as-dictionary") {
            oh.insert("use-as-dictionary", v.clone());
        }
        Some(out)
    }

    /// An expired entry served within its grace period.
    async fn micro_stale(
        &self,
        e: crate::microcache::Entry,
        req_headers: &http::HeaderMap,
    ) -> Response<Body> {
        let mut resp = self.micro_hit(e, req_headers).await;
        resp.headers_mut()
            .insert("nova-cache", HeaderValue::from_static("stale"));
        resp
    }

    /// A micro-cache hit, compressed for this client from the entry's
    /// stored rendition when there is one (made once per encoding).
    async fn micro_hit(
        &self,
        e: crate::microcache::Entry,
        req_headers: &http::HeaderMap,
    ) -> Response<Body> {
        let plain = |e: &crate::microcache::Entry| {
            let mut resp = Response::new(full(e.body.clone()));
            *resp.status_mut() = e.status;
            *resp.headers_mut() = e.headers.clone();
            resp
        };
        let accept = req_headers
            .get(header::ACCEPT_ENCODING)
            .and_then(|v| v.to_str().ok());
        let encoding = self
            .compression
            .as_ref()
            .and_then(|_| compress::accepted(accept).first().copied());
        let mut resp = match (encoding, &self.compression) {
            (Some(enc), Some(opts)) => match e.encoded(enc.token()) {
                Some((headers, body)) => {
                    let mut r = Response::new(full(body));
                    *r.status_mut() = e.status;
                    *r.headers_mut() = headers;
                    r
                }
                None => {
                    let r = compress::apply(plain(&e), &Method::GET, accept, opts);
                    if r.headers().contains_key(header::CONTENT_ENCODING) {
                        let (parts, body) = r.into_parts();
                        match body.collect().await {
                            Ok(c) => {
                                let body = c.to_bytes();
                                e.store_encoded(enc.token(), parts.headers.clone(), body.clone());
                                Response::from_parts(parts, full(body))
                            }
                            Err(_) => plain(&e),
                        }
                    } else {
                        r
                    }
                }
            },
            _ => plain(&e),
        };
        crate::microcache::mark(resp.headers_mut(), Some(e.age()));
        resp
    }

    async fn proxy(
        &self,
        p: &nova_config::SiteProxy,
        req: Request<ReqBody>,
        client: Client,
    ) -> Response<Body> {
        let base = p.upstream.trim_end_matches('/');
        let timeout = Duration::from_secs(p.timeout_secs);
        match crate::upstream::forward(&self.upstream, base, req, client, timeout).await {
            Ok(resp) => resp,
            Err(crate::upstream::ProxyError::Timeout) => self.error(
                StatusCode::GATEWAY_TIMEOUT,
                "The application did not respond in time.",
                None,
            ),
            Err(crate::upstream::ProxyError::Unavailable(e)) => {
                tracing::warn!(upstream = base, error = %e, "upstream request failed");
                self.error(
                    StatusCode::BAD_GATEWAY,
                    "The application is not available.",
                    Some(e),
                )
            }
        }
    }

    async fn internal(&self, req: &Request<ReqBody>, client: Client) -> Response<Body> {
        let admin = Cidr::any_contains(&self.http.admin_allow, client.ip);
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
            "/_nova/metrics" if self.metrics_enabled && admin => {
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
            "/_nova/optimize/status" if self.metrics_enabled && admin => {
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
                        let mut v = v;
                        if let Some(t) = opt.text_manifest(&site.name) {
                            v["scripts"] = serde_json::json!({
                                "files": t.files.len(),
                                "minified": t.files.values().filter(|f| f.minified.is_some()).count(),
                                "errors": t.errors.len(),
                                "report": t.report,
                            });
                        }
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
            .extension(NovaError)
            .body(full(body))
            .unwrap()
    }

    /// HTTPS upgrade, canonical host and configured redirects, in that order.
    fn site_redirect(
        &self,
        site: &Site,
        req: &Request<ReqBody>,
        client: Client,
    ) -> Option<Response<Body>> {
        let uri = req.uri();
        let path_q = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
        let host = request_host(req).unwrap_or("");
        let bare = strip_port(host).trim_end_matches('.').to_ascii_lowercase();
        let port = host
            .strip_prefix(strip_port(host))
            .and_then(|p| p.strip_prefix(':'));
        let rules = &site.rules;
        let redirect = |status: StatusCode, location: String| {
            let mut r = Response::builder()
                .status(status)
                .header(header::CACHE_CONTROL, "no-cache");
            if let Ok(v) = HeaderValue::from_str(&location) {
                r = r.header(header::LOCATION, v);
            }
            r.body(empty()).unwrap()
        };
        // Keep 301/302 semantics for GET/HEAD; 308 preserves other methods.
        let permanent = if matches!(*req.method(), Method::GET | Method::HEAD) {
            StatusCode::MOVED_PERMANENTLY
        } else {
            StatusCode::PERMANENT_REDIRECT
        };

        let target_host = rules.canonical_host.clone().unwrap_or_else(|| bare.clone());
        if let Some(tls) = &self.http.tls
            && !client.https
            && rules.https_redirect.unwrap_or_else(|| tls.trusted(&bare))
            && !uri.path().starts_with("/.well-known/acme-challenge/")
        {
            let port = if tls.port == 443 {
                String::new()
            } else {
                format!(":{}", tls.port)
            };
            return Some(redirect(
                permanent,
                format!("https://{target_host}{port}{path_q}"),
            ));
        }
        if let Some(canonical) = &rules.canonical_host
            && *canonical != bare
            && !bare.is_empty()
        {
            let scheme = if client.https { "https" } else { "http" };
            let port = port.map(|p| format!(":{p}")).unwrap_or_default();
            return Some(redirect(
                permanent,
                format!("{scheme}://{canonical}{port}{path_q}"),
            ));
        }
        rules
            .redirect(uri.path(), uri.query())
            .map(|(status, location)| redirect(status, location))
    }
}

/// Fail a body stream that delivers nothing for `timeout`.
fn idle_timeout<S>(
    stream: S,
    timeout: Duration,
) -> impl futures_util::Stream<Item = io::Result<Bytes>>
where
    S: futures_util::Stream<Item = io::Result<Bytes>> + Send + 'static,
{
    futures_util::stream::unfold(Some(Box::pin(stream)), move |state| async move {
        let mut s = state?;
        match tokio::time::timeout(timeout, s.next()).await {
            Ok(Some(item)) => Some((item, Some(s))),
            Ok(None) => None,
            Err(_) => Some((
                Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "request body timed out",
                )),
                None,
            )),
        }
    })
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

/// What `stat` + (for files) `canonicalize` found, in one blocking-pool hop.
#[derive(Clone)]
enum Probe {
    /// A regular file inside the site root: its canonical path.
    File(PathBuf, Metadata),
    /// A regular file whose real path leaves the site root (symlink escape).
    Escapes,
    Dir,
    Missing,
}

/// Recent `probe` results for files and directories (like nginx's
/// `open_file_cache`): a hit skips the blocking-pool hop and the
/// stat/readlink syscalls, which are slow on FUSE filesystems such as
/// Unraid's /mnt/user. Entries live [`PROBE_TTL`]; missing paths are never
/// cached, so new files appear immediately.
static PROBE_CACHE: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<PathBuf, (Probe, Instant)>>,
> = std::sync::LazyLock::new(Default::default);
const PROBE_TTL: Duration = Duration::from_secs(1);
/// Production only: in development every edit must show on the next request.
static PROBE_CACHING: AtomicBool = AtomicBool::new(false);
const PROBE_CACHE_MAX: usize = 16_384;

async fn probe(site: &Site, p: &Path) -> Probe {
    probe_with(site, p, false).await
}

/// `probe`, also caching "missing": for optional files looked up on every
/// request (precompressed `.br`/`.zst`/`.gz` siblings), where a new file
/// may take up to [`PROBE_TTL`] to be noticed.
async fn probe_optional(site: &Site, p: &Path) -> Probe {
    probe_with(site, p, true).await
}

/// Metadata of an Optimizer output file (written atomically, never
/// changed in place), through the same short-lived cache.
async fn variant_meta(p: &Path) -> Option<Metadata> {
    let caching = PROBE_CACHING.load(Ordering::Relaxed);
    if caching
        && let Some((Probe::File(_, m), at)) = PROBE_CACHE.lock().unwrap().get(p)
        && at.elapsed() < PROBE_TTL
    {
        return Some(m.clone());
    }
    let m = tokio::fs::metadata(p).await.ok().filter(|m| m.is_file())?;
    if caching {
        remember(p, Probe::File(p.to_path_buf(), m.clone()));
    }
    Some(m)
}

fn remember(p: &Path, probe: Probe) {
    let mut cache = PROBE_CACHE.lock().unwrap();
    if cache.len() >= PROBE_CACHE_MAX {
        cache.clear();
    }
    cache.insert(p.to_path_buf(), (probe, Instant::now()));
}

async fn probe_with(site: &Site, p: &Path, cache_missing: bool) -> Probe {
    let caching = PROBE_CACHING.load(Ordering::Relaxed);
    if caching
        && let Some((hit, at)) = PROBE_CACHE.lock().unwrap().get(p)
        && at.elapsed() < PROBE_TTL
    {
        return hit.clone();
    }
    let (root, path) = (site.root.clone(), p.to_path_buf());
    let found = tokio::task::spawn_blocking(move || match std::fs::metadata(&path) {
        Ok(m) if m.is_file() => match std::fs::canonicalize(&path) {
            Ok(c) if c.starts_with(&root) => Probe::File(c, m),
            _ => Probe::Escapes,
        },
        Ok(m) if m.is_dir() => Probe::Dir,
        _ => Probe::Missing,
    })
    .await
    .unwrap_or(Probe::Missing);
    if caching && (cache_missing || matches!(found, Probe::File(..) | Probe::Dir)) {
        remember(p, found.clone());
    }
    found
}

async fn resolve_target(site: &Site, safe: &SafePath, query: Option<&str>) -> Target {
    let full = site.root.join(safe.to_path());
    match probe(site, &full).await {
        Probe::Escapes => return Target::NotFound,
        Probe::File(c, m) => {
            if is_php(&c) {
                return Target::Php {
                    script: c,
                    script_name: safe.url(),
                    path_info: String::new(),
                };
            }
            return Target::Static(c, m);
        }
        Probe::Dir => {
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
                if let Probe::File(c, meta) = probe(site, &f).await {
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
        Probe::Missing => {
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
        let cfg = nova_config::Config::from_toml(
            "version = 1\n[[site]]\nname = \"t\"\ndefault = true\npath = \"/srv\"\n",
        )
        .unwrap();
        Site {
            rules: crate::rules::SiteRules::new(&cfg.sites[0], root).unwrap(),
            name: "t".into(),
            root: std::fs::canonicalize(root).unwrap(),
            project_dir: root.to_owned(),
            php: php.then(|| SitePhp {
                socket: "/nonexistent".into(),
                front_controller: fc.map(String::from),
                timeout: Duration::from_secs(1),
                micro_cache: None,
                micro_cache_grace: Duration::ZERO,
                pool: nova_runtime_php::Pool::new("/nonexistent", 2),
            }),
            proxy: None,
            optimize: false,
            html_rewrite: false,
            speculation_rules: false,
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
