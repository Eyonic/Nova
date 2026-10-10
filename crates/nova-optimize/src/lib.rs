//! NOVA Optimize: built-in, incremental image optimization.
//!
//! Pipeline per scan: Discover (walk document roots) → Analyze → Plan →
//! Transform → Validate → Publish (atomic manifest swap) → Serve
//! ([`Optimizer::select`]). Work happens in the background; requests are
//! never blocked on encoding. Until a variant exists the original is served.
//!
//! The same loop drives the Script Optimizer ([`text`]): JS/CSS
//! minification and precompression of text assets, with its own manifest
//! per site. Unreferenced objects are garbage-collected, and file changes
//! trigger a rescan right away (inotify) in addition to periodic polling.

pub mod manifest;
pub mod negotiate;
pub mod text;
pub mod transform;

use manifest::{Asset, AssetError, Format, Manifest, Source, Variant};
use nova_config::OptimizeConfig;
use std::collections::{BTreeMap, HashMap, HashSet};
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
    /// Project directory, searched for references in the script report.
    pub project: PathBuf,
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
    /// Text assets (re)processed by the Script Optimizer.
    pub text_processed: usize,
}

pub struct Optimizer {
    cfg: OptimizeConfig,
    /// `<state_dir>/optimize`
    dir: PathBuf,
    sites: Vec<SiteSource>,
    profile: String,
    manifests: RwLock<HashMap<String, Arc<Manifest>>>,
    text: RwLock<HashMap<String, Arc<text::TextManifest>>>,
    text_profile: String,
    last_gc: std::sync::Mutex<Option<std::time::Instant>>,
    /// Image encodes (seconds each, CPU heavy).
    permits: Arc<Semaphore>,
    /// Script/style jobs (milliseconds). Separate from `permits` so a
    /// backlog of image encodes never delays a changed script.
    text_permits: Arc<Semaphore>,
}

#[derive(Debug, Clone, Copy)]
enum Pass {
    Text,
    Images,
}

/// Bumped when minifier or compressor settings change.
const TEXT_REVISION: u32 = 1;

