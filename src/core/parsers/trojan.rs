//! trojan 分享链接解析器。
//! 输出字段遵循 mihomo 配置格式（见 plans §4 fixtures）。

use serde_yaml::{Mapping, Value};
use url::Url;

use crate::core::parsers::{err, insert_ws_opts, parse_query, pct_decode, ParseError};

/// 解析 trojan 链接，返回 (名称, yaml 映射)。名称来自 fragment（#）。
pub fn parse(line: &str) -> Result<(String, Mapping), ParseError> {
    let url = Url::parse(line).map_err(|e| err(format!("trojan 链接无效: {e}")))?;
    if url.scheme() != "trojan" {
        return Err(err("不是 trojan 链接"));
    }
    let password = url.username().to_string();
    let host = url
        .host_str()
        .ok_or_else(|| err("trojan 链接缺少主机"))?
        .to_string();
    let port = url.port().ok_or_else(|| err("trojan 链接缺少端口"))?;
    let name = pct_decode(url.fragment().unwrap_or(""));
    let q = parse_query(url.query());
    let get = |k: &str| q.iter().find(|(x, _)| x == k).map(|(_, v)| v.clone());

    let mut m = Mapping::new();
    m.insert(Value::String("type".into()), Value::String("trojan".into()));
    m.insert(Value::String("server".into()), Value::String(host));
    m.insert(Value::String("port".into()), Value::Number(port.into()));
    m.insert(Value::String("password".into()), Value::String(password));
    m.insert(Value::String("udp".into()), Value::Bool(true));

    let insecure = get("allowInsecure")
        .or_else(|| get("skip-cert-verify"))
        .unwrap_or_default();
    m.insert(
        Value::String("skip-cert-verify".into()),
        Value::Bool(insecure == "1" || insecure.eq_ignore_ascii_case("true")),
    );
    if let Some(sni) = get("sni") {
        if !sni.is_empty() {
            m.insert(Value::String("sni".into()), Value::String(sni));
        }
    }
    if let Some(net) = get("type") {
        if !net.is_empty() {
            m.insert(Value::String("network".into()), Value::String(net.clone()));
            let path = get("path").unwrap_or_default();
            let host = get("host").unwrap_or_default();
            insert_ws_opts(&mut m, &net, &path, &host);
        }
    }

    Ok((name, m))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::parsers::testutil::{b, nested, s, u};

    #[test]
    fn full_fixture() {
        let line = "trojan://pass123@1.2.3.4:443?sni=cdn.example.com&allowInsecure=1&type=ws&host=h.example.com&path=%2Fws#Trojan-WS";
        let (name, m) = parse(line).unwrap();
        assert_eq!(name, "Trojan-WS");
        assert_eq!(s(&m, "type"), "trojan");
        assert_eq!(s(&m, "server"), "1.2.3.4");
        assert_eq!(u(&m, "port"), 443);
        assert_eq!(s(&m, "password"), "pass123");
        assert_eq!(s(&m, "sni"), "cdn.example.com");
        assert!(b(&m, "skip-cert-verify"));
        assert_eq!(s(&m, "network"), "ws");
        let ws = nested(&m, &["ws-opts"]);
        assert_eq!(s(ws, "path"), "/ws");
        assert_eq!(s(nested(ws, &["headers"]), "Host"), "h.example.com");
        assert!(b(&m, "udp"));
    }

    #[test]
    fn allow_insecure_0_or_missing() {
        let line = "trojan://pass123@1.2.3.4:443?allowInsecure=0#A";
        let (_name, m) = parse(line).unwrap();
        assert!(!b(&m, "skip-cert-verify"));

        let line = "trojan://pass123@1.2.3.4:443#B";
        let (_name, m) = parse(line).unwrap();
        assert!(!b(&m, "skip-cert-verify"));
        assert!(m.get(serde_yaml::Value::String("sni".into())).is_none());
    }

    #[test]
    fn invalid_link_is_error() {
        assert!(parse("trojan://").is_err());
        assert!(parse("trojan://pass@").is_err());
    }
}
