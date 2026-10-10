//! Compression Dictionary Transport (RFC 9842) for fingerprinted assets.
//!
//! When a site deploys `app-NEW.js`, a returning visitor still has
//! `app-OLD.js` in its cache. NOVA marks every immutable JS/CSS response as
//! a dictionary for its own fingerprint pattern
//! (`Use-As-Dictionary: match="/build/assets/app-*.js"`). On the next
//! request for a newer version the browser announces the old one
//! (`Available-Dictionary: :<sha-256>:`) and NOVA answers with only the
//! difference: a zstd frame compressed against the old file
//! (`Content-Encoding: dcz`). Unknown hashes, other browsers and a restarted
//! NOVA simply get the normal encodings.
//!
//! Dictionaries are only offered for immutable responses: browsers use a
//! dictionary only while their cached copy is fresh.

use bytes::{BufMut, Bytes, BytesMut};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Mutex;

/// Memory for remembered versions and computed deltas.
const BUDGET: usize = 64 << 20;
/// Larger files are neither offered nor used as dictionaries.
pub const MAX_DICTIONARY: usize = 8 << 20;
/// `dcz` header: magic + SHA-256 of the dictionary (RFC 9842 §4).
const DCZ_MAGIC: [u8; 8] = [0x5e, 0x2a, 0x4d, 0x18, 0x20, 0x00, 0x00, 0x00];

pub type Hash = [u8; 32];

#[derive(Default)]
pub struct Dictionaries {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    /// Versions served to browsers: hash -> (match pattern, content).
    versions: HashMap<Hash, (String, Bytes)>,
    /// (dictionary, target) -> dcz body.
    deltas: HashMap<(Hash, Hash), Bytes>,
    bytes: usize,
}

pub fn sha256(data: &[u8]) -> Hash {
    Sha256::digest(data).into()
}

/// `Use-As-Dictionary` match pattern for a fingerprinted asset path:
/// `/build/assets/app-Bx7Kq2Lm.js` -> `/build/assets/app-*.js`. `None` when
/// the file name carries no fingerprint (then versions share no URL shape).
pub fn pattern_for(path: &str) -> Option<String> {
    let (dir, name) = path.rsplit_once('/')?;
    let (stem, ext) = name.rsplit_once('.')?;
    let (base, hash) = stem.rsplit_once(['-', '.', '_'])?;
    let fingerprint = hash.len() >= 6
        && hash.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        && hash
            .bytes()
            .any(|b| b.is_ascii_digit() || b.is_ascii_uppercase());
    let sep = &stem[base.len()..base.len() + 1];
    fingerprint.then(|| format!("{dir}/{base}{sep}*.{ext}"))
}

/// URL-pattern match for the patterns produced by [`pattern_for`].
fn matches(pattern: &str, path: &str) -> bool {
    match pattern.split_once('*') {
        Some((pre, post)) => {
            path.len() >= pre.len() + post.len() && path.starts_with(pre) && path.ends_with(post)
        }
        None => pattern == path,
    }
}

/// `Available-Dictionary: :<base64 sha-256>:` (an RFC 8941 byte sequence).
pub fn parse_available(value: &str) -> Option<Hash> {
    use base64::Engine;
    let b64 = value.trim().strip_prefix(':')?.strip_suffix(':')?;
    let raw = base64::engine::general_purpose::STANDARD.decode(b64).ok()?;
    raw.try_into().ok()
}

impl Dictionaries {
    /// Remember `content` (served at `path`) as a future dictionary.
    pub fn remember(&self, hash: Hash, pattern: &str, content: Bytes) {
        if content.len() > MAX_DICTIONARY {
            return;
        }
        let mut inner = self.inner.lock().unwrap();
        if inner.versions.contains_key(&hash) {
            return;
        }
        inner.bytes += content.len();
        inner.versions.insert(hash, (pattern.to_owned(), content));
        inner.evict();
    }

    /// The `dcz` body for `target` (served at `path`) against the version
    /// the browser has, if NOVA knows it and it may serve as a dictionary
    /// for this URL. Computed once, then cached.
    pub fn delta(
        &self,
        have: &Hash,
        path: &str,
        target_hash: Hash,
        target: &Bytes,
    ) -> Option<Bytes> {
        if have == &target_hash {
            return None;
        }
        let dict = {
            let inner = self.inner.lock().unwrap();
            if let Some(d) = inner.deltas.get(&(*have, target_hash)) {
                return Some(d.clone());
            }
            let (pattern, dict) = inner.versions.get(have)?;
            if !matches(pattern, path) {
                return None;
            }
            dict.clone()
        };
        let frame = compress_with(&dict, target)?;
        let mut body = BytesMut::with_capacity(40 + frame.len());
        body.put_slice(&DCZ_MAGIC);
        body.put_slice(have);
        body.put_slice(&frame);
        let body = body.freeze();
        // Only worth it when the delta beats plain compression clearly;
        // callers compare against their normal encoding, so store anyway.
        let mut inner = self.inner.lock().unwrap();
        inner.bytes += body.len();
        inner.deltas.insert((*have, target_hash), body.clone());
        inner.evict();
        Some(body)
    }
}

