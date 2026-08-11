//! 出口 IP 探测：经 mihomo 代理端口请求外部回显服务。
//! 多代理端口（mixed/http/socks5）× 多回显端点降级；解析器纯函数可单测。

use std::time::Duration;

use crate::core::models::NetworkSettings;

/// 代理候选端口集合（0 表示未启用）。
#[derive(Clone)]
pub struct ProxyPorts {
    pub mixed: u16,
    pub http: u16,
    pub socks: u16,
}

impl ProxyPorts {
    pub fn from_settings(s: &NetworkSettings) -> Self {
        Self {
            mixed: s.mixed_port,
            http: s.port,
            socks: s.socks_port,
        }
    }

    /// 按 mixed(HTTP)→http(HTTP)→socks(SOCKS5) 顺序生成候选；
    /// 值为 0 的端口跳过；端口重复时去重（保留先出现的 scheme）。
    fn candidates(&self) -> Vec<(u16, Scheme)> {
        let mut out: Vec<(u16, Scheme)> = Vec::new();
        for (port, scheme) in [
            (self.mixed, Scheme::Http),
            (self.http, Scheme::Http),
            (self.socks, Scheme::Socks5),
        ] {
            if port != 0 && !out.iter().any(|(p, _)| *p == port) {
                out.push((port, scheme));
            }
        }
        out
    }
}

/// 代理协议（决定 client 的 proxy URL scheme）。
#[derive(Clone, Copy, PartialEq, Debug)]
enum Scheme {
    Http,
    Socks5,
}

impl std::fmt::Display for Scheme {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Scheme::Http => write!(f, "HTTP"),
            Scheme::Socks5 => write!(f, "SOCKS5"),
        }
    }
}

/// 端点表（顺序即优先级；Trace/Ipip 为特殊解析格式）。
const ENDPOINTS: &[(&str, ParseMode)] = &[
    ("https://www.cloudflare.com/cdn-cgi/trace", ParseMode::Trace),
    ("https://api.ipify.org", ParseMode::Plain),
    ("https://ipv4.icanhazip.com", ParseMode::Plain),
    ("https://checkip.amazonaws.com", ParseMode::Plain),
    ("https://ipinfo.io/ip", ParseMode::Plain),
    ("http://api.ipify.org", ParseMode::Plain),
    ("http://ipv4.icanhazip.com", ParseMode::Plain),
    ("http://members.3322.org/dyndns/getip", ParseMode::Plain),
    ("https://ifconfig.me/ip", ParseMode::Plain),
    ("http://myip.ipip.net", ParseMode::Ipip),
];

#[derive(Clone, Copy)]
enum ParseMode {
    Plain,
    Trace,
    Ipip,
}

/// 逐候选端口探测：每端口构建独立 Client（单请求 timeout 8s，单端口总预算 30s），
/// 依次请求端点，成功解析出 IP 立即返回；全部失败返回聚合错误（每端口一行摘要）。
pub async fn fetch_exit_ip(ports: &ProxyPorts) -> Result<String, String> {
    let candidates = ports.candidates();
    if candidates.is_empty() {
        return Err("没有可用的代理端口".to_string());
    }
    let mut summaries: Vec<String> = Vec::new();
    for (port, scheme) in candidates {
        let proxy_url = match scheme {
            Scheme::Http => format!("http://127.0.0.1:{port}"),
            // socks5h：域名由远端（mihomo）解析，与 HTTP 候选语义一致
            Scheme::Socks5 => format!("socks5h://127.0.0.1:{port}"),
        };
        // 单端口总预算 30s（含 client 构建与全部端点尝试）：超时按该端口
        // "服务全失败（超时）" 计入 summaries，继续下一端口。
        let result = tokio::time::timeout(Duration::from_secs(30), async {
            // client 构建失败（Proxy::all 解析失败 / builder 错误）不短路整个
            // 函数，作为该端口失败并入 summaries。
            let client = reqwest::Client::builder()
                .proxy(reqwest::Proxy::all(&proxy_url).map_err(|e| e.to_string())?)
                .timeout(Duration::from_secs(8))
                .build()
                .map_err(|e| e.to_string())?;
            let mut last_err = String::new();
            for (url, mode) in ENDPOINTS {
                match fetch_one(&client, url, *mode).await {
                    Ok(ip) => return Ok(ip),
                    Err(e) => last_err = e,
                }
            }
            Err(last_err)
        })
        .await;
        match result {
            Ok(Ok(ip)) => return Ok(ip),
            Ok(Err(last_err)) => summaries.push(format!(
                "127.0.0.1:{port}({scheme}) 服务全失败（最后错误: {last_err}）"
            )),
            Err(_) => summaries.push(format!("127.0.0.1:{port}({scheme}) 服务全失败（超时）")),
        }
    }
    Err(format!("出口 IP 获取失败: {}", summaries.join("; ")))
}

