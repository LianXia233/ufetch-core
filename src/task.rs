// SPDX-License-Identifier: LGPL-3.0-or-later
// Copyright (c) 2026 UFetch-Core contributors

//! 任务控制与对外任务句柄。

use std::collections::HashMap;
use std::path::PathBuf;

use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::engine::DownloadOutcome;
use crate::error::DownloadError;
use crate::progress::Progress;

/// 运行状态（控制通道）。暂停在**分块边界**生效。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunState {
    Running,
    Paused,
    /// 终止（取消 / 慢速保护 / 致命错误）。
    Stopped,
}

/// 任务的控制器。克隆共享同一通道，可从任意任务 / 线程下发指令。
#[derive(Clone, Debug)]
pub struct TaskControl {
    tx: watch::Sender<RunState>,
}

impl TaskControl {
    pub(crate) fn new() -> (Self, watch::Receiver<RunState>) {
        let (tx, rx) = watch::channel(RunState::Running);
        (Self { tx }, rx)
    }

    /// 暂停。分块边界生效，已写入字节会记入续传记录。
    pub fn pause(&self) {
        if *self.tx.borrow() == RunState::Running {
            let _ = self.tx.send(RunState::Paused);
        }
    }

    /// 继续。
    pub fn resume(&self) {
        if *self.tx.borrow() == RunState::Paused {
            let _ = self.tx.send(RunState::Running);
        }
    }

    /// 取消。临时文件与续传记录会被清理。
    pub fn cancel(&self) {
        let _ = self.tx.send(RunState::Stopped);
    }

    /// 当前状态。
    #[must_use]
    pub fn run_state(&self) -> RunState {
        *self.tx.borrow()
    }

    pub(crate) fn subscribe(&self) -> watch::Receiver<RunState> {
        self.tx.subscribe()
    }
}

/// 一次下载请求。
#[derive(Clone, Debug)]
pub struct DownloadRequest {
    /// 原始（权威）URL，例如 GitHub Release 资产直链。
    pub url: String,
    /// 保存目录。
    pub save_dir: PathBuf,
    /// 指定文件名（含扩展名）。`None` = 由响应头 / URL 推断。
    pub file_name: Option<String>,
    /// 期望校验：`sha256=hex` / `md5=hex` 等。
    pub checksum: Option<String>,
    /// 并发连接数覆盖；`None` 用引擎默认（`0` = 自适应）。
    pub connections: Option<u32>,
    /// 任务级限速（B/s）；`None` 用全局限速。
    pub speed_limit_bps: Option<u64>,
    /// 镜像前缀。每个前缀与 `url` 拼成一条下载线路，按顺序尝试，线路级失败
    /// 才切换到下一条（用户取消 / 慢速保护不切换）。典型值：
    /// `https://ghfast.top/`、`https://ghproxy.net/`。
    pub mirrors: Vec<String>,
    /// 额外请求头（如 GitHub API 的 `Accept: application/octet-stream`）。
    pub headers: HashMap<String, String>,
    /// 目标文件已存在时是否覆盖。`false` = 自动编号（`name (1).ext`）。
    pub overwrite: bool,
    /// 失败 / 暂停时保留临时文件与续传记录（默认 `true`）。
    pub keep_partial: bool,
    /// 进度状态 JSON 路径（供 LuCI / rpcd 轮询）。`None` = 不写。
    pub status_file: Option<PathBuf>,
    /// 控制文件路径：轮询 `{"cmd":"pause|resume|cancel"}`。`None` = 只接受
    /// 进程内 [`TaskControl`] 指令。
    pub control_file: Option<PathBuf>,
    /// 强制分段：即使是小文件（插件 `.apk`）也按连接数切片，便于展示与
    /// 多线路提速。默认 `false`（小文件走单流更快）。
    pub force_split: bool,
}

impl DownloadRequest {
    /// 以 URL 与保存目录创建请求，其余字段取默认值。
    #[must_use]
    pub fn new(url: impl Into<String>, save_dir: impl Into<PathBuf>) -> Self {
        Self {
            url: url.into(),
            save_dir: save_dir.into(),
            file_name: None,
            checksum: None,
            connections: None,
            speed_limit_bps: None,
            mirrors: Vec::new(),
            headers: HashMap::new(),
            overwrite: false,
            keep_partial: true,
            status_file: None,
            control_file: None,
            force_split: false,
        }
    }

    /// 设置文件名。
    #[must_use]
    pub fn file_name(mut self, name: impl Into<String>) -> Self {
        self.file_name = Some(name.into());
        self
    }

