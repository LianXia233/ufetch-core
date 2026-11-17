// SPDX-License-Identifier: LGPL-3.0-or-later
// Copyright (c) 2026 UFetch-Core contributors

//! 把传输层错误归类成可判定的 `[net:code]` 标签。
//!
//! 只关心"换一条线路 / 重试是否有意义"：DNS、建连、超时、TLS、连接重置、停滞、IO。
//! 归类结果只用于路由决策与日志，不参与文案本地化。

use std::error::Error as StdError;
use std::io;

use crate::error::DownloadError;

/// 传输失败的类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetFailure {
    /// 域名解析失败。
    Dns,
    /// 建连失败（拒绝 / 不可达）。
    Connect,
    /// 超时。
    Timeout,
    /// TLS / 证书问题。
    Tls,
    /// 连接被对端重置或提前关闭。
    Reset,
    /// 长时间没有数据（看门狗判定）。
    Stall,
    /// 其它本地 IO 失败。
    Io,
}

impl NetFailure {
    /// 稳定的短代码，写进日志与 `summary()`。
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            Self::Dns => "dns",
            Self::Connect => "connect",
            Self::Timeout => "timeout",
            Self::Tls => "tls",
            Self::Reset => "reset",
            Self::Stall => "stall",
            Self::Io => "io",
        }
    }

    /// 从短代码还原。
    #[must_use]
    pub fn from_code(code: &str) -> Option<Self> {
        Some(match code {
            "dns" => Self::Dns,
            "connect" => Self::Connect,
            "timeout" => Self::Timeout,
            "tls" => Self::Tls,
            "reset" => Self::Reset,
            "stall" => Self::Stall,
            "io" => Self::Io,
            _ => return None,
        })
    }
}

/// 解析文案里已有的 `[net:xxx]` 标签。
#[must_use]
pub fn tag_of(msg: &str) -> Option<NetFailure> {
    let start = msg.find("[net:")? + "[net:".len();
    let rest = &msg[start..];
    let end = rest.find(']')?;
    NetFailure::from_code(rest[..end].trim())
}

/// 在文案末尾追加标签；已经有标签就不重复追加。
#[must_use]
pub fn append_tag(msg: String, failure: Option<NetFailure>) -> String {
    match failure {
        Some(failure) if tag_of(&msg).is_none() => format!("{msg} [net:{}]", failure.code()),
        _ => msg,
    }
}

/// 沿 `source()` 链识别错误类型。
#[must_use]
pub fn classify(err: &(dyn StdError + 'static)) -> Option<NetFailure> {
    if let Some(io_error) = err.downcast_ref::<io::Error>() {
        return classify_io(io_error.kind());
    }
    if let Some(reqwest_error) = err.downcast_ref::<reqwest::Error>() {
        if reqwest_error.is_timeout() {
            return Some(NetFailure::Timeout);
        }
        if reqwest_error.is_connect() {
            return Some(if mentions_dns(reqwest_error) {
                NetFailure::Dns
            } else {
                NetFailure::Connect
            });
        }
        if reqwest_error.is_body() || reqwest_error.is_decode() {
            return Some(NetFailure::Reset);
        }
    }
    if err.downcast_ref::<rustls::Error>().is_some() {
        return Some(NetFailure::Tls);
    }

    let mut source = err.source();
    while let Some(current) = source {
        if let Some(io_error) = current.downcast_ref::<io::Error>()
            && let Some(failure) = classify_io(io_error.kind())
        {
            return Some(failure);
        }
        if current.downcast_ref::<rustls::Error>().is_some() {
            return Some(NetFailure::Tls);
        }
        source = current.source();
    }
    None
}

fn classify_io(kind: io::ErrorKind) -> Option<NetFailure> {
    Some(match kind {
        io::ErrorKind::TimedOut => NetFailure::Timeout,
        io::ErrorKind::ConnectionRefused
        | io::ErrorKind::HostUnreachable
        | io::ErrorKind::NetworkUnreachable => NetFailure::Connect,
        io::ErrorKind::ConnectionReset
        | io::ErrorKind::ConnectionAborted
        | io::ErrorKind::BrokenPipe
        | io::ErrorKind::UnexpectedEof => NetFailure::Reset,
        io::ErrorKind::Interrupted => return None,
        _ => return None,
    })
}

fn mentions_dns(error: &reqwest::Error) -> bool {
    let mut current: Option<&(dyn StdError + 'static)> = Some(error);
    while let Some(item) = current {
        let text = item.to_string().to_ascii_lowercase();
        if text.contains("dns error") || text.contains("lookup address") {
            return true;
        }
        current = item.source();
    }
    false
}

/// 下载错误 → 网络类别。
#[must_use]
pub fn classify_download_error(error: &DownloadError) -> Option<NetFailure> {
    match error {
        DownloadError::Request(inner) => classify(inner),
        DownloadError::Io(inner) => classify(inner),
        DownloadError::Stall { .. } => Some(NetFailure::Stall),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn io_kinds_map_to_stable_codes() {
        let refused = io::Error::from(io::ErrorKind::ConnectionRefused);
        assert_eq!(classify(&refused), Some(NetFailure::Connect));
        let reset = io::Error::from(io::ErrorKind::ConnectionReset);
        assert_eq!(classify(&reset), Some(NetFailure::Reset));
        let timeout = io::Error::from(io::ErrorKind::TimedOut);
        assert_eq!(classify(&timeout), Some(NetFailure::Timeout));
        let other = io::Error::from(io::ErrorKind::NotFound);
        assert_eq!(classify(&other), None);
    }

    #[test]
    fn tags_are_appended_once_and_parsed_back() {
        let tagged = append_tag("boom".to_string(), Some(NetFailure::Dns));
        assert_eq!(tagged, "boom [net:dns]");
        assert_eq!(tag_of(&tagged), Some(NetFailure::Dns));
        assert_eq!(
            append_tag(tagged.clone(), Some(NetFailure::Timeout)),
            tagged
        );
        assert_eq!(append_tag("plain".into(), None), "plain");
    }

    #[test]
    fn all_codes_round_trip() {
        for failure in [
            NetFailure::Dns,
            NetFailure::Connect,
            NetFailure::Timeout,
            NetFailure::Tls,
            NetFailure::Reset,
            NetFailure::Stall,
            NetFailure::Io,
        ] {
            assert_eq!(NetFailure::from_code(failure.code()), Some(failure));
        }
        assert_eq!(NetFailure::from_code("nope"), None);
    }

    #[test]
    fn stall_is_classified_from_the_download_error() {
        let error = DownloadError::Stall {
            segment: 1,
            idle_secs: 30,
        };
        assert_eq!(classify_download_error(&error), Some(NetFailure::Stall));
    }
}
