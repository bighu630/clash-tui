# 设置页（Settings Tab）实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 新增第 6 个"设置"tab，集中编辑 config.yaml 可配置的 22 个网络参数（TUN/DNS/网络/端口/日志/其他），支持 Ctrl+S 仅保存与 Ctrl+A 保存并应用。

**Architecture:** 复用现有完整链路（`NetworkSettings` 模型 + `merger.rs` + `mihomo -t` + `mihomo-apply`），只在 UI 层新增整页表单 `SettingsPage`（区块化 + 滚动 + 编辑模式）。纯函数 `field_values`/`apply_values` 负责模型↔表单转换与校验（可单测）。`Page` trait 新增 `on_enter` 回调，进入设置页时从 `st.settings` 同步字段（dirty 时保留编辑），保证与仪表盘热切开关的双向一致。移除仪表盘 `s` 弹窗，`s` 键改为全局跳转设置页。

**Tech Stack:** Rust + ratatui + crossterm，现有测试框架（`#[cfg(test)]` 内联测试，`with_settings_dir` 串行锁辅助）。

**Spec:** `docs/superpowers/specs/2026-08-12-settings-tab-design.md`

---

## 文件结构

- Modify: `src/ui/widgets.rs` — `FieldKind` 加 `ReadOnly` 变体（secret 只读字段）
- Create: `src/ui/settings.rs` — 纯函数 `field_values`/`apply_values`/`split_csv`/`SECTIONS` + `SettingsPage` 整页表单
- Modify: `src/ui/mod.rs` — `Page` trait 加 `on_enter` 默认方法
- Modify: `src/app.rs` — TABS 6 个、pages 挂载、数字键 1-6、`s` 全局跳转、`switch_page` 转发 `on_enter`、page_hints、HELP_LINES、测试
- Modify: `src/ui/dashboard.rs` — 移除 `s` 弹窗（`settings_form`/`apply_form`/`split_csv`/`DashPopup::Form`），保留 toggle 双写
- Modify: `README.md` — 功能总览、按键表

任务依赖：Task 2 依赖 Task 1（ReadOnly）；Task 3 依赖 Task 1+2；Task 4 依赖 Task 3；Task 5 独立可并行；Task 6 依赖全部。

---

### Task 1: widgets.rs — FieldKind 加 ReadOnly 变体

**Files:**
- Modify: `src/ui/widgets.rs`（`FieldKind` enum 约 20-29 行、`FormPopup::handle_key` 约 58-130 行）

- [ ] **Step 1: 写失败测试**（`src/ui/widgets.rs` 末尾测试模块）

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// ReadOnly 字段：FormPopup 中按键不修改值、不移动光标。
    #[test]
    fn readonly_field_ignores_keys() {
        let mut form = FormPopup::new(
            "测试".into(),
            vec![
                FormField { label: "secret".into(), value: "abc".into(), kind: FieldKind::ReadOnly },
            ],
        );
        // 各种编辑键均不应改变值
        for key in [
            KeyCode::Char('x'),
            KeyCode::Backspace,
            KeyCode::Delete,
            KeyCode::Left,
            KeyCode::Right,
        ] {
            form.handle_key(KeyEvent::new(key, KeyModifiers::NONE));
            assert_eq!(form.values(), vec!["abc".to_string()]);
        }
    }
}
```

（若 widgets.rs 已有 `#[cfg(test)] mod tests`，合并进现有模块；`KeyModifiers` 需 `use crossterm::event::KeyModifiers;`）

- [ ] **Step 2: 运行确认失败**

Run: `cargo test -p mihomo-tui readonly_field_ignores_keys`（包名以 Cargo.toml 为准，或用 `cargo test readonly_field_ignores_keys`）
Expected: 编译失败（`FieldKind::ReadOnly` 未定义）

- [ ] **Step 3: 实现**

`FieldKind` enum 加变体：

```rust
pub enum FieldKind {
    /// 自由文本（支持字符输入/退格/Delete/←→/Home/End）
    Text,
    /// 下拉选项，←/→ 循环切换
    Dropdown(Vec<String>),
    /// 数字（仅允许 0-9 输入）
    Number,
    /// 只读展示（如 secret）：不响应任何编辑按键
    ReadOnly,
}
```

`FormPopup::handle_key` 中 `let is_dropdown = ...` 行后加：

```rust
let is_readonly = matches!(self.fields[self.focused].kind, FieldKind::ReadOnly);
```

`handle_key` 中以下分支加 `!is_readonly` 守卫（原 `if is_dropdown { ... } else { ... }` 结构保持）：
- `KeyCode::Left`：`if is_dropdown { self.cycle_dropdown(-1); } else if !is_readonly { self.move_cursor(-1); }`
- `KeyCode::Right`：同上改 `1`
- `KeyCode::Home` / `KeyCode::End`：`if !is_dropdown && !is_readonly { ... }`
- `KeyCode::Backspace` / `KeyCode::Delete`：`if !is_readonly { ... }`
- `KeyCode::Char(c)` 的 match：加 `FieldKind::ReadOnly => {}` 分支（与 `Dropdown` 并列）

`FormPopup::render` 中 `match &self.fields[idx].kind` 已有 `_ =>` 兜底分支，无需修改（ReadOnly 按普通文本渲染）。

- [ ] **Step 4: 运行确认通过**

Run: `cargo test readonly_field_ignores_keys`
Expected: PASS

- [ ] **Step 5: 全量回归 + 提交**

Run: `cargo test`（现有测试应全绿，无其他 match FieldKind 穷尽点遗漏）
Run: `cargo clippy --all-targets 2>&1 | grep -c warning` → `0`
Commit:
```bash
git add src/ui/widgets.rs
git commit -m "feat: FieldKind 加 ReadOnly 变体（secret 只读字段用）"
```

