//! HTTPS: certificate selection by SNI, automatic certificates (ACME,
//! TLS-ALPN-01), persistent self-signed certificates for local hosts, and
//! the QUIC configuration for HTTP/3.
//!
//! Per host, the first source that applies wins:
//! 1. a `[[server.tls.cert]]` whose `hosts` match (wildcards allowed),
//! 2. ACME, for public host names (once the certificate is issued),
//! 3. the self-signed certificate (local names, or while ACME is pending).

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use nova_config::{Config, TlsConfig};
use nova_http::TlsSettings;
use nova_http::rustls::{
    self, ServerConfig,
    crypto::aws_lc_rs::{default_provider, sign::any_supported_type},
    pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject},
    server::{ClientHello, ResolvesServerCert},
    sign::CertifiedKey,
};
use rustls_acme::{AcmeConfig, ResolvesServerCertAcme, caches::DirCache};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::task::JoinHandle;

pub struct Tls {
    pub settings: TlsSettings,
    pub quic: Option<nova_http::quinn::ServerConfig>,
    pub acme_task: Option<JoinHandle<()>>,
}

pub fn tls_dir(cfg: &Config) -> PathBuf {
    cfg.paths.state_dir.join("tls")
}

/// Host names a public CA can validate (no local or reserved names, no IPs).
pub fn acme_eligible(host: &str) -> bool {
    const LOCAL: [&str; 7] = [
        ".localhost",
        ".local",
        ".test",
        ".internal",
        ".invalid",
        ".example",
        ".lan",
    ];
    host.contains('.')
        && host.parse::<std::net::IpAddr>().is_err()
        && !LOCAL.iter().any(|s| host.ends_with(s))
        && !host.starts_with("*.")
}

fn wildcard_match(pattern: &str, host: &str) -> bool {
    match pattern.strip_prefix("*.") {
        Some(suffix) => host
            .split_once('.')
            .is_some_and(|(label, rest)| !label.is_empty() && rest == suffix),
        None => pattern == host,
    }
}

#[derive(Debug)]
struct Resolver {
    manual: Vec<(Vec<String>, Arc<CertifiedKey>)>,
    acme_hosts: BTreeSet<String>,
    acme: Option<Arc<ResolvesServerCertAcme>>,
    fallback: Option<Arc<CertifiedKey>>,
}

impl ResolvesServerCert for Resolver {
    fn resolve(&self, hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        let sni = hello.server_name().map(|s| s.to_ascii_lowercase());
        if let Some(sni) = &sni {
            if let Some((_, key)) = self
                .manual
                .iter()
                .find(|(hosts, _)| hosts.iter().any(|p| wildcard_match(p, sni)))
            {
                return Some(Arc::clone(key));
            }
            if self.acme_hosts.contains(sni)
                && let Some(acme) = &self.acme
                && let Some(key) = acme.resolve(hello)
            {
                return Some(key);
            }
        }
        self.fallback.clone()
    }
}

fn load_pem(cert: &Path, key: &Path) -> Result<Arc<CertifiedKey>> {
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(cert)
        .with_context(|| format!("reading {}", cert.display()))?
        .collect::<Result<_, _>>()
        .with_context(|| format!("parsing {}", cert.display()))?;
    if certs.is_empty() {
        bail!("{} contains no certificate", cert.display());
    }
    let key = PrivateKeyDer::from_pem_file(key)
        .with_context(|| format!("reading private key {}", key.display()))?;
    let signer = any_supported_type(&key).context("unsupported private key type")?;
    Ok(Arc::new(CertifiedKey::new(certs, signer)))
}

/// Load the persisted self-signed certificate, or create one when the
/// host list changed. Keeping it stable lets browsers remember the exception.
fn self_signed(dir: &Path, hosts: &BTreeSet<String>) -> Result<Arc<CertifiedKey>> {
    let dir = dir.join("self-signed");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let (cert, key, list) = (dir.join("cert.pem"), dir.join("key.pem"), dir.join("hosts"));
    let wanted = hosts.iter().cloned().collect::<Vec<_>>().join("\n");
    if std::fs::read_to_string(&list).is_ok_and(|l| l == wanted)
        && let Ok(k) = load_pem(&cert, &key)
    {
        return Ok(k);
    }
    let generated = rcgen::generate_simple_self_signed(hosts.iter().cloned().collect::<Vec<_>>())
        .context("generating a self-signed certificate")?;
    std::fs::write(&key, generated.signing_key.serialize_pem())?;
    std::fs::write(&cert, generated.cert.pem())?;
    std::fs::write(&list, wanted)?;
    tracing::info!(hosts = hosts.len(), "generated a self-signed certificate");
    load_pem(&cert, &key)
}

