//! NOVA Live, server side: serves the browser runtime and a per-site
//! invalidation hub over Server-Sent Events.
//!
//! The event stream never carries page content. A PHP response publishes
//! with a `Nova-Publish: <channel>[, <channel>]` header; subscribed browsers
//! receive `event: invalidate` / `data: <channel>` and refetch their regions
//! through the normal request path, with their own cookies. Authorization
//! therefore stays in the application, and no PHP worker is held open per
//! connected browser.

use bytes::Bytes;
use futures_util::StreamExt;
use http::{HeaderValue, Response, StatusCode, header};
use http_body_util::{BodyExt, StreamBody};
use hyper::body::Frame;
use nova_config::LiveConfig;
use nova_http::{Body, full};
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{broadcast, watch};

pub const RUNTIME_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The browser runtime, embedded in the binary, stamped with the version.
pub static RUNTIME_JS: std::sync::LazyLock<Bytes> = std::sync::LazyLock::new(|| {
    Bytes::from(include_str!("../assets/live.js").replace("__NOVA_VERSION__", RUNTIME_VERSION))
});

/// Response header PHP uses to publish invalidations.
pub const PUBLISH_HEADER: &str = "nova-publish";

/// `[a-z0-9]` followed by up to 63 of `[a-z0-9._:-]`.
pub fn valid_channel(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.as_bytes()[0].is_ascii_alphanumeric()
        && s.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b':' | b'-')
        })
}

/// Channels named in a `Nova-Publish` header value; invalid names are dropped.
pub fn parse_channels(value: &str) -> Vec<String> {
    let mut out: Vec<String> = value
        .split(',')
        .map(|c| c.trim().to_string())
        .filter(|c| valid_channel(c))
        .collect();
    out.sort();
    out.dedup();
    out
}

#[derive(Clone, Debug)]
struct Event {
    site: Arc<str>,
    channel: Arc<str>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum SubscribeError {
    BadChannels,
    TooManyChannels,
    TooManyConnections,
}

pub struct LiveHub {
    cfg: LiveConfig,
    tx: broadcast::Sender<Event>,
    connections: Arc<AtomicUsize>,
    per_ip: Arc<Mutex<HashMap<IpAddr, usize>>>,
    published: AtomicU64,
    closed: watch::Sender<bool>,
}

/// Releases a connection slot when the stream is dropped.
struct Slot {
    connections: Arc<AtomicUsize>,
    per_ip: Arc<Mutex<HashMap<IpAddr, usize>>>,
    ip: IpAddr,
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.connections.fetch_sub(1, Ordering::Relaxed);
        let mut m = self.per_ip.lock().unwrap();
        if let Some(n) = m.get_mut(&self.ip) {
            *n -= 1;
            if *n == 0 {
                m.remove(&self.ip);
            }
        }
    }
}

impl LiveHub {
    pub fn new(cfg: LiveConfig) -> Self {
        let (tx, _) = broadcast::channel(1024);
        let (closed, _) = watch::channel(false);
        Self {
            cfg,
            tx,
            connections: Arc::new(AtomicUsize::new(0)),
            per_ip: Arc::new(Mutex::new(HashMap::new())),
            published: AtomicU64::new(0),
            closed,
        }
    }

    pub fn enabled(&self) -> bool {
        self.cfg.enabled
    }

    pub fn connections(&self) -> usize {
        self.connections.load(Ordering::Relaxed)
    }

    pub fn published(&self) -> u64 {
        self.published.load(Ordering::Relaxed)
    }

    /// Notify a site's subscribers that `channel` changed.
    pub fn publish(&self, site: &str, channel: &str) {
        self.published.fetch_add(1, Ordering::Relaxed);
        // No receivers is fine: nobody is watching.
        let _ = self.tx.send(Event {
            site: site.into(),
            channel: channel.into(),
        });
    }

    /// End every open stream (server shutdown), so draining does not wait
    /// for browsers that would otherwise stay connected forever.
    pub fn close(&self) {
        let _ = self.closed.send(true);
    }

