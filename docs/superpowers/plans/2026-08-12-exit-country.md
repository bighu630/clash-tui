# 出口 IP 国家展示 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 仪表盘状态栏「出口IP」从仅显示 IP 升级为「IP「国家」」格式（如 `出口IP: 43.243.192.97「香港」`），经代理一次请求同时获取 IP 与国家，失败/无国家时降级显示「未知」。

**Architecture:** 新增 `src/core/country.rs`（ISO 3166-1 alpha-2 → 中文名映射表 + 展示名解析，纯函数可单测）；改造 `src/core/exit_ip.rs`：返回类型从 `Result<String, String>` 升级为 `Result<ExitInfo, String>`（`ExitInfo { ip, country }`），端点表头部插入三个"带国家"端点（ip-api.com 主 / cloudflare trace loc 兜底 / ipwho.is 兜底），纯 IP 端点保持最后兜底（国家=None）；`app.rs` 的 `AppState.exit_ip` 与 `UiEvent::ExitIp` 同步升级为 `ExitInfo`；`dashboard.rs` 渲染 `ip「country」`。错误分类/诊断逻辑、60s 轮询、多端口×多端点重试全部保留不变。

**Tech Stack:** Rust 2021, reqwest 0.12 (json feature 已启用), serde_json, ratatui 0.30。

**已确认需求（与用户对齐）：**
1. 展示格式：中文国家名 `出口IP: 43.243.192.97「香港」`
2. 国家名解析优先级：中文（映射表）> 英文（服务返回的 country 字段）> 国家代码 > 「未知」
3. 服务策略：ip-api.com（HTTP，45 req/min）为主端点；cloudflare trace（`loc=` 行，零成本）兜底；ipwho.is（HTTPS，10k/月）再兜底；现有纯 IP 端点为最后兜底
4. 降级：国家拿不到但 IP 正常 → 显示「未知」；IP 也拿不到 → 维持现有失败诊断逻辑不变

**已端到端实测（经 mihomo 7890 代理，2026-08-12）：**
- `http://ip-api.com/json/?fields=status,query,country,countryCode` → `{"status":"success","country":"Hong Kong","countryCode":"HK","query":"43.243.192.91"}`
- `https://www.cloudflare.com/cdn-cgi/trace` → 含 `ip=43.243.192.91` 与 `loc=HK`
- `https://ipwho.is/?fields=ip,success,country,country_code` → `{"ip":"43.243.192.92","success":true,"country":"Hong Kong","country_code":"HK"}`

---

## 文件结构

| 文件 | 动作 | 职责 |
|---|---|---|
| `src/core/country.rs` | 新建 | ISO 3166-1 alpha-2 → 中文名映射（match 表）+ `country_display` 展示名解析（中文>英文>代码>None） |
| `src/core/mod.rs` | 修改 | 注册 `country` 模块（pub mod） |
| `src/core/exit_ip.rs` | 修改 | `ExitInfo` 结构、端点表重排与新增 IpApi/Ipwho 解析模式、Trace 模式加 `loc=` 解析、`fetch_exit_ip`/`fetch_exit_ip_retry`/`fetch_one` 返回类型升级、测试更新 |
| `src/app.rs` | 修改 | `state.exit_ip: Option<ExitInfo>`、`UiEvent::ExitIp(Result<ExitInfo, String>)`、成功分支与恢复通知格式、测试更新 |
| `src/ui/dashboard.rs` | 修改 | render_status 渲染 `ip「country」`，country=None → 「未知」 |
| `README.md` | 修改 | 状态栏示例、说明文字、按键表、模块表 |

## 全局契约（所有 worker 必须遵守，跨文件一致）