impl Inner {
    fn evict(&mut self) {
        if self.bytes <= BUDGET {
            return;
        }
        // Deltas are cheap to recompute; drop them first, then versions.
        self.deltas.clear();
        self.bytes = self.versions.values().map(|(_, c)| c.len()).sum();
        while self.bytes > BUDGET {
            let Some(k) = self.versions.keys().next().copied() else {
                break;
            };
            if let Some((_, c)) = self.versions.remove(&k) {
                self.bytes -= c.len();
            }
        }
    }
}

/// zstd frame of `target` using `dict` as raw content dictionary, with a
/// window large enough for both (RFC 9842 caps it at max(8 MB, 1.25x dict)).
fn compress_with(dict: &[u8], target: &[u8]) -> Option<Vec<u8>> {
    use zstd::zstd_safe::CParameter;
    // A dictionary without zstd's magic number is loaded as raw content.
    let mut c = zstd::bulk::Compressor::with_dictionary(19, dict).ok()?;
    let window = (dict.len() + target.len()).next_power_of_two().max(1 << 20);
    let window_log = window.trailing_zeros().min(23);
    c.set_parameter(CParameter::WindowLog(window_log)).ok()?;
    c.compress(target).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_patterns() {
        assert_eq!(
            pattern_for("/build/assets/app-Bx7Kq2Lm.js").as_deref(),
            Some("/build/assets/app-*.js")
        );
        assert_eq!(
            pattern_for("/static/main.8f3a2b1c.css").as_deref(),
            Some("/static/main.*.css")
        );
        assert_eq!(pattern_for("/js/cart.js"), None);
        assert_eq!(
            pattern_for("/js/vendor-bundle.js"),
            None,
            "a word, not a hash"
        );
        assert!(matches(
            "/build/assets/app-*.js",
            "/build/assets/app-NEW123.js"
        ));
        assert!(!matches(
            "/build/assets/app-*.js",
            "/build/assets/vendor-NEW123.js"
        ));
    }

    #[test]
    fn delta_roundtrip_and_header() {
        // ~60 KB of varied JS-like text (compresses like real code), and a
        // new version with one changed line: the usual deploy.
        let mut seed = 42u64;
        let mut next = || {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            seed >> 33
        };
        let mut code = String::new();
        for i in 0..1500 {
            code += &format!(
                "function f{i}(a{},b{}){{return a{}*{}+b{}-{};}}\n",
                next() % 97,
                next() % 89,
                next() % 97,
                next() % 1000,
                next() % 89,
                next() % 50
            );
        }
        let old = Bytes::from(code.clone() + "var version = 1;");
        let new = Bytes::from(code + "var version = 2; var added = true;");
        let d = Dictionaries::default();
        let (ho, hn) = (sha256(&old), sha256(&new));
        d.remember(ho, "/a-*.js", old.clone());
        assert!(
            d.delta(&ho, "/b-NEW123.js", hn, &new).is_none(),
            "pattern must match the URL"
        );
        let body = d.delta(&ho, "/a-NEW123.js", hn, &new).unwrap();
        assert_eq!(&body[..8], &DCZ_MAGIC);
        assert_eq!(&body[8..40], &ho);
        let plain = zstd::bulk::compress(&new, 19).unwrap();
        assert!(
            body.len() * 4 < plain.len(),
            "delta {} bytes vs {} bytes compressed alone",
            body.len(),
            plain.len()
        );
        let mut dec = zstd::bulk::Decompressor::with_dictionary(&old).unwrap();
        assert_eq!(dec.decompress(&body[40..], 1 << 20).unwrap(), new.to_vec());
        assert!(
            d.delta(&[7; 32], "/a-NEW123.js", hn, &new).is_none(),
            "unknown dictionary"
        );
        let header = format!(":{}:", {
            use base64::Engine;
            base64::engine::general_purpose::STANDARD.encode(ho)
        });
        assert_eq!(parse_available(&header), Some(ho));
        assert_eq!(parse_available("nonsense"), None);
    }

    #[test]
    fn parses_chromium_header() {
        assert!(parse_available(":m1T0IR+gSor7AvRnQk9qjdRNZfgikoiccCqf9V3aHZs=:").is_some());
    }
}
