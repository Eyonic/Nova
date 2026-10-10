//! Who the client really is: its IP and whether it used HTTPS, taking
//! trusted reverse proxies (`X-Forwarded-*`, `Forwarded`) into account.

use http::{HeaderMap, header};
use nova_config::{Cidr, ForwardedHeader};
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

/// One entry of a forwarding chain: the address a proxy saw and, when
/// known, whether that hop used HTTPS. `ip` is `None` for entries that
/// cannot be verified (`unknown`, obfuscated identifiers, garbage).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Hop {
    ip: Option<IpAddr>,
    https: Option<bool>,
}

/// Only `source` is read: a proxy appends to its own header and passes any
/// other one through untouched, where the client could have forged it.
pub fn resolve(
    headers: &HeaderMap,
    conn: &ConnInfo,
    trusted: &[Cidr],
    source: ForwardedHeader,
) -> Client {
    let direct = Client {
        ip: canonical(conn.peer.ip()),
        https: conn.tls,
        proxied: false,
    };
    if !Cidr::any_contains(trusted, conn.peer.ip()) {
        return direct;
    }
    let hops = match source {
        ForwardedHeader::XForwardedFor => x_forwarded(headers),
        ForwardedHeader::Forwarded => forwarded(headers),
    };
    // Walk from the nearest hop outwards; the first untrusted address is the
    // client. Anything further left could have been forged by it, and an
    // unverifiable hop ends the walk at the last proxy that vouched for it.
    let mut ip = direct.ip;
    let mut https = conn.tls;
    for hop in hops.iter().rev() {
        let Some(addr) = hop.ip else { break };
        ip = canonical(addr);
        if let Some(h) = hop.https {
            https = h;
        }
        if !Cidr::any_contains(trusted, ip) {
            break;
        }
    }
    Client {
        ip,
        https,
        proxied: true,
    }
}

/// `X-Forwarded-For` plus `X-Forwarded-Proto`. When both list one entry per
/// hop they are paired; otherwise the nearest proxy's scheme applies.
fn x_forwarded(headers: &HeaderMap) -> Vec<Hop> {
    let list = |name: &str| -> Vec<String> {
        headers
            .get_all(name)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(','))
            .map(|v| v.trim().to_owned())
            .collect()
    };
    let ips = list("x-forwarded-for");
    let protos = list("x-forwarded-proto");
    let paired = protos.len() == ips.len();
    let nearest = protos.last().and_then(|p| scheme(p));
    let last = ips.len().saturating_sub(1);
    ips.iter()
        .enumerate()
        .map(|(i, ip)| Hop {
            ip: parse_ip(ip),
            https: if paired {
                scheme(&protos[i])
            } else if i == last {
                nearest
            } else {
                None
            },
        })
        .collect()
}

/// RFC 7239 `Forwarded: for=192.0.2.1;proto=https, for="[2001:db8::1]:4711"`.
fn forwarded(headers: &HeaderMap) -> Vec<Hop> {
    headers
        .get_all(header::FORWARDED)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(|element| {
            let mut hop = Hop {
                ip: None,
                https: None,
            };
            for pair in element.split(';') {
                let Some((k, v)) = pair.trim().split_once('=') else {
                    continue;
                };
                let v = v.trim().trim_matches('"');
                match k.trim().to_ascii_lowercase().as_str() {
                    "for" => hop.ip = parse_ip(v),
                    "proto" => hop.https = scheme(v),
                    _ => {}
                }
            }
            hop
        })
        .collect()
}

