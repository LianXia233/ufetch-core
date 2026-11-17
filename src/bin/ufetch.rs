// SPDX-License-Identifier: LGPL-3.0-or-later
// Copyright (c) 2026 UFetch-Core contributors

//! `ufetch` —— 面向 OpenWrt 插件的最小下载 CLI。
//!
//! ```text
//! ufetch [OPTIONS] <URL>
//! ufetch pause|resume|cancel --control-file <PATH>
//! ```

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::{Duration, Instant};

use ufetch_core::{
    CacheEntry, CacheStore, DownloadError, DownloadManager, DownloadRequest, ManagerConfig,
    Progress, Settings, TaskRecord, TaskRegistry, TaskState, default_home, human_bytes, now_secs,
    write_control,
};

const USAGE: &str = "\
ufetch — HTTP 下载内核 CLI

USAGE:
    ufetch [OPTIONS] <URL>
    ufetch list [--json]
    ufetch status <ID> [--json]
    ufetch start|pause|resume|cancel <ID> [--control-file <PATH>]
    ufetch delete <ID> [--keep-file]
    ufetch install <ID> [--keep-file]
    ufetch install-ok <ID>
    ufetch cache [list|clear] [--json]
    ufetch config [--json]
    ufetch config set <KEY=VALUE>...
    ufetch log [--tail <N>]

OPTIONS:
    -o, --output-dir <DIR>     保存目录（默认缓存目录）
    -f, --file-name <NAME>     指定文件名
    -n, --connections <N>      并发连接数（0 = 自适应，默认 0）
    -l, --limit <RATE>         限速，如 512K / 2M（默认不限）
    -m, --mirror <PREFIX>      镜像线路前缀，可重复（按顺序回退）
        --checksum <ALGO=HEX>  期望校验，如 sha256=abc…
        --sha256 <HEX>         等价于 --checksum sha256=<HEX>
        --force-split          小文件也分段（插件包展示多线程）
        --overwrite            覆盖同名文件
        --status-file <PATH>   写进度 JSON
        --control-file <PATH>  轮询控制文件
        --proxy <URL>          代理（http:// / socks5://）
        --ua <UA>              User-Agent
        --stall-secs <N>       停滞看门狗（默认 30）
        --max-retries <N>      单段重试次数（默认 3）
        --max-connections <N>  自适应上限（默认 8）
        --no-slow-guard        关闭慢速保护
        --no-keep-partial      失败时删除半成品
        --insecure             忽略证书错误（调试）
        --json                 以 JSON 行输出进度
    -q, --quiet                只在结束时输出路径
    -h, --help                 显示帮助
";

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || args.iter().any(|arg| arg == "-h" || arg == "--help") {
        print!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    let settings = Settings::load();
    ufetch_core::logger::set_log_file(settings.cache_dir.join("ufetch.log"));
    ufetch_core::logger::init_from_env();
    match dispatch(&args, &settings).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(Error::Usage(message)) => {
            eprintln!("error: {message}\n\n{USAGE}");
            ExitCode::from(2)
        }
        Err(Error::Download(DownloadError::Cancelled)) => ExitCode::from(3),
        Err(Error::Download(error)) => {
            eprintln!("error: {}", error.summary());
            ExitCode::FAILURE
        }
        Err(Error::Install(message)) => {
            eprintln!("error: {message}");
            ExitCode::FAILURE
        }
    }
}

enum Error {
    Usage(String),
    Download(DownloadError),
    Install(String),
}

impl From<DownloadError> for Error {
    fn from(error: DownloadError) -> Self {
        Self::Download(error)
    }
}

async fn dispatch(args: &[String], settings: &Settings) -> Result<(), Error> {
    match args[0].as_str() {
        "list" => cmd_list(&args[1..], settings),
        "status" => cmd_status(&args[1..], settings),
        "start" | "pause" | "resume" | "cancel" => cmd_control(&args[0], &args[1..], settings),
        "delete" => cmd_delete(&args[1..], settings, false).await,
        "install" => cmd_install(&args[1..], settings),
        "install-ok" => cmd_delete(&args[1..], settings, true).await,
        "cache" => cmd_cache(&args[1..], settings),
        "config" => cmd_config(&args[1..], settings),
        "log" => cmd_log(&args[1..]),
        _ => run_download(args, settings).await,
    }
}

