//! Opt-in micro-cache for PHP responses (`[site.php] micro_cache_secs`).
//!
//! A few seconds of caching turns a traffic spike on a public page into
//! one PHP execution per page per TTL. Only responses that are identical
//! for every visitor are stored:
//!
//! * request: `GET`, no `Cookie`, no `Authorization`;
//! * response: `200`, no `Set-Cookie`, no `Cache-Control: private /
//!   no-store / no-cache`, no `Vary` beyond `Accept-Encoding` (compression
//!   is applied after the cache), not an event stream, at most
//!   [`MAX_ENTRY`] bytes.
//!
//! A `Nova-Publish` from the site (its data changed) clears that site's
//! entries, so NOVA Live refetches always see fresh content.

use bytes::Bytes;
use http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Largest response body that is cached.
pub const MAX_ENTRY: usize = 1 << 20;
/// Total cache size across sites.
const BUDGET: usize = 32 << 20;

#[derive(Clone)]
pub struct Entry {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Bytes,
    stored: Instant,
    expires: Instant,
}

impl Entry {
    pub fn age(&self) -> Duration {
        self.stored.elapsed()
    }
}

#[derive(Default)]
pub struct MicroCache {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    map: HashMap<String, Entry>,
    bytes: usize,
}

/// Cache key for a request, or `None` when the request may be personalized.
/// `site` scopes keys per site; Host, scheme and the NOVA Live headers are
/// part of the key because PHP sees (and may render) them.
pub fn key(
    site: &str,
    method: &Method,
    uri: &http::Uri,
    headers: &HeaderMap,
    https: bool,
) -> Option<String> {
    if method != Method::GET
        || headers.contains_key(header::COOKIE)
        || headers.contains_key(header::AUTHORIZATION)
    {
        return None;
    }
    let h = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
    };
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .or_else(|| uri.authority().map(|a| a.as_str()))
        .unwrap_or("");
    Some(format!(
        "{site}\n{}\n{}\n{}\n{}\n{}",
        host.to_ascii_lowercase(),
        if https { "https" } else { "http" },
        uri.path_and_query().map(|p| p.as_str()).unwrap_or("/"),
        h("nova-live"),
        h("nova-target"),
    ))
}

/// Whether a PHP response may be shared between visitors.
pub fn cacheable(status: StatusCode, headers: &HeaderMap) -> bool {
    if status != StatusCode::OK || headers.contains_key(header::SET_COOKIE) {
        return false;
    }
    let no_cache = headers
        .get_all(header::CACHE_CONTROL)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .any(|d| {
            let d = d.trim().to_ascii_lowercase();
            d == "private" || d == "no-store" || d == "no-cache" || d.starts_with("private=")
        });
    let odd_vary = headers
        .get_all(header::VARY)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .any(|v| !v.trim().eq_ignore_ascii_case("accept-encoding") && !v.trim().is_empty());
    let stream = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("text/event-stream"));
    !(no_cache || odd_vary || stream)
}

impl MicroCache {
    pub fn get(&self, key: &str) -> Option<Entry> {
        let mut inner = self.inner.lock().unwrap();
        match inner.map.get(key) {
            Some(e) if e.expires > Instant::now() => Some(e.clone()),
            Some(_) => {
                if let Some(old) = inner.map.remove(key) {
                    inner.bytes -= old.body.len();
                }
                None
            }
            None => None,
        }
    }

    pub fn put(
        &self,
        key: String,
        status: StatusCode,
        headers: HeaderMap,
        body: Bytes,
        ttl: Duration,
    ) {
        if body.len() > MAX_ENTRY {
            return;
        }
        let now = Instant::now();
        let mut inner = self.inner.lock().unwrap();
        if let Some(old) = inner.map.remove(&key) {
            inner.bytes -= old.body.len();
        }
        if inner.bytes + body.len() > BUDGET {
            // Expired entries first, then anything (entries live seconds).
            inner.map.retain(|_, e| e.expires > now);
            inner.bytes = inner.map.values().map(|e| e.body.len()).sum();
            while inner.bytes + body.len() > BUDGET {
                let Some(k) = inner.map.keys().next().cloned() else {
                    break;
                };
                if let Some(old) = inner.map.remove(&k) {
                    inner.bytes -= old.body.len();
                }
            }
        }
        inner.bytes += body.len();
        inner.map.insert(
            key,
            Entry {
                status,
                headers,
                body,
                stored: now,
                expires: now + ttl,
            },
        );
    }

