//! HTTP-facing configuration: TLS, proxies, rate limits, and the per-site
//! rules applied to responses (caching, headers, redirects, error pages,
//! basic auth). Also the two small matchers these rules need: CIDR ranges
//! and path globs.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::str::FromStr;

// ---------------------------------------------------------------------------
// CIDR ranges

/// An IP network such as `10.0.0.0/8` or `::1/128` (a bare address is a /32 or /128).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cidr {
    addr: IpAddr,
    prefix: u8,
}

impl Cidr {
    pub fn contains(&self, ip: IpAddr) -> bool {
        // IPv4-mapped IPv6 peers (`::ffff:10.0.0.1`) match IPv4 ranges.
        let ip = match ip {
            IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(ip),
            v4 => v4,
        };
        match (self.addr, ip) {
            (IpAddr::V4(net), IpAddr::V4(ip)) => {
                let mask = u32::MAX.checked_shl(32 - self.prefix as u32).unwrap_or(0);
                u32::from(net) & mask == u32::from(ip) & mask
            }
            (IpAddr::V6(net), IpAddr::V6(ip)) => {
                let mask = u128::MAX.checked_shl(128 - self.prefix as u32).unwrap_or(0);
                u128::from(net) & mask == u128::from(ip) & mask
            }
            _ => false,
        }
    }

    pub fn any_contains(list: &[Cidr], ip: IpAddr) -> bool {
        list.iter().any(|c| c.contains(ip))
    }

    /// Loopback and private ranges (RFC 1918, RFC 4193).
    pub fn private_ranges() -> Vec<Cidr> {
        [
            "127.0.0.0/8",
            "10.0.0.0/8",
            "172.16.0.0/12",
            "192.168.0.0/16",
            "::1/128",
            "fc00::/7",
        ]
        .iter()
        .map(|s| s.parse().expect("valid built-in range"))
        .collect()
    }
}

impl Cidr {
    /// Prefix length (`8` for `10.0.0.0/8`).
    pub fn prefix(&self) -> u8 {
        self.prefix
    }
}

/// Which header a trusted proxy uses to pass the client address on.
/// Only this one is read: proxies typically append to their own header
/// and pass any other one through unchanged, so reading both would let a
/// client forge its address.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
pub enum ForwardedHeader {
    /// `X-Forwarded-For` + `X-Forwarded-Proto` (nginx, HAProxy, Traefik, most CDNs).
    #[default]
    #[serde(rename = "x-forwarded-for")]
    XForwardedFor,
    /// RFC 7239 `Forwarded: for=...;proto=...`.
    #[serde(rename = "forwarded")]
    Forwarded,
}

impl FromStr for Cidr {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        let (addr, prefix) = match s.split_once('/') {
            Some((a, p)) => (a, Some(p)),
            None => (s, None),
        };
        let addr: IpAddr = addr
            .trim()
            .parse()
            .map_err(|_| format!("invalid IP range {s:?}"))?;
        let max = if addr.is_ipv4() { 32 } else { 128 };
        let prefix = match prefix {
            Some(p) => p
                .trim()
                .parse::<u8>()
                .ok()
                .filter(|p| *p <= max)
                .ok_or_else(|| format!("invalid prefix length in {s:?}"))?,
            None => max,
        };
        Ok(Cidr { addr, prefix })
    }
}

impl fmt::Display for Cidr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.addr, self.prefix)
    }
}