fn registry(settings: &Settings) -> TaskRegistry {
    TaskRegistry::new(&settings.cache_dir)
}

fn cmd_list(args: &[String], settings: &Settings) -> Result<(), Error> {
    let records = registry(settings).list();
    if has_flag(args, "--json") {
        println!(
            "{}",
            serde_json::to_string_pretty(&records).unwrap_or_else(|_| "[]".into())
        );
        return Ok(());
    }
    if records.is_empty() {
        println!("no tasks");
        return Ok(());
    }
    println!(
        "{:<22} {:<10} {:>7} {:>10}  FILE",
        "ID", "STATE", "PERCENT", "SIZE"
    );
    for record in records {
        let size = if record.total > 0 {
            human_bytes(record.total)
        } else {
            human_bytes(record.downloaded)
        };
        println!(
            "{:<22} {:<10} {:>6.1}% {:>10}  {}",
            record.id,
            record.state,
            record.percent,
            size,
            record.file_name.as_deref().unwrap_or("-")
        );
    }
    Ok(())
}

fn cmd_status(args: &[String], settings: &Settings) -> Result<(), Error> {
    let id = args
        .first()
        .ok_or_else(|| Error::Usage("status: missing task ID".into()))?;
    let record = registry(settings)
        .get(id)
        .ok_or_else(|| Error::Usage(format!("unknown task `{id}`")))?;
    if has_flag(args, "--json") {
        println!(
            "{}",
            serde_json::to_string_pretty(&record).unwrap_or_default()
        );
    } else {
        println!("id:      {}", record.id);
        println!("state:   {}", record.state);
        println!("percent: {:.1}%", record.percent);
        println!(
            "size:    {}",
            human_bytes(record.total.max(record.downloaded))
        );
        println!("file:    {}", record.file_name.as_deref().unwrap_or("-"));
        if let Some(path) = record.artifact.as_ref() {
            println!("path:    {}", path.display());
        }
        if let Some(error) = record.error.as_ref() {
            println!("error:   {error}");
        }
    }
    Ok(())
}

fn cmd_control(command: &str, args: &[String], settings: &Settings) -> Result<(), Error> {
    let action = if command == "start" {
        "resume"
    } else {
        command
    };
    let explicit = option_value(args, "--control-file").map(PathBuf::from);
    let path = match explicit {
        Some(path) => path,
        None => {
            let id = args
                .first()
                .ok_or_else(|| Error::Usage(format!("{command}: missing task ID")))?;
            registry(settings)
                .get(id)
                .and_then(|record| record.control_file)
                .ok_or_else(|| Error::Usage(format!("task `{id}` has no control file")))?
        }
    };
    write_control(&path, action).map_err(|error| Error::Usage(error.to_string()))?;
    println!("{action} -> {}", path.display());
    Ok(())
}

async fn cmd_delete(
    args: &[String],
    settings: &Settings,
    require_completed: bool,
) -> Result<(), Error> {
    let id = args
        .first()
        .ok_or_else(|| Error::Usage("delete: missing task ID".into()))?;
    let keep_file = has_flag(args, "--keep-file");
    let tasks = registry(settings);
    let mut record = tasks
        .get(id)
        .ok_or_else(|| Error::Usage(format!("unknown task `{id}`")))?;
    if require_completed && record.state != "completed" {
        return Err(Error::Usage(format!(
            "task `{id}` is `{}`, not completed",
            record.state
        )));
    }
    if require_completed && !settings.auto_remove {
        println!("auto_remove=false, task `{id}` kept");
        return Ok(());
    }
    if !require_completed && !is_terminal(&record.state) {
        if let Some(control) = record.control_file.as_ref() {
            write_control(control, "cancel").map_err(|error| Error::Usage(error.to_string()))?;
        }
        for _ in 0..40 {
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            record = tasks
                .get(id)
                .ok_or_else(|| Error::Usage(format!("task `{id}` disappeared")))?;
            if is_terminal(&record.state) {
                break;
            }
        }
        if !is_terminal(&record.state) {
            return Err(Error::Usage(format!(
                "task `{id}` is still `{}`; cancel it first",
                record.state
            )));
        }
    }

    purge_task(settings, &record, keep_file)?;
    println!("task `{id}` removed");
    Ok(())
}

