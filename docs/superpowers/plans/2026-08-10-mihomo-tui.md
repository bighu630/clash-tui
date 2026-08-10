# mihomo-tui Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在 /data/code/clash-tui/.worktrees/impl 从零实现 Linux 上的 mihomo TUI 控制器（Rust + ratatui），交付可编译、可测试、可运行的项目。

**Architecture:** 二进制 crate（lib + bin）。core 层（纯 Rust，无 TUI 依赖，可单元测试）：模型、settings.toml 存取、订阅拉取/解析（7 协议解析器）、合并器（merger，去重+引用校验+默认模板）、REST 客户端、sudo 应用。ui 层：ratatui 四页面 + 通用弹窗组件。service 层：首装安装器 + 内嵌 mihomo-apply.sh。app.rs 用 tokio::select! 合并 crossterm 事件流、1s tick、/traffic 与 /memory 流、命令/事件通道。

**Tech Stack:** Rust 2021 edition, ratatui 0.30, crossterm 0.28 (event-stream), tokio 1 (full), reqwest 0.12 (json+stream+rustls-tls, 无 openssl), serde/serde_json/serde_yaml 0.9, toml 0.9, base64 0.22, url 2, chrono 0.4 (serde), thiserror 2, futures-util 0.3.

---

## 0. 工作区与前置（已由 lead 完成）

- 仓库：/data/code/clash-tui（git init，docs/ 已提交）
- worktree：/data/code/clash-tui/.worktrees/impl（分支 feature/mihomo-tui）——**所有工作在此目录**
- 骨架已存在：Cargo.toml（依赖完整）、src/lib.rs、src/main.rs（stub）、src/app.rs（stub）、src/core/mod.rs、src/ui/mod.rs、src/service/mod.rs、resources/mihomo-apply.sh、.gitignore（target/、.worktrees/）
- 每个任务完成即 `cargo build` + 相关 `cargo test` 通过后 commit

## 1. 文件结构总览

```
src/
  main.rs                 — 终端初始化/恢复、panic hook、调用 app::run (worker B1)
  lib.rs                  — pub mod core; pub mod ui; pub mod app; pub mod service;（已建）
  app.rs                  — AppState、UiCommand/UiEvent、事件循环 tokio::select! (B1)
  core/
    mod.rs                — pub mod 声明 (A)
    models.rs             — 全部数据模型 (A)
    settings.rs           — 配置文件存取 (A)
    subscription.rs       — 拉取/识别/解析/缓存 (A)
    parsers/mod.rs        — 分发 + 7 个协议解析器 (A)
    merger.rs             — 合并+去重+校验+模板 (A)
    client.rs             — REST 客户端 + 流 (A)
    apply.rs              — mihomo -t 校验 + sudo apply (A)
  ui/
    mod.rs                — Page trait、页面模块声明、共用类型 (B1)
    widgets.rs            — FormPopup/CheckboxList/ConfirmPopup/MessagePopup/SelectList/KeyHints/format_bytes (B1)
    dashboard.rs          — 首页 (B1)
    subscriptions.rs      — 订阅管理页 (B2)
    groups.rs             — 规则组页 (B2)
    rules.rs              — 规则页 (B2)
  service/
    mod.rs                — pub mod installer; (已建)
    installer.rs          — 首装检测/安装 (C)
resources/mihomo-apply.sh — 内嵌提权脚本 (lead 已写，C 负责 include 与 README 文档)
README.md                — (C)
examples/merge_sample.rs  — 合并样例输出工具，供 mihomo -t 集成验证 (A)
```