    /// 设置期望校验值（`sha256=hex`）。
    #[must_use]
    pub fn checksum(mut self, checksum: impl Into<String>) -> Self {
        self.checksum = Some(checksum.into());
        self
    }

    /// 设置并发连接数。
    #[must_use]
    pub fn connections(mut self, connections: u32) -> Self {
        self.connections = Some(connections);
        self
    }

    /// 追加一条镜像线路前缀（按调用顺序尝试）。
    #[must_use]
    pub fn mirror(mut self, prefix: impl Into<String>) -> Self {
        self.mirrors.push(prefix.into());
        self
    }

    /// 批量追加镜像线路前缀。
    #[must_use]
    pub fn mirrors<I, S>(mut self, prefixes: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.mirrors.extend(prefixes.into_iter().map(Into::into));
        self
    }

    /// 追加请求头。
    #[must_use]
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.insert(name.into(), value.into());
        self
    }

    /// 覆盖已存在的目标文件。
    #[must_use]
    pub fn overwrite(mut self, overwrite: bool) -> Self {
        self.overwrite = overwrite;
        self
    }

    /// 强制分段（小文件也切片）。
    #[must_use]
    pub fn force_split(mut self, force: bool) -> Self {
        self.force_split = force;
        self
    }

    /// 写进度状态 JSON 文件（LuCI 轮询用）。
    #[must_use]
    pub fn status_file(mut self, path: impl Into<PathBuf>) -> Self {
        self.status_file = Some(path.into());
        self
    }

    /// 轮询控制文件（`pause|resume|cancel`）。
    #[must_use]
    pub fn control_file(mut self, path: impl Into<PathBuf>) -> Self {
        self.control_file = Some(path.into());
        self
    }

    /// 失败 / 暂停时保留半成品。
    #[must_use]
    pub fn keep_partial(mut self, keep: bool) -> Self {
        self.keep_partial = keep;
        self
    }

    /// 本次请求的全部下载线路（原始 URL 优先）。
    #[must_use]
    pub fn routes(&self) -> Vec<String> {
        let mut routes = Vec::with_capacity(self.mirrors.len() + 1);
        routes.push(self.url.clone());
        for mirror in &self.mirrors {
            routes.push(apply_mirror(mirror, &self.url));
        }
        routes
    }
}

/// 把镜像前缀与原始 URL 拼成一条线路。
///
/// 支持两种写法：
/// - 前缀：`https://ghfast.top/` + `https://github.com/a/b` →
///   `https://ghfast.top/https://github.com/a/b`；
/// - 模板：`https://x/{url}` → 直接替换。
#[must_use]
pub fn apply_mirror(mirror: &str, url: &str) -> String {
    let trimmed = mirror.trim();
    if let Some(template_pos) = trimmed.find("{url}") {
        let mut out = String::with_capacity(trimmed.len() + url.len());
        out.push_str(&trimmed[..template_pos]);
        out.push_str(url);
        out.push_str(&trimmed[template_pos + 5..]);
        out
    } else if trimmed.ends_with('/') {
        format!("{trimmed}{url}")
    } else {
        format!("{trimmed}/{url}")
    }
}

/// 对外任务句柄：控制、进度订阅与 `wait()`。
pub struct DownloadTask {
    pub(crate) id: String,
    pub(crate) control: TaskControl,
    pub(crate) progress: watch::Receiver<Progress>,
    pub(crate) handle: JoinHandle<Result<DownloadOutcome, DownloadError>>,
}

impl DownloadTask {
    /// 任务 ID。
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// 控制器（暂停 / 继续 / 取消）。
    #[must_use]
    pub fn control(&self) -> &TaskControl {
        &self.control
    }

    /// 最近一次进度快照。
    #[must_use]
    pub fn progress(&self) -> Progress {
        self.progress.borrow().clone()
    }

    /// 订阅进度更新。
    #[must_use]
    pub fn subscribe(&self) -> watch::Receiver<Progress> {
        self.progress.clone()
    }

    /// 等待任务结束。取消返回 [`DownloadError::Cancelled`]。
    ///
    /// # Errors
    ///
    /// 下载失败、校验失败、被取消或任务 panic。
    pub async fn wait(self) -> Result<DownloadOutcome, DownloadError> {
        match self.handle.await {
            Ok(result) => result,
            Err(error) => Err(DownloadError::Other(format!(
                "download task panicked: {error}"
            ))),
        }
    }
}
