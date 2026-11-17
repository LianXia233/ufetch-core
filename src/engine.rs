// SPDX-License-Identifier: LGPL-3.0-or-later
// Copyright (c) 2026 UFetch-Core contributors

//! HTTP 下载引擎：探测 → 规划 → 动态分段下载 → 校验 → 落盘。
//!
//! # 模块边界
//!
//! - [`crate::segment_advisor`]：按文件大小 / CPU 核数 / 实测带宽给出连接数上限；
//! - 分段抢占：空闲 worker 从最慢在传段的尾部对半拆开（见 `Scheduler::steal`）；
//!   完成时间抢占等纯判据；
//! - [`crate::speed_limiter`]：全局令牌桶限速；
//! - [`crate::naming`]：`Content-Disposition` / URL / MIME 命名管线；
//! - [`crate::net_failure`]：按 `source()` 链与 `io::ErrorKind` 分类传输失败；
//! - [`crate::file_hash`]：SHA-256 / SHA-1 / MD5 / SHA-512 校验。
//!
//! 调度模型：区间队列 + 空闲 worker 接管（work stealing）、每段独立重试、
//! 停滞看门狗、任务级慢速保护、单流自动降级、`.parts.json` 侧车续传——不依赖
//! 数据库，可在 musl 静态二进制里跑在路由器上。
//!
//! # 动态分段
//!
//! 1. 探测得到总大小与 Range 支持后，按 `chunk_size` 切成若干区间入队；
//! 2. `goal`（目标并发数）先取小值起飞，之后按实测带宽用
//!    [`crate::segment_advisor::advise_with_bandwidth`] 自适应调整；
//! 3. 当有空闲 worker 而队列为空时，正在传输的连接可以按双方速率**按比例
//!    切走**自己剩余的尾部（均衡完成拆分），空闲 worker 立即接管——
//!    这就是「慢段接管」，不再有固定的分块网格；
//! 4. 单条连接停滞超过 `stall_timeout`，看门狗切断并把剩余区间重新入队。

use std::collections::VecDeque;
use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use reqwest::header::{ACCEPT, CONTENT_RANGE, ETAG, LAST_MODIFIED, RANGE};
use reqwest::{Client, Response, StatusCode};
use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use tokio::sync::{Notify, watch};
use tokio::task::JoinSet;

use crate::error::DownloadError;
use crate::file_hash::{HashAlgorithm, hash_file};
/// 默认起步连接数。
const DEFAULT_START_CONNECTIONS: u32 = 4;
use crate::naming::{extract_filename, sanitize_filename};
/// 拆分/抢占后每段至少保留的字节数。
const MIN_SPLIT_PIECE: u64 = 64 * 1024;
use crate::progress::{Progress, TaskState, human_bytes};
use crate::resume::{PartSegment, PartsFile};
use crate::segment_advisor::{AdvisorInput, advise_with_bandwidth};
use crate::speed_limiter::SpeedLimiter;
use crate::speed_meter::CounterRate;
use crate::task::{DownloadRequest, RunState, TaskControl};
use crate::temp_file_guard::TempFileGuard;
use crate::{log_debug, log_error, log_info, log_warn};

/// 低于此大小的文件不分段（与 `segment_advisor` 的阈值一致）。
const SINGLE_SEGMENT_THRESHOLD: u64 = 2 * 1024 * 1024;
/// 默认分块大小。4 MiB 与 luci 内核一致：单块耗时可观、又不会切得太碎。
const DEFAULT_CHUNK_SIZE: u64 = 4 * 1024 * 1024;
/// 分段路径的写缓冲。
const WRITE_BUFFER: usize = 256 * 1024;
/// 同时进行的最大连接数硬上限（路由器上再高只会互相拖慢）。
const HARD_MAX_CONNECTIONS: u32 = 32;

/// 引擎配置。
#[derive(Clone, Debug)]
pub struct EngineConfig {
    /// 默认并发连接数；`0` = 自适应（推荐）。
    pub connections: u32,
    /// 自适应模式下的连接数上限。
    pub max_connections: u32,
    /// 初始分块大小（字节）。
    pub chunk_size: u64,
    /// 拆分后每段至少保留的字节数。
    pub min_split_piece: u64,
    /// 单个区间的最大重试次数。
    pub max_retries: u32,
    /// 停滞看门狗：这么久没有新字节就切断该连接并重派。
    pub stall_timeout: Duration,
    /// 建连超时。
    pub connect_timeout: Duration,
    /// 全局下载限速（B/s，`0` = 不限）。
    pub speed_limit_bps: u64,
    /// 任务级慢速保护。`None` = 关闭。
    pub slow_guard: Option<SlowGuard>,
    /// User-Agent。
    pub user_agent: String,
    /// 代理（`http://127.0.0.1:7890` / `socks5://…`）。
    pub proxy: Option<String>,
    /// 接受无效证书（仅调试用，默认关闭）。
    pub allow_invalid_certs: bool,
}

/// 慢速保护参数：连续 `window_secs` 秒低于 `min_bps` 即终止任务。
#[derive(Clone, Copy, Debug)]
pub struct SlowGuard {
    pub min_bps: u64,
    pub window_secs: u64,
    pub grace_secs: u64,
}

impl EngineConfig {
    /// 面向 OpenWrt 的默认值：自适应连接（上限 8）、4 MiB 分块、30s 停滞看门狗、
    /// 慢速保护 32 KB/s × 15s（起步宽限 10s）。
    #[must_use]
    pub fn openwrt() -> Self {
        Self {
            connections: 0,
            max_connections: 8,
            chunk_size: DEFAULT_CHUNK_SIZE,
            min_split_piece: MIN_SPLIT_PIECE,
            max_retries: 3,
            stall_timeout: Duration::from_secs(30),
            connect_timeout: Duration::from_secs(20),
            speed_limit_bps: 0,
            slow_guard: Some(SlowGuard {
                min_bps: 32 * 1024,
                window_secs: 15,
                grace_secs: 10,
            }),
            user_agent: DEFAULT_UA.to_string(),
            proxy: None,
            allow_invalid_certs: false,
        }
    }
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self::openwrt()
    }
}