    /// Drop every entry of `site` (its data changed).
    pub fn purge_site(&self, site: &str) {
        let prefix = format!("{site}\n");
        let mut inner = self.inner.lock().unwrap();
        inner.map.retain(|k, _| !k.starts_with(&prefix));
        inner.bytes = inner.map.values().map(|e| e.body.len()).sum();
    }
}

/// `nova-cache: hit` / `miss` (and `age` on hits) for debugging.
pub fn mark(headers: &mut HeaderMap, hit: Option<Duration>) {
    headers.insert(
        "nova-cache",
        HeaderValue::from_static(if hit.is_some() { "hit" } else { "miss" }),
    );
    if let Some(age) = hit {
        headers.insert(header::AGE, age.as_secs().into());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hdrs(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.append(*k, v.parse().unwrap());
        }
        h
    }

    #[test]
    fn personalized_requests_are_not_keyed() {
        let uri: http::Uri = "/page?x=1".parse().unwrap();
        let host = hdrs(&[("host", "Example.com")]);
        let k = key("s", &Method::GET, &uri, &host, true).unwrap();
        assert!(k.contains("example.com") && k.contains("/page?x=1") && k.contains("https"));
        assert_ne!(Some(k), key("s", &Method::GET, &uri, &host, false));
        assert!(key("s", &Method::POST, &uri, &host, true).is_none());
        assert!(key("s", &Method::GET, &uri, &hdrs(&[("cookie", "a=b")]), true).is_none());
        assert!(
            key(
                "s",
                &Method::GET,
                &uri,
                &hdrs(&[("authorization", "Basic x")]),
                true
            )
            .is_none()
        );
        let live = hdrs(&[("host", "example.com"), ("nova-target", "#list")]);
        assert_ne!(
            key("s", &Method::GET, &uri, &live, true),
            key(
                "s",
                &Method::GET,
                &uri,
                &hdrs(&[("host", "example.com")]),
                true
            )
        );
    }

    #[test]
    fn only_shared_responses_are_cacheable() {
        let ok = StatusCode::OK;
        assert!(cacheable(ok, &hdrs(&[("content-type", "text/html")])));
        assert!(cacheable(ok, &hdrs(&[("vary", "Accept-Encoding")])));
        assert!(!cacheable(StatusCode::NOT_FOUND, &HeaderMap::new()));
        assert!(!cacheable(ok, &hdrs(&[("set-cookie", "s=1")])));
        assert!(!cacheable(
            ok,
            &hdrs(&[("cache-control", "public, no-cache")])
        ));
        assert!(!cacheable(
            ok,
            &hdrs(&[("cache-control", "private, max-age=60")])
        ));
        assert!(!cacheable(ok, &hdrs(&[("cache-control", "no-store")])));
        assert!(!cacheable(ok, &hdrs(&[("vary", "Accept-Language")])));
        assert!(!cacheable(
            ok,
            &hdrs(&[("content-type", "text/event-stream")])
        ));
    }

    #[test]
    fn expiry_and_purge() {
        let c = MicroCache::default();
        let body = Bytes::from_static(b"hi");
        c.put(
            "a\n1".into(),
            StatusCode::OK,
            HeaderMap::new(),
            body.clone(),
            Duration::from_secs(60),
        );
        c.put(
            "b\n1".into(),
            StatusCode::OK,
            HeaderMap::new(),
            body.clone(),
            Duration::from_secs(60),
        );
        c.put(
            "a\n2".into(),
            StatusCode::OK,
            HeaderMap::new(),
            body.clone(),
            Duration::ZERO,
        );
        assert!(c.get("a\n1").is_some());
        assert!(c.get("a\n2").is_none(), "expired");
        c.purge_site("a");
        assert!(c.get("a\n1").is_none());
        assert!(c.get("b\n1").is_some(), "other sites untouched");
        let big = Bytes::from(vec![0u8; MAX_ENTRY + 1]);
        c.put(
            "b\n2".into(),
            StatusCode::OK,
            HeaderMap::new(),
            big,
            Duration::from_secs(60),
        );
        assert!(c.get("b\n2").is_none(), "too large");
    }
}
