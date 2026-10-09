//! NOVA Optimize: built-in, incremental image optimization.
//!
//! Pipeline per scan: Discover (walk document roots) → Analyze → Plan →
//! Transform → Validate → Publish (atomic manifest swap) → Serve
//! ([`Optimizer::select`]). Work happens in the background; requests are
//! never blocked on encoding. Until a variant exists the original is served.

pub mod manifest;
pub mod negotiate;
pub mod transform;

use manifest::{Asset, AssetError, Format, Manifest, Source, Variant};
use nova_config::OptimizeConfig;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::{Semaphore, watch};

pub use negotiate::Choice;

/// Bumped whenever encoder behavior changes in a way that should regenerate outputs.
const PIPELINE_REVISION: u32 = 1;

#[derive(Debug, Clone)]
pub struct SiteSource {
    pub name: String,
    pub root: PathBuf,
}

/// A representation chosen for a request.
#[derive(Debug, Clone)]
pub struct Selected {
    pub path: PathBuf,
    pub format: Format,
    pub bytes: u64,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct ScanStats {
    pub sources: usize,
    pub reused: usize,
    pub processed: usize,
    pub failed: usize,
    pub variants_written: usize,
}

pub struct Optimizer {
    cfg: OptimizeConfig,
    /// `<state_dir>/optimize`
    dir: PathBuf,
    sites: Vec<SiteSource>,
    profile: String,
    manifests: RwLock<HashMap<String, Arc<Manifest>>>,
    permits: Arc<Semaphore>,
}

impl Optimizer {
    /// Create the optimizer and load previously published manifests, so a
    /// restarted container serves optimized assets immediately.
    pub fn new(cfg: OptimizeConfig, state_dir: &Path, sites: Vec<SiteSource>) -> Self {
        let dir = state_dir.join("optimize");
        let profile = profile_hash(&cfg);
        let mut manifests = HashMap::new();
        for site in &sites {
            match Manifest::load(&manifest_path(&dir, &site.name)) {
                Ok(Some(m)) if m.profile == profile => {
                    manifests.insert(site.name.clone(), Arc::new(m));
                }
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!(site = site.name, error = %e, "ignoring unreadable manifest")
                }
            }
        }
        let permits = Arc::new(Semaphore::new(cfg.workers));
        Self {
            cfg,
            dir,
            sites,
            profile,
            manifests: RwLock::new(manifests),
            permits,
        }
    }

    pub fn manifest(&self, site: &str) -> Option<Arc<Manifest>> {
        self.manifests.read().unwrap().get(site).cloned()
    }

    fn objects_dir(&self) -> PathBuf {
        self.dir.join("objects")
    }

    /// Choose a representation for `rel_path` (relative to the site's document
    /// root). `src_meta` is the current source file metadata, used to detect
    /// stale manifest entries: a changed source is served as-is until rescanned.
    pub fn select(
        &self,
        site: &str,
        rel_path: &str,
        src_meta: &std::fs::Metadata,
        accept: Option<&str>,
        width: Option<u32>,
    ) -> Option<Selected> {
        let manifest = self.manifest(site)?;
        let asset = manifest.assets.get(rel_path)?;
        if asset.source.bytes != src_meta.len() || asset.source.mtime_ns != mtime_ns(src_meta) {
            return None;
        }
        match negotiate::choose(asset, accept, width) {
            Choice::Original => None,
            Choice::Variant {
                object,
                format,
                bytes,
            } => Some(Selected {
                path: self.objects_dir().join(object),
                format,
                bytes,
            }),
        }
    }

    /// Scan every site once.
    pub async fn scan_all(self: &Arc<Self>) -> ScanStats {
        let mut total = ScanStats::default();
        for site in self.sites.clone() {
            let s = self.scan_site(&site).await;
            total.sources += s.sources;
            total.reused += s.reused;
            total.processed += s.processed;
            total.failed += s.failed;
            total.variants_written += s.variants_written;
        }
        total
    }

