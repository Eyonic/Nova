//! NOVA Core: site lifecycle, request dispatch and process orchestration.
//!
//! `serve` picks an isolation mode (see [`isolation`]):
//!
//! * **strict** (started as root): this process becomes a small supervisor.
//!   It prepares directory ownership, starts one sandboxed PHP-FPM master per
//!   site under that site's uid, and runs the HTTP worker as an unprivileged
//!   child (`nova worker`). It never handles network traffic itself.
//! * **shared** (started as non-root): one process runs PHP supervision and
//!   the worker; sites are still separated by Landlock.
//!
//! Shutdown (SIGTERM/SIGINT): readiness turns 503, the listener closes,
//! in-flight requests drain, then the optimizer and PHP stop.

pub mod dispatch;
pub mod framework;
pub mod isolation;
pub mod live;
pub mod metrics;
pub mod php;
pub mod sites;

use anyhow::{Context, Result};
use dispatch::App;
use isolation::Effective;
use nova_config::Config;
use nova_optimize::{Optimizer, SiteSource};
use sites::Sites;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::sync::watch;

pub fn optimizer_for(cfg: &Config) -> Option<Arc<Optimizer>> {
    if !cfg.optimize.enabled {
        return None;
    }
    let sources = cfg
        .sites
        .iter()
        .filter(|s| s.optimize)
        .map(|s| SiteSource {
            name: s.name.clone(),
            root: s.document_root(),
        })
        .collect();
    Some(Arc::new(Optimizer::new(
        cfg.optimize.clone(),
        &cfg.paths.state_dir,
        sources,
    )))
}

/// Entry point for `nova serve`.
pub async fn serve(cfg: Config, config_path: PathBuf) -> Result<()> {
    let mode = isolation::effective_mode(&cfg)?;
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        mode = %cfg.mode,
        isolation = mode.as_str(),
        sites = cfg.sites.len(),
        "starting NOVA"
    );
    isolation::prepare_dirs(&cfg, mode)?;
    let fpms = if cfg.php_enabled() {
        php::start_all(&cfg, mode).await?
    } else {
        Vec::new()
    };

    let result = match mode {
        Effective::Strict => supervise_worker(&cfg, &config_path).await,
        Effective::Shared => run_worker(cfg.clone(), wait_for_signal()).await,
    };
    php::stop_all(fpms).await;
    tracing::info!("NOVA stopped");
    result
}

