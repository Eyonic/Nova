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

pub mod client;
pub mod dispatch;
pub mod framework;
pub mod isolation;
pub mod live;
pub mod metrics;
pub mod php;
pub mod ratelimit;
pub mod rules;
pub mod sites;
pub mod tasks;
pub mod tls;

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
            project: s.path.clone(),
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
    let mut rt = Runtime {
        pools: php::start_all(&cfg, mode).await?,
        tasks: tasks::start_all(&cfg, mode)?,
        cfg,
        mode,
    };

    let result = match mode {
        Effective::Strict => supervise(&mut rt, &config_path).await,
        Effective::Shared => run_worker(rt.cfg.clone(), wait_for_signal(), true).await,
    };
    rt.tasks.stop().await;
    php::stop_all(rt.pools).await;
    tracing::info!("NOVA stopped");
    result
}

/// What the supervisor owns besides the worker.
struct Runtime {
    cfg: Config,
    mode: Effective,
    pools: Vec<php::Pool>,
    tasks: tasks::Tasks,
}

/// A running `nova worker` process.
struct Worker {
    child: tokio::process::Child,
    started: std::time::Instant,
}

impl Worker {
    /// Start `nova worker` as the unprivileged worker uid. With `wait_ready`,
    /// return only once it listens on every port (or fail).
    async fn spawn(cfg: &Config, config_path: &Path, wait_ready: bool) -> Result<Self> {
        let exe = std::env::current_exe().context("locating the nova binary")?;
        let (uid, gid) = isolation::worker_ids(cfg);
        let mut cmd = tokio::process::Command::new(&exe);
        // The worker never receives secrets: only logging and mode settings pass through.
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
        // Basic-auth password hashes (not secrets in themselves) the worker checks.
        for var in cfg
            .sites
            .iter()
            .filter_map(|s| s.auth.as_ref()?.users_env.as_ref())
        {
            if let Ok(v) = std::env::var(var) {
                cmd.env(var, v);
            }
        }
        // Readiness pipe: the worker writes one byte once it accepts connections.
        let mut fds = [0; 2];
        // SAFETY: pipe2 fills two descriptors on success; ownership is taken below.
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
            return Err(std::io::Error::last_os_error()).context("creating the ready pipe");
        }
        // SAFETY: both descriptors were just created and are owned here.
        let (read, write) = unsafe {
            use std::os::fd::FromRawFd;
            (
                std::os::fd::OwnedFd::from_raw_fd(fds[0]),
                std::os::fd::OwnedFd::from_raw_fd(fds[1]),
            )
        };
        let write_fd = std::os::fd::AsRawFd::as_raw_fd(&write);
        cmd.env("NOVA_READY_FD", READY_FD.to_string());
        // SAFETY: dup2 is async-signal-safe; it only places the inherited
        // write end at a fixed descriptor number without CLOEXEC.
        unsafe {
            cmd.pre_exec(move || {
                if libc::dup2(write_fd, READY_FD) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = cmd
            .kill_on_drop(true)
            .spawn()
            .context("starting the worker")?;
        drop(write);
        tracing::info!(pid = child.id(), uid, "worker started");
        let mut w = Worker {
            child,
            started: std::time::Instant::now(),
        };
        if wait_ready {
            let mut pipe =
                tokio::net::unix::pipe::Receiver::from_owned_fd(read).context("ready pipe")?;
            let mut byte = [0u8; 1];
            let ready = tokio::time::timeout(
                Duration::from_secs(30),
                tokio::io::AsyncReadExt::read(&mut pipe, &mut byte),
            )
            .await;
            if !matches!(ready, Ok(Ok(1))) {
                w.stop(Duration::from_secs(1)).await;
                anyhow::bail!("the new worker did not become ready");
            }
        }
        Ok(w)
    }

    /// SIGTERM (drain), then SIGKILL after `grace`.
    async fn stop(&mut self, grace: Duration) {
        if let Some(pid) = self.child.id() {
            // SAFETY: kill(2) on our own child.
            unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
        }
        match tokio::time::timeout(grace, self.child.wait()).await {
            Ok(status) => tracing::info!(?status, "worker stopped"),
            Err(_) => {
                tracing::warn!("worker did not stop in time; killing");
                let _ = self.child.kill().await;
            }
        }
    }
}

/// Descriptor number of the readiness pipe inside the worker.
const READY_FD: i32 = 3;

/// Strict mode: run the worker, restart it if it crashes, reload on SIGHUP
/// without dropping connections, and drain on SIGTERM/SIGINT.
async fn supervise(rt: &mut Runtime, config_path: &Path) -> Result<()> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate()).context("installing SIGTERM handler")?;
    let mut hup = signal(SignalKind::hangup()).context("installing SIGHUP handler")?;
    let mut backoff = Duration::from_millis(500);
    let mut worker = Worker::spawn(&rt.cfg, config_path, false).await?;