    /// Background loop: scan now, then every `scan_interval_secs` until stopped.
    pub async fn run(self: Arc<Self>, mut stop: watch::Receiver<bool>) {
        loop {
            let started = std::time::Instant::now();
            // Shutdown must not wait for a scan: dropping it cancels queued
            // jobs. Encodes already running finish on their blocking threads
            // but are never awaited; their output is published atomically or
            // not at all.
            let s = tokio::select! {
                s = self.scan_all() => s,
                _ = stop.changed() => return,
            };
            if s.processed > 0 || s.failed > 0 {
                tracing::info!(
                    sources = s.sources,
                    processed = s.processed,
                    reused = s.reused,
                    failed = s.failed,
                    variants = s.variants_written,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "optimize scan complete"
                );
            }
            if self.cfg.scan_interval_secs == 0 {
                return;
            }
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(self.cfg.scan_interval_secs)) => {}
                _ = stop.changed() => return,
            }
        }
    }

    async fn scan_site(self: &Arc<Self>, site: &SiteSource) -> ScanStats {
        let mut stats = ScanStats::default();
        let previous = self.manifest(&site.name);
        let root = site.root.clone();
        let found = tokio::task::spawn_blocking(move || discover(&root))
            .await
            .unwrap_or_default();
        stats.sources = found.len();

        let mut assets = BTreeMap::new();
        let mut errors = BTreeMap::new();
        let mut warnings = BTreeMap::new();
        let mut jobs = tokio::task::JoinSet::new();
        for d in found {
            let prev = previous.as_ref().and_then(|m| m.assets.get(&d.rel));
            // Fast path: unchanged size and mtime.
            if let Some(a) = prev
                && a.source.bytes == d.bytes
                && a.source.mtime_ns == d.mtime_ns
                && self.objects_present(a)
            {
                assets.insert(d.rel, a.clone());
                stats.reused += 1;
                continue;
            }
            if let Some(err) = previous.as_ref().and_then(|m| m.errors.get(&d.rel))
                && err.mtime_ns == d.mtime_ns
            {
                // Same broken file as last time; do not retry every scan.
                errors.insert(d.rel, err.clone());
                continue;
            }
            let this = Arc::clone(self);
            let permit = Arc::clone(&self.permits);
            let prev = prev.cloned();
            jobs.spawn(async move {
                let _permit = permit.acquire_owned().await;
                let rel = d.rel.clone();
                let res = tokio::task::spawn_blocking(move || this.process(&d, prev)).await;
                (rel, res)
            });
        }
        while let Some(joined) = jobs.join_next().await {
            let Ok((rel, res)) = joined else { continue };
            match res {
                Ok(Ok((asset, written))) => {
                    stats.processed += 1;
                    stats.variants_written += written;
                    assets.insert(rel, asset);
                }
                Ok(Err((e, mtime))) => {
                    stats.failed += 1;
                    tracing::warn!(site = site.name, asset = rel, error = %e, "image optimization failed");
                    errors.insert(
                        rel,
                        AssetError {
                            message: e.to_string(),
                            mtime_ns: mtime,
                        },
                    );
                }
                Err(e) => {
                    stats.failed += 1;
                    errors.insert(
                        rel,
                        AssetError {
                            message: format!("worker panicked: {e}"),
                            mtime_ns: 0,
                        },
                    );
                }
            }
        }

        // Budgets apply to every asset, including ones reused from earlier scans.
        if let Some(budget) = self.cfg.budget_bytes {
            for (rel, asset) in &assets {
                let smallest = asset
                    .variants
                    .iter()
                    .filter(|v| v.width == asset.source.width)
                    .map(|v| v.bytes)
                    .fold(asset.source.bytes, u64::min);
                if smallest > budget.0 {
                    warnings.insert(
                        rel.clone(),
                        format!(
                            "{smallest} bytes after optimization exceeds budget of {} bytes",
                            budget.0
                        ),
                    );
                }
            }
        }

        let changed = previous.as_ref().is_none_or(|p| {
            stats.processed > 0
                || p.assets.len() != assets.len()
                || p.errors != errors
                || p.warnings != warnings
        });
        if changed {
            let manifest = Manifest {
                version: manifest::MANIFEST_VERSION,
                site: site.name.clone(),
                profile: self.profile.clone(),
                generated_at: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0),
                assets,
                errors,
                warnings,
            };
            let path = manifest_path(&self.dir, &site.name);
            let to_save = manifest.clone();
            match tokio::task::spawn_blocking(move || to_save.save_atomic(&path)).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    tracing::error!(site = site.name, error = %e, "cannot write manifest")
                }
                Err(e) => tracing::error!(site = site.name, error = %e, "manifest writer panicked"),
            }
            self.manifests
                .write()
                .unwrap()
                .insert(site.name.clone(), Arc::new(manifest));
        }
        stats
    }

    fn objects_present(&self, a: &Asset) -> bool {
        a.variants
            .iter()
            .all(|v| self.objects_dir().join(&v.object).is_file())
    }

    /// Analyze, plan, transform and validate one source. Runs on a blocking thread.
    #[allow(clippy::type_complexity)]
    fn process(
        &self,
        d: &Discovered,
        prev: Option<Asset>,
    ) -> Result<(Asset, usize), (transform::TransformError, u128)> {
        let fail = |e: transform::TransformError| (e, d.mtime_ns);
        let data = std::fs::read(&d.path).map_err(|e| fail(e.into()))?;
        let hash = blake3::hash(&data).to_hex().to_string();
        drop(data);
        // Content unchanged (e.g. touched or copied): only refresh metadata.
        if let Some(mut a) = prev
            && a.source.hash == hash
            && self.objects_present(&a)
        {
            a.source.bytes = d.bytes;
            a.source.mtime_ns = d.mtime_ns;
            return Ok((a, 0));
        }
        let analyzed = transform::analyze(&d.path, d.format, &self.cfg).map_err(fail)?;
        let source = Source {
            hash,
            bytes: d.bytes,
            mtime_ns: d.mtime_ns,
            width: analyzed.width,
            height: analyzed.height,
            format: d.format,
            alpha: analyzed.alpha,
        };
        let mut variants = Vec::new();
        let mut written = 0;
        for p in transform::plan(&source, &self.cfg) {
            // Derivation-addressed: same source + same parameters = same object,
            // which deduplicates identical images across paths and sites.
            let key = blake3::hash(
                format!(
                    "{}|{}|{}|{:?}",
                    source.hash, self.profile, p.width, p.format
                )
                .as_bytes(),
            )
            .to_hex();
            let object = format!("{}/{}.{}", &key[..2], &key[..32], p.format.extension());
            let path = self.objects_dir().join(&object);
            let bytes = match std::fs::metadata(&path) {
                Ok(m) => m.len(),
                Err(_) => {
                    let out = transform::render(&analyzed.image, analyzed.alpha, p, &self.cfg)
                        .map_err(fail)?;
                    manifest::write_atomic(&path, &out).map_err(|e| fail(e.into()))?;
                    written += 1;
                    out.len() as u64
                }
            };
            variants.push(Variant {
                width: p.width,
                height: p.height,
                format: p.format,
                object,
                bytes,
            });
        }
        Ok((Asset { source, variants }, written))
    }
}

