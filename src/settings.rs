// SPDX-License-Identifier: LGPL-3.0-or-later
// Copyright (c) 2026 UFetch-Core contributors

//! 高级设置：`settings.toml`（TOML + Serde），默认放在 `UFETCH_HOME`。
//!
//! CLI 的 `ufetch config` / `ufetch config set key=value` 直接读写这个文件；
//! 库调用方可以 `Settings::load()` 后 `apply()` 到 `EngineConfig`。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::engine::EngineConfig;
use crate::fsutil::{default_home, write_atomic};

/// 所有可调项。未知字段会被 Serde 忽略，方便后续扩展。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// 缓存目录；默认 `<tmp>/ufetch`（OpenWrt 上是 tmpfs）。
    pub cache_dir: PathBuf,
    /// 缓存总字节上限（MiB）。
    pub cache_capacity_mb: u64,
    /// 缓存条目上限。
    pub cache_max_items: usize,
    /// 同时下载的任务数。
    pub max_concurrent: usize,
    /// 单任务连接数；`0` = 自适应。
    pub connections: u32,
    /// 自适应上限。
    pub max_connections: u32,
    /// 分块大小（MiB）。
    pub chunk_mb: u64,
    /// 全局限速，如 `512K` / `2M`；空串 = 不限。
    pub speed_limit: String,
    /// 停滞看门狗秒数。
    pub stall_secs: u64,
    /// 单段重试次数。
    pub max_retries: u32,
    /// 建连超时秒数。
    pub connect_timeout_secs: u64,
    /// 任务级慢速保护。
    pub slow_guard: bool,
    /// 安装成功后自动删除任务与产物。
    pub auto_remove: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            cache_dir: default_home(),
            cache_capacity_mb: 64,
            cache_max_items: 16,
            max_concurrent: 3,
            connections: 0,
            max_connections: 8,
            chunk_mb: 4,
            speed_limit: String::new(),
            stall_secs: 30,
            max_retries: 3,
            connect_timeout_secs: 20,
            slow_guard: true,
            auto_remove: true,
        }
    }
}

impl Settings {
    /// 默认配置文件：`<UFETCH_HOME>/settings.toml`。
    #[must_use]
    pub fn default_path() -> PathBuf {
        default_home().join("settings.toml")
    }

    /// 从指定路径读取；文件不存在时返回默认值。
    #[must_use]
    pub fn load_from(path: &Path) -> Self {
        let Ok(text) = std::fs::read_to_string(path) else {
            return Self::default();
        };
        toml::from_str(&text).unwrap_or_default()
    }

    /// 读取默认路径。
    #[must_use]
    pub fn load() -> Self {
        Self::load_from(&Self::default_path())
    }

    /// 原子写回。
    ///
    /// # Errors
    ///
    /// 序列化或磁盘写入失败。
    pub fn save_to(&self, path: &Path) -> Result<(), String> {
        let text = toml::to_string_pretty(self).map_err(|error| error.to_string())?;
        write_atomic(path, text.as_bytes()).map_err(|error| error.to_string())
    }

    /// 写回默认路径。
    ///
    /// # Errors
    ///
    /// 同上。
    pub fn save(&self) -> Result<(), String> {
        self.save_to(&Self::default_path())
    }

    /// 转成引擎配置。
    #[must_use]
    pub fn apply(&self) -> EngineConfig {
        let mut config = EngineConfig::openwrt();
        config.connections = self.connections;
        config.max_connections = self.max_connections;
        config.chunk_size = self.chunk_mb.max(1) * 1024 * 1024;
        config.min_split_piece = 64 * 1024;
        config.stall_timeout = std::time::Duration::from_secs(self.stall_secs.max(1));
        config.connect_timeout = std::time::Duration::from_secs(self.connect_timeout_secs.max(1));
        config.max_retries = self.max_retries;
        if self.speed_limit.trim().is_empty() {
            config.speed_limit_bps = 0;
        } else {
            config.speed_limit_bps = parse_rate(&self.speed_limit).unwrap_or(0);
        }
        if !self.slow_guard {
            config.slow_guard = None;
        }
        config
    }