---

### Task 2: ui/settings.rs — 纯函数（field_values/apply_values/SECTIONS）

**Files:**
- Create: `src/ui/settings.rs`（先只写纯函数部分，`SettingsPage` 在 Task 3 追加）

- [ ] **Step 1: 写失败测试**

`src/ui/settings.rs` 中先写：

```rust
//! 设置页：config.yaml 可配置项集中编辑（TUN/DNS/网络/端口/日志/其他）。
//! 交互规格见 docs/superpowers/specs/2026-08-12-settings-tab-design.md。
//! 本文件先落纯函数（模型↔表单转换与校验），SettingsPage 页面在后续任务追加。

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
    fn fixed_settings() -> NetworkSettings {
        let mut s = NetworkSettings::default();
        s.secret = "a".repeat(32);
        s.mode = "global".into();
        s.ipv6 = true;
        s.allow_lan = true;
        s.port = 1080;
        s.socks_port = 1081;
        s.mixed_port = 1082;
        s.log_level = "debug".into();
        s.external_controller = "0.0.0.0:9090".into();
        s.tun.enable = true;
        s.tun.stack = "gvisor".into();
        s.tun.auto_route = false;
        s.tun.mtu = 1500;
        s.tun.dns_hijack = vec!["any:53".into(), "any:5353".into()];
        s.dns.enable = false;
        s.dns.listen = "0.0.0.0:1053".into();
        s.dns.enhanced_mode = "redir-host".into();
        s.dns.fake_ip_range = "198.18.0.1/16".into();
        s.dns.nameserver = vec!["https://doh.pub/dns-query".into()];
        s.dns.default_nameserver = vec!["223.5.5.5".into()];
        s.dns.fallback = vec!["tls://dns.alidns.com".into()];
        s.dns.fake_ip_filter = vec!["*.lan".into()];
        s
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
        assert_eq!(back.ipv6, true);
        assert_eq!(back.allow_lan, true);
        assert_eq!(back.port, 1080);
        assert_eq!(back.socks_port, 1081);
        assert_eq!(back.mixed_port, 1082);
        assert_eq!(back.log_level, "debug");
        assert_eq!(back.external_controller, "0.0.0.0:9090");
        assert_eq!(back.secret, "a".repeat(32));
        assert_eq!(back.tun.enable, true);
        assert_eq!(back.tun.stack, "gvisor");
        assert_eq!(back.tun.auto_route, false);
        assert_eq!(back.tun.mtu, 1500);
        assert_eq!(back.tun.dns_hijack, vec!["any:53", "any:5353"]);
        assert_eq!(back.dns.enable, false);
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
        let mut s = NetworkSettings::default();
        s.secret = "b".repeat(32);
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
        // 空 CSV
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
```

- [ ] **Step 2: 运行确认失败**

Run: `cargo test field_values_apply_values_roundtrip`
Expected: 编译失败（`field_values` 未定义）或 0 tests 运行

- [ ] **Step 3: 实现**（将上面测试与实现代码全部写入 `src/ui/settings.rs`，注意 `field_values`/`apply_values`/`split_csv`/`SECTIONS`/`FIELD_COUNT`/`ValidationError` 用 `pub(crate)`，供 Task 3 的 SettingsPage 与 app.rs 使用）

注意：`src/ui/mod.rs` 需要加 `pub mod settings;`

- [ ] **Step 4: 运行确认通过**

Run: `cargo test field_ -- --nocapture`（settings 模块全部测试）
Expected: 全部 PASS（sections_cover_all_fields_without_gap / roundtrip / validation / secret / split_csv）

- [ ] **Step 5: 回归 + 提交**

Run: `cargo test`、`cargo clippy --all-targets 2>&1 | grep -c warning` → `0`
Commit:
```bash
git add src/ui/settings.rs src/ui/mod.rs
git commit -m "feat: 设置页纯函数（field_values/apply_values/SECTIONS 校验）"
```

---

### Task 3: ui/settings.rs — SettingsPage 整页表单

**Files:**
- Modify: `src/ui/settings.rs`（追加 SettingsPage 实现与测试）

- [ ] **Step 1: 写失败测试**

在 `src/ui/settings.rs` 测试模块追加（用 `crate::app::AppState`、`crate::core::settings::{load_settings, save_settings, with_settings_dir}`、`crate::ui::Page`）：