fn manifest_path(dir: &Path, site: &str) -> PathBuf {
    dir.join("sites").join(site).join("manifest.json")
}

fn profile_hash(cfg: &OptimizeConfig) -> String {
    let desc = format!(
        "r{PIPELINE_REVISION}|{:?}|{:?}|{:?}|{}",
        cfg.widths, cfg.formats, cfg.quality, cfg.avif_speed
    );
    blake3::hash(desc.as_bytes()).to_hex()[..16].to_string()
}

pub fn mtime_ns(m: &std::fs::Metadata) -> u128 {
    m.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

#[derive(Debug, Clone)]
struct Discovered {
    rel: String,
    path: PathBuf,
    format: Format,
    bytes: u64,
    mtime_ns: u128,
}

/// Step 1: find optimizable sources. Skips dot-directories and symlinks.
fn discover(root: &Path) -> Vec<Discovered> {
    let mut out = Vec::new();
    let walker = walkdir::WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            e.depth() == 0 || !e.file_name().to_str().is_some_and(|n| n.starts_with('.'))
        });
    for entry in walker.flatten() {
        if !entry.file_type().is_file() {
            continue;
        }
        let Some(format) = Format::from_path(entry.path()) else {
            continue;
        };
        let Ok(meta) = entry.metadata() else { continue };
        let Ok(rel) = entry.path().strip_prefix(root) else {
            continue;
        };
        let Some(rel) = rel.to_str() else { continue };
        out.push(Discovered {
            rel: rel.replace(std::path::MAIN_SEPARATOR, "/"),
            path: entry.path().to_owned(),
            format,
            bytes: meta.len(),
            mtime_ns: mtime_ns(&meta),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("nova-opt-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn scan_publishes_and_reuses() {
        let base = tempdir("scan");
        let root = base.join("public");
        std::fs::create_dir_all(root.join("img")).unwrap();
        std::fs::create_dir_all(root.join(".hidden")).unwrap();
        let img = image::RgbImage::from_fn(400, 200, |x, y| {
            image::Rgb([(x % 256) as u8, (y % 256) as u8, 90])
        });
        img.save(root.join("img/a.jpg")).unwrap();
        img.save(root.join(".hidden/b.jpg")).unwrap();
        std::fs::write(root.join("broken.png"), b"not a png").unwrap();

        let cfg = OptimizeConfig {
            widths: vec![100, 200],
            avif_speed: 10,
            scan_interval_secs: 0,
            ..Default::default()
        };
        let opt = Arc::new(Optimizer::new(
            cfg.clone(),
            &base.join("state"),
            vec![SiteSource {
                name: "s".into(),
                root: root.clone(),
            }],
        ));
        let s = opt.scan_all().await;
        assert_eq!((s.sources, s.processed, s.failed), (2, 1, 1), "{s:?}");
        let m = opt.manifest("s").unwrap();
        let a = &m.assets["img/a.jpg"];
        // 100, 200: avif+webp+jpeg; 400: avif+webp.
        assert_eq!(a.variants.len(), 8);
        assert!(m.errors.contains_key("broken.png"));

        let meta = std::fs::metadata(root.join("img/a.jpg")).unwrap();
        let sel = opt
            .select("s", "img/a.jpg", &meta, Some("image/avif"), Some(150))
            .unwrap();
        assert_eq!(sel.format, Format::Avif);
        assert!(sel.path.is_file());

        // Second scan reuses everything; a fresh optimizer loads the manifest from disk.
        let s2 = opt.scan_all().await;
        assert_eq!((s2.reused, s2.processed), (1, 0));
        let reloaded = Optimizer::new(
            cfg,
            &base.join("state"),
            vec![SiteSource {
                name: "s".into(),
                root,
            }],
        );
        assert!(reloaded.manifest("s").is_some());
        let _ = std::fs::remove_dir_all(&base);
    }
}
