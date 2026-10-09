//! The per-site asset manifest: the published result of an optimization run.
//!
//! Stored as `<state>/optimize/sites/<site>/manifest.json` and replaced
//! atomically (write to a temp file, fsync, rename). Variant files live in a
//! shared, immutable object store, so an old manifest stays valid until
//! garbage collection removes unreferenced objects.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::Path;

pub const MANIFEST_VERSION: u32 = 1;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Manifest {
    pub version: u32,
    pub site: String,
    /// Hash of the optimization profile; a change invalidates every entry.
    pub profile: String,
    pub generated_at: u64,
    /// Keyed by path relative to the document root, `/`-separated.
    pub assets: BTreeMap<String, Asset>,
    /// Sources that could not be optimized; they are served unmodified.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub errors: BTreeMap<String, AssetError>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub warnings: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssetError {
    pub message: String,
    /// Source mtime at the failed attempt; the file is retried once it changes.
    pub mtime_ns: u128,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Asset {
    pub source: Source,
    pub variants: Vec<Variant>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Source {
    pub hash: String,
    pub bytes: u64,
    /// Modification time in nanoseconds since the epoch.
    pub mtime_ns: u128,
    pub width: u32,
    pub height: u32,
    pub format: Format,
    pub alpha: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Variant {
    pub width: u32,
    pub height: u32,
    pub format: Format,
    /// Object path relative to the object store.
    pub object: String,
    pub bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    Avif,
    Webp,
    Jpeg,
    Png,
}

impl Format {
    pub fn extension(self) -> &'static str {
        match self {
            Format::Avif => "avif",
            Format::Webp => "webp",
            Format::Jpeg => "jpg",
            Format::Png => "png",
        }
    }

    pub fn mime(self) -> &'static str {
        match self {
            Format::Avif => "image/avif",
            Format::Webp => "image/webp",
            Format::Jpeg => "image/jpeg",
            Format::Png => "image/png",
        }
    }

    pub fn from_path(path: &Path) -> Option<Self> {
        match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
            "jpg" | "jpeg" => Some(Format::Jpeg),
            "png" => Some(Format::Png),
            "webp" => Some(Format::Webp),
            _ => None,
        }
    }
}

impl Manifest {
    pub fn load(path: &Path) -> io::Result<Option<Self>> {
        match std::fs::read(path) {
            Ok(bytes) => {
                let m: Manifest = serde_json::from_slice(&bytes)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                Ok((m.version == MANIFEST_VERSION).then_some(m))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub fn save_atomic(&self, path: &Path) -> io::Result<()> {
        let json = serde_json::to_vec_pretty(self).map_err(io::Error::other)?;
        write_atomic(path, &json)
    }
}

/// Write via a temporary sibling and rename, so readers never see partial files.
pub fn write_atomic(path: &Path, data: &[u8]) -> io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::other("path has no parent"))?;
    std::fs::create_dir_all(dir)?;
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = dir.join(format!(
        ".{}.{}-{seq}.tmp",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("file"),
        std::process::id()
    ));
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}