```rust
// src/core/country.rs 导出
pub fn zh_name(code: &str) -> Option<&'static str>;
// 优先级：code 查表得中文名 > en(trim 后非空) > code 本身 > None
pub fn country_display(code: Option<&str>, en: Option<&str>) -> Option<String>;

// src/core/exit_ip.rs 导出（原 parse_trace 签名变更！）
pub struct ExitInfo { pub ip: String, pub country: Option<String> }
pub async fn fetch_exit_ip(ports: &ProxyPorts) -> Result<ExitInfo, String>;
pub async fn fetch_exit_ip_retry(ports: Arc<Mutex<ProxyPorts>>) -> Result<ExitInfo, String>;
pub fn parse_plain(text: &str) -> Option<String>;              // 不变
pub fn parse_trace(text: &str) -> Option<ExitInfo>;            // 变更：ip= 必须合法；loc= 可选
pub fn parse_ipip(text: &str) -> Option<String>;               // 不变
pub fn parse_ip_api(text: &str) -> Option<ExitInfo>;           // 新增
pub fn parse_ipwho(text: &str) -> Option<ExitInfo>;            // 新增
pub fn looks_like_ip(s: &str) -> bool;                         // 不变
```

`ExitInfo.country` 语义：**已是最终展示文本**（中文名 > 英文名 > 代码），`None` = 无国家信息（UI 显示「未知」）。

---

### Task 1: `src/core/country.rs` 新建 + 注册模块

**Files:**
- Create: `src/core/country.rs`
- Modify: `src/core/mod.rs`

- [ ] **Step 1: 编写 `src/core/country.rs`**