impl<'de> Deserialize<'de> for Cidr {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        String::deserialize(d)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

impl Serialize for Cidr {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

// ---------------------------------------------------------------------------
// Path globs

/// Match a `/`-separated path against a glob: `*` matches within one
/// segment, `**` matches any number of segments, `?` one character.
/// Leading slashes on either side are ignored.
pub fn glob_match(pattern: &str, path: &str) -> bool {
    let p: Vec<&str> = pattern.trim_start_matches('/').split('/').collect();
    let s: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    match_segments(&p, &s)
}

fn match_segments(p: &[&str], s: &[&str]) -> bool {
    match p.split_first() {
        None => s.is_empty(),
        Some((&"**", rest)) => (0..=s.len()).any(|i| match_segments(rest, &s[i..])),
        Some((first, rest)) => match s.split_first() {
            Some((seg, srest)) => {
                match_segment(first.as_bytes(), seg.as_bytes()) && match_segments(rest, srest)
            }
            None => false,
        },
    }
}

fn match_segment(p: &[u8], s: &[u8]) -> bool {
    match p.split_first() {
        None => s.is_empty(),
        Some((b'*', rest)) => (0..=s.len()).any(|i| match_segment(rest, &s[i..])),
        Some((b'?', rest)) => !s.is_empty() && match_segment(rest, &s[1..]),
        Some((c, rest)) => s.first() == Some(c) && match_segment(rest, &s[1..]),
    }
}

// ---------------------------------------------------------------------------
// Server-level settings

/// `[server.tls]`: HTTPS with automatic certificates, HTTP/2 and HTTP/3.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct TlsConfig {
    pub enabled: bool,
    /// TCP (HTTP/1.1 + HTTP/2) and UDP (HTTP/3) address for HTTPS.
    pub listen: SocketAddr,
    /// Port clients use for HTTPS (redirects, Alt-Svc), e.g. 443 when the
    /// container maps 443 → 8443.
    pub public_port: u16,
    /// Serve HTTP/3 over QUIC on the same port (UDP).
    pub http3: bool,
    /// Obtain certificates from an ACME CA (Let's Encrypt) for public hosts.
    pub acme: bool,
    pub acme_email: Option<String>,
    pub acme_directory: String,
    /// Generate a self-signed certificate for hosts ACME cannot serve
    /// (`localhost`, `*.localhost`, `*.test`, IP addresses, or everything when ACME is off).
    pub self_signed: bool,
    /// `Strict-Transport-Security` max-age on HTTPS responses; 0 disables it.
    pub hsts_max_age_secs: u64,
    /// Certificates managed outside NOVA (PEM files).
    #[serde(rename = "cert")]
    pub certs: Vec<CertFiles>,
}

impl Default for TlsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            listen: "0.0.0.0:8443".parse().unwrap(),
            public_port: 443,
            http3: true,
            acme: false,
            acme_email: None,
            acme_directory: "https://acme-v02.api.letsencrypt.org/directory".into(),
            self_signed: true,
            hsts_max_age_secs: 31_536_000,
            certs: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CertFiles {
    pub hosts: Vec<String>,
    pub cert: PathBuf,
    pub key: PathBuf,
}

/// `[server.rate_limit]`: per-client token bucket and connection cap.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct RateLimitConfig {
    pub enabled: bool,
    /// Sustained requests per second per client IP.
    pub requests_per_sec: f64,
    /// Requests a client may make in a burst above the sustained rate.
    pub burst: u32,
    /// Open connections per client IP (0 = unlimited).
    pub max_connections_per_ip: usize,
    /// Clients never limited (in addition to `server.admin_allow`).
    pub exempt: Vec<Cidr>,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            requests_per_sec: 100.0,
            burst: 400,
            max_connections_per_ip: 256,
            exempt: Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Site-level rules

/// `[site.cache]`: `Cache-Control` for static files.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct SiteCache {
    /// Mark fingerprinted build output (Vite and Laravel Mix manifests)
    /// as immutable.
    pub auto_immutable: bool,
    /// Extra globs (relative to the document root) of files whose content
    /// never changes under the same URL.
    pub immutable: Vec<String>,
    /// `max-age` for every other static file (revalidated with ETag after).
    pub max_age_secs: u64,
    /// Path-specific `Cache-Control` values, first match wins.
    #[serde(rename = "rule")]
    pub rules: Vec<CacheRule>,
}

