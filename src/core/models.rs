//! 数据模型：网络设置、订阅、用户覆盖、代理节点。
//! 字段与默认值严格遵循 docs/superpowers/plans/2026-08-10-mihomo-tui.md §2。

use serde::{Deserialize, Serialize};

use crate::core::settings::generate_secret;

/// 内置规则目标（不可用作组名/节点名）。
pub const BUILTIN_TARGETS: [&str; 7] = [
    "DIRECT",
    "REJECT",
    "REJECT-DROP",
    "COMPATIBLE",
    "PASS",
    "PASS-RULE",
    "GLOBAL",
];

/// 网络段配置（等价于 mihomo config.yaml 的网络字段 + secret）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkSettings {
    /// "rule" | "global" | "direct"
    pub mode: String,
    pub ipv6: bool,
    pub allow_lan: bool,
    /// 7890
    pub port: u16,
    /// 7891
    pub socks_port: u16,
    /// 7892
    pub mixed_port: u16,
    /// "info"
    pub log_level: String,
    /// "127.0.0.1:9090"
    pub external_controller: String,
    /// 随机 32 hex（每次 Default 重新生成）
    pub secret: String,
    pub tun: TunSettings,
    pub dns: DnsSettings,
}

impl Default for NetworkSettings {
    fn default() -> Self {
        Self {
            mode: "rule".into(),
            ipv6: false,
            allow_lan: false,
            port: 7890,
            socks_port: 7891,
            mixed_port: 7892,
            log_level: "info".into(),
            external_controller: "127.0.0.1:9090".into(),
            secret: generate_secret(),
            tun: TunSettings::default(),
            dns: DnsSettings::default(),
        }
    }
}

/// TUN 段配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TunSettings {
    pub enable: bool,
    /// "system" | "gvisor" | "mixed"
    pub stack: String,
    pub auto_route: bool,
    pub dns_hijack: Vec<String>,
    pub mtu: u16,
}

impl Default for TunSettings {
    fn default() -> Self {
        Self {
            enable: false,
            stack: "mixed".into(),
            auto_route: true,
            dns_hijack: vec!["any:53".into()],
            mtu: 9000,
        }
    }
}

/// DNS 段配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DnsSettings {
    pub enable: bool,
    pub listen: String,
    /// "fake-ip" | "redir-host"
    pub enhanced_mode: String,
    pub fake_ip_range: String,
    pub nameserver: Vec<String>,
    pub default_nameserver: Vec<String>,
    pub fallback: Vec<String>,
    pub fake_ip_filter: Vec<String>,
}

impl Default for DnsSettings {
    fn default() -> Self {
        Self {
            enable: true,
            listen: "0.0.0.0:1053".into(),
            enhanced_mode: "fake-ip".into(),
            fake_ip_range: "198.18.0.1/16".into(),
            nameserver: vec!["https://doh.pub/dns-query".into()],
            default_nameserver: vec!["223.5.5.5".into()],
            // fallback 默认用国内可达 DoT（阿里云 + DNSPod 双冗余）。
            // 历史故障：8.8.4.4:853 在中国大陆网络不可达，导致 mihomo 国外域名解析全失败
            // （"all DNS requests failed"），走代理的国外端点全部不可用。
            // default_nameserver 223.5.5.5 负责 bootstrap 解析这两个 DoT 域名，端口用默认 853。
            fallback: vec!["tls://dns.alidns.com".into(), "tls://dot.pub".into()],
            fake_ip_filter: vec!["*.lan".into(), "+.local".into()],
        }
    }
}

/// 一条订阅。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Subscription {
    pub name: String,
    pub url: String,
    /// RFC3339 时间串
    #[serde(default)]
    pub last_fetch: Option<String>,
    #[serde(default)]
    pub active: bool,
    #[serde(default)]
    pub cache: Option<SubscriptionCache>,
}

/// 订阅解析缓存（保真，供合并器原样输出）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SubscriptionCache {
    pub proxies: Vec<ProxyNode>,
    /// 原始组映射（保真再输出）
    pub proxy_groups: Vec<serde_yaml::Value>,
    /// 原始规则串
    pub rules: Vec<String>,
    /// RFC3339 时间串
    pub fetched_at: String,
}