```rust
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use crate::app::{AppState, UiCommand};
    use crate::core::models::RuntimeConfig;
    use crate::core::settings::{load_settings, with_settings_dir};
    use crate::ui::Page;
    use std::collections::{HashMap, VecDeque};

    /// 构造最小 AppState（参照 dashboard 测试 test_state）。
    fn test_state() -> AppState {
        AppState {
            settings: crate::core::models::NetworkSettings::default(),
            subs: Vec::new(),
            overrides: crate::core::models::Overrides::default(),
            runtime: RuntimeConfig::default(),
            api_ok: false,
            api_confirmed: false,
            traffic: VecDeque::new(),
            mem_history: VecDeque::new(),
            connections: Vec::new(),
            exit_ip: None,
            proxy_groups: Vec::new(),
            group_delays: HashMap::new(),
            logs: VecDeque::new(),
            notices: VecDeque::new(),
        }
    }

    fn ctrl(key: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(key), KeyModifiers::CONTROL)
    }

    fn press(page: &mut SettingsPage, st: &mut AppState, key: KeyEvent) -> Option<UiCommand> {
        page.handle_key(key, st)
    }

    fn page_with_state(st: &AppState) -> SettingsPage {
        let mut p = SettingsPage::new();
        p.on_enter(st);
        p
    }

    /// 进入页面：字段从 st.settings 同步。
    #[test]
    fn on_enter_syncs_fields_from_settings() {
        let mut st = test_state();
        st.settings.port = 8888;
        let p = page_with_state(&st);
        assert_eq!(p.fields[3].value, "8888");
        assert!(!p.dirty(), "同步后不应是未保存状态");
    }

    /// dirty 时 on_enter 不覆盖（未保存编辑保留）。
    #[test]
    fn on_enter_keeps_dirty_edits() {
        let mut st = test_state();
        let mut p = page_with_state(&st);
        // 编辑 port 字段：选中 → Enter 进编辑 → 输入 9
        p.focused = 3;
        press(&mut p, &mut st, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        press(&mut p, &mut st, KeyEvent::new(KeyCode::Char('9'), KeyModifiers::NONE));
        assert!(p.dirty());
        // st.settings 被外部改动（如仪表盘热切）后 on_enter 不应覆盖编辑
        st.settings.port = 7777;
        p.on_enter(&st);
        assert!(p.fields[3].value.contains('9'), "dirty 时不应重新同步");
    }

    /// Ctrl+S 仅保存：落盘 + st.settings 同步 + 无命令 + 清除 dirty。
    #[test]
    fn ctrl_s_saves_without_applying() {
        with_settings_dir(|| {
            let mut st = test_state();
            save_settings(&st.settings).unwrap();
            let mut p = page_with_state(&st);
            p.focused = 3;
            press(&mut p, &mut st, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
            press(&mut p, &mut st, KeyEvent::new(KeyCode::Char('9'), KeyModifiers::NONE));
            press(&mut p, &mut st, KeyEvent::new(KeyCode::Char('0'), KeyModifiers::NONE));
            press(&mut p, &mut st, KeyEvent::new(KeyCode::Char('8'), KeyModifiers::NONE));
            let cmd = press(&mut p, &mut st, ctrl('s'));
            assert!(cmd.is_none(), "仅保存不应返回命令");
            let back = load_settings().unwrap();
            assert_eq!(back.port, 908, "磁盘应落盘新端口");
            assert_eq!(st.settings.port, 908, "st.settings 应同步");
            assert!(!p.dirty(), "保存后清除未保存标记");
            assert!(st.notices.iter().any(|(_, t)| t.contains("已保存")));
        });
    }

    /// Ctrl+A 保存并应用：落盘 + 返回 ApplyConfig（含合并后 YAML）。
    #[test]
    fn ctrl_a_saves_and_returns_apply_config() {
        with_settings_dir(|| {
            let mut st = test_state();
            save_settings(&st.settings).unwrap();
            let mut p = page_with_state(&st);
            p.focused = 3;
            press(&mut p, &mut st, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
            press(&mut p, &mut st, KeyEvent::new(KeyCode::Char('9'), KeyModifiers::NONE));
            press(&mut p, &mut st, KeyEvent::new(KeyCode::Char('0'), KeyModifiers::NONE));
            press(&mut p, &mut st, KeyEvent::new(KeyCode::Char('8'), KeyModifiers::NONE));
            let cmd = press(&mut p, &mut st, ctrl('a'));
            match cmd {
                Some(UiCommand::ApplyConfig(yaml)) => {
                    assert!(yaml.contains("port: 908"), "合并输出应含新端口: {yaml}");
                }
                other => panic!("应返回 ApplyConfig: {other:?}"),
            }
            let back = load_settings().unwrap();
            assert_eq!(back.port, 908);
            assert_eq!(st.settings.port, 908);
        });
    }

    /// 校验失败：弹窗 + 不落盘 + 内容保留 + 焦点指向出错字段。
    #[test]
    fn validation_error_keeps_edits_and_focuses_field() {
        with_settings_dir(|| {
            let mut st = test_state();
            save_settings(&st.settings).unwrap();
            let mut p = page_with_state(&st);
            // 清空 port
            p.focused = 3;
            press(&mut p, &mut st, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
            for _ in 0..4 {
                press(&mut p, &mut st, KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
            }
            let cmd = press(&mut p, &mut st, ctrl('s'));
            assert!(cmd.is_none());
            assert!(p.popup.is_some(), "应有错误弹窗");
            assert_eq!(p.focused, 3, "焦点应留在出错字段");
            assert!(p.fields[3].value.is_empty(), "已填内容应保留");
            let back = load_settings().unwrap();
            assert_eq!(back.port, 7890, "失败时不应落盘");
            // 关闭弹窗后仍可继续编辑（内容未丢）
            press(&mut p, &mut st, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
            assert!(p.popup.is_none());
        });
    }

    /// secret 重新生成：Enter 触发，32 hex，可保存落盘。
    #[test]
    fn secret_regen_on_enter() {
        with_settings_dir(|| {
            let mut st = test_state();
            save_settings(&st.settings).unwrap();
            let old = st.settings.secret.clone();
            let mut p = page_with_state(&st);
            p.focused = FIELD_COUNT - 1;
            press(&mut p, &mut st, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
            let new_secret = p.fields[FIELD_COUNT - 1].value.clone();
            assert_eq!(new_secret.len(), 32);
            assert!(new_secret.chars().all(|c| c.is_ascii_hexdigit()));
            assert_ne!(new_secret, old, "应生成新密钥");
            assert!(p.dirty(), "重新生成后应标记未保存");
            press(&mut p, &mut st, ctrl('s'));
            let back = load_settings().unwrap();
            assert_eq!(back.secret, new_secret, "Ctrl+S 后应落盘新密钥");
        });
    }

    /// 下拉循环：←/→ 切换，Tab 移动跳过标题。
    #[test]
    fn dropdown_cycle_and_navigation() {
        let mut st = test_state();
        let mut p = page_with_state(&st);
        assert_eq!(p.focused, 0);
        // mode → global → direct
        press(&mut p, &mut st, KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert_eq!(p.fields[0].value, "global");
        press(&mut p, &mut st, KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert_eq!(p.fields[0].value, "direct");
        // Tab 走到下一个字段
        press(&mut p, &mut st, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(p.focused, 1);
    }

    /// 文本字段编辑模式：Enter 进入、输入、Esc 退出。
    #[test]
    fn text_field_edit_mode() {
        let mut st = test_state();
        let mut p = page_with_state(&st);
        // dns.listen（index 13）追加端口
        p.focused = 13;
        press(&mut p, &mut st, KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
        press(&mut p, &mut st, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        press(&mut p, &mut st, KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE));
        press(&mut p, &mut st, KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE));
        press(&mut p, &mut st, KeyEvent::new(KeyCode::Char('5'), KeyModifiers::NONE));
        press(&mut p, &mut st, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(p.fields[13].value, "0.0.0.0:1053:15");
        assert!(!p.editing, "Esc 退出编辑模式");
    }
```