impl Default for SiteCache {
    fn default() -> Self {
        Self {
            auto_immutable: true,
            immutable: Vec::new(),
            max_age_secs: 0,
            rules: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CacheRule {
    /// Glob relative to the document root.
    pub path: String,
    pub cache_control: String,
}

/// `[[site.redirect]]`
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Redirect {
    /// Exact URL path, or a prefix ending in `*` (`/blog/*`).
    pub from: String,
    /// Target path or absolute URL; `$1` is replaced by what `*` matched.
    pub to: String,
    #[serde(default = "default_redirect_status")]
    pub status: u16,
    /// Append the request's query string to the target.
    #[serde(default = "crate::default_true")]
    pub keep_query: bool,
}

fn default_redirect_status() -> u16 {
    301
}

/// `[site.auth]`: HTTP basic authentication (staging sites, admin areas).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SiteAuth {
    #[serde(default = "default_realm")]
    pub realm: String,
    /// Environment variable with `user:bcrypt-hash` pairs separated by
    /// commas or newlines (`htpasswd -nbB user pass` output).
    #[serde(default)]
    pub users_env: Option<String>,
    /// Or an htpasswd file (bcrypt entries only).
    #[serde(default)]
    pub users_file: Option<PathBuf>,
    /// Globs (URL paths) that require authentication; empty = the whole site.
    #[serde(default)]
    pub paths: Vec<String>,
    /// Globs exempt from authentication (`/.well-known/**`, webhooks).
    #[serde(default)]
    pub except: Vec<String>,
}

fn default_realm() -> String {
    "Restricted".into()
}

/// Validate the HTTP rules of one site.
pub(crate) fn validate_site(site: &crate::SiteConfig, errs: &mut Vec<String>) {
    let n = &site.name;
    for r in &site.redirects {
        if !r.from.starts_with('/') {
            errs.push(format!(
                "site {n:?}: redirect from {:?} must start with /",
                r.from
            ));
        }
        if r.from[..r.from.len().saturating_sub(1)].contains('*') {
            errs.push(format!(
                "site {n:?}: redirect from {:?}: `*` is only allowed at the end",
                r.from
            ));
        }
        if ![301, 302, 303, 307, 308].contains(&r.status) {
            errs.push(format!(
                "site {n:?}: redirect status {} must be 301, 302, 303, 307 or 308",
                r.status
            ));
        }
        if r.to.is_empty() || r.to.contains(['\r', '\n']) {
            errs.push(format!("site {n:?}: invalid redirect target {:?}", r.to));
        }
    }
    for (name, value) in &site.headers {
        if name.is_empty()
            || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            || value.contains(['\r', '\n'])
        {
            errs.push(format!("site {n:?}: invalid header {name:?}"));
        }
    }
    for (code, page) in &site.error_pages {
        if !code.parse::<u16>().is_ok_and(|c| (400..600).contains(&c)) {
            errs.push(format!(
                "site {n:?}: error_pages key {code:?} must be a 4xx/5xx status"
            ));
        }
        if !page.starts_with('/') || page.contains("..") {
            errs.push(format!(
                "site {n:?}: error page {page:?} must be an absolute URL path"
            ));
        }
    }
    if let Some(h) = &site.canonical_host
        && (h.is_empty() || h.contains([':', '/', ' ']))
    {
        errs.push(format!("site {n:?}: invalid canonical_host {h:?}"));
    }
    for rule in &site.cache.rules {
        if rule.cache_control.contains(['\r', '\n']) {
            errs.push(format!(
                "site {n:?}: invalid cache rule for {:?}",
                rule.path
            ));
        }
    }
    if let Some(p) = &site.proxy {
        if let Err(e) = p.upstream_addr() {
            errs.push(format!("site {n:?}: [site.proxy] {e}"));
        }
        if p.paths.is_empty() {
            errs.push(format!("site {n:?}: [site.proxy] paths must not be empty"));
        }
        if p.timeout_secs == 0 {
            errs.push(format!("site {n:?}: [site.proxy] timeout_secs must be > 0"));
        }
    }
    if let Some(a) = &site.auth {
        if a.realm.contains(['"', '\r', '\n']) {
            errs.push(format!("site {n:?}: invalid [site.auth] realm"));
        }
        match (&a.users_env, &a.users_file) {
            (Some(v), None) if crate::valid_env_name(v) => {}
            (None, Some(f)) if f.is_absolute() => {}
            _ => errs.push(format!(
                "site {n:?}: [site.auth] needs either users_env (a variable name) or users_file (an absolute path)"
            )),
        }
    }
}

/// `[site.proxy]`: hand requests to an HTTP/1.1 application server
/// (Node.js, Python, Go, ...), including WebSocket upgrades.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SiteProxy {
    /// `http://host:port`, e.g. `http://127.0.0.1:3000` or `http://app:8000`.
    pub upstream: String,
    /// URL path globs sent upstream; the rest is served by NOVA.
    #[serde(default = "default_proxy_paths")]
    pub paths: Vec<String>,
    /// Serve files that exist in the document root without asking the
    /// upstream (assets, images: optimized and cached by NOVA).
    #[serde(default = "crate::default_true")]
    pub static_first: bool,
    /// Time allowed for the upstream's response headers.
    #[serde(default = "default_proxy_timeout")]
    pub timeout_secs: u64,
}

fn default_proxy_paths() -> Vec<String> {
    vec!["/**".into()]
}

fn default_proxy_timeout() -> u64 {
    60
}

impl SiteProxy {
    /// `(host, port)` of `upstream`; only plain `http://host:port[/]`.
    pub fn upstream_addr(&self) -> Result<(String, u16), String> {
        let rest = self
            .upstream
            .strip_prefix("http://")
            .ok_or_else(|| format!("upstream {:?} must start with http://", self.upstream))?;
        let authority = rest.trim_end_matches('/');
        if authority.contains('/') || authority.contains('@') || authority.is_empty() {
            return Err(format!(
                "upstream {:?} must be http://host:port",
                self.upstream
            ));
        }
        let (host, port) = authority
            .rsplit_once(':')
            .ok_or_else(|| format!("upstream {:?} needs an explicit port", self.upstream))?;
        let port: u16 = port
            .parse()
            .map_err(|_| format!("upstream {:?} has an invalid port", self.upstream))?;
        Ok((host.trim_matches(['[', ']']).to_string(), port))
    }

