//! vless 分享链接解析器。
//! 输出字段遵循 mihomo 配置格式（见 plans §4 fixtures）。

use serde_yaml::{Mapping, Value};
use url::Url;

use crate::core::parsers::{err, insert_ws_opts, parse_query, pct_decode, ParseError};

/// 解析 vless 链接，返回 (名称, yaml 映射)。名称来自 fragment（#）。
pub fn parse(line: &str) -> Result<(String, Mapping), ParseError> {
    let url = Url::parse(line).map_err(|e| err(format!("vless 链接无效: {e}")))?;
    if url.scheme() != "vless" {
        return Err(err("不是 vless 链接"));
    }
    let uuid = url.username().to_string();
    let host = url
        .host_str()
        .ok_or_else(|| err("vless 链接缺少主机"))?
        .to_string();
    let port = url.port().ok_or_else(|| err("vless 链接缺少端口"))?;
    let name = pct_decode(url.fragment().unwrap_or(""));
    let q = parse_query(url.query());
    let get = |k: &str| q.iter().find(|(x, _)| x == k).map(|(_, v)| v.clone());

    let mut m = Mapping::new();
    m.insert(Value::String("type".into()), Value::String("vless".into()));
    m.insert(Value::String("server".into()), Value::String(host));
    m.insert(Value::String("port".into()), Value::Number(port.into()));
    m.insert(Value::String("uuid".into()), Value::String(uuid));
    m.insert(Value::String("udp".into()), Value::Bool(true));

    let security = get("security").unwrap_or_default();
    m.insert(
        Value::String("tls".into()),
        Value::Bool(matches!(security.as_str(), "tls" | "reality")),
    );
    if let Some(sni) = get("sni") {
        if !sni.is_empty() {
            m.insert(Value::String("servername".into()), Value::String(sni));
        }
    }
    if let Some(net) = get("type") {
        if !net.is_empty() {
            m.insert(Value::String("network".into()), Value::String(net.clone()));
            let path = get("path").unwrap_or_default();
            let host = get("host").unwrap_or_default();
            insert_ws_opts(&mut m, &net, &path, &host);
            if net == "grpc" {
                let svc = get("serviceName")
                    .or_else(|| get("path"))
                    .unwrap_or_default();
                if !svc.is_empty() {
                    let mut opts = Mapping::new();
                    opts.insert(
                        Value::String("grpc-service-name".into()),
                        Value::String(svc),
                    );
                    m.insert(Value::String("grpc-opts".into()), Value::Mapping(opts));
                }
            }
        }
    }
    if let Some(fp) = get("fp") {
        if !fp.is_empty() {
            m.insert(
                Value::String("client-fingerprint".into()),
                Value::String(fp),
            );
        }
    }
    if security == "reality" {
        let pbk = get("pbk").unwrap_or_default();
        let sid = get("sid").unwrap_or_default();
        if !pbk.is_empty() || !sid.is_empty() {
            let mut opts = Mapping::new();
            if !pbk.is_empty() {
                opts.insert(Value::String("public-key".into()), Value::String(pbk));
            }
            if !sid.is_empty() {
                opts.insert(Value::String("short-id".into()), Value::String(sid));
            }
            m.insert(Value::String("reality-opts".into()), Value::Mapping(opts));
        }
    }
    if let Some(flow) = get("flow") {
        if !flow.is_empty() {
            m.insert(Value::String("flow".into()), Value::String(flow));
        }
    }

    Ok((name, m))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::parsers::testutil::{b, nested, s, u};

    const UUID: &str = "3b1b1b1b-3b1b-3b1b-3b1b-3b1b3b1b3b1b";

    #[test]
    fn full_fixture() {
        let line = format!(
            "vless://{UUID}@1.2.3.4:443?type=ws&security=tls&sni=cdn.example.com&fp=chrome&host=cdn.example.com&path=%2Fws%3Fedge%3D1&encryption=none#🇯🇵 JP"
        );
        let (name, m) = parse(&line).unwrap();
        assert_eq!(name, "🇯🇵 JP");
        assert_eq!(s(&m, "type"), "vless");
        assert_eq!(s(&m, "server"), "1.2.3.4");
        assert_eq!(u(&m, "port"), 443);
        assert_eq!(s(&m, "uuid"), UUID);
        assert!(b(&m, "tls"));
        assert_eq!(s(&m, "servername"), "cdn.example.com");
        assert_eq!(s(&m, "network"), "ws");
        assert_eq!(s(&m, "client-fingerprint"), "chrome");
        let ws = nested(&m, &["ws-opts"]);
        assert_eq!(s(ws, "path"), "/ws?edge=1");
        assert_eq!(s(nested(ws, &["headers"]), "Host"), "cdn.example.com");
        assert!(b(&m, "udp"));
    }

    #[test]
    fn security_none_means_no_tls() {
        let line = format!("vless://{UUID}@1.2.3.4:443?security=none&type=tcp");
        let (_name, m) = parse(&line).unwrap();
        assert!(!b(&m, "tls"));
        assert!(m
            .get(serde_yaml::Value::String("servername".into()))
            .is_none());
        assert_eq!(s(&m, "network"), "tcp");
    }

    #[test]
    fn reality_security() {
        let line = format!("vless://{UUID}@1.2.3.4:443?security=reality&pbk=abc123&sid=def456");
        let (_name, m) = parse(&line).unwrap();
        assert!(b(&m, "tls"));
        let ro = nested(&m, &["reality-opts"]);
        assert_eq!(s(ro, "public-key"), "abc123");
        assert_eq!(s(ro, "short-id"), "def456");
    }

    #[test]
    fn grpc_network() {
        let line = format!("vless://{UUID}@1.2.3.4:443?type=grpc&serviceName=my-svc&security=tls");
        let (_name, m) = parse(&line).unwrap();
        assert_eq!(s(&m, "network"), "grpc");
        let go = nested(&m, &["grpc-opts"]);
        assert_eq!(s(go, "grpc-service-name"), "my-svc");
    }

    #[test]
    fn flow_is_preserved() {
        let line = format!("vless://{UUID}@1.2.3.4:443?flow=xtls-rprx-vision");
        let (_name, m) = parse(&line).unwrap();
        assert_eq!(s(&m, "flow"), "xtls-rprx-vision");
    }

    #[test]
    fn no_fragment_means_empty_name() {
        let line = format!("vless://{UUID}@1.2.3.4:443");
        let (name, m) = parse(&line).unwrap();
        assert_eq!(name, "");
        assert_eq!(s(&m, "type"), "vless");
    }

    #[test]
    fn invalid_link_is_error() {
        assert!(parse("vless://").is_err());
        assert!(parse("not-a-vless").is_err());
    }
}