/// 代理节点（名称 + 类型 + 完整 yaml 映射保真）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProxyNode {
    pub name: String,
    /// ss|vmess|vless|trojan|ssr|hysteria2|tuic
    pub kind: String,
    /// 完整节点映射（保真再输出）
    pub yaml: serde_yaml::Value,
}

/// 用户覆盖配置（自定义规则；groups 已废弃，仅保留反序列化以迁移旧数据）。
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct Overrides {
    /// 已废弃：仅用于读取旧 overrides.toml 迁移（启动时清空），合并器/UI 不再使用。
    #[serde(default)]
    pub groups: Vec<UserGroup>,
    #[serde(default)]
    pub rules: Vec<UserRule>,
}

/// 自定义规则组（已废弃：仅用于反序列化旧 overrides.toml 以启动迁移，不再创建）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UserGroup {
    pub name: String,
    /// "select" | "url-test" | "fallback"
    pub group_type: String,
    #[serde(default = "default_test_url")]
    pub url: String,
    #[serde(default = "default_group_interval")]
    pub interval: u64,
    #[serde(default)]
    pub tolerance: u64,
    /// 组员 = 节点名 / 其他策略组名 / 内置目标名（如 DIRECT）
    #[serde(default)]
    pub proxies: Vec<String>,
}

/// 测速 URL 默认值。
pub fn default_test_url() -> String {
    "http://www.gstatic.com/generate_204".into()
}

/// 组测速间隔默认值（秒）。
pub fn default_group_interval() -> u64 {
    300
}

/// 自定义规则。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UserRule {
    /// DOMAIN|DOMAIN-SUFFIX|DOMAIN-KEYWORD|GEOIP|PROCESS-NAME|MATCH
    pub rule_type: String,
    /// MATCH 时为空串
    pub payload: String,
    pub target: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn network_defaults() {
        let s = NetworkSettings::default();
        assert_eq!(s.mode, "rule");
        assert!(!s.ipv6);
        assert!(!s.allow_lan);
        assert_eq!(s.port, 7890);
        assert_eq!(s.socks_port, 7891);
        assert_eq!(s.mixed_port, 7892);
        assert_eq!(s.log_level, "info");
        assert_eq!(s.external_controller, "127.0.0.1:9090");
    }

    #[test]
    fn tun_defaults() {
        let t = TunSettings::default();
        assert!(!t.enable);
        assert_eq!(t.stack, "mixed");
        assert!(t.auto_route);
        assert_eq!(t.dns_hijack, vec!["any:53"]);
        assert_eq!(t.mtu, 9000);
    }

    #[test]
    fn dns_defaults() {
        let d = DnsSettings::default();
        assert!(d.enable);
        assert_eq!(d.listen, "0.0.0.0:1053");
        assert_eq!(d.enhanced_mode, "fake-ip");
        assert_eq!(d.fake_ip_range, "198.18.0.1/16");
        assert_eq!(d.nameserver, vec!["https://doh.pub/dns-query"]);
        assert_eq!(d.default_nameserver, vec!["223.5.5.5"]);
        assert_eq!(d.fallback, vec!["tls://dns.alidns.com", "tls://dot.pub"]);
        assert_eq!(d.fake_ip_filter, vec!["*.lan", "+.local"]);
    }

    #[test]
    fn secret_is_32_hex() {
        let s = NetworkSettings::default();
        assert_eq!(s.secret.len(), 32);
        assert!(s.secret.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn builtin_targets() {
        assert_eq!(BUILTIN_TARGETS.len(), 7);
        assert!(BUILTIN_TARGETS.contains(&"DIRECT"));
        assert!(BUILTIN_TARGETS.contains(&"GLOBAL"));
    }

    #[test]
    fn default_group_helpers() {
        assert_eq!(default_test_url(), "http://www.gstatic.com/generate_204");
        assert_eq!(default_group_interval(), 300);
    }
}
