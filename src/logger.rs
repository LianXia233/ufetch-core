// SPDX-License-Identifier: LGPL-3.0-or-later
// Copyright (c) 2026 UFetch-Core contributors

//! 极简日志门面。
//!
//! 原 `ufetch_core` 的 `logger` 是「按日期分卷、自动清理、tracing 桥接」的
//! 完整文件日志系统，属于宿主管家能力（依赖数据目录、tracing、panic hook），
//! 不属于下载内核。这里只保留内核真正用到的 `log_info!` / `log_warn!` /
//! `log_error!` 三个宏：默认写 stderr，级别由 `UFETCH_LOG` 环境变量
//! （`off|error|warn|info|debug`）控制，不落盘、不建目录。

use std::fmt::Arguments;
use std::io::Write;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU8, Ordering};

/// 日志级别。数值越大越详细。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum Level {
    Off = 0,
    Error = 1,
    Warn = 2,
    Info = 3,
    Debug = 4,
}

static LEVEL: AtomicU8 = AtomicU8::new(Level::Info as u8);
/// 日志文件上限：512 KiB，超出后只保留最新的这一段。
const LOG_LIMIT: u64 = 512 * 1024;
static LOG_FILE: OnceLock<PathBuf> = OnceLock::new();

/// 设置全局日志级别。
pub fn set_level(level: Level) {
    LEVEL.store(level as u8, Ordering::Relaxed);
}

/// 当前日志级别。
pub fn level() -> Level {
    match LEVEL.load(Ordering::Relaxed) {
        0 => Level::Off,
        1 => Level::Error,
        2 => Level::Warn,
        3 => Level::Info,
        _ => Level::Debug,
    }
}

/// 按 `UFETCH_LOG` 初始化级别（未设置时用 `info`）。
pub fn init_from_env() {
    let level = match std::env::var("UFETCH_LOG")
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "off" => Level::Off,
        "error" => Level::Error,
        "warn" => Level::Warn,
        "" => Level::Info,
        "info" => Level::Info,
        "debug" | "trace" => Level::Debug,
        _ => Level::Warn,
    };
    set_level(level);
    if level == Level::Off {
        return;
    }
    let path = std::env::var("UFETCH_LOG_FILE")
        .ok()
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::fsutil::default_home().join("ufetch.log"));
    set_log_file(path);
}

/// 指定日志文件（默认 `<UFETCH_HOME>/ufetch.log`）。
pub fn set_log_file(path: PathBuf) {
    let _ = LOG_FILE.set(path);
}

/// 当前日志文件。
#[must_use]
pub fn log_file() -> Option<&'static PathBuf> {
    LOG_FILE.get()
}

/// 日志输出入口。级别不够时零开销返回。
pub fn log_line(level: Level, args: Arguments<'_>) {
    if level > self::level() || self::level() == Level::Off {
        return;
    }
    let tag = match level {
        Level::Error => "ERROR",
        Level::Warn => "WARN ",
        Level::Info => "INFO ",
        Level::Debug => "DEBUG",
        Level::Off => return,
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0);
    let line = format!("[{now}] [{tag}] {args}");
    eprintln!("{line}");
    append_file(&line);
}

/// 追加到内存缓存里的日志文件；超过 512 KiB 时截掉旧内容。
fn append_file(line: &str) {
    let Some(path) = LOG_FILE.get() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    else {
        return;
    };
    let _ = writeln!(file, "{line}");
    drop(file);
    if std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0) > LOG_LIMIT {
        trim_log(path);
    }
}

/// 只保留最后 512 KiB，形成环形滚动。
fn trim_log(path: &std::path::Path) {
    let Ok(bytes) = std::fs::read(path) else {
        return;
    };
    if bytes.len() as u64 <= LOG_LIMIT {
        return;
    }
    let keep = bytes[bytes.len() - LOG_LIMIT as usize..].to_vec();
    let _ = std::fs::write(path, keep);
}

#[macro_export]
macro_rules! log_info {
    ($($arg:tt)*) => {
        $crate::logger::log_line($crate::logger::Level::Info, format_args!($($arg)*))
    };
}

#[macro_export]
macro_rules! log_warn {
    ($($arg:tt)*) => {
        $crate::logger::log_line($crate::logger::Level::Warn, format_args!($($arg)*))
    };
}

#[macro_export]
macro_rules! log_error {
    ($($arg:tt)*) => {
        $crate::logger::log_line($crate::logger::Level::Error, format_args!($($arg)*))
    };
}

#[macro_export]
macro_rules! log_debug {
    ($($arg:tt)*) => {
        $crate::logger::log_line($crate::logger::Level::Debug, format_args!($($arg)*))
    };
}

#[allow(unused_imports)]
pub(crate) use crate::{log_debug, log_error, log_info, log_warn};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trim_keeps_only_the_newest_window() {
        let dir = std::env::temp_dir().join(format!("ufetch-log-{}", crate::registry::now_secs()));
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("ufetch.log");
        let mut text = String::new();
        for index in 0..20_000 {
            text.push_str(&format!("line {index} ....................\n"));
        }
        std::fs::write(&path, &text).expect("write");
        assert!(std::fs::metadata(&path).expect("meta").len() > LOG_LIMIT);
        trim_log(&path);
        let bytes = std::fs::read(&path).expect("read");
        assert!(bytes.len() as u64 <= LOG_LIMIT);
        let tail = String::from_utf8_lossy(&bytes);
        assert!(tail.contains("line 19999"));
    }
}
