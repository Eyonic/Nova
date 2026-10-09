//! Building and running one sandboxed PHP-FPM master per site.

use crate::isolation::{self, Effective};
use crate::sites;
use anyhow::{Context, Result};
use nova_config::{Config, SiteConfig};
use nova_runtime_php::{Fpm, FpmConfig, Launch, PoolSpec};
use std::ffi::OsString;
use std::time::Duration;

pub fn pool_spec(cfg: &Config, site: &SiteConfig) -> Result<Option<PoolSpec>> {
    let Some(php) = site.php.as_ref().filter(|p| p.enabled) else {
        return Ok(None);
    };
    Ok(Some(PoolSpec {
        site: site.name.clone(),
        socket: sites::php_socket(cfg, site),
        project_dir: site.path.clone(),
        state_dir: sites::site_state_dir(cfg, site),
        env: sites::site_env(cfg, site).map_err(anyhow::Error::msg)?,
        max_children: php.max_children,
        max_requests: php.max_requests,
        memory_limit: php.memory_limit.to_php(),
        timeout_secs: php.timeout_secs,
        upload_max: cfg.server.max_request_body.to_php(),
        disable_functions: php
            .disable_functions
            .clone()
            .unwrap_or_else(|| cfg.php.disable_functions.clone()),
        ini: php.ini.clone(),
        display_errors: cfg.mode.is_dev(),
        // Landlock (required by default) enforces the same boundary in the kernel.
        open_basedir: !cfg.isolation.require_landlock,
    }))
}

/// FPM configuration for every PHP-enabled site, including how to launch it.
pub fn fpm_configs(cfg: &Config, mode: Effective) -> Result<Vec<FpmConfig>> {
    let nova = std::env::current_exe().context("locating the nova binary")?;
    let mut out = Vec::new();
    for site in &cfg.sites {
        let Some(pool) = pool_spec(cfg, site)? else {
            continue;
        };
        let mut wrapper: Vec<OsString> = vec![nova.clone().into(), "sandbox".into()];
        wrapper.extend(
            isolation::site_sandbox(cfg, site)
                .to_args()
                .into_iter()
                .map(OsString::from),
        );
        if cfg.isolation.require_landlock {
            wrapper.push("--require".into());
        }
        wrapper.push("--".into());
        let uid = cfg.site_uid(site);
        out.push(FpmConfig {
            binary: cfg.php.fpm_binary.clone(),
            conf_path: sites::php_conf_path(cfg, site),
            run_dir: sites::php_run_dir(cfg, site),
            pool,
            shutdown_grace: Duration::from_secs(cfg.server.shutdown_grace_secs),
            launch: Launch {
                user: (mode == Effective::Strict).then_some((uid, uid)),
                wrapper,
            },
        });
    }
    Ok(out)
}

/// Start every site's PHP. A site that fails to start is an error: running
/// with a missing site would silently serve 503s.
pub async fn start_all(cfg: &Config, mode: Effective) -> Result<Vec<Fpm>> {
    let mut started = Vec::new();
    for fc in fpm_configs(cfg, mode)? {
        let site = fc.pool.site.clone();
        let fpm = Fpm::start(fc)
            .await
            .with_context(|| format!("starting PHP for site {site:?}"))?;
        started.push((site, fpm));
    }
    for (site, fpm) in &started {
        if !fpm.wait_ready(Duration::from_secs(20)).await {
            tracing::warn!(
                site,
                "PHP not ready after 20s; continuing, readiness will report it"
            );
        }
    }
    Ok(started.into_iter().map(|(_, f)| f).collect())
}

pub async fn stop_all(fpms: Vec<Fpm>) {
    futures_util::future::join_all(fpms.into_iter().map(Fpm::shutdown)).await;
}
