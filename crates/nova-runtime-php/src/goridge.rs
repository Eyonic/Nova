//! Goridge frames: the wire format between RoadRunner-compatible PHP
//! workers (`spiral/roadrunner-worker`, Laravel Octane, Symfony runtimes)
//! and their server.
//!
//! ```text
//! 0      version << 4 | header words (3 + options)
//! 1      flags (CONTROL, ERROR, codec)
//! 2..6   payload length, little endian (options excluded)
//! 6..10  CRC-32 of bytes 0..6
//! 10     byte10: STREAM / STOP / PING / PONG
//! 11     byte11 (unused by PHP; NOVA marks requests whose form body the
//!        site's worker manager parses)
//! 12..   options (u32 LE each), then the payload
//! ```
//!
//! For HTTP the first option is the length of the JSON header that starts
//! the payload; the rest of the payload is the body.

use bytes::{BufMut, Bytes, BytesMut};
use std::io;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const VERSION: u8 = 1;
pub const CONTROL: u8 = 0x01;
pub const CODEC_JSON: u8 = 0x08;
pub const ERROR: u8 = 0x40;

pub const STREAM: u8 = 0x01;
pub const STOP: u8 = 0x02;
pub const PING: u8 = 0x04;
pub const PONG: u8 = 0x08;

/// byte11: the body is `application/x-www-form-urlencoded` or
/// `multipart/form-data` and still has to be parsed (see [`crate::form`]).
pub const NOVA_PARSE_FORM: u8 = 0x01;
/// byte11 of a request: the header part is FastCGI-encoded CGI parameters
/// (NOVA's PHP extension, `frankenphp_handle_request()`).
pub const NOVA_FASTCGI: u8 = 0x02;
/// byte11 of a response: the header part is a CGI header block.
pub const NOVA_CGI_HEAD: u8 = 0x02;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Frame {
    pub flags: u8,
    pub byte10: u8,
    pub byte11: u8,
    pub options: Vec<u32>,
    pub payload: Bytes,
}

impl Frame {
    /// An HTTP message: JSON `header` followed by `body`.
    pub fn message(header: &[u8], body: &[u8]) -> Self {
        let mut payload = BytesMut::with_capacity(header.len() + body.len());
        payload.put_slice(header);
        payload.put_slice(body);
        Frame {
            flags: CODEC_JSON,
            options: vec![header.len() as u32],
            payload: payload.freeze(),
            ..Default::default()
        }
    }

    /// A control command such as `{"stop":true}`.
    pub fn control(json: &'static str) -> Self {
        Frame {
            flags: CONTROL,
            payload: Bytes::from_static(json.as_bytes()),
            ..Default::default()
        }
    }

    /// Split an HTTP message into (header, body).
    pub fn split(&self) -> io::Result<(Bytes, Bytes)> {
        let n = self.options.first().copied().unwrap_or(0) as usize;
        if n > self.payload.len() {
            return Err(invalid("header length beyond payload"));
        }
        Ok((self.payload.slice(..n), self.payload.slice(n..)))
    }

    pub fn encode(&self, out: &mut BytesMut) {
        self.encode_head(out);
        out.put_slice(&self.payload);
    }

    /// The 12-byte header and the options (everything before the payload).
    fn encode_head(&self, out: &mut BytesMut) {
        let words = 3 + self.options.len() as u8;
        let mut head = [0u8; 12];
        head[0] = VERSION << 4 | words;
        head[1] = self.flags;
        head[2..6].copy_from_slice(&(self.payload.len() as u32).to_le_bytes());
        let crc = crc32fast::hash(&head[..6]);
        head[6..10].copy_from_slice(&crc.to_le_bytes());
        head[10] = self.byte10;
        head[11] = self.byte11;
        out.reserve(12 + self.options.len() * 4 + self.payload.len());
        out.put_slice(&head);
        for o in &self.options {
            out.put_u32_le(*o);
        }
    }

    pub async fn write_to<W: AsyncWrite + Unpin>(&self, w: &mut W) -> io::Result<()> {
        let mut buf = BytesMut::new();
        self.encode_head(&mut buf);
        if self.payload.len() >= 16 * 1024 {
            // Large payloads go out as they are: one copy fewer.
            w.write_all(&buf).await?;
            return w.write_all(&self.payload).await;
        }
        buf.put_slice(&self.payload);
        w.write_all(&buf).await
    }
}

fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

/// Read one frame; payloads above `max` are refused. A clean EOF before
/// the first byte is `UnexpectedEof`.
pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R, max: usize) -> io::Result<Frame> {
    let mut head = [0u8; 12];
    r.read_exact(&mut head).await?;
    if head[0] >> 4 != VERSION {
        return Err(invalid("unsupported goridge version"));
    }
    let crc = u32::from_le_bytes(head[6..10].try_into().unwrap());
    if crc != crc32fast::hash(&head[..6]) {
        return Err(invalid("goridge header checksum mismatch"));
    }
    let words = (head[0] & 0x0f) as usize;
    if words < 3 {
        return Err(invalid("goridge header too short"));
    }
    let len = u32::from_le_bytes(head[2..6].try_into().unwrap()) as usize;
    if len > max {
        return Err(invalid("goridge frame too large"));
    }
    let mut opts = vec![0u8; (words - 3) * 4];
    r.read_exact(&mut opts).await?;
    // Read straight into uninitialised capacity: no zeroing of (possibly
    // large) response bodies, and never past this frame.
    let mut payload = BytesMut::with_capacity(len);
    while payload.len() < len {
        let want = (len - payload.len()) as u64;
        if (&mut *r).take(want).read_buf(&mut payload).await? == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
    }
    Ok(Frame {
        flags: head[1],
        byte10: head[10],
        byte11: head[11],
        options: opts
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| u32::from_le_bytes(*c))
            .collect(),
        payload: payload.freeze(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn roundtrip() {
        let mut f = Frame::message(br#"{"status":200}"#, b"hello");
        f.byte10 = STREAM | PING;
        let mut buf = BytesMut::new();
        f.encode(&mut buf);
        // What spiral/goridge's Frame::packFrame produces for the same frame.
        assert_eq!(buf[0], 0x14);
        assert_eq!(&buf[2..6], &19u32.to_le_bytes());
        assert_eq!(&buf[12..16], &14u32.to_le_bytes());
        let back = read_frame(&mut &buf[..], 1 << 20).await.unwrap();
        assert_eq!(back, f);
        let (h, b) = back.split().unwrap();
        assert_eq!(&h[..], br#"{"status":200}"#);
        assert_eq!(&b[..], b"hello");

        let mut bad = buf.clone();
        bad[3] ^= 1;
        assert!(read_frame(&mut &bad[..], 1 << 20).await.is_err(), "crc");
        assert!(read_frame(&mut &buf[..], 4).await.is_err(), "size limit");
    }
}
