//! In-memory sliding-window rate limiter (M25a).
//!
//! Multi-dimensional: per-key (e.g. client IP) and global buckets using a
//! fixed 1-second tick grid. Counters live in memory only — no DB on the hot
//! path. Windows are configurable per bucket; expired slots are pruned lazily
//! on each check.

use std::collections::HashMap;

use tokio::sync::Mutex;

/// One sliding-window counter for a key.
#[derive(Debug, Clone)]
struct WindowCounter {
    /// Millisecond-aligned slot timestamps with their hit counts.
    slots: Vec<(u64, u32)>,
    /// Window length in seconds.
    window_secs: u64,
}

impl WindowCounter {
    fn new(window_secs: u64) -> Self {
        Self {
            slots: Vec::new(),
            window_secs,
        }
    }

    /// Record one hit and return whether the window is now over limit.
    fn hit(&mut self, now_ms: u64, limit: u32) -> bool {
        let window_ms = self.window_secs * 1000;
        let start = now_ms.saturating_sub(window_ms);
        self.slots.retain(|(ts, _)| *ts >= start);
        // Aggregate into the current second slot.
        let slot = now_ms / 1000 * 1000;
        if let Some(last) = self.slots.last_mut() {
            if last.0 == slot {
                last.1 += 1;
            } else {
                self.slots.push((slot, 1));
            }
        } else {
            self.slots.push((slot, 1));
        }
        let total: u32 = self.slots.iter().map(|(_, c)| *c).sum();
        total > limit
    }
}

/// Rate limiter with per-key and global buckets.
#[derive(Debug)]
pub struct RateLimiter {
    /// Per-key buckets: key -> (window_secs, counter).
    key_buckets: Mutex<HashMap<String, (u64, WindowCounter)>>,
    /// Global buckets: endpoint -> counter.
    global_buckets: Mutex<HashMap<String, WindowCounter>>,
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

impl RateLimiter {
    pub fn new() -> Self {
        Self {
            key_buckets: Mutex::new(HashMap::new()),
            global_buckets: Mutex::new(HashMap::new()),
        }
    }

    /// Check a key-scoped window (e.g. per-IP). Returns `true` when the
    /// request should be REJECTED (over limit), `false` when allowed.
    pub async fn check_key(&self, key: &str, window_secs: u64, limit: u32) -> bool {
        let now_ms = now_ms();
        let mut buckets = self.key_buckets.lock().await;
        let entry = buckets
            .entry(key.to_string())
            .or_insert_with(|| (window_secs, WindowCounter::new(window_secs)));
        // Re-bucket if the window configuration changed.
        if entry.0 != window_secs {
            *entry = (window_secs, WindowCounter::new(window_secs));
        }
        entry.1.hit(now_ms, limit)
    }

    /// Check a global window (e.g. all logins). Returns `true` when REJECTED.
    pub async fn check_global(&self, bucket: &str, window_secs: u64, limit: u32) -> bool {
        let now_ms = now_ms();
        let mut buckets = self.global_buckets.lock().await;
        let entry = buckets
            .entry(bucket.to_string())
            .or_insert_with(|| WindowCounter::new(window_secs));
        if entry.window_secs != window_secs {
            *entry = WindowCounter::new(window_secs);
        }
        entry.hit(now_ms, limit)
    }

    /// Prune all expired slots (called opportunistically; cheap).
    pub async fn prune(&self) {
        let now_ms = now_ms();
        let mut kb = self.key_buckets.lock().await;
        kb.retain(|_, (window_secs, counter)| {
            let start = now_ms.saturating_sub(*window_secs * 1000);
            counter.slots.retain(|(ts, _)| *ts >= start);
            !counter.slots.is_empty()
        });
        drop(kb);
        let mut gb = self.global_buckets.lock().await;
        gb.retain(|_, counter| {
            let start = now_ms.saturating_sub(counter.window_secs * 1000);
            counter.slots.retain(|(ts, _)| *ts >= start);
            !counter.slots.is_empty()
        });
    }
}

fn now_ms() -> u64 {
    // Unix-millis via std (no chrono needed); stable across buckets.
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or_default())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn sliding_window_counts_within_limit() {
        let rl = RateLimiter::new();
        // 5 allowed, 6th rejected (limit 5, window 60s).
        for _ in 0..5 {
            assert!(!rl.check_key("ip-1", 60, 5).await);
        }
        assert!(rl.check_key("ip-1", 60, 5).await);
        // Different key unaffected.
        assert!(!rl.check_key("ip-2", 60, 5).await);
    }

    #[tokio::test]
    async fn global_bucket_independent_of_key() {
        let rl = RateLimiter::new();
        for _ in 0..3 {
            assert!(!rl.check_key("a", 60, 10).await);
            assert!(!rl.check_key("b", 60, 10).await);
        }
        // Global limit 5 across all keys.
        for i in 0..5 {
            assert!(!rl.check_global("login", 60, 5).await, "global hit {i}");
        }
        assert!(rl.check_global("login", 60, 5).await);
    }

    #[tokio::test]
    async fn windows_expire() {
        let rl = RateLimiter::new();
        // Tiny window (1s) — 6th hit after sleep passes.
        for _ in 0..5 {
            assert!(!rl.check_key("x", 1, 5).await);
        }
        assert!(rl.check_key("x", 1, 5).await);
        tokio::time::sleep(Duration::from_millis(1100)).await;
        assert!(!rl.check_key("x", 1, 5).await, "window should have expired");
    }
}
