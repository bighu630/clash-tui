//! 设置页：config.yaml 可配置项集中编辑（TUN/DNS/网络/端口/日志/其他）。
//! 交互规格见 docs/superpowers/specs/2026-08-12-settings-tab-design.md。
//! 本文件先落纯函数（模型↔表单转换与校验），SettingsPage 页面在后续任务追加。
// 中间态：纯函数仅供 Task 3 的 SettingsPage 使用，在此之前先抑制 dead_code；
// 后续任务挂载 SettingsPage 后应移除本属性。
#![allow(dead_code)]

use crate::core::models::{DnsSettings, NetworkSettings, TunSettings};
use crate::ui::widgets::{FieldKind, FormField};

/// 区块定义：(标题, 字段起始索引, 字段数)。渲染与导航共用，顺序即字段顺序。
pub(crate) const SECTIONS: &[(&str, usize, usize)] = &[
    ("网络", 0, 3),
    ("端口", 3, 3),
    ("日志", 6, 1),
    ("TUN", 7, 5),
    ("DNS", 12, 8),
    ("其他", 20, 2),
];

/// 字段总数（= SECTIONS 覆盖的 0..FIELD_COUNT）。
pub(crate) const FIELD_COUNT: usize = 22;

/// 校验错误：label 定位表单字段，message 说明原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ValidationError {
    pub label: String,
    pub message: String,
}

impl std::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "「{}」{}", self.label, self.message)
    }
}

/// CSV 字符串 → 数组（按逗号分割、trim、去空项）。
pub(crate) fn split_csv(s: &str) -> Vec<String> {
    s.split(',')
        .map(|part| part.trim().to_string())
        .filter(|part| !part.is_empty())
        .collect()
}

/// 模型 → 表单字段（22 个，顺序与 SECTIONS 一致）。
pub(crate) fn field_values(s: &NetworkSettings) -> Vec<FormField> {
    let yn = |b: bool| if b { "是".to_string() } else { "否".to_string() };
    let csv = |v: &[String]| v.join(",");
    vec![
        FormField { label: "mode".into(), value: s.mode.clone(), kind: FieldKind::Dropdown(vec!["rule".into(), "global".into(), "direct".into()]) },
        FormField { label: "ipv6".into(), value: yn(s.ipv6), kind: FieldKind::Dropdown(vec!["是".into(), "否".into()]) },
        FormField { label: "allow-lan".into(), value: yn(s.allow_lan), kind: FieldKind::Dropdown(vec!["是".into(), "否".into()]) },
        FormField { label: "port".into(), value: s.port.to_string(), kind: FieldKind::Number },
        FormField { label: "socks-port".into(), value: s.socks_port.to_string(), kind: FieldKind::Number },
        FormField { label: "mixed-port".into(), value: s.mixed_port.to_string(), kind: FieldKind::Number },
        FormField { label: "log-level".into(), value: s.log_level.clone(), kind: FieldKind::Dropdown(vec!["silent".into(), "error".into(), "warning".into(), "info".into(), "debug".into()]) },
        FormField { label: "tun.enable".into(), value: yn(s.tun.enable), kind: FieldKind::Dropdown(vec!["是".into(), "否".into()]) },
        FormField { label: "tun.stack".into(), value: s.tun.stack.clone(), kind: FieldKind::Dropdown(vec!["system".into(), "gvisor".into(), "mixed".into()]) },
        FormField { label: "tun.auto-route".into(), value: yn(s.tun.auto_route), kind: FieldKind::Dropdown(vec!["是".into(), "否".into()]) },
        FormField { label: "tun.mtu".into(), value: s.tun.mtu.to_string(), kind: FieldKind::Number },
        FormField { label: "tun.dns-hijack".into(), value: csv(&s.tun.dns_hijack), kind: FieldKind::Text },
        FormField { label: "dns.enable".into(), value: yn(s.dns.enable), kind: FieldKind::Dropdown(vec!["是".into(), "否".into()]) },
        FormField { label: "dns.listen".into(), value: s.dns.listen.clone(), kind: FieldKind::Text },
        FormField { label: "dns.enhanced-mode".into(), value: s.dns.enhanced_mode.clone(), kind: FieldKind::Dropdown(vec!["fake-ip".into(), "redir-host".into()]) },
        FormField { label: "dns.fake-ip-range".into(), value: s.dns.fake_ip_range.clone(), kind: FieldKind::Text },
        FormField { label: "dns.nameserver".into(), value: csv(&s.dns.nameserver), kind: FieldKind::Text },
        FormField { label: "dns.default-nameserver".into(), value: csv(&s.dns.default_nameserver), kind: FieldKind::Text },
        FormField { label: "dns.fallback".into(), value: csv(&s.dns.fallback), kind: FieldKind::Text },
        FormField { label: "dns.fake-ip-filter".into(), value: csv(&s.dns.fake_ip_filter), kind: FieldKind::Text },
        FormField { label: "external-controller".into(), value: s.external_controller.clone(), kind: FieldKind::Text },
        FormField { label: "secret".into(), value: s.secret.clone(), kind: FieldKind::ReadOnly },
    ]
}