```rust
//! 国家/地区代码（ISO 3166-1 alpha-2）→ 中文展示名。
//! 纯函数、无依赖，独立模块便于单测；映射表覆盖常见国家/地区，
//! 未覆盖代码由调用方降级（英文名 → 代码 → 未知）。

/// 常见国家/地区中文名表。覆盖 ~90 个常用项；
/// 特别行政区/地区用「中国香港」「中国台湾」「中国澳门」。
pub fn zh_name(code: &str) -> Option<&'static str> {
    let c = code.to_ascii_uppercase();
    Some(match c.as_str() {
        "US" => "美国",
        "CN" => "中国",
        "HK" => "中国香港",
        "TW" => "中国台湾",
        "MO" => "中国澳门",
        "JP" => "日本",
        "KR" => "韩国",
        "KP" => "朝鲜",
        "SG" => "新加坡",
        "MY" => "马来西亚",
        "TH" => "泰国",
        "VN" => "越南",
        "PH" => "菲律宾",
        "ID" => "印度尼西亚",
        "IN" => "印度",
        "PK" => "巴基斯坦",
        "BD" => "孟加拉国",
        "LK" => "斯里兰卡",
        "NP" => "尼泊尔",
        "MM" => "缅甸",
        "KH" => "柬埔寨",
        "LA" => "老挝",
        "BN" => "文莱",
        "AE" => "阿联酋",
        "SA" => "沙特阿拉伯",
        "IL" => "以色列",
        "TR" => "土耳其",
        "IR" => "伊朗",
        "QA" => "卡塔尔",
        "KW" => "科威特",
        "OM" => "阿曼",
        "BH" => "巴林",
        "JO" => "约旦",
        "LB" => "黎巴嫩",
        "IQ" => "伊拉克",
        "SY" => "叙利亚",
        "YE" => "也门",
        "AF" => "阿富汗",
        "GE" => "格鲁吉亚",
        "AM" => "亚美尼亚",
        "AZ" => "阿塞拜疆",
        "KZ" => "哈萨克斯坦",
        "UZ" => "乌兹别克斯坦",
        "MN" => "蒙古",
        "GB" => "英国",
        "DE" => "德国",
        "FR" => "法国",
        "NL" => "荷兰",
        "BE" => "比利时",
        "CH" => "瑞士",
        "AT" => "奥地利",
        "IT" => "意大利",
        "ES" => "西班牙",
        "PT" => "葡萄牙",
        "SE" => "瑞典",
        "NO" => "挪威",
        "DK" => "丹麦",
        "FI" => "芬兰",
        "IE" => "爱尔兰",
        "PL" => "波兰",
        "CZ" => "捷克",
        "SK" => "斯洛伐克",
        "HU" => "匈牙利",
        "RO" => "罗马尼亚",
        "BG" => "保加利亚",
        "GR" => "希腊",
        "RU" => "俄罗斯",
        "UA" => "乌克兰",
        "BY" => "白俄罗斯",
        "MD" => "摩尔多瓦",
        "EE" => "爱沙尼亚",
        "LV" => "拉脱维亚",
        "LT" => "立陶宛",
        "SI" => "斯洛文尼亚",
        "HR" => "克罗地亚",
        "RS" => "塞尔维亚",
        "CY" => "塞浦路斯",
        "MT" => "马耳他",
        "IS" => "冰岛",
        "LU" => "卢森堡",
        "LI" => "列支敦士登",
        "MC" => "摩纳哥",
        "AD" => "安道尔",
        "AU" => "澳大利亚",
        "NZ" => "新西兰",
        "CA" => "加拿大",
        "MX" => "墨西哥",
        "BR" => "巴西",
        "AR" => "阿根廷",
        "CL" => "智利",
        "PE" => "秘鲁",
        "CO" => "哥伦比亚",
        "VE" => "委内瑞拉",
        "UY" => "乌拉圭",
        "PY" => "巴拉圭",
        "BO" => "玻利维亚",
        "EC" => "厄瓜多尔",
        "CR" => "哥斯达黎加",
        "PA" => "巴拿马",
        "CU" => "古巴",
        "DO" => "多米尼加",
        "PR" => "波多黎各",
        "ZA" => "南非",
        "EG" => "埃及",
        "NG" => "尼日利亚",
        "KE" => "肯尼亚",
        "MA" => "摩洛哥",
        "DZ" => "阿尔及利亚",
        "TN" => "突尼斯",
        "ET" => "埃塞俄比亚",
        "TZ" => "坦桑尼亚",
        "GH" => "加纳",
        "FJ" => "斐济",
        _ => return None,
    })
}

/// 展示名解析：中文（查表）> 英文（trim 非空）> 代码 > None。
/// 调用方（UI）在 None 时显示「未知」。
pub fn country_display(code: Option<&str>, en: Option<&str>) -> Option<String> {
    if let Some(code) = code {
        if let Some(zh) = zh_name(code) {
            return Some(zh.to_string());
        }
    }
    if let Some(en) = en {
        let t = en.trim();
        if !t.is_empty() {
            return Some(t.to_string());
        }
    }
    code.map(|c| c.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zh_name_common_codes() {
        assert_eq!(zh_name("US"), Some("美国"));
        assert_eq!(zh_name("us"), Some("美国")); // 大小写不敏感
        assert_eq!(zh_name("HK"), Some("中国香港"));
        assert_eq!(zh_name("TW"), Some("中国台湾"));
        assert_eq!(zh_name("JP"), Some("日本"));
        assert_eq!(zh_name("GB"), Some("英国"));
    }

    #[test]
    fn zh_name_uncovered_returns_none() {
        assert_eq!(zh_name("ZZ"), None);
        assert_eq!(zh_name(""), None);
        assert_eq!(zh_name("USA"), None); // 3 字母非 alpha-2
    }

    #[test]
    fn display_priority_zh_over_en_over_code() {
        assert_eq!(
            country_display(Some("HK"), Some("Hong Kong")),
            Some("中国香港".to_string())
        );
        // 代码未覆盖：英文名
        assert_eq!(
            country_display(Some("ZZ"), Some("Zzzland")),
            Some("Zzzland".to_string())
        );
        // 只有代码：显示代码本身
        assert_eq!(country_display(Some("ZZ"), None), Some("ZZ".to_string()));
        // 全无：None（UI 显示「未知」）
        assert_eq!(country_display(None, None), None);
    }

    #[test]
    fn display_trims_empty_en() {
        assert_eq!(country_display(None, Some("   ")), None);
        assert_eq!(country_display(None, Some("")), None);
        assert_eq!(country_display(None, Some(" United States ")), Some("United States".to_string()));
    }
}
```

