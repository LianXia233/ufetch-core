// SPDX-License-Identifier: LGPL-3.0-or-later
// Copyright (c) 2026 UFetch-Core contributors

//! `Content-Type` → 扩展名，以及"这个名字是否已经带像样的扩展名"。

/// 常见下载类型；路由场景够用即可，未列出的按 `bin` 处理。
const TABLE: &[(&str, &str)] = &[
    ("application/zip", "zip"),
    ("application/gzip", "gz"),
    ("application/x-gzip", "gz"),
    ("application/x-tar", "tar"),
    ("application/x-xz", "xz"),
    ("application/zstd", "zst"),
    ("application/x-7z-compressed", "7z"),
    ("application/x-rar-compressed", "rar"),
    ("application/pdf", "pdf"),
    ("application/json", "json"),
    ("application/octet-stream", "bin"),
    ("application/vnd.android.package-archive", "apk"),
    ("application/vnd.debian.binary-package", "deb"),
    ("application/x-ipk", "ipk"),
    ("text/plain", "txt"),
    ("text/html", "html"),
    ("text/css", "css"),
    ("text/csv", "csv"),
    ("image/png", "png"),
    ("image/jpeg", "jpg"),
    ("image/webp", "webp"),
    ("image/gif", "gif"),
    ("image/svg+xml", "svg"),
    ("video/mp4", "mp4"),
    ("audio/mpeg", "mp3"),
    ("application/wasm", "wasm"),
    ("application/x-sh", "sh"),
];

/// 脚本端点常见的后缀，这些名字不该被当成真实文件名。
const SCRIPT_EXTENSIONS: &[&str] = &["php", "asp", "aspx", "jsp", "cgi", "do", "action", "ashx"];

/// `Content-Type` → 扩展名（不含点）。
#[must_use]
pub fn mime_to_ext(content_type: &str) -> Option<&'static str> {
    let essence = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    TABLE
        .iter()
        .find(|(mime, _)| *mime == essence)
        .map(|(_, extension)| *extension)
}

/// 名字的扩展名是否是脚本端点（`download.php` 这类）。
#[must_use]
pub fn is_script_endpoint(name: &str) -> bool {
    extension_of(name).is_some_and(|extension| {
        SCRIPT_EXTENSIONS
            .iter()
            .any(|script| script.eq_ignore_ascii_case(extension))
    })
}

/// 名字是否已经带一个可接受的扩展名（1–5 位字母数字，且不是脚本）。
#[must_use]
pub fn has_plausible_extension(name: &str) -> bool {
    match extension_of(name) {
        Some(extension) => {
            (1..=5).contains(&extension.len())
                && extension.chars().all(|ch| ch.is_ascii_alphanumeric())
                && !is_script_endpoint(name)
        }
        None => false,
    }
}

fn extension_of(name: &str) -> Option<&str> {
    name.rsplit_once('.')
        .map(|(_, extension)| extension)
        .filter(|extension| !extension.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_common_package_types() {
        assert_eq!(mime_to_ext("application/zip"), Some("zip"));
        assert_eq!(mime_to_ext("APPLICATION/GZIP; charset=x"), Some("gz"));
        assert_eq!(
            mime_to_ext("application/vnd.android.package-archive"),
            Some("apk")
        );
        assert_eq!(mime_to_ext("text/plain"), Some("txt"));
        assert_eq!(mime_to_ext("application/x-made-up"), None);
    }

    #[test]
    fn recognises_script_endpoints() {
        assert!(is_script_endpoint("download.php"));
        assert!(is_script_endpoint("get.ASPX"));
        assert!(!is_script_endpoint("pkg.apk"));
    }

    #[test]
    fn plausibility_requires_a_short_alnum_extension() {
        assert!(has_plausible_extension("pkg.apk"));
        assert!(has_plausible_extension("a.b.c.zip"));
        assert!(!has_plausible_extension("no-extension"));
        assert!(!has_plausible_extension("download.php"));
        assert!(!has_plausible_extension("x.verylongext"));
    }
}