fn err<T>(label: &str, message: &str) -> Result<T, ValidationError> {
    Err(ValidationError { label: label.into(), message: message.into() })
}

fn nonempty(label: &str, v: &str) -> Result<String, ValidationError> {
    if v.trim().is_empty() {
        err(label, "不能为空")
    } else {
        Ok(v.trim().to_string())
    }
}

fn parse_u16(label: &str, v: &str) -> Result<u16, ValidationError> {
    let t = v.trim();
    if t.is_empty() {
        return err(label, "不能为空");
    }
    t.parse().map_err(|_| ValidationError { label: label.into(), message: format!("数值无效: {v}") })
}

fn parse_csv(label: &str, v: &str) -> Result<Vec<String>, ValidationError> {
    let items = split_csv(v);
    if items.is_empty() {
        return err(label, "至少需要一项（逗号分隔）");
    }
    Ok(items)
}

fn parse_yn(label: &str, v: &str) -> Result<bool, ValidationError> {
    match v {
        "是" => Ok(true),
        "否" => Ok(false),
        _ => err(label, "选项无效"),
    }
}

fn parse_dropdown(label: &str, v: &str, options: &[&str]) -> Result<String, ValidationError> {
    if options.contains(&v) {
        Ok(v.to_string())
    } else {
        err(label, &format!("选项无效: {v}"))
    }
}

