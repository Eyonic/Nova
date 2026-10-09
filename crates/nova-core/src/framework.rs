//! Framework adapters: make well-known PHP frameworks run on NOVA's
//! read-only code mounts without changing their source.
//!
//! **Laravel** writes to `storage/` and `bootstrap/cache/`. NOVA points both
//! at the site's private persistent state directory using Laravel's own
//! environment hooks (`LARAVEL_STORAGE_PATH`, `APP_*_CACHE`), and seeds that
//! storage from the project's `storage/` directory the first time.

use crate::sites;
use nova_config::{Config, FrameworkSetting, SiteConfig};
use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Framework {
    Laravel,
}

impl Framework {
    pub fn as_str(self) -> &'static str {
        match self {
            Framework::Laravel => "laravel",
        }
    }
}

pub fn detect(site: &SiteConfig) -> Option<Framework> {
    match site.framework {
        FrameworkSetting::None => None,
        FrameworkSetting::Laravel => Some(Framework::Laravel),
        FrameworkSetting::Auto => {
            let laravel = site.path.join("artisan").is_file()
                && site.path.join("vendor/laravel/framework").is_dir();
            laravel.then_some(Framework::Laravel)
        }
    }
}

fn laravel_storage(cfg: &Config, site: &SiteConfig) -> PathBuf {
    sites::site_state_dir(cfg, site).join("storage")
}

fn laravel_bootstrap_cache(cfg: &Config, site: &SiteConfig) -> PathBuf {
    sites::site_state_dir(cfg, site).join("bootstrap-cache")
}

/// Default environment for the framework. Site `env` and database settings
/// are applied afterwards and take precedence.
pub fn env(cfg: &Config, site: &SiteConfig) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    if detect(site) == Some(Framework::Laravel) {
        let cache = laravel_bootstrap_cache(cfg, site);
        env.insert(
            "LARAVEL_STORAGE_PATH".into(),
            laravel_storage(cfg, site).display().to_string(),
        );
        for (key, file) in [
            ("APP_SERVICES_CACHE", "services.php"),
            ("APP_PACKAGES_CACHE", "packages.php"),
            ("APP_CONFIG_CACHE", "config.php"),
            ("APP_ROUTES_CACHE", "routes-v7.php"),
            ("APP_EVENTS_CACHE", "events.php"),
        ] {
            env.insert(key.into(), cache.join(file).display().to_string());
        }
    }
    env
}

const LARAVEL_STORAGE_DIRS: &[&str] = &[
    "app/public",
    "app/private",
    "framework/cache/data",
    "framework/sessions",
    "framework/views",
    "framework/testing",
    "logs",
];

/// Create the framework's writable state. Runs before ownership is applied,
/// so everything created here ends up owned by the site.
pub fn prepare_state(cfg: &Config, site: &SiteConfig) -> io::Result<()> {
    if detect(site) != Some(Framework::Laravel) {
        return Ok(());
    }
    let storage = laravel_storage(cfg, site);
    if !storage.exists() {
        let source = site.path.join("storage");
        if source.is_dir() {
            let n = copy_tree(&source, &storage)?;
            tracing::info!(
                site = site.name,
                files = n,
                "seeded Laravel storage from the project"
            );
        }
    }
    for d in LARAVEL_STORAGE_DIRS {
        std::fs::create_dir_all(storage.join(d))?;
    }
    std::fs::create_dir_all(laravel_bootstrap_cache(cfg, site))?;
    Ok(())
}

/// Copy regular files and directories; symlinks and special files are skipped.
fn copy_tree(from: &Path, to: &Path) -> io::Result<usize> {
    let mut copied = 0;
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let dest = to.join(entry.file_name());
        if ty.is_dir() {
            copied += copy_tree(&entry.path(), &dest)?;
        } else if ty.is_file() {
            std::fs::copy(entry.path(), &dest)?;
            copied += 1;
        }
    }
    Ok(copied)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("nova-fw-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn config(project: &Path, state: &Path) -> Config {
        let c = Config::from_toml(&format!(
            "version = 1\n[paths]\nstate_dir = \"{}\"\n[[site]]\nname = \"shop\"\ndefault = true\npath = \"{}\"\n",
            state.display(),
            project.display()
        ))
        .unwrap();
        c.validate().unwrap();
        c
    }

    #[test]
    fn detects_laravel_and_seeds_storage_once() {
        let base = tmp("laravel");
        let project = base.join("project");
        std::fs::create_dir_all(project.join("vendor/laravel/framework")).unwrap();
        std::fs::create_dir_all(project.join("storage/app/public/products")).unwrap();
        std::fs::write(project.join("artisan"), "<?php").unwrap();
        std::fs::write(project.join("storage/app/public/products/a.jpg"), "img").unwrap();
        let cfg = config(&project, &base.join("state"));
        let site = &cfg.sites[0];

        assert_eq!(detect(site), Some(Framework::Laravel));
        let env = env(&cfg, site);
        assert_eq!(
            env["LARAVEL_STORAGE_PATH"],
            base.join("state/sites/shop/storage").display().to_string()
        );
        assert!(env["APP_PACKAGES_CACHE"].ends_with("bootstrap-cache/packages.php"));

        prepare_state(&cfg, site).unwrap();
        let seeded = base.join("state/sites/shop/storage/app/public/products/a.jpg");
        assert_eq!(std::fs::read_to_string(&seeded).unwrap(), "img");
        assert!(
            base.join("state/sites/shop/storage/framework/views")
                .is_dir()
        );

        // Seeding happens once: later runs keep the state, not the project copy.
        std::fs::write(&seeded, "changed in state").unwrap();
        prepare_state(&cfg, site).unwrap();
        assert_eq!(
            std::fs::read_to_string(&seeded).unwrap(),
            "changed in state"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn plain_php_is_not_laravel() {
        let base = tmp("plain");
        let cfg = config(&base, &base.join("state"));
        assert_eq!(detect(&cfg.sites[0]), None);
        assert!(env(&cfg, &cfg.sites[0]).is_empty());
        let _ = std::fs::remove_dir_all(&base);
    }
}
