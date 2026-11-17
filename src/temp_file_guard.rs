// SPDX-License-Identifier: LGPL-3.0-or-later
// Copyright (c) 2026 UFetch-Core contributors

//! 进程内临时文件租约：同一个目标文件不允许两个任务同时写。
//!
//! 只做进程内互斥（跨进程靠调用方约定目标目录），用规范化的绝对路径做键；
//! 守卫析构时自动释放。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use crate::error::DownloadError;

fn registry() -> &'static Mutex<HashMap<PathBuf, String>> {
    static REGISTRY: OnceLock<Mutex<HashMap<PathBuf, String>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 临时文件的占用凭据；`Drop` 时释放。
#[derive(Debug)]
pub struct TempFileGuard {
    key: Option<PathBuf>,
}

impl TempFileGuard {
    /// 尝试占用 `path`。已被其它任务占用时返回错误（不会被抢占）。
    ///
    /// # Errors
    ///
    /// 目标已被另一个任务占用。
    pub async fn acquire(path: &Path, task_id: &str) -> Result<Self, DownloadError> {
        let key = normalize(path);
        let mut registry = registry()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(owner) = registry.get(&key) {
            return Err(DownloadError::Other(format!(
                "'{}' is already being downloaded by task {owner}",
                key.display()
            )));
        }
        registry.insert(key.clone(), task_id.to_string());
        Ok(Self { key: Some(key) })
    }
}

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        if let Some(key) = self.key.take() {
            let mut registry = registry()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            registry.remove(&key);
        }
    }
}

/// 尽量把路径变成绝对路径：优先 canonicalize 父目录，再拼回文件名。
fn normalize(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .map(std::ffi::OsStr::to_os_string)
        .map(PathBuf::from);
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty());
    match (parent, file_name) {
        (Some(parent), Some(name)) => {
            let base = parent
                .canonicalize()
                .or_else(|_| std::path::absolute(parent))
                .unwrap_or_else(|_| parent.to_path_buf());
            base.join(name)
        }
        _ => std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("ufetch-guard-{tag}-{}.part", std::process::id()))
    }

    #[tokio::test]
    async fn second_acquisition_is_rejected_until_the_guard_drops() {
        let path = scratch("busy");
        let guard = TempFileGuard::acquire(&path, "task-a")
            .await
            .expect("first");
        let error = TempFileGuard::acquire(&path, "task-b")
            .await
            .expect_err("must be exclusive");
        assert!(error.to_string().contains("task-a"), "{error}");
        drop(guard);
        TempFileGuard::acquire(&path, "task-b")
            .await
            .expect("released");
    }

    #[tokio::test]
    async fn different_paths_do_not_conflict() {
        let first = scratch("one");
        let second = scratch("two");
        let _first_guard = TempFileGuard::acquire(&first, "task-a").await.expect("a");
        let _second_guard = TempFileGuard::acquire(&second, "task-b").await.expect("b");
    }
}