**并行 worker 划分**（文件互不重叠，契约见下文 §2/§3）：
- **Worker A**（core 全部 + examples）：src/core/**、examples/merge_sample.rs
- **Worker B1**（ui 基础 + 首页 + 主循环）：src/main.rs、src/app.rs、src/ui/mod.rs、src/ui/widgets.rs、src/ui/dashboard.rs
- **Worker B2**（三个管理页）：src/ui/subscriptions.rs、src/ui/groups.rs、src/ui/rules.rs
- **Worker C**（服务层 + 文档）：src/service/installer.rs、README.md

## 2. Core API 契约（Worker A 必须实现，B1/B2 按其编码）

```rust
// ============ core/models.rs ============
pub const BUILTIN_TARGETS: [&str; 7] =
    ["DIRECT", "REJECT", "REJECT-DROP", "COMPATIBLE", "PASS", "PASS-RULE", "GLOBAL"];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkSettings {
    pub mode: String,                 // "rule"|"global"|"direct"
    pub ipv6: bool,
    pub allow_lan: bool,
    pub port: u16,                    // 7890
    pub socks_port: u16,              // 7891
    pub mixed_port: u16,              // 7892
    pub log_level: String,            // "info"
    pub external_controller: String,  // "127.0.0.1:9090"
    pub secret: String,               // 随机 32 hex
    pub tun: TunSettings,
    pub dns: DnsSettings,
}
impl Default for NetworkSettings  // 上述默认值；secret 用 generate_secret()

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TunSettings {
    pub enable: bool,                 // false
    pub stack: String,                // "mixed"
    pub auto_route: bool,             // true
    pub dns_hijack: Vec<String>,      // ["any:53"]
    pub mtu: u16,                     // 9000
}
impl Default for TunSettings

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DnsSettings {
    pub enable: bool,                 // true
    pub listen: String,               // "0.0.0.0:1053"
    pub enhanced_mode: String,        // "fake-ip"
    pub fake_ip_range: String,        // "198.18.0.1/16"
    pub nameserver: Vec<String>,      // ["https://doh.pub/dns-query"]
    pub default_nameserver: Vec<String>, // ["223.5.5.5"]
    pub fallback: Vec<String>,        // ["tls://8.8.4.4"]
    pub fake_ip_filter: Vec<String>,  // ["*.lan", "+.local"]
}
impl Default for DnsSettings

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Subscription {
    pub name: String,
    pub url: String,
    #[serde(default)] pub last_fetch: Option<String>,  // RFC3339
    #[serde(default)] pub active: bool,
    #[serde(default)] pub cache: Option<SubscriptionCache>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SubscriptionCache {
    pub proxies: Vec<ProxyNode>,
    pub proxy_groups: Vec<serde_yaml::Value>, // 原始组映射（保真再输出）
    pub rules: Vec<String>,                   // 原始规则串
    pub fetched_at: String,                   // RFC3339
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProxyNode {
    pub name: String,
    pub kind: String,      // ss|vmess|vless|trojan|ssr|hysteria2|tuic
    pub yaml: serde_yaml::Value, // 完整节点映射（保真再输出）
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct Overrides {
    #[serde(default)] pub groups: Vec<UserGroup>,
    #[serde(default)] pub rules: Vec<UserRule>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UserGroup {
    pub name: String,
    pub group_type: String,        // "select"|"url-test"|"fallback"
    #[serde(default = "default_test_url")] pub url: String,
    #[serde(default = "default_group_interval")] pub interval: u64,
    #[serde(default)] pub tolerance: u64,
    #[serde(default)] pub proxies: Vec<String>, // 组员=订阅节点名
}
pub fn default_test_url() -> String        // "http://www.gstatic.com/generate_204"
pub fn default_group_interval() -> u64     // 300

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UserRule {
    pub rule_type: String,  // DOMAIN|DOMAIN-SUFFIX|DOMAIN-KEYWORD|GEOIP|PROCESS-NAME|MATCH
    pub payload: String,    // MATCH 时为空串
    pub target: String,
}

// ============ core/settings.rs ============
pub fn config_dir() -> PathBuf                       // $HOME/.config/mihomo-tui（不存在则创建）
pub fn settings_path() -> PathBuf
pub fn subscriptions_path() -> PathBuf
pub fn overrides_path() -> PathBuf
pub fn load_settings() -> Result<NetworkSettings, SettingsError>   // 缺失→默认并落盘
pub fn save_settings(s: &NetworkSettings) -> Result<(), SettingsError>
pub fn load_subscriptions() -> Result<Vec<Subscription>, SettingsError> // 缺失→空
pub fn save_subscriptions(s: &[Subscription]) -> Result<(), SettingsError>
pub fn load_overrides() -> Result<Overrides, SettingsError>        // 缺失→默认
pub fn save_overrides(o: &Overrides) -> Result<(), SettingsError>
pub fn generate_secret() -> String                    // 16 字节 /dev/urandom → 32 hex
#[derive(Debug, thiserror::Error)] pub enum SettingsError { #[error("{0}")] Io(String), #[error("{0}")] Toml(String), #[error("{0}")] Yaml(String) }
// 写文件用 写临时文件+rename 原子替换；toml 存 NetworkSettings（Tun/Dns 嵌套）、
// subscriptions 存 Vec<Subscription>、overrides 存 Overrides（toml 对 serde_yaml::Value
// 不友好 → subscriptions/overrides 用 serde_yaml 序列化，文件仍是 .toml 后缀但内容 YAML。
// 注意：保持此约定，测试按 YAML 语义写。

// ============ core/subscription.rs ============
pub enum SubscriptionKind { Yaml, ShareLinks }
pub fn detect_kind(content: &str) -> SubscriptionKind
   // 规则：trim 后以 '{' 或含 "proxies:" 或 "proxy-groups:" 开头→Yaml；
   // 尝试 base64(standard/url-safe/去填充) 解码→解码后若是 YAML（含 "proxies:" 或 "port:" 等键）→Yaml；
   // 其余（含解码后）→ShareLinks（任意行以 vmess://|vless://|trojan://|ss://|ssr://|hysteria2://|tuic:// 开头）
pub async fn fetch_subscription(url: &str, via_proxy_port: Option<u16>) -> Result<String, FetchError>
   // reqwest GET（UA: mihomo-tui/0.1，timeout 20s）；失败且 via_proxy_port=Some(p) 时
   // 经 http://127.0.0.1:p 代理重试；最大响应 10MB
pub fn parse_subscription(content: &str) -> Result<SubscriptionCache, ParseError>
   // Yaml：serde_yaml 解析顶层，取 proxies(数组)/proxy-groups(数组)/rules(字符串数组)；
   //   proxies 缺失但存在 proxy-providers → Err("暂不支持 proxy-providers 订阅")
   //   proxies 缺失 → Err("订阅中没有 proxies 节点")
   //   ProxyNode.name 取映射的 name 字段（缺失→跳过+计数，不报错）；kind 取 type 字段
   // ShareLinks：按行 parse_share_link，忽略空行/注释行
pub fn parse_share_link(line: &str) -> Result<ProxyNode, ParseError>
   // 按 scheme 分发到 parsers::*；name 优先级：URI fragment(#) > 协议内名称字段 > "未命名-<n>"
   // 节点 yaml 统一补 "udp": true
#[derive(Debug, thiserror::Error)] pub enum FetchError { #[error("网络错误: {0}")] Network(String), #[error("HTTP {0}")] Http(u16), #[error("{0}")] Other(String) }
#[derive(Debug, thiserror::Error)] pub enum ParseError { #[error("{0}")] Message(String) }

// ============ core/parsers/mod.rs ============
pub mod vmess; pub mod vless; pub mod trojan; pub mod ss; pub mod ssr; pub mod hysteria2; pub mod tuic;
// 每个解析器：pub fn parse(line: &str) -> Result<(String /*name*/, serde_yaml::Mapping), ParseError>
// 返回 (名称, 映射)；映射含 type/server/port + 协议字段；名称来自 fragment/#ps；无名称→空串由上层兜底
// 输出字段遵循 mihomo 配置格式（见 §4 fixtures 与验收）

