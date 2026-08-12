//! 出口 IP 探测：经 mihomo 代理端口请求外部回显服务。
//! 多代理端口（mixed/http/socks5）× 多回显端点降级；解析器纯函数可单测。

use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::core::country::country_display;
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

/// 出口探测结果：IP + 展示用国家名（中文名 > 英文名 > 代码；None=无国家信息）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExitInfo {
    pub ip: String,
    pub country: Option<String>,
}

/// 端点表（顺序即优先级）：前三个为"带国家"端点（一次请求拿 IP+国家），
/// 其后为纯 IP 端点（国家=None，UI 显示「未知」）。
const ENDPOINTS: &[(&str, ParseMode)] = &[
    // ip-api.com 免费版仅支持 HTTP；fields 精简响应；45 req/min 对 60s 轮询足够
    ("http://ip-api.com/json/?fields=status,query,country,countryCode", ParseMode::IpApi),
    // cloudflare trace 零成本（HTTPS）：ip= 与 loc= 同行返回
    ("https://www.cloudflare.com/cdn-cgi/trace", ParseMode::Trace),
    // ipwho.is HTTPS 免费 10k/月；fields 精简响应
    ("https://ipwho.is/?fields=ip,success,country,country_code", ParseMode::Ipwho),
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
    IpApi,
    Ipwho,
}

/// 出口 IP 失败分类（供错误提示与测试使用）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ExitErrorKind {
    ConnectRefused,
    ConnectFailed,
    Dns,
    Timeout,
    Builder,
    HttpStatus(u16),
    BadBody,
    Other,
}

/// 遍历 source() 链拼接底层原因：reqwest 0.12 的 Display 只输出
/// "error sending request for url (...)"，Connection refused / DNS 失败等
/// 根因仅存在于 source 链中，需手动拼接。跳过空字符串，相邻重复去重，
/// 用 ": " 连接。
fn chain_string(err: &(dyn std::error::Error + 'static)) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut cur: Option<&(dyn std::error::Error + 'static)> = Some(err);
    while let Some(e) = cur {
        let s = e.to_string();
        if !s.is_empty() && parts.last().map(String::as_str) != Some(s.as_str()) {
            parts.push(s);
        }
        cur = e.source();
    }
    parts.join(": ")
}

/// 纯函数错误分类：is_builder → Builder；is_timeout → Timeout；
/// 链文本含 refused → ConnectRefused；含 DNS 特征 → Dns；
/// is_connect 或连接类特征 → ConnectFailed；否则 Other。
pub fn classify_from_chain(
    is_timeout: bool,
    is_connect: bool,
    is_builder: bool,
    chain: &str,
) -> ExitErrorKind {
    if is_builder {
        ExitErrorKind::Builder
    } else if is_timeout {
        ExitErrorKind::Timeout
    } else {
        let lower = chain.to_lowercase();
        if lower.contains("connection refused") {
            ExitErrorKind::ConnectRefused
        } else if lower.contains("dns resolve failed")
            || lower.contains("failed to lookup")
            || lower.contains("name or service not known")
            || lower.contains("no address associated with hostname")
            // glibc EAI_AGAIN 文本（getaddrinfo 临时失败），当前会落入 ConnectFailed 误导
            || lower.contains("temporary failure in name resolution")
            // hickory-dns 风格（若未来启用 hickory-dns feature）
            || lower.contains("dns error")
            || lower.contains("resolver error")
        {
            ExitErrorKind::Dns
        } else if is_connect
            || lower.contains("error trying to connect")
            || lower.contains("connection closed")
            || lower.contains("protocol error")
        {
            ExitErrorKind::ConnectFailed
        } else {
            ExitErrorKind::Other
        }
    }
}

/// reqwest 错误 → 分类（取 is_timeout/is_connect/is_builder + 完整 source 链）。
fn classify_reqwest(e: &reqwest::Error) -> ExitErrorKind {
    classify_from_chain(e.is_timeout(), e.is_connect(), e.is_builder(), &chain_string(e))
}

