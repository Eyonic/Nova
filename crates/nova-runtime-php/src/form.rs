//! Request body parsing for worker mode, the part PHP-FPM gets from the
//! SAPI: `application/x-www-form-urlencoded` and `multipart/form-data`
//! become the RoadRunner `parsed` body (a JSON tree like `$_POST`) and
//! `uploads` tree (one `{name, mime, size, error, tmpName}` per file).
//!
//! Field names follow PHP's rules (`a[b][]=1`, dots and spaces in the top
//! level name become `_`, `max_input_vars`, `max_input_nesting_level`,
//! `max_file_uploads`), so an application sees the same input as under
//! FPM. Runs in the site's worker manager, inside the site's sandbox.

use std::fmt::Write as _;
use std::io;
use std::path::{Path, PathBuf};

/// How NOVA ends every request header; the manager replaces it once the
/// body is parsed.
pub const UNPARSED_TAIL: &str = r#","parsed":false,"uploads":{}}"#;

/// Bodies the manager parses: multipart always (files must land in the
/// site's sandbox), urlencoded only for workers that expect it parsed.
pub fn needs_parsing(content_type: &str, urlencoded: bool) -> bool {
    let mime = content_type.split(';').next().unwrap_or("").trim();
    mime.eq_ignore_ascii_case("multipart/form-data")
        || (urlencoded && mime.eq_ignore_ascii_case("application/x-www-form-urlencoded"))
}

/// Turn a flagged request (JSON header + raw form body) into RoadRunner's
/// parsed form: header with `parsed`/`uploads`, body = the field tree.
/// Keep the returned value until the response is complete: dropping it
/// deletes the uploaded files.
pub fn parse_request(
    header: &[u8],
    body: &[u8],
    tmp_dir: &Path,
) -> io::Result<(Vec<u8>, Vec<u8>, Option<Multipart>)> {
    #[derive(serde::Deserialize)]
    struct Ctx {
        headers: std::collections::HashMap<String, Vec<String>>,
    }
    let bad = |m: &str| io::Error::new(io::ErrorKind::InvalidData, m.to_string());
    let base = header
        .strip_suffix(UNPARSED_TAIL.as_bytes())
        .ok_or_else(|| bad("request header without NOVA's tail"))?;
    let ctx: Ctx = serde_json::from_slice(header).map_err(|e| bad(&e.to_string()))?;
    let ct = ctx
        .headers
        .get("Content-Type")
        .and_then(|v| v.first())
        .map(String::as_str)
        .unwrap_or("");
    let mut out = base.to_vec();
    out.extend_from_slice(br#","parsed":true,"uploads":"#);
    let (fields, multipart) = match boundary(ct) {
        Some(b) => {
            let m = parse_multipart(body, &b, tmp_dir)?;
            out.extend_from_slice(m.uploads.to_json().as_bytes());
            (m.fields.to_json(), Some(m))
        }
        None => {
            out.extend_from_slice(b"{}");
            (parse_urlencoded(body).to_json(), None)
        }
    };
    out.push(b'}');
    Ok((out, fields.into_bytes(), multipart))
}

/// PHP's defaults.
const MAX_INPUT_VARS: usize = 1000;
const MAX_NESTING: usize = 64;
const MAX_FILE_UPLOADS: usize = 20;
const UPLOAD_ERR_NO_FILE: u32 = 4;

/// An ordered PHP array (string keys; PHP turns numeric ones into ints).
#[derive(Debug, Default)]
pub struct Tree(Vec<(String, Node)>);

#[derive(Debug)]
enum Node {
    Leaf(String),
    Tree(Tree),
}

impl Tree {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Insert `value` at the PHP variable `name`, e.g. `user[roles][]`.
    fn insert(&mut self, name: &str, value: Node) {
        let Some(path) = php_name_path(name) else {
            return;
        };
        let mut node = self;
        let last = path.len() - 1;
        for (i, seg) in path.into_iter().enumerate() {
            let key = match seg {
                Some(k) => k,
                None => node.next_index().to_string(),
            };
            if i == last {
                match node.0.iter_mut().find(|(k, _)| *k == key) {
                    Some(slot) => slot.1 = value,
                    None => node.0.push((key, value)),
                }
                return;
            }
            let pos = match node.0.iter().position(|(k, _)| *k == key) {
                Some(p) => {
                    if matches!(node.0[p].1, Node::Leaf(_)) {
                        node.0[p].1 = Node::Tree(Tree::default());
                    }
                    p
                }
                None => {
                    node.0.push((key, Node::Tree(Tree::default())));
                    node.0.len() - 1
                }
            };
            node = match &mut node.0[pos].1 {
                Node::Tree(t) => t,
                Node::Leaf(_) => unreachable!(),
            };
        }
    }

    /// PHP's next free integer key.
    fn next_index(&self) -> i64 {
        self.0
            .iter()
            .filter_map(|(k, _)| k.parse::<i64>().ok())
            .max()
            .map_or(0, |m| m + 1)
    }

    pub fn to_json(&self) -> String {
        let mut s = String::new();
        self.write_json(&mut s);
        s
    }

    fn write_json(&self, s: &mut String) {
        s.push('{');
        for (i, (k, v)) in self.0.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            json_str(s, k);
            s.push(':');
            match v {
                Node::Leaf(raw) => s.push_str(raw),
                Node::Tree(t) => t.write_json(s),
            }
        }
        s.push('}');
    }

    fn leaf_str(v: &str) -> Node {
        let mut s = String::with_capacity(v.len() + 2);
        json_str(&mut s, v);
        Node::Leaf(s)
    }
}