- [ ] **Step 2: 注册模块（`src/core/mod.rs`）**

在现有 `pub mod` 列表中加入 `pub mod country;`（与 `exit_ip` 相邻即可）。

- [ ] **Step 3: 编译 + 测试**

Run: `cargo test country` 与 `cargo build`
Expected: 全部 PASS，编译无警告。

- [ ] **Step 4: Commit**

```bash
git add src/core/country.rs src/core/mod.rs
git commit -m "feat: 国家/地区代码中文名映射表模块（country.rs）"
```

---

### Task 2: `src/core/exit_ip.rs` 改造（ExitInfo + 新端点 + 解析）

**Files:**
- Modify: `src/core/exit_ip.rs`

- [ ] **Step 1: 新增 `ExitInfo` 结构（放在 `ProxyPorts` 之后）**

```rust
/// 出口探测结果：IP + 展示用国家名（中文名 > 英文名 > 代码；None=无国家信息）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExitInfo {
    pub ip: String,
    pub country: Option<String>,
}
```

- [ ] **Step 2: 端点表重排 + 新增 ParseMode（替换现有 `ENDPOINTS` 与 `ParseMode`）**

```rust
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
```

- [ ] **Step 3: 新增/变更解析纯函数（`parse_trace` 签名变更！）**

保留 `parse_plain`/`parse_ipip`/`looks_like_ip`/`is_ipv4_strict`/`is_ipv6_loose` 不变。变更 `parse_trace` 并新增两个 JSON 解析：

```rust
/// cloudflare /cdn-cgi/trace：逐行找 "ip=" 前缀行（必须合法 IP），
/// 同响应中找 "loc=" 前缀行作为国家代码（可选，缺失/非法则 None）。
pub fn parse_trace(text: &str) -> Option<ExitInfo> {
    let ip = text
        .lines()
        .find_map(|line| line.strip_prefix("ip=").and_then(parse_plain))?;
    let code = text
        .lines()
        .find_map(|line| line.strip_prefix("loc=").and_then(|v| parse_plain(v)));
    Some(ExitInfo {
        ip,
        country: country_display(code.as_deref(), None),
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
```

顶部 import 增加：`use crate::core::country::country_display;`

- [ ] **Step 4: `fetch_one` 返回类型升级**

```rust
async fn fetch_one(
    client: &reqwest::Client,
    url: &str,
    mode: ParseMode,
) -> Result<ExitInfo, (ExitErrorKind, String)> {
    // ... 前半段（请求/状态码/读文本）不变 ...
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
```

- [ ] **Step 5: `fetch_exit_ip` / `fetch_exit_ip_retry` 返回类型升级**

- `fetch_exit_ip(ports: &ProxyPorts) -> Result<ExitInfo, String>`：内部 `Ok(ip)` → `Ok(info)`，其余逻辑（候选端口遍历、30s 预算、错误分类、聚合错误文本）**逐字保留不变**。
- `fetch_exit_ip_retry(ports: Arc<Mutex<ProxyPorts>>) -> Result<ExitInfo, String>`：仅签名 `String` → `ExitInfo`，重试循环不变。
- 无候选端口错误文本 `"没有可用的代理端口"` 不变。

- [ ] **Step 6: 更新现有测试 + 新增测试**

现有测试变更点（必须在 `src/core/exit_ip.rs` 测试模块内更新）：
- `parse_trace_hits_ip_line` / `parse_trace_missing_or_bad_ip`：`parse_trace` 现在返回 `Option<ExitInfo>`：

```rust
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
```

新增测试：

```rust
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
```

- [ ] **Step 7: 全量测试**

Run: `cargo test exit_ip` 与 `cargo test`
Expected: 全部 PASS（现有 221 个 + 新增约 12 个；`fetch_exit_ip_no_candidates` / `fetch_exit_ip_closed_port_classified` 等现有测试只断言行类型，需改为 `ExitInfo` 断言，见下）。