- [ ] **Step 2: 运行确认失败**

Run: `cargo test on_enter_syncs_fields_from_settings`
Expected: 编译失败（SettingsPage 未定义）

- [ ] **Step 3: 实现**（追加到 `src/ui/settings.rs`）

```rust
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::app::{AppState, UiCommand};
use crate::core::merger::{merge, MergeContext};
use crate::core::settings::{generate_secret, save_settings};
use crate::ui::widgets::{FieldKind, FormField, MessagePopup};
use crate::ui::Page;

/// 渲染行：区块标题或字段。
enum RenderRow<'a> {
    Section(&'a str),
    Field(usize),
}

/// 设置页：整页表单（区块 + 滚动 + 编辑模式）。
/// 数据流：on_enter 从 st.settings 同步字段（dirty 时保留）；
/// Ctrl+S 仅保存 settings.toml；Ctrl+A 保存并 ApplyConfig（合并 → 校验 → 提权 → 重启）。
pub struct SettingsPage {
    fields: Vec<FormField>,
    /// 可选中字段索引（0..FIELD_COUNT）
    focused: usize,
    /// 文本/数字字段编辑模式（Enter 进入，Esc/Enter 退出）
    editing: bool,
    /// 每字段编辑光标（字节位置，恒在字符边界）
    cursor: Vec<usize>,
    /// 最近保存/同步的值快照：与当前值不等 → 未保存
    saved: Vec<String>,
    /// 渲染滚动偏移（行索引，含区块标题行）
    offset: usize,
    /// 校验/保存失败弹窗
    popup: Option<MessagePopup>,
}

/// 区块标题所在行索引（渲染模型：标题行 = start + 前面标题数）。
fn section_title_rows() -> Vec<usize> {
    let mut rows = Vec::new();
    for (i, (_, start, _)) in SECTIONS.iter().enumerate() {
        rows.push(start + i);
    }
    rows
}

/// 全部渲染行（含标题），供滚动与绘制。
fn render_rows() -> Vec<RenderRow<'static>> {
    let mut rows = Vec::new();
    for (name, start, len) in SECTIONS {
        rows.push(RenderRow::Section(name));
        for j in *start..*start + *len {
            rows.push(RenderRow::Field(j));
        }
    }
    rows
}

impl SettingsPage {
    pub fn new() -> Self {
        Self {
            fields: Vec::new(),
            focused: 0,
            editing: false,
            cursor: Vec::new(),
            saved: Vec::new(),
            offset: 0,
            popup: None,
        }
    }

    /// 当前值快照（dirty 判断与保存基准）。
    fn values(&self) -> Vec<String> {
        self.fields.iter().map(|f| f.value.clone()).collect()
    }

    /// 是否有未保存修改。
    fn dirty(&self) -> bool {
        !self.saved.is_empty() && self.values() != self.saved
    }

    /// 从 st.settings 重新同步字段。有未保存编辑时保留（不覆盖）。
    fn sync_from_settings(&mut self, st: &AppState) {
        if self.dirty() {
            return;
        }
        self.fields = field_values(&st.settings);
        self.cursor = self.fields.iter().map(|f| f.value.len()).collect();
        self.saved = self.values();
        self.focused = 0;
        self.editing = false;
        self.offset = 0;
    }

    /// 保存（可选应用）。校验失败/落盘失败/合并失败 → 弹窗，内容保留。
    fn save(&mut self, st: &mut AppState, apply: bool) -> Option<UiCommand> {
        match apply_values(&self.fields) {
            Err(e) => {
                if let Some(i) = self.fields.iter().position(|f| f.label == e.label) {
                    self.focused = i;
                }
                self.popup = Some(MessagePopup::new(
                    "校验失败".into(),
                    vec![e.to_string()],
                ));
                None
            }
            Ok(s) => {
                if let Err(e) = save_settings(&s) {
                    self.popup = Some(MessagePopup::new(
                        "保存失败".into(),
                        vec![format!("写入 settings.toml 失败: {e}")],
                    ));
                    return None;
                }
                // 磁盘已落盘：立即同步 st.settings（merger 读取它）与保存快照
                st.settings = s.clone();
                self.saved = self.values();
                st.notice("[✓] 已保存".to_string());
                if !apply {
                    return None;
                }
                let active = st.subs.iter().find(|sub| sub.active);
                match merge(MergeContext {
                    settings: &s,
                    overrides: &st.overrides,
                    subscription: active,
                }) {
                    Err(e) => {
                        self.popup = Some(MessagePopup::new(
                            "合并失败".into(),
                            vec![format!("配置合并失败: {e}")],
                        ));
                        None
                    }
                    Ok(out) => {
                        if !out.warnings.is_empty() {
                            st.notice(format!("[!] 合并警告: {}", out.warnings.join("；")));
                        }
                        Some(UiCommand::ApplyConfig(out.config))
                    }
                }
            }
        }
    }

    /// 聚焦移动（focused 只在 0..FIELD_COUNT 内循环，标题行不参与）。
    fn focus_move(&mut self, dir: i32) {
        let n = FIELD_COUNT as i32;
        self.focused = (self.focused as i32 + dir).rem_euclid(n) as usize;
    }

    fn insert_char(&mut self, c: char) {
        let f = self.focused;
        let cur = self.cursor[f];
        self.fields[f].value.insert(cur, c);
        self.cursor[f] = cur + c.len_utf8();
    }

    fn backspace(&mut self) {
        let f = self.focused;
        let cur = self.cursor[f];
        if cur == 0 {
            return;
        }
        let prev = self.fields[f].value[..cur]
            .char_indices()
            .next_back()
            .map(|(i, _)| i)
            .unwrap_or(0);
        self.fields[f].value.replace_range(prev..cur, "");
        self.cursor[f] = prev;
    }

    fn delete_at_cursor(&mut self) {
        let f = self.focused;
        let cur = self.cursor[f];
        if cur >= self.fields[f].value.len() {
            return;
        }
        let next = self.fields[f].value[cur..]
            .char_indices()
            .next()
            .map(|(i, ch)| cur + i + ch.len_utf8())
            .unwrap_or(cur);
        self.fields[f].value.replace_range(cur..next, "");
    }

    fn move_cursor(&mut self, dir: i32) {
        let f = self.focused;
        let value = &self.fields[f].value;
        let cur = self.cursor[f];
        if dir < 0 {
            if let Some((i, _)) = value[..cur].char_indices().next_back() {
                self.cursor[f] = i;
            }
        } else if let Some((i, ch)) = value[cur..].char_indices().next() {
            self.cursor[f] = cur + i + ch.len_utf8();
        }
    }

    fn cycle_dropdown(&mut self, dir: i32) {
        if let FieldKind::Dropdown(options) = &self.fields[self.focused].kind {
            if options.is_empty() {
                return;
            }
            let idx = options
                .iter()
                .position(|o| o == &self.fields[self.focused].value)
                .unwrap_or(0);
            let len = options.len() as i32;
            let next = (idx as i32 + dir).rem_euclid(len) as usize;
            self.fields[self.focused].value = options[next].clone();
        }
    }
}

impl Page for SettingsPage {
    fn popup_open(&self) -> bool {
        self.popup.is_some()
    }

    fn on_enter(&mut self, st: &AppState) {
        self.sync_from_settings(st);
    }

    fn handle_key(&mut self, key: KeyEvent, st: &mut AppState) -> Option<UiCommand> {
        // 错误弹窗优先（关闭后回到表单，内容保留）
        if let Some(mut popup) = self.popup.take() {
            if !popup.handle_key(key) {
                self.popup = Some(popup);
            }
            return None;
        }
        // Ctrl+S / Ctrl+A 优先（编辑模式下也响应）
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('s') => return self.save(st, false),
                KeyCode::Char('a') => return self.save(st, true),
                _ => {}
            }
        }
        if self.editing {
            match key.code {
                KeyCode::Esc | KeyCode::Enter => self.editing = false,
                KeyCode::Backspace => self.backspace(),
                KeyCode::Delete => self.delete_at_cursor(),
                KeyCode::Left => self.move_cursor(-1),
                KeyCode::Right => self.move_cursor(1),
                KeyCode::Home => self.cursor[self.focused] = 0,
                KeyCode::End => self.cursor[self.focused] = self.fields[self.focused].value.len(),
                KeyCode::Char(c) => self.insert_char(c),
                _ => {}
            }
            return None;
        }
        match key.code {
            KeyCode::Up | KeyCode::BackTab => self.focus_move(-1),
            KeyCode::Down | KeyCode::Tab => self.focus_move(1),
            KeyCode::Home => self.focused = 0,
            KeyCode::End => self.focused = FIELD_COUNT - 1,
            KeyCode::Left => self.cycle_dropdown(-1),
            KeyCode::Right => self.cycle_dropdown(1),
            KeyCode::Enter => match &self.fields[self.focused].kind {
                FieldKind::ReadOnly => {
                    // secret：重新生成（32 hex）
                    self.fields[self.focused].value = generate_secret();
                    self.cursor[self.focused] = self.fields[self.focused].value.len();
                }
                FieldKind::Dropdown(_) => {}
                _ => {
                    self.editing = true;
                    self.cursor[self.focused] = self.fields[self.focused].value.len();
                }
            },
            _ => {}
        }
        None
    }

    fn render(&mut self, f: &mut Frame, area: Rect, _st: &AppState) {
        let [body, status] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(area);
        // 状态行：未保存标记 + 按键提示
        let dirty = self.dirty();
        let status_text = format!(
            "{}{}",
            if dirty { "[未保存] " } else { "" },
            "↑↓/Tab 移动 · ←→ 下拉 · Enter 编辑(secret 重新生成) · Ctrl+S 保存 · Ctrl+A 保存并应用"
        );
        f.render_widget(
            Paragraph::new(Span::styled(
                status_text,
                Style::default().fg(if dirty { Color::Yellow } else { Color::DarkGray }),
            )),
            status,
        );

        let rows = render_rows();
        let vis = body.height as usize;
        // 滚动：保持聚焦行可见
        let focus_row = rows
            .iter()
            .position(|r| matches!(r, RenderRow::Field(j) if *j == self.focused))
            .unwrap_or(0);
        if self.offset > focus_row {
            self.offset = focus_row;
        }
        if vis > 0 && self.offset + vis <= focus_row {
            self.offset = focus_row + 1 - vis;
        }
        let end = (self.offset + vis).min(rows.len());

        let label_w: u16 = 24;
        let vx = body.x + label_w + 2;
        let vw = body.width.saturating_sub(label_w + 3).max(1);

        for (i, row) in rows[self.offset..end].iter().enumerate() {
            let y = body.y + i as u16;
            match row {
                RenderRow::Section(name) => {
                    f.render_widget(
                        Paragraph::new(Span::styled(
                            format!("── {name} ──"),
                            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                        )),
                        Rect::new(body.x, y, body.width, 1),
                    );
                }
                RenderRow::Field(idx) => {
                    let focused = *idx == self.focused;
                    let field = &self.fields[*idx];
                    let label = if field.label.len() as u16 > label_w {
                        field.label.chars().take(label_w as usize).collect::<String>()
                    } else {
                        field.label.clone()
                    };
                    let label_style = if focused {
                        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().add_modifier(Modifier::BOLD)
                    };
                    f.render_widget(
                        Paragraph::new(Line::from(vec![
                            Span::styled(label, label_style),
                            Span::raw(": "),
                        ])),
                        Rect::new(body.x, y, label_w + 2, 1),
                    );
                    let value_style = if focused {
                        Style::default().fg(Color::Black).bg(Color::Cyan)
                    } else {
                        Style::default()
                    };
                    match &field.kind {
                        FieldKind::Dropdown(_) => {
                            let text = format!("◀ {} ▶", field.value);
                            f.render_widget(
                                Paragraph::new(Span::styled(text, value_style)),
                                Rect::new(vx, y, vw, 1),
                            );
                        }
                        _ => {
                            let cur_chars = field.value[..self.cursor[*idx]].chars().count();
                            let start_c = cur_chars.saturating_sub(vw as usize - 1);
                            let shown: String = field
                                .value
                                .chars()
                                .skip(start_c)
                                .take(vw as usize)
                                .collect();
                            f.render_widget(
                                Paragraph::new(Span::styled(shown, value_style)),
                                Rect::new(vx, y, vw, 1),
                            );
                            if focused && self.editing {
                                let cur_x = vx + (cur_chars - start_c) as u16;
                                f.set_cursor_position(ratatui::layout::Position::new(cur_x, y));
                            }
                        }
                    }
                }
            }
        }

        if let Some(popup) = &mut self.popup {
            popup.render(f, area);
        }
    }
}
```