    pub fn matches(&self, path: &str) -> bool {
        self.paths.iter().any(|g| glob_match(g, path))
    }
}

/// Default `server.admin_allow`: loopback and private networks.
pub fn default_admin_allow() -> Vec<Cidr> {
    Cidr::private_ranges()
}

/// Default request-body idle timeout.
pub const DEFAULT_BODY_TIMEOUT_SECS: u64 = 60;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cidr() {
        let c: Cidr = "10.0.0.0/8".parse().unwrap();
        assert!(c.contains("10.1.2.3".parse().unwrap()));
        assert!(c.contains("::ffff:10.1.2.3".parse().unwrap()));
        assert!(!c.contains("11.0.0.1".parse().unwrap()));
        let one: Cidr = "192.168.1.5".parse().unwrap();
        assert!(one.contains("192.168.1.5".parse().unwrap()));
        assert!(!one.contains("192.168.1.6".parse().unwrap()));
        let all: Cidr = "0.0.0.0/0".parse().unwrap();
        assert!(all.contains("8.8.8.8".parse().unwrap()));
        let v6: Cidr = "fc00::/7".parse().unwrap();
        assert!(v6.contains("fd12::1".parse().unwrap()));
        assert!(!v6.contains("2001:db8::1".parse().unwrap()));
        assert!("10.0.0.0/33".parse::<Cidr>().is_err());
        assert!("nope".parse::<Cidr>().is_err());
    }

    #[test]
    fn globs() {
        assert!(glob_match("build/assets/*", "build/assets/app-x.css"));
        assert!(glob_match("/build/assets/*", "/build/assets/app-x.css"));
        assert!(!glob_match("build/assets/*", "build/assets/sub/app.css"));
        assert!(glob_match("build/**", "build/assets/sub/app.css"));
        assert!(glob_match("**/*.woff2", "fonts/a/b.woff2"));
        assert!(glob_match("**/*.woff2", "b.woff2"));
        assert!(glob_match("img/?.png", "img/a.png"));
        assert!(!glob_match("img/?.png", "img/ab.png"));
        assert!(glob_match("/admin/**", "/admin"));
        assert!(glob_match("/admin/**", "/admin/users/1"));
        assert!(!glob_match("/admin/**", "/administrator"));
    }

    #[test]
    fn proxy_upstreams() {
        let p = |u: &str| SiteProxy {
            upstream: u.into(),
            paths: vec!["/api/**".into()],
            static_first: true,
            timeout_secs: 60,
        };
        assert_eq!(
            p("http://127.0.0.1:3000").upstream_addr(),
            Ok(("127.0.0.1".into(), 3000))
        );
        assert_eq!(
            p("http://app:8000/").upstream_addr(),
            Ok(("app".into(), 8000))
        );
        assert_eq!(
            p("http://[::1]:9000").upstream_addr(),
            Ok(("::1".into(), 9000))
        );
        for bad in [
            "https://a:1",
            "http://a",
            "http://a:x",
            "http://a:1/path",
            "http://u@a:1",
            "a:1",
        ] {
            assert!(p(bad).upstream_addr().is_err(), "{bad} accepted");
        }
        assert!(p("http://a:1").matches("/api/v1/items"));
        assert!(!p("http://a:1").matches("/assets/app.js"));
    }
}