    /// 按 `key=value` 修改一个字段。
    ///
    /// # Errors
    ///
    /// 未知键或值格式错误。
    pub fn set(&mut self, key: &str, value: &str) -> Result<(), String> {
        match key {
            "cache_dir" => self.cache_dir = PathBuf::from(value),
            "cache_capacity_mb" => self.cache_capacity_mb = parse_u64(value)?,
            "cache_max_items" => self.cache_max_items = parse_u64(value)? as usize,
            "max_concurrent" => self.max_concurrent = parse_u64(value)? as usize,
            "connections" => self.connections = parse_u64(value)? as u32,
            "max_connections" => self.max_connections = parse_u64(value)? as u32,
            "chunk_mb" => self.chunk_mb = parse_u64(value)?,
            "speed_limit" => {
                if !value.trim().is_empty() {
                    parse_rate(value)?;
                }
                self.speed_limit = value.to_string();
            }
            "stall_secs" => self.stall_secs = parse_u64(value)?,
            "max_retries" => self.max_retries = parse_u64(value)? as u32,
            "connect_timeout_secs" => self.connect_timeout_secs = parse_u64(value)?,
            "slow_guard" => self.slow_guard = parse_bool(value)?,
            "auto_remove" => self.auto_remove = parse_bool(value)?,
            other => return Err(format!("unknown setting `{other}`")),
        }
        Ok(())
    }
}

fn parse_u64(value: &str) -> Result<u64, String> {
    value
        .trim()
        .parse::<u64>()
        .map_err(|_| format!("expected an integer, got `{value}`"))
}

fn parse_bool(value: &str) -> Result<bool, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => Err(format!("expected true/false, got `{value}`")),
    }
}

/// 解析 `512K` / `2M` / `1024` 为 B/s。
///
/// # Errors
///
/// 数字或单位非法。
pub fn parse_rate(value: &str) -> Result<u64, String> {
    let text = value.trim().trim_end_matches(['/', 's']);
    let (number, factor) = match text.chars().last() {
        Some('K' | 'k') => (&text[..text.len() - 1], 1024),
        Some('M' | 'm') => (&text[..text.len() - 1], 1024 * 1024),
        Some('G' | 'g') => (&text[..text.len() - 1], 1024 * 1024 * 1024),
        _ => (text, 1),
    };
    let base = number
        .trim()
        .parse::<f64>()
        .map_err(|_| format!("invalid rate `{value}`"))?;
    if base <= 0.0 {
        return Err(format!("rate must be positive, got `{value}`"));
    }
    Ok((base * factor as f64) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sane() {
        let settings = Settings::default();
        assert_eq!(settings.cache_capacity_mb, 64);
        assert_eq!(settings.max_connections, 8);
        assert!(settings.auto_remove);
    }

    #[test]
    fn toml_roundtrip_and_apply() {
        let dir =
            std::env::temp_dir().join(format!("ufetch-settings-{}", crate::registry::now_secs()));
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("settings.toml");
        let mut settings = Settings::default();
        settings.set("connections", "6").expect("connections");
        settings.set("speed_limit", "512K").expect("limit");
        settings.set("auto_remove", "false").expect("auto");
        settings.save_to(&path).expect("save");

        let loaded = Settings::load_from(&path);
        assert_eq!(loaded.connections, 6);
        assert_eq!(loaded.speed_limit, "512K");
        assert!(!loaded.auto_remove);

        let engine = loaded.apply();
        assert_eq!(engine.connections, 6);
        assert_eq!(engine.speed_limit_bps, 512 * 1024);
    }

    #[test]
    fn rejects_unknown_keys_and_bad_values() {
        let mut settings = Settings::default();
        assert!(settings.set("nope", "1").is_err());
        assert!(settings.set("connections", "abc").is_err());
        assert!(settings.set("auto_remove", "maybe").is_err());
    }
}