/// 失败分类 → 可读中文提示。
pub fn hint_for(kind: ExitErrorKind) -> &'static str {
    match kind {
        ExitErrorKind::ConnectRefused => {
            "代理端口连接被拒：mihomo 未运行或端口配置不一致（systemctl status mihomo）"
        }
        ExitErrorKind::ConnectFailed => "代理连接失败：代理节点不可达或连接被断开",
        ExitErrorKind::Dns => "DNS 解析失败：检查网络/DNS（代理节点侧 DNS 可能不可达）",
        ExitErrorKind::Timeout => "请求超时：外部服务或代理节点响应慢",
        ExitErrorKind::Builder => "代理地址无效，客户端构建失败",
        ExitErrorKind::HttpStatus(_) => "回显服务返回错误状态码",
        ExitErrorKind::BadBody => "回显内容不含有效 IP",
        ExitErrorKind::Other => "未知错误",
    }
}

/// 逐候选端口探测：每端口构建独立 Client（单请求 timeout 8s，单端口总预算 30s），
/// 依次请求端点，成功解析出 IP 立即返回；全部失败返回聚合错误（每端口一行摘要）。
pub async fn fetch_exit_ip(ports: &ProxyPorts) -> Result<ExitInfo, String> {
    let candidates = ports.candidates();
    if candidates.is_empty() {
        return Err("没有可用的代理端口".to_string());
    }
    let mut summaries: Vec<String> = Vec::new();
    let mut kinds: Vec<ExitErrorKind> = Vec::new();
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
                .proxy(
                    reqwest::Proxy::all(&proxy_url)
                        .map_err(|e| (classify_reqwest(&e), chain_string(&e)))?,
                )
                .timeout(Duration::from_secs(8))
                .build()
                .map_err(|e| (classify_reqwest(&e), chain_string(&e)))?;
            let mut last_err: (ExitErrorKind, String) = (ExitErrorKind::Other, String::new());
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
            Ok(Err((kind, last_err))) => {
                summaries.push(format!(
                    "127.0.0.1:{port}({scheme}) 服务全失败（{}；最后错误: {last_err}）",
                    hint_for(kind)
                ));
                kinds.push(kind);
            }
            Err(_) => {
                summaries.push(format!(
                    "127.0.0.1:{port}({scheme}) 服务全失败（{}；最后错误: 单端口总预算超时）",
                    hint_for(ExitErrorKind::Timeout)
                ));
                kinds.push(ExitErrorKind::Timeout);
            }
        }
    }
    // 所有端口均连接被拒：基本可断定 mihomo 未运行或端口配置不一致，
    // 给出明确的首行结论；其余情况保持端口级摘要聚合。
    if !kinds.is_empty() && kinds.iter().all(|k| matches!(k, ExitErrorKind::ConnectRefused)) {
        Err(format!(
            "出口 IP 获取失败: 代理端口全部连接被拒（mihomo 未运行或端口配置不一致，检查 systemctl status mihomo）; {}",
            summaries.join("; ")
        ))
    } else {
        Err(format!("出口 IP 获取失败: {}", summaries.join("; ")))
    }
}