pub(crate) fn json_str(s: &mut String, v: &str) {
    s.reserve(v.len() + 2);
    s.push('"');
    // Copy runs that need no escaping in one go (the common case: all of it).
    let bytes = v.as_bytes();
    let mut start = 0;
    for (i, &b) in bytes.iter().enumerate() {
        let esc = match b {
            b'"' => "\\\"",
            b'\\' => "\\\\",
            b'\n' => "\\n",
            b'\r' => "\\r",
            b'\t' => "\\t",
            b if b < 0x20 => "",
            _ => continue,
        };
        s.push_str(&v[start..i]);
        if esc.is_empty() {
            let _ = write!(s, "\\u{:04x}", b);
        } else {
            s.push_str(esc);
        }
        start = i + 1;
    }
    s.push_str(&v[start..]);
    s.push('"');
}

/// `json_str` for bytes that are usually UTF-8 (no copy when they are).
pub(crate) fn json_bytes(s: &mut String, v: &[u8]) {
    match std::str::from_utf8(v) {
        Ok(t) => json_str(s, t),
        Err(_) => json_str(s, &String::from_utf8_lossy(v)),
    }
}

/// PHP's `php_register_variable_ex` key parsing: `a.b[x][]` ->
/// `[Some("a_b"), Some("x"), None]` (`None` = append). `None` for names
/// PHP drops (empty, or nested too deeply).
fn php_name_path(name: &str) -> Option<Vec<Option<String>>> {
    let name = name.trim_start_matches(' ');
    let (base, mut rest) = match name.find('[') {
        Some(i) => (&name[..i], &name[i..]),
        None => (name, ""),
    };
    if base.is_empty() {
        return None;
    }
    let mut first: String = base
        .chars()
        .map(|c| if c == ' ' || c == '.' { '_' } else { c })
        .collect();
    let mut path = Vec::new();
    while let Some(r) = rest.strip_prefix('[') {
        match r.find(']') {
            Some(end) => {
                let idx = &r[..end];
                path.push((!idx.is_empty()).then(|| idx.to_string()));
                rest = &r[end + 1..];
                // PHP ignores anything between `]` and the next `[`.
                match rest.find('[') {
                    Some(i) if i > 0 => rest = &rest[i..],
                    Some(_) => {}
                    None => break,
                }
            }
            None => {
                // Unclosed bracket: PHP keeps it literally (as `_` if first).
                if path.is_empty() {
                    first.push('_');
                    first.push_str(r);
                }
                break;
            }
        }
        if path.len() >= MAX_NESTING {
            return None;
        }
    }
    path.insert(0, Some(first));
    Some(path)
}

/// `application/x-www-form-urlencoded` -> parsed tree.
pub fn parse_urlencoded(body: &[u8]) -> Tree {
    let mut tree = Tree::default();
    for (n, pair) in body
        .split(|&b| b == b'&')
        .filter(|p| !p.is_empty())
        .enumerate()
    {
        if n >= MAX_INPUT_VARS {
            break;
        }
        let (k, v) = match pair.iter().position(|&b| b == b'=') {
            Some(i) => (&pair[..i], &pair[i + 1..]),
            None => (pair, &b""[..]),
        };
        tree.insert(&url_decode(k), Tree::leaf_str(&url_decode(v)));
    }
    tree
}