fn purge_task(settings: &Settings, record: &TaskRecord, keep_file: bool) -> Result<(), Error> {
    let cache = CacheStore::new(
        settings.cache_dir.clone(),
        settings.cache_capacity_mb * 1024 * 1024,
        settings.cache_max_items,
    );
    cache.remove(&record.id, !keep_file);
    if !keep_file {
        remove_task_files(record);
    }
    registry(settings)
        .remove(&record.id)
        .map_err(Error::Usage)?;
    Ok(())
}

/// OpenWrt 24.10+ 用 apk，旧版本用 opkg。
#[derive(Copy, Clone, Debug)]
enum Installer {
    Apk,
    Opkg,
}

impl Installer {
    fn detect(path: &Path) -> Self {
        match path
            .extension()
            .and_then(|ext| ext.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("apk") => Self::Apk,
            Some("ipk") => Self::Opkg,
            _ if Self::Apk.tool().is_some() => Self::Apk,
            _ => Self::Opkg,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Apk => "apk",
            Self::Opkg => "opkg",
        }
    }

    fn env_key(self) -> &'static str {
        match self {
            Self::Apk => "UFETCH_APK",
            Self::Opkg => "UFETCH_OPKG",
        }
    }

    fn candidates(self) -> &'static [&'static str] {
        match self {
            Self::Apk => &["/usr/bin/apk", "/sbin/apk", "/bin/apk"],
            Self::Opkg => &["/usr/bin/opkg", "/bin/opkg", "/sbin/opkg"],
        }
    }

    fn tool(self) -> Option<PathBuf> {
        if let Some(path) = std::env::var_os(self.env_key()) {
            let path = PathBuf::from(path);
            if path.is_file() {
                return Some(path);
            }
        }
        for candidate in self.candidates() {
            let path = PathBuf::from(candidate);
            if path.is_file() {
                return Some(path);
            }
        }
        find_in_path(self.name())
    }

    fn command(self, tool: &Path, package: &Path) -> Command {
        let mut command = Command::new(tool);
        match self {
            Self::Apk => {
                command.arg("add").arg("--allow-untrusted").arg(package);
            }
            Self::Opkg => {
                command.arg("install").arg(package);
            }
        }
        command
    }
}

fn find_in_path(name: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&paths) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// 安装已下载的 `.apk` / `.ipk`；成功后按 `auto_remove` 清掉任务。
fn cmd_install(args: &[String], settings: &Settings) -> Result<(), Error> {
    let id = args
        .first()
        .ok_or_else(|| Error::Usage("install: missing task ID".into()))?;
    let keep_file = has_flag(args, "--keep-file");
    let record = registry(settings)
        .get(id)
        .ok_or_else(|| Error::Usage(format!("unknown task `{id}`")))?;
    if record.state != "completed" {
        return Err(Error::Usage(format!(
            "task `{id}` is `{}`, not completed",
            record.state
        )));
    }
    let package = record
        .artifact
        .clone()
        .or_else(|| {
            record
                .file_name
                .as_ref()
                .map(|name| settings.cache_dir.join(name))
        })
        .ok_or_else(|| Error::Install(format!("task `{id}` has no artifact")))?;
    if !package.is_file() {
        return Err(Error::Install(format!(
            "package not found: {}",
            package.display()
        )));
    }

    let installer = Installer::detect(&package);
    let tool = installer.tool().ok_or_else(|| {
        Error::Install(format!(
            "{} not found; is this an OpenWrt system?",
            installer.name()
        ))
    })?;
    println!(
        "installing {} with {} ...",
        package.display(),
        installer.name()
    );
    let mut command = installer.command(&tool, &package);
    let mut child = command
        .spawn()
        .map_err(|error| Error::Install(format!("spawn {}: {error}", installer.name())))?;
    let timeout = Duration::from_secs(settings.install_timeout_secs.max(1));
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() >= timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(Error::Install(format!(
                    "{} timed out after {}s",
                    installer.name(),
                    timeout.as_secs()
                )));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            Err(error) => {
                return Err(Error::Install(format!(
                    "wait for {}: {error}",
                    installer.name()
                )));
            }
        }
    };
    if !status.success() {
        return Err(Error::Install(format!(
            "{} failed with {}",
            installer.name(),
            status
                .code()
                .map(|code| code.to_string())
                .unwrap_or_else(|| "signal".into())
        )));
    }

    if settings.auto_remove && !keep_file {
        purge_task(settings, &record, false)?;
        println!("installed, task `{id}` removed");
    } else {
        println!("installed, task `{id}` kept (auto_remove/--keep-file)");
    }
    Ok(())
}

