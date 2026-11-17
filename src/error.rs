// SPDX-License-Identifier: LGPL-3.0-or-later
// Copyright (c) 2026 UFetch-Core contributors

//! 下载内核的错误类型。
//!
//! 只保留 HTTP 路径真正会产生、且调用方需要据此决策的变体：
//!
//! - [`DownloadError::RangeNotSupported`] → 回退单流；
//! - [`DownloadError::RangeMisaligned`] → 劣质 CDN 对任意 Range 都回「从 0 开始
//!   的全量流」，重试无意义，回退单流；
//! - [`DownloadError::VersionChanged`] → 服务器换文件，必须清空临时文件重下；
//! - [`DownloadError::TrueSizeLarger`] → 服务器自报的总大小比规划的大，就地扩容；
//! - [`DownloadError::ServerRejected`] → 403/429，连接数被限，降并发而不是重试；
//! - [`DownloadError::SlowSpeed`] / [`DownloadError::Stall`] → 引擎自身判定，
//!   不再无脑重试。
//!
//! 被删除的变体（`Db` / `Ed2k` / `CdnNodeFailed` / `NotAnHlsPlaylist` /
//! `PluginAbort` 等）对应已裁掉的 DB、eD2K、CDN 节点池、HLS、插件拦截能力。

use std::error::Error as StdError;

use thiserror::Error;

/// 下载失败类型。
#[derive(Error, Debug)]
pub enum DownloadError {
    #[error("request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("cancelled")]
    Cancelled,
    #[error("checksum mismatch: {0}")]
    ChecksumMismatch(String),
    /// 服务器不支持 Range（返回 200 而非 206）。
    #[error("server does not support Range requests (returned {0} instead of 206 Partial Content)")]
    RangeNotSupported(String),
    /// 206 但 `Content-Range` 起点与请求偏移不一致（劣质 CDN 回全量流）。
    #[error(
        "server returned a misaligned Range response (206 but Content-Range does not match: {0})"
    )]
    RangeMisaligned(String),
    /// 下载途中服务器换了文件（validator 不一致），旧数据不能再拼接。
    #[error("file changed on server during download (validator mismatch: {0})")]
    VersionChanged(String),
    /// 服务器自报的真实总大小大于规划总量（渐进上传 / hint 偏小）。
    #[error("server reports a larger true size than planned (Content-Range total: {0})")]
    TrueSizeLarger(i64),
    /// 服务器主动拒绝（403 / 429），多连接被限。
    #[error("server rejected the request with HTTP {status}")]
    ServerRejected { status: u16 },
    /// 任务级慢速保护触发。
    #[error("download speed too low: {avg_bps} B/s over {window_secs}s (threshold {min_bps} B/s)")]
    SlowSpeed {
        avg_bps: u64,
        min_bps: u64,
        window_secs: u64,
    },
    /// 单条连接停滞（看门狗判定）。
    #[error("download stalled: segment {segment} idle for {idle_secs}s")]
    Stall { segment: usize, idle_secs: u64 },
    #[error("invalid url: {0}")]
    InvalidUrl(String),
    #[error("unsupported response: {0}")]
    Unsupported(String),
    #[error("{0}")]
    Other(String),
}

impl DownloadError {
    /// 服务端主动拒绝（403 / 429）：应立即降并发，而不是重试。
    #[must_use]
    pub fn is_server_rejection(&self) -> bool {
        match self {
            Self::ServerRejected { .. } => true,
            Self::Request(e) => e.status().is_some_and(|s| matches!(s.as_u16(), 403 | 429)),
            _ => false,
        }
    }

    /// 416：同一 Range 重试必然再次 416。
    #[must_use]
    pub fn is_range_not_satisfiable(&self) -> bool {
        match self {
            Self::Request(e) => e.status() == Some(reqwest::StatusCode::RANGE_NOT_SATISFIABLE),
            _ => false,
        }
    }

    /// 本地磁盘写满 / 配额耗尽：重试只会空烧预算。
    #[must_use]
    pub fn is_disk_full(&self) -> bool {
        matches!(
            self,
            Self::Io(io)
                if matches!(
                    io.kind(),
                    std::io::ErrorKind::StorageFull | std::io::ErrorKind::QuotaExceeded
                )
        )
    }

    /// 该错误是否可以原样重试（同一条连接 / 同一条路径）。
    #[must_use]
    pub fn retriable(&self) -> bool {
        !matches!(
            self,
            Self::Cancelled
                | Self::ChecksumMismatch(_)
                | Self::RangeMisaligned(_)
                | Self::VersionChanged(_)
                | Self::SlowSpeed { .. }
                | Self::InvalidUrl(_)
                | Self::Unsupported(_)
        ) && !self.is_server_rejection()
            && !self.is_range_not_satisfiable()
            && !self.is_disk_full()
    }

    /// 传输层失败分类（DNS / 超时 / TLS / 断流……）。
    #[must_use]
    pub fn net_failure(&self) -> Option<crate::net_failure::NetFailure> {
        crate::net_failure::classify_download_error(self)
    }

    /// 面向用户的简短说明，错误文案末尾带 `[net:<code>]` 标签（便于调用方
    /// 只解析标签而不依赖本地化文本）。
    #[must_use]
    pub fn summary(&self) -> String {
        crate::net_failure::append_tag(self.to_string(), self.net_failure())
    }
}

impl From<&str> for DownloadError {
    fn from(value: &str) -> Self {
        Self::Other(value.to_string())
    }
}

impl From<String> for DownloadError {
    fn from(value: String) -> Self {
        Self::Other(value)
    }
}

/// 沿 `source()` 链取根因（日志用）。
#[must_use]
pub fn root_cause(error: &(dyn StdError + 'static)) -> String {
    let mut cur: &(dyn StdError + 'static) = error;
    while let Some(next) = cur.source() {
        cur = next;
    }
    cur.to_string()
}
