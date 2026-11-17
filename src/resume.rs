// SPDX-License-Identifier: LGPL-3.0-or-later
// Copyright (c) 2026 UFetch-Core contributors

//! 断点续传记录（`.parts.json` 侧车）。
//!
//! 逐段位置的 sidecar 续传：
//! 原版只记录「哪些固定分块已完成」，本内核为支持动态拆段，记录每段的
//! `[start, pos, end)`——`pos` 是下次续传的起点。校验规则保留了原版的三条
//! 铁律：
//!
//! 1. 数据文件必须存在且长度等于总大小（被删除 / 截断的文件绝不能沿用
//!    「已完成」映射）；
//! 2. `url` / `total` / 强 validator（ETag，退化到 Last-Modified）必须一致；
//! 3. 每段都必须落在 `[0, total]` 内，且互不重叠（重叠会造成重复计数或
//!    让续传跳过从未写入的字节）。
//!
//! 任一条不满足就整份丢弃，宁可重下也绝不产出错位文件。

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// 侧车版本。格式变化时递增，旧版本一律丢弃。
const PARTS_VERSION: u32 = 2;

/// 一段已规划的区间。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartSegment {
    /// 区间起点（含）。
    pub start: u64,
    /// 下次续传的写入位置（`start <= pos <= end`）。
    pub pos: u64,
    /// 区间终点（不含）。
    pub end: u64,
}

impl PartSegment {
    /// 剩余字节。
    #[must_use]
    pub fn remaining(&self) -> u64 {
        self.end.saturating_sub(self.pos)
    }

    /// 是否已完成。
    #[must_use]
    pub fn is_done(&self) -> bool {
        self.pos >= self.end
    }
}

/// `.parts.json` 的完整内容。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartsFile {
    pub version: u32,
    pub url: String,
    #[serde(default)]
    pub etag: Option<String>,
    #[serde(default)]
    pub last_modified: Option<String>,
    pub total: u64,
    pub segments: Vec<PartSegment>,
}

/// 侧车拒绝沿用的原因（用于日志 / 诊断）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartsReject {
    Missing,
    Unreadable,
    VersionMismatch,
    UrlChanged,
    TotalChanged,
    ValidatorChanged,
    FileMissingOrResized,
    InvalidSegment,
    OverlappingSegments,
}

impl PartsReject {
    /// 简短说明。
    #[must_use]
    pub fn reason(self) -> &'static str {
        match self {
            Self::Missing => "no parts file",
            Self::Unreadable => "parts file unreadable",
            Self::VersionMismatch => "parts file version mismatch",
            Self::UrlChanged => "url changed",
            Self::TotalChanged => "total size changed",
            Self::ValidatorChanged => "server validator changed",
            Self::FileMissingOrResized => "partial file missing or resized",
            Self::InvalidSegment => "segment out of range",
            Self::OverlappingSegments => "segments overlap",
        }
    }
}

impl PartsFile {
    /// 侧车路径：`<out>.parts.json`。
    #[must_use]
    pub fn path_for(out: &Path) -> PathBuf {
        let mut name = out.as_os_str().to_owned();
        name.push(".parts.json");
        PathBuf::from(name)
    }

    /// 已落盘字节数。
    #[must_use]
    pub fn done_bytes(&self) -> u64 {
        self.segments
            .iter()
            .map(|segment| segment.pos.saturating_sub(segment.start))
            .sum()
    }