// ============ core/merger.rs ============
pub const AUTO_GROUP_NAME: &str = "🚀 节点选择";
pub const DEFAULT_RULES: [&str; 2] = ["GEOIP,CN,DIRECT", "MATCH,🚀 节点选择"];

pub struct MergeContext<'a> {
    pub settings: &'a NetworkSettings,
    pub overrides: &'a Overrides,
    pub subscription: Option<&'a Subscription>, // 激活订阅
}
pub struct MergeOutput { pub config: String, pub warnings: Vec<String> }
#[derive(Debug, thiserror::Error)] #[error("{message}")]
pub struct MergeError { pub message: String }
pub fn merge(ctx: MergeContext) -> Result<MergeOutput, MergeError>
// 输出 config.yaml 组装（serde_yaml::Mapping 保序，顶层键顺序）：
//   1) 网络段：port/socks-port/mixed-port/allow-lan/mode/ipv6/log-level/
//      external-controller/secret/tun{enable,stack,auto-route,dns-hijack,mtu}/dns{全部字段}
//      （tun.enable 也写入文件——重启后保持；运行时开关走 API）
//   2) proxy-groups = 自定义组 + 订阅组（去重后）+ 自动组（需要时）
//      select 组：{name,type,proxies:[成员]}；url-test：+url,interval；fallback：+url,interval,tolerance
//   3) rules = 自定义规则 + 订阅规则（去重后）+ 默认模板规则（需要时）
//      规则串：MATCH → "MATCH,target"；其余 → "TYPE,payload,target"
//   4) proxies = 订阅节点（去重后）
//   去重规则（全部记 warning）：
//     - 订阅 proxies 内重名：保留第一个
//     - 自定义组名与订阅组名冲突：保留自定义，丢弃订阅同名组
//     - 订阅组名与节点名冲突：丢弃该订阅组（节点名优先）
//     - 自定义组名与节点名冲突：MergeError（用户必须改名）
//   兜底模板（记 warning）：
//     - 订阅有节点但最终组列表为空 → 注入 {name:AUTO_GROUP_NAME, type:select, proxies:全部节点名}
//     - 订阅 rules 为空 且 节点非空 → 注入 DEFAULT_RULES（若组列表为空先注入自动组）
//   引用校验（校验对象=最终集）：
//     - targets = 节点名 ∪ 组名 ∪ BUILTIN_TARGETS
//     - 自定义规则 target ∉ targets → MergeError（消息含规则与缺失项）
//     - 自定义组成员 ∉ 节点名 → MergeError（消息含组与缺失成员）
//     - 订阅规则 target ∉ targets → 丢弃该规则 + warning
//     - 订阅组成员 ∉ 节点名 → 丢弃该成员 + warning；成员清空后组丢弃 + warning
//   返回 MergeOutput{config: yaml 字符串, warnings}