/// 表单值 → 模型（含校验）。失败返回带字段定位的错误。
/// 校验规则：端口/MTU 为 0-65535 数字且非空；CSV 字段至少一项；
/// listen/fake-ip-range/external-controller/secret 非空；枚举字段须在选项内。
pub(crate) fn apply_values(f: &[FormField]) -> Result<NetworkSettings, ValidationError> {
    debug_assert_eq!(f.len(), FIELD_COUNT, "字段数量必须与 SECTIONS 一致");
    Ok(NetworkSettings {
        mode: parse_dropdown("mode", &f[0].value, &["rule", "global", "direct"])?,
        ipv6: parse_yn("ipv6", &f[1].value)?,
        allow_lan: parse_yn("allow-lan", &f[2].value)?,
        port: parse_u16("port", &f[3].value)?,
        socks_port: parse_u16("socks-port", &f[4].value)?,
        mixed_port: parse_u16("mixed-port", &f[5].value)?,
        log_level: parse_dropdown("log-level", &f[6].value, &["silent", "error", "warning", "info", "debug"])?,
        tun: TunSettings {
            enable: parse_yn("tun.enable", &f[7].value)?,
            stack: parse_dropdown("tun.stack", &f[8].value, &["system", "gvisor", "mixed"])?,
            auto_route: parse_yn("tun.auto-route", &f[9].value)?,
            mtu: parse_u16("tun.mtu", &f[10].value)?,
            dns_hijack: parse_csv("tun.dns-hijack", &f[11].value)?,
        },
        dns: DnsSettings {
            enable: parse_yn("dns.enable", &f[12].value)?,
            listen: nonempty("dns.listen", &f[13].value)?,
            enhanced_mode: parse_dropdown("dns.enhanced-mode", &f[14].value, &["fake-ip", "redir-host"])?,
            fake_ip_range: nonempty("dns.fake-ip-range", &f[15].value)?,
            nameserver: parse_csv("dns.nameserver", &f[16].value)?,
            default_nameserver: parse_csv("dns.default-nameserver", &f[17].value)?,
            fallback: parse_csv("dns.fallback", &f[18].value)?,
            fake_ip_filter: parse_csv("dns.fake-ip-filter", &f[19].value)?,
        },
        external_controller: nonempty("external-controller", &f[20].value)?,
        secret: nonempty("secret", &f[21].value)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 固定设置（不用 Default——secret 每次 Default 重新生成，无法比较）。
    /// 全字段显式给出（避免 clippy field_reassign_with_default/needless_update）。
    fn fixed_settings() -> NetworkSettings {
        NetworkSettings {
            secret: "a".repeat(32),
            mode: "global".into(),
            ipv6: true,
            allow_lan: true,
            port: 1080,
            socks_port: 1081,
            mixed_port: 1082,
            log_level: "debug".into(),
            external_controller: "0.0.0.0:9090".into(),
            tun: TunSettings {
                enable: true,
                stack: "gvisor".into(),
                auto_route: false,
                mtu: 1500,
                dns_hijack: vec!["any:53".into(), "any:5353".into()],
            },
            dns: DnsSettings {
                enable: false,
                listen: "0.0.0.0:1053".into(),
                enhanced_mode: "redir-host".into(),
                fake_ip_range: "198.18.0.1/16".into(),
                nameserver: vec!["https://doh.pub/dns-query".into()],
                default_nameserver: vec!["223.5.5.5".into()],
                fallback: vec!["tls://dns.alidns.com".into()],
                fake_ip_filter: vec!["*.lan".into()],
            },
        }
    }

    #[test]
    fn sections_cover_all_fields_without_gap() {
        let mut expect = 0;
        for (_, start, len) in SECTIONS {
            assert_eq!(*start, expect, "区块起始必须连续");
            expect += len;
        }
        assert_eq!(expect, FIELD_COUNT);
    }

    /// 22 字段往返：field_values → apply_values 全等。
    #[test]
    fn field_values_apply_values_roundtrip() {
        let s = fixed_settings();
        let fields = field_values(&s);
        assert_eq!(fields.len(), FIELD_COUNT);
        let back = apply_values(&fields).expect("往返不应校验失败");
        assert_eq!(back.mode, "global");
        assert!(back.ipv6);
        assert!(back.allow_lan);
        assert_eq!(back.port, 1080);
        assert_eq!(back.socks_port, 1081);
        assert_eq!(back.mixed_port, 1082);
        assert_eq!(back.log_level, "debug");
        assert_eq!(back.external_controller, "0.0.0.0:9090");
        assert_eq!(back.secret, "a".repeat(32));
        assert!(back.tun.enable);
        assert_eq!(back.tun.stack, "gvisor");
        assert!(!back.tun.auto_route);
        assert_eq!(back.tun.mtu, 1500);
        assert_eq!(back.tun.dns_hijack, vec!["any:53", "any:5353"]);
        assert!(!back.dns.enable);
        assert_eq!(back.dns.listen, "0.0.0.0:1053");
        assert_eq!(back.dns.enhanced_mode, "redir-host");
        assert_eq!(back.dns.fake_ip_range, "198.18.0.1/16");
        assert_eq!(back.dns.nameserver, vec!["https://doh.pub/dns-query"]);
        assert_eq!(back.dns.default_nameserver, vec!["223.5.5.5"]);
        assert_eq!(back.dns.fallback, vec!["tls://dns.alidns.com"]);
        assert_eq!(back.dns.fake_ip_filter, vec!["*.lan"]);
    }

    /// 默认值往返（用固定 secret 避免 Default 随机）。
    #[test]
    fn default_settings_roundtrip() {
        let s = NetworkSettings { secret: "b".repeat(32), ..NetworkSettings::default() };
        let back = apply_values(&field_values(&s)).expect("默认值应通过校验");
        assert_eq!(back.secret, "b".repeat(32));
        assert_eq!(back.port, 7890);
        assert_eq!(back.mode, "rule");
        assert_eq!(back.tun.stack, "mixed");
        assert_eq!(back.dns.enhanced_mode, "fake-ip");
    }

    /// 校验错误：非法端口/空 CSV/空文本，错误信息含字段 label。
    #[test]
    fn validation_rejects_invalid_input() {
        let mut fields = field_values(&fixed_settings());
        // 空端口
        fields[3].value = "".into();
        let e = apply_values(&fields).unwrap_err();
        assert_eq!(e.label, "port");
        assert!(e.to_string().contains("port"));
        // 越界
        fields[3].value = "65536".into();
        assert_eq!(apply_values(&fields).unwrap_err().label, "port");
        // 非数字
        fields[3].value = "abc".into();
        assert_eq!(apply_values(&fields).unwrap_err().label, "port");
        // 空 CSV（先恢复合法端口，否则 port 先报错）
        fields[3].value = "1080".into();
        fields[16].value = " , , ".into();
        let e = apply_values(&fields).unwrap_err();
        assert_eq!(e.label, "dns.nameserver");
        // 空文本
        fields[16].value = "1.1.1.1".into();
        fields[13].value = "".into();
        assert_eq!(apply_values(&fields).unwrap_err().label, "dns.listen");
        // 非法枚举（绕过 UI 直接改值）
        fields[13].value = "0.0.0.0:1053".into();
        fields[0].value = "hack".into();
        assert_eq!(apply_values(&fields).unwrap_err().label, "mode");
    }

    /// secret 字段：ReadOnly + 值透传。
    #[test]
    fn secret_field_is_readonly() {
        let s = fixed_settings();
        let fields = field_values(&s);
        assert_eq!(fields[21].label, "secret");
        assert_eq!(fields[21].value, "a".repeat(32));
        assert_eq!(fields[21].kind, FieldKind::ReadOnly);
    }

    /// split_csv：分割、trim、去空项。
    #[test]
    fn split_csv_trims_and_drops_empty() {
        assert_eq!(split_csv(" a, b ,,c "), vec!["a", "b", "c"]);
        assert_eq!(split_csv(""), Vec::<String>::new());
        assert_eq!(split_csv(" , "), Vec::<String>::new());
    }
}
