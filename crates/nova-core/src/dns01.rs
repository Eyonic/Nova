//! ACME DNS-01 (`server.tls.acme_challenge = "dns-01"`).
//!
//! The CA checks a TXT record under `_acme-challenge.<host>` instead of
//! connecting to NOVA, so this works for wildcard hosts (`*.shop.example`)
//! and for servers no CA can reach (private networks, behind CDNs).
//!
//! The supervisor runs the order (it needs no listener): it sets the
//! records through the configured provider (Cloudflare's API, or a hook
//! program for any other DNS host), waits for propagation, lets the CA
//! validate, and writes the certificate to `<state>/tls/dns01/`. The HTTP
//! worker picks new files up within 30 s without a restart. Certificates
//! are renewed after 60 days (Let's Encrypt issues 90-day certificates).

use crate::isolation::Effective;
use anyhow::{Context, Result, anyhow, bail};
use bytes::Bytes;
use http::{Method, Request};
use http_body_util::{BodyExt, Full};
use instant_acme::{
    Account, AccountCredentials, AuthorizationStatus, ChallengeType, Identifier, NewAccount,
    NewOrder, OrderStatus, RetryPolicy,
};
use nova_config::{AcmeChallenge, Config, DnsProvider};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Renew certificates this old.
const RENEW_AFTER: Duration = Duration::from_secs(60 * 86_400);
/// How often the configuration and certificate age are checked.
const CHECK_EVERY: Duration = Duration::from_secs(600);
/// After a failed order.
const RETRY_AFTER: Duration = Duration::from_secs(3600);

pub fn dir(cfg: &Config) -> PathBuf {
    crate::tls::tls_dir(cfg).join("dns01")
}

pub fn enabled(cfg: &Config) -> bool {
    let t = &cfg.server.tls;
    t.enabled && t.acme && t.acme_challenge == AcmeChallenge::Dns01
}

/// Names to certify: public site hosts and wildcards, minus those covered
/// by a configured certificate or by a wildcard in the same set.
pub fn hosts(cfg: &Config) -> BTreeSet<String> {
    let manual: Vec<&String> = cfg
        .server
        .tls
        .certs
        .iter()
        .flat_map(|c| c.hosts.iter())
        .collect();
    let all: BTreeSet<String> = cfg
        .sites
        .iter()
        .flat_map(|s| s.hosts.iter())
        .map(|h| h.to_ascii_lowercase())
        .filter(|h| crate::tls::acme_eligible(h.strip_prefix("*.").unwrap_or(h)))
        .filter(|h| {
            !manual
                .iter()
                .any(|p| crate::tls::wildcard_match(&p.to_ascii_lowercase(), h))
        })
        .collect();
    all.iter()
        .filter(|h| {
            h.starts_with("*.")
                || !h
                    .split_once('.')
                    .is_some_and(|(_, rest)| all.contains(&format!("*.{rest}")))
        })
        .cloned()
        .collect()
}

/// Keep the DNS-01 certificate issued and fresh until the process ends.
/// Re-reads the configuration so reloads that add hosts are honoured.
pub async fn run(config_path: PathBuf, mode: Effective) {
    loop {
        let wait = match Config::load_env(&config_path) {
            Ok(cfg) if enabled(&cfg) => match ensure(&cfg, mode).await {
                Ok(()) => CHECK_EVERY,
                Err(e) => {
                    tracing::warn!(
                        error = format!("{e:#}"),
                        "DNS-01 certificate not issued; retrying in an hour"
                    );
                    RETRY_AFTER
                }
            },
            Ok(_) => return,
            Err(e) => {
                tracing::warn!(error = %e, "DNS-01: cannot read the configuration");
                CHECK_EVERY
            }
        };
        tokio::time::sleep(wait).await;
    }
}

async fn ensure(cfg: &Config, mode: Effective) -> Result<()> {
    let names = hosts(cfg);
    if names.is_empty() {
        return Ok(());
    }
    let dir = dir(cfg);
    let wanted = names.iter().cloned().collect::<Vec<_>>().join("\n");
    let current = std::fs::read_to_string(dir.join("hosts")).unwrap_or_default();
    let age = std::fs::read_to_string(dir.join("issued"))
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(|t| unix_now().saturating_sub(t));
    if current == wanted
        && dir.join("cert.pem").exists()
        && age.is_some_and(|a| a < RENEW_AFTER.as_secs())
    {
        return Ok(());
    }
    tracing::info!(hosts = ?names, "DNS-01: requesting a certificate");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let (cert, key) = order(cfg, mode, &names, &dir).await?;
    let owner = (mode == Effective::Strict).then(|| crate::isolation::worker_ids(cfg));
    write(&dir.join("key.pem"), key.as_bytes(), 0o600, owner)?;
    write(&dir.join("cert.pem"), cert.as_bytes(), 0o644, owner)?;
    write(&dir.join("hosts"), wanted.as_bytes(), 0o644, owner)?;
    write(
        &dir.join("issued"),
        unix_now().to_string().as_bytes(),
        0o644,
        owner,
    )?;
    tracing::info!(hosts = names.len(), "DNS-01: certificate issued");
    Ok(())
}