// ============ core/client.rs ============
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct RuntimeConfig { pub mode: String, pub ipv6: bool, pub tun_enable: bool }
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct TrafficFrame { pub up: u64, pub down: u64, pub up_total: u64, pub down_total: u64 }
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct MemoryFrame { pub inuse: u64 }

pub struct Client { /* base: String, secret: String, http: reqwest::Client */ }
impl Client {
    pub fn new(settings: &NetworkSettings) -> Self   // base=http://{external_controller}
    pub async fn ping(&self) -> Result<(), ApiError> // GET /version，非 2xx 或超时→Err
    pub async fn get_configs(&self) -> Result<RuntimeConfig, ApiError> // GET /configs
    pub async fn patch_configs(&self, patch: serde_json::Value) -> Result<(), ApiError> // PATCH /configs
    pub async fn traffic_stream(&self) -> Result<impl Stream<Item = Result<TrafficFrame, ApiError>> + Unpin, ApiError>
    pub async fn memory_stream(&self) -> Result<impl Stream<Item = Result<MemoryFrame, ApiError>> + Unpin, ApiError>
}
#[derive(Debug, thiserror::Error)] pub enum ApiError { #[error("{0}")] Http(String), #[error("连接失败: {0}")] Conn(String), #[error("HTTP 状态 {0}")] Status(u16), #[error("{0}")] Json(String) }
// 鉴权：secret 非空 → Authorization: Bearer {secret}
// traffic_stream：GET /traffic → bytes_stream → 按行切分 → serde_json 每行 {up,down,upTotal,downTotal}
//   （字段可能缺失→0）；stream 结束/错误 → Err 由调用方重连
// memory_stream：GET /memory → 同上 {inuse,os}

// ============ core/apply.rs ============
pub struct ApplyOutcome { pub success: bool, pub stdout: String, pub stderr: String }
#[derive(Debug, thiserror::Error)]
pub enum ApplyError {
    #[error("sudo 不可用")] SudoNotAvailable,
    #[error("sudo 需要密码（交互模式）")] SudoNeedsPassword,
    #[error("mihomo -t 校验失败:\n{stderr}")] ValidateFailed { stderr: String },
    #[error("执行失败:\n{stdout}\n{stderr}")] CommandFailed { stdout: String, stderr: String },
    #[error("{0}")] Io(String),
}
pub async fn validate_config(yaml: &str) -> Result<(), ApplyError>
   // 写 ~/.config/mihomo-tui/.validate.tmp.yaml → `mihomo -t -f <tmp>` 捕获 stderr
   // 退出码非 0 → ValidateFailed{stderr}（mihomo 输出原样反馈给用户）
pub async fn apply_config(yaml: &str, non_interactive: bool) -> Result<ApplyOutcome, ApplyError>
   // 写临时文件 → 运行 `sudo [-n] /usr/local/sbin/mihomo-apply < tmp`（tokio::process, stdin 喂入）
   // non_interactive=true 且退出码≠0 且 stderr 含 "password" → SudoNeedsPassword
   // 退出码≠0 → CommandFailed；=0 → ApplyOutcome{success:true, stdout, stderr}
