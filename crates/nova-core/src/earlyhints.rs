//! Automatic `103 Early Hints` (`[[site]] early_hints = true`).
//!
//! While PHP is still building a page, the browser could already be
//! fetching the page's stylesheets and scripts, but it only learns about
//! them from the HTML. NOVA learns them from the `<head>` of earlier
//! responses for the same path and, on the next navigation, sends them as
//! `103 Early Hints` (`Link: </app.css>; rel=preload; as=style`) before it
//! hands the request to PHP: the browser's downloads then overlap with the
//! server's think time (Chrome, Edge and Firefox act on 103 over HTTP/2 and
//! HTTP/3). Third-party origins become `rel=preconnect`.
//!
//! What is learned is refreshed every [`RELEARN`]; a path whose page lists
//! nothing to preload sends nothing.

use http::{HeaderMap, HeaderValue};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Relearn a path's hints after this long.
pub const RELEARN: Duration = Duration::from_secs(60);
/// Links per page (browsers start these at once).
const MAX_LINKS: usize = 8;
/// Paths remembered (all sites); the map is cleared when full.
const MAX_PATHS: usize = 10_000;
/// Bytes of a response scanned for `</head>`.
pub const SCAN_LIMIT: usize = 64 * 1024;

#[derive(Default)]
pub struct EarlyHints {
    paths: Mutex<HashMap<(String, String), Learned>>,
}

struct Learned {
    links: Vec<HeaderValue>,
    at: Instant,
}

impl EarlyHints {
    /// The `Link` headers to send for `path`, if any were learned.
    pub fn hints(&self, site: &str, path: &str) -> Option<HeaderMap> {
        let paths = self.paths.lock().unwrap();
        let learned = paths.get(&(site.to_string(), path.to_string()))?;
        if learned.links.is_empty() {
            return None;
        }
        let mut h = HeaderMap::new();
        for l in &learned.links {
            h.append(http::header::LINK, l.clone());
        }
        Some(h)
    }

    /// Whether the next response for `path` should be scanned.
    pub fn wants(&self, site: &str, path: &str) -> bool {
        let paths = self.paths.lock().unwrap();
        paths
            .get(&(site.to_string(), path.to_string()))
            .is_none_or(|l| l.at.elapsed() >= RELEARN)
    }

    /// Learn from the start of the page served at `host` + `path`.
    pub fn learn(&self, site: &str, host: &str, path: &str, head: &[u8]) {
        let links = scan(head, host, path)
            .into_iter()
            .filter_map(|l| HeaderValue::from_str(&l).ok())
            .collect();
        let mut paths = self.paths.lock().unwrap();
        if paths.len() >= MAX_PATHS {
            paths.clear();
        }
        paths.insert(
            (site.to_string(), path.to_string()),
            Learned {
                links,
                at: Instant::now(),
            },
        );
    }
}

