//! 订阅拉取/识别/解析/缓存。
//! 识别：YAML（明文或 base64 包裹）或分享链接列表；拉取：直连失败可经本地代理重试；解析：YAML 保真 + 链接分发。

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use futures_util::StreamExt;
use serde_yaml::{Mapping, Value};

use crate::core::models::{ProxyNode, SubscriptionCache};
use crate::core::parsers::{self, ParseError};

/// 订阅内容类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubscriptionKind {
    Yaml,
    ShareLinks,
}

#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    #[error("网络错误: {0}")]
    Network(String),
    #[error("HTTP {0}")]
    Http(u16),
    #[error("{0}")]
    Other(String),
}

/// 拉取订阅内容。直连失败且 via_proxy_port=Some(p) 时经 http://127.0.0.1:p 代理重试。
pub async fn fetch_subscription(url: &str, via_proxy_port: Option<u16>) -> Result<String, FetchError> {
    let client = build_client(None)?;
    match fetch_once(&client, url).await {
        Ok(body) => Ok(body),
        Err(first) => match via_proxy_port {
            Some(p) => {
                let client = build_client(Some(p))?;
                fetch_once(&client, url).await.map_err(|_| first)
            }
            None => Err(first),
        },
    }
}

fn build_client(proxy_port: Option<u16>) -> Result<reqwest::Client, FetchError> {
    let mut builder = reqwest::Client::builder()
        .user_agent("mihomo-tui/0.1")
        .timeout(Duration::from_secs(20));
    if let Some(p) = proxy_port {
        let proxy = reqwest::Proxy::all(format!("http://127.0.0.1:{p}"))
            .map_err(|e| FetchError::Other(e.to_string()))?;
        builder = builder.proxy(proxy);
    }
    builder
        .build()
        .map_err(|e| FetchError::Other(e.to_string()))
}

const MAX_BODY: usize = 10 * 1024 * 1024;

async fn fetch_once(client: &reqwest::Client, url: &str) -> Result<String, FetchError> {
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| FetchError::Network(e.to_string()))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(FetchError::Http(status.as_u16()));
    }
    let mut stream = resp.bytes_stream();
    let mut buf = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| FetchError::Network(e.to_string()))?;
        buf.extend_from_slice(&chunk);
        if buf.len() > MAX_BODY {
            return Err(FetchError::Other("订阅内容超过 10MB 上限".into()));
        }
    }
    String::from_utf8(buf).map_err(|_| FetchError::Other("订阅内容不是 UTF-8 文本".into()))
}

/// 识别订阅内容类型。
pub fn detect_kind(content: &str) -> SubscriptionKind {
    let t = content.trim();
    if t.starts_with('{') {
        return SubscriptionKind::Yaml;
    }
    if t.contains("proxies:") || t.contains("proxy-groups:") || t.contains("proxy-providers:") {
        return SubscriptionKind::Yaml;
    }
    // 整体 base64 包裹？解码后再看
    if let Some(decoded) = parsers::b64_decode(t) {
        let s = String::from_utf8_lossy(&decoded);
        for key in ["proxies:", "proxy-groups:", "port:", "mixed-port:", "dns:", "tun:"] {
            if s.contains(key) {
                return SubscriptionKind::Yaml;
            }
        }
    }
    SubscriptionKind::ShareLinks
}

/// 解析订阅内容为缓存。空输入报错。
pub fn parse_subscription(content: &str) -> Result<SubscriptionCache, ParseError> {
    if content.trim().is_empty() {
        return Err(ParseError::Message("订阅内容为空".into()));
    }
    match detect_kind(content) {
        SubscriptionKind::Yaml => parse_yaml(content),
        SubscriptionKind::ShareLinks => parse_links(content),
    }
}

