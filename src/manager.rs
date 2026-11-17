// SPDX-License-Identifier: LGPL-3.0-or-later
// Copyright (c) 2026 UFetch-Core contributors

//! 下载管理器：客户端装配、并发槽位、任务生命周期、状态/控制文件。

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio::sync::{Semaphore, watch};

use crate::engine::{self, DownloadOutcome, EngineConfig, RunResult, SharedProgress};
use crate::error::DownloadError;
use crate::fsutil::default_home;
use crate::progress::{Progress, TaskState};
use crate::registry::{TaskRecord, TaskRegistry, now_secs};
use crate::speed_limiter::SpeedLimiter;
use crate::task::{DownloadRequest, DownloadTask, RunState, TaskControl};
use crate::{log_error, log_info, log_warn};

/// 管理器配置。
#[derive(Clone, Debug)]
pub struct ManagerConfig {
    /// 引擎默认值（连接数、分块、看门狗、慢速保护……）。
    pub engine: EngineConfig,
    /// 同时进行的任务数上限。
    pub max_concurrent: usize,
    /// 进度发布间隔。
    pub progress_interval: Duration,
    /// 任务表与默认状态文件目录。
    pub task_dir: PathBuf,
    /// 安装成功后是否自动移除任务（仅记录，删除动作由调用方触发）。
    pub auto_remove: bool,
}

impl Default for ManagerConfig {
    fn default() -> Self {
        Self {
            engine: EngineConfig::openwrt(),
            max_concurrent: 3,
            progress_interval: Duration::from_millis(500),
            task_dir: default_home(),
            auto_remove: true,
        }
    }
}

/// 下载管理器。持有共享 HTTP 客户端与全局限速器。
#[derive(Clone)]
pub struct DownloadManager {
    client: reqwest::Client,
    limiter: SpeedLimiter,
    config: Arc<ManagerConfig>,
    slots: Arc<Semaphore>,
    counter: Arc<AtomicU64>,
    registry: TaskRegistry,
}

impl DownloadManager {
    /// 以默认配置创建。
    ///
    /// # Panics
    ///
    /// 仅在 TLS 后端初始化失败（几乎不可能）时 panic；需要返回值请用
    /// [`DownloadManager::try_new`]。
    #[must_use]
    pub fn new() -> Self {
        Self::try_new(ManagerConfig::default())
            .unwrap_or_else(|error| panic!("cannot build HTTP client: {error}"))
    }

    /// 以指定配置创建。
    ///
    /// # Errors
    ///
    /// 代理地址非法或 HTTP 客户端构建失败。
    pub fn try_new(config: ManagerConfig) -> Result<Self, DownloadError> {
        crate::logger::init_from_env();
        let client = build_client(&config.engine)?;
        let limiter = SpeedLimiter::new(config.engine.speed_limit_bps);
        limiter.spawn_refill_task();
        Ok(Self {
            client,
            limiter,
            slots: Arc::new(Semaphore::new(config.max_concurrent.max(1))),
            registry: TaskRegistry::new(&config.task_dir),
            config: Arc::new(config),
            counter: Arc::new(AtomicU64::new(0)),
        })
    }

    /// 当前全局限速（B/s，0 = 不限）。
    #[must_use]
    pub fn speed_limit(&self) -> u64 {
        self.limiter.limit()
    }

    /// 运行中调整全局限速。
    pub fn set_speed_limit(&self, bps: u64) {
        self.limiter.set_limit(bps);
    }

