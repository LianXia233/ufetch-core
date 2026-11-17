// SPDX-License-Identifier: LGPL-3.0-or-later
// Copyright (c) 2026 UFetch-Core contributors

//! 进度快照与任务状态。

use std::fmt;

use serde::{Deserialize, Serialize};

/// 任务状态机。
///
/// ```text
/// Queued → Probing → Running ⇄ Paused → Verifying → Completed
///                  ↘ Failed        ↘ Cancelled
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    /// 等待并发槽位。
    Queued,
    /// 正在探测文件大小 / Range 能力 / 文件名。
    Probing,
    /// 正在传输。
    Running,
    /// 已暂停（分块边界生效，续传记录已落盘）。
    Paused,
    /// 传输完成，正在校验。
    Verifying,
    /// 完成。
    Completed,
    /// 失败（临时文件与续传记录按配置保留）。
    Failed,
    /// 已取消（临时文件已清理）。
    Cancelled,
}

impl TaskState {
    /// 是否终态。
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Probing => "probing",
            Self::Running => "running",
            Self::Paused => "paused",
            Self::Verifying => "verifying",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

impl fmt::Display for TaskState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 一次进度快照。由引擎每 300–500 ms 发布一次，用于 UI 或 CLI 渲染。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Progress {
    pub task_id: String,
    pub state: TaskState,
    /// 已落盘字节（续传时包含上次运行的进度）。
    pub downloaded: u64,
    /// 文件总字节；未知时为 0。
    pub total: u64,
    /// 平滑后的下载速度（B/s）。
    pub speed_bps: u64,
    /// 当前目标并发连接数（自适应上调 / 下调后的值）。
    pub connections: u32,
    /// 当前正在传输的连接数。
    pub active_connections: u32,
    /// 分段总数。
    pub segments_total: u32,
    /// 已完成分段数。
    pub segments_done: u32,
    /// 百分比（0–100）；总大小未知时为 0。
    pub percent: f64,
    /// 已推断出的文件名。
    pub file_name: Option<String>,
    /// 已运行秒数（含暂停前的时间）。
    pub elapsed_secs: u64,
}

impl Default for Progress {
    fn default() -> Self {
        Self {
            task_id: String::new(),
            state: TaskState::Queued,
            downloaded: 0,
            total: 0,
            speed_bps: 0,
            connections: 0,
            active_connections: 0,
            segments_total: 0,
            segments_done: 0,
            percent: 0.0,
            file_name: None,
            elapsed_secs: 0,
        }
    }
}

impl Progress {
    /// 剩余字节（总大小未知时为 `None`）。
    #[must_use]
    pub fn remaining(&self) -> Option<u64> {
        (self.total > 0).then(|| self.total.saturating_sub(self.downloaded))
    }

    /// 预计剩余秒数（速度未知时为 `None`）。
    #[must_use]
    pub fn eta_secs(&self) -> Option<u64> {
        let remaining = self.remaining()?;
        (self.speed_bps > 0).then(|| remaining / self.speed_bps)
    }

    /// 人类可读速度，如 `12.3 MB/s`。
    #[must_use]
    pub fn human_speed(&self) -> String {
        human_bytes(self.speed_bps) + "/s"
    }
}

/// 字节数转人类可读格式。
#[must_use]
pub fn human_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let units = ["KB", "MB", "GB", "TB", "PB"];
    let mut scaled = bytes as f64 / 1024.0;
    let mut index = 0usize;
    while scaled >= 1024.0 && index + 1 < units.len() {
        scaled /= 1024.0;
        index += 1;
    }
    format!("{scaled:.1} {}", units[index])
}
