// SPDX-License-Identifier: LGPL-3.0-or-later
// Copyright (c) 2026 UFetch-Core contributors

//! 下载文件名推断。
//!
//! 证据优先级：响应 `Content-Disposition` → URL 查询里的
//! `response-content-disposition` / `rscd` → URL 路径末段 → `Content-Type`
//! → `download`。所有结果都会经过 [`sanitize_filename`]。
//!
//! 设计目标只有一个：让 OpenWrt 插件升级时拿到的文件名可靠、可落盘、
//! 不把服务器上的路径穿越带进本地。

mod charset;
mod disposition;
mod mime;
mod sanitize;

use charset::percent_decode;
pub use mime::{has_plausible_extension, is_script_endpoint, mime_to_ext};
pub use sanitize::sanitize_filename;

/// 推断结果。目前只暴露文件名本身；证据来源留在实现内部，避免调用方依赖。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedName {
    pub name: String,
}

/// 从响应头与 URL 推断文件名。
#[must_use]
pub fn extract_filename(
    headers: &reqwest::header::HeaderMap,
    request_url: &str,
    final_url: &str,
) -> ResolvedName {
    if let Some(value) = headers.get(reqwest::header::CONTENT_DISPOSITION)
        && let Some(raw) = disposition::parse(value.as_bytes())
    {
        return ResolvedName {
            name: sanitize_filename(&raw),
        };
    }

    for url in [final_url, request_url] {
        if let Some(raw) = query_disposition(url) {
            return ResolvedName {
                name: sanitize_filename(&raw),
            };
        }
    }

    let content_type = headers
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");

    for url in [final_url, request_url] {
        if let Some(mut name) = path_filename(url) {
            name = apply_content_type(name, content_type);
            return ResolvedName {
                name: sanitize_filename(&name),
            };
        }
    }

    if let Some(extension) = mime_to_ext(content_type) {
        return ResolvedName {
            name: format!("download.{extension}"),
        };
    }

    ResolvedName {
        name: "download".to_string(),
    }
}

/// 从 URL 查询里取服务器回显的 disposition。
fn query_disposition(url: &str) -> Option<String> {
    let parsed = reqwest::Url::parse(url).ok()?;
    for (key, value) in parsed.query_pairs() {
        if (key.eq_ignore_ascii_case("response-content-disposition")
            || key.eq_ignore_ascii_case("rscd"))
            && let Some(name) = disposition::parse_str(&value)
        {
            return Some(name);
        }
    }
    None
}

/// URL 路径末段 → 文件名（百分号解码后）。
fn path_filename(url: &str) -> Option<String> {
    let parsed = reqwest::Url::parse(url).ok()?;
    let segment = parsed
        .path_segments()?
        .rfind(|segment| !segment.is_empty())?;
    let decoded = String::from_utf8_lossy(&percent_decode(segment)).into_owned();
    let decoded = decoded.trim();
    if decoded.is_empty() || decoded == "." || decoded == ".." {
        return None;
    }
    Some(decoded.to_string())
}

/// 用 `Content-Type` 修正 URL 名字：脚本端点换扩展名，没有像样扩展名时补一个。
fn apply_content_type(name: String, content_type: &str) -> String {
    let Some(extension) = mime_to_ext(content_type) else {
        return name;
    };
    if is_script_endpoint(&name) {
        let stem = name
            .rsplit_once('.')
            .map(|(stem, _)| stem)
            .unwrap_or(&name)
            .to_string();
        return format!("{stem}.{extension}");
    }
    if !has_plausible_extension(&name) && !name.ends_with(&format!(".{extension}")) {
        return format!("{name}.{extension}");
    }
    name
}

#[cfg(test)]
mod tests {
    use reqwest::header::{CONTENT_DISPOSITION, CONTENT_TYPE, HeaderMap, HeaderValue};

    use super::*;

    fn headers(pairs: &[(reqwest::header::HeaderName, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(name.clone(), HeaderValue::from_str(value).expect("value"));
        }
        map
    }

    #[test]
    fn disposition_is_the_strongest_signal() {
        let map = headers(&[
            (CONTENT_DISPOSITION, "attachment; filename=\"pkg 1.apk\""),
            (CONTENT_TYPE, "application/octet-stream"),
        ]);
        let resolved = extract_filename(&map, "https://x/download.php", "https://x/download.php");
        assert_eq!(resolved.name, "pkg 1.apk");
    }

    #[test]
    fn query_disposition_is_used_when_the_header_is_absent() {
        let map = headers(&[(CONTENT_TYPE, "application/zip")]);
        let url = "https://bucket.s3.example/key?response-content-disposition=attachment%3B%20filename%3D%22fw.bin%22";
        let resolved = extract_filename(&map, url, url);
        assert_eq!(resolved.name, "fw.bin");
    }

    #[test]
    fn url_path_is_percent_decoded() {
        let map = headers(&[(CONTENT_TYPE, "application/octet-stream")]);
        let resolved = extract_filename(
            &map,
            "https://x/%E4%B8%AD%E6%96%87.apk",
            "https://x/%E4%B8%AD%E6%96%87.apk",
        );
        assert_eq!(resolved.name, "中文.apk");
    }

    #[test]
    fn script_endpoint_gets_the_mime_extension() {
        let map = headers(&[(CONTENT_TYPE, "application/zip")]);
        let resolved = extract_filename(
            &map,
            "https://x/download.php?id=1",
            "https://x/download.php?id=1",
        );
        assert_eq!(resolved.name, "download.zip");
    }

    #[test]
    fn redirect_target_wins_over_the_request_url() {
        let map = headers(&[(CONTENT_TYPE, "application/octet-stream")]);
        let resolved = extract_filename(
            &map,
            "https://x/redirect?id=1",
            "https://cdn.example/pkg.apk",
        );
        assert_eq!(resolved.name, "pkg.apk");
    }

    #[test]
    fn content_type_alone_builds_a_name() {
        let map = headers(&[(CONTENT_TYPE, "application/vnd.android.package-archive")]);
        let resolved = extract_filename(&map, "https://x/", "https://x/");
        assert_eq!(resolved.name, "download.apk");
    }

    #[test]
    fn no_evidence_falls_back_to_download() {
        let map = HeaderMap::new();
        let resolved = extract_filename(&map, "https://x/", "https://x/");
        assert_eq!(resolved.name, "download");
    }

    #[test]
    fn path_traversal_in_the_url_is_neutralised() {
        let map = headers(&[(CONTENT_TYPE, "application/octet-stream")]);
        let resolved = extract_filename(
            &map,
            "https://x/..%2F..%2Fetc%2Fpasswd",
            "https://x/..%2F..%2Fetc%2Fpasswd",
        );
        assert!(!resolved.name.contains('/'));
    }
}