pub async fn is_apply_script_installed() -> bool   // -x /usr/local/sbin/mihomo-apply
pub async fn service_is_active() -> bool           // systemctl is-active --quiet mihomo
pub async fn mihomo_is_installed() -> bool         // which mihomo
```

## 3. UI 契约（B1 实现，B2 依赖）

```rust
// ============ app.rs ============
pub struct AppState {
    pub settings: NetworkSettings,
    pub subs: Vec<Subscription>,
    pub overrides: Overrides,
    pub runtime: RuntimeConfig,          // API 读取的运行时状态
    pub api_ok: bool,                    // 最近 ping/stream 是否成功
    pub traffic: VecDeque<TrafficFrame>, // 最近 120 帧（dashboard 曲线）
    pub mem_history: VecDeque<u64>,      // 最近 120 个 inuse
    pub exit_ip: Option<String>,
    pub notices: VecDeque<String>,       // 最近 5 条（底栏显示 3 条；前缀 [✓]/[✗]）
}
pub enum UiCommand {  // 页面 → 主循环（异步操作请求）
    PatchConfigs(serde_json::Value),
    ApplyConfig(String),                 // 已合并好的 yaml（先 validate_config 再 sudo）
    FetchSubscription(usize),            // subs 索引；重拉+解析+存盘
    FetchExitIp,
    ReloadConfigs,                       // GET /configs 刷新 runtime
    InstallSetup,                        // 触发 service::installer
}
pub enum UiEvent {   // 后台任务 → 主循环
    PatchDone(Result<(), String>),
    ApplyDone(Result<ApplyOutcome, String>),
    SubscriptionFetched(usize, Result<SubscriptionCache, String>),
    ExitIp(Result<String, String>),
    ConfigsRefreshed(Result<RuntimeConfig, String>),
}
pub async fn run() -> anyhow::Result<()>
// 初始化：AppState 载入本地三文件；spawn 后台任务：
//   traffic 循环：Client::traffic_stream() → mpsc<TrafficFrame>；流错误→sleep 2s 重连；api_ok 联动
//   memory 循环：同上（首帧 inuse==0 忽略）
//   exit_ip 循环：每 60s + FetchExitIp 命令触发；经 mixed_port 代理 GET https://api.ipify.org
//     （失败降级 http://api.ipify.org → https://ifconfig.me/ip），文本即 IP；全失败→显示"未知"
// 主循环 tokio::select!：
//   crossterm EventStream::next() → 分发：popup 优先，否则当前页 handle_key；q/Ctrl-C 退出
//   1s tick → 每 5 tick 发 ReloadConfigs；重绘（每事件+每 tick 都 draw）
//   traffic_rx / memory_rx → 更新 AppState
//   ui_event_rx → 更新状态 + popup/notice（失败→MessagePopup 完整错误；成功→notice）
//   commands_rx → spawn 对应任务（PATCH 超时 5s；apply 前先 validate_config）
// apply 交互：ApplyConfig 失败 SudoNeedsPassword → ConfirmPopup("需要 sudo 密码，将以交互模式重试")
//   → 用户确认 → 离开 raw 模式/AltScreen → apply_config(interactive) → 恢复 → 结果 popup
//   （终端恢复用 crossterm::terminal::disable_raw_mode + LeaveAlternateScreen，finally 恢复）
// 渲染：Tabs 顶栏 + 当前页 render + 底栏 KeyHints + notices；popup 置顶渲染

// ============ ui/mod.rs ============
pub mod dashboard; pub mod subscriptions; pub mod groups; pub mod rules; pub mod widgets;
pub trait Page {
    fn handle_key(&mut self, key: KeyEvent, st: &mut AppState) -> Option<UiCommand>;
    fn render(&mut self, f: &mut Frame, area: Rect, st: &AppState);
}
// 页面内部可持有自己的 popup 状态（FormPopup 等），render 时最后绘制