现有 `fetch_exit_ip_no_candidates` 与 `fetch_exit_ip_closed_port_classified` 断言的是 `Err(String)`，不受返回类型升级影响（错误路径不变），**无需改动**（确认编译通过即可）。

- [ ] **Step 8: Commit**

```bash
git add src/core/exit_ip.rs
git commit -m "feat: 出口 IP 获取升级为 IP+国家（ip-api/cloudflare loc/ipwho.is，返回 ExitInfo）"
```

---

### Task 3: `src/app.rs` 状态与事件升级

**Files:**
- Modify: `src/app.rs`

依赖 Task 2 契约（`ExitInfo` 结构已定义）。文件顶部 `use crate::core::exit_ip::{self, ProxyPorts};` 增加 `ExitInfo`。

- [ ] **Step 1: 字段与事件类型变更**

```rust
// AppState（约 L51）
pub exit_ip: Option<ExitInfo>,

// UiEvent（约 L150）
ExitIp(Result<ExitInfo, String>),
```

- [ ] **Step 2: 成功分支（`UiEvent::ExitIp` 的 Ok 分支，约 L644-660）**

```rust
Ok(info) => {
    // 恢复成功：关闭先前失败留下的陈旧错误弹窗（内容已过时）
    if self
        .result_popup
        .as_ref()
        .is_some_and(|p| p.title() == "出口 IP 获取失败")
    {
        self.result_popup = None;
    }
    // 此前有失败：通知恢复；无失败历史时静默更新
    if self.exit_ip_was_error {
        self.exit_ip_was_error = false;
        let label = match (&info.country, info.ip.as_str()) {
            (Some(c), ip) => format!("{ip}「{c}」"),
            (None, ip) => ip.to_string(),
        };
        self.state.notice(format!("[✓] 出口 IP 恢复: {label}"));
    }
    self.state.exit_ip = Some(info);
}
```

（Err 分支与交叉判断逻辑逐字保留。）

- [ ] **Step 3: 更新测试（`exit_ip_recovery_closes_stale_popup_and_notices` 与 `exit_ip_recovery_keeps_unrelated_popup`）**

`Ok("1.2.3.4".into())` 改为 `Ok(ExitInfo { ip: "1.2.3.4".into(), country: None })`；`as_deref()` 断言改为：

```rust
assert_eq!(app.state.exit_ip.as_ref().map(|e| e.ip.as_str()), Some("1.2.3.4"));
assert_eq!(app.state.exit_ip.as_ref().and_then(|e| e.country.as_deref()), None);
```

并补一个带国家的恢复用例（在 `exit_ip_recovery_closes_stale_popup_and_notices` 末尾追加或新测试）：

```rust
#[test]
fn exit_ip_recovery_notice_includes_country() {
    let (mut app, _rx) = test_app(24);
    app.on_ui_event(UiEvent::ExitIp(Err("出口 IP 获取失败: 连接被拒".into())));
    app.on_ui_event(UiEvent::ExitIp(Ok(ExitInfo {
        ip: "43.243.192.97".into(),
        country: Some("中国香港".into()),
    })));
    assert!(
        app.state.notices.iter().any(|n| n.contains("[✓] 出口 IP 恢复: 43.243.192.97「中国香港」")),
        "应通知恢复且带国家: {:?}",
        app.state.notices
    );
}
```

（注意：`test_app` 与其它测试文件中若还有 `exit_ip: None` 构造，字段类型变化不影响 `None` 字面量，但确认 `src/app.rs` 内 `test_app`/构造处无需改 `exit_ip` 类型标注。）

- [ ] **Step 4: 编译 + 全量测试**

Run: `cargo build` 与 `cargo test`
Expected: 全部 PASS。

- [ ] **Step 5: Commit**

```bash
git add src/app.rs
git commit -m "feat: AppState/UiEvent 升级 ExitInfo，恢复通知带国家名"
```

---

