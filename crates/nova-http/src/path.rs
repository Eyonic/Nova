//! Turns a request path into a safe relative filesystem path.
//!
//! Lexical checks happen here; the dispatcher additionally canonicalizes the
//! final path and verifies it is still below the document root, which
//! catches symlinks pointing outside.

use percent_encoding::percent_decode_str;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathError {
    /// Malformed or hostile (`..`, NUL, invalid UTF-8): answer 400.
    BadRequest,
    /// Hidden path (dotfile): answer 404 so its existence is not revealed.
    Hidden,
}

/// A normalized request path: clean segments plus whether it ended in `/`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SafePath {
    pub segments: Vec<String>,
    pub trailing_slash: bool,
}

impl SafePath {
    /// `/`-joined relative path, e.g. `images/a.jpg`.
    pub fn rel(&self) -> String {
        self.segments.join("/")
    }

    pub fn to_path(&self) -> std::path::PathBuf {
        self.segments.iter().collect()
    }

    /// URL form with a leading slash, e.g. `/images/a.jpg`.
    pub fn url(&self) -> String {
        format!("/{}", self.rel())
    }
}

pub fn resolve(raw_path: &str) -> Result<SafePath, PathError> {
    if !raw_path.starts_with('/') {
        return Err(PathError::BadRequest);
    }
    let decoded = percent_decode_str(raw_path)
        .decode_utf8()
        .map_err(|_| PathError::BadRequest)?;
    if decoded.contains(['\0', '\\']) {
        return Err(PathError::BadRequest);
    }
    let mut segments = Vec::new();
    for seg in decoded.split('/') {
        match seg {
            "" | "." => {}
            ".." => return Err(PathError::BadRequest),
            s if s.starts_with('.') && s != ".well-known" => return Err(PathError::Hidden),
            s => segments.push(s.to_string()),
        }
    }
    Ok(SafePath {
        segments,
        trailing_slash: decoded.ends_with('/') && decoded.len() > 1,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes() {
        let p = resolve("/a//b/./c%20d.txt").unwrap();
        assert_eq!(p.rel(), "a/b/c d.txt");
        assert!(!p.trailing_slash);
        assert!(resolve("/docs/").unwrap().trailing_slash);
        assert_eq!(resolve("/").unwrap().segments.len(), 0);
    }

    #[test]
    fn rejects_traversal_and_hidden() {
        assert_eq!(resolve("/../etc/passwd"), Err(PathError::BadRequest));
        assert_eq!(
            resolve("/a/%2e%2e/%2e%2e/etc/passwd"),
            Err(PathError::BadRequest)
        );
        assert_eq!(resolve("/a%00.php"), Err(PathError::BadRequest));
        assert_eq!(resolve("/a%5c..%5cb"), Err(PathError::BadRequest));
        assert_eq!(resolve("/%ff"), Err(PathError::BadRequest));
        assert_eq!(resolve("relative"), Err(PathError::BadRequest));
        assert_eq!(resolve("/.env"), Err(PathError::Hidden));
        assert_eq!(resolve("/.git/config"), Err(PathError::Hidden));
        assert!(resolve("/.well-known/acme-challenge/x").is_ok());
    }
}