    /// Open an event stream for `site`, filtered to `channels`.
    pub fn subscribe(
        &self,
        site: &str,
        channels_param: &str,
        ip: IpAddr,
    ) -> Result<Response<Body>, SubscribeError> {
        let channels = parse_channels(channels_param);
        let requested = channels_param
            .split(',')
            .filter(|c| !c.trim().is_empty())
            .count();
        if channels.is_empty() || channels.len() != requested {
            return Err(SubscribeError::BadChannels);
        }
        if channels.len() > self.cfg.max_channels {
            return Err(SubscribeError::TooManyChannels);
        }
        {
            let mut m = self.per_ip.lock().unwrap();
            let n = m.entry(ip).or_insert(0);
            if *n >= self.cfg.max_connections_per_ip
                || self.connections.load(Ordering::Relaxed) >= self.cfg.max_connections
            {
                if *n == 0 {
                    m.remove(&ip);
                }
                return Err(SubscribeError::TooManyConnections);
            }
            *n += 1;
            self.connections.fetch_add(1, Ordering::Relaxed);
        }
        let slot = Slot {
            connections: Arc::clone(&self.connections),
            per_ip: Arc::clone(&self.per_ip),
            ip,
        };

        struct State {
            rx: broadcast::Receiver<Event>,
            closed: watch::Receiver<bool>,
            heartbeat: tokio::time::Interval,
            site: Arc<str>,
            channels: Vec<String>,
            _slot: Slot,
        }
        let mut heartbeat = tokio::time::interval(Duration::from_secs(self.cfg.heartbeat_secs));
        heartbeat.reset(); // first tick after one period, not immediately
        let state = State {
            rx: self.tx.subscribe(),
            closed: self.closed.subscribe(),
            heartbeat,
            site: site.into(),
            channels,
            _slot: slot,
        };
        let hello = futures_util::stream::once(async {
            Ok::<_, std::io::Error>(Frame::data(Bytes::from_static(
                b"retry: 3000\n: connected\n\n",
            )))
        });
        let events = futures_util::stream::unfold(state, |mut st| async move {
            loop {
                if *st.closed.borrow() {
                    return None;
                }
                let chunk: Bytes = tokio::select! {
                    _ = st.closed.changed() => return None,
                    _ = st.heartbeat.tick() => Bytes::from_static(b": ping\n\n"),
                    ev = st.rx.recv() => match ev {
                        Ok(ev) if *ev.site == *st.site && st.channels.iter().any(|c| **c == *ev.channel) => {
                            Bytes::from(format!("event: invalidate\ndata: {}\n\n", ev.channel))
                        }
                        Ok(_) => continue,
                        // Missed events: tell the client to refresh everything it shows.
                        Err(broadcast::error::RecvError::Lagged(_)) => Bytes::from_static(b"event: reset\ndata: *\n\n"),
                        Err(broadcast::error::RecvError::Closed) => return None,
                    },
                };
                return Some((Ok::<_, std::io::Error>(Frame::data(chunk)), st));
            }
        });
        let body = StreamBody::new(hello.chain(events)).boxed_unsync();
        Ok(Response::builder()
            .header(header::CONTENT_TYPE, "text/event-stream; charset=utf-8")
            .header(header::CACHE_CONTROL, "no-store")
            .header("x-accel-buffering", "no")
            .body(body)
            .unwrap())
    }

    /// `GET /_nova/live.js`. Versioned URLs (`?v=<version>`) are immutable.
    pub fn runtime_response(query: Option<&str>) -> Response<Body> {
        let versioned =
            query.is_some_and(|q| q.split('&').any(|kv| kv == format!("v={RUNTIME_VERSION}")));
        let cache = if versioned {
            "public, max-age=31536000, immutable"
        } else {
            "public, max-age=300"
        };
        Response::builder()
            .header(header::CONTENT_TYPE, "text/javascript; charset=utf-8")
            .header(header::CACHE_CONTROL, cache)
            .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
            .header(
                header::ETAG,
                HeaderValue::from_static(concat!("\"live-", env!("CARGO_PKG_VERSION"), "\"")),
            )
            .body(full(RUNTIME_JS.clone()))
            .unwrap()
    }