// ============ ui/widgets.rs（B1，B2 仅调用，API 如下） ============
pub enum FormAction { Confirm, Cancel }
pub enum FieldKind { Text, Dropdown(Vec<String>), Number }
pub struct FormField { pub label: String, pub value: String, pub kind: FieldKind }
pub struct FormPopup { /* title, fields, focused, ... */ }
impl FormPopup {
    pub fn new(title: String, fields: Vec<FormField>) -> Self
    pub fn handle_key(&mut self, key: KeyEvent) -> Option<FormAction>
    pub fn render(&mut self, f: &mut Frame, area: Rect)
    pub fn values(&self) -> Vec<String>   // 与 fields 顺序一致
}
// Text/Number: 字符输入/退格/Delete/←→/Home/End；Dropdown: ←→ 循环选项；Tab/↓: 下一字段；↑: 上一字段
// Enter=Confirm, Esc=Cancel
pub enum CheckAction { Confirm, Cancel }
pub struct CheckboxList { /* title, items, checked, selected, filter */ }
impl CheckboxList {
    pub fn new(title: String, items: Vec<String>) -> Self  // 全部默认未选中
    pub fn handle_key(&mut self, key: KeyEvent) -> Option<CheckAction>
    pub fn render(&mut self, f: &mut Frame, area: Rect)
    pub fn selected_items(&self) -> Vec<String>
}
// j/k/↑↓ 移动；Space 勾选；/ 或字母输入过滤（过滤时列表只显示匹配项，匹配项勾选状态保留）；
// Enter=Confirm(返回选中项), Esc=Cancel
pub struct ConfirmPopup { /* title, message */ }
impl ConfirmPopup {
    pub fn new(title: String, message: String) -> Self
    pub fn handle_key(&mut self, key: KeyEvent) -> Option<bool>  // Some(true)=yes Some(false)=no
    pub fn render(&mut self, f: &mut Frame, area: Rect)
}
// y/Enter=yes, n/Esc=no
pub struct MessagePopup { /* title, lines, scroll */ }
impl MessagePopup {
    pub fn new(title: String, lines: Vec<String>) -> Self
    pub fn handle_key(&mut self, key: KeyEvent) -> bool  // true=关闭（Esc/Enter/q；↑↓/PgUp/PgDn 滚动）
    pub fn render(&mut self, f: &mut Frame, area: Rect)
}
pub struct SelectList { /* items, selected, offset */ }
impl SelectList {
    pub fn new(items: Vec<String>) -> Self
    pub fn handle_key(&mut self, key: KeyEvent) // j/k/↑↓ 移动+滚动
    pub fn selected(&self) -> usize
    pub fn render(&mut self, f: &mut Frame, area: Rect)
}
pub struct KeyHints { pub hints: Vec<(String, String)> }  // (键, 说明)
impl KeyHints { pub fn render(&self, f: &mut Frame, area: Rect) }
pub fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect
pub fn format_bytes(n: u64) -> String   // B/KB/MB/GB/TB，1 位小数，如 "1.2 GB"
pub fn format_rate(n: u64) -> String    // format_bytes + "/s"
```

**页面内部状态约定（B2 按此实现，B1 的 app.rs 按此使用）**：
- 每页 `pub struct XPage { list: SelectList, popup: Option<XPopup>, ... }`，`impl Page for XPage`，`pub fn new() -> Self`
- popup 打开时 `handle_key` 先喂 popup，返回 None；popup 关闭后恢复页面键处理
- 页面内对 `st` 的本地修改（如切换激活标记、增删 override）需要同步落盘（直接调 core::settings 的 save_*）

**页面交互规格**：

*Dashboard（B1）* 顶栏（tabs 下方）：`模式: rule [m] | TUN: on [t] | IPv6: on [6] | 出口IP: 1.2.3.4 [r] | API: 已连接`；中部左 60%：实时网速（up 绿 sparkline + 当前 up/s、down 蓝 sparkline + down/s，取自 st.traffic）；右 40%：总流量（↑ upTotal / ↓ downTotal 大字）+ 内存（inuse + sparkline）；`s`=网络设置表单（字段：port/socks-port/mixed-port/allow-lan(是/否 Dropdown)/log-level(Dropdown silent|error|warning|info|debug)/tun.stack(Dropdown system|gvisor|mixed)/tun.auto-route(是/否)/tun.mtu/tun.dns-hijack(逗号分隔)/dns.enable(是/否)/dns.nameserver(逗号分隔)）→ 存 settings → 合并 → validate → apply（结构性流程，成功后更新 st.settings）。

*Subscriptions（B2）* 列表行：`[★] 名称 | 节点N 组N 规则N | 上次拉取`；`a`=添加表单(name,url)→FetchSubscription；`Enter`=激活：标记 active→save→merge→(Err→popup；Ok→ApplyConfig)；`r`=刷新选中；`d`=删除(Confirm)。FetchSubscription 成功→更新 cache+last_fetch+存盘→notice 显示节点/组/规则数；失败→MessagePopup 完整错误。

*Groups（B2）* 列表行：`名称 | 类型 | 成员数 | url | interval`；`n`=新建表单（名称/类型 Dropdown/url/interval/tolerance）；`Enter`=编辑表单；`m`=成员 CheckboxList（items=激活订阅节点名，预勾选当前成员）；`d`=删除(Confirm)。保存后 save_overrides。

*Rules（B2）* 列表行：`DOMAIN, example.com, 🚀 节点选择`；`n`=新建表单（类型 Dropdown 6 种/payload/目标 Dropdown：BUILTIN_TARGETS+自定义组+激活订阅组名，MATCH 时隐藏 payload 字段）；`Enter`=编辑；`d`=删除(Confirm)；`K`/`J`=上移/下移（落盘）。规则串构建/解析 helper 放本页：`fn rule_to_string(r:&UserRule)->String`、`fn parse_rule(s:&str)->Option<UserRule>`。

**全局快捷键**：`Tab`/`←→`/`1-4` 切页；`?` 帮助 popup（MessagePopup 列出全部按键）；`q`/`Esc`(无 popup)/`Ctrl-C` 退出。

## 4. 协议解析器 fixtures（Worker A 验收标准）

每个解析器：`parse(line) -> Result<(String, Mapping)>`，Mapping 的字段名/值必须与 mihomo 配置格式一致。测试 fixture（输入→期望关键字段）：

- **vless**: `vless://3b1b1b1b-...-uuid@1.2.3.4:443?type=ws&security=tls&sni=cdn.example.com&fp=chrome&host=cdn.example.com&path=%2Fws%3Fedge%3D1&encryption=none#🇯🇵 JP` →
  `{type:vless, server:1.2.3.4, port:443, uuid:<uuid>, tls:true, servername:cdn.example.com, network:ws, client-fingerprint:chrome, ws-opts:{path:"/ws?edge=1", headers:{Host:cdn.example.com}}}`；name="🇯🇵 JP"
  security=none → tls:false；security=reality → `reality-opts:{public-key,short-id}`；type=grpc → `grpc-opts:{grpc-service-name:path}`；flow 存在则保留