    /// 提交一个下载任务，立即返回句柄。
    #[must_use]
    pub fn download(&self, mut request: DownloadRequest) -> DownloadTask {
        let id = format!(
            "{}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_millis())
                .unwrap_or(0),
            self.counter.fetch_add(1, Ordering::Relaxed)
        );
        if request.status_file.is_none() {
            request.status_file = Some(self.config.task_dir.join(format!("{id}.status.json")));
        }
        if request.control_file.is_none() {
            request.control_file = Some(self.config.task_dir.join(format!("{id}.ctl.json")));
        }
        let record = record_for(
            &id,
            &request.url,
            "queued",
            None,
            request.status_file.clone(),
            request.control_file.clone(),
            self.config.auto_remove,
        );
        if let Err(error) = self.registry.upsert(&record) {
            log_warn!("[tasks] initial write failed: {error}");
        }
        let shared = SharedProgress::new(&id);
        let (control, _) = TaskControl::new();
        let (progress_tx, progress_rx) = watch::channel(Progress {
            task_id: id.clone(),
            state: TaskState::Queued,
            ..Progress::default()
        });

        // 进度发布。
        {
            let shared = Arc::clone(&shared);
            let tx = progress_tx.clone();
            let registry = self.registry.clone();
            let task_id = id.clone();
            let url = request.url.clone();
            let status_file = request.status_file.clone();
            let control_file = request.control_file.clone();
            let auto_remove = self.config.auto_remove;
            let interval = self.config.progress_interval;
            tokio::spawn(async move {
                let mut ticker = tokio::time::interval(interval);
                ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    ticker.tick().await;
                    let snapshot = shared.snapshot(&task_id);
                    let terminal = snapshot.state.is_terminal();
                    if !terminal {
                        let _ = registry.upsert(&record_for_progress(
                            &task_id,
                            &url,
                            &snapshot,
                            None,
                            status_file.clone(),
                            control_file.clone(),
                            auto_remove,
                        ));
                    }
                    if tx.send(snapshot).is_err() {
                        return;
                    }
                    if terminal {
                        return;
                    }
                }
            });
        }

        // 状态文件（LuCI / rpcd 轮询）。
        if let Some(path) = request.status_file.clone() {
            let mut rx = progress_rx.clone();
            let task_id = id.clone();
            tokio::spawn(async move {
                while rx.changed().await.is_ok() {
                    let snapshot = StatusSnapshot::from(rx.borrow().clone());
                    if let Err(error) = write_status(&path, &snapshot) {
                        log_warn!(
                            "[status] task {} failed to write {}: {}",
                            task_id,
                            path.display(),
                            error
                        );
                        return;
                    }
                    if snapshot.progress.state.is_terminal() {
                        return;
                    }
                }
            });
        }

        // 控制文件（LuCI 在进程外下发 pause / resume / cancel）。
        if let Some(path) = request.control_file.clone() {
            let control = control.clone();
            tokio::spawn(async move { watch_control_file(path, control).await });
        }

        let client = self.client.clone();
        let limiter = self.limiter.clone();
        let config = Arc::clone(&self.config);
        let slots = Arc::clone(&self.slots);
        let task_id = id.clone();
        let registry = self.registry.clone();
        let control_for_task = control.clone();
        let shared_for_task = Arc::clone(&shared);
        let progress_for_task = progress_tx.clone();

        let final_status = request.status_file.clone();
        let final_url = request.url.clone();
        let final_status_path = request.status_file.clone();
        let final_control_path = request.control_file.clone();
        let handle = tokio::spawn(async move {
            let permit = match slots.acquire_owned().await {
                Ok(permit) => permit,
                Err(_) => return Err(DownloadError::Cancelled),
            };
            log_info!("[task {task_id}] started: {}", request.url);
            let result = run_task(
                &task_id,
                request,
                &config.engine,
                &client,
                &limiter,
                &control_for_task,
                &shared_for_task,
                &progress_for_task,
            )
            .await;
            drop(permit);
            if let Some(path) = final_status.as_ref() {
                let snapshot = StatusSnapshot {
                    progress: shared_for_task.snapshot(&task_id),
                    error: result.as_ref().err().map(|error| error.summary()),
                    path: result
                        .as_ref()
                        .ok()
                        .map(|outcome| outcome.path.display().to_string()),
                };
                if let Err(error) = write_status(path, &snapshot) {
                    log_warn!("[status] task {task_id} final write failed: {error}");
                }
            }
            let final_record = match &result {
                Ok(outcome) => record_for(
                    &task_id,
                    &final_url,
                    "completed",
                    Some(&shared_for_task.snapshot(&task_id)),
                    final_status_path.clone(),
                    final_control_path.clone(),
                    config.auto_remove,
                )
                .with_artifact(outcome.path.clone()),
                Err(DownloadError::Cancelled) => record_for(
                    &task_id,
                    &final_url,
                    "cancelled",
                    Some(&shared_for_task.snapshot(&task_id)),
                    final_status_path.clone(),
                    final_control_path.clone(),
                    config.auto_remove,
                ),
                Err(error) => {
                    let mut record = record_for(
                        &task_id,
                        &final_url,
                        "failed",
                        Some(&shared_for_task.snapshot(&task_id)),
                        final_status_path.clone(),
                        final_control_path.clone(),
                        config.auto_remove,
                    );
                    record.error = Some(error.summary());
                    record
                }
            };
            if let Err(error) = registry.upsert(&final_record) {
                log_warn!("[tasks] final write failed: {error}");
            }
            match &result {
                Ok(outcome) => log_info!(
                    "[task {task_id}] done: {} ({} bytes in {:?})",
                    outcome.path.display(),
                    outcome.total_bytes,
                    outcome.elapsed
                ),
                Err(DownloadError::Cancelled) => log_info!("[task {task_id}] cancelled"),
                Err(error) => log_error!("[task {task_id}] failed: {}", error.summary()),
            }
            result
        });