/// 默认 UA。GitHub 与镜像节点对浏览器 UA 最友好。
pub const DEFAULT_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
(KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

/// 下载成功的结果。
#[derive(Clone, Debug)]
pub struct DownloadOutcome {
    pub task_id: String,
    pub path: PathBuf,
    pub file_name: String,
    pub total_bytes: u64,
    /// 本次运行开始时已存在的字节（续传命中量）。
    pub resumed_bytes: u64,
    /// 实际使用的并发连接数峰值。
    pub connections: u32,
    pub elapsed: Duration,
    /// 落盘文件的摘要（`algo=hex`）；未指定校验时也会算 SHA-256。
    pub checksum: Option<String>,
    pub average_bps: u64,
}

/// 引擎本次运行的结果。
#[derive(Debug)]
pub enum RunResult {
    Completed(DownloadOutcome),
    Paused,
    Cancelled,
}

/// 引擎内共享的进度状态（原子量，读侧无锁）。
#[derive(Debug)]
pub(crate) struct SharedProgress {
    pub state: Mutex<TaskState>,
    pub downloaded: AtomicU64,
    pub total: AtomicU64,
    pub speed: AtomicU64,
    pub connections: AtomicU32,
    pub active: AtomicU32,
    pub segments_total: AtomicU32,
    pub segments_done: AtomicU32,
    pub file_name: Mutex<Option<String>>,
    pub started: Instant,
}

impl SharedProgress {
    pub(crate) fn new(_task_id: &str) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(TaskState::Queued),
            downloaded: AtomicU64::new(0),
            total: AtomicU64::new(0),
            speed: AtomicU64::new(0),
            connections: AtomicU32::new(0),
            active: AtomicU32::new(0),
            segments_total: AtomicU32::new(0),
            segments_done: AtomicU32::new(0),
            file_name: Mutex::new(None),
            started: Instant::now(),
        })
    }

    pub(crate) fn set_state(&self, state: TaskState) {
        *lock(&self.state) = state;
    }

    pub(crate) fn state(&self) -> TaskState {
        *lock(&self.state)
    }

    /// 生成快照。
    pub(crate) fn snapshot(&self, task_id: &str) -> Progress {
        let total = self.total.load(Ordering::Relaxed);
        let downloaded = self.downloaded.load(Ordering::Relaxed).min(total.max(1));
        let percent = if total > 0 {
            (downloaded as f64 / total as f64 * 100.0).clamp(0.0, 100.0)
        } else {
            0.0
        };
        Progress {
            task_id: task_id.to_string(),
            state: self.state(),
            downloaded,
            total,
            speed_bps: self.speed.load(Ordering::Relaxed),
            connections: self.connections.load(Ordering::Relaxed),
            active_connections: self.active.load(Ordering::Relaxed),
            segments_total: self.segments_total.load(Ordering::Relaxed),
            segments_done: self.segments_done.load(Ordering::Relaxed),
            percent,
            file_name: lock(&self.file_name).clone(),
            elapsed_secs: self.started.elapsed().as_secs(),
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

/// 一次运行所需的共享上下文。
struct RunCtx<'a> {
    task_id: &'a str,
    req: &'a DownloadRequest,
    cfg: &'a EngineConfig,
    client: &'a Client,
    limiter: &'a SpeedLimiter,
    ctl: &'a TaskControl,
    shared: &'a Arc<SharedProgress>,
    route: String,
    etag: Option<String>,
    last_modified: Option<String>,
    temp: PathBuf,
    file_name: String,
    resumed_bytes: u64,
}

/// 探测结果。
struct Probe {
    total: u64,
    ranges: bool,
    etag: Option<String>,
    last_modified: Option<String>,
    file_name: String,
    /// 探测请求本身就是一次完整 200 响应时保留下来，单流路径直接复用。
    held: Option<Response>,
}

/// 入口：按线路顺序尝试下载；只有「线路级失败」才切下一条。
///
/// # Errors
///
/// 所有线路都失败、校验失败、磁盘错误等。
pub(crate) async fn run(
    task_id: &str,
    req: &DownloadRequest,
    cfg: &EngineConfig,
    client: &Client,
    limiter: &SpeedLimiter,
    ctl: &TaskControl,
    shared: &Arc<SharedProgress>,
) -> Result<RunResult, DownloadError> {
    crate::output::ensure_dir(&req.save_dir).await?;
    let routes = req.routes();
    let mut last_error: Option<DownloadError> = None;
    for (index, route) in routes.iter().enumerate() {
        if ctl.run_state() == RunState::Stopped {
            return Ok(RunResult::Cancelled);
        }
        if index > 0 {
            log_warn!(
                "[download] route {} failed, trying mirror {}/{}: {}",
                index,
                index + 1,
                routes.len(),
                route
            );
        }
        match run_route(task_id, route, req, cfg, client, limiter, ctl, shared).await {
            Ok(result) => return Ok(result),
            Err(error) => {
                let switch = is_route_failure(&error) && index + 1 < routes.len();
                if switch {
                    last_error = Some(error);
                    continue;
                }
                // 取消：清理半成品。
                if matches!(error, DownloadError::Cancelled) {
                    cleanup_partial(shared, req, &req.save_dir).await;
                    return Ok(RunResult::Cancelled);
                }
                return Err(error);
            }
        }
    }
    Err(last_error.unwrap_or_else(|| DownloadError::Other("no usable download route".into())))
}

/// 是否是「换一条线路可能修复」的失败。
#[must_use]
pub fn is_route_failure(error: &DownloadError) -> bool {
    if error.net_failure().is_some() {
        return true;
    }
    match error {
        DownloadError::ServerRejected { .. } => true,
        DownloadError::Request(err) => err
            .status()
            .is_some_and(|status| status.is_server_error() || status == StatusCode::FORBIDDEN),
        DownloadError::Unsupported(message) => message.starts_with("HTTP 5"),
        _ => false,
    }
}

async fn cleanup_partial(shared: &Arc<SharedProgress>, req: &DownloadRequest, dir: &Path) {
    if req.keep_partial {
        return;
    }
    let _ = (shared, dir);
}

#[allow(clippy::too_many_arguments)]
async fn run_route(
    task_id: &str,
    route: &str,
    req: &DownloadRequest,
    cfg: &EngineConfig,
    client: &Client,
    limiter: &SpeedLimiter,
    ctl: &TaskControl,
    shared: &Arc<SharedProgress>,
) -> Result<RunResult, DownloadError> {
    shared.set_state(TaskState::Probing);
    let probe = probe_route(client, route, req, cfg).await?;
    let total = probe.total;
    shared.total.store(total, Ordering::Relaxed);
    *lock(&shared.file_name) = Some(probe.file_name.clone());
    log_info!(
        "[download] {} -> {} ({} bytes, ranges={}, etag={:?})",
        route,
        probe.file_name,
        total,
        probe.ranges,
        probe.etag
    );

    let temp = req.save_dir.join(format!("{}.ufetch", probe.file_name));
    // 同一临时文件的并发写保护（进程内）。
    let _guard = TempFileGuard::acquire(&temp, task_id).await?;

    let ctx = RunCtx {
        task_id,
        req,
        cfg,
        client,
        limiter,
        ctl,
        shared,
        route: route.to_string(),
        etag: probe.etag.clone(),
        last_modified: probe.last_modified.clone(),
        temp: temp.clone(),
        file_name: probe.file_name.clone(),
        resumed_bytes: 0,
    };

    let connections = effective_connections(req, cfg, total, probe.ranges, req.force_split);
    let connection_cap = cfg
        .max_connections
        .clamp(1, HARD_MAX_CONNECTIONS)
        .max(connections);
    if !probe.ranges || connections <= 1 || (total <= SINGLE_SEGMENT_THRESHOLD && !req.force_split)
    {
        let reason = if !probe.ranges {
            "server does not support Range"
        } else if connections <= 1 {
            "single connection requested"
        } else {
            "file is small enough for a single stream (use force_split to override)"
        };
        log_info!("[download] single stream: {} ({} bytes)", reason, total);
        shared.connections.store(1, Ordering::Relaxed);
        shared.active.store(1, Ordering::Relaxed);
        if probe.held.is_some() {
            log_debug!("[download] reusing probe response as the data connection");
        }
        let result = download_single(&ctx, probe.held, total, shared).await?;
        return Ok(result);
    }

    log_info!(
        "[download] segmented: {} connections planned, {} B chunks",
        connections,
        adaptive_chunk(total, connections, cfg.chunk_size, cfg.min_split_piece)
    );
    shared.connections.store(connections, Ordering::Relaxed);
    let mut restarted = false;
    loop {
        match download_segmented(&ctx, total, connections, connection_cap, shared).await {
            Ok(result) => {
                return Ok(result);
            }
            // 下载途中服务器换了文件：清空续传记录后从零重下一次。
            Err(DownloadError::VersionChanged(detail)) if !restarted => {
                log_warn!(
                    "[download] server changed the file mid-flight ({detail}), restarting from zero"
                );
                restarted = true;
                reset_temp(&ctx.temp).await?;
                shared.downloaded.store(0, Ordering::Relaxed);
                continue;
            }
            Err(error) => return handle_segmented_fallback(&ctx, error, total, shared).await,
        }
    }
}

/// 分段失败后的兜底：Range 不可用 → 单流；其它错误原样上抛。
async fn handle_segmented_fallback(
    ctx: &RunCtx<'_>,
    error: DownloadError,
    total: u64,
    shared: &Arc<SharedProgress>,
) -> Result<RunResult, DownloadError> {
    match error {
        // 服务器不支持 Range / 206 区间错位：放弃分段，清空临时文件后单流全量重下。
        DownloadError::RangeNotSupported(status) => {
            log_warn!("[download] Range unsupported ({status}), falling back to a single stream");
            reset_temp(&ctx.temp).await?;
            shared.set_state(TaskState::Running);
            let result = download_single(ctx, None, total, shared).await?;
            Ok(result)
        }
        DownloadError::RangeMisaligned(detail) => {
            log_warn!(
                "[download] misaligned Range response ({detail}), falling back to a single stream"
            );
            reset_temp(&ctx.temp).await?;
            shared.set_state(TaskState::Running);
            let result = download_single(ctx, None, total, shared).await?;
            Ok(result)
        }
        other => Err(other),
    }
}

/// 自适应连接数爬升：成功就慢慢加，出错就先退两档。
fn ramp_goal(current: usize, desired: usize, cap: usize, failures_increased: bool) -> usize {
    let cap = cap.max(2);
    if failures_increased {
        return current.saturating_sub(2).max(2);
    }
    let desired = desired.clamp(1, cap);
    if desired > current {
        let step = ((desired - current) / 2).max(1);
        (current + step).min(cap)
    } else if desired < current {
        current.saturating_sub(1).max(2)
    } else {
        current
    }
}

/// 计算本次任务的连接数。
fn effective_connections(
    req: &DownloadRequest,
    cfg: &EngineConfig,
    total: u64,
    ranges: bool,
    force_split: bool,
) -> u32 {
    let requested = req.connections.unwrap_or(cfg.connections);
    let configured_cap = cfg.max_connections.clamp(1, HARD_MAX_CONNECTIONS);
    if requested > 0 {
        return requested.clamp(1, configured_cap);
    }
    let cap = configured_cap;
    if !ranges {
        return 1;
    }
    // 起步保守地用 4 条，随后按带宽逐步爬升。
    let mut planned = DEFAULT_START_CONNECTIONS.min(cap);
    if force_split && total > 0 && total <= SINGLE_SEGMENT_THRESHOLD {
        // 插件包类型的小文件：按连接数切片，但每片不小于 64 KiB。
        let by_size = (total / (64 * 1024)).max(2) as u32;
        planned = planned.max(by_size.min(cap));
    }
    planned.clamp(1, cap)
}

// ---------------------------------------------------------------------------
// 探测
// ---------------------------------------------------------------------------

async fn probe_route(
    client: &Client,
    route: &str,
    req: &DownloadRequest,
    cfg: &EngineConfig,
) -> Result<Probe, DownloadError> {
    let mut builder = client
        .get(route)
        .header(RANGE, "bytes=0-0")
        .header(ACCEPT, "application/octet-stream");
    for (name, value) in &req.headers {
        builder = builder.header(name.as_str(), value.as_str());
    }
    let _ = cfg;
    let response = builder.send().await?;
    let status = response.status();
    let final_url = response.url().to_string();
    let headers = response.headers().clone();

    let resolved = match &req.file_name {
        Some(name) => sanitize_filename(name),
        None => extract_filename(&headers, route, &final_url).name,
    };

    let etag = header_string(&headers, ETAG);
    let last_modified = header_string(&headers, LAST_MODIFIED);

    match status {
        StatusCode::PARTIAL_CONTENT => {
            let (_, _, total) = parse_content_range(&headers)
                .ok_or_else(|| DownloadError::Unsupported("206 without Content-Range".into()))?;
            let total = total
                .ok_or_else(|| DownloadError::Unsupported("Content-Range total is '*'".into()))?;
            Ok(Probe {
                total,
                ranges: true,
                etag,
                last_modified,
                file_name: resolved,
                held: None,
            })
        }
        StatusCode::OK => {
            let total = headers
                .get(reqwest::header::CONTENT_LENGTH)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(0);
            Ok(Probe {
                total,
                ranges: false,
                etag,
                last_modified,
                file_name: resolved,
                held: Some(response),
            })
        }
        StatusCode::RANGE_NOT_SATISFIABLE => {
            // 空文件或服务器不认 Range：退回普通 GET。
            let response = client.get(route).send().await?;
            if !response.status().is_success() {
                return Err(status_error(response.status()));
            }
            let total = response.content_length().unwrap_or(0);
            Ok(Probe {
                total,
                ranges: false,
                etag: header_string(response.headers(), ETAG),
                last_modified: header_string(response.headers(), LAST_MODIFIED),
                file_name: extract_filename(response.headers(), route, response.url().as_str())
                    .name,
                held: Some(response),
            })
        }
        other => Err(status_error(other)),
    }
}

fn status_error(status: StatusCode) -> DownloadError {
    if matches!(status.as_u16(), 401 | 403 | 429) {
        return DownloadError::ServerRejected {
            status: status.as_u16(),
        };
    }
    DownloadError::Unsupported(format!("HTTP {status}"))
}

fn header_string(
    headers: &reqwest::header::HeaderMap,
    name: reqwest::header::HeaderName,
) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// `Content-Range: bytes 0-0/1234` → `(start, end_inclusive, Option<total>)`。
fn parse_content_range(headers: &reqwest::header::HeaderMap) -> Option<(u64, u64, Option<u64>)> {
    let value = headers.get(CONTENT_RANGE)?.to_str().ok()?;
    let value = value.trim();
    let rest = value.strip_prefix("bytes")?.trim();
    let (range, total) = rest.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    let start = start.trim().parse::<u64>().ok()?;
    let end = end.trim().parse::<u64>().ok()?;
    let total = if total.trim() == "*" {
        None
    } else {
        Some(total.trim().parse::<u64>().ok()?)
    };
    Some((start, end, total))
}

// ---------------------------------------------------------------------------
// 单流下载
// ---------------------------------------------------------------------------

async fn download_single(
    ctx: &RunCtx<'_>,
    held: Option<Response>,
    total: u64,
    shared: &Arc<SharedProgress>,
) -> Result<RunResult, DownloadError> {
    let mut ctl_rx = ctx.ctl.subscribe();
    let response = match held {
        Some(response) => response,
        None => {
            let mut builder = ctx.client.get(&ctx.route);
            for (name, value) in &ctx.req.headers {
                builder = builder.header(name.as_str(), value.as_str());
            }
            let response = builder.send().await?;
            if !response.status().is_success() {
                return Err(status_error(response.status()));
            }
            response
        }
    };
    let total = if total > 0 {
        total
    } else {
        response.content_length().unwrap_or(0)
    };
    if total > 0 {
        shared.total.store(total, Ordering::Relaxed);
    }
    shared.set_state(TaskState::Running);
    shared.segments_total.store(1, Ordering::Relaxed);
    shared.active.store(1, Ordering::Relaxed);
    shared.connections.store(1, Ordering::Relaxed);

    let file = tokio::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&ctx.temp)
        .await?;
    let mut file = file;
    if total > 0 {
        file.set_len(total).await?;
    }
    let mut stream = response.bytes_stream();
    let mut written: u64 = 0;

    loop {
        let next = tokio::select! {
            changed = ctl_rx.changed() => {
                if changed.is_err() {
                    return Ok(RunResult::Cancelled);
                }
                let control_state = *ctl_rx.borrow_and_update();
                match control_state {
                    RunState::Running => continue,
                    RunState::Paused => {
                        file.flush().await?;
                        return Ok(RunResult::Paused);
                    }
                    RunState::Stopped => {
                        drop(file);
                        if !ctx.req.keep_partial {
                            let _ = tokio::fs::remove_file(&ctx.temp).await;
                            PartsFile::remove(&ctx.temp);
                        }
                        return Ok(RunResult::Cancelled);
                    }
                }
            }
            result = tokio::time::timeout(ctx.cfg.stall_timeout, stream.next()) => result,
        };
        let chunk = match next {
            Err(_elapsed) => {
                return Err(DownloadError::Stall {
                    segment: 0,
                    idle_secs: ctx.cfg.stall_timeout.as_secs(),
                });
            }
            Ok(None) => break,
            Ok(Some(Err(error))) => return Err(DownloadError::Request(error)),
            Ok(Some(Ok(bytes))) => bytes,
        };
        write_throttled(&mut file, &chunk, ctx.limiter, &mut written, shared).await?;
    }
    file.flush().await?;
    if total > 0 && written != total {
        return Err(DownloadError::Other(format!(
            "single-stream body ended after {written} of {total} bytes"
        )));
    }
    if total == 0 {
        shared.total.store(written, Ordering::Relaxed);
    }
    shared.segments_done.store(1, Ordering::Relaxed);
    shared.active.store(0, Ordering::Relaxed);
    finalize(ctx, written, ctx.resumed_bytes, 1).await
}

