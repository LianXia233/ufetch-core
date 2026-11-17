// SPDX-License-Identifier: LGPL-3.0-or-later
// Copyright (c) 2026 UFetch-Core contributors

//! 落盘文件的摘要校验。
//!
//! 只实现下载校验需要的四种算法，流式读取，随时可取消。

use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

use md5::Md5;
use sha1::Sha1;
use sha2::{Digest, Sha256, Sha512};

/// 读取文件的块大小。
const CHUNK: usize = 64 * 1024;

/// 支持的摘要算法。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HashAlgorithm {
    Sha256,
    Sha1,
    Md5,
    Sha512,
}

impl HashAlgorithm {
    /// 解析 `sha256` / `sha-256` / `sha_256` / `sha1` / `md5` / `sha512` 这类写法。
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        let normalized: String = raw
            .trim()
            .to_ascii_lowercase()
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .collect();
        match normalized.as_str() {
            "sha256" => Some(Self::Sha256),
            "sha1" => Some(Self::Sha1),
            "md5" => Some(Self::Md5),
            "sha512" => Some(Self::Sha512),
            _ => None,
        }
    }

    /// 标准写法，用于 `algo=hex` 输出。
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sha256 => "sha256",
            Self::Sha1 => "sha1",
            Self::Md5 => "md5",
            Self::Sha512 => "sha512",
        }
    }

    /// 十六进制摘要长度，用于提前拒绝长度不对的期望值。
    #[must_use]
    pub fn hex_len(self) -> usize {
        match self {
            Self::Sha256 => 64,
            Self::Sha1 => 40,
            Self::Md5 => 32,
            Self::Sha512 => 128,
        }
    }
}

/// 流式计算摘要，返回小写十六进制。
///
/// # Errors
///
/// 读取失败；`cancelled()` 返回真时用 `ErrorKind::Interrupted` 提前结束。
pub fn hash_file(
    path: &Path,
    algorithm: HashAlgorithm,
    cancelled: impl Fn() -> bool,
) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut buffer = vec![0u8; CHUNK];
    match algorithm {
        HashAlgorithm::Sha256 => digest(&mut file, &mut buffer, Sha256::new(), &cancelled),
        HashAlgorithm::Sha1 => digest(&mut file, &mut buffer, Sha1::new(), &cancelled),
        HashAlgorithm::Md5 => digest(&mut file, &mut buffer, Md5::new(), &cancelled),
        HashAlgorithm::Sha512 => digest(&mut file, &mut buffer, Sha512::new(), &cancelled),
    }
}

fn digest<D: Digest>(
    file: &mut File,
    buffer: &mut [u8],
    mut hasher: D,
    cancelled: &impl Fn() -> bool,
) -> io::Result<String> {
    loop {
        if cancelled() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "checksum cancelled",
            ));
        }
        let read = file.read(buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str, bytes: &[u8]) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("ufetch-hash-{tag}-{}", std::process::id()));
        std::fs::write(&path, bytes).expect("write");
        path
    }

    #[test]
    fn parses_common_spellings() {
        assert_eq!(HashAlgorithm::parse("SHA-256"), Some(HashAlgorithm::Sha256));
        assert_eq!(HashAlgorithm::parse("sha_1"), Some(HashAlgorithm::Sha1));
        assert_eq!(HashAlgorithm::parse(" MD5 "), Some(HashAlgorithm::Md5));
        assert_eq!(HashAlgorithm::parse("sha-512"), Some(HashAlgorithm::Sha512));
        assert_eq!(HashAlgorithm::parse("crc32"), None);
    }

    #[test]
    fn matches_known_empty_file_digests() {
        let path = scratch("empty", b"");
        assert_eq!(
            hash_file(&path, HashAlgorithm::Sha256, || false).expect("sha256"),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hash_file(&path, HashAlgorithm::Sha1, || false).expect("sha1"),
            "da39a3ee5e6b4b0d3255bfef95601890afd80709"
        );
        assert_eq!(
            hash_file(&path, HashAlgorithm::Md5, || false).expect("md5"),
            "d41d8cd98f00b204e9800998ecf8427e"
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn hashes_file_contents() {
        let path = scratch("hello", b"hello");
        assert_eq!(
            hash_file(&path, HashAlgorithm::Sha256, || false).expect("sha256"),
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn cancellation_stops_the_read() {
        let path = scratch("cancel", &vec![0u8; 512 * 1024]);
        let error = hash_file(&path, HashAlgorithm::Sha256, || true).expect_err("cancelled");
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn hex_lengths_match_the_algorithms() {
        assert_eq!(HashAlgorithm::Md5.hex_len(), 32);
        assert_eq!(HashAlgorithm::Sha1.hex_len(), 40);
        assert_eq!(HashAlgorithm::Sha256.hex_len(), 64);
        assert_eq!(HashAlgorithm::Sha512.hex_len(), 128);
    }
}