注意：
- `MessagePopup::handle_key` 返回 `bool`（true = 关闭弹窗）。上面 `if !popup.handle_key(key) { self.popup = Some(popup); }` 与 dashboard 现有用法一致——先确认 `src/ui/widgets.rs` 中 `MessagePopup::handle_key` 的真实语义再写（读该函数确认返回值语义后对齐）。
- `on_enter` 首次进入时 `saved` 为空 → `dirty()` 为 false（`!saved.is_empty()` 守卫）→ 正常同步。
- `render_rows()` 每次渲染重建（28 行，开销可忽略）。`RenderRow` 去掉生命周期参数（`enum RenderRow { Section(&'static str), Field(usize) }`）。
- `editing` 模式下 `KeyCode::Char(c)` 会吞掉所有字符输入（含数字）——Number 字段无需限制（与 FormPopup 不同：编辑模式下自由输入，校验兜底）。

- [ ] **Step 4: 运行确认通过**

Run: `cargo test settings`（模块全部测试）
Expected: PASS（含 Task 2 的纯函数测试）

- [ ] **Step 5: 回归 + 提交**

Run: `cargo test`、`cargo clippy --all-targets 2>&1 | grep -c warning` → `0`
Commit:
```bash
git add src/ui/settings.rs
git commit -m "feat: 设置页 SettingsPage 整页表单（区块/滚动/编辑模式/保存与保存并应用）"
```