/// Selected text representation.
#[derive(Debug, Clone)]
pub struct TextSelected {
    pub path: PathBuf,
    /// `Content-Encoding` token, `None` for the minified identity version.
    pub encoding: Option<&'static str>,
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
        let text_profile = blake3::hash(format!("t{TEXT_REVISION}|{}", cfg.minify).as_bytes())
            .to_hex()[..16]
            .to_string();
        let mut texts = HashMap::new();
        for site in &sites {
            if let Ok(bytes) = std::fs::read(text_manifest_path(&dir, &site.name))
                && let Ok(m) = serde_json::from_slice::<text::TextManifest>(&bytes)
                && m.version == text::TEXT_MANIFEST_VERSION
                && m.profile == text_profile
            {
                texts.insert(site.name.clone(), Arc::new(m));
            }
        }
        let permits = Arc::new(Semaphore::new(cfg.workers));
        let text_permits = Arc::new(Semaphore::new(cfg.workers));
        Self {
            cfg,
            dir,
            sites,
            profile,
            manifests: RwLock::new(manifests),
            text: RwLock::new(texts),
            text_profile,
            last_gc: std::sync::Mutex::new(None),
            permits,
            text_permits,
        }
    }

    pub fn manifest(&self, site: &str) -> Option<Arc<Manifest>> {
        self.manifests.read().unwrap().get(site).cloned()
    }

    pub fn text_manifest(&self, site: &str) -> Option<Arc<text::TextManifest>> {
        self.text.read().unwrap().get(site).cloned()
    }

    /// Choose a stored representation of a text asset for a client that
    /// accepts `accepted` encodings (preference order). `None`: serve the
    /// original (unknown, changed since the scan, or nothing better).
    pub fn select_text(
        &self,
        site: &str,
        rel_path: &str,
        src_meta: &std::fs::Metadata,
        accepted: &[&str],
    ) -> Option<TextSelected> {
        let m = self.text_manifest(site)?;
        let a = m.files.get(rel_path)?;
        if a.bytes != src_meta.len() || a.mtime_ns != mtime_ns(src_meta) {
            return None;
        }
        let c = text::choose(a, accepted)?;
        Some(TextSelected {
            path: self.objects_dir().join(c.object),
            encoding: c.encoding.map(|e| e.token()),
        })
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

    /// Scan every site once: text assets, then images.
    pub async fn scan_all(self: &Arc<Self>) -> ScanStats {
        let mut total = self.scan_pass(Pass::Text).await;
        let images = self.scan_pass(Pass::Images).await;
        total.sources = images.sources;
        total.reused = images.reused;
        total.processed = images.processed;
        total.failed = images.failed;
        total.variants_written = images.variants_written;
        total
    }

    async fn scan_pass(self: &Arc<Self>, pass: Pass) -> ScanStats {
        let mut total = ScanStats::default();
        for site in self.sites.clone() {
            match pass {
                Pass::Text => {
                    if self.cfg.scripts {
                        total.text_processed += self.scan_text(&site).await;
                    }
                }
                Pass::Images => {
                    let s = self.scan_site(&site).await;
                    total.sources += s.sources;
                    total.reused += s.reused;
                    total.processed += s.processed;
                    total.failed += s.failed;
                    total.variants_written += s.variants_written;
                }
            }
        }
        if total.processed > 0 || total.text_processed > 0 || total.failed > 0 {
            self.maybe_gc();
        }
        total
    }

    /// Background work until stopped: a text loop and an image loop, each
    /// scanning now, again whenever files change (inotify, debounced) and
    /// every `scan_interval_secs`. They are independent so minutes of image
    /// encoding never delay a changed script.
    pub async fn run(self: Arc<Self>, stop: watch::Receiver<bool>) {
        let wake = [
            Arc::new(tokio::sync::Notify::new()),
            Arc::new(tokio::sync::Notify::new()),
        ];
        let watcher = if self.cfg.watch {
            self.watch_roots(wake.to_vec())
        } else {
            None
        };
        let polling_only = watcher.is_none();
        tokio::join!(
            Arc::clone(&self).pass_loop(
                Pass::Text,
                Arc::clone(&wake[0]),
                polling_only,
                stop.clone()
            ),
            Arc::clone(&self).pass_loop(Pass::Images, Arc::clone(&wake[1]), polling_only, stop),
        );
        drop(watcher);
    }

    async fn pass_loop(
        self: Arc<Self>,
        pass: Pass,
        wake: Arc<tokio::sync::Notify>,
        polling_only: bool,
        mut stop: watch::Receiver<bool>,
    ) {
        loop {
            let started = std::time::Instant::now();
            // Shutdown must not wait for a scan: dropping it cancels queued
            // jobs. Encodes already running finish on their blocking threads
            // but are never awaited; their output is published atomically or
            // not at all.
            let s = tokio::select! {
                s = self.scan_pass(pass) => s,
                _ = stop.changed() => return,
            };
            if s.processed > 0 || s.failed > 0 || s.text_processed > 0 {
                tracing::info!(
                    pass = ?pass,
                    sources = s.sources,
                    processed = s.processed,
                    reused = s.reused,
                    failed = s.failed,
                    variants = s.variants_written,
                    text = s.text_processed,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "optimize scan complete"
                );
            }
            if self.cfg.scan_interval_secs == 0 && polling_only {
                return;
            }
            let interval = match self.cfg.scan_interval_secs {
                0 => Duration::from_secs(365 * 24 * 3600),
                n => Duration::from_secs(n),
            };
            tokio::select! {
                _ = tokio::time::sleep(interval) => {}
                _ = wake.notified() => {
                    // Let editors and deploys finish writing before rescanning.
                    tokio::time::sleep(Duration::from_millis(300)).await;
                }
                _ = stop.changed() => return,
            }
        }
    }

    /// Watch every document root; any event wakes the scan loop.
    fn watch_roots(
        &self,
        wake: Vec<Arc<tokio::sync::Notify>>,
    ) -> Option<notify::RecommendedWatcher> {
        use notify::Watcher;
        let mut w = match notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if let Ok(ev) = res
                && !matches!(ev.kind, notify::EventKind::Access(_))
            {
                // notify_one stores a permit, so a change during a scan
                // triggers one more scan right after it.
                for w in &wake {
                    w.notify_one();
                }
            }
        }) {
            Ok(w) => w,
            Err(e) => {
                tracing::warn!(error = %e, "file watching unavailable; polling only");
                return None;
            }
        };
        for site in &self.sites {
            if let Err(e) = w.watch(&site.root, notify::RecursiveMode::Recursive) {
                tracing::warn!(site = site.name, error = %e, "cannot watch document root; polling only");
            }
        }
        Some(w)
    }

    /// Delete objects no manifest references any more. Runs at most every
    /// 10 minutes and spares files younger than an hour, so a manifest that
    /// is being replaced never points at a deleted object.
    fn maybe_gc(&self) {
        {
            let mut last = self.last_gc.lock().unwrap();
            if last.is_some_and(|t| t.elapsed() < Duration::from_secs(600)) {
                return;
            }
            *last = Some(std::time::Instant::now());
        }
        let mut keep: HashSet<String> = HashSet::new();
        for m in self.manifests.read().unwrap().values() {
            for a in m.assets.values() {
                keep.extend(a.variants.iter().map(|v| v.object.clone()));
            }
        }
        for m in self.text.read().unwrap().values() {
            for a in m.files.values() {
                keep.extend(a.minified.iter().map(|o| o.object.clone()));
                keep.extend(a.encoded.values().map(|o| o.object.clone()));
            }
        }
        let dir = self.objects_dir();
        let (removed, bytes) = gc_objects(&dir, &keep, Duration::from_secs(3600));
        if removed > 0 {
            tracing::info!(removed, bytes, "optimizer objects garbage-collected");
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

    /// Script Optimizer pass for one site; returns files (re)processed.
    async fn scan_text(self: &Arc<Self>, site: &SiteSource) -> usize {
        let previous = self.text_manifest(&site.name);
        let root = site.root.clone();
        let max = self.cfg.text_max_bytes.0;
        let found = tokio::task::spawn_blocking(move || discover_text(&root, max))
            .await
            .unwrap_or_default();
        let mut files = BTreeMap::new();
        let mut errors = BTreeMap::new();
        let mut jobs = tokio::task::JoinSet::new();
        let objects = self.objects_dir();
        for d in found {
            let prev = previous.as_ref().and_then(|m| m.files.get(&d.rel)).cloned();
            if let Some(a) = &prev
                && a.bytes == d.bytes
                && a.mtime_ns == d.mtime_ns
                && text_objects_present(&objects, a)
            {
                files.insert(d.rel, a.clone());
                continue;
            }
            let permit = Arc::clone(&self.text_permits);
            let (profile, minify, objects) =
                (self.text_profile.clone(), self.cfg.minify, objects.clone());
            jobs.spawn(async move {
                let _permit = permit.acquire_owned().await;
                let rel = d.rel.clone();
                let res =
                    tokio::task::spawn_blocking(move || -> std::io::Result<text::TextAsset> {
                        let data = std::fs::read(&d.path)?;
                        let hash = blake3::hash(&data).to_hex().to_string();
                        if let Some(mut a) = prev
                            && a.hash == hash
                            && text_objects_present(&objects, &a)
                        {
                            a.bytes = d.bytes;
                            a.mtime_ns = d.mtime_ns;
                            return Ok(a);
                        }
                        let p = text::process(&d.rel, d.kind, &data, d.mtime_ns, &profile, minify);
                        for (name, content) in p.objects {
                            let path = objects.join(&name);
                            if !path.is_file() {
                                manifest::write_atomic(&path, &content)?;
                            }
                        }
                        Ok(p.asset)
                    })
                    .await;
                (rel, d.mtime_ns, res)
            });
        }
        let mut processed = 0;
        while let Some(joined) = jobs.join_next().await {
            let Ok((rel, mtime, res)) = joined else {
                continue;
            };
            match res {
                Ok(Ok(asset)) => {
                    processed += 1;
                    files.insert(rel, asset);
                }
                Ok(Err(e)) => {
                    errors.insert(
                        rel,
                        AssetError {
                            message: e.to_string(),
                            mtime_ns: mtime,
                        },
                    );
                }
                Err(e) => {
                    errors.insert(
                        rel,
                        AssetError {
                            message: format!("worker panicked: {e}"),
                            mtime_ns: mtime,
                        },
                    );
                }
            }
        }
        let changed = previous
            .as_ref()
            .is_none_or(|p| processed > 0 || p.files.len() != files.len() || p.errors != errors);
        if !changed {
            return 0;
        }
        let (project, root) = (site.project.clone(), site.root.clone());
        let report_files = files.clone();
        let report =
            tokio::task::spawn_blocking(move || text::build_report(&report_files, &project, &root))
                .await
                .unwrap_or_default();
        let manifest = text::TextManifest {
            version: text::TEXT_MANIFEST_VERSION,
            site: site.name.clone(),
            profile: self.text_profile.clone(),
            generated_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            files,
            errors,
            report,
        };
        let path = text_manifest_path(&self.dir, &site.name);
        let json = serde_json::to_vec_pretty(&manifest).unwrap_or_default();
        if let Err(e) = tokio::task::spawn_blocking(move || manifest::write_atomic(&path, &json))
            .await
            .unwrap_or_else(|e| Err(std::io::Error::other(e)))
        {
            tracing::error!(site = site.name, error = %e, "cannot write script manifest");
        }
        self.text
            .write()
            .unwrap()
            .insert(site.name.clone(), Arc::new(manifest));
        processed
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

fn text_manifest_path(dir: &Path, site: &str) -> PathBuf {
    dir.join("sites").join(site).join("text.json")
}

fn text_objects_present(objects: &Path, a: &text::TextAsset) -> bool {
    a.minified
        .iter()
        .chain(a.encoded.values())
        .all(|o| objects.join(&o.object).is_file())
}

/// Remove files under `dir` that are not in `keep` (paths relative to
/// `dir`) and older than `min_age`. Returns (files, bytes) removed.
fn gc_objects(dir: &Path, keep: &HashSet<String>, min_age: Duration) -> (usize, u64) {
    let now = SystemTime::now();
    let (mut n, mut bytes) = (0, 0);
    for entry in walkdir::WalkDir::new(dir)
        .min_depth(1)
        .into_iter()
        .flatten()
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let Ok(rel) = entry.path().strip_prefix(dir) else {
            continue;
        };
        let rel = rel
            .to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/");
        if keep.contains(&rel) {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        let old = meta
            .modified()
            .ok()
            .and_then(|t| now.duration_since(t).ok())
            .is_some_and(|age| age >= min_age);
        if old && std::fs::remove_file(entry.path()).is_ok() {
            n += 1;
            bytes += meta.len();
        }
    }
    (n, bytes)
}

#[derive(Debug, Clone)]
struct DiscoveredText {
    rel: String,
    path: PathBuf,
    kind: text::TextKind,
    bytes: u64,
    mtime_ns: u128,
}

/// Text assets under a document root (no dot-directories, no symlinks).
fn discover_text(root: &Path, max_bytes: u64) -> Vec<DiscoveredText> {
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
        let Some(kind) = text::TextKind::from_path(entry.path()) else {
            continue;
        };
        let Ok(meta) = entry.metadata() else { continue };
        // Tiny files are not worth a stored copy; huge ones stay on-the-fly.
        if meta.len() < 256 || meta.len() > max_bytes {
            continue;
        }
        let Ok(rel) = entry.path().strip_prefix(root) else {
            continue;
        };
        let Some(rel) = rel.to_str() else { continue };
        out.push(DiscoveredText {
            rel: rel.replace(std::path::MAIN_SEPARATOR, "/"),
            path: entry.path().to_owned(),
            kind,
            bytes: meta.len(),
            mtime_ns: mtime_ns(&meta),
        });
    }
    out
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
                project: base.clone(),
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
                project: base.clone(),
            }],
        );
        assert!(reloaded.manifest("s").is_some());
        let _ = std::fs::remove_dir_all(&base);
    }
}
