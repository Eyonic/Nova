//! Per-site HTTP rules: static-file caching, redirects, response headers,
//! error pages and basic authentication.

use http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use nova_config::{CacheRule, Redirect, SiteConfig, glob_match};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

/// `Cache-Control` for content that never changes under its URL.
pub const IMMUTABLE: &str = "public, max-age=31536000, immutable";

pub struct SiteRules {
    pub cache: CachePolicy,
    pub headers: Vec<(HeaderName, HeaderValue)>,
    pub security_headers: bool,
    pub canonical_host: Option<String>,
    pub https_redirect: Option<bool>,
    pub redirects: Vec<Redirect>,
    pub error_pages: HashMap<u16, String>,
    pub auth: Option<BasicAuth>,
}

impl SiteRules {
    pub fn new(site: &SiteConfig, root: &Path) -> Result<Self, String> {
        let headers = site
            .headers
            .iter()
            .map(|(k, v)| {
                Ok((
                    HeaderName::from_bytes(k.as_bytes()).map_err(|e| e.to_string())?,
                    HeaderValue::from_str(v).map_err(|e| e.to_string())?,
                ))
            })
            .collect::<Result<Vec<_>, String>>()
            .map_err(|e| format!("site {:?}: header: {e}", site.name))?;
        let auth = match &site.auth {
            Some(a) => Some(BasicAuth::new(site, a)?),
            None => None,
        };
        Ok(Self {
            cache: CachePolicy::new(site, root),
            headers,
            security_headers: site.security_headers,
            canonical_host: site.canonical_host.as_ref().map(|h| h.to_ascii_lowercase()),
            https_redirect: site.https_redirect,
            redirects: site.redirects.clone(),
            error_pages: site
                .error_pages
                .iter()
                .filter_map(|(k, v)| Some((k.parse().ok()?, v.clone())))
                .collect(),
            auth,
        })
    }

    /// The first configured redirect matching `path` (raw, percent-encoded).
    pub fn redirect(&self, path: &str, query: Option<&str>) -> Option<(StatusCode, String)> {
        self.redirects.iter().find_map(|r| {
            let target = match r.from.strip_suffix('*') {
                Some(prefix) => {
                    let rest = path.strip_prefix(prefix)?;
                    r.to.replace("$1", rest)
                }
                None if path == r.from => r.to.clone(),
                None => return None,
            };
            let target = match query.filter(|q| r.keep_query && !q.is_empty()) {
                Some(q) if target.contains('?') => format!("{target}&{q}"),
                Some(q) => format!("{target}?{q}"),
                None => target,
            };
            Some((StatusCode::from_u16(r.status).ok()?, target))
        })
    }

    /// Configured headers and the security baseline, where the response has
    /// not set them itself.
    pub fn apply_headers(&self, h: &mut HeaderMap, https: bool, hsts_max_age: u64) {
        for (k, v) in &self.headers {
            if !h.contains_key(k) {
                h.insert(k.clone(), v.clone());
            }
        }
        if !self.security_headers {
            return;
        }
        let defaults = [
            ("x-content-type-options", "nosniff"),
            ("referrer-policy", "strict-origin-when-cross-origin"),
            ("x-frame-options", "SAMEORIGIN"),
        ];
        for (k, v) in defaults {
            if !h.contains_key(k) {
                h.insert(k, HeaderValue::from_static(v));
            }
        }
        if https
            && hsts_max_age > 0
            && !h.contains_key(http::header::STRICT_TRANSPORT_SECURITY)
            && let Ok(v) = HeaderValue::from_str(&format!("max-age={hsts_max_age}"))
        {
            h.insert(http::header::STRICT_TRANSPORT_SECURITY, v);
        }
    }
}

// ---------------------------------------------------------------------------
// Caching

pub struct CachePolicy {
    root: PathBuf,
    auto: bool,
    immutable: Vec<String>,
    max_age: u64,
    rules: Vec<CacheRule>,
    manifests: Mutex<Manifests>,
}

#[derive(Default)]
struct Manifests {
    checked: Option<Instant>,
    stamps: Vec<Option<SystemTime>>,
    /// Fingerprinted files (document-root relative) from Vite manifests.
    files: HashSet<String>,
    /// Laravel Mix: path → expected query string (`id=…`).
    mix: HashMap<String, String>,
}

