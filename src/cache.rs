// SPDX-License-Identifier: LGPL-3.0-or-later
// Copyright (c) 2026 UFetch-Core contributors

//! 内存环形缓存：完成的任务按“总字节 + 条目数”双上限淘汰最旧的一条。
//!
//! OpenWrt 上缓存目录默认是 `/tmp/ufetch`（tmpfs，本身就在内存里）；
//! 在普通机器上则是临时目录。内核只负责登记、淘汰和删除，不假设介质。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::fsutil::write_atomic;

/// 一条缓存产物。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CacheEntry {
    pub task_id: String,
    pub file_name: String,
    pub path: PathBuf,
    pub bytes: u64,
    pub created_at: u64,
}

/// 环形缓存索引。每次登记都会重新读盘并立即执行淘汰。
#[derive(Clone, Debug)]
pub struct CacheStore {
    dir: PathBuf,
    index: PathBuf,
    capacity_bytes: u64,
    max_items: usize,
}

impl CacheStore {
    /// 绑定缓存目录与上限。
    #[must_use]
    pub fn new(dir: PathBuf, capacity_bytes: u64, max_items: usize) -> Self {
        Self {
            index: dir.join("cache.json"),
            dir,
            capacity_bytes: capacity_bytes.max(1),
            max_items: max_items.max(1),
        }
    }

    /// 缓存目录。
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// 读取索引；缺失或损坏时返回空。
    #[must_use]
    pub fn entries(&self) -> Vec<CacheEntry> {
        let Ok(text) = std::fs::read_to_string(&self.index) else {
            return Vec::new();
        };
        serde_json::from_str::<Vec<CacheEntry>>(&text).unwrap_or_default()
    }

    /// 登记产物并按环形规则淘汰，返回被淘汰的条目（文件已删除）。
    ///
    /// # Errors
    ///
    /// 索引不可写。
    pub fn register(&self, entry: CacheEntry) -> Result<Vec<CacheEntry>, String> {
        let mut entries = self.entries();
        entries.retain(|item| item.task_id != entry.task_id);
        entries.push(entry);
        entries.sort_by_key(|item| item.created_at);

        let mut evicted = Vec::new();
        while entries.len() > self.max_items || total_bytes(&entries) > self.capacity_bytes {
            let oldest = entries.remove(0);
            let _ = std::fs::remove_file(&oldest.path);
            evicted.push(oldest);
        }
        let text = serde_json::to_vec_pretty(&entries).map_err(|error| error.to_string())?;
        write_atomic(&self.index, &text).map_err(|error| error.to_string())?;
        Ok(evicted)
    }

    /// 按任务 ID 删除产物与索引项；`delete_file=false` 只摘索引。
    pub fn remove(&self, task_id: &str, delete_file: bool) {
        let mut entries = self.entries();
        if let Some(index) = entries.iter().position(|item| item.task_id == task_id) {
            let entry = entries.remove(index);
            if delete_file {
                let _ = std::fs::remove_file(&entry.path);
            }
            if let Ok(text) = serde_json::to_vec_pretty(&entries) {
                let _ = write_atomic(&self.index, &text);
            }
        }
    }

    /// 清空全部缓存产物。
    pub fn clear(&self) {
        for entry in self.entries() {
            let _ = std::fs::remove_file(&entry.path);
        }
        let _ = write_atomic(&self.index, b"[]");
    }

    /// 已用字节。
    #[must_use]
    pub fn used_bytes(&self) -> u64 {
        total_bytes(&self.entries())
    }

    /// 上限（字节）。
    #[must_use]
    pub fn capacity_bytes(&self) -> u64 {
        self.capacity_bytes
    }
}

fn total_bytes(entries: &[CacheEntry]) -> u64 {
    entries.iter().map(|entry| entry.bytes).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ufetch-cache-{name}-{}",
            crate::registry::now_secs()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        dir
    }

    fn entry(name: &str, bytes: u64, created_at: u64) -> CacheEntry {
        CacheEntry {
            task_id: name.to_string(),
            file_name: format!("{name}.apk"),
            path: PathBuf::from(format!("/tmp/{name}.apk")),
            bytes,
            created_at,
        }
    }

    #[test]
    fn evicts_oldest_when_item_limit_is_hit() {
        let dir = temp_dir("items");
        let store = CacheStore::new(dir, 1024 * 1024, 2);
        store.register(entry("a", 1, 1)).expect("a");
        store.register(entry("b", 1, 2)).expect("b");
        let evicted = store.register(entry("c", 1, 3)).expect("c");
        assert_eq!(evicted.len(), 1);
        assert_eq!(evicted[0].task_id, "a");
        assert_eq!(store.entries().len(), 2);
    }

    #[test]
    fn evicts_by_total_bytes() {
        let dir = temp_dir("bytes");
        let store = CacheStore::new(dir, 100, 16);
        store.register(entry("a", 60, 1)).expect("a");
        let evicted = store.register(entry("b", 60, 2)).expect("b");
        assert_eq!(evicted.len(), 1);
        assert_eq!(evicted[0].task_id, "a");
        assert_eq!(store.used_bytes(), 60);
    }

    #[test]
    fn replacing_same_task_does_not_grow() {
        let dir = temp_dir("replace");
        let store = CacheStore::new(dir, 1024, 4);
        store.register(entry("a", 10, 1)).expect("a");
        store.register(entry("a", 20, 2)).expect("a");
        assert_eq!(store.entries().len(), 1);
        assert_eq!(store.entries()[0].bytes, 20);
    }
}