    loop {
        let grace = Duration::from_secs(rt.cfg.server.shutdown_grace_secs + 5);
        tokio::select! {
            status = worker.child.wait() => {
                tracing::error!(?status, "worker exited unexpectedly; restarting");
                if worker.started.elapsed() > Duration::from_secs(30) {
                    backoff = Duration::from_millis(500);
                }
                tokio::select! {
                    _ = tokio::time::sleep(backoff) => {}
                    _ = term.recv() => return Ok(()),
                    _ = tokio::signal::ctrl_c() => return Ok(()),
                }
                backoff = (backoff * 2).min(Duration::from_secs(30));
                worker = Worker::spawn(&rt.cfg, config_path, false).await?;
            }
            _ = term.recv() => {
                tracing::info!("received SIGTERM");
                worker.stop(grace).await;
                return Ok(());
            }
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("received SIGINT");
                worker.stop(grace).await;
                return Ok(());
            }
            _ = hup.recv() => {
                tracing::info!("received SIGHUP; reloading configuration");
                match reload(rt, config_path).await {
                    Ok(()) => match Worker::spawn(&rt.cfg, config_path, true).await {
                        Ok(new) => {
                            // The new worker shares the ports (SO_REUSEPORT);
                            // the old one stops accepting and drains.
                            let mut old = std::mem::replace(&mut worker, new);
                            tokio::spawn(async move { old.stop(grace).await });
                            tracing::info!("configuration reloaded");
                        }
                        Err(e) => tracing::error!(error = %e, "reload: keeping the previous worker"),
                    },
                    Err(e) => tracing::error!(error = format!("{e:#}"), "reload rejected; nothing changed"),
                }
            }
        }
    }
}

/// Validate the new configuration, then apply it to directories, PHP pools
/// and background tasks. An invalid file changes nothing.
async fn reload(rt: &mut Runtime, config_path: &Path) -> Result<()> {
    let cfg = Config::load_env(config_path)?;
    let mode = isolation::effective_mode(&cfg)?;
    if mode != rt.mode {
        anyhow::bail!("the isolation mode cannot change on reload; restart instead");
    }
    // Catch site errors (paths, auth files, ...) before touching anything.
    Sites::new(&cfg).map_err(anyhow::Error::msg)?;
    isolation::prepare_dirs(&cfg, mode)?;
    php::reconcile(&cfg, mode, &mut rt.pools).await?;
    let old_tasks = std::mem::replace(&mut rt.tasks, tasks::start_all(&cfg, mode)?);
    old_tasks.stop().await;
    rt.cfg = cfg;
    Ok(())
}

/// Entry point for `nova worker` (strict mode child): sandbox self, then serve.
pub async fn worker_main(cfg: Config, config_path: PathBuf) -> Result<()> {
    if nova_security::is_root() {
        anyhow::bail!("`nova worker` must not run as root");
    }
    // Same rule as for PHP: with require_landlock the worker fails closed.
    let level = isolation::worker_sandbox(&cfg, &config_path)
        .apply(cfg.isolation.require_landlock)
        .map_err(|e| anyhow::anyhow!("sandboxing the worker: {e}"))?;
    // SAFETY: getuid has no preconditions.
    let uid = unsafe { libc::getuid() };
    tracing::info!(uid, landlock = level.as_str(), "worker sandboxed");
    run_worker(cfg, wait_for_signal(), false).await
}

