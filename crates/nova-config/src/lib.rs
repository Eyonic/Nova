//! NOVA configuration: the versioned, declarative `nova.toml` schema.
//!
//! Loading is two-step: [`Config::from_toml`] parses the file, then
//! [`Config::validate`] checks cross-field invariants (unique site names,
//! host collisions, absolute paths, ...). Secrets are never stored in the
//! file itself; fields ending in `_env` name an environment variable that is
//! resolved at startup.

pub mod http;

pub use http::{
    CacheRule, CertFiles, Cidr, RateLimitConfig, Redirect, SiteAuth, SiteCache, TlsConfig,
    glob_match,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

/// The only schema version this build understands.
pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("invalid configuration syntax: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("invalid configuration:\n{}", .0.iter().map(|e| format!("  - {e}")).collect::<Vec<_>>().join("\n"))]
    Invalid(Vec<String>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Development,
    #[default]
    Production,
}

impl Mode {
    pub fn is_dev(self) -> bool {
        self == Mode::Development
    }
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Mode::Development => "development",
            Mode::Production => "production",
        })
    }
}

/// A byte size written as an integer or as a string such as `"64MiB"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(into = "u64")]
pub struct ByteSize(pub u64);

impl From<ByteSize> for u64 {
    fn from(b: ByteSize) -> u64 {
        b.0
    }
}

impl ByteSize {
    pub fn parse(s: &str) -> Result<Self, String> {
        let s = s.trim();
        let split = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
        let (num, unit) = s.split_at(split);
        let n: u64 = num
            .parse()
            .map_err(|_| format!("invalid byte size {s:?}"))?;
        let mult: u64 = match unit.trim().to_ascii_lowercase().as_str() {
            "" | "b" => 1,
            "k" | "kb" | "kib" => 1 << 10,
            "m" | "mb" | "mib" => 1 << 20,
            "g" | "gb" | "gib" => 1 << 30,
            other => return Err(format!("unknown byte size unit {other:?} in {s:?}")),
        };
        n.checked_mul(mult)
            .map(ByteSize)
            .ok_or_else(|| format!("byte size {s:?} overflows"))
    }

    /// Format for php.ini style directives (`64M`).
    pub fn to_php(self) -> String {
        if self.0.is_multiple_of(1 << 20) {
            format!("{}M", self.0 >> 20)
        } else if self.0.is_multiple_of(1 << 10) {
            format!("{}K", self.0 >> 10)
        } else {
            self.0.to_string()
        }
    }
}

impl<'de> Deserialize<'de> for ByteSize {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Int(u64),
            Str(String),
        }
        match Raw::deserialize(d)? {
            Raw::Int(n) => Ok(ByteSize(n)),
            Raw::Str(s) => ByteSize::parse(&s).map_err(serde::de::Error::custom),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub version: u32,
    #[serde(default)]
    pub mode: Mode,
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub paths: PathsConfig,
    #[serde(default)]
    pub php: PhpConfig,
    #[serde(default)]
    pub optimize: OptimizeConfig,
    #[serde(default)]
    pub isolation: IsolationConfig,
    #[serde(default)]
    pub live: LiveConfig,
    /// Named backing services (currently databases) that sites refer to.
    #[serde(default)]
    pub services: ServicesConfig,
    #[serde(default, rename = "site")]
    pub sites: Vec<SiteConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct ServerConfig {
    pub listen: SocketAddr,
    /// Seconds to wait for in-flight requests after SIGTERM.
    pub shutdown_grace_secs: u64,
    pub max_connections: usize,
    pub max_request_body: ByteSize,
    pub header_read_timeout_secs: u64,
    /// Expose `/_nova/metrics`. Health endpoints are always available.
    pub metrics: bool,
    pub compression: CompressionConfig,
    /// Clients allowed to read `/_nova/metrics` and `/_nova/optimize/status`.
    pub admin_allow: Vec<Cidr>,
    /// Reverse proxies whose `X-Forwarded-*` / `Forwarded` headers are trusted.
    pub trusted_proxies: Vec<Cidr>,
    /// Expect a PROXY protocol (v1/v2) header on every connection.
    pub proxy_protocol: bool,
    /// Abort uploads that send nothing for this long.
    pub request_body_timeout_secs: u64,
    /// One structured access-log line per request.
    pub access_log: bool,
    pub rate_limit: RateLimitConfig,
    pub tls: TlsConfig,
}

/// `[server.compression]`: brotli / zstd / gzip for text responses.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct CompressionConfig {
    pub enabled: bool,
    /// Responses with a known length below this are sent uncompressed.
    pub min_size: ByteSize,
    /// Serve `file.br` / `file.zst` / `file.gz` built next to a static file.
    pub precompressed: bool,
}

