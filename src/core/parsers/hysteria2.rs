//! hysteria2 分享链接解析器。
//! 输出字段遵循 mihomo 配置格式（见 plans §4 fixtures）。

use serde_yaml::{Mapping, Value};
use url::Url;

use crate::core::parsers::{err, parse_query, pct_decode, ParseError};

/// 解析 hysteria2 链接（hy2:// 亦接受），返回 (名称, yaml 映射)。名称来自 fragment（#）。
pub fn parse(line: &str) -> Result<(String, Mapping), ParseError> {
    let url = Url::parse(line).map_err(|e| err(format!("hysteria2 链接无效: {e}")))?;
    if !matches!(url.scheme(), "hysteria2" | "hy2") {
        return Err(err("不是 hysteria2 链接"));
    }
    let password = url.username().to_string();
    let host = url
        .host_str()
        .ok_or_else(|| err("hysteria2 链接缺少主机"))?
        .to_string();
    let port = url.port().ok_or_else(|| err("hysteria2 链接缺少端口"))?;
    let name = pct_decode(url.fragment().unwrap_or(""));
    let q = parse_query(url.query());
    let get = |k: &str| q.iter().find(|(x, _)| x == k).map(|(_, v)| v.clone());

    let mut m = Mapping::new();
    m.insert(
        Value::String("type".into()),
        Value::String("hysteria2".into()),
    );
    m.insert(Value::String("server".into()), Value::String(host));
    m.insert(Value::String("port".into()), Value::Number(port.into()));
    m.insert(Value::String("password".into()), Value::String(password));
    m.insert(Value::String("udp".into()), Value::Bool(true));

    let insecure = get("insecure").unwrap_or_default();
    m.insert(
        Value::String("skip-cert-verify".into()),
        Value::Bool(insecure == "1" || insecure.eq_ignore_ascii_case("true")),
    );
    if let Some(sni) = get("sni") {
        if !sni.is_empty() {
            m.insert(Value::String("sni".into()), Value::String(sni));
        }
    }
    if let Some(obfs) = get("obfs") {
        if !obfs.is_empty() {
            m.insert(Value::String("obfs".into()), Value::String(obfs));
        }
    }
    if let Some(obfs_password) = get("obfs-password") {
        if !obfs_password.is_empty() {
            m.insert(
                Value::String("obfs-password".into()),
                Value::String(obfs_password),
            );
        }
    }

    Ok((name, m))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::parsers::testutil::{b, s, u};

    #[test]
    fn full_fixture() {
        let line = "hysteria2://pass@1.2.3.4:8443?sni=cdn.example.com&insecure=1&obfs=salamander&obfs-password=obfs-pass#Hy2";
        let (name, m) = parse(line).unwrap();
        assert_eq!(name, "Hy2");
        assert_eq!(s(&m, "type"), "hysteria2");
        assert_eq!(s(&m, "server"), "1.2.3.4");
        assert_eq!(u(&m, "port"), 8443);
        assert_eq!(s(&m, "password"), "pass");
        assert_eq!(s(&m, "sni"), "cdn.example.com");
        assert!(b(&m, "skip-cert-verify"));
        assert_eq!(s(&m, "obfs"), "salamander");
        assert_eq!(s(&m, "obfs-password"), "obfs-pass");
        assert!(b(&m, "udp"));
    }

    #[test]
    fn plain() {
        let line = "hysteria2://pass@1.2.3.4:8443#X";
        let (_name, m) = parse(line).unwrap();
        assert!(!b(&m, "skip-cert-verify"));
        assert!(m.get(serde_yaml::Value::String("sni".into())).is_none());
        assert!(m.get(serde_yaml::Value::String("obfs".into())).is_none());
    }

    #[test]
    fn hy2_scheme_alias() {
        let line = "hy2://pass@1.2.3.4:8443#Y";
        let (name, m) = parse(line).unwrap();
        assert_eq!(name, "Y");
        assert_eq!(s(&m, "type"), "hysteria2");
    }

    #[test]
    fn invalid_link_is_error() {
        assert!(parse("hysteria2://").is_err());
    }
}
