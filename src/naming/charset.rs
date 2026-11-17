// SPDX-License-Identifier: LGPL-3.0-or-later
// Copyright (c) 2026 UFetch-Core contributors

//! 百分号解码与带 label 的字符集解码。
//!
//! 只服务文件名场景：`filename*=utf-8''%E4%B8%AD.apk` 与 URL 路径里的 `%20`。
//! 未知 label 按 UTF-8 → Latin-1 的顺序兜底，宁可显示成乱码也不返回失败。

/// 把 `%XX` 还原成字节；非法转义原样保留（与浏览器行为一致）。
#[must_use]
pub fn percent_decode(input: &str) -> Vec<u8> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let high = hex_value(bytes[index + 1]);
            let low = hex_value(bytes[index + 2]);
            if let (Some(high), Some(low)) = (high, low) {
                out.push(high << 4 | low);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    out
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// 按 label 解码；label 为空或无法识别时先试 UTF-8，再按 Latin-1 逐字节映射。
#[must_use]
pub fn decode_bytes(bytes: &[u8], label: Option<&str>) -> String {
    if let Some(label) = label {
        let encoding = encoding_rs::Encoding::for_label(label.trim().as_bytes());
        if let Some(encoding) = encoding
            && encoding != encoding_rs::UTF_8
        {
            let (text, _, _) = encoding.decode(bytes);
            return text.into_owned();
        }
    }
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_string(),
        Err(_) => bytes
            .iter()
            .map(|byte| {
                if *byte < 0x80 {
                    char::from(*byte)
                } else {
                    char::from_u32(u32::from(*byte)).unwrap_or('�')
                }
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_percent_escapes() {
        assert_eq!(percent_decode("%E4%B8%AD%20x"), "中 x".as_bytes());
        assert_eq!(percent_decode("abc"), b"abc");
    }

    #[test]
    fn keeps_invalid_escapes_verbatim() {
        assert_eq!(percent_decode("%zz%2"), b"%zz%2");
    }

    #[test]
    fn decodes_utf8_and_latin1_labels() {
        let utf8 = percent_decode("%E4%B8%AD%E6%96%87.apk");
        assert_eq!(decode_bytes(&utf8, Some("UTF-8")), "中文.apk");
        let latin1 = percent_decode("caf%E9.apk");
        assert_eq!(decode_bytes(&latin1, Some("ISO-8859-1")), "café.apk");
    }

    #[test]
    fn falls_back_to_latin1_for_undecodable_bytes() {
        assert_eq!(decode_bytes(&[0x63, 0x61, 0x66, 0xE9], None), "café");
    }
}