fn is_terminal(state: &str) -> bool {
    matches!(state, "completed" | "failed" | "cancelled")
}

fn remove_task_files(record: &ufetch_core::TaskRecord) {
    let dir = record
        .artifact
        .as_ref()
        .and_then(|path| path.parent())
        .or_else(|| record.status_file.as_ref().and_then(|path| path.parent()))
        .unwrap_or_else(|| std::path::Path::new("."))
        .to_path_buf();
    if let Some(path) = record.artifact.as_ref() {
        let _ = std::fs::remove_file(path);
    }
    if let Some(name) = record.file_name.as_ref() {
        let _ = std::fs::remove_file(dir.join(format!("{name}.ufetch")));
        let _ = std::fs::remove_file(dir.join(format!("{name}.ufetch.parts.json")));
    }
    if let Some(path) = record.status_file.as_ref() {
        let _ = std::fs::remove_file(path);
    }
    if let Some(path) = record.control_file.as_ref() {
        let _ = std::fs::remove_file(path);
    }
}

fn cmd_cache(args: &[String], settings: &Settings) -> Result<(), Error> {
    let store = CacheStore::new(
        settings.cache_dir.clone(),
        settings.cache_capacity_mb * 1024 * 1024,
        settings.cache_max_items,
    );
    match args.first().map(String::as_str) {
        Some("clear") => {
            let tasks = registry(settings);
            for entry in store.entries() {
                if let Some(record) = tasks.get(&entry.task_id) {
                    remove_task_files(&record);
                }
                let _ = tasks.remove(&entry.task_id);
            }
            store.clear();
            println!("cache cleared");
            Ok(())
        }
        _ => {
            let entries = store.entries();
            if has_flag(args, "--json") {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&entries).unwrap_or_else(|_| "[]".into())
                );
            } else if entries.is_empty() {
                println!("cache is empty");
            } else {
                println!("{:<22} {:>10}  PATH", "TASK", "SIZE");
                for entry in entries {
                    println!(
                        "{:<22} {:>10}  {}",
                        entry.task_id,
                        human_bytes(entry.bytes),
                        entry.path.display()
                    );
                }
                println!(
                    "\n{} / {} used",
                    human_bytes(store.used_bytes()),
                    human_bytes(store.capacity_bytes())
                );
            }
            Ok(())
        }
    }
}

fn cmd_config(args: &[String], settings: &Settings) -> Result<(), Error> {
    if args.first().map(String::as_str) == Some("set") {
        let mut updated = settings.clone();
        for pair in &args[1..] {
            let (key, value) = pair.split_once('=').ok_or_else(|| {
                Error::Usage(format!("config set: expected KEY=VALUE, got `{pair}`"))
            })?;
            updated.set(key, value).map_err(Error::Usage)?;
        }
        updated.save().map_err(Error::Usage)?;
        println!("settings saved to {}", Settings::default_path().display());
        return Ok(());
    }
    if has_flag(args, "--json") {
        println!(
            "{}",
            serde_json::to_string_pretty(settings).unwrap_or_default()
        );
    } else {
        println!("{}", toml::to_string_pretty(settings).unwrap_or_default());
    }
    Ok(())
}

fn cmd_log(args: &[String]) -> Result<(), Error> {
    let path = default_home().join("ufetch.log");
    let tail = option_value(args, "--tail")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(40);
    let text = std::fs::read_to_string(&path)
        .map_err(|error| Error::Usage(format!("cannot read {}: {error}", path.display())))?;
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(tail);
    for line in &lines[start..] {
        println!("{line}");
    }
    Ok(())
}

fn has_flag(args: &[String], flag: &str) -> bool {
    args.iter().any(|arg| arg == flag)
}

