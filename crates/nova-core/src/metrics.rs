//! Process-wide counters exported in Prometheus text format.

use std::fmt::Write;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Static,
    Image,
    Php,
    Internal,
    Error,
}

impl Kind {
    pub const ALL: [Kind; 5] = [
        Kind::Static,
        Kind::Image,
        Kind::Php,
        Kind::Internal,
        Kind::Error,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Static => "static",
            Kind::Image => "image",
            Kind::Php => "php",
            Kind::Internal => "internal",
            Kind::Error => "error",
        }
    }
}

#[derive(Default)]
pub struct Metrics {
    requests: [AtomicU64; 5],
    status: [AtomicU64; 6], // index = status / 100
    php_micros: AtomicU64,
    php_count: AtomicU64,
    php_errors: AtomicU64,
    bytes_saved: AtomicU64,
}

impl Metrics {
    pub fn record(&self, kind: Kind, status: u16) {
        self.requests[kind as usize].fetch_add(1, Relaxed);
        self.status[(status / 100).min(5) as usize].fetch_add(1, Relaxed);
    }

    pub fn php_done(&self, micros: u64, ok: bool) {
        self.php_micros.fetch_add(micros, Relaxed);
        self.php_count.fetch_add(1, Relaxed);
        if !ok {
            self.php_errors.fetch_add(1, Relaxed);
        }
    }

    pub fn image_saved(&self, bytes: u64) {
        self.bytes_saved.fetch_add(bytes, Relaxed);
    }

    pub fn render(&self, extra: &[(&str, &str, u64)]) -> String {
        let mut s = String::new();
        let _ = writeln!(s, "# TYPE nova_requests_total counter");
        for k in Kind::ALL {
            let _ = writeln!(
                s,
                "nova_requests_total{{kind=\"{}\"}} {}",
                k.as_str(),
                self.requests[k as usize].load(Relaxed)
            );
        }
        let _ = writeln!(s, "# TYPE nova_responses_total counter");
        for (i, c) in self.status.iter().enumerate().skip(1) {
            let _ = writeln!(
                s,
                "nova_responses_total{{class=\"{i}xx\"}} {}",
                c.load(Relaxed)
            );
        }
        let _ = writeln!(s, "# TYPE nova_php_request_duration_seconds summary");
        let _ = writeln!(
            s,
            "nova_php_request_duration_seconds_sum {:.6}",
            self.php_micros.load(Relaxed) as f64 / 1e6
        );
        let _ = writeln!(
            s,
            "nova_php_request_duration_seconds_count {}",
            self.php_count.load(Relaxed)
        );
        let _ = writeln!(s, "# TYPE nova_php_errors_total counter");
        let _ = writeln!(s, "nova_php_errors_total {}", self.php_errors.load(Relaxed));
        let _ = writeln!(s, "# TYPE nova_image_bytes_saved_total counter");
        let _ = writeln!(
            s,
            "nova_image_bytes_saved_total {}",
            self.bytes_saved.load(Relaxed)
        );
        for (name, help, v) in extra {
            let _ = writeln!(s, "# HELP {name} {help}\n# TYPE {name} gauge\n{name} {v}");
        }
        s
    }
}
