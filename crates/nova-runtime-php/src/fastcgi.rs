//! Minimal FastCGI 1.0 client (responder role), as specified in
//! <https://fastcgi-archives.github.io/FastCGI_Specification.html>.
//!
//! One connection serves one request; NOVA does not multiplex. That keeps
//! the classic PHP request lifecycle and lets PHP-FPM detect aborted clients
//! by the connection closing.

use bytes::{Buf, BufMut, Bytes, BytesMut};
use std::io;
use tokio::io::{AsyncRead, AsyncReadExt};

pub const VERSION_1: u8 = 1;
pub const HEADER_LEN: usize = 8;
pub const MAX_CONTENT_LEN: usize = 0xFFFF;
/// NOVA uses a single request per connection, so the id is constant.
pub const REQUEST_ID: u16 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum RecordType {
    BeginRequest = 1,
    AbortRequest = 2,
    EndRequest = 3,
    Params = 4,
    Stdin = 5,
    Stdout = 6,
    Stderr = 7,
    Data = 8,
    GetValues = 9,
    GetValuesResult = 10,
    UnknownType = 11,
}

impl RecordType {
    fn from_u8(v: u8) -> Option<Self> {
        use RecordType::*;
        Some(match v {
            1 => BeginRequest,
            2 => AbortRequest,
            3 => EndRequest,
            4 => Params,
            5 => Stdin,
            6 => Stdout,
            7 => Stderr,
            8 => Data,
            9 => GetValues,
            10 => GetValuesResult,
            11 => UnknownType,
            _ => return None,
        })
    }
}

const ROLE_RESPONDER: u16 = 1;

/// Append one record. `content` must fit in a single record.
pub fn put_record(buf: &mut BytesMut, ty: RecordType, content: &[u8]) {
    assert!(content.len() <= MAX_CONTENT_LEN, "record content too large");
    let padding = (8 - content.len() % 8) % 8;
    buf.reserve(HEADER_LEN + content.len() + padding);
    buf.put_u8(VERSION_1);
    buf.put_u8(ty as u8);
    buf.put_u16(REQUEST_ID);
    buf.put_u16(content.len() as u16);
    buf.put_u8(padding as u8);
    buf.put_u8(0);
    buf.put_slice(content);
    buf.put_bytes(0, padding);
}

/// Append a stream (PARAMS or STDIN payload) split into maximal records.
/// Does not write the terminating empty record.
pub fn put_stream(buf: &mut BytesMut, ty: RecordType, data: &[u8]) {
    for chunk in data.chunks(MAX_CONTENT_LEN) {
        put_record(buf, ty, chunk);
    }
}

pub fn put_begin_request(buf: &mut BytesMut) {
    let mut body = [0u8; 8];
    body[..2].copy_from_slice(&ROLE_RESPONDER.to_be_bytes());
    // flags = 0: FPM closes the connection after the request.
    put_record(buf, RecordType::BeginRequest, &body);
}

fn put_length(buf: &mut Vec<u8>, len: usize) {
    if len < 0x80 {
        buf.push(len as u8);
    } else {
        buf.extend_from_slice(&((len as u32) | 0x8000_0000).to_be_bytes());
    }
}

/// Encode name-value pairs for the PARAMS stream.
pub fn encode_params<'a>(pairs: impl IntoIterator<Item = (&'a [u8], &'a [u8])>) -> Vec<u8> {
    let mut out = Vec::with_capacity(1024);
    for (name, value) in pairs {
        put_length(&mut out, name.len());
        put_length(&mut out, value.len());
        out.extend_from_slice(name);
        out.extend_from_slice(value);
    }
    out
}

/// Decode name-value pairs (used by tests and GET_VALUES_RESULT).
pub fn decode_params(mut data: &[u8]) -> io::Result<Vec<(Vec<u8>, Vec<u8>)>> {
    fn len(data: &mut &[u8]) -> io::Result<usize> {
        let bad = || io::Error::new(io::ErrorKind::InvalidData, "truncated name-value length");
        let first = *data.first().ok_or_else(bad)?;
        if first & 0x80 == 0 {
            data.advance(1);
            Ok(first as usize)
        } else {
            if data.len() < 4 {
                return Err(bad());
            }
            Ok((data.get_u32() & 0x7FFF_FFFF) as usize)
        }
    }
    let mut out = Vec::new();
    while !data.is_empty() {
        let n = len(&mut data)?;
        let v = len(&mut data)?;
        if data.len() < n + v {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "truncated name-value pair",
            ));
        }
        out.push((data[..n].to_vec(), data[n..n + v].to_vec()));
        data.advance(n + v);
    }
    Ok(out)
}