async fn run_download(args: &[String], settings: &Settings) -> Result<(), Error> {
    let mut config = settings.apply();
    let mut manager_config = ManagerConfig {
        task_dir: settings.cache_dir.clone(),
        auto_remove: settings.auto_remove,
        ..ManagerConfig::default()
    };
    let mut output_dir = settings.cache_dir.clone();
    let mut file_name = None;
    let mut checksum = None;
    let mut mirrors: Vec<String> = Vec::new();
    let mut status_file = None;
    let mut control_file = None;
    let mut force_split = false;
    let mut overwrite = false;
    let mut keep_partial = true;
    let mut json = false;
    let mut quiet = false;
    let mut url: Option<String> = None;

    let mut index = 0usize;
    while index < args.len() {
        let arg = args[index].as_str();
        let take_value = |index: &mut usize| -> Result<String, Error> {
            *index += 1;
            args.get(*index)
                .cloned()
                .ok_or_else(|| Error::Usage(format!("missing value for {arg}")))
        };
        match arg {
            "-o" | "--output-dir" => output_dir = PathBuf::from(take_value(&mut index)?),
            "-f" | "--file-name" => file_name = Some(take_value(&mut index)?),
            "-n" | "--connections" => {
                config.connections = parse_u32(&take_value(&mut index)?, arg)?;
            }
            "-l" | "--limit" => {
                config.speed_limit_bps = parse_rate(&take_value(&mut index)?)?;
            }
            "-m" | "--mirror" => mirrors.push(take_value(&mut index)?),
            "--checksum" => checksum = Some(take_value(&mut index)?),
            "--sha256" => checksum = Some(format!("sha256={}", take_value(&mut index)?)),
            "--force-split" => force_split = true,
            "--overwrite" => overwrite = true,
            "--status-file" => status_file = Some(PathBuf::from(take_value(&mut index)?)),
            "--control-file" => control_file = Some(PathBuf::from(take_value(&mut index)?)),
            "--proxy" => config.proxy = Some(take_value(&mut index)?),
            "--ua" => config.user_agent = take_value(&mut index)?,
            "--stall-secs" => {
                config.stall_timeout =
                    Duration::from_secs(parse_u64(&take_value(&mut index)?, arg)?);
            }
            "--max-retries" => {
                config.max_retries = parse_u32(&take_value(&mut index)?, arg)?;
            }
            "--max-connections" => {
                config.max_connections = parse_u32(&take_value(&mut index)?, arg)?;
            }

            "--no-slow-guard" => config.slow_guard = None,
            "--no-keep-partial" => keep_partial = false,
            "--insecure" => config.allow_invalid_certs = true,
            "--json" => json = true,
            "-q" | "--quiet" => quiet = true,
            other if other.starts_with('-') => {
                return Err(Error::Usage(format!("unknown option {other}")));
            }
            other => {
                if url.is_some() {
                    return Err(Error::Usage(format!("unexpected argument {other}")));
                }
                url = Some(other.to_string());
            }
        }
        index += 1;
    }

    let url = url.ok_or_else(|| Error::Usage("missing URL".into()))?;
    manager_config.engine = config;

    let manager = DownloadManager::try_new(manager_config)?;
    let mut request = DownloadRequest::new(url, output_dir)
        .mirrors(mirrors)
        .force_split(force_split)
        .keep_partial(keep_partial)
        .overwrite(overwrite);
    if let Some(name) = file_name {
        request = request.file_name(name);
    }
    if let Some(checksum) = checksum {
        request = request.checksum(checksum);
    }
    if let Some(path) = status_file {
        request = request.status_file(path);
    }
    if let Some(path) = control_file {
        request = request.control_file(path);
    }

    let task = manager.download(request);
    let task_id = task.id().to_string();
    let mut progress = task.subscribe();
    let mut last_state = TaskState::Queued;
    let mut printed = false;

    loop {
        let snapshot: Progress = progress.borrow().clone();
        if !quiet && snapshot.state != last_state || !quiet && !printed {
            render(&snapshot, json);
            last_state = snapshot.state;
            printed = true;
        }
        if snapshot.state.is_terminal() {
            break;
        }
        if progress.changed().await.is_err() {
            break;
        }
    }

    // 等待真正的结果（进度通道可能先于 JoinHandle 结束）。
    match task.wait().await {
        Ok(outcome) => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "event": "done",
                        "task_id": task_id,
                        "path": outcome.path,
                        "file_name": outcome.file_name,
                        "bytes": outcome.total_bytes,
                        "resumed_bytes": outcome.resumed_bytes,
                        "connections": outcome.connections,
                        "checksum": outcome.checksum,
                        "elapsed_secs": outcome.elapsed.as_secs(),
                        "average_bps": outcome.average_bps,
                    })
                );
            } else if !json {
                if !quiet {
                    println!();
                }
                println!(
                    "saved {} ({}, {}, {})",
                    outcome.path.display(),
                    human_bytes(outcome.total_bytes),
                    outcome
                        .checksum
                        .as_deref()
                        .map(|checksum| format!("checksum {checksum}"))
                        .unwrap_or_else(|| "no checksum".into()),
                    human_bytes(outcome.average_bps) + "/s"
                );
            }
            if outcome.path.starts_with(&settings.cache_dir)
                && outcome.total_bytes <= settings.cache_capacity_mb * 1024 * 1024
            {
                let cache = CacheStore::new(
                    settings.cache_dir.clone(),
                    settings.cache_capacity_mb * 1024 * 1024,
                    settings.cache_max_items,
                );
                let entry = CacheEntry {
                    task_id: task_id.clone(),
                    file_name: outcome.file_name.clone(),
                    path: outcome.path.clone(),
                    bytes: outcome.total_bytes,
                    created_at: now_secs(),
                };
                match cache.register(entry) {
                    Ok(evicted) => {
                        let tasks = TaskRegistry::new(&settings.cache_dir);
                        for item in evicted {
                            let _ = tasks.remove(&item.task_id);
                        }
                    }
                    Err(error) => eprintln!("warning: cache index write failed: {error}"),
                }
            }
            Ok(())
        }
        Err(DownloadError::Cancelled) => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({ "event": "cancelled", "task_id": task_id })
                );
            }
            Err(DownloadError::Cancelled.into())
        }
        Err(error) => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({ "event": "failed", "task_id": task_id, "error": error.summary() })
                );
            }
            Err(error.into())
        }
    }
}

