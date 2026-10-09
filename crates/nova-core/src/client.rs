//! Who the client really is: its IP and whether it used HTTPS, taking
//! trusted reverse proxies (`X-Forwarded-*`, `Forwarded`) into account.

use http::{HeaderMap, header};
use nova_config::Cidr;
use nova_http::ConnInfo;
use std::net::IpAddr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Client {
    pub ip: IpAddr,
    /// The client reached the edge (NOVA or the trusted proxy) over HTTPS.
    pub https: bool,
    /// The connection came through a trusted proxy.
    pub proxied: bool,
}

pub fn resolve(headers: &HeaderMap, conn: &ConnInfo, trusted: &[Cidr]) -> Client {
    let direct = Client {
        ip: canonical(conn.peer.ip()),
        https: conn.tls,
        proxied: false,
    };
    if !Cidr::any_contains(trusted, conn.peer.ip()) {
        return direct;
    }

    let (chain, proto) = match forwarded(headers) {
        Some(f) => f,
        None => (
            headers
                .get_all("x-forwarded-for")
                .iter()
                .filter_map(|v| v.to_str().ok())
                .flat_map(|v| v.split(','))
                .filter_map(|ip| parse_ip(ip.trim()))
                .collect(),
            headers
                .get("x-forwarded-proto")
                .and_then(|v| v.to_str().ok())
                .map(|v| {
                    v.split(',')
                        .next()
                        .unwrap_or("")
                        .trim()
                        .to_ascii_lowercase()
                }),
        ),
    };
    // Walk from the nearest hop outwards; the first untrusted address is the
    // client. Anything further left could have been forged by it.
    let mut ip = direct.ip;
    for hop in chain.iter().rev() {
        ip = canonical(*hop);
        if !Cidr::any_contains(trusted, ip) {
            break;
        }
    }
    Client {
        ip,
        https: match proto.as_deref() {
            Some("https") => true,
            Some("http") => false,
            _ => conn.tls,
        },
        proxied: true,
    }
}

/// RFC 7239 `Forwarded: for=192.0.2.1;proto=https, for="[2001:db8::1]:4711"`.
fn forwarded(headers: &HeaderMap) -> Option<(Vec<IpAddr>, Option<String>)> {
    let values: Vec<&str> = headers
        .get_all(header::FORWARDED)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .collect();
    if values.is_empty() {
        return None;
    }
    let mut chain = Vec::new();
    let mut first_proto = None;
    for element in values.iter().flat_map(|v| v.split(',')) {
        for pair in element.split(';') {
            let Some((k, v)) = pair.trim().split_once('=') else {
                continue;
            };
            let v = v.trim().trim_matches('"');
            match k.trim().to_ascii_lowercase().as_str() {
                "for" => {
                    if let Some(ip) = parse_ip(v) {
                        chain.push(ip);
                    }
                }
                "proto" if first_proto.is_none() => first_proto = Some(v.to_ascii_lowercase()),
                _ => {}
            }
        }
    }
    Some((chain, first_proto))
}

/// `1.2.3.4`, `1.2.3.4:80`, `[::1]`, `[::1]:80` or a bare IPv6 address.
fn parse_ip(s: &str) -> Option<IpAddr> {
    if let Ok(ip) = s.parse() {
        return Some(ip);
    }
    if let Some(rest) = s.strip_prefix('[') {
        return rest.split_once(']')?.0.parse().ok();
    }
    s.rsplit_once(':')?.0.parse().ok()
}

/// IPv4-mapped IPv6 (`::ffff:1.2.3.4`) as plain IPv4.
fn canonical(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(ip),
        v4 => v4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn(peer: &str, tls: bool) -> ConnInfo {
        let peer = peer.parse().unwrap();
        ConnInfo {
            peer,
            socket_peer: peer,
            tls,
            http3: false,
        }
    }

    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.append(*k, v.parse().unwrap());
        }
        h
    }

    #[test]
    fn untrusted_peers_cannot_spoof() {
        let trusted = vec!["10.0.0.0/8".parse().unwrap()];
        let h = headers(&[
            ("x-forwarded-for", "1.1.1.1"),
            ("x-forwarded-proto", "https"),
        ]);
        let c = resolve(&h, &conn("203.0.113.5:999", false), &trusted);
        assert_eq!(c.ip, "203.0.113.5".parse::<IpAddr>().unwrap());
        assert!(!c.https && !c.proxied);
    }

    #[test]
    fn trusted_proxy_chain() {
        let trusted = vec!["10.0.0.0/8".parse().unwrap()];
        // spoofed, real client, inner proxy
        let h = headers(&[
            ("x-forwarded-for", "6.6.6.6, 198.51.100.7"),
            ("x-forwarded-for", "10.0.0.9"),
            ("x-forwarded-proto", "https"),
        ]);
        let c = resolve(&h, &conn("10.0.0.2:999", false), &trusted);
        assert_eq!(c.ip, "198.51.100.7".parse::<IpAddr>().unwrap());
        assert!(c.https && c.proxied);
    }

    #[test]
    fn rfc7239() {
        let trusted = vec!["10.0.0.0/8".parse().unwrap()];
        let h = headers(&[(
            "forwarded",
            "for=\"[2001:db8::1]:4711\";proto=https, for=10.1.1.1",
        )]);
        let c = resolve(&h, &conn("10.0.0.2:999", false), &trusted);
        assert_eq!(c.ip, "2001:db8::1".parse::<IpAddr>().unwrap());
        assert!(c.https);
    }

    #[test]
    fn mapped_ipv4() {
        let c = resolve(&HeaderMap::new(), &conn("[::ffff:192.0.2.1]:1", true), &[]);
        assert_eq!(c.ip, "192.0.2.1".parse::<IpAddr>().unwrap());
        assert!(c.https);
    }
}
