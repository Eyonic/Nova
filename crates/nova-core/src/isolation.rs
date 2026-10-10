//! Isolation planning: which mode applies, who owns which directory, and
//! the Landlock policy for each site's PHP and for the HTTP worker.
//!
//! Strict mode process tree (container started as root with only
//! CHOWN, DAC_OVERRIDE, FOWNER, SETUID, SETGID and KILL):
//!
//! ```text
//! nova serve      supervisor, root: prepares ownership, launches and stops processes
//! ├─ nova worker  worker_uid, no capabilities, Landlock: HTTP + optimizer
//! └─ php-fpm × N  one master per site, site uid, no capabilities, Landlock
//! ```

use crate::{framework, sites};
use anyhow::{Context, Result, bail};
use nova_config::{Config, IsolationMode, SiteConfig};
use nova_security::{Sandbox, chown_tree, ensure_dir, is_root, safe_devices, system_read_paths};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effective {
    /// Per-site uids + Landlock; NOVA was started as root.
    Strict,
    /// One uid for everything; Landlock still separates sites.
    Shared,
}

impl Effective {
    pub fn as_str(self) -> &'static str {
        match self {
            Effective::Strict => "strict",
            Effective::Shared => "shared",
        }
    }
}

pub fn effective_mode(cfg: &Config) -> Result<Effective> {
    let root = is_root();
    Ok(match (cfg.isolation.mode, root) {
        (IsolationMode::Strict, true) | (IsolationMode::Auto, true) => Effective::Strict,
        (IsolationMode::Strict, false) => {
            bail!(
                "isolation.mode = \"strict\" requires starting NOVA as root (it drops privileges itself)"
            )
        }
        (IsolationMode::Shared, true) => {
            bail!(
                "refusing to run every site as root; use isolation.mode = \"strict\" or start as a non-root user"
            )
        }
        (_, false) => Effective::Shared,
    })
}

pub fn worker_ids(cfg: &Config) -> (u32, u32) {
    (cfg.isolation.worker_uid, cfg.isolation.worker_uid)
}

/// Create runtime and state directories with the right owners and modes.
pub fn prepare_dirs(cfg: &Config, mode: Effective) -> Result<()> {
    let state = &cfg.paths.state_dir;
    let run = &cfg.paths.run_dir;
    let (wuid, wgid) = worker_ids(cfg);
    let ctx = |p: &PathBuf| format!("preparing {}", p.display());

    if mode == Effective::Shared {
        for site in &cfg.sites {
            let sdir = sites::site_state_dir(cfg, site);
            for d in [
                sdir.join("tmp"),
                sdir.join("sessions"),
                sites::php_run_dir(cfg, site),
            ] {
                std::fs::create_dir_all(&d).with_context(|| ctx(&d))?;
            }
            framework::prepare_state(cfg, site).with_context(|| ctx(&sdir))?;
        }
        std::fs::create_dir_all(state.join("optimize"))?;
        std::fs::create_dir_all(state.join("tls"))?;
        std::fs::create_dir_all(run.join("conf"))?;
        return Ok(());
    }

    for d in [
        state.clone(),
        state.join("sites"),
        run.clone(),
        run.join("conf"),
        run.join("php"),
    ] {
        ensure_dir(&d, 0, 0, 0o755).with_context(|| ctx(&d))?;
    }
    let opt = state.join("optimize");
    ensure_dir(&opt, wuid, wgid, 0o755).with_context(|| ctx(&opt))?;
    let n = chown_tree(&opt, wuid, wgid).with_context(|| ctx(&opt))?;
    if n > 0 {
        tracing::info!(changed = n, "re-owned optimizer state for the worker");
    }
    // Certificates and ACME account keys: the worker's alone.
    let tls = crate::tls::tls_dir(cfg);
    ensure_dir(&tls, wuid, wgid, 0o700).with_context(|| ctx(&tls))?;
    chown_tree(&tls, wuid, wgid).with_context(|| ctx(&tls))?;
    for site in &cfg.sites {
        let uid = cfg.site_uid(site);
        let sdir = sites::site_state_dir(cfg, site);
        ensure_dir(&sdir, uid, uid, 0o700).with_context(|| ctx(&sdir))?;
        for sub in ["tmp", "sessions"] {
            let d = sdir.join(sub);
            ensure_dir(&d, uid, uid, 0o700).with_context(|| ctx(&d))?;
        }
        framework::prepare_state(cfg, site).with_context(|| ctx(&sdir))?;
        let n = chown_tree(&sdir, uid, uid).with_context(|| ctx(&sdir))?;
        if n > 0 {
            tracing::info!(site = site.name, uid, changed = n, "re-owned site state");
        }
        // Only the site (owner) and the worker (group, traverse only) can reach the socket.
        let rdir = sites::php_run_dir(cfg, site);
        ensure_dir(&rdir, uid, wgid, 0o710).with_context(|| ctx(&rdir))?;
    }
    Ok(())
}

