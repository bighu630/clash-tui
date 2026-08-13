//! tuic 分享链接解析器。
//! 输出字段遵循 mihomo 配置格式（见 plans §4 fixtures）。

use serde_yaml::{Mapping, Value};
use url::Url;

use crate::core::parsers::{err, parse_query, pct_decode, ParseError};

/// 解析 tuic 链接，返回 (名称, yaml 映射)。名称来自 fragment（#）。
pub fn parse(line: &str) -> Result<(String, Mapping), ParseError> {
    let url = Url::parse(line).map_err(|e| err(format!("tuic 链接无效: {e}")))?;
    if url.scheme() != "tuic" {
        return Err(err("不是 tuic 链接"));
    }
    let uuid = url.username().to_string();
    let password = url.password().unwrap_or("").to_string();
    let host = url
        .host_str()
        .ok_or_else(|| err("tuic 链接缺少主机"))?
        .to_string();
    let port = url.port().ok_or_else(|| err("tuic 链接缺少端口"))?;
    let name = pct_decode(url.fragment().unwrap_or(""));
    let q = parse_query(url.query());
    let get = |k: &str| q.iter().find(|(x, _)| x == k).map(|(_, v)| v.clone());

    let mut m = Mapping::new();
    m.insert(Value::String("type".into()), Value::String("tuic".into()));
    m.insert(Value::String("server".into()), Value::String(host));
    m.insert(Value::String("port".into()), Value::Number(port.into()));
    m.insert(Value::String("uuid".into()), Value::String(uuid));
    m.insert(Value::String("password".into()), Value::String(password));
    m.insert(Value::String("udp".into()), Value::Bool(true));

    if let Some(alpn) = get("alpn") {
        if !alpn.is_empty() {
            m.insert(
                Value::String("alpn".into()),
                Value::Sequence(
                    alpn.split(',')
                        .map(|s| Value::String(s.trim().to_string()))
                        .collect(),
                ),
            );
        }
    }
    m.insert(
        Value::String("congestion-controller".into()),
        Value::String(get("congestion_control").unwrap_or_else(|| "cubic".into())),
    );
    m.insert(
        Value::String("udp-relay-mode".into()),
        Value::String(get("udp_relay_mode").unwrap_or_else(|| "native".into())),
    );
    if let Some(sni) = get("sni") {
        if !sni.is_empty() {
            m.insert(Value::String("sni".into()), Value::String(sni));
        }
    }

    Ok((name, m))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::parsers::testutil::{s, seq_str, u};

    const UUID: &str = "9f5f2e5c-1b2c-3d4e-5f6a-7b8c9d0e1f2a";

    #[test]
    fn full_fixture() {
        let line = format!(
            "tuic://{UUID}:pass@1.2.3.4:443?sni=cdn.example.com&alpn=h3&congestion_control=bbr&udp_relay_mode=native#Tuic"
        );
        let (name, m) = parse(&line).unwrap();
        assert_eq!(name, "Tuic");
        assert_eq!(s(&m, "type"), "tuic");
        assert_eq!(s(&m, "server"), "1.2.3.4");
        assert_eq!(u(&m, "port"), 443);
        assert_eq!(s(&m, "uuid"), UUID);
        assert_eq!(s(&m, "password"), "pass");
        assert_eq!(seq_str(&m, "alpn"), vec!["h3"]);
        assert_eq!(s(&m, "congestion-controller"), "bbr");
        assert_eq!(s(&m, "udp-relay-mode"), "native");
        assert_eq!(s(&m, "sni"), "cdn.example.com");
    }

    #[test]
    fn multi_alpn() {
        let line = format!("tuic://{UUID}:pass@1.2.3.4:443?alpn=h3,http/1.1#T");
        let (_name, m) = parse(&line).unwrap();
        assert_eq!(seq_str(&m, "alpn"), vec!["h3", "http/1.1"]);
    }

    #[test]
    fn minimal() {
        let line = format!("tuic://{UUID}:pass@1.2.3.4:443#T");
        let (_name, m) = parse(&line).unwrap();
        assert_eq!(s(&m, "congestion-controller"), "cubic");
        assert_eq!(s(&m, "udp-relay-mode"), "native");
    }

    #[test]
    fn invalid_link_is_error() {
        assert!(parse("tuic://").is_err());
    }
}
