//! `nova` — the NOVA command line.

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use nova_config::{Config, Mode};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

#[derive(Parser)]
#[command(name = "nova", version, about = "NOVA web runtime")]
struct Cli {
    /// Configuration file.
    #[arg(
        long,
        short,
        global = true,
        env = "NOVA_CONFIG",
        default_value = "/etc/nova/nova.toml"
    )]
    config: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the platform: HTTP server, PHP runtime and optimizer.
    Serve,
    /// Validate the configuration and print a summary.
    Check {
        /// Also print the generated PHP-FPM configuration.
        #[arg(long)]
        php: bool,
    },
    /// Run one optimization pass over every site and exit.
    Optimize,
    /// Internal: the unprivileged HTTP worker started by `serve` in strict isolation.
    #[command(hide = true)]
    Worker,
    /// Internal: apply a Landlock sandbox to this process, then exec COMMAND.
    #[command(hide = true)]
    Sandbox {
        #[arg(long)]
        read: Vec<PathBuf>,
        #[arg(long)]
        write: Vec<PathBuf>,
        #[arg(long)]
        connect: Vec<u16>,
        #[arg(long)]
        bind: Vec<u16>,
        /// Fail instead of running unsandboxed when Landlock is unavailable.
        #[arg(long)]
        require: bool,
        #[arg(last = true, required = true)]
        command: Vec<std::ffi::OsString>,
    },
    /// Probe the readiness endpoint of a running instance (for container health checks).
    Health {
        /// Address to probe; defaults to the configured listen port on loopback.
        #[arg(long)]
        addr: Option<SocketAddr>,
        /// Probe liveness instead of readiness.
        #[arg(long)]
        live: bool,
    },
}

fn load(path: &Path) -> Result<Config> {
    Config::load_env(path).with_context(|| format!("loading {}", path.display()))
}

fn init_logging(mode: Mode) {
    use tracing_subscriber::{EnvFilter, fmt};
    let filter = EnvFilter::try_from_env("NOVA_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    let json = match std::env::var("NOVA_LOG_FORMAT").as_deref() {
        Ok("json") => true,
        Ok("text") => false,
        _ => !mode.is_dev(),
    };
    if json {
        fmt()
            .json()
            .flatten_event(true)
            .with_current_span(false)
            .with_env_filter(filter)
            .init();
    } else {
        use std::io::IsTerminal;
        fmt()
            .with_ansi(std::io::stdout().is_terminal())
            .with_env_filter(filter)
            .init();
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Health { addr, live } => health(&cli.config, addr, live),
        Command::Check { php } => check(&cli.config, php),
        Command::Serve => load(&cli.config).and_then(|cfg| {
            init_logging(cfg.mode);
            run_to_completion(nova_core::serve(cfg, cli.config.clone()))
        }),
        Command::Worker => load(&cli.config).and_then(|cfg| {
            init_logging(cfg.mode);
            run_to_completion(nova_core::worker_main(cfg, cli.config.clone()))
        }),
        Command::Sandbox {
            read,
            write,
            connect,
            bind,
            require,
            command,
        } => sandbox_exec(read, write, connect, bind, require, command),
        Command::Optimize => load(&cli.config).and_then(|cfg| {
            init_logging(Mode::Development);
            runtime()?.block_on(optimize(cfg))
        }),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

/// Restrict this process with Landlock, then replace it with `command`.
/// Used to launch each site's PHP-FPM master.
fn sandbox_exec(
    read: Vec<PathBuf>,
    write: Vec<PathBuf>,
    connect: Vec<u16>,
    bind: Vec<u16>,
    require: bool,
    command: Vec<std::ffi::OsString>,
) -> Result<()> {
    use std::os::unix::process::CommandExt;
    let sandbox = nova_security::Sandbox {
        read,
        write,
        connect_tcp: connect,
        bind_tcp: bind,
    };
    let level = sandbox.apply(require).map_err(|e| anyhow::anyhow!("{e}"))?;
    // SAFETY: getuid has no preconditions.
    let uid = unsafe { libc::getuid() };
    eprintln!(
        "nova sandbox: uid={uid} landlock={} connect={:?} exec {}",
        level.as_str(),
        sandbox.connect_tcp,
        command[0].to_string_lossy()
    );
    let err = std::process::Command::new(&command[0])
        .args(&command[1..])
        .exec();
    bail!("exec {}: {err}", command[0].to_string_lossy())
}

/// Run a long-lived command, then exit without waiting for blocking tasks
/// that are still busy (e.g. an image encode): shutdown stays fast.
fn run_to_completion(fut: impl std::future::Future<Output = Result<()>>) -> Result<()> {
    let rt = runtime()?;
    let result = rt.block_on(fut);
    rt.shutdown_timeout(Duration::from_secs(1));
    result
}

fn runtime() -> Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("nova")
        .build()?)
}

