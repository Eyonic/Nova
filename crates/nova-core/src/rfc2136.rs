//! RFC 2136 dynamic DNS updates signed with TSIG (RFC 8945, HMAC-SHA256),
//! for ACME DNS-01 with any standard DNS server: BIND, PowerDNS, Knot,
//! Technitium, and hosted DNS that accepts signed updates.
//!
//! One UPDATE message per change, over TCP: add or delete one TXT record.

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const TYPE_SOA: u16 = 6;
const TYPE_TXT: u16 = 16;
const TYPE_TSIG: u16 = 250;
const CLASS_IN: u16 = 1;
const CLASS_NONE: u16 = 254;
const CLASS_ANY: u16 = 255;
const OPCODE_UPDATE: u16 = 5 << 11;
const ALGORITHM: &str = "hmac-sha256.";
const FUDGE: u16 = 300;

pub struct Updater {
    pub server: String,
    pub zone: String,
    pub key_name: String,
    pub secret: Vec<u8>,
}

impl Updater {
    pub async fn add_txt(&self, name: &str, value: &str, ttl: u32) -> Result<()> {
        self.send(name, value, CLASS_IN, ttl).await
    }

    /// Delete exactly this TXT record (class NONE, RFC 2136 §2.5.4).
    pub async fn delete_txt(&self, name: &str, value: &str) -> Result<()> {
        self.send(name, value, CLASS_NONE, 0).await
    }

    async fn send(&self, name: &str, value: &str, class: u16, ttl: u32) -> Result<()> {
        let id = rand_u16();
        let msg = self.message(id, name, value, class, ttl, unix_now())?;
        let mut stream = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            tokio::net::TcpStream::connect(&self.server),
        )
        .await
        .context("DNS server timed out")?
        .with_context(|| format!("connecting to {}", self.server))?;
        let mut framed = (msg.len() as u16).to_be_bytes().to_vec();
        framed.extend_from_slice(&msg);
        stream.write_all(&framed).await?;
        let mut len = [0u8; 2];
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            stream.read_exact(&mut len),
        )
        .await
        .context("no answer to the DNS update")??;
        let mut resp = vec![0u8; u16::from_be_bytes(len) as usize];
        stream.read_exact(&mut resp).await?;
        if resp.len() < 12 || u16::from_be_bytes([resp[0], resp[1]]) != id {
            bail!("malformed answer to the DNS update");
        }
        let rcode = resp[3] & 0x0f;
        if rcode != 0 {
            bail!(
                "DNS server refused the update: {}",
                match rcode {
                    5 => "REFUSED (key not allowed to update this zone?)",
                    9 => "NOTAUTH (TSIG key or signature not accepted)",
                    10 => "NOTZONE (name outside the zone)",
                    2 => "SERVFAIL",
                    _ => "error",
                }
            );
        }
        Ok(())
    }

    /// The signed UPDATE message.
    fn message(
        &self,
        id: u16,
        name: &str,
        value: &str,
        class: u16,
        ttl: u32,
        now: u64,
    ) -> Result<Vec<u8>> {
        let zone = fqdn(&self.zone);
        let name = fqdn(name);
        if !name.ends_with(&format!(".{zone}")) && name != zone {
            bail!("{name} is not inside zone {zone}");
        }
        let mut m = Vec::with_capacity(256);
        // Header: id, flags (UPDATE), ZOCOUNT 1, PRCOUNT 0, UPCOUNT 1, ADCOUNT 0 (TSIG added below).
        for v in [id, OPCODE_UPDATE, 1, 0, 1, 0] {
            m.extend_from_slice(&v.to_be_bytes());
        }
        put_name(&mut m, &zone)?;
        m.extend_from_slice(&TYPE_SOA.to_be_bytes());
        m.extend_from_slice(&CLASS_IN.to_be_bytes());
        put_name(&mut m, &name)?;
        m.extend_from_slice(&TYPE_TXT.to_be_bytes());
        m.extend_from_slice(&class.to_be_bytes());
        m.extend_from_slice(&ttl.to_be_bytes());
        let rdata = txt_rdata(value)?;
        m.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
        m.extend_from_slice(&rdata);

        // TSIG variables (RFC 8945 §4.3.3) appended to the message for the MAC.
        let key = fqdn(&self.key_name).to_ascii_lowercase();
        let mut vars = Vec::new();
        put_name(&mut vars, &key)?;
        vars.extend_from_slice(&CLASS_ANY.to_be_bytes());
        vars.extend_from_slice(&0u32.to_be_bytes());
        put_name(&mut vars, ALGORITHM)?;
        vars.extend_from_slice(&now.to_be_bytes()[2..]);
        vars.extend_from_slice(&FUDGE.to_be_bytes());
        vars.extend_from_slice(&0u16.to_be_bytes()); // error
        vars.extend_from_slice(&0u16.to_be_bytes()); // other len
        let mut signed = m.clone();
        signed.extend_from_slice(&vars);
        let mac = hmac_sha256(&self.secret, &signed);

        // The TSIG record itself.
        put_name(&mut m, &key)?;
        m.extend_from_slice(&TYPE_TSIG.to_be_bytes());
        m.extend_from_slice(&CLASS_ANY.to_be_bytes());
        m.extend_from_slice(&0u32.to_be_bytes());
        let mut rd = Vec::new();
        put_name(&mut rd, ALGORITHM)?;
        rd.extend_from_slice(&now.to_be_bytes()[2..]);
        rd.extend_from_slice(&FUDGE.to_be_bytes());
        rd.extend_from_slice(&(mac.len() as u16).to_be_bytes());
        rd.extend_from_slice(&mac);
        rd.extend_from_slice(&id.to_be_bytes());
        rd.extend_from_slice(&0u16.to_be_bytes());
        rd.extend_from_slice(&0u16.to_be_bytes());
        m.extend_from_slice(&(rd.len() as u16).to_be_bytes());
        m.extend_from_slice(&rd);
        m[10..12].copy_from_slice(&1u16.to_be_bytes()); // ADCOUNT 1
        Ok(m)
    }
}