fn parse_yaml(content: &str) -> Result<SubscriptionCache, ParseError> {
    let doc: Value = serde_yaml::from_str(content)
        .map_err(|e| ParseError::Message(format!("YAML 解析失败: {e}")))?;
    let map = doc
        .as_mapping()
        .ok_or_else(|| ParseError::Message("订阅不是 YAML 映射".into()))?;

    let proxies = map.get(Value::String("proxies".into()));
    let has_provider = map.get(Value::String("proxy-providers".into())).is_some();
    let proxies = match proxies {
        Some(Value::Sequence(seq)) => seq,
        Some(_) if has_provider => {
            return Err(ParseError::Message("暂不支持 proxy-providers 订阅".into()));
        }
        Some(_) => {
            return Err(ParseError::Message("订阅中 proxies 格式无效（应为节点列表）".into()));
        }
        None if has_provider => {
            return Err(ParseError::Message("暂不支持 proxy-providers 订阅".into()));
        }
        None => return Err(ParseError::Message("订阅中没有 proxies 节点".into())),
    };

    let mut nodes = Vec::new();
    for item in proxies {
        let Some(m) = item.as_mapping() else { continue };
        let Some(name) = m.get(Value::String("name".into())).and_then(|v| v.as_str()) else {
            continue; // 无 name → 跳过计数，不报错
        };
        let kind = m
            .get(Value::String("type".into()))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        nodes.push(ProxyNode {
            name: name.to_string(),
            kind,
            yaml: Value::Mapping(m.clone()),
        });
    }

    let groups: Vec<Value> = map
        .get(Value::String("proxy-groups".into()))
        .and_then(|v| v.as_sequence())
        .map(|seq| seq.to_vec())
        .unwrap_or_default();

    let rules: Vec<String> = map
        .get(Value::String("rules".into()))
        .and_then(|v| v.as_sequence())
        .map(|seq| {
            seq.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();

    Ok(SubscriptionCache {
        proxies: nodes,
        proxy_groups: groups,
        rules,
        fetched_at: chrono::Utc::now().to_rfc3339(),
    })
}

fn parse_links(content: &str) -> Result<SubscriptionCache, ParseError> {
    let effective = decode_link_content(content);
    let mut nodes = Vec::new();
    for line in effective.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Ok(node) = parse_share_link(line) {
            nodes.push(node);
        }
    }
    Ok(SubscriptionCache {
        proxies: nodes,
        proxy_groups: Vec::new(),
        rules: Vec::new(),
        fetched_at: chrono::Utc::now().to_rfc3339(),
    })
}

/// 整体 base64 包裹的链接列表：解码成功且解码后含已知 scheme 行 → 用解码内容，
/// 否则按原文逐行。先剔除空白再解码（多行 base64 常见）；明文链接因含 `:`/`/`
/// 无法通过 base64 解码，自然回落到原文路径。
fn decode_link_content<'a>(content: &'a str) -> std::borrow::Cow<'a, str> {
    let compact: String = content.chars().filter(|c| !c.is_whitespace()).collect();
    if let Some(decoded) = parsers::b64_decode(&compact) {
        let text = String::from_utf8_lossy(&decoded).into_owned();
        let has_scheme = text.lines().any(|l| {
            let t = l.trim();
            !t.is_empty() && !t.starts_with('#') && dispatch(t).is_some()
        });
        if has_scheme {
            return std::borrow::Cow::Owned(text);
        }
    }
    std::borrow::Cow::Borrowed(content)
}

static UNNAMED_COUNTER: AtomicU64 = AtomicU64::new(1);

/// 解析单条分享链接为代理节点。
/// 名称优先级：fragment（#）> 协议内名称字段 > "未命名-<n>"。
pub fn parse_share_link(line: &str) -> Result<ProxyNode, ParseError> {
    let line = line.trim();
    let (kind, parse) =
        dispatch(line).ok_or_else(|| ParseError::Message("不支持的链接格式".into()))?;
    let (name, mut mapping) = parse(line)?;
    let name = if name.is_empty() {
        format!(
            "未命名-{}",
            UNNAMED_COUNTER.fetch_add(1, Ordering::Relaxed)
        )
    } else {
        name
    };
    // 节点 yaml 统一补 udp: true（幂等）
    mapping.insert(Value::String("udp".into()), Value::Bool(true));
    Ok(ProxyNode {
        name,
        kind: kind.to_string(),
        yaml: Value::Mapping(mapping),
    })
}