/// `Link` header values for the resources the document's `<head>` loads.
/// Absolute URLs on `host` (the page's own, port ignored) count as
/// same-origin: WordPress writes all of its asset URLs that way.
pub fn scan(html: &[u8], host: &str, page_path: &str) -> Vec<String> {
    let host = host.rsplit_once(':').map_or(host, |(h, p)| {
        if p.bytes().all(|b| b.is_ascii_digit()) {
            h
        } else {
            host
        }
    });
    let html = &html[..html.len().min(SCAN_LIMIT)];
    let end = find_ci(html, b"</head").unwrap_or(html.len());
    let html = &html[..end];
    let mut out: Vec<String> = Vec::new();
    let mut origins: Vec<String> = Vec::new();
    let mut i = 0;
    while let Some(lt) = memchr::memchr(b'<', &html[i..]) {
        let start = i + lt + 1;
        let Some(gt) = memchr::memchr(b'>', &html[start..]) else {
            break;
        };
        let tag = &html[start..start + gt];
        i = start + gt + 1;
        if tag.starts_with(b"!--") {
            // Skip comments (they may contain markup).
            if let Some(e) = find_ci(&html[start..], b"-->") {
                i = start + e + 3;
            }
            continue;
        }
        let name_end = tag
            .iter()
            .position(|b| b.is_ascii_whitespace() || *b == b'/')
            .unwrap_or(tag.len());
        let name = String::from_utf8_lossy(&tag[..name_end]).to_ascii_lowercase();
        let attrs = attributes(&tag[name_end..]);
        // Script and style contents are not markup.
        if name == "script" || name == "style" {
            let close = if name == "script" {
                &b"</script"[..]
            } else {
                &b"</style"[..]
            };
            match find_ci(&html[i..], close) {
                Some(e) => i += e,
                None => i = html.len(),
            }
        }
        let get = |k: &str| attrs.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
        let (url, rel, as_) = match name.as_str() {
            "link" => {
                let rel = get("rel").unwrap_or("").to_ascii_lowercase();
                let Some(href) = get("href") else { continue };
                if rel.split_whitespace().any(|r| r == "stylesheet") {
                    if get("media").is_some_and(|m| m.eq_ignore_ascii_case("print")) {
                        continue;
                    }
                    (href, "preload", Some("style".to_string()))
                } else if rel.split_whitespace().any(|r| r == "preload") {
                    (href, "preload", get("as").map(str::to_string))
                } else if rel.split_whitespace().any(|r| r == "modulepreload") {
                    (href, "modulepreload", None)
                } else {
                    continue;
                }
            }
            "script" => {
                let Some(src) = get("src") else { continue };
                if get("type").is_some_and(|t| t.eq_ignore_ascii_case("module")) {
                    (src, "modulepreload", None)
                } else {
                    (src, "preload", Some("script".to_string()))
                }
            }
            _ => continue,
        };
        let url = url.replace("&amp;", "&");
        if url.is_empty() || url.starts_with("data:") || url.contains(['<', '>', '"', '\n', '\r']) {
            continue;
        }
        let url = same_host_path(&url, host).unwrap_or(url);
        if let Some(origin) = cross_origin(&url) {
            let link = format!("<{origin}>; rel=preconnect");
            if !origins.contains(&link) {
                origins.push(link);
            }
            continue;
        }
        let path = resolve(page_path, &url);
        let mut link = format!("<{path}>; rel={rel}");
        if let Some(a) = as_ {
            link.push_str("; as=");
            link.push_str(&a);
        }
        if get("crossorigin").is_some() || (rel == "preload" && get("as") == Some("font")) {
            link.push_str("; crossorigin");
        }
        if !out.contains(&link) {
            out.push(link);
        }
    }
    out.truncate(MAX_LINKS);
    origins.truncate(MAX_LINKS.saturating_sub(out.len()).min(3));
    out.extend(origins);
    out
}

/// `https://site.example/x.css?v=1` -> `/x.css?v=1` when on `host`.
fn same_host_path(url: &str, host: &str) -> Option<String> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .or_else(|| url.strip_prefix("//"))?;
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let url_host = rest[..end]
        .rsplit_once(':')
        .map_or(&rest[..end], |(h, _)| h);
    if !url_host.eq_ignore_ascii_case(host) || host.is_empty() {
        return None;
    }
    let path = &rest[end..];
    Some(if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    })
}

/// `https://cdn.example/x.css` -> `https://cdn.example`; `None` for
/// same-origin URLs (`/x`, `x`, `../x`).
fn cross_origin(url: &str) -> Option<String> {
    let (scheme, rest) = if let Some(r) = url.strip_prefix("//") {
        ("https", r)
    } else if let Some(r) = url.strip_prefix("https://") {
        ("https", r)
    } else {
        ("http", url.strip_prefix("http://")?)
    };
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    (!host.is_empty()).then(|| format!("{scheme}://{host}"))
}

