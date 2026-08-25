use std::collections::HashMap;
use std::net::IpAddr;
use std::num::NonZeroU32;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use thiserror::Error;

/// Returned by [`PeerLimiter::new`] when `capacity` or `refill_per_sec` is
/// zero, negative, or NaN. `try_consume` divides by `refill_per_sec` and
/// feeds the quotient to `Duration::from_secs_f64`, which panics on
/// non-finite input — this error makes that state unconstructible instead
/// of merely `debug_assert`-ed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("capacity and refill_per_sec must be positive and finite")]
pub struct InvalidRate;

/// Per-peer token-bucket limiter. Each peer (keyed by `IpAddr`) gets a
/// bucket that refills continuously toward `capacity` at `refill_per_sec`.
/// `try_consume` deducts one token and returns the wait time when empty.
pub struct PeerLimiter {
    capacity: f64,
    refill_per_sec: f64,
    buckets: Mutex<HashMap<IpAddr, Bucket>>,
    evict_threshold: usize,
}

/// A single peer's token-bucket state: current token count and the instant of
/// the last `try_consume` (used to compute refill on the next call).
#[derive(Clone, Copy)]
struct Bucket {
    tokens: f64,
    last: Instant,
}

impl PeerLimiter {
    pub fn new(
        capacity: f64,
        refill_per_sec: f64,
        evict_threshold: usize,
    ) -> Result<Self, InvalidRate> {
        // NaN fails both comparisons, so it lands here too.
        if !(capacity > 0.0 && refill_per_sec > 0.0) {
            return Err(InvalidRate);
        }
        Ok(Self {
            capacity,
            refill_per_sec,
            buckets: Mutex::new(HashMap::new()),
            evict_threshold,
        })
    }

    /// Convenience: a per-minute rate (capacity = `rate`, refill = `rate`/60s).
    pub fn per_minute(rate: NonZeroU32) -> Self {
        let cap = f64::from(rate.get());
        Self::new(cap, cap / 60.0, 4096).expect("derived from a NonZeroU32 rate")
    }

    /// Try to consume one token. Returns the time until the next token would
    /// be available when the bucket is empty.
    pub fn try_consume(&self, ip: IpAddr) -> Result<(), Duration> {
        let now = Instant::now();
        let mut guard = self.buckets.lock().expect("limiter mutex poisoned");

        if guard.len() > self.evict_threshold {
            let stale = self.stale_after();
            guard.retain(|_, b| now.duration_since(b.last) < stale);
        }

        let bucket = guard.entry(ip).or_insert(Bucket {
            tokens: self.capacity,
            last: now,
        });
        let elapsed = now.duration_since(bucket.last).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * self.refill_per_sec).min(self.capacity);
        bucket.last = now;

        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            Ok(())
        } else {
            let needed = 1.0 - bucket.tokens;
            Err(Duration::from_secs_f64(needed / self.refill_per_sec))
        }
    }

    /// The idle duration after which a peer's bucket is considered stale and
    /// eligible for eviction. Set to twice the time it takes to fully refill
    /// an empty bucket, so a peer that briefly goes idle isn't dropped.
    fn stale_after(&self) -> Duration {
        // Twice the time it takes to fully refill an empty bucket.
        Duration::from_secs_f64((self.capacity / self.refill_per_sec) * 2.0)
    }

    #[cfg(test)]
    pub(crate) fn bucket_count(&self) -> usize {
        self.buckets.lock().unwrap().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn ip(n: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(127, 0, 0, n))
    }

    #[test]
    fn allows_within_capacity() {
        let l = PeerLimiter::new(3.0, 0.001, 1024).unwrap(); // refill effectively zero
        for _ in 0..3 {
            assert!(l.try_consume(ip(1)).is_ok());
        }
    }

    #[test]
    fn blocks_after_exhaustion() {
        let l = PeerLimiter::new(2.0, 0.001, 1024).unwrap();
        l.try_consume(ip(1)).unwrap();
        l.try_consume(ip(1)).unwrap();
        let err = l
            .try_consume(ip(1))
            .expect_err("third call should be rate-limited");
        assert!(err > Duration::from_secs(0));
    }

    #[test]
    fn separate_ips_get_separate_buckets() {
        let l = PeerLimiter::new(1.0, 0.001, 1024).unwrap();
        assert!(l.try_consume(ip(1)).is_ok());
        assert!(l.try_consume(ip(2)).is_ok());
        assert!(l.try_consume(ip(1)).is_err());
    }

    #[test]
    fn new_rejects_non_positive_rates() {
        assert!(PeerLimiter::new(0.0, 1.0, 8).is_err());
        assert!(PeerLimiter::new(1.0, 0.0, 8).is_err());
        assert!(PeerLimiter::new(-1.0, 1.0, 8).is_err());
        assert!(PeerLimiter::new(1.0, f64::NAN, 8).is_err());
        assert!(PeerLimiter::new(1.0, 1.0, 8).is_ok());
    }

    #[test]
    fn refills_over_time() {
        // 10 tokens/sec → one token per 100ms.
        let l = PeerLimiter::new(1.0, 10.0, 1024).unwrap();
        l.try_consume(ip(1)).unwrap();
        assert!(l.try_consume(ip(1)).is_err());
        std::thread::sleep(Duration::from_millis(150));
        assert!(l.try_consume(ip(1)).is_ok());
    }

    #[test]
    fn evicts_stale_entries_when_threshold_exceeded() {
        // Tiny threshold + fast refill so the prior entry is "stale" by the
        // time we hit the threshold.
        let l = PeerLimiter::new(1.0, 1000.0, 4).unwrap();
        for n in 0..4 {
            l.try_consume(ip(n)).unwrap();
        }
        // Sleep long enough that all earlier buckets are stale (>2× refill window).
        std::thread::sleep(Duration::from_millis(20));
        // Crossing the threshold triggers eviction inside try_consume.
        for n in 4..10 {
            l.try_consume(ip(n)).unwrap();
        }
        assert!(
            l.bucket_count() <= 6,
            "bucket count should have shrunk after eviction, got {}",
            l.bucket_count()
        );
    }
}
