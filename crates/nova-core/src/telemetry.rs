//! OpenTelemetry tracing (`[telemetry] otlp_endpoint`).
//!
//! Every request gets a server span; the PHP execution or upstream call it
//! makes gets a child span. W3C trace context is followed: a `traceparent`
//! from the client (or a trusted front proxy) continues its trace, and PHP
//! and upstreams receive a `traceparent` naming their parent span, so
//! their own instrumentation (OpenTelemetry PHP, Node, ...) joins the same
//! trace. Spans are exported in batches as OTLP/HTTP JSON to
//! `<otlp_endpoint>/v1/traces` by a background task; a slow or absent
//! collector costs dropped spans, never request latency.

use bytes::Bytes;
use http::{HeaderValue, Request};
use http_body_util::{BodyExt, Full};
use std::cell::Cell;
use std::fmt::Write as _;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;

/// Spans waiting for export; more are dropped.
const QUEUE: usize = 8192;
/// Spans per export request.
const BATCH: usize = 512;
const FLUSH_EVERY: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Internal = 1,
    Server = 2,
    Client = 3,
}

/// The trace a request belongs to and its server span.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TraceCtx {
    pub trace_id: [u8; 16],
    /// The request's (server) span.
    pub span_id: [u8; 8],
    /// The remote parent from an incoming `traceparent`.
    pub parent: Option<[u8; 8]>,
    pub sampled: bool,
}

impl TraceCtx {
    /// Continue an incoming `traceparent`, or start a trace (sampled with
    /// probability `ratio`).
    pub fn start(traceparent: Option<&str>, ratio: f64) -> Self {
        if let Some((trace_id, parent, sampled)) = traceparent.and_then(parse_traceparent) {
            return TraceCtx {
                trace_id,
                span_id: random_id(),
                parent: Some(parent),
                sampled,
            };
        }
        let mut trace_id = [0u8; 16];
        trace_id[..8].copy_from_slice(&random_id());
        trace_id[8..].copy_from_slice(&random_id());
        TraceCtx {
            trace_id,
            span_id: random_id(),
            parent: None,
            sampled: ratio >= 1.0 || (ratio > 0.0 && random_unit() < ratio),
        }
    }

    /// `traceparent` naming `span` as the parent of what comes next.
    pub fn traceparent(&self, span: [u8; 8]) -> HeaderValue {
        let s = format!(
            "00-{}-{}-{}",
            hex(&self.trace_id),
            hex(&span),
            if self.sampled { "01" } else { "00" }
        );
        HeaderValue::from_str(&s).expect("hex is a valid header value")
    }
}

fn parse_traceparent(v: &str) -> Option<([u8; 16], [u8; 8], bool)> {
    let mut parts = v.trim().split('-');
    let version = parts.next()?;
    let (trace, span, flags) = (parts.next()?, parts.next()?, parts.next()?);
    if version.len() != 2
        || version == "ff"
        || trace.len() != 32
        || span.len() != 16
        || flags.len() != 2
    {
        return None;
    }
    let trace_id: [u8; 16] = unhex(trace)?.try_into().ok()?;
    let parent: [u8; 8] = unhex(span)?.try_into().ok()?;
    if trace_id == [0; 16] || parent == [0; 8] {
        return None;
    }
    let flags = u8::from_str_radix(flags, 16).ok()?;
    Some((trace_id, parent, flags & 1 == 1))
}