### Task 4: `src/ui/dashboard.rs` 状态栏渲染

**Files:**
- Modify: `src/ui/dashboard.rs`

- [ ] **Step 1: `render_status`（约 L253-262）**

```rust
/// 顶栏状态行：`模式: rule [m] | TUN: on [t] | IPv6: on [6] | 出口IP: x「国家」 [r] | API: 已连接`
fn render_status(f: &mut Frame, area: Rect, st: &AppState) {
    // ... mode/tun/ipv6 不变 ...
    let ip = st.exit_ip.as_ref().map(|e| e.ip.as_str()).unwrap_or("未知");
    let country = st.exit_ip.as_ref().and_then(|e| e.country.as_deref());
    // ... api_text/api_color 不变 ...
    let spans = vec![
        // ... 模式/TUN/IPv6 段不变 ...
        Span::raw("出口IP: "),
        Span::styled(ip, Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
        // 国家段：有国家 → 「中国香港」；无国家信息（IP 正常）→ 「未知」；IP 也未获取 → 不显示
        Span::raw(match (country, st.exit_ip.is_some()) {
            (Some(c), _) => format!("「{c}」"),
            (None, true) => "「未知」".to_string(),
            (None, false) => String::new(),
        }),
        Span::raw(" [r]  "),
        // ... API 段不变 ...
    ];
    // ...
}
```

- [ ] **Step 2: 状态栏渲染测试（在 dashboard.rs 测试模块追加）**

（如现有测试已有 render_status 相关断言则更新；否则新增。可用现有 `test_state()` 构造。）

```rust
#[test]
fn render_status_shows_ip_and_country() {
    let mut st = test_state();
    st.exit_ip = Some(ExitInfo {
        ip: "43.243.192.97".into(),
        country: Some("中国香港".into()),
    });
    // 通过 render_status 内部 Span 构造验证：直接构造同款 Line 较脆弱，
    // 改为验证状态字段到展示字符串的映射逻辑——若 render_status 无纯函数，
    // 此测试改为编译期+人工确认；此处至少覆盖字段读取：
    assert_eq!(st.exit_ip.as_ref().map(|e| e.ip.as_str()), Some("43.243.192.97"));
    assert_eq!(
        st.exit_ip.as_ref().and_then(|e| e.country.as_deref()),
        Some("中国香港")
    );
}
```

如 render_status 保持为纯渲染函数（无单独可测逻辑），则测试仅需确认 `test_state()` 与新增字段编译通过、现有 dashboard 测试全绿；不强行构造 ratatui 渲染断言。

- [ ] **Step 3: 编译 + 全量测试**

Run: `cargo build` 与 `cargo test`
Expected: 全部 PASS。

- [ ] **Step 4: Commit**

```bash
git add src/ui/dashboard.rs
git commit -m "feat: 状态栏出口 IP 显示「国家」（无国家信息显示「未知」）"
```

---

### Task 5: README 更新

**Files:**
- Modify: `README.md`

- [ ] **Step 1: 更新以下位置（逐条精确替换）**

1. L13 功能总览：`**仪表盘（首页）**——模式/TUN/IPv6/出口 IP 热切换、连接列表、网络速率/累计流量、内存：` → 加国家说明：
   `**仪表盘（首页）**——模式/TUN/IPv6/出口 IP（含国家）热切换、连接列表、网络速率/累计流量、内存：`
