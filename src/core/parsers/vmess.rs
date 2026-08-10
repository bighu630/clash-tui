//! vmess 分享链接解析器（v2rayN JSON 格式，base64 编码）。
//! 输出字段遵循 mihomo 配置格式（见 plans §4 fixtures）。

use serde_yaml::{Mapping, Value};

use crate::core::parsers::{b64_decode, err, pct_decode, ParseError};

/// 解析 vmess 链接，返回 (名称, yaml 映射)。
/// 名称优先级：fragment（#）> JSON 内 ps 字段。
pub fn parse(line: &str) -> Result<(String, Mapping), ParseError> {
    let body = line
        .strip_prefix("vmess://")
        .ok_or_else(|| err("不是 vmess 链接"))?;
    let (body, frag) = match body.split_once('#') {
        Some((b, f)) => (b, Some(f)),
        None => (body, None),
    };
    // 部分客户端会在 base64 后追加 ?ed=2048 等查询参数
    let b64part = body.split('?').next().unwrap_or(body);
    let decoded = b64_decode(b64part).ok_or_else(|| err("vmess base64 解码失败"))?;
    let json: serde_json::Value = serde_json::from_slice(&decoded)
        .map_err(|e| err(format!("vmess 配置无效: {e}")))?;

    let get = |k: &str| json.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();

    let name = pct_decode(frag.unwrap_or(""));
    let name = if name.is_empty() { get("ps") } else { name };

    let mut m = Mapping::new();
    m.insert(Value::String("type".into()), Value::String("vmess".into()));
    m.insert(Value::String("server".into()), Value::String(get("add")));
    let port: u16 = get("port")
        .parse()
        .map_err(|_| err(format!("vmess 端口无效: {}", get("port"))))?;
    m.insert(Value::String("port".into()), Value::Number(port.into()));
    m.insert(Value::String("uuid".into()), Value::String(get("id")));
    let aid: u64 = get("aid").parse().unwrap_or(0);
    m.insert(Value::String("alterId".into()), Value::Number(aid.into()));
    let cipher = get("scy");
    m.insert(
        Value::String("cipher".into()),
        Value::String(if cipher.is_empty() { "auto".into() } else { cipher }),
    );
    m.insert(Value::String("udp".into()), Value::Bool(true));

    let tls = get("tls");
    m.insert(
        Value::String("tls".into()),
        Value::Bool(tls == "tls" || tls == "true"),
    );
    let sni = get("sni");
    if !sni.is_empty() {
        m.insert(Value::String("servername".into()), Value::String(sni));
    }
    let net = get("net");
    if !net.is_empty() {
        m.insert(Value::String("network".into()), Value::String(net.clone()));
        let path = get("path");
        let host = get("host");
        if net == "ws" && (!path.is_empty() || !host.is_empty()) {
            let mut opts = Mapping::new();
            if !path.is_empty() {
                opts.insert(Value::String("path".into()), Value::String(path));
            }
            if !host.is_empty() {
                let mut headers = Mapping::new();
                headers.insert(Value::String("Host".into()), Value::String(host));
                opts.insert(Value::String("headers".into()), Value::Mapping(headers));
            }
            m.insert(Value::String("ws-opts".into()), Value::Mapping(opts));
        } else if net == "grpc" && !path.is_empty() {
            let mut opts = Mapping::new();
            opts.insert(
                Value::String("grpc-service-name".into()),
                Value::String(path),
            );
            m.insert(Value::String("grpc-opts".into()), Value::Mapping(opts));
        }
    }
    let fp = get("fp");
    if !fp.is_empty() {
        m.insert(
            Value::String("client-fingerprint".into()),
            Value::String(fp),
        );
    }
    let alpn = get("alpn");
    if !alpn.is_empty() {
        m.insert(
            Value::String("alpn".into()),
            Value::Sequence(
                alpn.split(',').map(|s| Value::String(s.trim().to_string())).collect(),
            ),
        );
    }

    Ok((name, m))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::parsers::testutil::{b, base64_encode, nested, s, seq_str, u};

    const UUID: &str = "9f5f2e5c-1b2c-3d4e-5f6a-7b8c9d0e1f2a";

    fn fixture_json() -> String {
        format!(
            r#"{{"v":"2","ps":"测试节点","add":"1.2.3.4","port":"443","id":"{UUID}","aid":"0","scy":"auto","net":"ws","type":"none","host":"h.example.com","path":"/ws","tls":"tls","sni":"s.example.com","alpn":"h2,http/1.1","fp":"chrome"}}"#
        )
    }

    #[test]
    fn full_fixture() {
        let line = format!("vmess://{}", base64_encode(&fixture_json()));
        let (name, m) = parse(&line).unwrap();
        assert_eq!(name, "测试节点");
        assert_eq!(s(&m, "type"), "vmess");
        assert_eq!(s(&m, "server"), "1.2.3.4");
        assert_eq!(u(&m, "port"), 443);
        assert_eq!(s(&m, "uuid"), UUID);
        assert_eq!(u(&m, "alterId"), 0);
        assert_eq!(s(&m, "cipher"), "auto");
        assert!(b(&m, "udp"));
        assert!(b(&m, "tls"));
        assert_eq!(s(&m, "servername"), "s.example.com");
        assert_eq!(s(&m, "network"), "ws");
        assert_eq!(s(&m, "client-fingerprint"), "chrome");
        assert_eq!(seq_str(&m, "alpn"), vec!["h2", "http/1.1"]);
        let ws = nested(&m, &["ws-opts"]);
        assert_eq!(s(ws, "path"), "/ws");
        assert_eq!(s(nested(ws, &["headers"]), "Host"), "h.example.com");
    }

    #[test]
    fn tls_none_and_tcp() {
        let json = fixture_json()
            .replace("\"tls\":\"tls\"", "\"tls\":\"none\"")
            .replace("\"net\":\"ws\"", "\"net\":\"tcp\"")
            .replace("\"sni\":\"s.example.com\",", "");
        let line = format!("vmess://{}", base64_encode(&json));
        let (_name, m) = parse(&line).unwrap();
        assert!(!b(&m, "tls"));
        assert_eq!(s(&m, "network"), "tcp");
        assert!(m.get(serde_yaml::Value::String("ws-opts".into())).is_none());
        assert!(m.get(serde_yaml::Value::String("servername".into())).is_none());
    }

    #[test]
    fn grpc_network() {
        let json = fixture_json().replace("\"net\":\"ws\"", "\"net\":\"grpc\"");
        let line = format!("vmess://{}", base64_encode(&json));
        let (_name, m) = parse(&line).unwrap();
        assert_eq!(s(&m, "network"), "grpc");
        let go = nested(&m, &["grpc-opts"]);
        assert_eq!(s(go, "grpc-service-name"), "/ws");
    }

    #[test]
    fn name_from_fragment_overrides_ps() {
        let line = format!("vmess://{}#别名", base64_encode(&fixture_json()));
        let (name, _m) = parse(&line).unwrap();
        assert_eq!(name, "别名");
    }

    #[test]
    fn no_name_no_ps() {
        let json = fixture_json().replace("\"ps\":\"测试节点\",", "");
        let line = format!("vmess://{}", base64_encode(&json));
        let (name, _m) = parse(&line).unwrap();
        assert_eq!(name, "");
    }

    #[test]
    fn invalid_base64_is_error() {
        assert!(parse("vmess://!!!not-base64!!!").is_err());
    }
}
