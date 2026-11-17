// SPDX-License-Identifier: LGPL-3.0-or-later
// Copyright (c) 2026 UFetch-Core contributors

//! 保存目录准备。

use std::io;
use std::path::Path;

/// 确保目录存在（含多级父目录）。
pub(crate) async fn ensure_dir(dir: &Path) -> io::Result<()> {
    if dir.as_os_str().is_empty() {
        return Ok(());
    }
    tokio::fs::create_dir_all(dir).await.map_err(|source| {
        io::Error::new(
            source.kind(),
            format!("create output directory '{}': {source}", dir.display()),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::ensure_dir;

    fn scratch(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("ufetch-out-{tag}-{}", std::process::id()))
    }

    #[tokio::test]
    async fn creates_nested_directories_and_is_idempotent() {
        let root = scratch("nested");
        let dir = root.join("a").join("b");
        let _ = std::fs::remove_dir_all(&root);
        ensure_dir(&dir).await.expect("create");
        ensure_dir(&dir).await.expect("second call is a no-op");
        assert!(dir.is_dir());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn empty_path_is_ignored() {
        ensure_dir(std::path::Path::new("")).await.expect("no-op");
    }
}