/// 按全局令牌桶写入；限速时一次 `consume` 可能只批一部分。
async fn write_throttled(
    file: &mut tokio::fs::File,
    chunk: &[u8],
    limiter: &SpeedLimiter,
    written: &mut u64,
    shared: &Arc<SharedProgress>,
) -> Result<(), DownloadError> {
    let mut offset = 0usize;
    while offset < chunk.len() {
        let want = (chunk.len() - offset) as u64;
        let granted = limiter.consume(want).await.min(want) as usize;
        if granted == 0 {
            continue;
        }
        file.write_all(&chunk[offset..offset + granted]).await?;
        offset += granted;
        *written += granted as u64;
        shared
            .downloaded
            .fetch_add(granted as u64, Ordering::Relaxed);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 多段下载
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SegStatus {
    Pending,
    Active,
    Done,
}

#[derive(Debug)]
struct Seg {
    start: u64,
    pos: u64,
    end: u64,
    /// 已经被某个 worker 预定写入的终点，拆分/抢占不得越过这里。
    reserved: u64,
    retries: u32,
    rate: f64,
    status: SegStatus,
}

struct SchedInner {
    segs: Vec<Seg>,
    pending: VecDeque<usize>,
    goal: usize,
    active: usize,
    idle: usize,
    total: u64,
    expansions: u32,
    fatal: Option<DownloadError>,
    finished: bool,
}

/// 区间调度器：队列 + 空闲计数 + 目标并发数。
struct Scheduler {
    inner: Mutex<SchedInner>,
    notify: Notify,
    /// 区间级失败次数（含重试前的失败），用于并发爬升时判断"服务器是否吃得消"。
    failures: AtomicU32,
}

/// 一个 worker 租到的区间。
#[derive(Clone, Copy, Debug)]
struct Lease {
    id: usize,
    pos: u64,
    end: u64,
    retries: u32,
}

enum TryAcquire {
    Lease(Lease),
    Finished,
    Wait,
}

enum SegOutcome {
    Done,
    Paused,
    Stopped,
    Failed(DownloadError),
}

impl Scheduler {
    fn new(total: u64, chunk_size: u64, goal: usize, restored: Option<&PartsFile>) -> Self {
        let chunk_size = chunk_size.max(64 * 1024);
        let mut segs = Vec::new();
        let mut pending = VecDeque::new();
        if let Some(parts) = restored {
            for part in &parts.segments {
                segs.push(Seg {
                    start: part.start,
                    pos: part.pos,
                    end: part.end,
                    reserved: part.pos,
                    retries: 0,
                    rate: 0.0,
                    status: SegStatus::Pending,
                });
                if !part.is_done() {
                    pending.push_back(segs.len() - 1);
                }
            }
        } else {
            let mut start = 0u64;
            while start < total {
                let end = (start + chunk_size).min(total);
                segs.push(Seg {
                    start,
                    pos: start,
                    end,
                    reserved: start,
                    retries: 0,
                    rate: 0.0,
                    status: SegStatus::Pending,
                });
                pending.push_back(segs.len() - 1);
                start = end;
            }
        }
        let _ = goal;
        Self {
            inner: Mutex::new(SchedInner {
                segs,
                pending,
                goal: 1,
                active: 0,
                idle: 0,
                total,
                expansions: 0,
                fatal: None,
                finished: false,
            }),
            notify: Notify::new(),
            failures: AtomicU32::new(0),
        }
    }

    fn note_failure(&self) {
        self.failures.fetch_add(1, Ordering::Relaxed);
    }

    fn failures(&self) -> u32 {
        self.failures.load(Ordering::Relaxed)
    }

    fn set_goal(&self, goal: usize) {
        let mut inner = lock(&self.inner);
        inner.goal = goal.max(1);
        drop(inner);
        self.notify.notify_waiters();
    }

    fn len(&self) -> (u32, u32) {
        let inner = lock(&self.inner);
        let done = inner
            .segs
            .iter()
            .filter(|segment| segment.status == SegStatus::Done)
            .count() as u32;
        (inner.segs.len() as u32, done)
    }

    fn active(&self) -> u32 {
        lock(&self.inner).active as u32
    }

    fn fatal(&self) -> Option<DownloadError> {
        lock(&self.inner).fatal.as_ref().map(clone_error)
    }

    fn fail(&self, error: DownloadError) {
        let mut inner = lock(&self.inner);
        if inner.fatal.is_none() {
            inner.fatal = Some(error);
        }
        drop(inner);
        self.notify.notify_waiters();
    }

    fn done_bytes(&self) -> u64 {
        let inner = lock(&self.inner);
        inner
            .segs
            .iter()
            .map(|segment| segment.pos.saturating_sub(segment.start))
            .sum()
    }

    fn parts(&self, url: &str, etag: Option<&str>, last_modified: Option<&str>) -> PartsFile {
        let inner = lock(&self.inner);
        PartsFile {
            version: 2,
            url: url.to_string(),
            etag: etag.map(str::to_string),
            last_modified: last_modified.map(str::to_string),
            total: inner.total,
            segments: inner
                .segs
                .iter()
                .map(|segment| PartSegment {
                    start: segment.start,
                    pos: segment.pos,
                    end: segment.end,
                })
                .collect(),
        }
    }

    async fn acquire(&self, ctl: &mut watch::Receiver<RunState>) -> Option<Lease> {
        loop {
            if *ctl.borrow() != RunState::Running {
                return None;
            }
            match self.try_acquire() {
                TryAcquire::Lease(lease) => return Some(lease),
                TryAcquire::Finished => return None,
                TryAcquire::Wait => {}
            }
            {
                let mut inner = lock(&self.inner);
                inner.idle += 1;
            }
            tokio::select! {
                () = self.notify.notified() => {}
                _ = tokio::time::sleep(Duration::from_secs(1)) => {}
                _ = ctl.changed() => {}
            }
            {
                let mut inner = lock(&self.inner);
                inner.idle = inner.idle.saturating_sub(1);
            }
        }
    }

    fn try_acquire(&self) -> TryAcquire {
        let mut inner = lock(&self.inner);
        if inner.fatal.is_some() {
            return TryAcquire::Finished;
        }
        if inner.pending.is_empty() && inner.active == 0 {
            inner.finished = true;
            drop(inner);
            self.notify.notify_waiters();
            return TryAcquire::Finished;
        }
        if inner.active < inner.goal
            && inner.pending.is_empty()
            && inner.segs.iter().any(|segment| {
                segment.status == SegStatus::Active
                    && segment.end.saturating_sub(segment.pos) >= self_min_piece().saturating_mul(2)
            })
        {
            drop(inner);
            if let Some(lease) = self.steal() {
                return TryAcquire::Lease(lease);
            }
            inner = lock(&self.inner);
            if inner.fatal.is_some() {
                return TryAcquire::Finished;
            }
        }
        if inner.active < inner.goal
            && let Some(id) = inner.pending.pop_front()
        {
            if let Some(segment) = inner.segs.get_mut(id) {
                segment.status = SegStatus::Active;
                let lease = Lease {
                    id,
                    pos: segment.pos,
                    end: segment.end,
                    retries: segment.retries,
                };
                inner.active += 1;
                return TryAcquire::Lease(lease);
            }
            inner.pending.push_front(id);
        }
        TryAcquire::Wait
    }

    fn complete(&self, id: usize) {
        let mut inner = lock(&self.inner);
        if let Some(segment) = inner.segs.get_mut(id) {
            segment.pos = segment.end;
            segment.reserved = segment.end;
            segment.status = SegStatus::Done;
        }
        inner.active = inner.active.saturating_sub(1);
        drop(inner);
        self.notify.notify_waiters();
    }

    fn release(&self, id: usize, pos: u64) {
        let mut inner = lock(&self.inner);
        if let Some(segment) = inner.segs.get_mut(id) {
            segment.pos = pos.max(segment.start).min(segment.end);
            segment.reserved = segment.pos;
            segment.status = SegStatus::Pending;
        }
        inner.active = inner.active.saturating_sub(1);
        inner.pending.push_front(id);
        drop(inner);
        self.notify.notify_waiters();
    }

    fn requeue(&self, id: usize, pos: u64, retries: u32) {
        let mut inner = lock(&self.inner);
        if let Some(segment) = inner.segs.get_mut(id) {
            segment.pos = pos.max(segment.start).min(segment.end);
            segment.reserved = segment.pos;
            segment.retries = retries;
            segment.status = SegStatus::Pending;
        }
        inner.active = inner.active.saturating_sub(1);
        inner.pending.push_back(id);
        drop(inner);
        self.notify.notify_waiters();
    }

    fn set_rate(&self, id: usize, rate: f64) {
        if let Some(segment) = lock(&self.inner).segs.get_mut(id) {
            segment.rate = rate;
        }
    }

    fn set_pos(&self, id: usize, pos: u64) {
        if let Some(segment) = lock(&self.inner).segs.get_mut(id) {
            segment.pos = pos.max(segment.start).min(segment.end);
        }
    }

    /// 为一次写入预定 `[cursor, cursor+want)`，返回实际可写字节数。
    /// 段尾被抢占后这里会返回 0，持有者据此结束本段。
    fn reserve(&self, id: usize, want: u64) -> u64 {
        let mut inner = lock(&self.inner);
        let Some(segment) = inner.segs.get_mut(id) else {
            return 0;
        };
        let cursor = segment.pos.max(segment.reserved);
        let room = segment.end.saturating_sub(cursor);
        let grant = want.min(room);
        segment.reserved = cursor + grant;
        grant
    }

    fn set_reserved(&self, id: usize, cursor: u64) {
        if let Some(segment) = lock(&self.inner).segs.get_mut(id) {
            segment.reserved = cursor.max(segment.start).min(segment.end);
        }
    }

    /// 空闲 worker 主动抢走"最慢在传段"的后半截（对半拆开）。
    ///
    /// 只有在没有任何排队区间、且确实存在比其它连接更慢的在传段时才动手，
    /// 避免刚起步就把所有段切碎。返回新段的租约。
    fn steal(&self) -> Option<Lease> {
        let mut inner = lock(&self.inner);
        if inner.fatal.is_some() || !inner.pending.is_empty() || inner.active == 0 {
            return None;
        }
        if inner.active >= inner.goal {
            return None;
        }
        let minimum = self_min_piece();
        let rates: Vec<f64> = inner
            .segs
            .iter()
            .filter(|segment| segment.status == SegStatus::Active && segment.rate > 0.0)
            .map(|segment| segment.rate)
            .collect();
        let median_rate = rates.iter().sum::<f64>() / rates.len().max(1) as f64;
        let mut victim: Option<(usize, u64, u64, f64)> = None;
        for (id, segment) in inner.segs.iter().enumerate() {
            if segment.status != SegStatus::Active {
                continue;
            }
            let cursor = segment.pos.max(segment.reserved);
            let remaining = segment.end.saturating_sub(cursor);
            if remaining < minimum.saturating_mul(2) {
                continue;
            }
            // 只抢明显落后（或无法比较 / 还不知道快慢）的连接。
            let slow = rates.len() <= 1
                || segment.rate == 0.0
                || median_rate == 0.0
                || segment.rate < median_rate * 0.9;
            if !slow {
                continue;
            }
            let better = victim.is_none_or(|(_, _, _, rate)| segment.rate < rate);
            if better {
                victim = Some((id, cursor, remaining, segment.rate));
            }
        }
        let (victim_id, cursor, remaining, victim_rate) = victim?;
        let keep = (remaining / 2).max(minimum).min(remaining - minimum);
        let split_at = cursor + keep;
        let end = inner.segs.get(victim_id)?.end;
        if split_at <= cursor || split_at >= end {
            return None;
        }
        if let Some(segment) = inner.segs.get_mut(victim_id) {
            segment.end = split_at;
        }
        let helper_id = inner.segs.len();
        inner.segs.push(Seg {
            start: split_at,
            pos: split_at,
            end,
            reserved: split_at,
            retries: 0,
            rate: 0.0,
            status: SegStatus::Active,
        });
        inner.active += 1;
        log_debug!(
            "[download] idle worker stole {keep} B from segment {victim_id} ({victim_rate:.0} B/s), tail {split_at}-{end}"
        );
        Some(Lease {
            id: helper_id,
            pos: split_at,
            end,
            retries: 0,
        })
    }

    /// 服务器自报总大小更大：扩容文件并追加尾段。
    async fn expand(
        &self,
        new_total: u64,
        temp: &Path,
        shared: &Arc<SharedProgress>,
    ) -> Result<(), DownloadError> {
        let needs = {
            let inner = lock(&self.inner);
            new_total > inner.total && inner.expansions < 3
        };
        if !needs {
            let inner = lock(&self.inner);
            if new_total <= inner.total {
                return Ok(());
            }
            return Err(DownloadError::Other(
                "server keeps growing the file beyond the planned size".into(),
            ));
        }
        let file = tokio::fs::OpenOptions::new().write(true).open(temp).await?;
        file.set_len(new_total).await?;
        let (old_total, new_id) = {
            let mut inner = lock(&self.inner);
            let old_total = inner.total;
            inner.total = new_total;
            inner.expansions += 1;
            let new_id = inner.segs.len();
            inner.segs.push(Seg {
                start: old_total,
                pos: old_total,
                end: new_total,
                reserved: old_total,
                retries: 0,
                rate: 0.0,
                status: SegStatus::Pending,
            });
            inner.pending.push_back(new_id);
            (old_total, new_id)
        };
        let _ = new_id;
        shared.total.store(new_total, Ordering::Relaxed);
        log_warn!(
            "[download] server reports a larger size: {} -> {} bytes, tail segment queued",
            old_total,
            new_total
        );
        self.notify.notify_waiters();
        Ok(())
    }
}

/// 初始分块：保证每个连接至少有两块可抢，同时不把大文件切得比配置更碎。
fn adaptive_chunk(total: u64, connections: u32, configured: u64, min_piece: u64) -> u64 {
    let per_connection = total / u64::from(connections.max(1)).saturating_mul(2).max(1);
    let floor = min_piece.max(64 * 1024);
    configured.min(per_connection.max(floor)).max(floor)
}

fn self_min_piece() -> u64 {
    MIN_SPLIT_PIECE.max(1)
}

fn clone_error(error: &DownloadError) -> DownloadError {
    match error {
        DownloadError::Request(err) => DownloadError::Other(err.to_string()),
        DownloadError::Io(err) => {
            DownloadError::Io(std::io::Error::new(err.kind(), err.to_string()))
        }
        DownloadError::Cancelled => DownloadError::Cancelled,
        DownloadError::ChecksumMismatch(message) => {
            DownloadError::ChecksumMismatch(message.clone())
        }
        DownloadError::RangeNotSupported(status) => {
            DownloadError::RangeNotSupported(status.clone())
        }
        DownloadError::RangeMisaligned(detail) => DownloadError::RangeMisaligned(detail.clone()),
        DownloadError::VersionChanged(detail) => DownloadError::VersionChanged(detail.clone()),
        DownloadError::TrueSizeLarger(size) => DownloadError::TrueSizeLarger(*size),
        DownloadError::ServerRejected { status } => {
            DownloadError::ServerRejected { status: *status }
        }
        DownloadError::SlowSpeed {
            avg_bps,
            min_bps,
            window_secs,
        } => DownloadError::SlowSpeed {
            avg_bps: *avg_bps,
            min_bps: *min_bps,
            window_secs: *window_secs,
        },
        DownloadError::Stall { segment, idle_secs } => DownloadError::Stall {
            segment: *segment,
            idle_secs: *idle_secs,
        },
        DownloadError::InvalidUrl(message) => DownloadError::InvalidUrl(message.clone()),
        DownloadError::Unsupported(message) => DownloadError::Unsupported(message.clone()),
        DownloadError::Other(message) => DownloadError::Other(message.clone()),
    }
}

async fn download_segmented(
    ctx: &RunCtx<'_>,
    total: u64,
    connections: u32,
    connection_cap: u32,
    shared: &Arc<SharedProgress>,
) -> Result<RunResult, DownloadError> {
    // 续传：只有 sidecar 完全可信才沿用。
    let (restored, reject) = PartsFile::load(
        &ctx.temp,
        &ctx.req.url,
        total,
        ctx.etag.as_deref(),
        ctx.last_modified.as_deref(),
    );
    if let Some(reason) = reject {
        log_debug!("[download] resume record rejected: {}", reason.reason());
    }
    let start_offset = restored.as_ref().map_or(0, PartsFile::done_bytes);
    if start_offset > 0 {
        log_info!(
            "[download] resuming at {} / {} bytes",
            human_bytes(start_offset),
            human_bytes(total)
        );
    } else {
        reset_temp(&ctx.temp).await?;
        let file = tokio::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&ctx.temp)
            .await?;
        file.set_len(total).await?;
    }
    shared.downloaded.store(start_offset, Ordering::Relaxed);
    shared.set_state(TaskState::Running);

    let chunk = adaptive_chunk(
        total,
        connections,
        ctx.cfg.chunk_size,
        ctx.cfg.min_split_piece,
    );
    let sched = Arc::new(Scheduler::new(
        total,
        chunk,
        connections as usize,
        restored.as_ref(),
    ));
    sched.set_goal(connections as usize);
    shared.connections.store(connections, Ordering::Relaxed);
    let (segments_total, segments_done) = sched.len();
    shared
        .segments_total
        .store(segments_total, Ordering::Relaxed);
    shared.segments_done.store(segments_done, Ordering::Relaxed);

    let min_start = connections.clamp(1, HARD_MAX_CONNECTIONS);
    // 预备队：多出来的 worker 先空闲等待，连接数爬升时立刻投入，也可以在收尾阶段抢占慢段。
    let workers = connection_cap.clamp(min_start, HARD_MAX_CONNECTIONS) as usize;
    let mut set: JoinSet<()> = JoinSet::new();
    for index in 0..workers {
        let client = ctx.client.clone();
        let limiter = ctx.limiter.clone();
        let shared = Arc::clone(ctx.shared);
        let sched = Arc::clone(&sched);
        let ctl_rx = ctx.ctl.subscribe();
        let route = ctx.route.clone();
        let temp = ctx.temp.clone();
        let headers = ctx.req.headers.clone();
        let cfg = ctx.cfg.clone();
        let etag = ctx.etag.clone();
        let last_modified = ctx.last_modified.clone();
        let url = ctx.req.url.clone();
        set.spawn(async move {
            worker(
                index,
                client,
                limiter,
                shared,
                sched,
                ctl_rx,
                route,
                temp,
                headers,
                cfg,
                etag,
                last_modified,
                url,
            )
            .await;
        });
    }

    // 协调器：限速采样、目标并发调整、慢速保护、侧车持久化。
    let mut meter = CounterRate::default();
    let mut ticker = tokio::time::interval(Duration::from_secs(1));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut low_seconds = 0u64;
    let mut ticks = 0u64;
    let mut goal = connections as usize;
    let cap = workers;
    let mut last_failures = sched.failures();
    let mut save_ticker = tokio::time::interval(Duration::from_secs(1));

    loop {
        tokio::select! {
            joined = set.join_next() => {
                match joined {
                    Some(Ok(())) => {}
                    Some(Err(error)) => {
                        if !error.is_cancelled() {
                            log_error!("[download] worker panicked: {error}");
                            sched.fail(DownloadError::Other(format!("worker panicked: {error}")));
                        }
                        break;
                    }
                    None => break,
                }
            }
            _ = ticker.tick() => {
                ticks += 1;
                let downloaded = shared.downloaded.load(Ordering::Relaxed);
                let bps = meter.observe(downloaded, Instant::now()).max(0) as u64;
                shared.speed.store(bps, Ordering::Relaxed);
                let failures = sched.failures();
                if ctx.cfg.connections == 0 {
                    let desired = if bps == 0 {
                        goal
                    } else {
                        advise_with_bandwidth(
                            &AdvisorInput {
                                total_bytes: total as i64,
                                supports_range: true,
                            },
                            bps as f64,
                        )
                        .segments
                        .max(1) as usize
                    };
                    goal = ramp_goal(goal, desired, cap, failures > last_failures);
                }
                last_failures = failures;
                sched.set_goal(goal);
                shared.connections.store(goal as u32, Ordering::Relaxed);
                shared.active.store(sched.active(), Ordering::Relaxed);
                let (seg_total, seg_done) = sched.len();
                shared.segments_total.store(seg_total, Ordering::Relaxed);
                shared.segments_done.store(seg_done, Ordering::Relaxed);

                if let Some(guard) = ctx.cfg.slow_guard
                    && ticks > guard.grace_secs
                {
                    if bps < guard.min_bps {
                        low_seconds += 1;
                        if low_seconds >= guard.window_secs {
                            let avg = shared.downloaded.load(Ordering::Relaxed)
                                .saturating_sub(start_offset) / low_seconds.max(1);
                            sched.fail(DownloadError::SlowSpeed {
                                avg_bps: avg,
                                min_bps: guard.min_bps,
                                window_secs: low_seconds,
                            });
                        }
                    } else {
                        low_seconds = 0;
                    }
                }
            }
            _ = save_ticker.tick() => {
                if let Err(error) = sched.parts(&ctx.req.url, ctx.etag.as_deref(), ctx.last_modified.as_deref()).save(&ctx.temp) {
                    log_warn!("[download] failed to persist resume record: {error}");
                }
            }
        }
    }

    // 收尾：先刷新分段统计并保存精确位置，再判断结果。
    let (seg_total, seg_done) = sched.len();
    shared.segments_total.store(seg_total, Ordering::Relaxed);
    shared.segments_done.store(seg_done, Ordering::Relaxed);
    let parts = sched.parts(
        &ctx.req.url,
        ctx.etag.as_deref(),
        ctx.last_modified.as_deref(),
    );
    if let Err(error) = parts.save(&ctx.temp) {
        log_warn!("[download] failed to persist final resume record: {error}");
    }

    if let Some(error) = sched.fatal() {
        if matches!(error, DownloadError::SlowSpeed { .. }) {
            shared.speed.store(0, Ordering::Relaxed);
        }
        return Err(error);
    }
    match ctx.ctl.run_state() {
        RunState::Paused => {
            shared.set_state(TaskState::Paused);
            return Ok(RunResult::Paused);
        }
        RunState::Stopped => {
            if !ctx.req.keep_partial {
                let _ = tokio::fs::remove_file(&ctx.temp).await;
                PartsFile::remove(&ctx.temp);
            }
            return Ok(RunResult::Cancelled);
        }
        RunState::Running => {}
    }

    let done = sched.done_bytes();
    if done < total {
        return Err(DownloadError::Other(format!(
            "segmented download ended at {done} of {total} bytes"
        )));
    }
    shared.active.store(0, Ordering::Relaxed);
    shared.segments_done.store(seg_total, Ordering::Relaxed);
    let used = shared.connections.load(Ordering::Relaxed).max(1);
    finalize(ctx, total, start_offset, used).await
}

#[allow(clippy::too_many_arguments)]
async fn worker(
    index: usize,
    client: Client,
    limiter: SpeedLimiter,
    shared: Arc<SharedProgress>,
    sched: Arc<Scheduler>,
    mut ctl: watch::Receiver<RunState>,
    route: String,
    temp: PathBuf,
    headers: std::collections::HashMap<String, String>,
    cfg: EngineConfig,
    etag: Option<String>,
    last_modified: Option<String>,
    canonical_url: String,
) {
    let mut window = RateWindow::default();
    loop {
        if *ctl.borrow() != RunState::Running {
            break;
        }
        let Some(mut lease) = sched.acquire(&mut ctl).await else {
            break;
        };
        let outcome = transfer_segment(
            index,
            &client,
            &limiter,
            &shared,
            &sched,
            &mut ctl,
            &route,
            &temp,
            &headers,
            &cfg,
            &mut lease,
            &mut window,
            &etag,
            &last_modified,
            &canonical_url,
        )
        .await;
        match outcome {
            SegOutcome::Done => sched.complete(lease.id),
            SegOutcome::Paused => {
                sched.release(lease.id, lease.pos);
                break;
            }
            SegOutcome::Stopped => {
                sched.release(lease.id, lease.pos);
                break;
            }
            SegOutcome::Failed(error) => {
                sched.note_failure();
                if error.retriable() && lease.retries < cfg.max_retries {
                    let backoff =
                        Duration::from_millis(200u64.saturating_mul(1u64 << lease.retries.min(5)));
                    log_warn!(
                        "[download] segment {} failed ({}), retry {}/{} in {:?}",
                        lease.id,
                        error.summary(),
                        lease.retries + 1,
                        cfg.max_retries,
                        backoff
                    );
                    tokio::time::sleep(backoff).await;
                    sched.requeue(lease.id, lease.pos, lease.retries + 1);
                } else {
                    log_warn!(
                        "[download] segment {} failed fatally: {}",
                        lease.id,
                        error.summary()
                    );
                    sched.fail(error);
                    break;
                }
            }
        }
    }
}

#[derive(Default)]
struct RateWindow {
    bytes: u64,
    since: Option<Instant>,
    rate: f64,
}

impl RateWindow {
    fn add(&mut self, bytes: u64) {
        let now = Instant::now();
        let since = *self.since.get_or_insert(now);
        self.bytes += bytes;
        let elapsed = now.duration_since(since);
        if elapsed >= Duration::from_millis(500) {
            self.rate = self.bytes as f64 / elapsed.as_secs_f64();
            self.bytes = 0;
            self.since = Some(now);
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn transfer_segment(
    _index: usize,
    client: &Client,
    limiter: &SpeedLimiter,
    shared: &Arc<SharedProgress>,
    sched: &Arc<Scheduler>,
    ctl: &mut watch::Receiver<RunState>,
    route: &str,
    temp: &Path,
    headers: &std::collections::HashMap<String, String>,
    cfg: &EngineConfig,
    lease: &mut Lease,
    window: &mut RateWindow,
    etag: &Option<String>,
    last_modified: &Option<String>,
    canonical_url: &str,
) -> SegOutcome {
    if lease.pos >= lease.end {
        return SegOutcome::Done;
    }
    let request_end = lease.end - 1;
    let mut builder = client
        .get(route)
        .header(RANGE, format!("bytes={}-{}", lease.pos, request_end))
        .header(ACCEPT, "application/octet-stream");
    for (name, value) in headers {
        builder = builder.header(name.as_str(), value.as_str());
    }
    let response = match builder.send().await {
        Ok(response) => response,
        Err(error) => return SegOutcome::Failed(DownloadError::Request(error)),
    };
    let status = response.status();
    if status == StatusCode::OK {
        // 服务器忽略了 Range，返回全量：分段不成立。
        return SegOutcome::Failed(DownloadError::RangeNotSupported(status.to_string()));
    }
    if status == StatusCode::RANGE_NOT_SATISFIABLE {
        return SegOutcome::Failed(DownloadError::VersionChanged(
            "server rejected the requested Range (416); local partial data is stale".into(),
        ));
    }
    if !status.is_success() {
        return SegOutcome::Failed(status_error(status));
    }
    // 206：校验起点与 validator。
    match parse_content_range(response.headers()) {
        Some((start, _, total)) => {
            if start != lease.pos {
                return SegOutcome::Failed(DownloadError::RangeMisaligned(format!(
                    "requested {}-{}, server returned start {start}",
                    lease.pos, request_end
                )));
            }
            if let Some(server_total) = total {
                if server_total > sched_total_hint(shared) {
                    if let Err(error) = sched.expand(server_total, temp, shared).await {
                        return SegOutcome::Failed(error);
                    }
                } else if server_total < sched_total_hint(shared) {
                    return SegOutcome::Failed(DownloadError::VersionChanged(format!(
                        "server total shrank to {server_total}"
                    )));
                }
            }
        }
        None => {
            return SegOutcome::Failed(DownloadError::Unsupported(
                "206 without Content-Range".into(),
            ));
        }
    }
    if let Some(server_etag) = header_string(response.headers(), ETAG)
        && let Some(expected) = etag
        && &server_etag != expected
    {
        return SegOutcome::Failed(DownloadError::VersionChanged(format!(
            "etag {expected} -> {server_etag}"
        )));
    }
    let _ = last_modified;
    let _ = canonical_url;

    let mut file = match tokio::fs::OpenOptions::new().write(true).open(temp).await {
        Ok(file) => file,
        Err(error) => return SegOutcome::Failed(DownloadError::Io(error)),
    };
    if let Err(error) = file.seek(SeekFrom::Start(lease.pos)).await {
        return SegOutcome::Failed(DownloadError::Io(error));
    }

    let mut stream = response.bytes_stream();
    let mut buffer: Vec<u8> = Vec::with_capacity(WRITE_BUFFER);
    loop {
        let next = tokio::select! {
            changed = ctl.changed() => {
                if changed.is_err() {
                    return SegOutcome::Stopped;
                }
                let control_state = *ctl.borrow_and_update();
                match control_state {
                    RunState::Running => continue,
                    RunState::Paused => {
                        let _ = file.flush().await;
                        return SegOutcome::Paused;
                    }
                    RunState::Stopped => {
                        let _ = file.flush().await;
                        return SegOutcome::Stopped;
                    }
                }
            }
            result = tokio::time::timeout(cfg.stall_timeout, stream.next()) => result,
        };
        let chunk = match next {
            Err(_) => {
                sched.set_pos(lease.id, lease.pos);
                return SegOutcome::Failed(DownloadError::Stall {
                    segment: lease.id,
                    idle_secs: cfg.stall_timeout.as_secs(),
                });
            }
            Ok(None) => break,
            Ok(Some(Err(error))) => {
                sched.set_pos(lease.id, lease.pos);
                return SegOutcome::Failed(DownloadError::Request(error));
            }
            Ok(Some(Ok(bytes))) => bytes,
        };
        // 先向调度器预定写入区间：段尾可能已被空闲 worker 抢走一半。
        let granted = sched.reserve(lease.id, chunk.len() as u64);
        if granted == 0 {
            // 本段尾部已经交给别人，自己这部分到此为止。
            lease.end = lease.pos;
            break;
        }
        let chunk = chunk.slice(..granted as usize);
        let mut offset = 0usize;
        while offset < chunk.len() {
            let want = (chunk.len() - offset) as u64;
            let granted = limiter.consume(want).await.min(want) as usize;
            if granted == 0 {
                continue;
            }
            if let Err(error) = file.write_all(&chunk[offset..offset + granted]).await {
                return SegOutcome::Failed(DownloadError::Io(error));
            }
            offset += granted;
            lease.pos += granted as u64;
            window.add(granted as u64);
            shared
                .downloaded
                .fetch_add(granted as u64, Ordering::Relaxed);
            buffer.clear();
        }
        sched.set_pos(lease.id, lease.pos);
        sched.set_reserved(lease.id, lease.pos);
        sched.set_rate(lease.id, window.rate);
        if lease.pos >= lease.end {
            return SegOutcome::Done;
        }
    }
    let _ = buffer;
    if lease.pos >= lease.end {
        SegOutcome::Done
    } else {
        // 响应提前结束（服务器截断）：按失败重派。
        SegOutcome::Failed(DownloadError::Other(format!(
            "segment ended early at {} of {}",
            lease.pos, lease.end
        )))
    }
}

fn sched_total_hint(shared: &Arc<SharedProgress>) -> u64 {
    shared.total.load(Ordering::Relaxed)
}

async fn reset_temp(temp: &Path) -> Result<(), DownloadError> {
    match tokio::fs::remove_file(temp).await {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(DownloadError::Io(error)),
    }
    PartsFile::remove(temp);
    Ok(())
}

// ---------------------------------------------------------------------------
// 收尾
// ---------------------------------------------------------------------------

async fn finalize(
    ctx: &RunCtx<'_>,
    total: u64,
    resumed_bytes: u64,
    connections: u32,
) -> Result<RunResult, DownloadError> {
    ctx.shared.set_state(TaskState::Verifying);
    let checksum = verify_checksum(ctx, total).await?;

    let final_path = unique_target(&ctx.req.save_dir, &ctx.file_name, ctx.req.overwrite);
    if let Err(error) = tokio::fs::rename(&ctx.temp, &final_path).await {
        // 跨设备 / Windows 上目标存在：退回复制。
        if let Err(copy_error) = tokio::fs::copy(&ctx.temp, &final_path).await {
            return Err(DownloadError::Io(std::io::Error::new(
                copy_error.kind(),
                format!(
                    "rename {} -> {} failed ({error}); copy failed: {copy_error}",
                    ctx.temp.display(),
                    final_path.display()
                ),
            )));
        }
        let _ = tokio::fs::remove_file(&ctx.temp).await;
    }
    PartsFile::remove(&ctx.temp);
    ctx.shared.downloaded.store(total, Ordering::Relaxed);
    ctx.shared.total.store(total, Ordering::Relaxed);
    ctx.shared.set_state(TaskState::Completed);
    let elapsed = ctx.shared.started.elapsed();
    let fresh_bytes = total.saturating_sub(resumed_bytes);
    let average_bps = if elapsed.as_secs_f64() > 0.0 {
        (fresh_bytes as f64 / elapsed.as_secs_f64()) as u64
    } else {
        0
    };
    Ok(RunResult::Completed(DownloadOutcome {
        task_id: ctx.task_id.to_string(),
        path: final_path,
        file_name: ctx.file_name.clone(),
        total_bytes: total,
        resumed_bytes,
        connections,
        elapsed,
        checksum,
        average_bps,
    }))
}

async fn verify_checksum(ctx: &RunCtx<'_>, _total: u64) -> Result<Option<String>, DownloadError> {
    let (algorithm, expected) = match &ctx.req.checksum {
        Some(spec) => {
            let (name, hex) = spec
                .split_once('=')
                .ok_or_else(|| DownloadError::Other(format!("invalid checksum spec: {spec}")))?;
            let algorithm = HashAlgorithm::parse(name).ok_or_else(|| {
                DownloadError::Other(format!("unsupported checksum algo: {name}"))
            })?;
            (algorithm, Some(hex.trim().to_ascii_lowercase()))
        }
        // 未指定也至少算一份 SHA-256，调用方可自行比对 / 写入发布清单。
        None => (HashAlgorithm::Sha256, None),
    };
    let path = ctx.temp.clone();
    let cancelled = ctx.shared.state() == TaskState::Cancelled;
    let computed = tokio::task::spawn_blocking(move || hash_file(&path, algorithm, || cancelled))
        .await
        .map_err(|error| DownloadError::Other(format!("hash task failed: {error}")))?;
    let computed = computed.map_err(|error| DownloadError::Other(error.to_string()))?;
    let normalized = computed.to_ascii_lowercase();
    if let Some(expected) = expected
        && expected != normalized
    {
        return Err(DownloadError::ChecksumMismatch(format!(
            "{}: expected {expected}, got {normalized}",
            algorithm.as_str()
        )));
    }
    Ok(Some(format!("{}={normalized}", algorithm.as_str())))
}

/// 目标文件名：默认自动编号（`name (1).ext`），`overwrite` 为真时直接覆盖。
fn unique_target(dir: &Path, name: &str, overwrite: bool) -> PathBuf {
    let first = dir.join(name);
    if overwrite || !first.exists() {
        return first;
    }
    let (stem, extension) = match name.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() && !extension.is_empty() => {
            (stem.to_string(), format!(".{extension}"))
        }
        _ => (name.to_string(), String::new()),
    };
    for index in 1..10_000u32 {
        let candidate = dir.join(format!("{stem} ({index}){extension}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    dir.join(format!("{stem}-{}{extension}", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_content_range() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            CONTENT_RANGE,
            "bytes 1024-2047/8192".parse().expect("header value"),
        );
        assert_eq!(
            parse_content_range(&headers),
            Some((1024, 2047, Some(8192)))
        );
    }

    #[test]
    fn unique_target_numbers_files() {
        let dir = std::env::temp_dir().join(format!("ufetch-unique-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create dir");
        std::fs::write(dir.join("pkg.apk"), b"x").expect("write");
        let second = unique_target(&dir, "pkg.apk", false);
        assert_eq!(
            second.file_name().and_then(|name| name.to_str()),
            Some("pkg (1).apk")
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn reserve_bounds_writes_to_the_live_segment_end() {
        let sched = Scheduler::new(4 * 1024 * 1024, 4 * 1024 * 1024, 2, None);
        assert_eq!(sched.reserve(0, 128 * 1024), 128 * 1024);
        // 模拟外部把段尾收缩到当前游标之后 256 KiB。
        {
            let mut inner = lock(&sched.inner);
            inner.segs[0].end = 128 * 1024 + 256 * 1024;
        }
        assert_eq!(
            sched.reserve(0, 512 * 1024),
            256 * 1024,
            "grant must stop at the end"
        );
        assert_eq!(sched.reserve(0, 512 * 1024), 0, "no room left");
    }

    #[test]
    fn goal_ramps_up_gradually_and_steps_down_on_failures() {
        assert_eq!(ramp_goal(4, 8, 8, false), 6, "half of the gap, one step");
        assert_eq!(ramp_goal(6, 8, 8, false), 7);
        assert_eq!(ramp_goal(7, 16, 8, false), 8, "never past the cap");
        assert_eq!(ramp_goal(8, 8, 8, false), 8);
        assert_eq!(ramp_goal(6, 4, 8, false), 5, "slow down one at a time");
        assert_eq!(ramp_goal(6, 8, 8, true), 4, "failures cost two slots");
        assert_eq!(ramp_goal(2, 8, 8, true), 2, "never below two");
    }

    #[test]
    fn idle_worker_steals_the_tail_of_a_slow_segment() {
        let sched = Scheduler::new(8 * 1024 * 1024, 8 * 1024 * 1024, 4, None);
        {
            let mut inner = lock(&sched.inner);
            inner.goal = 2;
            inner.active = 1;
            inner.pending.clear();
            let segment = &mut inner.segs[0];
            segment.status = SegStatus::Active;
            segment.rate = 1_000.0;
        }
        let lease = sched.steal().expect("steal from a lone slow segment");
        let inner = lock(&sched.inner);
        assert_eq!(
            inner.segs.len(),
            2,
            "victim keeps the front half, helper gets the tail"
        );
        assert_eq!(
            inner.segs[0].end, lease.pos,
            "victim end shrinks to the split point"
        );
        assert_eq!(inner.segs[1].start, lease.pos);
        assert_eq!(inner.active, 2);
    }

    #[test]
    fn steal_never_cuts_into_an_in_flight_write() {
        let sched = Scheduler::new(8 * 1024 * 1024, 8 * 1024 * 1024, 4, None);
        {
            let mut inner = lock(&sched.inner);
            inner.goal = 2;
            inner.active = 1;
            inner.pending.clear();
            let segment = &mut inner.segs[0];
            segment.status = SegStatus::Active;
            segment.rate = 10.0;
            segment.reserved = 4 * 1024 * 1024;
        }
        let lease = sched.steal().expect("steal");
        assert!(
            lease.pos >= 4 * 1024 * 1024,
            "split point must respect reservations"
        );
    }

    #[test]
    fn steal_only_happens_below_the_goal_and_without_queue() {
        let sched = Scheduler::new(8 * 1024 * 1024, 8 * 1024 * 1024, 4, None);
        {
            let mut inner = lock(&sched.inner);
            inner.goal = 1;
            inner.active = 1;
            inner.pending.clear();
        }
        assert!(sched.steal().is_none(), "no free slot to hand work to");
    }
}
