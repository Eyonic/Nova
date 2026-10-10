//! Fast path for the JSON access log.
//!
//! In JSON mode one access line per request went through `tracing`'s
//! generic field visitors and JSON formatter, which cost ~19% of
//! small-file throughput. When the CLI registers a sink, the same line
//! (same keys, same order, same shape as `tracing`'s flattened JSON) is
//! built here directly into a reused buffer and handed to the log writer
//! thread. Text mode and `NOVA_LOG` filtering keep working: the sink is
//! only installed for JSON output, and the line is only built when
//! `nova::access` is enabled at INFO.

use std::fmt::Write as _;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

type Sink = Box<dyn Fn(&[u8]) + Send + Sync>;

static SINK: OnceLock<Sink> = OnceLock::new();

/// Install the writer for pre-formatted JSON access lines (once).
pub fn set_json_sink(sink: impl Fn(&[u8]) + Send + Sync + 'static) {
    let _ = SINK.set(Box::new(sink));
}

/// One access-log record. Field names match the `tracing` event they replace.
pub struct Record<'a> {
    pub request_id: &'a str,
    pub site: &'a str,
    pub client: std::net::IpAddr,
    pub peer: std::net::SocketAddr,
    pub host: &'a str,
    pub method: &'a str,
    pub path: &'a str,
    pub protocol: &'a str,
    pub tls: bool,
    pub status: u16,
    pub bytes: Option<u64>,
    pub kind: &'a str,
    pub encoding: Option<&'a str>,
    pub referer: Option<&'a str>,
    pub user_agent: Option<&'a str>,
    pub duration_ms: f64,
}

/// Write the record through the JSON sink. `false` when no sink is
/// installed (the caller then logs through `tracing`).
pub fn write(r: &Record<'_>) -> bool {
    let Some(sink) = SINK.get() else {
        return false;
    };
    thread_local! {
        static BUF: std::cell::RefCell<String> = std::cell::RefCell::new(String::with_capacity(512));
    }
    BUF.with_borrow_mut(|line| {
        line.clear();
        format_json(line, r, SystemTime::now());
        sink(line.as_bytes());
    });
    true
}

fn format_json(out: &mut String, r: &Record<'_>, now: SystemTime) {
    out.push_str("{\"timestamp\":\"");
    timestamp(out, now);
    out.push_str("\",\"level\":\"INFO\"");
    field_str(out, "request_id", r.request_id);
    field_str(out, "site", r.site);
    let _ = write!(out, ",\"client\":\"{}\"", r.client);
    let _ = write!(out, ",\"peer\":\"{}\"", r.peer);
    field_str(out, "host", r.host);
    field_str(out, "method", r.method);
    field_str(out, "path", r.path);
    field_str(out, "protocol", r.protocol);
    let _ = write!(out, ",\"tls\":{},\"status\":{}", r.tls, r.status);
    if let Some(b) = r.bytes {
        let _ = write!(out, ",\"bytes\":{b}");
    }
    field_str(out, "kind", r.kind);
    if let Some(e) = r.encoding {
        field_str(out, "encoding", e);
    }
    if let Some(v) = r.referer {
        field_str(out, "referer", v);
    }
    if let Some(v) = r.user_agent {
        field_str(out, "user_agent", v);
    }
    let _ = write!(out, ",\"duration_ms\":{}", r.duration_ms);
    out.push_str(",\"target\":\"nova::access\"}\n");
}

fn field_str(out: &mut String, key: &str, value: &str) {
    out.push_str(",\"");
    out.push_str(key);
    out.push_str("\":\"");
    escape(out, value);
    out.push('"');
}

/// JSON string escaping (RFC 8259): quotes, backslash and control characters.
fn escape(out: &mut String, s: &str) {
    let mut start = 0;
    for (i, c) in s.char_indices() {
        let rep = match c {
            '"' => "\\\"",
            '\\' => "\\\\",
            '\n' => "\\n",
            '\r' => "\\r",
            '\t' => "\\t",
            c if (c as u32) < 0x20 => {
                out.push_str(&s[start..i]);
                let _ = write!(out, "\\u{:04x}", c as u32);
                start = i + c.len_utf8();
                continue;
            }
            _ => continue,
        };
        out.push_str(&s[start..i]);
        out.push_str(rep);
        start = i + 1;
    }
    out.push_str(&s[start..]);
}

/// RFC 3339 UTC with microseconds, like `tracing-subscriber`'s default:
/// `2026-10-09T23:11:51.302353Z`.
fn timestamp(out: &mut String, now: SystemTime) {
    let d = now.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = d.as_secs();
    let (days, rem) = (secs / 86_400, secs % 86_400);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    let _ = write!(
        out,
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:06}Z",
        rem / 3600,
        rem / 60 % 60,
        rem % 60,
        d.subsec_micros()
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn record() -> Record<'static> {
        Record {
            request_id: "485328ed00000004",
            site: "example",
            client: "10.89.0.4".parse().unwrap(),
            peer: "10.89.0.4:37942".parse().unwrap(),
            host: "localhost:8080",
            method: "GET",
            path: "/a\"b\\c\n",
            protocol: "HTTP/1.1",
            tls: false,
            status: 200,
            bytes: Some(1270),
            kind: "static",
            encoding: None,
            referer: None,
            user_agent: Some("curl/8.22.0\u{1}"),
            duration_ms: 0.25,
        }
    }

    #[test]
    fn json_line_is_valid_and_complete() {
        let mut s = String::new();
        let t = UNIX_EPOCH + Duration::from_micros(1_791_587_506_302_353);
        format_json(&mut s, &record(), t);
        assert!(s.ends_with("}\n"));
        let v: serde_json::Value = serde_json::from_str(s.trim_end()).unwrap();
        assert_eq!(v["timestamp"], "2026-10-09T23:11:46.302353Z");
        assert_eq!(v["level"], "INFO");
        assert_eq!(v["target"], "nova::access");
        assert_eq!(v["path"], "/a\"b\\c\n");
        assert_eq!(v["user_agent"], "curl/8.22.0\u{1}");
        assert_eq!(v["status"], 200);
        assert_eq!(v["bytes"], 1270);
        assert_eq!(v["tls"], false);
        assert_eq!(v["duration_ms"], 0.25);
        assert!(v.get("encoding").is_none() && v.get("referer").is_none());
    }

    #[test]
    fn timestamps() {
        let mut s = String::new();
        timestamp(&mut s, UNIX_EPOCH);
        assert_eq!(s, "1970-01-01T00:00:00.000000Z");
        s.clear();
        // 2000-02-29 (leap day) 12:34:56.000001
        timestamp(&mut s, UNIX_EPOCH + Duration::new(951_827_696, 1_000));
        assert_eq!(s, "2000-02-29T12:34:56.000001Z");
    }
}