/// Landlock policy for one site's PHP: read the system and its own
/// project, write only its own state and socket directory, connect only to
/// its allowed TCP ports, listen on nothing.
pub fn site_sandbox(cfg: &Config, site: &SiteConfig) -> Sandbox {
    let mut read = system_read_paths();
    read.push(site.path.clone());
    read.push(sites::php_conf_path(cfg, site));
    read.extend(site.isolation.allow_read.iter().cloned());
    let mut write = safe_devices();
    // Resolved by the sandbox wrapper, which exec's into the FPM master
    // (same pid): only the master's own descriptors, used for its stderr log.
    write.push(PathBuf::from("/proc/self/fd"));
    write.push(sites::site_state_dir(cfg, site));
    write.push(sites::php_run_dir(cfg, site));
    Sandbox {
        read,
        write,
        connect_tcp: cfg.site_connect_ports(site),
        bind_tcp: vec![],
    }
}

/// TCP port of the ACME directory URL (`https://host[:port]/...`).
fn acme_port(directory: &str) -> u16 {
    directory
        .strip_prefix("https://")
        .and_then(|r| r.split('/').next())
        .and_then(|authority| authority.rsplit_once(':'))
        .filter(|(host, _)| !host.is_empty())
        .and_then(|(_, port)| port.parse().ok())
        .unwrap_or(443)
}

/// Landlock policy for the HTTP worker: read every site and the config,
/// write optimizer and TLS state, reach PHP sockets, listen on the HTTP(S)
/// ports and connect only to database ports (readiness checks) and, with
/// ACME, to the certificate authority (443).
pub fn worker_sandbox(cfg: &Config, config_path: &std::path::Path) -> Sandbox {
    let mut read = system_read_paths();
    read.push(config_path.to_path_buf());
    read.extend(cfg.sites.iter().map(|s| s.path.clone()));
    read.push(PathBuf::from("/proc/self"));
    let tls = &cfg.server.tls;
    for c in &tls.certs {
        read.push(c.cert.clone());
        read.push(c.key.clone());
    }
    read.extend(tls.acme_ca_file.clone());
    read.extend(
        cfg.sites
            .iter()
            .filter_map(|s| s.auth.as_ref()?.users_file.clone()),
    );
    let mut write = safe_devices();
    write.push(cfg.paths.state_dir.join("optimize"));
    write.push(crate::tls::tls_dir(cfg));
    write.push(cfg.paths.run_dir.join("php"));
    let mut connect: Vec<u16> = cfg.services.database.values().map(|d| d.port).collect();
    // Reverse-proxy upstreams are reached by the worker itself.
    connect.extend(
        cfg.sites
            .iter()
            .filter_map(|s| s.proxy.as_ref()?.upstream_addr().ok())
            .map(|(_, port)| port),
    );
    let mut bind = vec![cfg.server.listen.port()];
    if tls.enabled {
        bind.push(tls.listen.port());
        if tls.acme {
            connect.push(acme_port(&tls.acme_directory));
        }
    }
    connect.sort_unstable();
    connect.dedup();
    Sandbox {
        read,
        write,
        connect_tcp: connect,
        bind_tcp: bind,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acme_directory_ports() {
        assert_eq!(
            acme_port("https://acme-v02.api.letsencrypt.org/directory"),
            443
        );
        assert_eq!(acme_port("https://localhost:14000/dir"), 14000);
        assert_eq!(
            acme_port("https://ca.internal:9000/acme/acme/directory"),
            9000
        );
        assert_eq!(acme_port("https://[::1]:9443/dir"), 9443);
    }
}