/// One finished span.
#[derive(Debug, Clone)]
pub struct Span {
    pub trace_id: [u8; 16],
    pub span_id: [u8; 8],
    pub parent: Option<[u8; 8]>,
    pub name: String,
    pub kind: Kind,
    pub start: SystemTime,
    pub end: SystemTime,
    pub attributes: Vec<(&'static str, Value)>,
    pub error: bool,
}

#[derive(Debug, Clone)]
pub enum Value {
    Str(String),
    Int(i64),
    Bool(bool),
}

impl From<&str> for Value {
    fn from(v: &str) -> Self {
        Value::Str(v.to_string())
    }
}
impl From<String> for Value {
    fn from(v: String) -> Self {
        Value::Str(v)
    }
}
impl From<i64> for Value {
    fn from(v: i64) -> Self {
        Value::Int(v)
    }
}
impl From<bool> for Value {
    fn from(v: bool) -> Self {
        Value::Bool(v)
    }
}

/// Hands finished spans to the exporter task.
#[derive(Debug)]
pub struct Tracer {
    tx: mpsc::Sender<Span>,
    pub ratio: f64,
}

impl Tracer {
    /// Start the exporter (in the serving process).
    pub fn start(cfg: &nova_config::TelemetryConfig) -> Option<Self> {
        let endpoint = cfg
            .otlp_endpoint
            .as_ref()?
            .trim_end_matches('/')
            .to_string();
        let (tx, rx) = mpsc::channel(QUEUE);
        tokio::spawn(export(
            format!("{endpoint}/v1/traces"),
            cfg.service_name.clone(),
            rx,
        ));
        Some(Tracer {
            tx,
            ratio: cfg.sample_ratio,
        })
    }

