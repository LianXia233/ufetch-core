// SPDX-License-Identifier: LGPL-3.0-or-later
// Copyright (c) 2026 UFetch-Core contributors

//! 任务表：跨进程可读的任务清单，落在 `<home>/tasks.json`。
//!
//! CLI / LuCI 用 `ufetch list` 读它；下载进程在启动、进度 tick 和终态时更新。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::fsutil::write_atomic;

/// 一条任务记录。字段保持扁平，便于 LuCI 直接展示。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TaskRecord {
    pub id: String,
    pub url: String,
    #[serde(default)]
    pub file_name: Option<String>,
    pub state: String,
    #[serde(default)]
    pub downloaded: u64,
    #[serde(default)]
    pub total: u64,
    #[serde(default)]
    pub percent: f64,
    #[serde(default)]
    pub speed_bps: u64,
    #[serde(default)]
    pub connections: u32,
    #[serde(default)]
    pub active_connections: u32,
    #[serde(default)]
    pub created_at: u64,
    #[serde(default)]
    pub updated_at: u64,
    #[serde(default)]
    pub status_file: Option<PathBuf>,
    #[serde(default)]
    pub control_file: Option<PathBuf>,
    /// 完成后产物的路径（缓存目录内）。
    #[serde(default)]
    pub artifact: Option<PathBuf>,
    #[serde(default)]
    pub error: Option<String>,
    /// 安装成功后是否自动移除任务；插件默认开启。
    #[serde(default = "default_true")]
    pub auto_remove: bool,
}

fn default_true() -> bool {
    true
}

impl TaskRecord {
    /// 设置产物路径。
    #[must_use]
    pub fn with_artifact(mut self, path: PathBuf) -> Self {
        self.artifact = Some(path);
        self
    }

    /// 是否已经结束（成功 / 失败 / 取消）。
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        matches!(self.state.as_str(), "completed" | "failed" | "cancelled")
    }
}

/// 任务表文件。每次操作都重新读盘，避免多进程互相覆盖内存副本。
#[derive(Clone, Debug)]
pub struct TaskRegistry {
    path: PathBuf,
}

impl TaskRegistry {
    /// 绑定到 `<home>/tasks.json`。
    #[must_use]
    pub fn new(home: &Path) -> Self {
        Self {
            path: home.join("tasks.json"),
        }
    }

    /// 任务表路径。
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 读取全部记录；文件缺失或损坏时返回空表。
    #[must_use]
    pub fn load(&self) -> Vec<TaskRecord> {
        let Ok(text) = std::fs::read_to_string(&self.path) else {
            return Vec::new();
        };
        serde_json::from_str::<Vec<TaskRecord>>(&text).unwrap_or_default()
    }

    /// 覆盖写入。调用方负责去重。
    ///
    /// # Errors
    ///
    /// 磁盘不可写或序列化失败。
    pub fn save(&self, records: &[TaskRecord]) -> Result<(), String> {
        let text = serde_json::to_vec_pretty(records).map_err(|error| error.to_string())?;
        write_atomic(&self.path, &text).map_err(|error| error.to_string())
    }

    /// 插入或替换同 ID 记录。
    ///
    /// # Errors
    ///
    /// 磁盘不可写。
    pub fn upsert(&self, record: &TaskRecord) -> Result<(), String> {
        let mut records = self.load();
        match records.iter_mut().find(|item| item.id == record.id) {
            Some(slot) => {
                let created_at = slot.created_at;
                *slot = record.clone();
                slot.created_at = created_at;
            }
            None => records.push(record.clone()),
        }
        self.save(&records)
    }

    /// 按 ID 取一条。
    #[must_use]
    pub fn get(&self, id: &str) -> Option<TaskRecord> {
        self.load().into_iter().find(|record| record.id == id)
    }

    /// 全部任务，最近更新的在前。
    #[must_use]
    pub fn list(&self) -> Vec<TaskRecord> {
        let mut records = self.load();
        records.sort_by_key(|record| std::cmp::Reverse(record.updated_at));
        records
    }

    /// 移除一条，返回被移除的记录。
    ///
    /// # Errors
    ///
    /// 磁盘不可写。
    pub fn remove(&self, id: &str) -> Result<Option<TaskRecord>, String> {
        let mut records = self.load();
        let Some(index) = records.iter().position(|record| record.id == id) else {
            return Ok(None);
        };
        let removed = records.remove(index);
        self.save(&records)?;
        Ok(Some(removed))
    }

    /// 清空任务表。
    ///
    /// # Errors
    ///
    /// 磁盘不可写。
    pub fn clear(&self) -> Result<(), String> {
        self.save(&[])
    }
}

/// 当前 Unix 秒。
#[must_use]
pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ufetch-registry-{name}-{}", now_secs()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        dir
    }

    fn record(id: &str) -> TaskRecord {
        TaskRecord {
            id: id.to_string(),
            url: "https://example.com/a.apk".into(),
            file_name: Some("a.apk".into()),
            state: "completed".into(),
            downloaded: 10,
            total: 10,
            percent: 100.0,
            speed_bps: 1,
            connections: 2,
            active_connections: 0,
            created_at: 1,
            updated_at: 2,
            status_file: None,
            control_file: None,
            artifact: Some(PathBuf::from("/tmp/a.apk")),
            error: None,
            auto_remove: true,
        }
    }

    #[test]
    fn upsert_get_list_remove() {
        let dir = temp_dir("crud");
        let registry = TaskRegistry::new(&dir);
        registry.upsert(&record("a")).expect("insert");
        registry.upsert(&record("b")).expect("insert");
        assert_eq!(registry.list().len(), 2);
        assert_eq!(registry.get("a").expect("a").state, "completed");

        let mut updated = record("a");
        updated.state = "running".into();
        updated.updated_at = 9;
        registry.upsert(&updated).expect("update");
        assert_eq!(registry.get("a").expect("a").state, "running");
        assert_eq!(registry.list()[0].id, "a");

        assert!(registry.remove("a").expect("remove").is_some());
        assert!(registry.get("a").is_none());
        assert_eq!(registry.list().len(), 1);
    }

    #[test]
    fn corrupt_file_reads_as_empty() {
        let dir = temp_dir("corrupt");
        let registry = TaskRegistry::new(&dir);
        std::fs::write(registry.path(), b"{not json").expect("write");
        assert!(registry.list().is_empty());
    }
}
