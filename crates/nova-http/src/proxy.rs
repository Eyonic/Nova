//! PROXY protocol v1 (text) and v2 (binary) headers, as sent by HAProxy,
//! AWS/GCP load balancers and others in front of a TCP listener.
//!
//! The header is read byte-exactly so nothing after it is consumed.

use std::io::{Error, ErrorKind, Result};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use tokio::io::{AsyncRead, AsyncReadExt};

const V2_SIGNATURE: [u8; 12] = *b"\r\n\r\n\0\r\nQUIT\n";

fn invalid(msg: &str) -> Error {
    Error::new(ErrorKind::InvalidData, msg.to_string())
}

/// Read one PROXY header. `Ok(None)` means the header carried no client
/// address (`UNKNOWN`, `LOCAL` or a non-TCP family): use the socket peer.
pub async fn read_header<R: AsyncRead + Unpin>(r: &mut R) -> Result<Option<SocketAddr>> {
    let mut head = [0u8; 8];
    r.read_exact(&mut head).await?;
    if head == V2_SIGNATURE[..8] {
        let mut rest = [0u8; 8];
        r.read_exact(&mut rest).await?;
        if rest[..4] != V2_SIGNATURE[8..] {
            return Err(invalid("bad v2 signature"));
        }
        let ver_cmd = rest[4];
        let family = rest[5];
        let len = u16::from_be_bytes([rest[6], rest[7]]) as usize;
        if ver_cmd >> 4 != 2 || len > 1024 {
            return Err(invalid("unsupported v2 header"));
        }
        let mut body = vec![0u8; len];
        r.read_exact(&mut body).await?;
        return Ok(parse_v2(ver_cmd & 0x0f, family, &body));
    }
    if &head[..6] != b"PROXY " {
        return Err(invalid("missing PROXY header"));
    }
    // v1: at most 107 bytes including CRLF.
    let mut line = head.to_vec();
    while !line.ends_with(b"\r\n") {
        if line.len() >= 107 {
            return Err(invalid("v1 header too long"));
        }
        line.push(r.read_u8().await?);
    }
    let text =
        std::str::from_utf8(&line[..line.len() - 2]).map_err(|_| invalid("bad v1 header"))?;
    parse_v1(text)
}

fn parse_v1(line: &str) -> Result<Option<SocketAddr>> {
    let parts: Vec<&str> = line.split(' ').collect();
    match parts.as_slice() {
        ["PROXY", "UNKNOWN", ..] => Ok(None),
        ["PROXY", "TCP4" | "TCP6", src, _dst, sport, _dport] => {
            let ip: IpAddr = src.parse().map_err(|_| invalid("bad v1 address"))?;
            let port: u16 = sport.parse().map_err(|_| invalid("bad v1 port"))?;
            Ok(Some(SocketAddr::new(ip, port)))
        }
        _ => Err(invalid("bad v1 header")),
    }
}

fn parse_v2(command: u8, family: u8, body: &[u8]) -> Option<SocketAddr> {
    if command == 0 {
        return None; // LOCAL: health checks from the proxy itself
    }
    match family {
        0x11 if body.len() >= 12 => {
            let ip = Ipv4Addr::new(body[0], body[1], body[2], body[3]);
            let port = u16::from_be_bytes([body[8], body[9]]);
            Some(SocketAddr::new(ip.into(), port))
        }
        0x21 if body.len() >= 36 => {
            let mut a = [0u8; 16];
            a.copy_from_slice(&body[..16]);
            let port = u16::from_be_bytes([body[32], body[33]]);
            Some(SocketAddr::new(Ipv6Addr::from(a).into(), port))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn v1() {
        let mut data: &[u8] = b"PROXY TCP4 203.0.113.7 10.0.0.1 51234 443\r\nGET / HTTP/1.1\r\n";
        let a = read_header(&mut data).await.unwrap().unwrap();
        assert_eq!(a, "203.0.113.7:51234".parse().unwrap());
        assert_eq!(data, b"GET / HTTP/1.1\r\n");

        let mut data: &[u8] = b"PROXY UNKNOWN\r\nrest";
        assert!(read_header(&mut data).await.unwrap().is_none());
        assert_eq!(data, b"rest");

        let mut data: &[u8] = b"GET / HTTP/1.1\r\n";
        assert!(read_header(&mut data).await.is_err());
    }

    #[tokio::test]
    async fn v2() {
        let mut h = V2_SIGNATURE.to_vec();
        h.extend([0x21, 0x11, 0, 12]);
        h.extend([198, 51, 100, 9, 10, 0, 0, 1]);
        h.extend(4000u16.to_be_bytes());
        h.extend(443u16.to_be_bytes());
        h.extend(b"next");
        let mut data: &[u8] = &h;
        let a = read_header(&mut data).await.unwrap().unwrap();
        assert_eq!(a, "198.51.100.9:4000".parse().unwrap());
        assert_eq!(data, b"next");

        let mut local = V2_SIGNATURE.to_vec();
        local.extend([0x20, 0x00, 0, 0]);
        let mut data: &[u8] = &local;
        assert!(read_header(&mut data).await.unwrap().is_none());
    }
}
