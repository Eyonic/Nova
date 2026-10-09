//! Runtime view of configured sites and Host-based routing.

use nova_config::{Config, Mode, SiteConfig};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug)]
pub struct Site {
    pub name: String,
    /// Canonical document root; every served path must stay below it.
    pub root: PathBuf,
    pub project_dir: PathBuf,
    pub php: Option<SitePhp>,
    pub optimize: bool,
}

#[derive(Debug)]
pub struct SitePhp {
    pub socket: PathBuf,
    /// URL path of the front controller, e.g. `/index.php`.
    pub front_controller: Option<String>,
    pub timeout: Duration,
}

pub struct Sites {
    sites: Vec<Site>,
    by_host: HashMap<String, usize>,
    default: Option<usize>,
}

impl Sites {
    pub fn new(cfg: &Config) -> Result<Self, String> {
        let mut sites = Vec::new();
        let mut by_host = HashMap::new();
        let mut default = None;
        for (i, s) in cfg.sites.iter().enumerate() {
            let root = s.document_root();
            let root = std::fs::canonicalize(&root).map_err(|e| {
                format!(
                    "site {:?}: document root {} is not accessible: {e}",
                    s.name,
                    root.display()
                )
            })?;
            let project_dir = std::fs::canonicalize(&s.path).map_err(|e| {
                format!(
                    "site {:?}: path {} is not accessible: {e}",
                    s.name,
                    s.path.display()
                )
            })?;
            let php = s.php.as_ref().filter(|p| p.enabled).map(|p| SitePhp {
                socket: php_socket(cfg, s),
                front_controller: p
                    .front_controller
                    .as_ref()
                    .map(|f| format!("/{}", f.trim_start_matches('/'))),
                timeout: Duration::from_secs(p.timeout_secs),
            });
            for h in &s.hosts {
                by_host.insert(h.to_ascii_lowercase(), i);
            }
            if s.default {
                default = Some(i);
            }
            sites.push(Site {
                name: s.name.clone(),
                root,
                project_dir,
                php,
                optimize: s.optimize,
            });
        }
        Ok(Self {
            sites,
            by_host,
            default,
        })
    }

    /// Route by Host header (port and trailing dot ignored).
    pub fn lookup(&self, host: Option<&str>) -> Option<&Site> {
        let idx = host
            .map(|h| strip_port(h).trim_end_matches('.').to_ascii_lowercase())
            .and_then(|h| self.by_host.get(&h).copied())
            .or(self.default)?;
        self.sites.get(idx)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Site> {
        self.sites.iter()
    }
}

pub fn strip_port(host: &str) -> &str {
    if host.starts_with('[') {
        // IPv6 literal: [::1]:8080
        return host
            .split_once(']')
            .map(|(h, _)| &host[..h.len() + 1])
            .unwrap_or(host);
    }
    host.rsplit_once(':')
        .filter(|(_, p)| p.bytes().all(|b| b.is_ascii_digit()))
        .map(|(h, _)| h)
        .unwrap_or(host)
}

/// Per-site socket directory: owner = site, group = worker, mode 0710.
pub fn php_run_dir(cfg: &Config, site: &SiteConfig) -> PathBuf {
    cfg.paths.run_dir.join("php").join(&site.name)
}

pub fn php_socket(cfg: &Config, site: &SiteConfig) -> PathBuf {
    php_run_dir(cfg, site).join("fpm.sock")
}

/// Generated FPM configuration (root-owned, world-readable, no secrets).
pub fn php_conf_path(cfg: &Config, site: &SiteConfig) -> PathBuf {
    cfg.paths
        .run_dir
        .join("conf")
        .join(format!("{}.conf", site.name))
}

pub fn site_state_dir(cfg: &Config, site: &SiteConfig) -> PathBuf {
    cfg.paths.state_dir.join("sites").join(&site.name)
}

/// Environment for a site's PHP workers: configured values, values pulled
/// from NOVA's environment, and database connection details.
pub fn site_env(cfg: &Config, site: &SiteConfig) -> Result<BTreeMap<String, String>, String> {
    let mut env = BTreeMap::new();
    env.insert("NOVA_SITE".to_string(), site.name.clone());
    env.insert("NOVA_MODE".to_string(), cfg.mode.to_string());
    env.insert(
        "APP_ENV".to_string(),
        if cfg.mode == Mode::Development {
            "local"
        } else {
            "production"
        }
        .to_string(),
    );
    // Framework defaults first, so the site's own settings override them.
    env.extend(crate::framework::env(cfg, site));
    for (k, v) in &site.env {
        env.insert(k.clone(), v.clone());
    }
    for (k, var) in &site.env_from {
        let v = std::env::var(var).map_err(|_| {
            format!(
                "site {:?}: environment variable {var} (for {k}) is not set",
                site.name
            )
        })?;
        env.insert(k.clone(), v);
    }
    if let Some(db) = &site.database {
        let svc = &cfg.services.database[&db.service];
        let password = std::env::var(&db.password_env).map_err(|_| {
            format!(
                "site {:?}: database password variable {} is not set",
                site.name, db.password_env
            )
        })?;
        env.insert("DB_CONNECTION".into(), svc.driver.connection_name().into());
        env.insert("DB_HOST".into(), svc.host.clone());
        env.insert("DB_PORT".into(), svc.port.to_string());
        env.insert("DB_DATABASE".into(), db.name.clone());
        env.insert("DB_USERNAME".into(), db.user.clone());
        env.insert("DB_PASSWORD".into(), password);
    }
    Ok(env)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_ports() {
        assert_eq!(strip_port("example.test:8080"), "example.test");
        assert_eq!(strip_port("example.test"), "example.test");
        assert_eq!(strip_port("[::1]:8080"), "[::1]");
        assert_eq!(strip_port("[::1]"), "[::1]");
    }
}
