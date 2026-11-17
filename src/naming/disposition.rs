// SPDX-License-Identifier: LGPL-3.0-or-later
// Copyright (c) 2026 UFetch-Core contributors

//! `Content-Disposition` 里的文件名解析。
//!
//! 支持 RFC 6266 的 `filename`、RFC 8187 的 `filename*=charset'lang'value`，
//! 以及 RFC 2231 的 `filename*0*` / `filename*1*` 续行。优先级：
//! `filename*` > 续行 > `filename`。

use std::collections::BTreeMap;

use super::charset::{decode_bytes, percent_decode};

/// 解析头值，返回清理前的原始文件名；没有文件名参数时返回 `None`。
#[must_use]
pub fn parse(value: &[u8]) -> Option<String> {
    parse_str(&String::from_utf8_lossy(value))
}

/// 解析字符串形式的头值。
#[must_use]
pub fn parse_str(value: &str) -> Option<String> {
    let mut extended: Option<String> = None;
    let mut plain: Option<String> = None;
    let mut continuations: BTreeMap<u32, (String, bool)> = BTreeMap::new();

    for parameter in split_parameters(value) {
        let Some((key, raw)) = parameter.split_once('=') else {
            continue;
        };
        let key = key.trim().to_ascii_lowercase();
        let raw = raw.trim();
        let unquoted = unquote(raw);
        if key == "filename*" {
            extended = Some(unquoted);
        } else if key == "filename" {
            plain = Some(unquoted);
        } else if let Some(rest) = key.strip_prefix("filename*")
            && let Some(index) = rest.trim_end_matches('*').parse::<u32>().ok()
        {
            let encoded = rest.ends_with('*');
            continuations.insert(index, (unquoted, encoded));
        }
    }

    if let Some(value) = extended
        && let Some(name) = decode_extended(&value)
        && !name.is_empty()
    {
        return Some(name);
    }

    if !continuations.is_empty() {
        let mut joined = String::new();
        for (piece, _) in continuations.values() {
            joined.push_str(piece);
        }
        if let Some(name) = decode_extended(&joined)
            && !name.is_empty()
        {
            return Some(name);
        }
    }

    plain.filter(|name| !name.is_empty())
}

/// 按分号切参数，但尊重引号内的分号。
fn split_parameters(value: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut escaped = false;
    for ch in value.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        match ch {
            '\\' if quoted => {
                current.push(ch);
                escaped = true;
            }
            '"' => {
                quoted = !quoted;
                current.push(ch);
            }
            ';' if !quoted => {
                parts.push(std::mem::take(&mut current));
            }
            _ => current.push(ch),
        }
    }
    parts.push(current);
    parts
}

/// 去掉包裹的引号并还原 `\"` 转义。
fn unquote(raw: &str) -> String {
    let trimmed = raw.trim();
    let inner = if trimmed.len() >= 2 && trimmed.starts_with('"') && trimmed.ends_with('"') {
        &trimmed[1..trimmed.len() - 1]
    } else {
        trimmed
    };
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            if let Some(next) = chars.next() {
                out.push(next);
            }
        } else {
            out.push(ch);
        }
    }
    out
}

/// 解析 `charset'lang'percent-encoded`。
fn decode_extended(value: &str) -> Option<String> {
    let mut parts = value.splitn(3, '\'');
    let charset = parts.next()?;
    let _language = parts.next();
    let data = parts.next()?;
    let bytes = percent_decode(data);
    Some(decode_bytes(&bytes, Some(charset)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_plain_quoted_filenames() {
        assert_eq!(
            parse_str("attachment; filename=\"pkg 1.apk\"").as_deref(),
            Some("pkg 1.apk")
        );
        assert_eq!(
            parse_str("attachment; filename=pkg.apk").as_deref(),
            Some("pkg.apk")
        );
    }

    #[test]
    fn supports_rfc8187_extended_values() {
        assert_eq!(
            parse_str("attachment; filename*=UTF-8''%E4%B8%AD%E6%96%87.apk").as_deref(),
            Some("中文.apk")
        );
        assert_eq!(
            parse_str("attachment; filename*=ISO-8859-1''caf%E9.apk").as_deref(),
            Some("café.apk")
        );
    }

    #[test]
    fn joins_rfc2231_continuations() {
        assert_eq!(
            parse_str("attachment; filename*0*=UTF-8''%E4%B8%AD; filename*1*=%E6%96%87.apk")
                .as_deref(),
            Some("中文.apk")
        );
    }

    #[test]
    fn extended_wins_over_plain() {
        assert_eq!(
            parse_str("attachment; filename=\"fallback.apk\"; filename*=UTF-8''real.apk")
                .as_deref(),
            Some("real.apk")
        );
    }

    #[test]
    fn quoted_values_may_contain_semicolons_and_escapes() {
        assert_eq!(
            parse_str("attachment; filename=\"a;b\\\"c.apk\"").as_deref(),
            Some("a;b\"c.apk")
        );
    }

    #[test]
    fn missing_or_empty_filename_is_none() {
        assert_eq!(parse_str("attachment"), None);
        assert_eq!(parse_str("attachment; filename=\"\""), None);
    }
}