/// Vite manifests and the directory their `file` entries are relative to.
const VITE_MANIFESTS: [(&str, &str); 5] = [
    ("build/manifest.json", "build"),
    ("build/.vite/manifest.json", "build"),
    (".vite/manifest.json", ""),
    ("dist/.vite/manifest.json", "dist"),
    ("assets/.vite/manifest.json", "assets"),
];
const MIX_MANIFEST: &str = "mix-manifest.json";

impl CachePolicy {
    pub fn new(site: &SiteConfig, root: &Path) -> Self {
        Self {
            root: root.to_owned(),
            auto: site.cache.auto_immutable,
            immutable: site.cache.immutable.clone(),
            max_age: site.cache.max_age_secs,
            rules: site.cache.rules.clone(),
            manifests: Mutex::new(Manifests::default()),
        }
    }

    /// `Cache-Control` for a static file (`rel` is document-root relative).
    pub fn cache_control(&self, rel: &str, query: Option<&str>, dev: bool) -> String {
        if let Some(r) = self.rules.iter().find(|r| glob_match(&r.path, rel)) {
            return r.cache_control.clone();
        }
        if self.immutable.iter().any(|g| glob_match(g, rel)) || self.fingerprinted(rel, query) {
            return IMMUTABLE.to_string();
        }
        if dev {
            "no-cache".into()
        } else if self.max_age > 0 {
            format!("public, max-age={}", self.max_age)
        } else {
            "public, max-age=0, must-revalidate".into()
        }
    }

    fn fingerprinted(&self, rel: &str, query: Option<&str>) -> bool {
        if !self.auto {
            return false;
        }
        let mut m = self.manifests.lock().unwrap();
        self.refresh(&mut m);
        let rel = rel.trim_start_matches('/');
        if m.files.contains(rel) {
            return true;
        }
        match (m.mix.get(rel), query) {
            (Some(expected), Some(q)) => expected == q,
            _ => false,
        }
    }

    /// Re-read manifests when one changed; stat them at most every 2 s.
    fn refresh(&self, m: &mut Manifests) {
        if m.checked
            .is_some_and(|t| t.elapsed() < Duration::from_secs(2))
        {
            return;
        }
        m.checked = Some(Instant::now());
        let paths: Vec<PathBuf> = VITE_MANIFESTS
            .iter()
            .map(|(p, _)| *p)
            .chain([MIX_MANIFEST])
            .map(|p| self.root.join(p))
            .collect();
        let stamps: Vec<Option<SystemTime>> = paths
            .iter()
            .map(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok())
            .collect();
        if stamps == m.stamps {
            return;
        }
        m.stamps = stamps;
        m.files.clear();
        m.mix.clear();
        for ((_, base), path) in VITE_MANIFESTS.iter().zip(&paths) {
            if let Ok(text) = std::fs::read_to_string(path) {
                vite_files(&text, base, &mut m.files);
            }
        }
        if let Ok(text) = std::fs::read_to_string(&paths[VITE_MANIFESTS.len()]) {
            mix_files(&text, &mut m.mix);
        }
    }
}

fn vite_files(text: &str, base: &str, out: &mut HashSet<String>) {
    let Ok(serde_json::Value::Object(entries)) = serde_json::from_str(text) else {
        return;
    };
    let join = |f: &str| {
        let f = f.trim_start_matches('/');
        if base.is_empty() {
            f.to_string()
        } else {
            format!("{base}/{f}")
        }
    };
    for entry in entries.values() {
        if let Some(f) = entry.get("file").and_then(|f| f.as_str()) {
            out.insert(join(f));
        }
        for key in ["css", "assets"] {
            for f in entry
                .get(key)
                .and_then(|v| v.as_array())
                .into_iter()
                .flatten()
                .filter_map(|v| v.as_str())
            {
                out.insert(join(f));
            }
        }
    }
}

fn mix_files(text: &str, out: &mut HashMap<String, String>) {
    let Ok(serde_json::Value::Object(entries)) = serde_json::from_str(text) else {
        return;
    };
    for versioned in entries.values() {
        if let Some((p, q)) = versioned.as_str().and_then(|v| v.split_once('?')) {
            out.insert(p.trim_start_matches('/').to_string(), q.to_string());
        }
    }
}

// ---------------------------------------------------------------------------
// Basic authentication