    /// 读取并校验侧车。返回 `(Option<PartsFile>, 拒绝原因)`。
    #[must_use]
    pub fn load(
        out: &Path,
        url: &str,
        total: u64,
        etag: Option<&str>,
        last_modified: Option<&str>,
    ) -> (Option<Self>, Option<PartsReject>) {
        let path = Self::path_for(out);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return (None, Some(PartsReject::Missing));
            }
            Err(_) => return (None, Some(PartsReject::Unreadable)),
        };
        let mut parts: Self = match serde_json::from_str(&text) {
            Ok(parts) => parts,
            Err(_) => return (None, Some(PartsReject::Unreadable)),
        };
        if parts.version != PARTS_VERSION {
            return (None, Some(PartsReject::VersionMismatch));
        }
        if parts.url != url {
            return (None, Some(PartsReject::UrlChanged));
        }
        if parts.total != total {
            return (None, Some(PartsReject::TotalChanged));
        }
        // 强 validator 优先：ETag 双方都有时必须一致；否则比较 Last-Modified。
        let validator_same = match (parts.etag.as_deref(), etag) {
            (Some(old), Some(new)) => old == new,
            (None, Some(_)) | (Some(_), None) => parts.last_modified.as_deref() == last_modified,
            (None, None) => parts.last_modified.as_deref() == last_modified,
        };
        if !validator_same {
            return (None, Some(PartsReject::ValidatorChanged));
        }
        match std::fs::metadata(out) {
            Ok(meta) if meta.len() == total => {}
            _ => return (None, Some(PartsReject::FileMissingOrResized)),
        }
        let mut segments = parts.segments;
        if segments.is_empty() {
            return (None, Some(PartsReject::Unreadable));
        }
        if segments.iter().any(|segment| {
            segment.start > segment.pos || segment.pos > segment.end || segment.end > total
        }) {
            return (None, Some(PartsReject::InvalidSegment));
        }
        segments.sort_by_key(|segment| segment.start);
        let overlaps = segments.windows(2).any(|window| match window {
            [left, right] => left.end > right.start,
            _ => false,
        });
        if overlaps {
            return (None, Some(PartsReject::OverlappingSegments));
        }
        parts.segments = segments;
        (Some(parts), None)
    }

    /// 原子写入（临时文件 + rename），避免进程被杀时留下半份 JSON。
    pub fn save(&self, out: &Path) -> io::Result<()> {
        let path = Self::path_for(out);
        let text = serde_json::to_string(self)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let tmp = path.with_extension("parts.json.tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, &path)
    }

    /// 删除侧车（忽略不存在）。
    pub fn remove(out: &Path) {
        let _ = std::fs::remove_file(Self::path_for(out));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ufetch-parts-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn write_file(path: &Path, len: usize) {
        let mut file = std::fs::File::create(path).expect("create file");
        file.write_all(&vec![0u8; len]).expect("write file");
    }

    #[test]
    fn roundtrip_and_validate() {
        let dir = temp_dir();
        let out = dir.join("a.bin");
        write_file(&out, 1000);
        let parts = PartsFile {
            version: PARTS_VERSION,
            url: "https://example.com/a.bin".into(),
            etag: Some("abc".into()),
            last_modified: None,
            total: 1000,
            segments: vec![
                PartSegment {
                    start: 0,
                    pos: 500,
                    end: 500,
                },
                PartSegment {
                    start: 500,
                    pos: 500,
                    end: 1000,
                },
            ],
        };
        parts.save(&out).expect("save");
        let (loaded, reject) = PartsFile::load(&out, &parts.url, 1000, Some("abc"), None);
        assert!(reject.is_none());
        assert_eq!(loaded.map(|p| p.done_bytes()), Some(500));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn rejects_changed_validator() {
        let dir = temp_dir();
        let out = dir.join("b.bin");
        write_file(&out, 1000);
        let parts = PartsFile {
            version: PARTS_VERSION,
            url: "u".into(),
            etag: Some("abc".into()),
            last_modified: None,
            total: 1000,
            segments: vec![PartSegment {
                start: 0,
                pos: 100,
                end: 1000,
            }],
        };
        parts.save(&out).expect("save");
        let (_, reject) = PartsFile::load(&out, "u", 1000, Some("def"), None);
        assert_eq!(reject, Some(PartsReject::ValidatorChanged));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn rejects_overlap_and_out_of_range() {
        let dir = temp_dir();
        let out = dir.join("c.bin");
        write_file(&out, 1000);
        let mut parts = PartsFile {
            version: PARTS_VERSION,
            url: "u".into(),
            etag: None,
            last_modified: None,
            total: 1000,
            segments: vec![
                PartSegment {
                    start: 0,
                    pos: 600,
                    end: 600,
                },
                PartSegment {
                    start: 500,
                    pos: 500,
                    end: 1000,
                },
            ],
        };
        parts.save(&out).expect("save");
        let (_, reject) = PartsFile::load(&out, "u", 1000, None, None);
        assert_eq!(reject, Some(PartsReject::OverlappingSegments));

        parts.segments = vec![PartSegment {
            start: 0,
            pos: 100,
            end: 2000,
        }];
        parts.save(&out).expect("save");
        let (_, reject) = PartsFile::load(&out, "u", 1000, None, None);
        assert_eq!(reject, Some(PartsReject::InvalidSegment));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn rejects_resized_file() {
        let dir = temp_dir();
        let out = dir.join("d.bin");
        write_file(&out, 500);
        let parts = PartsFile {
            version: PARTS_VERSION,
            url: "u".into(),
            etag: None,
            last_modified: None,
            total: 1000,
            segments: vec![PartSegment {
                start: 0,
                pos: 10,
                end: 1000,
            }],
        };
        parts.save(&out).expect("save");
        let (_, reject) = PartsFile::load(&out, "u", 1000, None, None);
        assert_eq!(reject, Some(PartsReject::FileMissingOrResized));
        let _ = std::fs::remove_dir_all(dir);
    }
}