fn scheme(s: &str) -> Option<bool> {
    match s.to_ascii_lowercase().as_str() {
        "https" => Some(true),
        "http" => Some(false),
        _ => None,
    }
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

    const XFF: ForwardedHeader = ForwardedHeader::XForwardedFor;
    const RFC: ForwardedHeader = ForwardedHeader::Forwarded;

    fn trusted() -> Vec<Cidr> {
        vec!["10.0.0.0/8".parse().unwrap()]
    }

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn untrusted_peers_cannot_spoof() {
        let h = headers(&[
            ("x-forwarded-for", "1.1.1.1"),
            ("x-forwarded-proto", "https"),
            ("forwarded", "for=1.1.1.1;proto=https"),
        ]);
        for source in [XFF, RFC] {
            let c = resolve(&h, &conn("203.0.113.5:999", false), &trusted(), source);
            assert_eq!(c.ip, ip("203.0.113.5"));
            assert!(!c.https && !c.proxied);
        }
    }

    #[test]
    fn trusted_proxy_chain() {
        // spoofed, real client, inner proxy
        let h = headers(&[
            ("x-forwarded-for", "6.6.6.6, 198.51.100.7"),
            ("x-forwarded-for", "10.0.0.9"),
            ("x-forwarded-proto", "https"),
        ]);
        let c = resolve(&h, &conn("10.0.0.2:999", false), &trusted(), XFF);
        assert_eq!(c.ip, ip("198.51.100.7"));
        assert!(c.https && c.proxied);
    }

    #[test]
    fn rfc7239() {
        let h = headers(&[(
            "forwarded",
            "for=\"[2001:db8::1]:4711\";proto=https, for=10.1.1.1;proto=http",
        )]);
        let c = resolve(&h, &conn("10.0.0.2:999", false), &trusted(), RFC);
        assert_eq!(c.ip, ip("2001:db8::1"));
        assert!(c.https, "the client's own hop decides the scheme");
    }

    /// A proxy that maintains X-Forwarded-For passes a client-sent
    /// `Forwarded` header through unchanged; it must not be believed.
    #[test]
    fn only_the_configured_header_counts() {
        let h = headers(&[
            ("forwarded", "for=10.0.0.5;proto=https"),
            ("x-forwarded-for", "198.51.100.7"),
        ]);
        let c = resolve(&h, &conn("10.0.0.2:999", false), &trusted(), XFF);
        assert_eq!(c.ip, ip("198.51.100.7"));
        assert!(!c.https);

        let h = headers(&[
            ("x-forwarded-for", "10.0.0.5"),
            ("forwarded", "for=198.51.100.7"),
        ]);
        let c = resolve(&h, &conn("10.0.0.2:999", false), &trusted(), RFC);
        assert_eq!(c.ip, ip("198.51.100.7"));
    }

    #[test]
    fn unverifiable_hops_stop_the_walk() {
        let h = headers(&[("x-forwarded-for", "10.0.0.7, unknown, 10.0.0.9")]);
        let c = resolve(&h, &conn("10.0.0.2:999", false), &trusted(), XFF);
        assert_eq!(c.ip, ip("10.0.0.9"));

        let h = headers(&[("forwarded", "for=10.0.0.7, for=_hidden")]);
        let c = resolve(&h, &conn("10.0.0.2:999", true), &trusted(), RFC);
        assert_eq!(c.ip, ip("10.0.0.2"));
        assert!(c.https);
    }

    /// The client may send its own X-Forwarded-Proto; with one value per hop
    /// the pairing picks the client's hop, otherwise the nearest proxy's.
    #[test]
    fn forwarded_proto_comes_from_the_proxy() {
        let h = headers(&[
            ("x-forwarded-for", "198.51.100.7"),
            ("x-forwarded-proto", "http, https"),
        ]);
        let c = resolve(&h, &conn("10.0.0.2:999", false), &trusted(), XFF);
        assert!(c.https);

        let h = headers(&[
            ("x-forwarded-for", "198.51.100.7, 10.0.0.9"),
            ("x-forwarded-proto", "https, http"),
        ]);
        let c = resolve(&h, &conn("10.0.0.2:999", false), &trusted(), XFF);
        assert_eq!(c.ip, ip("198.51.100.7"));
        assert!(c.https);
    }

    #[test]
    fn mapped_ipv4() {
        let c = resolve(
            &HeaderMap::new(),
            &conn("[::ffff:192.0.2.1]:1", true),
            &[],
            XFF,
        );
        assert_eq!(c.ip, ip("192.0.2.1"));
        assert!(c.https);
    }
}
