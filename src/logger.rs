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

static LEVEL: AtomicU8 = AtomicU8::new(Level::Warn as u8);

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

/// 按 `UFETCH_LOG` 初始化级别（未设置时保持 `warn`）。
pub fn init_from_env() {
    let Ok(raw) = std::env::var("UFETCH_LOG") else {
        return;
    };
    let level = match raw.to_ascii_lowercase().as_str() {
        "off" => Level::Off,
        "error" => Level::Error,
        "warn" => Level::Warn,
        "info" => Level::Info,
        "debug" | "trace" => Level::Debug,
        _ => Level::Warn,
    };
    set_level(level);
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
    eprintln!("[{tag}] {args}");
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