fn write(path: &Path, data: &[u8], mode: u32, owner: Option<(u32, u32)>) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let tmp = path.with_extension("new");
    let _ = std::fs::remove_file(&tmp);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(&tmp)
        .with_context(|| format!("writing {}", tmp.display()))?;
    f.write_all(data)?;
    f.sync_all()?;
    if let Some((uid, gid)) = owner {
        std::os::unix::fs::chown(&tmp, Some(uid), Some(gid))?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Run one ACME order; returns (certificate chain PEM, private key PEM).
async fn order(
    cfg: &Config,
    mode: Effective,
    names: &BTreeSet<String>,
    dir: &Path,
) -> Result<(String, String)> {
    let t = &cfg.server.tls;
    let account = account(cfg, dir).await?;
    let dns = Dns::new(cfg, mode)?;
    let identifiers: Vec<Identifier> = names.iter().map(|n| Identifier::Dns(n.clone())).collect();
    let mut order = account
        .new_order(&NewOrder::new(&identifiers))
        .await
        .context("creating the ACME order")?;

    // Set every record first, wait once for propagation, then let the CA look.
    let mut records = Vec::new();
    let mut auths = order.authorizations();
    while let Some(authz) = auths.next().await {
        let mut authz = authz.context("fetching an authorization")?;
        if authz.status != AuthorizationStatus::Pending {
            continue;
        }
        let challenge = authz
            .challenge(ChallengeType::Dns01)
            .ok_or_else(|| anyhow!("the CA offers no dns-01 challenge"))?;
        let Identifier::Dns(domain) = challenge.identifier().identifier else {
            bail!("unexpected identifier type");
        };
        let name = format!("_acme-challenge.{domain}");
        let value = challenge.key_authorization().dns_value();
        let id = dns
            .set(&name, &value)
            .await
            .with_context(|| format!("setting TXT {name}"))?;
        records.push((name, value, id));
    }
    let result = async {
        tokio::time::sleep(Duration::from_secs(t.acme_dns_propagation_secs)).await;
        let mut auths = order.authorizations();
        while let Some(authz) = auths.next().await {
            let mut authz = authz?;
            if authz.status != AuthorizationStatus::Pending {
                continue;
            }
            if let Some(mut c) = authz.challenge(ChallengeType::Dns01) {
                c.set_ready().await?;
            }
        }
        let status = order.poll_ready(&RetryPolicy::default()).await?;
        if status != OrderStatus::Ready {
            bail!("the CA did not validate the records (order {status:?})");
        }
        let key = order.finalize().await?;
        let cert = order.poll_certificate(&RetryPolicy::default()).await?;
        Ok((cert, key))
    }
    .await;
    for (name, value, id) in &records {
        if let Err(e) = dns.clear(name, value, id).await {
            tracing::warn!(
                record = name,
                error = format!("{e:#}"),
                "DNS-01: could not remove the TXT record"
            );
        }
    }
    result
}

async fn account(cfg: &Config, dir: &Path) -> Result<Account> {
    let t = &cfg.server.tls;
    let builder = || match &t.acme_ca_file {
        Some(ca) => Account::builder_with_root(ca),
        None => Account::builder(),
    };
    let path = dir.join("account.json");
    if let Ok(json) = std::fs::read_to_string(&path)
        && let Ok(creds) = serde_json::from_str::<AccountCredentials>(&json)
    {
        return builder()?
            .from_credentials(creds)
            .await
            .context("restoring the ACME account");
    }
    let contact = format!("mailto:{}", t.acme_email.as_deref().unwrap_or_default());
    let (account, creds) = builder()?
        .create(
            &NewAccount {
                contact: &[&contact],
                terms_of_service_agreed: true,
                only_return_existing: false,
            },
            t.acme_directory.clone(),
            None,
        )
        .await
        .context("creating the ACME account")?;
    write(
        &path,
        serde_json::to_string(&creds)?.as_bytes(),
        0o600,
        None,
    )?;
    Ok(account)
}

enum Dns {
    Cloudflare {
        api: String,
        token: String,
        client: Box<
            hyper_util::client::legacy::Client<
                hyper_rustls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>,
                Full<Bytes>,
            >,
        >,
    },
    Hook {
        program: PathBuf,
        user: Option<(u32, u32)>,
    },
}

impl Dns {
    fn new(cfg: &Config, mode: Effective) -> Result<Self> {
        let t = &cfg.server.tls;
        Ok(match t.acme_dns_provider {
            DnsProvider::Cloudflare => {
                let token = std::env::var(&t.acme_dns_token_env)
                    .with_context(|| format!("{} is not set", t.acme_dns_token_env))?;
                let https = hyper_rustls::HttpsConnectorBuilder::new()
                    .with_webpki_roots()
                    .https_or_http()
                    .enable_http1()
                    .build();
                Dns::Cloudflare {
                    api: t.acme_dns_api.trim_end_matches('/').to_string(),
                    token,
                    client: Box::new(
                        hyper_util::client::legacy::Client::builder(
                            hyper_util::rt::TokioExecutor::new(),
                        )
                        .build(https),
                    ),
                }
            }
            DnsProvider::Hook => Dns::Hook {
                program: t
                    .acme_dns_hook
                    .clone()
                    .context("acme_dns_hook is not set")?,
                // The hook runs as the HTTP worker's user, not as root.
                user: (mode == Effective::Strict).then(|| crate::isolation::worker_ids(cfg)),
            },
        })
    }

    /// Create the TXT record; returns what `clear` needs to remove it.
    async fn set(&self, name: &str, value: &str) -> Result<String> {
        match self {
            Dns::Hook { .. } => {
                self.hook("set", name, value).await?;
                Ok(String::new())
            }
            Dns::Cloudflare { .. } => {
                let zone = self.cloudflare_zone(name).await?;
                let body = serde_json::json!({
                    "type": "TXT", "name": name, "content": value, "ttl": 60,
                    "comment": "ACME challenge (NOVA)",
                });
                let resp = self
                    .cloudflare(
                        Method::POST,
                        &format!("/zones/{zone}/dns_records"),
                        Some(body),
                    )
                    .await?;
                let id = resp["result"]["id"]
                    .as_str()
                    .ok_or_else(|| anyhow!("no record id in Cloudflare's answer"))?;
                Ok(format!("{zone}/{id}"))
            }
        }
    }

    async fn clear(&self, name: &str, value: &str, id: &str) -> Result<()> {
        match self {
            Dns::Hook { .. } => self.hook("clear", name, value).await,
            Dns::Cloudflare { .. } => {
                let (zone, record) = id.split_once('/').unwrap_or_default();
                self.cloudflare(
                    Method::DELETE,
                    &format!("/zones/{zone}/dns_records/{record}"),
                    None,
                )
                .await
                .map(|_| ())
            }
        }
    }

    async fn hook(&self, action: &str, name: &str, value: &str) -> Result<()> {
        let Dns::Hook { program, user } = self else {
            unreachable!()
        };
        let mut cmd = tokio::process::Command::new(program);
        cmd.args([action, name, value])
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true);
        if let Some((uid, gid)) = user {
            cmd.uid(*uid).gid(*gid);
        }
        let out = tokio::time::timeout(Duration::from_secs(120), cmd.output())
            .await
            .context("the DNS hook did not finish within 120 s")?
            .with_context(|| format!("running {}", program.display()))?;
        if !out.status.success() {
            bail!(
                "{} {action} failed ({}): {}",
                program.display(),
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(())
    }

    /// The Cloudflare zone holding `name`: the longest suffix that is a zone.
    async fn cloudflare_zone(&self, name: &str) -> Result<String> {
        let labels: Vec<&str> = name.trim_end_matches('.').split('.').collect();
        for i in 1..labels.len().saturating_sub(1) {
            let candidate = labels[i..].join(".");
            let resp = self
                .cloudflare(Method::GET, &format!("/zones?name={candidate}"), None)
                .await?;
            if let Some(id) = resp["result"][0]["id"].as_str() {
                return Ok(id.to_string());
            }
        }
        bail!("no Cloudflare zone found for {name}")
    }

    async fn cloudflare(
        &self,
        method: Method,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> Result<serde_json::Value> {
        let Dns::Cloudflare { api, token, client } = self else {
            unreachable!()
        };
        let req = Request::builder()
            .method(method)
            .uri(format!("{api}{path}"))
            .header(http::header::AUTHORIZATION, format!("Bearer {token}"))
            .header(http::header::CONTENT_TYPE, "application/json")
            .body(Full::new(Bytes::from(
                body.map(|b| b.to_string()).unwrap_or_default(),
            )))?;
        let resp = tokio::time::timeout(Duration::from_secs(30), client.request(req))
            .await
            .context("Cloudflare API timed out")??;
        let status = resp.status();
        let bytes = resp.into_body().collect().await?.to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or_default();
        if !status.is_success() || json["success"] == false {
            bail!("Cloudflare API {status}: {}", json["errors"]);
        }
        Ok(json)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_selection() {
        let cfg = Config::from_toml(
            r#"
version = 1
[server.tls]
enabled = true
acme = true
acme_email = "ops@shop.example.com"
acme_challenge = "dns-01"
[[server.tls.cert]]
hosts = ["legacy.example.com"]
cert = "/c.pem"
key = "/k.pem"
[[site]]
name = "a"
default = true
path = "/srv/a"
hosts = ["shop.example.com", "*.shop.example.com", "eu.shop.example.com", "legacy.example.com", "dev.localhost", "*.test"]
"#,
        )
        .unwrap();
        assert!(enabled(&cfg));
        let h: Vec<String> = hosts(&cfg).into_iter().collect();
        assert_eq!(h, vec!["*.shop.example.com", "shop.example.com"]);
    }
}