/// 出口 IP 探测（带重试）：最多 3 次尝试，失败间隔 5s，返回最后一次错误。
/// mihomo 重启窗口（apply 后 ~10-30s）内代理端口短暂不可用，单次探测必失败；
/// 重试可覆盖该窗口，避免对瞬时失败误弹「出口 IP 获取失败」。
pub async fn fetch_exit_ip_retry(ports: Arc<Mutex<ProxyPorts>>) -> Result<ExitInfo, String> {
    let mut last_err = String::new();
    for attempt in 0..3 {
        // 先 clone 快照再释放锁：MutexGuard 非 Send，不能跨 await 持锁
        // （60s 周期任务，快照期间锁被占用至多到 clone 完成，约瞬间）
        let snapshot = ports.lock().unwrap().clone();
        match fetch_exit_ip(&snapshot).await {
            Ok(ip) => return Ok(ip),
            Err(e) => {
                last_err = e;
                if attempt < 2 {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            }
        }
    }
    Err(last_err)
}

/// 单端点请求 + 按模式解析。错误为 (分类, 含 source 链的详细文本)。
async fn fetch_one(
    client: &reqwest::Client,
    url: &str,
    mode: ParseMode,
) -> Result<ExitInfo, (ExitErrorKind, String)> {
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| (classify_reqwest(&e), format!("{url}: {}", chain_string(&e))))?;
    if !resp.status().is_success() {
        return Err((
            ExitErrorKind::HttpStatus(resp.status().as_u16()),
            format!("{url} HTTP {}", resp.status()),
        ));
    }
    let text = resp
        .text()
        .await
        .map_err(|e| (classify_reqwest(&e), format!("{url}: {}", chain_string(&e))))?;
    let info = match mode {
        ParseMode::Plain => parse_plain(&text).map(|ip| ExitInfo { ip, country: None }),
        ParseMode::Trace => parse_trace(&text),
        ParseMode::IpApi => parse_ip_api(&text),
        ParseMode::Ipwho => parse_ipwho(&text),
        ParseMode::Ipip => parse_ipip(&text).map(|ip| ExitInfo { ip, country: None }),
    }
    .ok_or_else(|| (ExitErrorKind::BadBody, format!("{url} 内容不含有效 IP/国家信息")))?;
    Ok(info)
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

/// cloudflare /cdn-cgi/trace：逐行找 "ip=" 前缀行（必须合法 IP），
/// 同响应中找 "loc=" 前缀行作为国家代码（可选，缺失/空白则 None）。
/// 注：loc 值（如 HK）是 alpha-2 代码而非 IP，不能经 parse_plain 校验。
pub fn parse_trace(text: &str) -> Option<ExitInfo> {
    let ip = text
        .lines()
        .find_map(|line| line.strip_prefix("ip=").and_then(parse_plain))?;
    let code = text
        .lines()
        .find_map(|line| line.strip_prefix("loc=").map(str::trim).filter(|v| !v.is_empty()));
    Some(ExitInfo {
        ip,
        country: country_display(code, None),
    })
}

/// ip-api.com JSON：{"status":"success","query":"1.2.3.4","country":"United States","countryCode":"US"}
/// 要求 status=="success" 且 query 为合法 IP；country/countryCode 缺失时容忍
/// （返回 ip + 按 code/英文名降级）。
pub fn parse_ip_api(text: &str) -> Option<ExitInfo> {
    #[derive(serde::Deserialize)]
    struct IpApiResp {
        status: String,
        query: Option<String>,
        country: Option<String>,
        #[serde(rename = "countryCode")]
        country_code: Option<String>,
    }
    let resp: IpApiResp = serde_json::from_str(text).ok()?;
    if resp.status != "success" {
        return None;
    }
    let ip = resp.query.as_deref().and_then(parse_plain)?;
    Some(ExitInfo {
        ip,
        country: country_display(resp.country_code.as_deref(), resp.country.as_deref()),
    })
}

/// ipwho.is JSON：{"ip":"1.2.3.4","success":true,"country":"United States","country_code":"US"}
pub fn parse_ipwho(text: &str) -> Option<ExitInfo> {
    #[derive(serde::Deserialize)]
    struct IpwhoResp {
        success: bool,
        ip: Option<String>,
        country: Option<String>,
        #[serde(rename = "country_code")]
        country_code: Option<String>,
    }
    let resp: IpwhoResp = serde_json::from_str(text).ok()?;
    if !resp.success {
        return None;
    }
    let ip = resp.ip.as_deref().and_then(parse_plain)?;
    Some(ExitInfo {
        ip,
        country: country_display(resp.country_code.as_deref(), resp.country.as_deref()),
    })
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
        let text = "fl=20f\nh=www.cloudflare.com\nip=103.151.172.89\nloc=HK\nts=1710000000.000\n";
        let info = parse_trace(text).expect("应解析成功");
        assert_eq!(info.ip, "103.151.172.89");
        assert_eq!(info.country.as_deref(), Some("中国香港"));
    }

    #[test]
    fn parse_trace_missing_or_bad_ip() {
        assert_eq!(parse_trace("fl=20f\nh=www.cloudflare.com\n"), None);
        assert_eq!(parse_trace("ip=foo\nh=www.cloudflare.com\n"), None);
        assert_eq!(parse_trace(""), None);
    }

    #[test]
    fn parse_trace_no_loc_country_none() {
        let text = "fl=20f\nip=103.151.172.89\nts=1710000000.000\n";
        let info = parse_trace(text).expect("ip 存在应解析成功");
        assert_eq!(info.ip, "103.151.172.89");
        assert_eq!(info.country, None);
    }

    #[test]
    fn parse_trace_unmapped_loc_falls_back_to_code() {
        // loc 代码未进映射表：降级显示代码本身
        let text = "ip=1.2.3.4\nloc=ZZ\n";
        let info = parse_trace(text).expect("应解析成功");
        assert_eq!(info.country.as_deref(), Some("ZZ"));
    }

    #[test]
    fn parse_ip_api_ok() {
        let text = r#"{"status":"success","country":"Hong Kong","countryCode":"HK","query":"43.243.192.91"}"#;
        let info = parse_ip_api(text).expect("应解析成功");
        assert_eq!(info.ip, "43.243.192.91");
        assert_eq!(info.country.as_deref(), Some("中国香港"));
    }

    #[test]
    fn parse_ip_api_error_status() {
        assert_eq!(parse_ip_api(r#"{"status":"fail","message":"invalid query"}"#), None);
    }

    #[test]
    fn parse_ip_api_missing_fields() {
        // 仅 status+query：country 缺失 → 降级 None
        let info = parse_ip_api(r#"{"status":"success","query":"1.2.3.4"}"#).expect("应解析成功");
        assert_eq!(info.ip, "1.2.3.4");
        assert_eq!(info.country, None);
    }

    #[test]
    fn parse_ip_api_bad_ip_or_garbage() {
        assert_eq!(parse_ip_api(r#"{"status":"success","query":"not-an-ip"}"#), None);
        assert_eq!(parse_ip_api("not json at all"), None);
        assert_eq!(parse_ip_api(""), None);
    }

    #[test]
    fn parse_ipwho_ok() {
        let text = r#"{"ip":"43.243.192.92","success":true,"country":"Hong Kong","country_code":"HK"}"#;
        let info = parse_ipwho(text).expect("应解析成功");
        assert_eq!(info.ip, "43.243.192.92");
        assert_eq!(info.country.as_deref(), Some("中国香港"));
    }

    #[test]
    fn parse_ipwho_success_false_or_missing() {
        assert_eq!(parse_ipwho(r#"{"ip":"1.2.3.4","success":false}"#), None);
        assert_eq!(parse_ipwho(r#"{"success":true,"country":"X"}"#), None); // 无 ip
        assert_eq!(parse_ipwho("garbage"), None);
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

    /// 可构造多级 source 链的测试错误。
    #[derive(Debug)]
    struct TestErr {
        msg: String,
        source: Option<Box<dyn std::error::Error>>,
    }

    impl TestErr {
        fn new(msg: &str, source: Option<Box<dyn std::error::Error>>) -> Self {
            Self {
                msg: msg.to_string(),
                source,
            }
        }
    }

    impl std::fmt::Display for TestErr {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{}", self.msg)
        }
    }

    impl std::error::Error for TestErr {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            self.source.as_deref()
        }
    }

    #[test]
    fn chain_string_full_chain() {
        let leaf = TestErr::new("connection refused", None);
        let mid = TestErr::new(
            "error sending request for url (http://x)",
            Some(Box::new(leaf)),
        );
        let top = TestErr::new("request failed", Some(Box::new(mid)));
        assert_eq!(
            chain_string(&top),
            "request failed: error sending request for url (http://x): connection refused"
        );
    }

    #[test]
    fn chain_string_dedups_adjacent_duplicates() {
        let leaf = TestErr::new("same", None);
        let mid = TestErr::new("same", Some(Box::new(leaf)));
        let top = TestErr::new("top", Some(Box::new(mid)));
        assert_eq!(chain_string(&top), "top: same");
    }

    #[test]
    fn chain_string_skips_empty_nodes() {
        let leaf = TestErr::new("bottom", None);
        let mid = TestErr::new("", Some(Box::new(leaf)));
        let top = TestErr::new("", Some(Box::new(mid)));
        assert_eq!(chain_string(&top), "bottom");
    }

    #[test]
    fn chain_string_single_layer() {
        let err = TestErr::new("just one", None);
        assert_eq!(chain_string(&err), "just one");
    }

    /// 非相邻重复不去重：a → b → a 保持 "a: b: a" 原样
    /// （实现仅跳过与上一条相同的节点，防止未来误改成全局去重）。
    #[test]
    fn chain_string_keeps_non_adjacent_duplicates() {
        let leaf = TestErr::new("a", None);
        let mid = TestErr::new("b", Some(Box::new(leaf)));
        let top = TestErr::new("a", Some(Box::new(mid)));
        assert_eq!(chain_string(&top), "a: b: a");
    }

    #[test]
    fn classify_connection_refused() {
        assert_eq!(
            classify_from_chain(
                false,
                false,
                false,
                "error sending request for url (http://x): error trying to connect: tcp connect error: Connection refused (os error 111)"
            ),
            ExitErrorKind::ConnectRefused
        );
    }

    #[test]
    fn classify_dns_variants() {
        assert_eq!(
            classify_from_chain(false, false, false, "dns resolve failed: nxdomain"),
            ExitErrorKind::Dns
        );
        assert_eq!(
            classify_from_chain(false, false, false, "failed to lookup host example.com"),
            ExitErrorKind::Dns
        );
        assert_eq!(
            classify_from_chain(false, false, false, "Name or service not known"),
            ExitErrorKind::Dns
        );
        assert_eq!(
            classify_from_chain(
                false,
                false,
                false,
                "no address associated with hostname"
            ),
            ExitErrorKind::Dns
        );
    }

    #[test]
    fn classify_connect_failed_variants() {
        assert_eq!(
            classify_from_chain(
                false,
                false,
                false,
                "connection closed before message completed"
            ),
            ExitErrorKind::ConnectFailed
        );
        assert_eq!(
            classify_from_chain(false, false, false, "error trying to connect: tcp connect error"),
            ExitErrorKind::ConnectFailed
        );
        // is_connect 标志本身即 ConnectFailed（即使文本无特征）
        assert_eq!(
            classify_from_chain(false, true, false, "something else"),
            ExitErrorKind::ConnectFailed
        );
    }

    #[test]
    fn classify_timeout_builder_other() {
        assert_eq!(
            classify_from_chain(true, false, false, "whatever text"),
            ExitErrorKind::Timeout
        );
        assert_eq!(
            classify_from_chain(false, false, true, "whatever text"),
            ExitErrorKind::Builder
        );
        assert_eq!(
            classify_from_chain(false, false, false, "totally random text"),
            ExitErrorKind::Other
        );
    }

    /// 分类优先级钉死：标志位（Builder > Timeout）优先于链文本特征；
    /// 文本特征内 refused > Dns > Connect。防止未来调整顺序时被文本误导。
    #[test]
    fn classify_priority_builder_timeout_over_text() {
        // is_builder 优先：即使链文本含 connection refused
        assert_eq!(
            classify_from_chain(false, false, true, "connection refused (os error 111)"),
            ExitErrorKind::Builder
        );
        // is_builder 优先：即使链文本含 timed out
        assert_eq!(
            classify_from_chain(false, false, true, "operation timed out"),
            ExitErrorKind::Builder
        );
        // is_timeout 优先于 refused 文本特征
        assert_eq!(
            classify_from_chain(true, false, false, "connection refused (os error 111)"),
            ExitErrorKind::Timeout
        );
        // 文本特征内 Dns 优先于 Connect（is_connect 不抢 DNS 文本）
        assert_eq!(
            classify_from_chain(false, true, false, "failed to lookup address"),
            ExitErrorKind::Dns
        );
    }

    #[test]
    fn hint_contains_keywords() {
        assert!(hint_for(ExitErrorKind::ConnectRefused).contains("mihomo 未运行"));
        assert!(hint_for(ExitErrorKind::Timeout).contains("超时"));
        assert!(hint_for(ExitErrorKind::Dns).contains("DNS"));
    }

    /// 已关闭端口：bind 拿到空闲端口后 drop 释放，此时无监听，reqwest 对
    /// 127.0.0.1 的代理连接立即 ConnectRefused（不触网、不等待），
    /// 10 端点 × 瞬时失败远小于 30s 预算。
    #[tokio::test]
    async fn fetch_exit_ip_closed_port_classified() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let port = listener.local_addr().expect("local addr").port();
        drop(listener);
        let ports = ProxyPorts {
            mixed: port,
            http: 0,
            socks: 0,
        };
        let err = fetch_exit_ip(&ports).await.expect_err("closed port must fail");
        assert!(err.contains(&port.to_string()), "err: {err}");
        assert!(err.contains("连接被拒"), "err: {err}");
        assert!(err.contains("代理端口全部连接被拒"), "err: {err}");
    }
}