/// HTTP server + optimizer, until `shutdown` resolves.
async fn run_worker(
    cfg: Config,
    shutdown: impl std::future::Future<Output = ()>,
    in_process: bool,
) -> Result<()> {
    if in_process {
        // Shared mode has no separate supervisor to hand a reload to.
        tokio::spawn(async {
            use tokio::signal::unix::{SignalKind, signal};
            if let Ok(mut hup) = signal(SignalKind::hangup()) {
                while hup.recv().await.is_some() {
                    tracing::warn!(
                        "SIGHUP: live reload needs strict isolation (start as root); restart NOVA to apply changes"
                    );
                }
            }
        });
    }
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
    let mut app = App::new(
        cfg.mode,
        sites,
        optimizer,
        cfg.server.max_request_body.0,
        cfg.server.listen.port(),
        cfg.server.metrics,
        php_ready,
        databases,
        Arc::new(live::LiveHub::new(cfg.live.clone())),
    );
    let comp = &cfg.server.compression;
    if comp.enabled {
        app.compression = Some(nova_http::compress::Options {
            min_size: comp.min_size.0,
        });
        app.precompressed = comp.precompressed;
    }
    let srv = &cfg.server;
    let rl = &srv.rate_limit;
    let mut rate_exempt = srv.admin_allow.clone();
    rate_exempt.extend(rl.exempt.iter().copied());
    let tls_cfg = &srv.tls;
    app.http = dispatch::HttpSettings {
        trusted_proxies: srv.trusted_proxies.clone(),
        forwarded_header: srv.forwarded_header,
        admin_allow: srv.admin_allow.clone(),
        rate_limit: rl
            .enabled
            .then(|| ratelimit::RateLimiter::new(rl.requests_per_sec, rl.burst)),
        rate_exempt: rate_exempt.clone(),
        body_timeout: Duration::from_secs(srv.request_body_timeout_secs.max(1)),
        access_log: srv.access_log,
        tls: tls_cfg.enabled.then_some(dispatch::TlsPublic {
            port: tls_cfg.public_port,
            hsts_max_age: tls_cfg.hsts_max_age_secs,
            http3: tls_cfg.http3,
            trusted_certs: tls_cfg.acme || !tls_cfg.certs.is_empty(),
        }),
    };
    let app = Arc::new(app);

    let mut listeners = vec![nova_http::Listener {
        tcp: bind_tcp(srv.listen).with_context(|| format!("cannot listen on {}", srv.listen))?,
        tls: None,
        proxy_protocol: srv.proxy_protocol,
    }];
    tracing::info!(listen = %srv.listen, "accepting HTTP connections");
    let mut quic = None;
    let mut acme_task = None;
    if tls_cfg.enabled {
        let _ = nova_http::rustls::crypto::ring::default_provider().install_default();
        let t = tls::setup(&cfg).context("setting up TLS")?;
        listeners.push(nova_http::Listener {
            tcp: bind_tcp(tls_cfg.listen)
                .with_context(|| format!("cannot listen on {}", tls_cfg.listen))?,
            tls: Some(t.settings),
            proxy_protocol: srv.proxy_protocol,
        });
        if let Some(qc) = t.quic {
            let udp = bind_udp(tls_cfg.listen)
                .with_context(|| format!("cannot listen on udp {}", tls_cfg.listen))?;
            quic = Some(
                nova_http::quinn::Endpoint::new(
                    nova_http::quinn::EndpointConfig::default(),
                    Some(qc),
                    udp,
                    Arc::new(nova_http::quinn::TokioRuntime),
                )
                .context("starting the QUIC endpoint")?,
            );
        }
        acme_task = t.acme_task;
        tracing::info!(listen = %tls_cfg.listen, http3 = quic.is_some(), "accepting HTTPS connections");
    }

    notify_ready();

    let shutdown = {
        let app = Arc::clone(&app);
        async move {
            shutdown.await;
            app.shutting_down.store(true, Ordering::Relaxed);
            // Event streams never end on their own; close them so draining finishes.
            app.live.close();
        }
    };
    let proxies = srv.trusted_proxies.clone();
    // Trusted proxies multiplex many clients over few connections.
    let mut conn_exempt = rate_exempt;
    conn_exempt.extend(srv.trusted_proxies.iter().copied());
    let opts = nova_http::ServerOptions {
        max_connections: srv.max_connections,
        max_connections_per_ip: if rl.enabled {
            rl.max_connections_per_ip
        } else {
            0
        },
        per_ip_exempt: Arc::new(move |ip| nova_config::Cidr::any_contains(&conn_exempt, ip)),
        proxy_from: Arc::new(move |ip| nova_config::Cidr::any_contains(&proxies, ip)),
        header_read_timeout: Duration::from_secs(srv.header_read_timeout_secs),
        shutdown_grace: Duration::from_secs(srv.shutdown_grace_secs),
    };
    nova_http::serve(listeners, quic, opts, Arc::clone(&app), shutdown).await?;
    if let Some(t) = acme_task {
        t.abort();
    }

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

/// Listening sockets with SO_REUSEPORT, so a reloaded worker can bind the
/// same ports while the previous one drains.
fn bind_tcp(addr: std::net::SocketAddr) -> std::io::Result<tokio::net::TcpListener> {
    let s = socket2::Socket::new(
        socket2::Domain::for_address(addr),
        socket2::Type::STREAM,
        Some(socket2::Protocol::TCP),
    )?;
    s.set_reuse_address(true)?;
    s.set_reuse_port(true)?;
    s.set_nonblocking(true)?;
    s.bind(&addr.into())?;
    s.listen(1024)?;
    tokio::net::TcpListener::from_std(s.into())
}

fn bind_udp(addr: std::net::SocketAddr) -> std::io::Result<std::net::UdpSocket> {
    let s = socket2::Socket::new(
        socket2::Domain::for_address(addr),
        socket2::Type::DGRAM,
        Some(socket2::Protocol::UDP),
    )?;
    s.set_reuse_address(true)?;
    s.set_reuse_port(true)?;
    s.bind(&addr.into())?;
    Ok(s.into())
}

/// Tell the supervisor (through the inherited pipe) that we accept connections.
fn notify_ready() {
    let Some(fd) = std::env::var("NOVA_READY_FD")
        .ok()
        .and_then(|v| v.parse::<i32>().ok())
    else {
        return;
    };
    // SAFETY: the supervisor placed the pipe's write end at this descriptor;
    // we take ownership once, write one byte and close it.
    let mut f = unsafe { <std::fs::File as std::os::fd::FromRawFd>::from_raw_fd(fd) };
    let _ = std::io::Write::write_all(&mut f, b"1");
}

async fn wait_for_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
    tokio::select! {
        _ = term.recv() => tracing::info!("received SIGTERM"),
        _ = tokio::signal::ctrl_c() => tracing::info!("received SIGINT"),
    }
}
