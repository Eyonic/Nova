//! Builds the CGI/1.1 environment (RFC 3875) PHP expects for a request.

use http::HeaderMap;
use std::net::SocketAddr;
use std::path::Path;

/// Request facts the HTTP layer resolved before handing off to PHP.
pub struct CgiRequest<'a> {
    pub method: &'a str,
    /// Original request target path, still percent-encoded.
    pub request_uri: &'a str,
    pub query: &'a str,
    /// URL path of the script, e.g. `/index.php`.
    pub script_name: &'a str,
    /// Absolute filesystem path of the script.
    pub script_filename: &'a Path,
    pub path_info: &'a str,
    pub document_root: &'a Path,
    pub server_name: &'a str,
    pub server_port: u16,
    pub server_protocol: &'a str,
    pub remote_addr: SocketAddr,
    pub https: bool,
    pub headers: &'a HeaderMap,
    pub content_length: Option<u64>,
    pub request_id: &'a str,
}

pub fn build_params(r: &CgiRequest<'_>) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut p: Vec<(Vec<u8>, Vec<u8>)> = Vec::with_capacity(32 + r.headers.len());
    let mut put = |k: &str, v: &[u8]| p.push((k.as_bytes().to_vec(), v.to_vec()));
    let path = |x: &Path| x.as_os_str().as_encoded_bytes().to_vec();

    put("GATEWAY_INTERFACE", b"CGI/1.1");
    put(
        "SERVER_SOFTWARE",
        concat!("NOVA/", env!("CARGO_PKG_VERSION")).as_bytes(),
    );
    put("REQUEST_METHOD", r.method.as_bytes());
    put("REQUEST_URI", r.request_uri.as_bytes());
    put("QUERY_STRING", r.query.as_bytes());
    put("SCRIPT_NAME", r.script_name.as_bytes());
    put(
        "PHP_SELF",
        format!("{}{}", r.script_name, r.path_info).as_bytes(),
    );
    put("SCRIPT_FILENAME", &path(r.script_filename));
    put("DOCUMENT_ROOT", &path(r.document_root));
    put("DOCUMENT_URI", r.script_name.as_bytes());
    if !r.path_info.is_empty() {
        put("PATH_INFO", r.path_info.as_bytes());
        let mut translated = path(r.document_root);
        translated.extend_from_slice(r.path_info.as_bytes());
        put("PATH_TRANSLATED", &translated);
    }
    put("SERVER_NAME", r.server_name.as_bytes());
    put("SERVER_PORT", r.server_port.to_string().as_bytes());
    put("SERVER_PROTOCOL", r.server_protocol.as_bytes());
    put("REMOTE_ADDR", r.remote_addr.ip().to_string().as_bytes());
    put("REMOTE_PORT", r.remote_addr.port().to_string().as_bytes());
    // Required by php-cgi builds compiled with force-cgi-redirect.
    put("REDIRECT_STATUS", b"200");
    put("NOVA_REQUEST_ID", r.request_id.as_bytes());
    if r.https {
        put("HTTPS", b"on");
        put("REQUEST_SCHEME", b"https");
    } else {
        put("REQUEST_SCHEME", b"http");
    }
    if let Some(len) = r.content_length {
        put("CONTENT_LENGTH", len.to_string().as_bytes());
    }
    if let Some(ct) = r.headers.get(http::header::CONTENT_TYPE) {
        put("CONTENT_TYPE", ct.as_bytes());
    }

    // HTTP_* variables. Repeated headers are joined as one value.
    let mut joined: Vec<(String, Vec<u8>)> = Vec::new();
    for (name, value) in r.headers {
        let n = name.as_str();
        // httpoxy (CVE-2016-5385): never forward a client "Proxy" header as HTTP_PROXY.
        if n == "proxy" || n == "content-type" || n == "content-length" {
            continue;
        }
        let key = format!("HTTP_{}", n.to_ascii_uppercase().replace('-', "_"));
        match joined.iter_mut().find(|(k, _)| *k == key) {
            Some((_, v)) => {
                v.extend_from_slice(if n == "cookie" { b"; " } else { b", " });
                v.extend_from_slice(value.as_bytes());
            }
            None => joined.push((key, value.as_bytes().to_vec())),
        }
    }
    for (k, v) in joined {
        p.push((k.into_bytes(), v));
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get<'a>(p: &'a [(Vec<u8>, Vec<u8>)], k: &str) -> Option<&'a str> {
        p.iter()
            .find(|(n, _)| n == k.as_bytes())
            .map(|(_, v)| std::str::from_utf8(v).unwrap())
    }

    #[test]
    fn builds_cgi_environment() {
        let mut headers = HeaderMap::new();
        headers.insert("host", "example.test".parse().unwrap());
        headers.insert("proxy", "evil:1234".parse().unwrap());
        headers.append("cookie", "a=1".parse().unwrap());
        headers.append("cookie", "b=2".parse().unwrap());
        headers.insert("content-type", "application/json".parse().unwrap());
        let r = CgiRequest {
            method: "POST",
            request_uri: "/index.php/foo?x=1",
            query: "x=1",
            script_name: "/index.php",
            script_filename: Path::new("/srv/site/public/index.php"),
            path_info: "/foo",
            document_root: Path::new("/srv/site/public"),
            server_name: "example.test",
            server_port: 8080,
            server_protocol: "HTTP/1.1",
            remote_addr: "10.0.0.2:5555".parse().unwrap(),
            https: false,
            headers: &headers,
            content_length: Some(12),
            request_id: "abc",
        };
        let p = build_params(&r);
        assert_eq!(
            get(&p, "SCRIPT_FILENAME"),
            Some("/srv/site/public/index.php")
        );
        assert_eq!(get(&p, "PATH_INFO"), Some("/foo"));
        assert_eq!(get(&p, "PHP_SELF"), Some("/index.php/foo"));
        assert_eq!(get(&p, "CONTENT_TYPE"), Some("application/json"));
        assert_eq!(get(&p, "CONTENT_LENGTH"), Some("12"));
        assert_eq!(get(&p, "HTTP_HOST"), Some("example.test"));
        assert_eq!(get(&p, "HTTP_COOKIE"), Some("a=1; b=2"));
        assert_eq!(get(&p, "HTTP_PROXY"), None);
        assert_eq!(get(&p, "HTTP_CONTENT_TYPE"), None);
    }
}