/// Strict mode: run `nova worker` as the unprivileged worker uid, restart it
/// if it crashes, forward SIGTERM and wait for it to drain.
async fn supervise_worker(cfg: &Config, config_path: &Path) -> Result<()> {
    let exe = std::env::current_exe().context("locating the nova binary")?;
    let (uid, gid) = isolation::worker_ids(cfg);
    let grace = Duration::from_secs(cfg.server.shutdown_grace_secs + 5);
    let mut backoff = Duration::from_millis(500);
    let signal = wait_for_signal();
    tokio::pin!(signal);

    loop {
        // The worker never receives secrets: only logging and mode settings pass through.
        let mut cmd = tokio::process::Command::new(&exe);
        cmd.arg("--config")
            .arg(config_path)
            .arg("worker")
            .uid(uid)
            .gid(gid)
            .env_clear();
        for key in ["PATH", "NOVA_MODE", "NOVA_LOG", "NOVA_LOG_FORMAT", "TZ"] {
            if let Ok(v) = std::env::var(key) {
                cmd.env(key, v);
            }
        }
        let mut child = cmd
            .kill_on_drop(true)
            .spawn()
            .context("starting the worker")?;
        let started = std::time::Instant::now();
        tracing::info!(pid = child.id(), uid, "worker started");

        tokio::select! {
            status = child.wait() => {
                tracing::error!(?status, "worker exited unexpectedly; restarting");
                if started.elapsed() > Duration::from_secs(30) {
                    backoff = Duration::from_millis(500);
                }
                tokio::select! {
                    _ = tokio::time::sleep(backoff) => {}
                    _ = &mut signal => return Ok(()),
                }
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
            _ = &mut signal => {
                if let Some(pid) = child.id() {
                    // SAFETY: kill(2) on our own child.
                    unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
                }
                match tokio::time::timeout(grace, child.wait()).await {
                    Ok(status) => tracing::info!(?status, "worker stopped"),
                    Err(_) => {
                        tracing::warn!("worker did not stop in time; killing");
                        let _ = child.kill().await;
                    }
                }
                return Ok(());
            }
        }
    }
}

/// Entry point for `nova worker` (strict mode child): sandbox self, then serve.
pub async fn worker_main(cfg: Config, config_path: PathBuf) -> Result<()> {
    if nova_security::is_root() {
        anyhow::bail!("`nova worker` must not run as root");
    }
    let level = isolation::worker_sandbox(&cfg, &config_path)
        .apply(false)
        .map_err(|e| anyhow::anyhow!("sandboxing the worker: {e}"))?;
    // SAFETY: getuid has no preconditions.
    let uid = unsafe { libc::getuid() };
    tracing::info!(uid, landlock = level.as_str(), "worker sandboxed");
    run_worker(cfg, wait_for_signal()).await
}

/// HTTP server + optimizer, until `shutdown` resolves.
async fn run_worker(cfg: Config, shutdown: impl std::future::Future<Output = ()>) -> Result<()> {
    let sites = Sites::new(&cfg).map_err(anyhow::Error::msg)?;
    let (stop_tx, stop_rx) = watch::channel(false);

    let optimizer = optimizer_for(&cfg);
    let optimize_task = optimizer
        .clone()
        .map(|opt| tokio::spawn(opt.run(stop_rx.clone())));

    let php_ready = cfg.php_enabled().then(|| {
        let sockets: Vec<PathBuf> = cfg
            .sites
            .iter()
            .filter(|s| s.php.as_ref().is_some_and(|p| p.enabled))
            .map(|s| sites::php_socket(&cfg, s))
            .collect();
        let (tx, rx) = watch::channel(false);
        tokio::spawn(probe_php(sockets, tx, stop_rx.clone()));
        rx
    });

    let databases = cfg
        .services
        .database
        .iter()
        .map(|(name, d)| (name.clone(), d.host.clone(), d.port))
        .collect();
    let app = Arc::new(App::new(
        cfg.mode,
        sites,
        optimizer,
        cfg.server.max_request_body.0,
        cfg.server.listen.port(),
        cfg.server.metrics,
        php_ready,
        databases,
        Arc::new(live::LiveHub::new(cfg.live.clone())),
    ));

    let listener = tokio::net::TcpListener::bind(cfg.server.listen)
        .await
        .with_context(|| format!("cannot listen on {}", cfg.server.listen))?;
    tracing::info!(listen = %cfg.server.listen, "accepting connections");

    let shutdown = {
        let app = Arc::clone(&app);
        async move {
            shutdown.await;
            app.shutting_down.store(true, Ordering::Relaxed);
            // Event streams never end on their own; close them so draining finishes.
            app.live.close();
        }
    };
    let opts = nova_http::ServerOptions {
        max_connections: cfg.server.max_connections,
        header_read_timeout: Duration::from_secs(cfg.server.header_read_timeout_secs),
        shutdown_grace: Duration::from_secs(cfg.server.shutdown_grace_secs),
    };
    nova_http::serve(listener, opts, Arc::clone(&app), shutdown).await?;

    let _ = stop_tx.send(true);
    if let Some(t) = optimize_task {
        // An in-progress scan finishes its current images; don't wait forever.
        let _ = tokio::time::timeout(Duration::from_secs(5), t).await;
    }
    Ok(())
}

/// The worker does not own PHP processes, so it learns readiness by pinging
/// every site's pool through the real FastCGI path.
async fn probe_php(
    sockets: Vec<PathBuf>,
    tx: watch::Sender<bool>,
    mut stop: watch::Receiver<bool>,
) {
    let mut tick = tokio::time::interval(Duration::from_secs(2));
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            _ = stop.changed() => return,
        }
        let mut all = true;
        for s in &sockets {
            if !nova_runtime_php::fpm::ping(s).await {
                all = false;
                break;
            }
        }
        tx.send_if_modified(|v| std::mem::replace(v, all) != all);
    }
}

async fn wait_for_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
    tokio::select! {
        _ = term.recv() => tracing::info!("received SIGTERM"),
        _ = tokio::signal::ctrl_c() => tracing::info!("received SIGINT"),
    }
}