pub(crate) fn url_decode(s: &[u8]) -> String {
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        match s[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < s.len() => match (hex(s[i + 1]), hex(s[i + 2])) {
                (Some(h), Some(l)) => {
                    out.push(h << 4 | l);
                    i += 2;
                }
                _ => out.push(b'%'),
            },
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// `boundary` parameter of a `multipart/form-data` content type.
pub fn boundary(content_type: &str) -> Option<String> {
    let (mime, params) = content_type.split_once(';')?;
    if !mime.trim().eq_ignore_ascii_case("multipart/form-data") {
        return None;
    }
    params.split(';').find_map(|p| {
        let (k, v) = p.split_once('=')?;
        k.trim()
            .eq_ignore_ascii_case("boundary")
            .then(|| v.trim().trim_matches('"').to_string())
            .filter(|b| !b.is_empty() && b.len() <= 200)
    })
}

/// Parsed multipart body: form fields, uploaded files (already written to
/// `tmp_dir`) and the temporary files to delete after the request.
#[derive(Debug, Default)]
pub struct Multipart {
    pub fields: Tree,
    pub uploads: Tree,
    pub files: Vec<PathBuf>,
}

impl Drop for Multipart {
    fn drop(&mut self) {
        for f in &self.files {
            let _ = std::fs::remove_file(f);
        }
    }
}

/// Parse `multipart/form-data`, writing files to `tmp_dir`.
pub fn parse_multipart(body: &[u8], boundary: &str, tmp_dir: &Path) -> io::Result<Multipart> {
    let delim = format!("--{boundary}");
    let finder = memchr::memmem::Finder::new(delim.as_bytes());
    let mut out = Multipart::default();
    let mut vars = 0;
    let mut starts = finder.find_iter(body).peekable();
    while let Some(start) = starts.next() {
        let Some(&next) = starts.peek() else { break };
        let mut part = &body[start + delim.len()..next];
        if part.starts_with(b"--") {
            break;
        }
        part = part.strip_prefix(b"\r\n").unwrap_or(part);
        // The CRLF before the next delimiter belongs to the delimiter.
        part = part.strip_suffix(b"\r\n").unwrap_or(part);
        let Some(head_end) = memchr::memmem::find(part, b"\r\n\r\n") else {
            continue;
        };
        let head = String::from_utf8_lossy(&part[..head_end]);
        let content = &part[head_end + 4..];
        let mut name = None;
        let mut filename = None;
        let mut mime = String::from("application/octet-stream");
        for line in head.split("\r\n") {
            let Some((k, v)) = line.split_once(':') else {
                continue;
            };
            if k.trim().eq_ignore_ascii_case("content-disposition") {
                name = disposition_param(v, "name");
                filename = disposition_param(v, "filename");
            } else if k.trim().eq_ignore_ascii_case("content-type") {
                mime = v.trim().to_string();
            }
        }
        let Some(name) = name else { continue };
        vars += 1;
        if vars > MAX_INPUT_VARS {
            break;
        }
        let Some(filename) = filename else {
            let value = String::from_utf8_lossy(content);
            out.fields.insert(&name, Tree::leaf_str(&value));
            continue;
        };
        // PHP keeps only the last path component of the client's name.
        let filename = filename
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or_default()
            .to_string();
        let mut info = Tree::default();
        info.0.push(("name".into(), Tree::leaf_str(&filename)));
        info.0.push(("mime".into(), Tree::leaf_str(&mime)));
        if filename.is_empty() && content.is_empty() {
            info.0.push(("size".into(), Node::Leaf("0".into())));
            info.0
                .push(("error".into(), Node::Leaf(UPLOAD_ERR_NO_FILE.to_string())));
            info.0.push(("tmpName".into(), Tree::leaf_str("")));
        } else {
            if out.files.len() >= MAX_FILE_UPLOADS {
                continue;
            }
            let path = write_temp(tmp_dir, content)?;
            info.0
                .push(("size".into(), Node::Leaf(content.len().to_string())));
            info.0.push(("error".into(), Node::Leaf("0".into())));
            info.0
                .push(("tmpName".into(), Tree::leaf_str(&path.to_string_lossy())));
            out.files.push(path);
        }
        out.uploads.insert(&name, Node::Tree(info));
    }
    Ok(out)
}

fn disposition_param(v: &str, key: &str) -> Option<String> {
    // Parameters are `; key="value"` (quoted strings may contain `;`).
    let mut rest = v;
    while let Some(i) = rest.find(';') {
        rest = rest[i + 1..].trim_start();
        let (k, after) = rest.split_once('=')?;
        let (value, tail) = if let Some(q) = after.strip_prefix('"') {
            let mut val = String::new();
            let mut chars = q.char_indices();
            let mut end = q.len();
            while let Some((i, c)) = chars.next() {
                match c {
                    '\\' => {
                        if let Some((_, n)) = chars.next() {
                            val.push(n);
                        }
                    }
                    '"' => {
                        end = i + 1;
                        break;
                    }
                    c => val.push(c),
                }
            }
            (val, &q[end.min(q.len())..])
        } else {
            let end = after.find(';').unwrap_or(after.len());
            (after[..end].trim().to_string(), &after[end..])
        };
        if k.trim().eq_ignore_ascii_case(key) {
            return Some(value);
        }
        rest = tail;
    }
    None
}

fn write_temp(dir: &Path, content: &[u8]) -> io::Result<PathBuf> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut seed = [0u8; 8];
    for _ in 0..16 {
        getrandom(&mut seed);
        let path = dir.join(format!("php{:016x}", u64::from_ne_bytes(seed)));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(mut f) => {
                f.write_all(content)?;
                return Ok(path);
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::other("could not create an upload file"))
}

fn getrandom(buf: &mut [u8]) {
    // SAFETY: valid buffer; getrandom(2) fills at most buf.len() bytes.
    unsafe { libc::getrandom(buf.as_mut_ptr().cast(), buf.len(), 0) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn php_names() {
        let t = parse_urlencoded(
            b"a=1&b[]=x&b[]=y&c[k][]=z&d.e=2&f+g=3&h[=4&b[5]=q&b[]=r&u=%C3%A9%2B%zz&a=last",
        );
        assert_eq!(
            t.to_json(),
            r#"{"a":"last","b":{"0":"x","1":"y","5":"q","6":"r"},"c":{"k":{"0":"z"}},"d_e":"2","f_g":"3","h_":"4","u":"é+%zz"}"#
        );
        assert!(parse_urlencoded(b"=x&[a]=y").is_empty());
    }

    #[test]
    fn multipart_files_and_fields() {
        let dir = std::env::temp_dir().join(format!("nova-form-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let body = b"--XyZ\r\nContent-Disposition: form-data; name=\"title\"\r\n\r\nHello; \"world\"\r\n\
--XyZ\r\nContent-Disposition: form-data; name=\"docs[]\"; filename=\"C:\\\\dir\\\\a.txt\"\r\nContent-Type: text/plain\r\n\r\nfile one\r\n\
--XyZ\r\nContent-Disposition: form-data; name=\"docs[]\"; filename=\"b;c.bin\"\r\n\r\n\x00\x01\r\n\
--XyZ\r\nContent-Disposition: form-data; name=\"empty\"; filename=\"\"\r\n\r\n\r\n--XyZ--\r\n";
        let ct = "multipart/form-data; boundary=\"XyZ\"";
        let b = boundary(ct).unwrap();
        let m = parse_multipart(body, &b, &dir).unwrap();
        assert_eq!(m.fields.to_json(), r#"{"title":"Hello; \"world\""}"#);
        assert_eq!(m.files.len(), 2);
        assert_eq!(std::fs::read(&m.files[0]).unwrap(), b"file one");
        assert_eq!(std::fs::read(&m.files[1]).unwrap(), b"\x00\x01");
        let up = m.uploads.to_json();
        assert!(
            up.starts_with(
                r#"{"docs":{"0":{"name":"a.txt","mime":"text/plain","size":8,"error":0,"tmpName":""#
            ),
            "{up}"
        );
        assert!(
            up.contains(r#""1":{"name":"b;c.bin","mime":"application/octet-stream","size":2"#),
            "{up}"
        );
        assert!(up.ends_with(r#""empty":{"name":"","mime":"application/octet-stream","size":0,"error":4,"tmpName":""}}"#), "{up}");
        let files = m.files.clone();
        drop(m);
        assert!(
            files.iter().all(|f| !f.exists()),
            "deleted after the request"
        );
        assert_eq!(boundary("text/plain; boundary=x"), None);
    }
}
