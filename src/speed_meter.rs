// SPDX-License-Identifier: LGPL-3.0-or-later
// Copyright (c) 2026 UFetch-Core contributors

//! 速度采样：把"累计字节数"折算成平滑的每秒速率。
//!
//! 只做一件事：给定 `(累计字节, 当前时刻)`，返回一个不易抖动的 B/s。
//! 采样间隔过短时会被限制为一次最小间隔，避免除零和虚高。

use std::time::{Duration, Instant};

/// 两次采样之间至少间隔这么久，否则沿用上一次的速率。
const MIN_INTERVAL: Duration = Duration::from_millis(150);
/// EMA 权重：新样本占多少。
const ALPHA: f64 = 0.45;

/// 累计计数器 → 速率。
#[derive(Debug, Default)]
pub struct CounterRate {
    last_total: u64,
    last_at: Option<Instant>,
    rate: f64,
}

impl CounterRate {
    /// 喂入新的累计值，返回平滑后的速率（B/s，取整）。第一次调用返回 0。
    pub fn observe(&mut self, total: u64, now: Instant) -> i64 {
        let Some(last_at) = self.last_at else {
            self.last_at = Some(now);
            self.last_total = total;
            return 0;
        };
        let elapsed = now.saturating_duration_since(last_at);
        if elapsed < MIN_INTERVAL {
            return self.rate.max(0.0) as i64;
        }
        let delta = total.saturating_sub(self.last_total) as f64;
        let instant = delta / elapsed.as_secs_f64();
        self.rate = if self.rate == 0.0 {
            instant
        } else {
            ALPHA * instant + (1.0 - ALPHA) * self.rate
        };
        self.last_total = total;
        self.last_at = Some(now);
        self.rate.max(0.0) as i64
    }

    /// 当前平滑速率。
    #[must_use]
    pub fn rate(&self) -> f64 {
        self.rate
    }

    /// 计数器回退或换任务时重置。
    pub fn reset(&mut self) {
        self.last_total = 0;
        self.last_at = None;
        self.rate = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_sample_has_no_rate() {
        let mut meter = CounterRate::default();
        assert_eq!(meter.observe(0, Instant::now()), 0);
    }

    #[test]
    fn computes_bytes_per_second_over_elapsed_time() {
        let start = Instant::now();
        let mut meter = CounterRate::default();
        meter.observe(0, start);
        let rate = meter.observe(512 * 1024, start + Duration::from_secs(1));
        assert!((rate - 512 * 1024).abs() < 1024, "got {rate}");
    }

    #[test]
    fn smoothing_keeps_a_spike_from_doubling_the_display() {
        let start = Instant::now();
        let mut meter = CounterRate::default();
        meter.observe(0, start);
        meter.observe(2 * 1024 * 1024, start + Duration::from_secs(1));
        let smoothed = meter.observe(2 * 1024 * 1024, start + Duration::from_secs(2));
        assert!(
            smoothed < 2 * 1024 * 1024,
            "decay after a silent second: {smoothed}"
        );
        assert!(
            smoothed > 1024 * 1024,
            "but not collapse instantly: {smoothed}"
        );
    }

    #[test]
    fn too_short_intervals_keep_the_previous_rate() {
        let start = Instant::now();
        let mut meter = CounterRate::default();
        meter.observe(0, start);
        let first = meter.observe(256 * 1024, start + Duration::from_secs(1));
        let second = meter.observe(512 * 1024, start + Duration::from_millis(1100));
        assert_eq!(first, second);
    }

    #[test]
    fn reset_clears_state() {
        let start = Instant::now();
        let mut meter = CounterRate::default();
        meter.observe(0, start);
        meter.observe(1024, start + Duration::from_secs(1));
        meter.reset();
        assert_eq!(meter.rate(), 0.0);
    }
}
