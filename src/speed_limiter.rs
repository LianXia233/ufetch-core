// SPDX-License-Identifier: LGPL-3.0-or-later
// Copyright (c) 2026 UFetch-Core contributors

//! 全局令牌桶限速，所有连接、所有任务共享一个额度。
//!
//! 采用"按需要令牌、睡到够用为止"的按需补充方式，不需要后台任务：
//! 每次 `consume()` 先按 `limit * 经过时间` 补桶，够就扣，不够就睡一小段再来。
//! 这样限速值在运行中改变也能立刻生效。

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// 单次等待的上限：保证 `set_limit()` 之后最多 50 ms 就能按新速率走。
const MAX_SLEEP: Duration = Duration::from_millis(50);

#[derive(Debug)]
pub struct SpeedLimiter {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    limit_bps: AtomicU64,
    bucket: Mutex<Bucket>,
    waits: AtomicU64,
}

#[derive(Debug)]
struct Bucket {
    tokens: f64,
    last: Instant,
}

impl Bucket {
    fn new(limit: u64) -> Self {
        Self {
            tokens: burst(limit),
            last: Instant::now(),
        }
    }

    fn refill(&mut self, limit: u64, now: Instant) {
        let elapsed = now.saturating_duration_since(self.last).as_secs_f64();
        if elapsed > 0.0 {
            self.tokens = (self.tokens + elapsed * limit as f64).min(burst(limit));
            self.last = now;
        }
    }
}

/// 允许的突发量：至少 4 KiB，最多 1/10 秒的额度（避免大文件开头一口气冲出去）。
fn burst(limit: u64) -> f64 {
    (limit / 10).max(4096) as f64
}

impl Clone for SpeedLimiter {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl SpeedLimiter {
    /// `limit_bps = 0` 表示不限速。
    #[must_use]
    pub fn new(limit_bps: u64) -> Self {
        Self {
            inner: Arc::new(Inner {
                limit_bps: AtomicU64::new(limit_bps),
                bucket: Mutex::new(Bucket::new(limit_bps)),
                waits: AtomicU64::new(0),
            }),
        }
    }

    /// 运行中调整限速（0 = 取消限速）。
    pub fn set_limit(&self, limit_bps: u64) {
        self.inner.limit_bps.store(limit_bps, Ordering::Relaxed);
    }

    /// 当前限速值（B/s）。
    #[must_use]
    pub fn limit(&self) -> u64 {
        self.inner.limit_bps.load(Ordering::Relaxed)
    }

    /// 因为限速而睡过的次数（诊断用）。
    #[must_use]
    pub fn throttle_waits(&self) -> u64 {
        self.inner.waits.load(Ordering::Relaxed)
    }

    /// 申请 `want` 字节的发送额度。返回本次允许发送的字节数（可能小于 `want`）。
    pub async fn consume(&self, want: u64) -> u64 {
        let limit = self.limit();
        if limit == 0 || want == 0 {
            return want;
        }
        loop {
            match self.take(want) {
                Ok(grant) => return grant,
                Err(wait) => {
                    self.inner.waits.fetch_add(1, Ordering::Relaxed);
                    tokio::time::sleep(wait.min(MAX_SLEEP)).await;
                }
            }
        }
    }

    /// 同步扣一次桶：成功返回额度，不够则返回"还需要等多久"。
    fn take(&self, want: u64) -> Result<u64, Duration> {
        let limit = self.limit();
        let now = Instant::now();
        let mut bucket = self
            .inner
            .bucket
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        bucket.refill(limit, now);
        if bucket.tokens >= 1.0 {
            let grant = (bucket.tokens as u64).min(want).max(1);
            bucket.tokens -= grant as f64;
            return Ok(grant);
        }
        Err(Duration::from_secs_f64(
            (1.0 - bucket.tokens) / limit as f64,
        ))
    }

    /// 兼容旧接口：当前实现按需补桶，没有后台任务，这里什么都不做。
    pub fn spawn_refill_task(&self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unlimited_grants_the_full_request() {
        let limiter = SpeedLimiter::new(0);
        assert_eq!(limiter.consume(123_456).await, 123_456);
    }

    #[tokio::test]
    async fn zero_request_is_a_no_op() {
        let limiter = SpeedLimiter::new(10_000);
        assert_eq!(limiter.consume(0).await, 0);
    }

    #[tokio::test]
    async fn caps_the_grant_and_forces_waiting() {
        let limiter = SpeedLimiter::new(32 * 1024); // 32 KiB/s
        let start = Instant::now();
        let mut sent = 0u64;
        while sent < 48 * 1024 {
            let grant = limiter.consume(48 * 1024 - sent).await;
            assert!(grant > 0 && grant <= 48 * 1024, "grant {grant}");
            sent += grant;
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(700),
            "48 KiB at 32 KiB/s cannot finish in {elapsed:?}"
        );
        assert!(limiter.throttle_waits() > 0);
    }

    #[tokio::test]
    async fn raising_the_limit_takes_effect_quickly() {
        let limiter = SpeedLimiter::new(8 * 1024);
        let slow = limiter.limit();
        assert_eq!(slow, 8 * 1024);
        limiter.set_limit(0);
        let start = Instant::now();
        assert_eq!(limiter.consume(1024 * 1024).await, 1024 * 1024);
        assert!(start.elapsed() < Duration::from_secs(1));
    }
}