fn check(path: &Path, php: bool) -> Result<()> {
    let cfg = load(path)?;
    println!(
        "configuration OK (schema v{}, {} mode)",
        cfg.version, cfg.mode
    );
    println!("listen: {}", cfg.server.listen);
    match nova_core::isolation::effective_mode(&cfg) {
        Ok(m) => println!(
            "isolation: {} (worker uid {})",
            m.as_str(),
            cfg.isolation.worker_uid
        ),
        Err(e) => println!("isolation: cannot start here: {e}"),
    }
    for s in &cfg.sites {
        let php_on = s.php.as_ref().is_some_and(|p| p.enabled);
        println!(
            "site {:<16} uid={} framework={} connect={:?} hosts={:?}{} root={} php={} optimize={} database={}",
            s.name,
            cfg.site_uid(s),
            nova_core::framework::detect(s)
                .map(|f| f.as_str())
                .unwrap_or("-"),
            cfg.site_connect_ports(s),
            s.hosts,
            if s.default { " (default)" } else { "" },
            s.document_root().display(),
            php_on,
            s.optimize && cfg.optimize.enabled,
            s.database.as_ref().map(|d| d.name.as_str()).unwrap_or("-"),
        );
    }
    if php {
        let mode = nova_core::isolation::effective_mode(&cfg)
            .unwrap_or(nova_core::isolation::Effective::Shared);
        for fpm in nova_core::php::fpm_configs(&cfg, mode)? {
            println!(
                "\n# {} (launch: {:?})",
                fpm.conf_path.display(),
                fpm.launch.user
            );
            println!("{}", fpm.render());
        }
    }
    Ok(())
}

async fn optimize(cfg: Config) -> Result<()> {
    let Some(opt) = nova_core::optimizer_for(&cfg) else {
        bail!("optimization is disabled in the configuration");
    };
    let started = std::time::Instant::now();
    let s = opt.scan_all().await;
    println!(
        "{} images: {} processed, {} unchanged, {} failed, {} variants written; {} text assets processed in {:.1}s",
        s.sources,
        s.processed,
        s.reused,
        s.failed,
        s.variants_written,
        s.text_processed,
        started.elapsed().as_secs_f64()
    );
    let kb = |b: u64| b as f64 / 1024.0;
    for site in &cfg.sites {
        if let Some(t) = opt.text_manifest(&site.name) {
            let r = &t.report;
            for (label, x) in [("JavaScript", &r.js), ("CSS", &r.css)] {
                if x.files > 0 {
                    println!(
                        "  {}: {label}: {} files, {:.0} KiB → {:.0} KiB minified → {:.0} KiB brotli",
                        site.name,
                        x.files,
                        kb(x.original),
                        kb(x.minified),
                        kb(x.brotli)
                    );
                }
            }
            for group in &r.duplicates {
                println!("  {}: duplicate content: {}", site.name, group.join(", "));
            }
            for f in &r.unreferenced {
                println!(
                    "  {}: no reference found (review, do not delete blindly): {f}",
                    site.name
                );
            }
            for (f, a) in &t.files {
                if let Some(note) = a.note.as_deref().filter(|n| n.starts_with("not minified")) {
                    println!("  {}: {f}: {note}", site.name);
                }
            }
        }
        if let Some(m) = opt.manifest(&site.name) {
            for (asset, err) in &m.errors {
                println!("  {}: {asset}: {}", site.name, err.message);
            }
            for (asset, w) in &m.warnings {
                println!("  {}: {asset}: warning: {w}", site.name);
            }
        }
    }
    if s.failed > 0 {
        bail!("{} images failed to optimize", s.failed);
    }
    Ok(())
}

/// Minimal HTTP/1.0 probe so the runtime image needs no curl.
fn health(path: &Path, addr: Option<SocketAddr>, live: bool) -> Result<()> {
    let addr = match addr {
        Some(a) => a,
        None => {
            let port = Config::load(path)
                .map(|c| c.server.listen.port())
                .unwrap_or(8080);
            SocketAddr::from(([127, 0, 0, 1], port))
        }
    };
    let target = if live {
        "/_nova/health/live"
    } else {
        "/_nova/health/ready"
    };
    let timeout = Duration::from_secs(3);
    let mut s = TcpStream::connect_timeout(&addr, timeout)
        .with_context(|| format!("connecting to {addr}"))?;
    s.set_read_timeout(Some(timeout))?;
    s.set_write_timeout(Some(timeout))?;
    write!(s, "GET {target} HTTP/1.0\r\nHost: localhost\r\n\r\n")?;
    let mut resp = String::new();
    s.read_to_string(&mut resp)?;
    let status = resp.split_whitespace().nth(1).unwrap_or("");
    let body = resp.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("");
    println!("{body}");
    if status != "200" {
        bail!("health check returned status {status}");
    }
    Ok(())
}
