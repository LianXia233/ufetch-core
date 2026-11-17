// SPDX-License-Identifier: LGPL-3.0-or-later
// Copyright (c) 2026 UFetch-Core contributors

//! `ufetch` —— 面向 OpenWrt 插件的最小下载 CLI。
//!
//! ```text
//! ufetch [OPTIONS] <URL>
//! ufetch pause|resume|cancel --control-file <PATH>
//! ```

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use ufetch_core::{
    DownloadError, DownloadManager, DownloadRequest, EngineConfig, ManagerConfig, Progress,
    TaskState, human_bytes, write_control,
};

const USAGE: &str = "\
ufetch — HTTP 下载内核 CLI

USAGE:
    ufetch [OPTIONS] <URL>
    ufetch pause|resume|cancel --control-file <PATH>

OPTIONS:
    -o, --output-dir <DIR>     保存目录（默认当前目录）
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

    // 子命令：pause / resume / cancel
    if let Some(command) = ["pause", "resume", "cancel"]
        .iter()
        .find(|cmd| args[0] == **cmd)
    {
        let Some(path) = option_value(&args, "--control-file") else {
            eprintln!("error: --control-file is required for `{command}`");
            return ExitCode::from(2);
        };
        return match write_control(PathBuf::from(path).as_path(), command) {
            Ok(()) => {
                println!("{command} written to {path}");
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("error: {error}");
                ExitCode::FAILURE
            }
        };
    }

    match run_download(&args).await {
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
    }
}

enum Error {
    Usage(String),
    Download(DownloadError),
}

impl From<DownloadError> for Error {
    fn from(error: DownloadError) -> Self {
        Self::Download(error)
    }
}

async fn run_download(args: &[String]) -> Result<(), Error> {
    let mut config = EngineConfig::openwrt();
    let mut manager_config = ManagerConfig::default();
    let mut output_dir = PathBuf::from(".");
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
    manager_config.engine = config.clone();

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
            Ok(())
        }
        Err(DownloadError::Cancelled) => {
            if json {
                println!("{}", serde_json::json!({ "event": "cancelled" }));
            }
            Err(DownloadError::Cancelled.into())
        }
        Err(error) => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({ "event": "failed", "error": error.summary() })
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
