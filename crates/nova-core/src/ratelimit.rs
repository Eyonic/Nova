//! Per-client request rate limiting: one token bucket per IP address.
//!
//! Buckets live in sharded maps; idle buckets (full again) are swept
//! periodically so memory stays bounded by the number of active clients.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

const SHARDS: usize = 32;

#[derive(Clone, Copy)]
struct Bucket {
    tokens: f64,
    updated: Instant,
}

pub struct RateLimiter {
    rate: f64,
    burst: f64,
    shards: Vec<Mutex<HashMap<IpAddr, Bucket>>>,
    /// Reference point for `last_sweep_ms`.
    epoch: Instant,
    /// Milliseconds after `epoch` of the last sweep. An atomic, not a
    /// mutex: every request reads it, and a global lock here serialized
    /// all requests across shards.
    last_sweep_ms: AtomicU64,
}

impl RateLimiter {
    pub fn new(requests_per_sec: f64, burst: u32) -> Self {
        Self {
            rate: requests_per_sec,
            burst: burst as f64,
            shards: (0..SHARDS).map(|_| Mutex::new(HashMap::new())).collect(),
            epoch: Instant::now(),
            last_sweep_ms: AtomicU64::new(0),
        }
    }

    fn shard(&self, ip: IpAddr) -> &Mutex<HashMap<IpAddr, Bucket>> {
        let h = match ip {
            IpAddr::V4(v4) => u32::from(v4) as usize,
            // Group IPv6 clients by /64: one host usually owns the whole prefix.
            IpAddr::V6(v6) => (u128::from(v6) >> 64) as usize,
        };
        &self.shards[h.wrapping_mul(0x9E37_79B9) % SHARDS]
    }

    fn key(ip: IpAddr) -> IpAddr {
        match ip {
            IpAddr::V6(v6) => IpAddr::V6((u128::from(v6) & !((1u128 << 64) - 1)).into()),
            v4 => v4,
        }
    }

    /// Take one token. `Err(retry_after)` when the client must wait.
    pub fn check(&self, ip: IpAddr) -> Result<(), Duration> {
        self.check_at(ip, Instant::now())
    }

    fn check_at(&self, ip: IpAddr, now: Instant) -> Result<(), Duration> {
        self.maybe_sweep(now);
        let key = Self::key(ip);
        let mut m = self.shard(key).lock().unwrap();
        let b = m.entry(key).or_insert(Bucket {
            tokens: self.burst,
            updated: now,
        });
        let elapsed = now.saturating_duration_since(b.updated).as_secs_f64();
        b.tokens = (b.tokens + elapsed * self.rate).min(self.burst);
        b.updated = now;
        if b.tokens >= 1.0 {
            b.tokens -= 1.0;
            Ok(())
        } else {
            Err(Duration::from_secs_f64((1.0 - b.tokens) / self.rate))
        }
    }

    fn maybe_sweep(&self, now: Instant) {
        let now_ms = now.saturating_duration_since(self.epoch).as_millis() as u64;
        let last = self.last_sweep_ms.load(Ordering::Relaxed);
        // Exactly one caller wins the exchange and sweeps.
        if now_ms.saturating_sub(last) < 60_000
            || self
                .last_sweep_ms
                .compare_exchange(last, now_ms, Ordering::Relaxed, Ordering::Relaxed)
                .is_err()
        {
            return;
        }
        let refill = Duration::from_secs_f64(self.burst / self.rate);
        for shard in &self.shards {
            shard
                .lock()
                .unwrap()
                .retain(|_, b| now.saturating_duration_since(b.updated) < refill);
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.shards.iter().map(|s| s.lock().unwrap().len()).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bucket() {
        let rl = RateLimiter::new(10.0, 3);
        let ip: IpAddr = "192.0.2.1".parse().unwrap();
        let t = Instant::now();
        for _ in 0..3 {
            assert!(rl.check_at(ip, t).is_ok());
        }
        let wait = rl.check_at(ip, t).unwrap_err();
        assert!(wait <= Duration::from_millis(100));
        // Another client is unaffected.
        assert!(rl.check_at("192.0.2.2".parse().unwrap(), t).is_ok());
        // Refilled after 100 ms.
        assert!(rl.check_at(ip, t + Duration::from_millis(100)).is_ok());
    }

    #[test]
    fn ipv6_prefix_shares_a_bucket() {
        let rl = RateLimiter::new(1.0, 1);
        let t = Instant::now();
        assert!(rl.check_at("2001:db8::1".parse().unwrap(), t).is_ok());
        assert!(rl.check_at("2001:db8::2".parse().unwrap(), t).is_err());
    }

    #[test]
    fn sweeps_idle_clients() {
        let rl = RateLimiter::new(100.0, 10);
        let t = Instant::now();
        for i in 0..50u8 {
            rl.check_at(IpAddr::from([10, 0, 0, i]), t).unwrap();
        }
        assert_eq!(rl.len(), 50);
        rl.check_at("10.1.0.1".parse().unwrap(), t + Duration::from_secs(61))
            .unwrap();
        assert_eq!(rl.len(), 1);
    }
}