/// Resolve a same-origin reference against the page's path.
fn resolve(page: &str, url: &str) -> String {
    if url.starts_with('/') {
        return url.to_string();
    }
    let dir = &page[..page.rfind('/').map_or(0, |i| i + 1)];
    let mut parts: Vec<&str> = dir.split('/').filter(|s| !s.is_empty()).collect();
    let (url_path, suffix) = match url.find(['?', '#']) {
        Some(i) => (&url[..i], &url[i..]),
        None => (url, ""),
    };
    for seg in url_path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    format!("/{}{suffix}", parts.join("/"))
}

fn attributes(s: &[u8]) -> Vec<(String, String)> {
    let s = String::from_utf8_lossy(s);
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        while i < b.len() && (b[i].is_ascii_whitespace() || b[i] == b'/') {
            i += 1;
        }
        let ns = i;
        while i < b.len() && !b[i].is_ascii_whitespace() && b[i] != b'=' && b[i] != b'/' {
            i += 1;
        }
        if ns == i {
            break;
        }
        let name = s[ns..i].to_ascii_lowercase();
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        let mut value = String::new();
        if i < b.len() && b[i] == b'=' {
            i += 1;
            while i < b.len() && b[i].is_ascii_whitespace() {
                i += 1;
            }
            if i < b.len() && (b[i] == b'"' || b[i] == b'\'') {
                let q = b[i];
                let vs = i + 1;
                i = vs;
                while i < b.len() && b[i] != q {
                    i += 1;
                }
                value = s[vs..i.min(b.len())].to_string();
                i += 1;
            } else {
                let vs = i;
                while i < b.len() && !b[i].is_ascii_whitespace() {
                    i += 1;
                }
                value = s[vs..i].to_string();
            }
        }
        out.push((name, value));
    }
    out
}

fn find_ci(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len())
        .position(|w| w.eq_ignore_ascii_case(needle))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn learns_the_head() {
        let html = br#"<!doctype html><html><head>
<meta charset="utf-8"><title>x</title>
<!-- <link rel="stylesheet" href="/commented.css"> -->
<link rel='stylesheet' id='wp-block' href='/wp-includes/css/style.min.css?ver=7.1&amp;x=1' media='all' />
<link rel="stylesheet" href="print.css" media="print">
<link rel="preload" href="/fonts/inter.woff2" as="font" type="font/woff2" crossorigin>
<link rel="stylesheet" href="https://fonts.googleapis.com/css2?family=Inter">
<link rel="stylesheet" href="https://shop.example:8443/wp-content/themes/t/style.css?ver=2">
<script src="../js/app.js?v=3" defer></script>
<script type="module" src="/build/assets/app-Bx7.js"></script>
<script>var inline = "<script src='/no.js'>";</script>
<link rel="icon" href="/favicon.ico">
</head><body><script src="/body.js"></script></body></html>"#;
        assert_eq!(
            scan(html, "shop.example", "/blog/post"),
            vec![
                "</wp-includes/css/style.min.css?ver=7.1&x=1>; rel=preload; as=style",
                "</fonts/inter.woff2>; rel=preload; as=font; crossorigin",
                "</wp-content/themes/t/style.css?ver=2>; rel=preload; as=style",
                "</js/app.js?v=3>; rel=preload; as=script",
                "</build/assets/app-Bx7.js>; rel=modulepreload",
                "<https://fonts.googleapis.com>; rel=preconnect",
            ]
        );
    }

    #[test]
    fn cache_relearns() {
        let h = EarlyHints::default();
        assert!(h.wants("s", "/"));
        h.learn(
            "s",
            "h",
            "/",
            b"<head><link rel=stylesheet href=/a.css></head>",
        );
        assert!(!h.wants("s", "/"));
        assert_eq!(
            h.hints("s", "/").unwrap()["link"],
            "</a.css>; rel=preload; as=style"
        );
        h.learn("s", "h", "/empty", b"<head></head>");
        assert!(h.hints("s", "/empty").is_none());
    }
}