type ParserFn = fn(&str) -> Result<(String, Mapping), ParseError>;

/// 按 scheme 前缀分发；返回 (kind, 解析函数)。
fn dispatch(line: &str) -> Option<(&'static str, ParserFn)> {
    const PARSERS: [(&str, &str, ParserFn); 8] = [
        ("vless://", "vless", parsers::vless::parse as ParserFn),
        ("vmess://", "vmess", parsers::vmess::parse as ParserFn),
        ("trojan://", "trojan", parsers::trojan::parse as ParserFn),
        ("ssr://", "ssr", parsers::ssr::parse as ParserFn),
        ("ss://", "ss", parsers::ss::parse as ParserFn),
        ("hysteria2://", "hysteria2", parsers::hysteria2::parse as ParserFn),
        ("hy2://", "hysteria2", parsers::hysteria2::parse as ParserFn),
        ("tuic://", "tuic", parsers::tuic::parse as ParserFn),
    ];
    PARSERS
        .iter()
        .find(|(prefix, _, _)| line.starts_with(prefix))
        .map(|(_, kind, f)| (*kind, *f))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::parsers::testutil::base64_encode;

    // ---------- detect_kind ----------

    #[test]
    fn detect_plain_yaml() {
        let yaml = "proxies:\n  - name: a\n    type: ss\n";
        assert!(matches!(detect_kind(yaml), SubscriptionKind::Yaml));
        assert!(matches!(detect_kind("proxy-groups: []\n"), SubscriptionKind::Yaml));
        assert!(matches!(detect_kind("{port: 7890}"), SubscriptionKind::Yaml));
    }

    #[test]
    fn detect_base64_yaml() {
        let yaml = "proxies:\n  - name: a\n    type: ss\n";
        let b64 = base64_encode(yaml);
        assert!(matches!(detect_kind(&b64), SubscriptionKind::Yaml));
    }

    #[test]
    fn detect_base64_links() {
        let links = "vless://uuid@1.2.3.4:443#A\ntrojan://pass@1.2.3.4:443#B\n";
        let b64 = base64_encode(links);
        assert!(matches!(detect_kind(&b64), SubscriptionKind::ShareLinks));
    }

    #[test]
    fn detect_plain_links() {
        let links = "vless://uuid@1.2.3.4:443#A\nvmess://xxx#B\n";
        assert!(matches!(detect_kind(links), SubscriptionKind::ShareLinks));
    }

    #[test]
    fn detect_empty_is_share_links() {
        assert!(matches!(detect_kind(""), SubscriptionKind::ShareLinks));
    }

    // ---------- parse_subscription: YAML ----------

    const YAML_SUB: &str = r#"
proxies:
  - name: 节点A
    type: ss
    server: 1.2.3.4
    port: 8388
    cipher: aes-128-gcm
    password: x
  - name: 节点B
    type: vless
    server: 1.2.3.4
    port: 443
  - type: trojan
    server: 1.2.3.4
    port: 443
proxy-groups:
  - name: 自动选择
    type: url-test
    proxies: [节点A, 节点B]
rules:
  - DOMAIN-SUFFIX,example.com,节点A
  - MATCH,自动选择
"#;

    #[test]
    fn parse_yaml_counts() {
        let c = parse_subscription(YAML_SUB).unwrap();
        assert_eq!(c.proxies.len(), 2); // 无 name 的节点被跳过
        assert_eq!(c.proxy_groups.len(), 1);
        assert_eq!(c.rules.len(), 2);
        assert_eq!(c.proxies[0].name, "节点A");
        assert_eq!(c.proxies[0].kind, "ss");
        assert_eq!(c.proxies[1].name, "节点B");
        assert_eq!(c.proxies[1].kind, "vless");
        assert!(!c.fetched_at.is_empty());
    }

    #[test]
    fn parse_yaml_proxy_providers_error() {
        let yaml = "proxy-providers:\n  p1:\n    url: https://x\n";
        let e = parse_subscription(yaml).unwrap_err();
        assert!(e.to_string().contains("proxy-providers"), "错误信息: {e}");
    }

    #[test]
    fn parse_yaml_no_proxies_error() {
        let e = parse_subscription("proxy-groups: []\n").unwrap_err();
        assert!(e.to_string().contains("proxies"), "错误信息: {e}");
    }

    #[test]
    fn parse_yaml_proxies_not_sequence_error() {
        let e = parse_subscription("proxies: 123\n").unwrap_err();
        assert!(e.to_string().contains("格式无效"), "错误信息: {e}");
    }

    #[test]
    fn parse_empty_yaml_error() {
        assert!(parse_subscription("").is_err());
    }

    // ---------- parse_subscription: ShareLinks ----------

    #[test]
    fn parse_share_links_lines() {
        let content =
            "vless://uuid@1.2.3.4:443#A\n\n# 注释行\ntrojan://pass@1.2.3.4:443#B\n";
        let c = parse_subscription(content).unwrap();
        assert_eq!(c.proxies.len(), 2);
        assert_eq!(c.proxies[0].name, "A");
        assert_eq!(c.proxies[0].kind, "vless");
        assert_eq!(c.proxies[1].name, "B");
        assert_eq!(c.proxies[1].kind, "trojan");
        assert!(c.proxy_groups.is_empty());
        assert!(c.rules.is_empty());
    }

    #[test]
    fn parse_share_links_bad_lines_skipped() {
        let content = "not-a-link\nvmess://!!!bad!!!\nvless://uuid@1.2.3.4:443#C\n";
        let c = parse_subscription(content).unwrap();
        assert_eq!(c.proxies.len(), 1);
        assert_eq!(c.proxies[0].name, "C");
    }

    #[test]
    fn parse_base64_wrapped_share_links() {
        let links =
            "ss://YWVzLTEyOC1nY206cGFzc0AxLjIuMy40OjgzODg=#节点A\nss://YWVzLTEyOC1nY206cGFzc0AxLjIuMy40OjgzODg=#节点B\n";
        let b64 = base64_encode(links);
        let c = parse_subscription(&b64).unwrap();
        assert_eq!(c.proxies.len(), 2, "base64 包裹的分享链接应解析出节点");
        assert_eq!(c.proxies[0].name, "节点A");
        assert_eq!(c.proxies[0].kind, "ss");
        assert_eq!(c.proxies[1].name, "节点B");
        assert_eq!(c.proxies[1].kind, "ss");
    }

    #[test]
    fn parse_base64_wrapped_links_with_newlines() {
        // 多行 base64（带换行）也应解码成功
        let links = "vless://uuid@1.2.3.4:443#X\ntrojan://pass@1.2.3.4:443#Y\n";
        let b64 = base64_encode(links);
        let wrapped = format!("\n{}\n", b64);
        let c = parse_subscription(&wrapped).unwrap();
        assert_eq!(c.proxies.len(), 2);
        assert_eq!(c.proxies[0].name, "X");
        assert_eq!(c.proxies[1].name, "Y");
    }

    #[test]
    fn plain_links_still_parse_after_decode_attempt() {
        // 明文行不会被误判为 base64（含 : 无法解码），回落到原文解析
        let content = "ss://YWVzLTEyOC1nY206cGFzc0AxLjIuMy40OjgzODg=#P\ntrojan://pass@1.2.3.4:443#Q\n";
        let c = parse_subscription(content).unwrap();
        assert_eq!(c.proxies.len(), 2);
        assert_eq!(c.proxies[0].name, "P");
        assert_eq!(c.proxies[1].name, "Q");
    }

    // ---------- parse_share_link ----------

    #[test]
    fn share_link_name_priority_and_udp() {
        let vmess_json = |ps: &str| {
            format!(
                r#"{{"add":"1.2.3.4","port":"443","id":"uuid-1","ps":"{ps}"}}"#
            )
        };
        // fragment 优先于协议内名称
        let n = parse_share_link(&format!(
            "vmess://{}#frag名",
            base64_encode(&vmess_json("ps节点一"))
        ))
        .unwrap();
        assert_eq!(n.name, "frag名");
        assert_eq!(n.kind, "vmess");
        assert!(n.yaml.get("udp").and_then(|v| v.as_bool()) == Some(true));

        // 无 fragment → 协议内名称（vmess ps）
        let n = parse_share_link(&format!("vmess://{}", base64_encode(&vmess_json("ps节点一"))))
            .unwrap();
        assert_eq!(n.name, "ps节点一");
        assert_eq!(n.kind, "vmess");
    }

    #[test]
    fn share_link_unnamed_fallback() {
        let n = parse_share_link("ss://YWVzLTEyOC1nY206cGFzc0AxLjIuMy40OjgzODg=").unwrap();
        assert!(n.name.starts_with("未命名-"), "名称: {}", n.name);
        assert_eq!(n.kind, "ss");
    }

    #[test]
    fn share_link_bad_line_error() {
        assert!(parse_share_link("garbage").is_err());
    }

    // ---------- fetch_subscription ----------

    async fn spawn_http_server(body: &'static str, status: &'static str) -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else { break };
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = [0u8; 8192];
                    // 读到请求头结束
                    let mut seen = 0usize;
                    loop {
                        match sock.read(&mut buf[seen..]).await {
                            Ok(0) => break,
                            Ok(n) => {
                                seen += n;
                                if buf[..seen].windows(4).any(|w| w == b"\r\n\r\n") {
                                    break;
                                }
                            }
                            Err(_) => break,
                        }
                    }
                    let _ = sock
                        .write_all(
                            format!(
                                "{status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                                body.len()
                            )
                            .as_bytes(),
                        )
                        .await;
                });
            }
        });
        port
    }

    fn closed_port() -> u16 {
        // 绑定后立即释放，端口大概率保持关闭
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    }

    #[tokio::test]
    async fn fetch_ok() {
        let port = spawn_http_server("hello 订阅内容", "HTTP/1.1 200 OK").await;
        let body = fetch_subscription(&format!("http://127.0.0.1:{port}/sub"), None)
            .await
            .unwrap();
        assert_eq!(body, "hello 订阅内容");
    }

    #[tokio::test]
    async fn fetch_http_error() {
        let port = spawn_http_server("nope", "HTTP/1.1 404 Not Found").await;
        let e = fetch_subscription(&format!("http://127.0.0.1:{port}/sub"), None)
            .await
            .unwrap_err();
        assert!(matches!(e, FetchError::Http(404)), "错误: {e}");
    }

    #[tokio::test]
    async fn fetch_proxy_retry_when_direct_fails() {
        let proxy_port = spawn_http_server("via-proxy", "HTTP/1.1 200 OK").await;
        let target = format!("http://127.0.0.1:{}/sub", closed_port());
        let body = fetch_subscription(&target, Some(proxy_port)).await.unwrap();
        assert_eq!(body, "via-proxy");
    }

    #[tokio::test]
    async fn fetch_network_error_without_proxy() {
        let target = format!("http://127.0.0.1:{}/sub", closed_port());
        let e = fetch_subscription(&target, None).await.unwrap_err();
        assert!(matches!(e, FetchError::Network(_)), "错误: {e}");
    }

    #[tokio::test]
    async fn fetch_too_large_rejected() {
        let big = "x".repeat(11 * 1024 * 1024);
        let port = spawn_http_server(Box::leak(big.into_boxed_str()), "HTTP/1.1 200 OK").await;
        let e = fetch_subscription(&format!("http://127.0.0.1:{port}/sub"), None)
            .await
            .unwrap_err();
        assert!(matches!(e, FetchError::Other(_)), "错误: {e}");
    }
}