- **vmess**: `vmess://` + base64(`{"v":"2","ps":"测试节点","add":"1.2.3.4","port":"443","id":"<uuid>","aid":"0","scy":"auto","net":"ws","type":"none","host":"h.example.com","path":"/ws","tls":"tls","sni":"s.example.com","alpn":"h2,http/1.1","fp":"chrome"}`) →
  `{type:vmess, server:1.2.3.4, port:443, uuid:<uuid>, alterId:0, cipher:auto, udp:true, tls:true, servername:s.example.com, network:ws, client-fingerprint:chrome, alpn:["h2","http/1.1"], ws-opts:{path:"/ws", headers:{Host:h.example.com}}}`；name="测试节点"；tls="none"→tls:false；net=tcp→network:tcp 无 ws-opts；net=grpc→grpc-opts
- **trojan**: `trojan://pass123@1.2.3.4:443?sni=cdn.example.com&allowInsecure=1&type=ws&host=h.example.com&path=%2Fws#Trojan-WS` →
  `{type:trojan, server:1.2.3.4, port:443, password:pass123, sni:cdn.example.com, skip-cert-verify:true, network:ws, ws-opts:{path:"/ws", headers:{Host:h.example.com}}}`；name="Trojan-WS"；allowInsecure=0/缺失→skip-cert-verify:false
- **ss**: 新格式 `ss://` + base64(`aes-128-gcm:pass123`) + `@1.2.3.4:8388#SS节点` →
  `{type:ss, server:1.2.3.4, port:8388, cipher:aes-128-gcm, password:pass123, udp:true}`；name="SS节点"
  旧格式：`ss://` + base64(`aes-128-gcm:pass123@1.2.3.4:8388`)（无 @ 分隔的 host:port 部分）→ 同上
  plugin：`...?plugin=obfs-local%3Bobfs%3Dhttp%3Bobfs-host%3Dcdn.example.com` → `plugin:obfs-local, plugin-opts:"obfs=http;obfs-host=cdn.example.com"`
- **ssr**: `ssr://` + base64(`1.2.3.4:8388:auth_aes128_md5:chacha20-ietf:tls1.2_ticket_auth:base64(pass)/?obfsparam=base64(http_post)&protoparam=&remarks=base64(SSR节点)&group=base64(g)`) →
  `{type:ssr, server:1.2.3.4, port:8388, cipher:chacha20-ietf, password:pass, protocol:auth_aes128_md5, obfs:tls1.2_ticket_auth, protocol-param:"", obfs-param:http_post}`；name="SSR节点"
- **hysteria2**: `hysteria2://pass@1.2.3.4:8443?sni=cdn.example.com&insecure=1&obfs=salamander&obfs-password=obfs-pass#Hy2` →
  `{type:hysteria2, server:1.2.3.4, port:8443, password:pass, sni:cdn.example.com, skip-cert-verify:true, obfs:salamander, obfs-password:obfs-pass}`；name="Hy2"
- **tuic**: `tuic://<uuid>:<pass>@1.2.3.4:443?sni=cdn.example.com&alpn=h3&congestion_control=bbr&udp_relay_mode=native#Tuic` →
  `{type:tuic, server:1.2.3.4, port:443, uuid:<uuid>, password:<pass>, alpn:["h3"], congestion-controller:bbr, udp-relay-mode:native, sni:cdn.example.com}`；name="Tuic"

URL 组件用 url crate；query 值 percent-decode；base64 解码 tri-state（standard/url_safe/无填充）。

## 5. Merger 测试清单（必须全部覆盖）

1. 完整合并：网络段字段齐全、键顺序、自定义组/规则在订阅组/规则前、proxies 存在
2. 去重：自定义组名与订阅组同名 → 保留自定义 + warning
3. 去重：订阅内 proxies 重名 → 保留首个 + warning
4. 去重：订阅组名与节点名冲突 → 丢弃订阅组 + warning
5. 校验：自定义规则 target 缺失 → MergeError 含规则名/缺失 target
6. 校验：自定义组成员缺失 → MergeError
7. 兜底：仅节点无组无规则 → 自动组+默认规则注入 + warning
8. 兜底：无激活订阅 → 仅网络+自定义段，无 proxies/无模板
9. 订阅规则引用被丢弃的组 → 该规则被丢弃 + warning
10. 订阅组成员不存在 → 成员被丢弃 + warning
11. 内置 target（DIRECT 等）合法
12. MATCH 规则序列化为 "MATCH,target"（无 payload）
13. GEOIP 规则 → "GEOIP,CN,DIRECT"
14. 自定义组名与节点名冲突 → MergeError