fn render(progress: &Progress, json: bool) {
    if json {
        if let Ok(text) = serde_json::to_string(progress) {
            println!("{text}");
        }
        return;
    }
    let percent = format!("{:5.1}%", progress.percent);
    let size = if progress.total > 0 {
        format!(
            "{} / {}",
            human_bytes(progress.downloaded),
            human_bytes(progress.total)
        )
    } else {
        human_bytes(progress.downloaded)
    };
    let speed = format!("{:>10}/s", human_bytes(progress.speed_bps));
    let eta = progress
        .eta_secs()
        .map(|secs| format!("ETA {secs}s"))
        .unwrap_or_else(|| "ETA --".into());
    let conns = format!(
        "{}/{} conn ({} seg)",
        progress.active_connections, progress.connections, progress.segments_total
    );
    print!(
        "\r{percent} {size} {speed} {eta} {conns} {}        ",
        progress.state
    );
    let _ = std::io::stdout().flush();
}

fn option_value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    args.iter()
        .position(|arg| arg == flag)
        .and_then(|index| args.get(index + 1))
        .map(String::as_str)
}

fn parse_u32(value: &str, flag: &str) -> Result<u32, Error> {
    value
        .parse::<u32>()
        .map_err(|_| Error::Usage(format!("{flag}: invalid number '{value}'")))
}

fn parse_u64(value: &str, flag: &str) -> Result<u64, Error> {
    value
        .parse::<u64>()
        .map_err(|_| Error::Usage(format!("{flag}: invalid number '{value}'")))
}

/// 解析 `512K` / `2M` / `1048576` 这类限速写法（按 1024 进制）。
fn parse_rate(value: &str) -> Result<u64, Error> {
    let trimmed = value.trim();
    let (number, multiplier) = match trimmed.chars().last() {
        Some('k' | 'K') => (&trimmed[..trimmed.len() - 1], 1024u64),
        Some('m' | 'M') => (&trimmed[..trimmed.len() - 1], 1024 * 1024),
        Some('g' | 'G') => (&trimmed[..trimmed.len() - 1], 1024 * 1024 * 1024),
        _ => (trimmed, 1),
    };
    let number: f64 = number
        .parse()
        .map_err(|_| Error::Usage(format!("--limit: invalid rate '{value}'")))?;
    if number < 0.0 {
        return Err(Error::Usage(format!("--limit: negative rate '{value}'")));
    }
    Ok((number * multiplier as f64) as u64)
}