pub struct BasicAuth {
    pub realm: String,
    users: HashMap<String, String>,
    paths: Vec<String>,
    except: Vec<String>,
    /// `Authorization` values already verified (bcrypt is deliberately slow).
    verified: Mutex<HashSet<String>>,
}

const VERIFIED_CACHE: usize = 1024;

impl BasicAuth {
    fn new(site: &SiteConfig, cfg: &nova_config::SiteAuth) -> Result<Self, String> {
        let text = match (&cfg.users_env, &cfg.users_file) {
            (Some(var), _) => std::env::var(var).map_err(|_| {
                format!("site {:?}: auth users variable {var} is not set", site.name)
            })?,
            (None, Some(file)) => std::fs::read_to_string(file).map_err(|e| {
                format!(
                    "site {:?}: cannot read auth users file {}: {e}",
                    site.name,
                    file.display()
                )
            })?,
            (None, None) => unreachable!("validated"),
        };
        let users: HashMap<String, String> = text
            .split([',', '\n'])
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .filter_map(|l| l.split_once(':'))
            .map(|(u, h)| (u.to_string(), h.to_string()))
            .collect();
        if users.is_empty() {
            return Err(format!("site {:?}: auth has no users", site.name));
        }
        if let Some((u, _)) = users.iter().find(|(_, h)| !h.starts_with("$2")) {
            return Err(format!(
                "site {:?}: auth user {u:?} needs a bcrypt hash (htpasswd -nbB)",
                site.name
            ));
        }
        Ok(Self {
            realm: cfg.realm.clone(),
            users,
            paths: cfg.paths.clone(),
            except: cfg.except.clone(),
            verified: Mutex::new(HashSet::new()),
        })
    }

    pub fn applies(&self, path: &str) -> bool {
        (self.paths.is_empty() || self.paths.iter().any(|g| glob_match(g, path)))
            && !self.except.iter().any(|g| glob_match(g, path))
    }

    pub async fn check(&self, authorization: Option<&str>) -> bool {
        let Some(value) = authorization else {
            return false;
        };
        if self.verified.lock().unwrap().contains(value) {
            return true;
        }
        let Some(encoded) = value
            .strip_prefix("Basic ")
            .or_else(|| value.strip_prefix("basic "))
        else {
            return false;
        };
        let Some(decoded) = base64_decode(encoded.trim()) else {
            return false;
        };
        let Ok(decoded) = String::from_utf8(decoded) else {
            return false;
        };
        let Some((user, password)) = decoded.split_once(':') else {
            return false;
        };
        let Some(hash) = self.users.get(user).cloned() else {
            return false;
        };
        let password = password.to_string();
        let ok =
            tokio::task::spawn_blocking(move || bcrypt::verify(password, &hash).unwrap_or(false))
                .await
                .unwrap_or(false);
        if ok {
            let mut v = self.verified.lock().unwrap();
            if v.len() >= VERIFIED_CACHE {
                v.clear();
            }
            v.insert(value.to_string());
        }
        ok
    }
}

fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let val = |c: u8| -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        } as u32)
    };
    let s = s.trim_end_matches('=').as_bytes();
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    for chunk in s.chunks(4) {
        let mut n = 0u32;
        for (i, &c) in chunk.iter().enumerate() {
            n |= val(c)? << (18 - 6 * i);
        }
        let bytes = n.to_be_bytes();
        match chunk.len() {
            4 => out.extend_from_slice(&bytes[1..4]),
            3 => out.extend_from_slice(&bytes[1..3]),
            2 => out.push(bytes[1]),
            _ => return None,
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn site(extra: &str) -> SiteConfig {
        let cfg = nova_config::Config::from_toml(&format!(
            "version = 1\n[[site]]\nname = \"t\"\ndefault = true\npath = \"/srv\"\n{extra}"
        ))
        .unwrap();
        cfg.sites[0].clone()
    }

    #[test]
    fn redirects() {
        let s = site(
            r#"
            [[site.redirect]]
            from = "/old"
            to = "/new"
            [[site.redirect]]
            from = "/blog/*"
            to = "https://blog.example.com/$1"
            status = 308
            "#,
        );
        let r = SiteRules::new(&s, Path::new("/srv/public")).unwrap();
        assert_eq!(
            r.redirect("/old", Some("a=1")),
            Some((StatusCode::MOVED_PERMANENTLY, "/new?a=1".into()))
        );
        assert_eq!(
            r.redirect("/blog/2026/hi", None),
            Some((
                StatusCode::PERMANENT_REDIRECT,
                "https://blog.example.com/2026/hi".into()
            ))
        );
        assert_eq!(r.redirect("/older", None), None);
    }

    #[test]
    fn vite_and_mix_manifests() {
        let root = std::env::temp_dir().join(format!("nova-rules-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("build")).unwrap();
        std::fs::write(
            root.join("build/manifest.json"),
            r#"{"resources/js/app.js":{"file":"assets/app-CH3EuBRR.js","css":["assets/app-DaWUEnDd.css"]}}"#,
        )
        .unwrap();
        std::fs::write(
            root.join("mix-manifest.json"),
            r#"{"/js/app.js":"/js/app.js?id=abc123"}"#,
        )
        .unwrap();
        let p = CachePolicy::new(&site(""), &root);
        assert_eq!(
            p.cache_control("build/assets/app-CH3EuBRR.js", None, false),
            IMMUTABLE
        );
        assert_eq!(
            p.cache_control("build/assets/app-DaWUEnDd.css", None, true),
            IMMUTABLE
        );
        assert_eq!(
            p.cache_control("js/app.js", Some("id=abc123"), false),
            IMMUTABLE
        );
        assert_eq!(
            p.cache_control("js/app.js", Some("id=old"), false),
            "public, max-age=0, must-revalidate"
        );
        assert_eq!(p.cache_control("index.html", None, true), "no-cache");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn cache_rules_and_globs() {
        let s = site(
            r#"
            [site.cache]
            immutable = ["fonts/**"]
            max_age_secs = 300
            [[site.cache.rule]]
            path = "downloads/**"
            cache_control = "private, no-store"
            "#,
        );
        let p = CachePolicy::new(&s, Path::new("/nonexistent"));
        assert_eq!(p.cache_control("fonts/a/b.woff2", None, false), IMMUTABLE);
        assert_eq!(
            p.cache_control("downloads/x.zip", None, false),
            "private, no-store"
        );
        assert_eq!(p.cache_control("x.css", None, false), "public, max-age=300");
    }

    #[test]
    fn security_headers_do_not_override() {
        let s = site("[site.headers]\nContent-Security-Policy = \"default-src 'self'\"\n");
        let r = SiteRules::new(&s, Path::new("/srv")).unwrap();
        let mut h = HeaderMap::new();
        h.insert("x-frame-options", HeaderValue::from_static("DENY"));
        r.apply_headers(&mut h, true, 600);
        assert_eq!(h["x-frame-options"], "DENY");
        assert_eq!(h["content-security-policy"], "default-src 'self'");
        assert_eq!(h["strict-transport-security"], "max-age=600");
        let mut plain = HeaderMap::new();
        r.apply_headers(&mut plain, false, 600);
        assert!(!plain.contains_key("strict-transport-security"));
    }

    #[tokio::test]
    async fn basic_auth() {
        let hash = bcrypt::hash("s3cret", 4).unwrap();
        // SAFETY: test-only, single-threaded access to this variable.
        unsafe { std::env::set_var("NOVA_TEST_AUTH_USERS", format!("alice:{hash}")) };
        let s = site(
            "[site.auth]\nusers_env = \"NOVA_TEST_AUTH_USERS\"\nexcept = [\"/.well-known/**\"]\n",
        );
        let r = SiteRules::new(&s, Path::new("/srv")).unwrap();
        let a = r.auth.as_ref().unwrap();
        assert!(a.applies("/admin"));
        assert!(!a.applies("/.well-known/acme-challenge/x"));
        assert!(a.check(Some("Basic YWxpY2U6czNjcmV0")).await); // alice:s3cret
        assert!(a.check(Some("Basic YWxpY2U6czNjcmV0")).await); // cached
        assert!(!a.check(Some("Basic YWxpY2U6d3Jvbmc=")).await); // alice:wrong
        assert!(!a.check(None).await);
    }

    #[test]
    fn base64() {
        assert_eq!(base64_decode("YWxpY2U6czNjcmV0").unwrap(), b"alice:s3cret");
        assert_eq!(base64_decode("YQ==").unwrap(), b"a");
        assert_eq!(base64_decode("YWI=").unwrap(), b"ab");
        assert!(base64_decode("!!!!").is_none());
    }
}