/// Build the TLS state. Must run inside the worker (it owns `state/tls`).
pub fn setup(cfg: &Config) -> Result<Tls> {
    let t: &TlsConfig = &cfg.server.tls;
    let dir = tls_dir(cfg);
    let hosts: BTreeSet<String> = cfg
        .sites
        .iter()
        .flat_map(|s| s.hosts.iter())
        .map(|h| h.to_ascii_lowercase())
        .collect();

    let mut manual = Vec::new();
    for c in &t.certs {
        let hosts = c.hosts.iter().map(|h| h.to_ascii_lowercase()).collect();
        manual.push((hosts, load_pem(&c.cert, &c.key)?));
    }
    let covered = |h: &str| {
        manual
            .iter()
            .any(|(hs, _): &(Vec<String>, _)| hs.iter().any(|p| wildcard_match(p, h)))
    };

    let acme_hosts: BTreeSet<String> = if t.acme {
        hosts
            .iter()
            .filter(|h| acme_eligible(h) && !covered(h))
            .cloned()
            .collect()
    } else {
        BTreeSet::new()
    };

    let (acme, challenge, acme_task) = if acme_hosts.is_empty() {
        (None, None, None)
    } else {
        let email = t.acme_email.clone().unwrap_or_default();
        let mut state = AcmeConfig::new(acme_hosts.iter())
            .contact_push(format!("mailto:{email}"))
            .cache(DirCache::new(dir.join("acme")))
            .directory(&t.acme_directory)
            .state();
        let resolver = state.resolver();
        let challenge = state.challenge_rustls_config();
        tracing::info!(hosts = ?acme_hosts, directory = t.acme_directory, "ACME enabled");
        let task = tokio::spawn(async move {
            while let Some(event) = state.next().await {
                match event {
                    Ok(ok) => tracing::info!(event = ?ok, "ACME"),
                    Err(err) => tracing::warn!(error = %err, "ACME"),
                }
            }
        });
        (Some(resolver), Some(challenge), Some(task))
    };

    let fallback = if t.self_signed {
        let mut names: BTreeSet<String> = hosts.iter().filter(|h| !covered(h)).cloned().collect();
        names.insert("localhost".into());
        Some(self_signed(&dir, &names)?)
    } else {
        None
    };

    let resolver = Arc::new(Resolver {
        manual,
        acme_hosts,
        acme,
        fallback,
    });
    let provider = Arc::new(default_provider());
    let mut config = ServerConfig::builder_with_provider(Arc::clone(&provider))
        .with_safe_default_protocol_versions()
        .context("TLS protocol versions")?
        .with_no_client_auth()
        .with_cert_resolver(resolver.clone());
    config.ticketer = rustls::crypto::aws_lc_rs::Ticketer::new().context("TLS session tickets")?;

    let quic = if t.http3 {
        let mut q = config.clone();
        q.alpn_protocols = vec![b"h3".to_vec()];
        q.max_early_data_size = 0;
        let crypto = nova_http::quinn::crypto::rustls::QuicServerConfig::try_from(q)
            .context("QUIC TLS configuration")?;
        let mut sc = nova_http::quinn::ServerConfig::with_crypto(Arc::new(crypto));
        let mut transport = nova_http::quinn::TransportConfig::default();
        transport
            .max_idle_timeout(Some(
                std::time::Duration::from_secs(30)
                    .try_into()
                    .expect("valid idle timeout"),
            ))
            .max_concurrent_bidi_streams(256u32.into());
        sc.transport_config(Arc::new(transport));
        Some(sc)
    } else {
        None
    };
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];

    Ok(Tls {
        settings: TlsSettings {
            config: Arc::new(config),
            challenge,
        },
        quic,
        acme_task,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eligibility() {
        assert!(acme_eligible("example.com"));
        assert!(acme_eligible("www.shop.example.org"));
        assert!(!acme_eligible("localhost"));
        assert!(!acme_eligible("xxlgifts.localhost"));
        assert!(!acme_eligible("app.test"));
        assert!(!acme_eligible("10.0.0.1"));
        assert!(!acme_eligible("*.example.com"));
    }

    #[test]
    fn wildcards() {
        assert!(wildcard_match("*.example.com", "www.example.com"));
        assert!(!wildcard_match("*.example.com", "example.com"));
        assert!(!wildcard_match("*.example.com", "a.b.example.com"));
        assert!(wildcard_match("example.com", "example.com"));
    }

    #[test]
    fn self_signed_is_persistent() {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let dir = std::env::temp_dir().join(format!("nova-tls-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let hosts: BTreeSet<String> = ["localhost".to_string(), "a.localhost".to_string()].into();
        self_signed(&dir, &hosts).unwrap();
        let first = std::fs::read(dir.join("self-signed/cert.pem")).unwrap();
        self_signed(&dir, &hosts).unwrap();
        assert_eq!(
            first,
            std::fs::read(dir.join("self-signed/cert.pem")).unwrap()
        );
        let more: BTreeSet<String> = ["localhost".to_string(), "b.localhost".to_string()].into();
        self_signed(&dir, &more).unwrap();
        assert_ne!(
            first,
            std::fs::read(dir.join("self-signed/cert.pem")).unwrap()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
