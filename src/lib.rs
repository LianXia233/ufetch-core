// SPDX-License-Identifier: LGPL-3.0-or-later
// Copyright (c) 2026 UFetch-Core contributors

//! `ufetch-core` —— 纯 HTTP 下载内核，面向 OpenWrt 插件自更新。
//!
//! 行为契约见 `docs/SPEC.md`。
//!
//! # 保留的能力
//!
//! - HTTP/HTTPS 单流与多段下载（`Range`），动态分块 + 空闲 worker 抢占慢段后半截；
//! - 按需切分：连接数从 4 起步，按实测带宽逐秒爬升，出错退两档；
//! - 连接复用：`http1_only` + 连接池，分段之间复用同一条 TCP/TLS 连接；
//! - 断点续传（`.parts.json` 侧车，逐段位置，跨进程重启有效）；
//! - 镜像线路回退（`ghfast.top` / `ghproxy.net` … 前缀或 `{url}` 模板）；
//! - 停滞看门狗（默认 30 s）、断连重试（指数退避）、403/429 降并发；
//! - 任务级慢速保护（默认 32 KB/s × 15 s，起步宽限 10 s）；
//! - 全局限速（令牌桶）、SHA-256 / SHA-1 / MD5 / SHA-512 校验；
//! - `Content-Disposition` / URL / MIME 文件名推断与清洗；
//! - 进度 JSON 状态文件 + 控制文件（`pause` / `resume` / `cancel`），供 LuCI / rpcd 使用；
//! - 单流自动降级（服务器不支持 Range、206 区间错位）。
//!
//! # 已移除
//!
//! BitTorrent、eD2K、FTP、HLS/DASH、RSS 订阅、插件系统、组件托管（ffmpeg/yt-dlp）、
//! 浏览器下载拦截、CDN 节点池、多网卡聚合、PAC/自动代理、任务数据库（SQLite/PostgreSQL）、
//! GPUI/Web 界面。
//!
//! # 快速开始
//!
//! ```no_run
//! use ufetch_core::{DownloadManager, DownloadRequest};
//!
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! let manager = DownloadManager::new();
//! let task = manager.download(
//!     DownloadRequest::new(
//!         "https://example.com/releases/download/v2.6.13/x.apk",
//!         "/tmp",
//!     )
//!     .mirror("https://ghfast.top/")
//!     .checksum("sha256=…")
//!     .status_file("/tmp/online-upgrade-status.json"),
//! );
//! let outcome = task.wait().await?;
//! println!("{}", outcome.path.display());
//! # Ok(()) }
//! ```

pub mod engine;
pub mod error;
pub mod file_hash;
pub mod logger;
pub mod manager;
pub mod naming;
pub mod net_failure;
pub(crate) mod output;
pub mod progress;
pub mod resume;
pub mod segment_advisor;
pub mod speed_limiter;
pub mod speed_meter;
pub mod task;
pub(crate) mod temp_file_guard;

pub use engine::{DownloadOutcome, EngineConfig, SlowGuard};
pub use error::DownloadError;
pub use manager::{DownloadManager, ManagerConfig, StatusSnapshot, build_client, write_control};
pub use progress::{Progress, TaskState, human_bytes};
pub use resume::{PartSegment, PartsFile};
pub use segment_advisor::{AdvisorInput, SegmentAdvice};
pub use task::{DownloadRequest, DownloadTask, RunState, TaskControl, apply_mirror};
