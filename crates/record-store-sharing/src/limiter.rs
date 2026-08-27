//! Abuse controls for public capability routes.
//!
//! Two different problems live behind one public surface, and conflating them
//! would break one while failing to stop the other. Guessing a share password is
//! authentication abuse: it is rare, it is per-link, and it must be throttled
//! hard. Fetching byte ranges of a video is normal: a single viewer seeking
//! through a file issues dozens of requests a minute and must never be slowed.
//! Probing for valid tokens is a third case, throttled per client rather than
//! per link because by definition no link is involved.
//!
//! Limits are held in process memory rather than in durable state. That is a
//! deliberate trade: a limiter that writes to disk on every public request would
//! turn a read path into a write path, and a restart merely resets a counter
//! rather than losing anything Record Store is authoritative for.

use std::{
    collections::HashMap,
    hash::Hash,
    sync::Mutex,
    time::{Duration, Instant},
};

/// How a limiter answered one request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateDecision {
    /// The request may proceed.
    Allowed,
    /// The request is throttled; retry after roughly this long.
    Throttled {
        /// Seconds a caller should wait, suitable for a `Retry-After` header.
        retry_after_seconds: u64,
    },
}

impl RateDecision {
    /// Whether the caller may proceed.
    #[must_use]
    pub const fn allowed(self) -> bool {
        matches!(self, Self::Allowed)
    }
}

/// A fixed-window counter over a bounded key space.
///
/// Bounded is the important word: the keys are attacker-influenced (a client
/// address, a link identifier), so an unbounded map would be a memory-exhaustion
/// primitive handed to the internet. When the map is full it is swept of expired
/// entries, and if it is still full the limiter fails closed.
#[derive(Debug)]
pub struct RateLimiter<K> {
    inner: Mutex<HashMap<K, Window>>,
    permitted: u32,
    window: Duration,
    capacity: usize,
}

#[derive(Debug, Clone, Copy)]
struct Window {
    started: Instant,
    count: u32,
}

impl<K: Eq + Hash + Clone> RateLimiter<K> {
    /// Creates a limiter permitting `permitted` events per `window` per key.
    #[must_use]
    pub fn new(permitted: u32, window: Duration, capacity: usize) -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            permitted: permitted.max(1),
            window,
            capacity: capacity.max(1),
        }
    }

    /// Records one event against `key` and reports whether it may proceed.
    pub fn check(&self, key: K) -> RateDecision {
        self.check_at(key, Instant::now())
    }

    fn check_at(&self, key: K, now: Instant) -> RateDecision {
        let Ok(mut map) = self.inner.lock() else {
            // A poisoned lock means another thread panicked while holding it.
            // Failing closed is the only safe reading of an abuse control.
            return RateDecision::Throttled {
                retry_after_seconds: self.window.as_secs().max(1),
            };
        };
        if map.len() >= self.capacity && !map.contains_key(&key) {
            map.retain(|_, window| now.duration_since(window.started) < self.window);
            if map.len() >= self.capacity {
                return RateDecision::Throttled {
                    retry_after_seconds: self.window.as_secs().max(1),
                };
            }
        }
        let entry = map.entry(key).or_insert(Window {
            started: now,
            count: 0,
        });
        if now.duration_since(entry.started) >= self.window {
            *entry = Window {
                started: now,
                count: 0,
            };
        }
        if entry.count >= self.permitted {
            let elapsed = now.duration_since(entry.started);
            let remaining = self.window.saturating_sub(elapsed);
            return RateDecision::Throttled {
                retry_after_seconds: remaining.as_secs().saturating_add(1),
            };
        }
        entry.count += 1;
        RateDecision::Allowed
    }

    /// Clears the counter for one key, used after a success.
    ///
    /// A correct password should not leave a visitor one attempt away from being
    /// throttled the next time they open the link.
    pub fn forget(&self, key: &K) {
        if let Ok(mut map) = self.inner.lock() {
            map.remove(key);
        }
    }

    /// Returns how many keys are currently tracked, for tests and metrics.
    #[must_use]
    pub fn tracked(&self) -> usize {
        self.inner.lock().map(|map| map.len()).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_is_throttled_only_after_its_allowance_is_spent() {
        let limiter = RateLimiter::new(3, Duration::from_secs(60), 16);
        let start = Instant::now();
        for attempt in 0..3 {
            assert!(
                limiter.check_at("visitor", start).allowed(),
                "attempt {attempt} should be allowed"
            );
        }
        let decision = limiter.check_at("visitor", start);
        assert!(!decision.allowed());
        assert!(matches!(
            decision,
            RateDecision::Throttled {
                retry_after_seconds
            } if retry_after_seconds > 0 && retry_after_seconds <= 61
        ));
    }

    #[test]
    fn separate_keys_do_not_throttle_one_another() {
        let limiter = RateLimiter::new(1, Duration::from_secs(60), 16);
        let start = Instant::now();
        assert!(limiter.check_at("first", start).allowed());
        assert!(limiter.check_at("second", start).allowed());
        assert!(!limiter.check_at("first", start).allowed());
    }

    #[test]
    fn the_allowance_returns_once_the_window_has_passed() {
        let limiter = RateLimiter::new(1, Duration::from_secs(60), 16);
        let start = Instant::now();
        assert!(limiter.check_at("visitor", start).allowed());
        assert!(!limiter.check_at("visitor", start).allowed());
        assert!(
            limiter
                .check_at("visitor", start + Duration::from_secs(61))
                .allowed()
        );
    }

    #[test]
    fn a_success_clears_the_visitor_s_record() {
        let limiter = RateLimiter::new(2, Duration::from_secs(60), 16);
        let start = Instant::now();
        assert!(limiter.check_at("visitor", start).allowed());
        limiter.forget(&"visitor");
        assert!(limiter.check_at("visitor", start).allowed());
        assert!(limiter.check_at("visitor", start).allowed());
        assert!(!limiter.check_at("visitor", start).allowed());
    }

    #[test]
    fn the_key_space_stays_bounded_under_a_flood_of_distinct_keys() {
        let limiter = RateLimiter::new(5, Duration::from_secs(60), 8);
        let start = Instant::now();
        for index in 0..1_000 {
            let _ = limiter.check_at(format!("visitor-{index}"), start);
        }
        assert!(limiter.tracked() <= 8, "limiter grew past its capacity");
    }

    #[test]
    fn expired_entries_are_swept_so_the_limiter_recovers_after_a_flood() {
        let limiter = RateLimiter::new(5, Duration::from_secs(60), 4);
        let start = Instant::now();
        for index in 0..16 {
            let _ = limiter.check_at(format!("visitor-{index}"), start);
        }
        let later = start + Duration::from_secs(61);
        assert!(
            limiter
                .check_at("fresh-visitor".to_owned(), later)
                .allowed()
        );
    }
}
