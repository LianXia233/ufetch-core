// SPDX-License-Identifier: LGPL-3.0-or-later
// Copyright (c) 2026 UFetch-Core contributors

//! 根据文件大小与实测带宽给出"建议连接数"。
//!
//! 这里只是**建议**：真正的起停由引擎的爬升逻辑决定。原则有两条：
//! 1. 小文件不值得为 HTTP 头开销多开连接；
//! 2. 慢链路上多开连接反而可能绕开单连接限速，所以不要一味按带宽缩到 1。

/// 输入：总大小与服务器是否支持 Range。
#[derive(Debug, Clone, Copy)]
pub struct AdvisorInput {
    pub supports_range: bool,
    pub total_bytes: i64,
}

/// 输出：建议连接数与一句可读的理由（写日志用）。
#[derive(Debug, Clone)]
pub struct SegmentAdvice {
    pub reason: String,
    pub segments: i32,
}

const MIB: i64 = 1024 * 1024;

/// 只按文件大小估算，不考虑带宽。
#[must_use]
pub fn advise_static(input: &AdvisorInput) -> SegmentAdvice {
    if !input.supports_range || input.total_bytes <= 0 {
        return SegmentAdvice {
            segments: 1,
            reason: "no Range support or unknown size".into(),
        };
    }
    let total = input.total_bytes;
    // 少于 2 MiB 的文件直接单流：分段的握手与区间开销并不划算。
    let segments = match total {
        value if value < 2 * MIB => 1,
        value if value < 8 * MIB => 2,
        value if value < 32 * MIB => 4,
        value if value < 256 * MIB => 6,
        _ => 8,
    };
    SegmentAdvice {
        segments,
        reason: format!("{total} B, by size -> {segments}"),
    }
}

/// 在静态建议上叠加实测带宽。
///
/// - `< 512 KiB/s`：保留静态建议（多连接通常能绕过单连接限速），但不低于 2；
/// - `512 KiB/s – 4 MiB/s`：线性收敛到静态值；
/// - `> 4 MiB/s`：最多在静态值上加 2，避免把路由器打爆。
#[must_use]
pub fn advise_with_bandwidth(input: &AdvisorInput, bandwidth_bps: f64) -> SegmentAdvice {
    let base = advise_static(input);
    if base.segments <= 1 || !bandwidth_bps.is_finite() || bandwidth_bps <= 0.0 {
        return base;
    }
    let static_segments = base.segments as f64;
    let factor = if bandwidth_bps < 512.0 * 1024.0 {
        1.0
    } else if bandwidth_bps < 4.0 * 1024.0 * 1024.0 {
        let t = (bandwidth_bps - 512.0 * 1024.0) / (4.0 * 1024.0 * 1024.0 - 512.0 * 1024.0);
        1.0 - 0.25 * t
    } else {
        0.75
    };
    let scaled = (static_segments * factor).ceil() as i32;
    let segments = if bandwidth_bps > 4.0 * 1024.0 * 1024.0 {
        (base.segments + 2).min(8)
    } else {
        scaled.clamp(2, base.segments)
    };
    SegmentAdvice {
        segments,
        reason: format!(
            "static={}, measured={:.0} KB/s -> {}",
            base.segments,
            bandwidth_bps / 1024.0,
            segments
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(total: i64, ranges: bool) -> AdvisorInput {
        AdvisorInput {
            total_bytes: total,
            supports_range: ranges,
        }
    }

    #[test]
    fn without_range_or_size_we_stay_single_stream() {
        assert_eq!(advise_static(&input(10 << 20, false)).segments, 1);
        assert_eq!(advise_static(&input(0, true)).segments, 1);
    }

    #[test]
    fn tiny_files_stay_single_stream() {
        assert_eq!(advise_static(&input(1 << 20, true)).segments, 1);
        assert_eq!(advise_static(&input(2 << 20, true)).segments, 2);
    }

    #[test]
    fn large_files_scale_up_but_stay_bounded() {
        assert_eq!(advise_static(&input(32 << 20, true)).segments, 6);
        assert_eq!(advise_static(&input(4 << 30, true)).segments, 8);
    }

    #[test]
    fn bandwidth_scaling_never_drops_below_two_or_above_cap() {
        let advice = advise_with_bandwidth(&input(256 << 20, true), 64.0 * 1024.0);
        assert_eq!(advice.segments, 8, "slow link keeps the static breadth");
        let fast = advise_with_bandwidth(&input(256 << 20, true), 20.0 * 1024.0 * 1024.0);
        assert_eq!(fast.segments, 8, "capped at 8");
        let mid = advise_with_bandwidth(&input(64 << 20, true), 0.0);
        assert_eq!(mid.segments, 6, "no measurement keeps the static value");
    }
}