---

### Task 4: app.rs + ui/mod.rs — tab 挂载与全局键

**Files:**
- Modify: `src/ui/mod.rs`（Page trait 加 on_enter）
- Modify: `src/app.rs`（TABS、pages、数字键、s 键、switch_page、page_hints、HELP_LINES、测试）

- [ ] **Step 1: 写失败测试**（`src/app.rs` 测试模块追加）

```rust
    /// 数字键 6 切到设置页（index 5）。
    #[test]
    fn tab_key_6_switches_to_settings_page() {
        let (mut app, _rx) = test_app(24);
        assert_eq!(app.pages.len(), 6, "应挂载 6 个页面");
        let _ = app.handle_key(KeyEvent::new(KeyCode::Char('6'), KeyModifiers::NONE));
        assert_eq!(app.current, 5);
    }

    /// s 全局跳转设置页；已在设置页时按 s 不切走（s 供字段输入）。
    #[test]
    fn s_key_switches_to_settings_page() {
        let (mut app, _rx) = test_app(24);
        assert_eq!(app.current, 0);
        let _ = app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE));
        assert_eq!(app.current, 5, "s 应全局跳转设置页");
        let _ = app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE));
        assert_eq!(app.current, 5, "设置页内 s 不应切走");
    }

    /// switch_page(5) 触发 on_enter：设置页字段从 st.settings 同步。
    /// 验证方式：先改 st.settings，切页后 Ctrl+S 落盘的应是新值。
    #[test]
    fn switch_page_syncs_settings_page_fields() {
        let (mut app, _rx) = test_app(24);
        app.state.settings.port = 9999;
        app.switch_page(5);
        // 页面字段应已同步为 9999：Ctrl+S 直接落盘
        let cmd = app.pages[5].handle_key(
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
            &mut app.state,
        );
        assert!(cmd.is_none(), "仅保存不应返回命令");
        let loaded = crate::core::settings::load_settings().unwrap();
        assert_eq!(loaded.port, 9999, "on_enter 同步后 Ctrl+S 应落盘新值");
    }
```