    /// Record a span (dropped when the queue is full).
    pub fn record(&self, span: Span) {
        let _ = self.tx.try_send(span);
    }
}

async fn export(url: String, service: String, mut rx: mpsc::Receiver<Span>) {
    let client = crate::upstream::http_client();
    let mut batch = Vec::with_capacity(BATCH);
    let mut warned = false;
    loop {
        let deadline = tokio::time::sleep(FLUSH_EVERY);
        tokio::pin!(deadline);
        while batch.len() < BATCH {
            tokio::select! {
                s = rx.recv() => match s {
                    Some(s) => batch.push(s),
                    None => return,
                },
                _ = &mut deadline => break,
            }
        }
        if batch.is_empty() {
            continue;
        }
        let body = Bytes::from(otlp_json(&service, &batch));
        batch.clear();
        let req = Request::post(&url)
            .header(http::header::CONTENT_TYPE, "application/json")
            .body(Full::new(body).map_err(|n| match n {}).boxed())
            .expect("valid request");
        let ok = match tokio::time::timeout(Duration::from_secs(5), client.request(req)).await {
            Ok(Ok(r)) => r.status().is_success(),
            _ => false,
        };
        if !ok && !warned {
            tracing::warn!(
                collector = url,
                "could not export spans; dropping them until it answers"
            );
        }
        warned = !ok;
    }
}

/// OTLP/HTTP JSON (`ExportTraceServiceRequest`).
pub fn otlp_json(service: &str, spans: &[Span]) -> String {
    let mut s = String::with_capacity(256 + spans.len() * 512);
    s.push_str(r#"{"resourceSpans":[{"resource":{"attributes":[{"key":"service.name","value":{"stringValue":"#);
    json_str(&mut s, service);
    s.push_str(r#"}}]},"scopeSpans":[{"scope":{"name":"nova","version":"#);
    json_str(&mut s, env!("CARGO_PKG_VERSION"));
    s.push_str(r#"},"spans":["#);
    for (i, sp) in spans.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        let _ = write!(
            s,
            r#"{{"traceId":"{}","spanId":"{}","#,
            hex(&sp.trace_id),
            hex(&sp.span_id)
        );
        if let Some(p) = sp.parent {
            let _ = write!(s, r#""parentSpanId":"{}","#, hex(&p));
        }
        s.push_str(r#""name":"#);
        json_str(&mut s, &sp.name);
        let _ = write!(
            s,
            r#","kind":{},"startTimeUnixNano":"{}","endTimeUnixNano":"{}","attributes":["#,
            sp.kind as u8,
            nanos(sp.start),
            nanos(sp.end)
        );
        for (j, (k, v)) in sp.attributes.iter().enumerate() {
            if j > 0 {
                s.push(',');
            }
            s.push_str(r#"{"key":"#);
            json_str(&mut s, k);
            s.push_str(r#","value":{"#);
            match v {
                Value::Str(v) => {
                    s.push_str(r#""stringValue":"#);
                    json_str(&mut s, v);
                }
                Value::Int(v) => {
                    let _ = write!(s, r#""intValue":"{v}""#);
                }
                Value::Bool(v) => {
                    let _ = write!(s, r#""boolValue":{v}"#);
                }
            }
            s.push_str("}}");
        }
        s.push(']');
        if sp.error {
            s.push_str(r#","status":{"code":2}"#);
        }
        s.push('}');
    }
    s.push_str("]}]}]}");
    s
}

fn nanos(t: SystemTime) -> u128 {
    t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos())
}

fn json_str(s: &mut String, v: &str) {
    s.push('"');
    for c in v.chars() {
        match c {
            '"' => s.push_str("\\\""),
            '\\' => s.push_str("\\\\"),
            c if (c as u32) < 0x20 => {
                let _ = write!(s, "\\u{:04x}", c as u32);
            }
            c => s.push(c),
        }
    }
    s.push('"');
}

fn hex(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        let _ = write!(s, "{x:02x}");
    }
    s
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2)
        || !s
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

thread_local! {
    static RNG: Cell<u64> = Cell::new({
        let mut seed = [0u8; 8];
        // SAFETY: valid buffer of 8 bytes.
        unsafe { libc::getrandom(seed.as_mut_ptr().cast(), 8, 0) };
        u64::from_ne_bytes(seed) | 1
    });
}

/// xorshift64*: unique enough for span ids, no syscall per request.
fn next_u64() -> u64 {
    RNG.with(|r| {
        let mut x = r.get();
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        r.set(x);
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    })
}

pub fn random_id() -> [u8; 8] {
    loop {
        let v = next_u64();
        if v != 0 {
            return v.to_be_bytes();
        }
    }
}

fn random_unit() -> f64 {
    (next_u64() >> 11) as f64 / (1u64 << 53) as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trace_context() {
        let tp = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
        let c = TraceCtx::start(Some(tp), 0.0);
        assert!(c.sampled, "an incoming sampled flag wins over the ratio");
        assert_eq!(hex(&c.trace_id), "4bf92f3577b34da6a3ce929d0e0e4736");
        assert_eq!(
            c.parent.map(|p| hex(&p)).as_deref(),
            Some("00f067aa0ba902b7")
        );
        let child = random_id();
        let out = c.traceparent(child);
        assert_eq!(
            out.to_str().unwrap(),
            format!("00-4bf92f3577b34da6a3ce929d0e0e4736-{}-01", hex(&child))
        );
        for bad in [
            "00-00000000000000000000000000000000-00f067aa0ba902b7-01",
            "00-4BF92F3577B34DA6A3CE929D0E0E4736-00f067aa0ba902b7-01",
            "ff-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7",
            "nonsense",
        ] {
            assert!(parse_traceparent(bad).is_none(), "{bad}");
        }
        let fresh = TraceCtx::start(None, 1.0);
        assert!(fresh.sampled && fresh.parent.is_none());
        assert!(!TraceCtx::start(None, 0.0).sampled);
    }

    #[test]
    fn otlp_encoding() {
        let now = SystemTime::now();
        let json = otlp_json(
            "shop",
            &[Span {
                trace_id: [1; 16],
                span_id: [2; 8],
                parent: Some([3; 8]),
                name: "GET".into(),
                kind: Kind::Server,
                start: now,
                end: now + Duration::from_millis(5),
                attributes: vec![
                    ("url.path", "/a\"b".into()),
                    ("http.response.status_code", 500i64.into()),
                ],
                error: true,
            }],
        );
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let span = &v["resourceSpans"][0]["scopeSpans"][0]["spans"][0];
        assert_eq!(span["traceId"], "01010101010101010101010101010101");
        assert_eq!(span["parentSpanId"], "0303030303030303");
        assert_eq!(span["kind"], 2);
        assert_eq!(span["attributes"][0]["value"]["stringValue"], "/a\"b");
        assert_eq!(span["attributes"][1]["value"]["intValue"], "500");
        assert_eq!(span["status"]["code"], 2);
        assert_eq!(
            v["resourceSpans"][0]["resource"]["attributes"][0]["value"]["stringValue"],
            "shop"
        );
    }
}
