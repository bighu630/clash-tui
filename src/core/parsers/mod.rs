//! 分享链接协议解析器分发与公共工具。
//! 每个解析器：`parse(line) -> Result<(名称, serde_yaml::Mapping), ParseError>`。
//! 名称来自 fragment（#），无 fragment → 空串由上层兜底（vmess/ssr 含协议内名称字段）。

pub mod hysteria2;
pub mod ss;
pub mod ssr;
pub mod trojan;
pub mod tuic;
pub mod vless;
pub mod vmess;

use base64::Engine;
use serde_yaml::{Mapping, Value};

/// 解析错误（订阅/解析层共用）。
#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("{0}")]
    Message(String),
}

/// base64 三态解码：standard / url-safe / 无填充。
pub(crate) fn b64_decode(s: &str) -> Option<Vec<u8>> {
    use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD};
    for engine in [STANDARD, URL_SAFE, STANDARD_NO_PAD, URL_SAFE_NO_PAD] {
        if let Ok(v) = engine.decode(s) {
            return Some(v);
        }
    }
    None
}

/// 百分号解码（url 2.5 起 percent_encoding 模块被移除，手动实现）。
pub(crate) fn pct_decode(s: &str) -> String {
    fn hex(b: u8) -> Option<u8> {
        match b {
            b'0'..=b'9' => Some(b - b'0'),
            b'a'..=b'f' => Some(b - b'a' + 10),
            b'A'..=b'F' => Some(b - b'A' + 10),
            _ => None,
        }
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 构造解析错误。
pub(crate) fn err(msg: impl Into<String>) -> ParseError {
    ParseError::Message(msg.into())
}

/// 解析 `host:port` 段（host 可为 IPv6 字面量 `[::1]`）。
pub(crate) fn split_host_port(s: &str) -> Result<(String, u16), ParseError> {
    let s = s.trim();
    let (host, port) = if let Some(rest) = s.strip_prefix('[') {
        let end = rest.find(']').ok_or_else(|| err(format!("无效地址: {s}")))?;
        let port = rest[end + 1..]
            .strip_prefix(':')
            .ok_or_else(|| err(format!("无效地址: {s}")))?;
        (rest[..end].to_string(), port)
    } else {
        let idx = s.rfind(':').ok_or_else(|| err(format!("无效地址: {s}")))?;
        (s[..idx].to_string(), &s[idx + 1..])
    };
    if host.is_empty() {
        return Err(err(format!("无效地址: {s}")));
    }
    let port: u16 = port.parse().map_err(|_| err(format!("无效端口: {port}")))?;
    Ok((host, port))
}

/// 手写 query 解析（percent-decode；保留空值；`+` 不作空格，分享链接惯例）。
pub(crate) fn parse_query(s: Option<&str>) -> Vec<(String, String)> {
    s.map(|q| {
        q.split('&')
            .filter(|p| !p.is_empty())
            .filter_map(|p| p.split_once('=').map(|(k, v)| (pct_decode(k), pct_decode(v))))
            .collect()
    })
    .unwrap_or_default()
}

/// 往 mapping 写 ws-opts（network=="ws" 且 path/host 非空时）。
pub(crate) fn insert_ws_opts(m: &mut Mapping, network: &str, path: &str, host: &str) {
    if network != "ws" || (path.is_empty() && host.is_empty()) {
        return;
    }
    let mut opts = Mapping::new();
    if !path.is_empty() {
        opts.insert(
            Value::String("path".into()),
            Value::String(path.to_string()),
        );
    }
    if !host.is_empty() {
        let mut headers = Mapping::new();
        headers.insert(Value::String("Host".into()), Value::String(host.to_string()));
        opts.insert(Value::String("headers".into()), Value::Mapping(headers));
    }
    m.insert(Value::String("ws-opts".into()), Value::Mapping(opts));
}

#[cfg(test)]
pub(crate) mod testutil {
    use serde_yaml::{Mapping, Value};

    pub fn v<'a>(m: &'a Mapping, k: &str) -> &'a Value {
        m.get(Value::String(k.to_string()))
            .unwrap_or_else(|| panic!("missing key: {k}"))
    }

    pub fn s(m: &Mapping, k: &str) -> String {
        v(m, k)
            .as_str()
            .unwrap_or_else(|| panic!("{k} 不是字符串: {:?}", v(m, k)))
            .to_string()
    }

    pub fn b(m: &Mapping, k: &str) -> bool {
        v(m, k).as_bool().unwrap_or_else(|| panic!("{k} 不是布尔"))
    }

    pub fn u(m: &Mapping, k: &str) -> u64 {
        v(m, k).as_u64().unwrap_or_else(|| panic!("{k} 不是整数"))
    }

    pub fn seq_str(m: &Mapping, k: &str) -> Vec<String> {
        v(m, k)
            .as_sequence()
            .unwrap_or_else(|| panic!("{k} 不是序列"))
            .iter()
            .map(|x| x.as_str().unwrap_or_else(|| panic!("{k} 元素非字符串")).to_string())
            .collect()
    }

    /// 按路径取嵌套 mapping（借用自原始 mapping）。
    pub fn nested<'a>(m: &'a Mapping, path: &[&str]) -> &'a Mapping {
        let mut cur: &'a Value = m
            .get(Value::String(path[0].to_string()))
            .unwrap_or_else(|| panic!("missing nested key: {}", path[0]));
        for (i, k) in path[1..].iter().enumerate() {
            cur = cur
                .get(Value::String((*k).to_string()))
                .unwrap_or_else(|| panic!("missing nested key: {}", path[..=i + 1].join(".")));
        }
        cur.as_mapping().unwrap_or_else(|| panic!("{} 不是 mapping", path.join(".")))
    }

    pub fn base64_encode(s: &str) -> String {
        use base64::engine::general_purpose::STANDARD;
        base64::Engine::encode(&STANDARD, s.as_bytes())
    }
}