/// 单端点请求 + 按模式解析。
async fn fetch_one(client: &reqwest::Client, url: &str, mode: ParseMode) -> Result<String, String> {
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("{url}: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("{url} HTTP {}", resp.status()));
    }
    let text = resp
        .text()
        .await
        .map_err(|e| format!("{url}: {e}"))?;
    let ip = match mode {
        ParseMode::Plain => parse_plain(&text),
        ParseMode::Trace => parse_trace(&text),
        ParseMode::Ipip => parse_ipip(&text),
    }
    .ok_or_else(|| format!("{url} 内容不含有效 IP"))?;
    Ok(ip)
}

/// 纯函数解析：trim 后要求符合 IP 形态（非空、≤45 字符、无空白、
/// 仅 [0-9a-fA-F:.%]）；含 '.' 按 IPv4 严格校验（4 段十进制 0-255），
/// 含 ':' 按 IPv6 松校验（hex 组 + "::" 压缩）；否则 None。
pub fn parse_plain(text: &str) -> Option<String> {
    let s = text.trim();
    if looks_like_ip(s) {
        Some(s.to_string())
    } else {
        None
    }
}

/// cloudflare /cdn-cgi/trace：逐行找 "ip=" 前缀行，值经 parse_plain。
pub fn parse_trace(text: &str) -> Option<String> {
    text.lines()
        .find_map(|line| line.strip_prefix("ip=").and_then(parse_plain))
}

/// myip.ipip.net 中文文本（"当前 IP：1.2.3.4 来自于：..."）：
/// 扫描连续 [0-9.] 片段，逐个按 IPv4 校验。
pub fn parse_ipip(text: &str) -> Option<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i].is_ascii_digit() || chars[i] == '.' {
            let mut j = i;
            while j < chars.len() && (chars[j].is_ascii_digit() || chars[j] == '.') {
                j += 1;
            }
            let frag: String = chars[i..j].iter().collect();
            if looks_like_ip(&frag) {
                return Some(frag);
            }
            i = j;
        } else {
            i += 1;
        }
    }
    None
}

/// 供 parse_plain 内部复用（公开便于测试）。
pub fn looks_like_ip(s: &str) -> bool {
    if s.is_empty() || s.len() > 45 {
        return false;
    }
    if !s
        .chars()
        .all(|c| c.is_ascii_hexdigit() || c == ':' || c == '.' || c == '%')
    {
        return false;
    }
    if s.contains(':') {
        is_ipv6_loose(s)
    } else if s.contains('.') {
        is_ipv4_strict(s)
    } else {
        false
    }
}

/// IPv4 严格校验：恰好 4 段十进制，每段 0-255。
fn is_ipv4_strict(s: &str) -> bool {
    let parts: Vec<&str> = s.split('.').collect();
    parts.len() == 4
        && parts.iter().all(|p| {
            !p.is_empty()
                && p.len() <= 3
                && p.chars().all(|c| c.is_ascii_digit())
                && p.parse::<u16>().is_ok_and(|v| v <= 255)
        })
}