注意第三个测试会真实写 `~/.config/mihomo-tui/settings.toml`（test_app 不包 with_settings_dir）。**必须**用 `with_settings_dir` 包裹（`use crate::core::settings::with_settings_dir;`），否则污染真实配置。

- [ ] **Step 2: 运行确认失败**

Run: `cargo test tab_key_6_switches_to_settings_page`
Expected: FAIL（pages.len()==5）

- [ ] **Step 3: 实现**

`src/ui/mod.rs` — Page trait 加方法（默认实现）：

```rust
    /// 切页进入时的回调（默认无操作）。设置页用它从 st.settings 重新同步字段。
    fn on_enter(&mut self, _st: &AppState) {}
```

`src/app.rs`：
1. `const TABS: [&str; 5]` → `[&str; 6]`，末尾加 `"设置"`。
2. `run()` 中 pages 追加 `Box::new(crate::ui::settings::SettingsPage::new()),`。
3. `test_app_with_width` 中 pages 同样追加。
4. `switch_page` 加设置页同步（idx == 5）：

```rust
    /// 切页；进入规则组页（index 2）时刷新运行时策略组；
    /// 进入设置页（index 5）时同步字段（页面内部 dirty 时保留编辑）。
    fn switch_page(&mut self, idx: usize) {
        self.current = idx;
        if idx == 2 {
            let _ = self.cmd_tx.send(UiCommand::RefreshGroups);
        }
        if idx == 5 {
            let st = &self.state;
            self.pages[idx].on_enter(st);
        }
    }
```

5. 数字键范围 `('1'..='5')` → `('1'..='6')`。
6. 数字键分支后加 s 键全局跳转：

```rust
            // s：全局跳转设置页（设置页内不拦截——s 是文本字段输入字符）
            KeyCode::Char('s') if self.current != 5 => self.switch_page(5),
```

7. `page_hints` 加分支：

```rust
        5 => vec![
            ("Ctrl+S".into(), "保存".into()),
            ("Ctrl+A".into(), "应用".into()),
            ("Enter".into(), "编辑".into()),
            ("↑↓".into(), "移动".into()),
        ],
```

兜底分支注释 `0..=4` 改为 `0..=5`。

8. `HELP_LINES` 更新：
- `"  Tab / ← → / 1-5    切换页面"` → `"  Tab / ← → / 1-6    切换页面"`
- 仪表盘段 `"  s                  网络设置（保存后自动合并并应用）"` → `"  s                  跳转设置页"`
- 日志段后追加：

```rust
    "",
    "设置:",
    "  ↑↓ / Tab          切换字段",
    "  ←→                切换下拉选项",
    "  Enter             编辑字段（Esc 退出；secret 字段为重新生成密钥）",
    "  Ctrl+S            仅保存（写 settings.toml，不重启）",
    "  Ctrl+A            保存并应用（合并 → mihomo -t 校验 → 提权重启）",
```

9. 测试模块顶部补 `use crate::core::settings::with_settings_dir;`（若已有类似 import 则合并）。

- [ ] **Step 4: 运行确认通过**

Run: `cargo test tab_key_6 s_key_switch switch_page_syncs`
Expected: PASS

- [ ] **Step 5: 回归 + 提交**

Run: `cargo test`（全部）、`cargo clippy --all-targets 2>&1 | grep -c warning` → `0`
Commit:
```bash
git add src/app.rs src/ui/mod.rs
git commit -m "feat: 设置页 tab 挂载（TABS 6、数字键 1-6、s 全局跳转、on_enter 同步）"
```

---

### Task 5: dashboard.rs — 移除 s 弹窗（可与其他任务并行）

**Files:**
- Modify: `src/ui/dashboard.rs`

- [ ] **Step 1: 写失败测试**（dashboard 测试模块追加）

```rust
    /// s 键不再打开设置弹窗（功能迁移到设置页 tab）。
    #[test]
    fn s_key_no_longer_opens_popup() {
        let mut st = test_state();
        let mut page = DashboardPage::new();
        let cmd = page.handle_key(
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE),
            &mut st,
        );
        assert!(cmd.is_none(), "s 不应再产生命令");
        assert!(page.popup.is_none(), "s 不应再弹设置表单");
    }
```

- [ ] **Step 2: 运行确认失败**

Run: `cargo test s_key_no_longer_opens_popup`
Expected: FAIL（当前 s 打开弹窗）

- [ ] **Step 3: 实现**

删除以下内容：
- `apply_form` 方法（约 30-53 行）与 `merge`/`MergeContext` 相关代码
- `DashPopup::Form` 变体与 `handle_key` 中 `Some(DashPopup::Form(...))` 分支（约 115-126 行）——`DashPopup` 只剩 `Msg` 变体时可简化为 `popup: Option<MessagePopup>`（同时改 `popup_open`、`handle_key` 弹窗分支、`render` 弹窗部分、`toggle_double_write` 中 `self.popup = Some(DashPopup::Msg(...))` → `Some(MessagePopup::new(...))`、测试中 `Some(DashPopup::Msg(m))` → `Some(m)`）
- `KeyCode::Char('s')` 分支（约 169-172 行）
- `render` 中 `DashPopup::Form` 分支（约 198-200 行）
- `settings_form` 函数（约 217-234 行）
- `split_csv`、`yes_no` 函数
- imports 清理：`FormAction, FormField, FieldKind, FormPopup`（`MessagePopup` 保留）、`merge, MergeContext`（若 `apply_form` 删除后无其他使用）、`NetworkSettings`（检查 `test_state` 与 toggle 测试仍用，保留）、`save_settings`（toggle_double_write 仍用，保留）

保留：toggle 双写（m/t/6）、`DashPopup::Msg`（若保留 enum）或直接 `Option<MessagePopup>`。

- [ ] **Step 4: 运行确认通过**