        DownloadTask {
            id,
            control,
            progress: progress_rx,
            handle,
        }
    }

    /// 提交并等待完成。
    ///
    /// # Errors
    ///
    /// 下载 / 校验失败或被取消。
    pub async fn download_and_wait(
        &self,
        request: DownloadRequest,
    ) -> Result<DownloadOutcome, DownloadError> {
        self.download(request).wait().await
    }
}

impl Default for DownloadManager {
    fn default() -> Self {
        Self::new()
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_task(
    task_id: &str,
    request: DownloadRequest,
    config: &EngineConfig,
    client: &reqwest::Client,
    limiter: &SpeedLimiter,
    control: &TaskControl,
    shared: &Arc<SharedProgress>,
    progress: &watch::Sender<Progress>,
) -> Result<DownloadOutcome, DownloadError> {
    let mut resume_wait = control.subscribe();
    loop {
        let outcome =
            engine::run(task_id, &request, config, client, limiter, control, shared).await;
        // 每次运行结束后立刻发布一次终态 / 暂停态，避免 UI 等待下一个 tick。
        let _ = progress.send(shared.snapshot(task_id));
        match outcome {
            Ok(RunResult::Completed(outcome)) => return Ok(outcome),
            Ok(RunResult::Cancelled) => {
                shared.set_state(TaskState::Cancelled);
                let _ = progress.send(shared.snapshot(task_id));
                return Err(DownloadError::Cancelled);
            }
            Ok(RunResult::Paused) => {
                shared.set_state(TaskState::Paused);
                let _ = progress.send(shared.snapshot(task_id));
                // 等待 resume 或 cancel。
                loop {
                    match *resume_wait.borrow_and_update() {
                        RunState::Running => break,
                        RunState::Stopped => {
                            shared.set_state(TaskState::Cancelled);
                            let _ = progress.send(shared.snapshot(task_id));
                            return Err(DownloadError::Cancelled);
                        }
                        RunState::Paused => {}
                    }
                    if resume_wait.changed().await.is_err() {
                        return Err(DownloadError::Cancelled);
                    }
                }
                shared.set_state(TaskState::Running);
            }
            Err(error) => {
                if matches!(error, DownloadError::Cancelled) {
                    shared.set_state(TaskState::Cancelled);
                } else {
                    shared.set_state(TaskState::Failed);
                }
                let _ = progress.send(shared.snapshot(task_id));
                return Err(error);
            }
        }
    }
}

fn record_for(
    id: &str,
    url: &str,
    state: &str,
    progress: Option<&Progress>,
    status_file: Option<PathBuf>,
    control_file: Option<PathBuf>,
    auto_remove: bool,
) -> TaskRecord {
    let now = now_secs();
    let progress = progress.cloned().unwrap_or_default();
    TaskRecord {
        id: id.to_string(),
        url: url.to_string(),
        file_name: progress.file_name.clone(),
        state: state.to_string(),
        downloaded: progress.downloaded,
        total: progress.total,
        percent: progress.percent,
        speed_bps: progress.speed_bps,
        connections: progress.connections,
        active_connections: progress.active_connections,
        created_at: now,
        updated_at: now,
        status_file,
        control_file,
        artifact: None,
        error: None,
        auto_remove,
    }
}

fn record_for_progress(
    id: &str,
    url: &str,
    progress: &Progress,
    artifact: Option<PathBuf>,
    status_file: Option<PathBuf>,
    control_file: Option<PathBuf>,
    auto_remove: bool,
) -> TaskRecord {
    let mut record = record_for(
        id,
        url,
        &progress.state.to_string(),
        Some(progress),
        status_file,
        control_file,
        auto_remove,
    );
    record.artifact = artifact;
    record
}

/// 装配 HTTP 客户端。
///
/// 关键点：`http1_only()`。多段下载必须让每条区间占用**独立的 TCP 连接**；
/// 若协商 HTTP/2，多条流会被复用到同一连接上，并发加速失效。
///
/// # Errors
///
/// 代理地址非法或构建失败。
pub fn build_client(config: &EngineConfig) -> Result<reqwest::Client, DownloadError> {
    let mut builder = reqwest::Client::builder()
        .user_agent(&config.user_agent)
        .connect_timeout(config.connect_timeout)
        .http1_only()
        .tcp_keepalive(Duration::from_secs(30))
        .pool_max_idle_per_host(config.max_connections.clamp(1, 16) as usize)
        .pool_idle_timeout(Duration::from_secs(90))
        .redirect(reqwest::redirect::Policy::limited(10));
    if let Some(proxy) = &config.proxy {
        builder = builder.proxy(reqwest::Proxy::all(proxy).map_err(DownloadError::Request)?);
    }
    if config.allow_invalid_certs {
        builder = builder.danger_accept_invalid_certs(true);
    }
    builder.build().map_err(DownloadError::Request)
}

/// 状态文件内容（`Progress` + 终态错误 / 落盘路径）。
#[derive(Clone, Debug, Serialize)]
pub struct StatusSnapshot {
    #[serde(flatten)]
    pub progress: Progress,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

impl From<Progress> for StatusSnapshot {
    fn from(progress: Progress) -> Self {
        Self {
            progress,
            error: None,
            path: None,
        }
    }
}

fn write_status(path: &PathBuf, status: &StatusSnapshot) -> std::io::Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string(status)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, text)?;
    std::fs::rename(&temporary, path)
}

/// 轮询控制文件：只认 `cmd` / `action` / `command` 三种键，值取
/// `pause` / `resume` / `cancel`（`stop` 同 cancel）。命令重复写入不会重复生效。
async fn watch_control_file(path: PathBuf, control: TaskControl) {
    let mut last = String::new();
    let mut ticker = tokio::time::interval(Duration::from_millis(300));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;
        if control.run_state() == RunState::Stopped {
            return;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        let command = value
            .get("cmd")
            .or_else(|| value.get("action"))
            .or_else(|| value.get("command"))
            .and_then(|value| value.as_str())
            .map(|value| value.trim().to_ascii_lowercase())
            .unwrap_or_default();
        if command.is_empty() || command == last {
            continue;
        }
        last = command.clone();
        match command.as_str() {
            "pause" => {
                log_info!("[control] pause requested via {}", path.display());
                control.pause();
            }
            "resume" | "continue" => {
                log_info!("[control] resume requested via {}", path.display());
                control.resume();
            }
            "cancel" | "stop" | "abort" => {
                log_info!("[control] cancel requested via {}", path.display());
                control.cancel();
                return;
            }
            other => log_warn!("[control] unknown command '{other}' in {}", path.display()),
        }
    }
}

/// 写控制文件（供 CLI / LuCI 调用）。
///
/// # Errors
///
/// 目录创建或写入失败。
pub fn write_control(path: &std::path::Path, command: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    let payload = serde_json::json!({ "cmd": command, "ts": Instant::now().elapsed().as_millis() });
    std::fs::write(path, serde_json::to_string(&payload)?)
}