/// IPv6 松校验：hex 组（1-4 位十六进制），允许单个 "::" 压缩
/// （空组须相邻，全冒号串仅 "::" 合法）。
/// % zone id 不真正支持：校验前按 '%' 截断仅看主地址部分，且外层字符过滤
/// 只放行 hex digit 与 ':' '.' '%'，故 % 后仅纯 hex 或空后缀（如 %25）能
/// 通过，真实 zone id（如 %eth0）会被字符过滤拒绝。
fn is_ipv6_loose(s: &str) -> bool {
    let addr = s.split('%').next().unwrap_or(s);
    if addr.is_empty() {
        return false;
    }
    let groups: Vec<&str> = addr.split(':').collect();
    // 组数判据：非空组数 + (存在空组 ? 1 : 0) ≤ 8。不能直接用 split(':') 的
    // 元素数判组数——"1:2:3:4:5:6:7::" 拆出 9 个元素，但实际是 7 段 + "::"
    // 压缩 1 段 = 8 组，合法；而 "1::2:3:4:5:6:7:8" 为 8 段 + "::" = 9 组，非法。
    let non_empty = groups.iter().filter(|g| !g.is_empty()).count();
    if non_empty + usize::from(groups.iter().any(|g| g.is_empty())) > 8 {
        return false;
    }
    // 全冒号串仅 "::" 合法（":"、":::" 等拒绝）
    if addr.chars().all(|c| c == ':') && addr != "::" {
        return false;
    }
    let empties: Vec<usize> = groups
        .iter()
        .enumerate()
        .filter(|(_, g)| g.is_empty())
        .map(|(i, _)| i)
        .collect();
    // "::" 压缩产生 1~2 个空组（"::1"→[0,1]、"1::"→[1,2]、"2001:db8::1"→[2]）；
    // 空组须相邻，否则形如 "1::2::3" 拒绝。
    // 裸 "::" 拆出 3 个空组（[0,1,2]）> 2 在此被拒：未指定地址，不视为合法 IP。
    if empties.len() > 2 {
        return false;
    }
    if empties.len() == 2 && empties[1] != empties[0] + 1 {
        return false;
    }
    groups
        .iter()
        .all(|g| g.is_empty() || (g.len() <= 4 && g.chars().all(|c| c.is_ascii_hexdigit())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_plain_ok() {
        assert_eq!(parse_plain("1.2.3.4"), Some("1.2.3.4".to_string()));
        assert_eq!(parse_plain(" 1.2.3.4\n"), Some("1.2.3.4".to_string()));
        assert_eq!(parse_plain("::1"), Some("::1".to_string()));
        assert_eq!(parse_plain("2001:db8::1"), Some("2001:db8::1".to_string()));
        assert_eq!(parse_plain("fe80::1%25"), Some("fe80::1%25".to_string()));
        assert_eq!(parse_plain("0.0.0.0"), Some("0.0.0.0".to_string()));
        assert_eq!(parse_plain("255.255.255.255"), Some("255.255.255.255".to_string()));
    }

    #[test]
    fn parse_plain_none() {
        assert_eq!(parse_plain(""), None);
        assert_eq!(parse_plain("   "), None);
        assert_eq!(parse_plain("abc"), None);
        assert_eq!(parse_plain("1.2.3.4a"), None);
        assert_eq!(parse_plain("1.2.3.999"), None); // 越界
        assert_eq!(parse_plain("1.2.3"), None); // 段数不足
        assert_eq!(parse_plain("1.2.3.4.5"), None); // 段数过多
        assert_eq!(parse_plain("1.2.3.4 "), Some("1.2.3.4".to_string())); // trim 后合法
        assert_eq!(parse_plain("256.1.1.1"), None); // 越界
    }

    #[test]
    fn ipv6_loose_group_count_uses_compression() {
        // split(':') 元素数 ≠ 组数："1:2:3:4:5:6:7::" 拆出 9 个元素，但为
        // 7 段 + "::" 压缩 1 段 = 8 组，合法。
        assert!(is_ipv6_loose("1:2:3:4:5:6:7::"));
        assert!(is_ipv6_loose("::1:2:3:4:5:6:7"));
        assert!(is_ipv6_loose("1:2:3:4:5:6:7:8"));
        assert!(is_ipv6_loose("1::"));
        assert!(is_ipv6_loose("2001:db8::1"));
        // 8 段 + "::" 压缩 = 9 组，超出上限，非法
        assert!(!is_ipv6_loose("1::2:3:4:5:6:7:8"));
        assert!(!is_ipv6_loose("1:2:3:4:5:6:7:8::"));
        // 裸 "::"（未指定地址）仍拒绝
        assert!(!is_ipv6_loose("::"));
        // 多个 "::" 拒绝
        assert!(!is_ipv6_loose("1::2::3"));
        // 非法 hex 组拒绝
        assert!(!is_ipv6_loose("1:2:3:4:5:6:7:gg"));
    }

    #[test]
    fn parse_trace_hits_ip_line() {
        let text = "fl=20f\nh=www.cloudflare.com\nip=103.151.172.89\nts=1710000000.000\n";
        assert_eq!(parse_trace(text), Some("103.151.172.89".to_string()));
    }

    #[test]
    fn parse_trace_missing_or_bad_ip() {
        assert_eq!(parse_trace("fl=20f\nh=www.cloudflare.com\n"), None);
        assert_eq!(parse_trace("ip=foo\nh=www.cloudflare.com\n"), None);
        assert_eq!(parse_trace(""), None);
    }

    #[test]
    fn parse_ipip_hits_chinese_text() {
        let text = "当前 IP：1.2.3.4  来自于：中国 电信";
        assert_eq!(parse_ipip(text), Some("1.2.3.4".to_string()));
    }

    #[test]
    fn parse_ipip_no_ip() {
        assert_eq!(parse_ipip("当前 IP：未知  来自于：中国 电信"), None);
        assert_eq!(parse_ipip(""), None);
    }

    #[test]
    fn candidates_default_order() {
        let ports = ProxyPorts {
            mixed: 7892,
            http: 7890,
            socks: 7891,
        };
        assert_eq!(
            ports.candidates(),
            vec![(7892, Scheme::Http), (7890, Scheme::Http), (7891, Scheme::Socks5)]
        );
    }

    #[test]
    fn candidates_skips_zero_ports() {
        let ports = ProxyPorts {
            mixed: 0,
            http: 7890,
            socks: 7891,
        };
        assert_eq!(ports.candidates(), vec![(7890, Scheme::Http), (7891, Scheme::Socks5)]);
        let all_zero = ProxyPorts {
            mixed: 0,
            http: 0,
            socks: 0,
        };
        assert!(all_zero.candidates().is_empty());
    }

    #[test]
    fn candidates_dedup_keep_first_scheme() {
        // socks == mixed：保留 mixed 的 HTTP
        let ports = ProxyPorts {
            mixed: 7892,
            http: 7890,
            socks: 7892,
        };
        assert_eq!(ports.candidates(), vec![(7892, Scheme::Http), (7890, Scheme::Http)]);
        // socks == http：保留 http 的 HTTP
        let ports = ProxyPorts {
            mixed: 7892,
            http: 7890,
            socks: 7890,
        };
        assert_eq!(ports.candidates(), vec![(7892, Scheme::Http), (7890, Scheme::Http)]);
        // 三端口相同：仅保留 mixed
        let ports = ProxyPorts {
            mixed: 7890,
            http: 7890,
            socks: 7890,
        };
        assert_eq!(ports.candidates(), vec![(7890, Scheme::Http)]);
    }

    /// 无候选端口时立即返回本地错误（不触网）。
    #[tokio::test]
    async fn fetch_exit_ip_no_candidates() {
        let ports = ProxyPorts {
            mixed: 0,
            http: 0,
            socks: 0,
        };
        assert_eq!(fetch_exit_ip(&ports).await, Err("没有可用的代理端口".to_string()));
    }
}