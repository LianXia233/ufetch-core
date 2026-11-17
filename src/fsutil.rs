// SPDX-License-Identifier: LGPL-3.0-or-later
// Copyright (c) 2026 UFetch-Core contributors

//! 小文件原子写与目录创建，供任务表、缓存索引、设置共用。

use std::io;
use std::path::Path;

/// 同步原子写：`<path>.tmp` 写完 rename，读侧永远看到完整 JSON。
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    let temp = path.with_extension(format!(
        "{}.tmp",
        path.extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or("json")
    ));
    std::fs::write(&temp, bytes)?;
    std::fs::rename(&temp, path)
}

/// 默认工作目录：`UFETCH_HOME`，否则 `<tmp>/ufetch`。
#[must_use]
pub fn default_home() -> std::path::PathBuf {
    if let Ok(home) = std::env::var("UFETCH_HOME")
        && !home.is_empty()
    {
        return std::path::PathBuf::from(home);
    }
    std::env::temp_dir().join("ufetch")
}
