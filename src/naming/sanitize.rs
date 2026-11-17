// SPDX-License-Identifier: LGPL-3.0-or-later
// Copyright (c) 2026 UFetch-Core contributors

//! 文件名清洗：把服务器给的名字变成各平台都能落盘的单段文件名。

/// 单段文件名的字节预算。多数文件系统上限 255 字节，这里留出临时后缀与
/// `" (1)"` 去重后缀的空间。
const MAX_BYTES: usize = 200;

/// Windows 保留设备名（比较时只看扩展名之前的部分）。
const RESERVED: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// 清洗文件名。
///
/// - 只保留最后一段（`../../etc/passwd` → `passwd`）；
/// - 非法字符 `< > : " / \ | ? *` 与所有控制字符换成 `_`；
/// - 去掉双向控制符（避免 `invoice\u{202E}fdp.exe` 这类显示欺骗）；
/// - 去掉首尾的空格与点；空名回退到 `download`；
/// - 命中 Windows 保留设备名时加 `_` 前缀；
/// - 超过 200 字节时按字符边界截断，并尽量保留扩展名。
#[must_use]
pub fn sanitize_filename(raw: &str) -> String {
    let last_segment = raw.rsplit(['/', '\\']).next().unwrap_or("").trim();

    let cleaned: String = last_segment
        .chars()
        .filter_map(|ch| {
            if ch.is_control() {
                return None;
            }
            if matches!(ch, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') {
                return Some('_');
            }
            if is_bidi_control(ch) {
                return None;
            }
            Some(ch)
        })
        .collect();

    let mut name = cleaned.trim().trim_matches('.').trim().to_string();
    if name.is_empty() || name == "." || name == ".." {
        return "download".to_string();
    }
    if is_reserved(&name) {
        name.insert(0, '_');
    }
    truncate_bytes(&name, MAX_BYTES)
}

fn is_bidi_control(ch: char) -> bool {
    matches!(
        ch,
        '\u{061C}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}'
    ) || ch == '\u{FEFF}'
        || ch == '\u{200B}'
}

fn is_reserved(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name).to_ascii_uppercase();
    RESERVED.contains(&stem.as_str())
}

/// 按字节截断，但不切断 UTF-8 字符；有扩展名时优先保住扩展名。
fn truncate_bytes(name: &str, budget: usize) -> String {
    if name.len() <= budget {
        return name.to_string();
    }
    let extension = name
        .rfind('.')
        .filter(|index| name.len() - index <= 16)
        .map(|index| name[index..].to_string())
        .unwrap_or_default();
    let keep = budget.saturating_sub(extension.len());
    let mut out = String::new();
    for ch in name.chars() {
        if out.len() + ch.len_utf8() > keep {
            break;
        }
        out.push(ch);
    }
    let out = out.trim_end_matches('.').trim_end();
    if out.is_empty() {
        return "download".to_string();
    }
    format!("{out}{extension}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_directories_and_illegal_characters() {
        assert_eq!(sanitize_filename("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_filename("a<b>c:d\"e|f?g*h"), "a_b_c_d_e_f_g_h");
        assert_eq!(sanitize_filename("  spaced.apk  "), "spaced.apk");
    }

    #[test]
    fn empty_and_dot_names_fall_back() {
        assert_eq!(sanitize_filename(""), "download");
        assert_eq!(sanitize_filename(".."), "download");
        assert_eq!(sanitize_filename("///"), "download");
    }

    #[test]
    fn avoids_windows_reserved_devices() {
        assert_eq!(sanitize_filename("CON"), "_CON");
        assert_eq!(sanitize_filename("nul.txt"), "_nul.txt");
        assert_eq!(sanitize_filename("console.txt"), "console.txt");
    }

    #[test]
    fn removes_bidi_override_characters() {
        assert_eq!(
            sanitize_filename("invoice\u{202E}fdp.exe"),
            "invoicefdp.exe"
        );
    }

    #[test]
    fn truncates_without_breaking_utf8_and_keeps_extension() {
        let name = format!("{}x.apk", "长".repeat(200));
        let out = sanitize_filename(&name);
        assert!(out.len() <= MAX_BYTES, "{} bytes", out.len());
        assert!(out.ends_with(".apk"), "{out}");
        assert!(out.chars().all(|c| c != '\u{FFFD}'));
    }
}
