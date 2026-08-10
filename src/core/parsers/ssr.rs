//! shadowsocksr 分享链接解析器。
//! 格式：ssr://base64(host:port:protocol:cipher:obfs:base64(pass)/?params)。见 plans §4。

use serde_yaml::{Mapping, Value};

use crate::core::parsers::{b64_decode, err, parse_query, pct_decode, ParseError};

/// 解析 ssr 链接，返回 (名称, yaml 映射)。
/// 名称优先级：fragment（#）> remarks 参数。
pub fn parse(line: &str) -> Result<(String, Mapping), ParseError> {
    let body = line.strip_prefix("ssr://").ok_or_else(|| err("不是 ssr 链接"))?;
    let (body, frag) = match body.split_once('#') {
        Some((b, f)) => (b, Some(f)),
        None => (body, None),
    };
    let frag = pct_decode(frag.unwrap_or(""));

    let decoded = b64_decode(body).ok_or_else(|| err("ssr base64 解码失败"))?;
    let s = String::from_utf8_lossy(&decoded).into_owned();
    let (core, query) = match s.split_once('?') {
        Some((c, q)) => (c.trim_end_matches('/').to_string(), Some(q.to_string())),
        None => (s.clone(), None),
    };

    let parts: Vec<&str> = core.split(':').collect();
    if parts.len() < 6 {
        return Err(err("ssr 配置无效"));
    }
    let server = parts[0].to_string();
    let port: u16 = parts[1]
        .parse()
        .map_err(|_| err(format!("ssr 端口无效: {}", parts[1])))?;
    let protocol = parts[2].to_string();
    let cipher = parts[3].to_string();
    let obfs = parts[4].to_string();
    // 第 6 段为 base64(password)
    let pass_b64 = parts[5..].join(":");
    let password = b64_decode(&pass_b64)
        .map(|v| String::from_utf8_lossy(&v).into_owned())
        .unwrap_or_default();

    // 参数值：先尝试 base64 解码，失败则按 percent-decode 原值
    let mut params: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for (k, v) in parse_query(query.as_deref()) {
        let val = b64_decode(&v)
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_else(|| v.clone());
        params.insert(k, val);
    }
    let param = |k: &str| params.get(k).cloned().unwrap_or_default();

    let name = if frag.is_empty() {
        param("remarks")
    } else {
        frag
    };

    let mut m = Mapping::new();
    m.insert(Value::String("type".into()), Value::String("ssr".into()));
    m.insert(Value::String("server".into()), Value::String(server));
    m.insert(Value::String("port".into()), Value::Number(port.into()));
    m.insert(Value::String("cipher".into()), Value::String(cipher));
    m.insert(Value::String("password".into()), Value::String(password));
    m.insert(Value::String("protocol".into()), Value::String(protocol));
    m.insert(Value::String("obfs".into()), Value::String(obfs));
    m.insert(Value::String("protocol-param".into()), Value::String(param("protoparam")));
    m.insert(Value::String("obfs-param".into()), Value::String(param("obfsparam")));
    m.insert(Value::String("udp".into()), Value::Bool(true));

    Ok((name, m))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::parsers::testutil::{b, base64_encode, s, u};

    fn ssr_link() -> String {
        let core = format!(
            "1.2.3.4:8388:auth_aes128_md5:chacha20-ietf:tls1.2_ticket_auth:{}",
            base64_encode("pass")
        );
        let params = format!(
            "obfsparam={}&protoparam=&remarks={}&group={}",
            base64_encode("http_post"),
            base64_encode("SSR节点"),
            base64_encode("g")
        );
        format!("ssr://{}", base64_encode(&format!("{core}/?{params}")))
    }

    #[test]
    fn full_fixture() {
        let line = ssr_link();
        let (name, m) = parse(&line).unwrap();
        assert_eq!(name, "SSR节点");
        assert_eq!(s(&m, "type"), "ssr");
        assert_eq!(s(&m, "server"), "1.2.3.4");
        assert_eq!(u(&m, "port"), 8388);
        assert_eq!(s(&m, "cipher"), "chacha20-ietf");
        assert_eq!(s(&m, "password"), "pass");
        assert_eq!(s(&m, "protocol"), "auth_aes128_md5");
        assert_eq!(s(&m, "obfs"), "tls1.2_ticket_auth");
        assert_eq!(s(&m, "protocol-param"), "");
        assert_eq!(s(&m, "obfs-param"), "http_post");
        assert!(b(&m, "udp"));
    }

    #[test]
    fn fragment_overrides_remarks() {
        let line = format!("{}#frag名称", ssr_link());
        let (name, _m) = parse(&line).unwrap();
        assert_eq!(name, "frag名称");
    }

    #[test]
    fn invalid_link_is_error() {
        assert!(parse("ssr://").is_err());
        assert!(parse("ssr://!!!").is_err());
    }
}