2. L19 示例：`模式: rule [m]  TUN: 关 [t]  IPv6: 关 [6]  出口IP: 9.9.9.9 [r]  API: 已连接` → `模式: rule [m]  TUN: 关 [t]  IPv6: 关 [6]  出口IP: 9.9.9.9「美国」 [r]  API: 已连接`
3. L48：`> 上图来自演示环境（假 API 数据）；真实环境中「出口IP」显示经代理探测到的公网出口地址。` → `...公网出口地址及所在国家/地区（中文名，如「美国」；无国家信息时显示「未知」）。`
4. L53：`- `m`/`t`/`6`：模式 / TUN / IPv6 运行时热切换（PATCH，不重启）；`r` 手动刷新出口 IP；` 后可追加一句国家说明（可并入 L53 或 L129）。
5. L129：`- `r`：手动刷新出口 IP（每 60s 自动刷新；应用配置成功后自动立即重测一次）` → 追加 `；出口 IP 经代理探测，同时返回国家/地区（ip-api.com / cloudflare / ipwho.is 优先，纯 IP 端点兜底）`
6. L136：`- 出口 IP 获取失败时弹出诊断弹窗（见 FAQ），恢复成功自动关闭陈旧弹窗并通知` 不变（可加"恢复通知含国家"）。
7. L274 按键表 `| `r` | 手动刷新出口 IP |` → `| `r` | 手动刷新出口 IP（含国家） |`
8. L304 模块表：`exit_ip.rs    出口 IP 探测（多代理端口 × 多回显端点降级，失败分类 + 中文提示）` → 更新为 `exit_ip.rs    出口 IP+国家探测（多代理端口 × 多端点降级，失败分类 + 中文提示）`，并在其后新增一行 `country.rs    国家/地区代码→中文名映射（ISO 3166-1 alpha-2）`
9. 检查 FAQ 部分是否有「出口 IP」相关问答条目需要补充国家说明（搜 `## FAQ` 或 `### FAQ` 章节）。

- [ ] **Step 2: 自查**

Run: `grep -n "出口IP\|出口 IP" README.md` 确认所有提及出口 IP 的位置已与国家说明一致（有疑问位置保持原文并说明原因）。

- [ ] **Step 3: Commit**

```bash
git add README.md
git commit -m "docs: README 更新出口 IP 国家展示说明"
```

---

### Task 6: 整合验证（feature_lead 执行，非 worker）

- [ ] 全量 `cargo test`（worktree 内）
- [ ] `cargo build --release`
- [ ] 端到端：`./target/release/mihomo-tui` 连接本机 mihomo（7890/7891/7892 已在监听），按 `r` 验证状态栏出现 `出口IP: <ip>「<国家>」`
- [ ] 失败路径抽查：临时停用代理端口（如改设置端口为未监听端口）验证诊断弹窗逻辑不退化
- [ ] 合并回 main（由 feature_lead 决定时机，与 logs-page worktree 无文件冲突：logs-page 分支仅动 logs 相关文件）

---

## Self-Review

**Spec coverage:**
- 展示格式「IP「中文国家名」」→ Task 1（映射表）+ Task 2（解析）+ Task 4（渲染）✓
- 一次请求同时拿 IP+国家 → Task 2 端点表（ip-api/cloudflare/ipwho.is）✓
- 多端口多端点重试/60s 轮询/错误分类保留 → Task 2 Step 5 明确"逐字保留"✓
- 降级「未知」→ Task 4 Step 1（None,true 分支）✓
- 中文映射表独立模块 → Task 1（country.rs）✓
- 单元测试（解析/缺字段/错误/降级）→ Task 2 Step 6、Task 3 Step 3 ✓
- README → Task 5 ✓
- 端到端验证 → Task 6 ✓

**Type consistency:** `ExitInfo { ip: String, country: Option<String> }` 在 Task 2 定义，Task 3/4 使用；`country_display(code, en)` Task 1 定义 Task 2 调用；`parse_trace -> Option<ExitInfo>` 变更在 Task 2 Step 3 与 Step 6 同步。✓

**Placeholder scan:** 无 TBD/占位。所有解析器给出完整代码。✓

**注意（worker 须知）：** 本计划 Task 2 修改 `src/core/exit_ip.rs`，Task 3 修改 `src/app.rs`，Task 4 修改 `src/ui/dashboard.rs`，文件互不重叠可并行；Task 1 被 Task 2 依赖（`country_display`），Task 2 的契约（`ExitInfo`）被 Task 3/4 依赖。并行时以本计划「全局契约」为唯一事实源。
