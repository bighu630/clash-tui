//! shadowsocks 分享链接解析器：新格式（base64 用户信息 + 显式 host:port）
//! 与旧格式（整体 base64），支持 plugin 混淆。见 plans §4。

use serde_yaml::{Mapping, Value};

use crate::core::parsers::{b64_decode, err, parse_query, pct_decode, split_host_port, ParseError};

/// 解析 ss 链接，返回 (名称, yaml 映射)。名称来自 fragment（#）。
pub fn parse(line: &str) -> Result<(String, Mapping), ParseError> {
    let body = line
        .strip_prefix("ss://")
        .ok_or_else(|| err("不是 ss 链接"))?;
    let (body, frag) = match body.split_once('#') {
        Some((b, f)) => (b, Some(f)),
        None => (body, None),
    };
    let name = pct_decode(frag.unwrap_or(""));
    let (main, query) = match body.split_once('?') {
        Some((m, q)) => (m, Some(q)),
        None => (body, None),
    };

    let (method, password, server, port) = if let Some((userinfo, hostport)) = main.split_once('@')
    {
        // 新格式：userinfo 为 base64(method:password)
        let (method, password) = decode_userinfo(userinfo)?;
        let (server, port) = split_host_port(hostport)?;
        (method, password, server, port)
    } else {
        // 旧格式：整体为 base64(method:password@host:port)，内部为明文 method:pass
        let decoded = b64_decode(main).ok_or_else(|| err("ss base64 解码失败"))?;
        let s = String::from_utf8_lossy(&decoded).into_owned();
        let (userinfo, hostport) = s.split_once('@').ok_or_else(|| err("ss 旧格式无效"))?;
        let (method, password) = split_userinfo(userinfo)?;
        let (server, port) = split_host_port(hostport)?;
        (method, password, server, port)
    };

    let mut m = Mapping::new();
    m.insert(Value::String("type".into()), Value::String("ss".into()));
    m.insert(Value::String("server".into()), Value::String(server));
    m.insert(Value::String("port".into()), Value::Number(port.into()));
    m.insert(Value::String("cipher".into()), Value::String(method));
    m.insert(Value::String("password".into()), Value::String(password));
    m.insert(Value::String("udp".into()), Value::Bool(true));

    if let Some(plugin) = parse_query(query)
        .into_iter()
        .find(|(k, _)| k == "plugin")
        .map(|(_, v)| v)
    {
        if let Some((name, opts)) = plugin.split_once(';') {
            m.insert(
                Value::String("plugin".into()),
                Value::String(name.to_string()),
            );
            m.insert(
                Value::String("plugin-opts".into()),
                Value::String(opts.to_string()),
            );
        } else if !plugin.is_empty() {
            m.insert(Value::String("plugin".into()), Value::String(plugin));
        }
    }

    Ok((name, m))
}

/// 解码 base64(method:password)（新格式 userinfo）。
fn decode_userinfo(s: &str) -> Result<(String, String), ParseError> {
    let decoded = b64_decode(s).ok_or_else(|| err("ss 用户信息 base64 解码失败"))?;
    let s = String::from_utf8_lossy(&decoded).into_owned();
    split_userinfo(&s)
}

/// 拆分明文 method:password。
fn split_userinfo(s: &str) -> Result<(String, String), ParseError> {
    let (method, password) = s.split_once(':').ok_or_else(|| err("ss 用户信息无效"))?;
    Ok((method.to_string(), password.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::parsers::testutil::{b, base64_encode, s, u};

    #[test]
    fn new_format() {
        let line = format!(
            "ss://{}@1.2.3.4:8388#SS节点",
            base64_encode("aes-128-gcm:pass123")
        );
        let (name, m) = parse(&line).unwrap();
        assert_eq!(name, "SS节点");
        assert_eq!(s(&m, "type"), "ss");
        assert_eq!(s(&m, "server"), "1.2.3.4");
        assert_eq!(u(&m, "port"), 8388);
        assert_eq!(s(&m, "cipher"), "aes-128-gcm");
        assert_eq!(s(&m, "password"), "pass123");
        assert!(b(&m, "udp"));
    }

    #[test]
    fn old_format() {
        let line = format!("ss://{}", base64_encode("aes-128-gcm:pass123@1.2.3.4:8388"));
        let (name, m) = parse(&line).unwrap();
        assert_eq!(name, "");
        assert_eq!(s(&m, "type"), "ss");
        assert_eq!(s(&m, "server"), "1.2.3.4");
        assert_eq!(u(&m, "port"), 8388);
        assert_eq!(s(&m, "cipher"), "aes-128-gcm");
        assert_eq!(s(&m, "password"), "pass123");
    }

    #[test]
    fn plugin() {
        let line = format!(
            "ss://{}@1.2.3.4:8388?plugin=obfs-local%3Bobfs%3Dhttp%3Bobfs-host%3Dcdn.example.com#P",
            base64_encode("aes-128-gcm:pass123")
        );
        let (_name, m) = parse(&line).unwrap();
        assert_eq!(s(&m, "plugin"), "obfs-local");
        assert_eq!(s(&m, "plugin-opts"), "obfs=http;obfs-host=cdn.example.com");
    }

    #[test]
    fn invalid_link_is_error() {
        assert!(parse("ss://").is_err());
        assert!(parse("ss://!!@1.2.3.4:8388").is_err());
    }
}