Run: `cargo test dashboard`
Expected: PASS（toggle 测试 + 新 s 测试）

- [ ] **Step 5: 回归 + 提交**

Run: `cargo test`、`cargo clippy --all-targets 2>&1 | grep -c warning` → `0`
Commit:
```bash
git add src/ui/dashboard.rs
git commit -m "refactor: 移除仪表盘 s 设置弹窗（迁移至设置页 tab）"
```

---

### Task 6: README + 全量验证 + 端到端

**Files:**
- Modify: `README.md`

- [ ] **Step 1: README 更新**

1. 功能总览首段："五个页面" → "六个页面"，`1`-`5` → `1`-`6`。
2. 顶部 tab 示例图加 `│ 设置`。
3. 新增设置页小节（放在日志页小节后、仪表盘小节前或最后，参照现有格式）：

```markdown
**设置页**——集中编辑 config.yaml 可配置的网络参数（保存与运行时热切分离）：

```
┌ 设置 ─────────────────────────────────────────────────────────────────────┐
│ ── 网络 ──                                                               │
│ mode: ◀ rule ▶     ipv6: ◀ 否 ▶     allow-lan: ◀ 否 ▶                    │
│ ── 端口 ──                                                               │
│ port: 7890         socks-port: 7891        mixed-port: 7892              │
│ ── 日志 ──                                                               │
│ log-level: ◀ info ▶                                                      │
│ ── TUN ──                                                                │
│ tun.enable: ◀ 否 ▶   tun.stack: ◀ mixed ▶   tun.auto-route: ◀ 是 ▶       │
│ ...（其余区块与字段）                                                    │
└───────────────────────────────────────────────────────────────────────────┘
[未保存] ↑↓/Tab 移动 · ←→ 下拉 · Enter 编辑(secret 重新生成) · Ctrl+S 保存 · Ctrl+A 保存并应用
```

- 区块：网络（mode/ipv6/allow-lan）、端口（port/socks-port/mixed-port）、日志（log-level）、
  TUN（enable/stack/auto-route/mtu/dns-hijack）、DNS（enable/listen/enhanced-mode/
  fake-ip-range/nameserver/default-nameserver/fallback/fake-ip-filter）、其他
  （external-controller/secret）
- `↑↓`/`Tab` 移动字段，`←→` 循环下拉，`Enter` 编辑文本/数字字段（`Esc` 退出；
  secret 只读字段上按 `Enter` 重新生成 32 位密钥）
- `Ctrl+S` 仅保存 settings.toml（不重启不断网）；`Ctrl+A` 保存并应用——合并生成
  config.yaml → `mihomo -t` 校验 → 提权重启。校验失败弹窗提示并保留已填内容
- 与仪表盘热切的关系：仪表盘 `m`/`t`/`6` 是运行时立即生效（同时写回 settings.toml）；
  设置页是持久配置编辑——两者读写同一份 settings.toml，进入设置页自动同步最新值；
  设置页的改动需 `Ctrl+S`/`Ctrl+A` 才落盘
```

4. 仪表盘小节按键说明：`s` 从"网络设置表单（结构性变更流程）"改为"跳转设置页"。
5. 按键速查表（若 README 有）或底栏示例加设置页键。
6. 帮助/页脚示例中 `[s] 设置` 说明更新。

- [ ] **Step 2: 全量验证**

Run: `cargo build`
Expected: 0 errors
Run: `cargo test`
Expected: 全部 PASS（记录数量，计划前基线 220）
Run: `cargo clippy --all-targets 2>&1 | grep -c warning`
Expected: `0`

- [ ] **Step 3: 端到端验证（mihomo 配置生效）**

```bash
# 1. 构造临时配置目录并生成 config.yaml
cargo run --example merge_sample 2>/dev/null | head -50   # 若 examples/merge_sample.rs 是 CLI 样例则用它
# 或直接走 merger 测试路径验证字段映射（merger 测试已覆盖 port/dns.listen 等全部字段）
# 2. 若本机有 mihomo：对合并输出跑 mihomo -t 校验
which mihomo && cargo test merger  # merger 测试断言 config 含全部映射字段
```

预期：merger 测试覆盖全部 22 字段映射（已有）；若本机装有 mihomo 且可提权，可手动跑一次真实应用（改 dns.listen → Ctrl+A → `systemctl status mihomo` 确认重启成功、`mihomo -t` 通过）。真实提权操作如环境不允许则记录为手动验证步骤。

- [ ] **Step 4: 提交**

Commit:
```bash
git add README.md
git commit -m "docs: README 更新设置页说明（区块/按键/保存语义/与热切关系）"
```

---

## 自审记录（writing-plans 要求）

1. **Spec 覆盖**：
   - Tab 6 + s 全局跳转 + 移除弹窗 → Task 4/5 ✓
   - 区块布局 22 字段 → Task 2/3 ✓
   - Ctrl+S 仅保存 / Ctrl+A 保存并应用 → Task 3 ✓
   - 校验失败保留内容 + 焦点定位 → Task 3 ✓
   - secret 只读 + 重新生成 → Task 2/3 ✓
   - dirty 未保存标记 + on_enter 双向一致 → Task 3/4 ✓
   - 纯函数可测 → Task 2 ✓
   - README/帮助 → Task 4/6 ✓
2. **占位符扫描**：全部步骤含具体代码/命令；Task 3 中 `MessagePopup::handle_key` 语义、Task 6 中 merge_sample 用法标注了"先读源码确认"。
3. **类型一致性**：`field_values`/`apply_values`/`split_csv`/`SECTIONS`/`FIELD_COUNT`/`ValidationError`/`SettingsPage::new`/`on_enter` 在 Task 2/3/4 间签名一致；`ValidationError { label, message }` 的 label 与 `field_values` 的 label 完全对应。