#[derive(Debug)]
pub struct Record {
    pub ty: RecordType,
    pub content: Bytes,
}

/// Reads records from the FPM side of the connection.
pub struct RecordReader<R> {
    inner: R,
}

impl<R: AsyncRead + Unpin> RecordReader<R> {
    pub fn new(inner: R) -> Self {
        Self { inner }
    }

    /// Returns `Ok(None)` on a clean EOF at a record boundary.
    pub async fn next(&mut self) -> io::Result<Option<Record>> {
        let mut header = [0u8; HEADER_LEN];
        match self.inner.read_exact(&mut header).await {
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e),
        }
        if header[0] != VERSION_1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsupported FastCGI version {}", header[0]),
            ));
        }
        let ty = RecordType::from_u8(header[1]).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unknown record type {}", header[1]),
            )
        })?;
        let len = u16::from_be_bytes([header[4], header[5]]) as usize;
        let padding = header[6] as usize;
        let mut content = vec![0u8; len + padding];
        self.inner.read_exact(&mut content).await?;
        content.truncate(len);
        Ok(Some(Record {
            ty,
            content: content.into(),
        }))
    }
}

/// Parsed END_REQUEST body.
#[derive(Debug, Clone, Copy)]
pub struct EndRequest {
    pub app_status: u32,
    pub protocol_status: u8,
}

impl EndRequest {
    pub fn parse(content: &[u8]) -> io::Result<Self> {
        if content.len() < 8 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "short END_REQUEST body",
            ));
        }
        Ok(Self {
            app_status: u32::from_be_bytes(content[..4].try_into().unwrap()),
            protocol_status: content[4],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_is_padded_to_eight_bytes() {
        let mut buf = BytesMut::new();
        put_record(&mut buf, RecordType::Stdin, b"hello");
        assert_eq!(buf.len(), 8 + 8);
        assert_eq!(&buf[..8], &[1, 5, 0, 1, 0, 5, 3, 0]);
        assert_eq!(&buf[8..13], b"hello");
    }

    #[test]
    fn params_roundtrip_short_and_long() {
        let long = vec![b'x'; 300];
        let enc = encode_params([
            (&b"SCRIPT_NAME"[..], &b"/index.php"[..]),
            (&b"LONG"[..], &long[..]),
        ]);
        // Short lengths are one byte, long ones four bytes with the high bit set.
        assert_eq!(enc[0], 11);
        let dec = decode_params(&enc).unwrap();
        assert_eq!(dec[0], (b"SCRIPT_NAME".to_vec(), b"/index.php".to_vec()));
        assert_eq!(dec[1].1.len(), 300);
    }

    #[test]
    fn large_stream_splits_into_records() {
        let mut buf = BytesMut::new();
        put_stream(
            &mut buf,
            RecordType::Stdin,
            &vec![0u8; MAX_CONTENT_LEN + 10],
        );
        // 65535 bytes + 1 padding, then 10 bytes + 6 padding, plus two headers.
        assert_eq!(buf.len(), 8 + 65536 + 8 + 16);
    }

    #[tokio::test]
    async fn reader_parses_records_and_skips_padding() {
        let mut buf = BytesMut::new();
        put_record(&mut buf, RecordType::Stdout, b"Status: 200\r\n\r\nok");
        put_record(&mut buf, RecordType::EndRequest, &[0, 0, 0, 0, 0, 0, 0, 0]);
        let data = buf.freeze();
        let mut r = RecordReader::new(&data[..]);
        let a = r.next().await.unwrap().unwrap();
        assert_eq!(a.ty, RecordType::Stdout);
        assert_eq!(&a.content[..], b"Status: 200\r\n\r\nok");
        let b = r.next().await.unwrap().unwrap();
        assert_eq!(b.ty, RecordType::EndRequest);
        assert!(r.next().await.unwrap().is_none());
    }
}