impl Default for CompressionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            min_size: ByteSize(1024),
            precompressed: true,
        }
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen: "0.0.0.0:8080".parse().unwrap(),
            shutdown_grace_secs: 20,
            max_connections: 4096,
            max_request_body: ByteSize(64 << 20),
            header_read_timeout_secs: 15,
            metrics: true,
            compression: CompressionConfig::default(),
            admin_allow: http::default_admin_allow(),
            trusted_proxies: Vec::new(),
            proxy_protocol: false,
            request_body_timeout_secs: http::DEFAULT_BODY_TIMEOUT_SECS,
            access_log: true,
            rate_limit: RateLimitConfig::default(),
            tls: TlsConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct PathsConfig {
    /// Persistent state (generated assets, per-site tmp and sessions). Must be a volume.
    pub state_dir: PathBuf,
    /// Ephemeral runtime files (sockets, generated PHP-FPM config). tmpfs is fine.
    pub run_dir: PathBuf,
}

impl Default for PathsConfig {
    fn default() -> Self {
        Self {
            state_dir: "/var/lib/nova".into(),
            run_dir: "/run/nova".into(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct PhpConfig {
    /// The php-fpm binary NOVA supervises.
    pub fpm_binary: PathBuf,
    /// Functions disabled in every pool unless a site overrides the list.
    pub disable_functions: Vec<String>,
}

impl Default for PhpConfig {
    fn default() -> Self {
        Self {
            fpm_binary: "php-fpm".into(),
            disable_functions: [
                "exec",
                "passthru",
                "shell_exec",
                "system",
                "proc_open",
                "popen",
                "pcntl_exec",
                "proc_nice",
                "posix_kill",
                "posix_setuid",
                "dl",
            ]
            .map(String::from)
            .to_vec(),
        }
    }
}

/// NOVA Live: the `/_nova/live.js` runtime and the invalidation event stream.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct LiveConfig {
    pub enabled: bool,
    /// Open event-stream connections across all sites.
    pub max_connections: usize,
    /// Open event-stream connections from one client IP.
    pub max_connections_per_ip: usize,
    /// Channels one connection may subscribe to.
    pub max_channels: usize,
    pub heartbeat_secs: u64,
}

impl Default for LiveConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_connections: 1024,
            max_connections_per_ip: 16,
            max_channels: 16,
            heartbeat_secs: 20,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum IsolationMode {
    /// `strict` when started as root, otherwise `shared`.
    #[default]
    Auto,
    /// Separate uid per site plus Landlock. Requires starting as root.
    Strict,
    /// One uid for everything; Landlock still separates sites.
    Shared,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct IsolationConfig {
    pub mode: IsolationMode,
    /// Derived site uids are `base_uid + hash(name) % 30000`.
    pub base_uid: u32,
    /// Identity of the network-facing worker (HTTP + optimizer).
    pub worker_uid: u32,
    /// Refuse to start PHP when the kernel cannot enforce Landlock at all.
    pub require_landlock: bool,
}

impl Default for IsolationConfig {
    fn default() -> Self {
        Self {
            mode: IsolationMode::Auto,
            base_uid: 20000,
            worker_uid: 10001,
            require_landlock: true,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct SiteIsolation {
    /// Explicit uid; derived from the site name when unset.
    pub uid: Option<u32>,
    /// Extra outbound TCP ports for this site's PHP (database ports are
    /// added automatically). Example: `[443]` for HTTPS APIs.
    pub allow_connect: Vec<u16>,
    /// Extra read-only paths (e.g. a shared asset directory).
    pub allow_read: Vec<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum ImageFormat {
    Avif,
    Webp,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct OptimizeConfig {
    pub enabled: bool,
    /// Candidate responsive widths; only widths smaller than the source are generated.
    pub widths: Vec<u32>,
    /// Modern formats to generate in addition to the source format.
    pub formats: Vec<ImageFormat>,
    pub quality: QualityProfile,
    /// AVIF encoder speed 1 (slow, small) ..= 10 (fast).
    pub avif_speed: u8,
    /// Concurrent transform jobs.
    pub workers: usize,
    /// Refuse to decode images larger than this many pixels.
    pub max_pixels: u64,
    /// Seconds between change scans. 0 disables rescans (scan once at startup).
    pub scan_interval_secs: u64,
    /// Warn about sources larger than this after optimization.
    pub budget_bytes: Option<ByteSize>,
}

impl Default for OptimizeConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            widths: vec![320, 640, 960, 1280, 1920],
            formats: vec![ImageFormat::Avif, ImageFormat::Webp],
            quality: QualityProfile::default(),
            avif_speed: 7,
            workers: 2,
            max_pixels: 50_000_000,
            scan_interval_secs: 30,
            budget_bytes: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct QualityProfile {
    pub avif: f32,
    pub webp: f32,
    pub jpeg: u8,
}

impl Default for QualityProfile {
    fn default() -> Self {
        Self {
            avif: 60.0,
            webp: 78.0,
            jpeg: 82,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ServicesConfig {
    #[serde(default)]
    pub database: BTreeMap<String, DatabaseService>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DatabaseDriver {
    Mariadb,
    Mysql,
    Postgres,
}

impl DatabaseDriver {
    /// Value exposed to PHP as `DB_CONNECTION` (Laravel naming).
    pub fn connection_name(self) -> &'static str {
        match self {
            DatabaseDriver::Mariadb => "mariadb",
            DatabaseDriver::Mysql => "mysql",
            DatabaseDriver::Postgres => "pgsql",
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DatabaseService {
    pub driver: DatabaseDriver,
    pub host: String,
    pub port: u16,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SiteConfig {
    /// Identifier: lowercase letters, digits and dashes.
    pub name: String,
    /// Host names (without port) routed to this site.
    #[serde(default)]
    pub hosts: Vec<String>,
    /// Serve requests whose Host matches no site.
    #[serde(default)]
    pub default: bool,
    /// Project directory. PHP may read anything below it, and nothing else.
    pub path: PathBuf,
    /// Document root, relative to `path`.
    #[serde(default = "default_public")]
    pub public: PathBuf,
    #[serde(default)]
    pub php: Option<SitePhpConfig>,
    #[serde(default)]
    pub database: Option<SiteDatabase>,
    /// Plain environment variables passed to this site's PHP workers only.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Like `env`, but values are read from NOVA's environment (`NAME = "NOVA_VAR"`).
    #[serde(default)]
    pub env_from: BTreeMap<String, String>,
    #[serde(default = "default_true")]
    pub optimize: bool,
    #[serde(default)]
    pub isolation: SiteIsolation,
    /// Framework integration; `auto` detects it from the project files.
    #[serde(default)]
    pub framework: FrameworkSetting,
    /// `Cache-Control` for static files.
    #[serde(default)]
    pub cache: SiteCache,
    /// Response headers added when the application has not set them.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// Baseline security headers (nosniff, Referrer-Policy, X-Frame-Options,
    /// HSTS over HTTPS) unless the response sets them.
    #[serde(default = "default_true")]
    pub security_headers: bool,
    /// Redirect every other host of this site here (308).
    #[serde(default)]
    pub canonical_host: Option<String>,
    /// Redirect plain HTTP to HTTPS (default: on for public hosts with an
    /// ACME or configured certificate; off for self-signed local hosts).
    #[serde(default)]
    pub https_redirect: Option<bool>,
    #[serde(default, rename = "redirect")]
    pub redirects: Vec<Redirect>,
    /// Status code → page (URL path in the document root) used for NOVA's
    /// own errors and for empty error responses from PHP.
    #[serde(default)]
    pub error_pages: BTreeMap<String, String>,
    /// HTTP basic authentication.
    #[serde(default)]
    pub auth: Option<SiteAuth>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum FrameworkSetting {
    #[default]
    Auto,
    None,
    Laravel,
}

fn default_public() -> PathBuf {
    "public".into()
}

pub(crate) fn default_true() -> bool {
    true
}

impl SiteConfig {
    pub fn document_root(&self) -> PathBuf {
        self.path.join(&self.public)
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct SitePhpConfig {
    pub enabled: bool,
    /// Script (relative to the document root) that receives requests for
    /// paths that match no file, e.g. `index.php` for Laravel or WordPress.
    pub front_controller: Option<String>,
    pub max_children: u32,
    pub max_requests: u32,
    pub memory_limit: ByteSize,
    pub timeout_secs: u64,
    /// Overrides `[php].disable_functions` for this site.
    pub disable_functions: Option<Vec<String>>,
    /// Extra `php_admin_value` settings (cannot be changed by the application).
    pub ini: BTreeMap<String, String>,
}

impl Default for SitePhpConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            front_controller: None,
            max_children: 8,
            max_requests: 1000,
            memory_limit: ByteSize(256 << 20),
            timeout_secs: 30,
            disable_functions: None,
            ini: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SiteDatabase {
    /// Name of a `[services.database.<name>]` entry.
    pub service: String,
    pub name: String,
    pub user: String,
    /// Environment variable holding the password.
    pub password_env: String,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_owned(),
            source,
        })?;
        let cfg = Self::from_toml(&text)?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn from_toml(text: &str) -> Result<Self, ConfigError> {
        Ok(toml::from_str(text)?)
    }

    /// Check invariants serde cannot express. Collects every problem instead
    /// of stopping at the first one.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let mut errs = Vec::new();
        if self.version != SCHEMA_VERSION {
            errs.push(format!(
                "unsupported config version {} (this build supports version {SCHEMA_VERSION})",
                self.version
            ));
        }
        if self.sites.is_empty() {
            errs.push("at least one [[site]] is required".into());
        }
        for (label, p) in [
            ("paths.state_dir", &self.paths.state_dir),
            ("paths.run_dir", &self.paths.run_dir),
        ] {
            if !p.is_absolute() {
                errs.push(format!("{label} must be an absolute path"));
            }
        }
        if self.server.max_connections == 0 {
            errs.push("server.max_connections must be > 0".into());
        }
        let o = &self.optimize;
        if o.workers == 0 {
            errs.push("optimize.workers must be > 0".into());
        }
        if !(1..=10).contains(&o.avif_speed) {
            errs.push("optimize.avif_speed must be within 1..=10".into());
        }
        if o.widths.iter().any(|&w| w == 0 || w > 16384) {
            errs.push("optimize.widths must be within 1..=16384".into());
        }
        if !(0.0..=100.0).contains(&o.quality.avif)
            || !(0.0..=100.0).contains(&o.quality.webp)
            || !(1..=100).contains(&o.quality.jpeg)
        {
            errs.push("optimize.quality values must be within 1..=100".into());
        }

        let mut names = HashSet::new();
        let mut hosts = HashSet::new();
        let mut defaults = 0;
        for site in &self.sites {
            let n = &site.name;
            if !valid_ident(n) {
                errs.push(format!(
                    "site name {n:?} must match [a-z0-9][a-z0-9-]* (max 32 chars)"
                ));
            }
            if !names.insert(n.as_str()) {
                errs.push(format!("duplicate site name {n:?}"));
            }
            if site.default {
                defaults += 1;
            }
            if site.hosts.is_empty() && !site.default {
                errs.push(format!(
                    "site {n:?} needs at least one host or `default = true`"
                ));
            }
            for h in &site.hosts {
                let h = h.to_ascii_lowercase();
                if h.is_empty() || h.contains([':', '/', ' ']) {
                    errs.push(format!(
                        "site {n:?}: invalid host {h:?} (no port or scheme)"
                    ));
                }
                if !hosts.insert(h.clone()) {
                    errs.push(format!("host {h:?} is assigned to more than one site"));
                }
            }
            if !site.path.is_absolute() {
                errs.push(format!("site {n:?}: path must be absolute"));
            }
            if site.public.is_absolute()
                || site
                    .public
                    .components()
                    .any(|c| matches!(c, std::path::Component::ParentDir))
            {
                errs.push(format!(
                    "site {n:?}: public must be a relative path inside the project"
                ));
            }
            for key in site.env.keys().chain(site.env_from.keys()) {
                if !valid_env_name(key) {
                    errs.push(format!(
                        "site {n:?}: invalid environment variable name {key:?}"
                    ));
                }
            }
            if let Some(php) = &site.php {
                if php.max_children == 0 {
                    errs.push(format!("site {n:?}: php.max_children must be > 0"));
                }
                if let Some(fc) = &php.front_controller
                    && (fc.starts_with('/') || fc.contains("..") || !fc.ends_with(".php"))
                {
                    errs.push(format!(
                        "site {n:?}: php.front_controller must be a relative .php path"
                    ));
                }
                for (k, v) in &php.ini {
                    if !k
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_')
                        || v.contains(['\n', '\r'])
                    {
                        errs.push(format!("site {n:?}: invalid php.ini entry {k:?}"));
                    }
                }
            }
            http::validate_site(site, &mut errs);
            if let Some(db) = &site.database {
                if !self.services.database.contains_key(&db.service) {
                    errs.push(format!(
                        "site {n:?}: unknown database service {:?}",
                        db.service
                    ));
                }
                if !valid_env_name(&db.password_env) {
                    errs.push(format!("site {n:?}: invalid database.password_env"));
                }
            }
        }
        let mut uids: std::collections::HashMap<u32, &str> = std::collections::HashMap::new();
        for site in &self.sites {
            let uid = self.site_uid(site);
            if uid < 1000 || uid == self.isolation.worker_uid {
                errs.push(format!("site {:?}: uid {uid} is reserved", site.name));
            }
            if let Some(other) = uids.insert(uid, &site.name) {
                errs.push(format!(
                    "sites {other:?} and {:?} both map to uid {uid}; set [site.isolation] uid explicitly",
                    site.name
                ));
            }
        }
        if self.live.heartbeat_secs == 0 || self.live.max_channels == 0 {
            errs.push("live.heartbeat_secs and live.max_channels must be > 0".into());
        }
        let tls = &self.server.tls;
        if tls.enabled {
            if tls.listen.port() == self.server.listen.port() {
                errs.push("server.tls.listen must use another port than server.listen".into());
            }
            if tls.acme && tls.acme_email.as_deref().is_none_or(|e| !e.contains('@')) {
                errs.push("server.tls.acme needs server.tls.acme_email".into());
            }
            if !tls.acme && !tls.self_signed && tls.certs.is_empty() {
                errs.push(
                    "server.tls needs acme, self_signed or at least one [[server.tls.cert]]".into(),
                );
            }
        }
        let rl = &self.server.rate_limit;
        if rl.enabled && (rl.requests_per_sec <= 0.0 || rl.burst == 0) {
            errs.push("server.rate_limit needs requests_per_sec > 0 and burst > 0".into());
        }
        if self.isolation.worker_uid == 0 {
            errs.push("isolation.worker_uid must not be 0".into());
        }
        if defaults > 1 {
            errs.push("only one site may set `default = true`".into());
        }

        if errs.is_empty() {
            Ok(())
        } else {
            Err(ConfigError::Invalid(errs))
        }
    }

    /// The Unix identity a site's PHP runs as (in strict isolation).
    pub fn site_uid(&self, site: &SiteConfig) -> u32 {
        site.isolation
            .uid
            .unwrap_or_else(|| derived_uid(&site.name, self.isolation.base_uid))
    }

    /// TCP ports a site's PHP may connect to: its database plus explicit extras.
    pub fn site_connect_ports(&self, site: &SiteConfig) -> Vec<u16> {
        let mut ports = site.isolation.allow_connect.clone();
        if let Some(db) = &site.database
            && let Some(svc) = self.services.database.get(&db.service)
        {
            ports.push(svc.port);
        }
        ports.sort_unstable();
        ports.dedup();
        ports
    }

    pub fn php_enabled(&self) -> bool {
        self.sites
            .iter()
            .any(|s| s.php.as_ref().is_some_and(|p| p.enabled))
    }
}

/// FNV-1a: stable across builds, unlike std's hasher.
fn derived_uid(name: &str, base: u32) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for b in name.bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    base + h % 30_000
}

fn valid_ident(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 32
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !s.starts_with('-')
}

pub(crate) fn valid_env_name(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with(|c: char| c.is_ascii_digit())
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"
        version = 1
        [[site]]
        name = "example"
        default = true
        path = "/srv/sites/example"
    "#;

    #[test]
    fn minimal_config_uses_defaults() {
        let c = Config::from_toml(MINIMAL).unwrap();
        c.validate().unwrap();
        assert_eq!(c.mode, Mode::Production);
        assert_eq!(c.server.listen.port(), 8080);
        assert_eq!(
            c.sites[0].document_root(),
            PathBuf::from("/srv/sites/example/public")
        );
        assert!(!c.php_enabled());
    }

    #[test]
    fn site_uids_and_ports() {
        let c = Config::from_toml(
            r#"
            version = 1
            [services.database.main]
            driver = "mariadb"
            host = "db"
            port = 3306
            [[site]]
            name = "a"
            default = true
            path = "/srv/a"
            database = { service = "main", name = "a", user = "a", password_env = "A" }
            isolation = { allow_connect = [443, 3306] }
            [[site]]
            name = "b"
            hosts = ["b.test"]
            path = "/srv/b"
            isolation = { uid = 30000 }
        "#,
        )
        .unwrap();
        c.validate().unwrap();
        assert_eq!(c.site_uid(&c.sites[0]), derived_uid("a", 20000));
        assert_eq!(c.site_uid(&c.sites[1]), 30000);
        assert_eq!(c.site_connect_ports(&c.sites[0]), [443, 3306]);
        assert!(c.site_connect_ports(&c.sites[1]).is_empty());
    }

    #[test]
    fn rejects_uid_collisions() {
        let c = Config::from_toml(
            r#"
            version = 1
            [[site]]
            name = "a"
            default = true
            path = "/srv/a"
            isolation = { uid = 30000 }
            [[site]]
            name = "b"
            hosts = ["b.test"]
            path = "/srv/b"
            isolation = { uid = 30000 }
        "#,
        )
        .unwrap();
        assert!(
            c.validate()
                .unwrap_err()
                .to_string()
                .contains("both map to uid 30000")
        );
    }

    #[test]
    fn byte_sizes() {
        assert_eq!(ByteSize::parse("64MiB").unwrap().0, 64 << 20);
        assert_eq!(ByteSize::parse("512k").unwrap().0, 512 << 10);
        assert_eq!(ByteSize::parse("10").unwrap().0, 10);
        assert!(ByteSize::parse("10 parsecs").is_err());
        assert_eq!(ByteSize(256 << 20).to_php(), "256M");
    }

    #[test]
    fn rejects_unknown_fields() {
        let err = Config::from_toml("version = 1\nbogus = true").unwrap_err();
        assert!(err.to_string().contains("bogus"), "{err}");
    }

    #[test]
    fn collects_all_validation_errors() {
        let c = Config::from_toml(
            r#"
            version = 2
            [[site]]
            name = "Bad Name"
            hosts = ["a.test"]
            path = "relative"
            [[site]]
            name = "b"
            hosts = ["a.test"]
            path = "/srv/b"
            public = "../escape"
            database = { service = "missing", name = "x", user = "x", password_env = "X" }
        "#,
        )
        .unwrap();
        let ConfigError::Invalid(errs) = c.validate().unwrap_err() else {
            panic!()
        };
        let all = errs.join("\n");
        for needle in [
            "version 2",
            "Bad Name",
            "must be absolute",
            "more than one site",
            "public must be",
            "unknown database",
        ] {
            assert!(all.contains(needle), "missing {needle:?} in:\n{all}");
        }
    }
}