fn fqdn(name: &str) -> String {
    let n = name.trim().trim_end_matches('.');
    format!("{n}.")
}

fn put_name(out: &mut Vec<u8>, name: &str) -> Result<()> {
    for label in name
        .trim_end_matches('.')
        .split('.')
        .filter(|l| !l.is_empty())
    {
        if label.len() > 63 {
            bail!("DNS label too long: {label}");
        }
        out.push(label.len() as u8);
        out.extend_from_slice(label.as_bytes());
    }
    out.push(0);
    Ok(())
}

fn txt_rdata(value: &str) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    for chunk in value.as_bytes().chunks(255) {
        out.push(chunk.len() as u8);
        out.extend_from_slice(chunk);
    }
    if out.is_empty() {
        out.push(0);
    }
    Ok(out)
}

fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    let mut k = [0u8; 64];
    if key.len() > 64 {
        k[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    inner.update(k.map(|b| b ^ 0x36));
    inner.update(msg);
    let mut outer = Sha256::new();
    outer.update(k.map(|b| b ^ 0x5c));
    outer.update(inner.finalize());
    outer.finalize().into()
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn rand_u16() -> u16 {
    let mut b = [0u8; 2];
    // SAFETY: valid 2-byte buffer.
    unsafe { libc::getrandom(b.as_mut_ptr().cast(), 2, 0) };
    u16::from_ne_bytes(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_matches_rfc4231() {
        // RFC 4231 test case 2.
        let mac = hmac_sha256(b"Jefe", b"what do ya want for nothing?");
        let hex: String = mac.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex,
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn update_message_layout() {
        let u = Updater {
            server: "127.0.0.1:53".into(),
            zone: "example.com".into(),
            key_name: "acme".into(),
            secret: b"secret".to_vec(),
        };
        let m = u
            .message(
                0x1234,
                "_acme-challenge.shop.example.com",
                "abc",
                CLASS_IN,
                60,
                1_700_000_000,
            )
            .unwrap();
        assert_eq!(&m[..2], &[0x12, 0x34]);
        assert_eq!(u16::from_be_bytes([m[2], m[3]]), OPCODE_UPDATE);
        assert_eq!(&m[4..12], &[0, 1, 0, 0, 0, 1, 0, 1]);
        assert!(m.windows(4).any(|w| w == b"\x03abc"), "TXT rdata");
        assert!(
            u.message(1, "x.other.org", "v", CLASS_IN, 60, 0).is_err(),
            "outside the zone"
        );
    }
}