settings.rs 测试：默认值落盘/回读、generate_secret 长度与 hex、缺失文件→默认。subscription.rs 测试：detect_kind 四种样本（yaml/base64-yaml/base64-links/plain-links）、yaml 解析计数、proxy-providers 报错、空输入报错。

## 6. Worker 任务划分

| Worker | 范围 | 文件 |
|---|---|---|
| A | core 全部 | src/core/**、examples/merge_sample.rs |
| B1 | ui 基础+首页+主循环 | src/main.rs、src/app.rs、src/ui/mod.rs、src/ui/widgets.rs、src/ui/dashboard.rs |
| B2 | 三个管理页 | src/ui/subscriptions.rs、src/ui/groups.rs、src/ui/rules.rs |
| C | 服务+文档 | src/service/installer.rs、README.md |

**Worker A**（最大任务，分步：models+settings → parsers → subscription → merger → client+apply → examples）：
- 每步 TDD：先写测试→跑红→实现→跑绿→commit（commit 信息如 `feat(core): vmess parser`）
- 契约见 §2/§4/§5；serde_yaml::Mapping 构建用 `serde_yaml::Mapping::new()` + insert
- examples/merge_sample.rs：读环境变量 MIHOMO_TUI_SETTINGS_DIR（缺省 ~/.config/mihomo-tui）加载三文件→merge→println 输出 yaml；供 `mihomo -t -f` 集成验证
- 完成后跑 `cargo test` 全绿 + `cargo clippy -- -D warnings`（core 内）

**Worker B1**：契约见 §3；完成后 `cargo build` 通过（B2 页面可能尚未就绪——ui/mod.rs 中三个页面模块由 B2 创建，B1 需在 app.rs 中用 `pub fn new()` 引用，若 B2 未完成可先用占位实现保证编译；最终以 B2 版本为准）
- main.rs：panic hook 恢复终端、enable_raw_mode+EnterAlternateScreen、app::run 后恢复
- 注意 ratatui 0.30 API（Block::borders(Borders::ALL)、Sparkline::default().data(&data).style(...)）

**Worker B2**：契约见 §3；页面 popup 交互自测方式：`cargo build` + 逻辑审查（无 TUI 单测），保证 handle_key 状态机正确；表单值解析（str→u16/u64/bool）失败→页面内 MessagePopup 提示

**Worker C**：
- src/service/installer.rs：`pub async fn needs_install() -> bool`（脚本或 sudoers 缺失）、`pub async fn install() -> Result<Vec<String>, InstallError>`（日志行数组；步骤：检查 mihomo 二进制→检查 service 单元→groupadd --system mihomo-admin→写 /usr/local/sbin/mihomo-apply(root:root 0755, include_str!("../../resources/mihomo-apply.sh"))→写 /etc/sudoers.d/99-mihomo(0440, `%mihomo-admin ALL=(root) NOPASSWD: /usr/local/sbin/mihomo-apply`+`visudo -cf` 校验)→usermod -aG mihomo-admin $USER→可选 enable --now mihomo）；sudo 一律交互模式（std::process::Command + status 等待，install 在 UI 确认后由 app 触发，先恢复终端）
- README.md：功能总览、安装（TUI 首启引导 + 手动步骤）、按键表、架构、配置文件说明、FAQ（sudo 密码/重登录、TUN 权限、订阅格式、常见合并错误）

**集成（lead 执行）**：cargo build/test/clippy 全绿 → `cargo run --example merge_sample | mihomo -t -f -`?（-t 不支持 stdin，用临时文件）→ 用真实 mihomo 验证合并产物 → 跨 worker 契约不一致修复 → 组织 reviewer 审查。

## 7. 验收清单

- [ ] cargo build 无警告；cargo test 全绿（merger ≥14 测试、parsers ≥14 测试、settings/subscription 测试）
- [ ] cargo clippy -- -D warnings 通过
- [ ] `cargo run --example merge_sample` 产物通过真实 `mihomo -t -f` 校验（本机 mihomo 1.19.29 已装）
- [ ] UI 四页 + 弹窗 + 快捷键按 §3 规格实现；错误反馈（MessagePopup/notice/API 断连指示）齐全
- [ ] README 完整