    pub fn error_response(e: SubscribeError) -> Response<Body> {
        let (status, msg) = match e {
            SubscribeError::BadChannels => (StatusCode::BAD_REQUEST, "invalid channels"),
            SubscribeError::TooManyChannels => (StatusCode::BAD_REQUEST, "too many channels"),
            SubscribeError::TooManyConnections => {
                (StatusCode::SERVICE_UNAVAILABLE, "too many live connections")
            }
        };
        let mut r = Response::new(full(msg));
        *r.status_mut() = status;
        r.headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        if status == StatusCode::SERVICE_UNAVAILABLE {
            r.headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from_static("10"));
        }
        r
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;

    fn hub(max: usize, per_ip: usize) -> LiveHub {
        LiveHub::new(LiveConfig {
            max_connections: max,
            max_connections_per_ip: per_ip,
            max_channels: 3,
            heartbeat_secs: 60,
            enabled: true,
        })
    }

    async fn next_text(body: &mut Body) -> String {
        let frame = tokio::time::timeout(Duration::from_secs(2), body.frame())
            .await
            .expect("frame in time")
            .unwrap()
            .unwrap();
        String::from_utf8(frame.into_data().unwrap().to_vec()).unwrap()
    }

    #[test]
    fn channel_names() {
        assert!(valid_channel("notes"));
        assert!(valid_channel("orders:42"));
        assert!(!valid_channel(""));
        assert!(!valid_channel("Notes"));
        assert!(!valid_channel("-x"));
        assert!(!valid_channel("a b"));
        assert!(!valid_channel(&"a".repeat(65)));
        assert_eq!(parse_channels(" b, a ,a, BAD, "), ["a", "b"]);
    }

    #[tokio::test]
    async fn delivers_only_own_site_and_channels() {
        let h = hub(10, 10);
        let ip: IpAddr = "10.0.0.1".parse().unwrap();
        let mut body = h
            .subscribe("site-a", "notes,stock", ip)
            .unwrap()
            .into_body();
        assert!(next_text(&mut body).await.contains("connected"));
        h.publish("site-b", "notes"); // other site: must not arrive
        h.publish("site-a", "other"); // other channel: must not arrive
        h.publish("site-a", "stock");
        assert_eq!(
            next_text(&mut body).await,
            "event: invalidate\ndata: stock\n\n"
        );
        assert_eq!(h.published(), 3);
    }

    #[tokio::test]
    async fn enforces_limits_and_releases_slots() {
        let h = hub(2, 1);
        let a: IpAddr = "10.0.0.1".parse().unwrap();
        let b: IpAddr = "10.0.0.2".parse().unwrap();
        let c: IpAddr = "10.0.0.3".parse().unwrap();
        assert_eq!(
            h.subscribe("s", "x,Y", a).unwrap_err(),
            SubscribeError::BadChannels
        );
        assert_eq!(
            h.subscribe("s", "a,b,c,d", a).unwrap_err(),
            SubscribeError::TooManyChannels
        );
        let first = h.subscribe("s", "x", a).unwrap();
        assert_eq!(
            h.subscribe("s", "x", a).unwrap_err(),
            SubscribeError::TooManyConnections
        ); // per IP
        let second = h.subscribe("s", "x", b).unwrap();
        assert_eq!(
            h.subscribe("s", "x", c).unwrap_err(),
            SubscribeError::TooManyConnections
        ); // global
        assert_eq!(h.connections(), 2);
        drop(first);
        drop(second);
        assert_eq!(h.connections(), 0);
        assert!(h.subscribe("s", "x", a).is_ok());
    }

    #[tokio::test]
    async fn close_ends_streams() {
        let h = hub(10, 10);
        let mut body = h
            .subscribe("s", "x", "10.0.0.1".parse().unwrap())
            .unwrap()
            .into_body();
        let _ = next_text(&mut body).await;
        h.close();
        let end = tokio::time::timeout(Duration::from_secs(2), body.frame())
            .await
            .unwrap();
        assert!(end.is_none());
    }
}
