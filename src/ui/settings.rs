//! 设置页：config.yaml 可配置项集中编辑 + 运行方式区块（模式切换/路径/状态/启停）。
//! 交互规格见 docs/superpowers/specs/2026-08-12-settings-tab-design.md。
//! 本文件包含：纯函数（模型↔表单转换与校验）+ SettingsPage 整页表单。

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::app::{AppState, UiCommand};
use crate::core::apply::{find_mihomo_in_path, ProcOp, RunStatus};
use crate::core::merger::{merge, MergeContext};
use crate::core::models::{DnsSettings, NetworkSettings, RunMode, TunSettings};
use crate::core::settings::{generate_secret, save_settings};
use crate::service::installer::validate_mihomo_bin;
use crate::ui::widgets::{FieldKind, FormAction, FormField, FormPopup, MessagePopup};
use crate::ui::Page;

/// 区块定义：(标题, 字段起始索引, 字段数)。渲染与导航共用，顺序即字段顺序。
/// 首区块「运行方式」为 TUI 自身设置（不写入 config.yaml），其余为 config 字段。
pub(crate) const SECTIONS: &[(&str, usize, usize)] = &[
    ("运行方式", 0, 6),
    ("网络", 6, 3),
    ("端口", 9, 3),
    ("日志", 12, 1),
    ("TUN", 13, 5),
    ("DNS", 18, 8),
    ("其他", 26, 2),
];

/// 字段总数（= SECTIONS 覆盖的 0..FIELD_COUNT）。
pub(crate) const FIELD_COUNT: usize = 28;

/// config 字段起始索引（运行方式区块之后）。
pub(crate) const CONFIG_START: usize = 6;

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

/// 模型 → 表单字段（28 个：运行方式区块 6 + config 字段 22）。
/// f[1]/f[2]（路径/状态）为占位值，由 sync_from_settings 用 run_status 覆盖；
/// f[3..6]（启停按钮）按 run_mode 启用或禁用。
pub(crate) fn field_values(s: &NetworkSettings) -> Vec<FormField> {
    let yn = |b: bool| {
        if b {
            "是".to_string()
        } else {
            "否".to_string()
        }
    };
    let csv = |v: &[String]| v.join(",");
    let mode_str = match s.run_mode {
        RunMode::Systemd => "systemd",
        RunMode::Direct => "direct",
    };
    let action = |label: &str| FormField {
        label: label.into(),
        value: label.to_string(),
        kind: FieldKind::Action,
    };
    let mut fields = vec![
        FormField {
            label: "run-mode".into(),
            value: mode_str.into(),
            kind: FieldKind::Dropdown(vec!["systemd".into(), "direct".into()]),
        },
        FormField {
            label: "mihomo-bin".into(),
            value: "未设置（Enter 设置）".into(),
            kind: FieldKind::ReadOnly,
        },
        FormField {
            label: "mihomo-status".into(),
            value: "查询中…".into(),
            kind: FieldKind::ReadOnly,
        },
        action("启动"),
        action("停止"),
        action("重启"),
    ];
    fields.extend(vec![
        FormField {
            label: "mode".into(),
            value: s.mode.clone(),
            kind: FieldKind::Dropdown(vec!["rule".into(), "global".into(), "direct".into()]),
        },
        FormField {
            label: "ipv6".into(),
            value: yn(s.ipv6),
            kind: FieldKind::Dropdown(vec!["是".into(), "否".into()]),
        },
        FormField {
            label: "allow-lan".into(),
            value: yn(s.allow_lan),
            kind: FieldKind::Dropdown(vec!["是".into(), "否".into()]),
        },
        FormField {
            label: "port".into(),
            value: s.port.to_string(),
            kind: FieldKind::Number,
        },
        FormField {
            label: "socks-port".into(),
            value: s.socks_port.to_string(),
            kind: FieldKind::Number,
        },
        FormField {
            label: "mixed-port".into(),
            value: s.mixed_port.to_string(),
            kind: FieldKind::Number,
        },
        FormField {
            label: "log-level".into(),
            value: s.log_level.clone(),
            kind: FieldKind::Dropdown(vec![
                "silent".into(),
                "error".into(),
                "warning".into(),
                "info".into(),
                "debug".into(),
            ]),
        },
        FormField {
            label: "tun.enable".into(),
            value: yn(s.tun.enable),
            kind: FieldKind::Dropdown(vec!["是".into(), "否".into()]),
        },
        FormField {
            label: "tun.stack".into(),
            value: s.tun.stack.clone(),
            kind: FieldKind::Dropdown(vec!["system".into(), "gvisor".into(), "mixed".into()]),
        },
        FormField {
            label: "tun.auto-route".into(),
            value: yn(s.tun.auto_route),
            kind: FieldKind::Dropdown(vec!["是".into(), "否".into()]),
        },
        FormField {
            label: "tun.mtu".into(),
            value: s.tun.mtu.to_string(),
            kind: FieldKind::Number,
        },
        FormField {
            label: "tun.dns-hijack".into(),
            value: csv(&s.tun.dns_hijack),
            kind: FieldKind::Text,
        },
        FormField {
            label: "dns.enable".into(),
            value: yn(s.dns.enable),
            kind: FieldKind::Dropdown(vec!["是".into(), "否".into()]),
        },
        FormField {
            label: "dns.listen".into(),
            value: s.dns.listen.clone(),
            kind: FieldKind::Text,
        },
        FormField {
            label: "dns.enhanced-mode".into(),
            value: s.dns.enhanced_mode.clone(),
            kind: FieldKind::Dropdown(vec!["fake-ip".into(), "redir-host".into()]),
        },
        FormField {
            label: "dns.fake-ip-range".into(),
            value: s.dns.fake_ip_range.clone(),
            kind: FieldKind::Text,
        },
        FormField {
            label: "dns.nameserver".into(),
            value: csv(&s.dns.nameserver),
            kind: FieldKind::Text,
        },
        FormField {
            label: "dns.default-nameserver".into(),
            value: csv(&s.dns.default_nameserver),
            kind: FieldKind::Text,
        },
        FormField {
            label: "dns.fallback".into(),
            value: csv(&s.dns.fallback),
            kind: FieldKind::Text,
        },
        FormField {
            label: "dns.fake-ip-filter".into(),
            value: csv(&s.dns.fake_ip_filter),
            kind: FieldKind::Text,
        },
        FormField {
            label: "external-controller".into(),
            value: s.external_controller.clone(),
            kind: FieldKind::Text,
        },
        FormField {
            label: "secret".into(),
            value: s.secret.clone(),
            kind: FieldKind::ReadOnly,
        },
    ]);
    fields
}

fn err<T>(label: &str, message: &str) -> Result<T, ValidationError> {
    Err(ValidationError {
        label: label.into(),
        message: message.into(),
    })
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
    t.parse().map_err(|_| ValidationError {
        label: label.into(),
        message: format!("数值无效: {v}"),
    })
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
    let cfg = &f[CONFIG_START..];
    debug_assert_eq!(cfg.len(), 22, "config 字段应为 22 个");
    Ok(NetworkSettings {
        run_mode: match cfg_parse_dropdown(&f[0], &["systemd", "direct"])?.as_str() {
            "systemd" => RunMode::Systemd,
            _ => RunMode::Direct,
        },
        mode: parse_dropdown("mode", &cfg[0].value, &["rule", "global", "direct"])?,
        ipv6: parse_yn("ipv6", &cfg[1].value)?,
        allow_lan: parse_yn("allow-lan", &cfg[2].value)?,
        port: parse_u16("port", &cfg[3].value)?,
        socks_port: parse_u16("socks-port", &cfg[4].value)?,
        mixed_port: parse_u16("mixed-port", &cfg[5].value)?,
        log_level: parse_dropdown(
            "log-level",
            &cfg[6].value,
            &["silent", "error", "warning", "info", "debug"],
        )?,
        tun: TunSettings {
            enable: parse_yn("tun.enable", &cfg[7].value)?,
            stack: parse_dropdown("tun.stack", &cfg[8].value, &["system", "gvisor", "mixed"])?,
            auto_route: parse_yn("tun.auto-route", &cfg[9].value)?,
            mtu: parse_u16("tun.mtu", &cfg[10].value)?,
            dns_hijack: parse_csv("tun.dns-hijack", &cfg[11].value)?,
        },
        dns: DnsSettings {
            enable: parse_yn("dns.enable", &cfg[12].value)?,
            listen: nonempty("dns.listen", &cfg[13].value)?,
            enhanced_mode: parse_dropdown(
                "dns.enhanced-mode",
                &cfg[14].value,
                &["fake-ip", "redir-host"],
            )?,
            fake_ip_range: nonempty("dns.fake-ip-range", &cfg[15].value)?,
            nameserver: parse_csv("dns.nameserver", &cfg[16].value)?,
            default_nameserver: parse_csv("dns.default-nameserver", &cfg[17].value)?,
            fallback: parse_csv("dns.fallback", &cfg[18].value)?,
            fake_ip_filter: parse_csv("dns.fake-ip-filter", &cfg[19].value)?,
        },
        external_controller: nonempty("external-controller", &cfg[20].value)?,
        secret: nonempty("secret", &cfg[21].value)?,
    })
}

/// 运行方式下拉解析（label 用 f[0] 本身，复用 parse_dropdown 语义）。
fn cfg_parse_dropdown(field: &FormField, options: &[&str]) -> Result<String, ValidationError> {
    parse_dropdown(&field.label, &field.value, options)
}

/// 渲染行：区块标题或字段。
enum RenderRow {
    Section(&'static str),
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
    /// 路径输入弹窗（mihomo-bin Enter 打开；确认 → 校验 → SaveMihomoBin）
    pub(crate) path_popup: Option<FormPopup>,
}

/// 全部渲染行（含标题），供滚动与绘制。
fn render_rows() -> Vec<RenderRow> {
    let mut rows = Vec::new();
    for (name, start, len) in SECTIONS {
        rows.push(RenderRow::Section(name));
        for j in *start..*start + *len {
            rows.push(RenderRow::Field(j));
        }
    }
    rows
}

impl Default for SettingsPage {
    fn default() -> Self {
        Self::new()
    }
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
            path_popup: None,
        }
    }

    /// 当前值快照（dirty 判断与保存基准）。
    fn values(&self) -> Vec<String> {
        self.fields.iter().map(|f| f.value.clone()).collect()
    }

    /// 是否有未保存修改（只比较 run-mode 与 config 字段；
    /// 路径/状态/按钮显示值随状态刷新变化，不参与 dirty）。
    fn dirty(&self) -> bool {
        if self.saved.is_empty() {
            return false;
        }
        let v = self.values();
        v[0] != self.saved[0] || v[CONFIG_START..] != self.saved[CONFIG_START..]
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
        self.path_popup = None;
        self.apply_status_display(st);
    }

    /// 用 st.run_status 覆盖 f[1]/f[2]（路径/状态）显示值，并同步光标数组
    /// （光标是字节位置，值被覆盖后不更新会导致渲染时切片越界 panic）。
    /// sync_from_settings（全量同步）与 refresh_state（仅显示刷新）共用。
    fn apply_status_display(&mut self, st: &AppState) {
        if let Some(rs) = &st.run_status {
            let bin_text = rs
                .proc
                .as_ref()
                .and_then(|p| p.bin.clone())
                .unwrap_or_else(|| "未设置（Enter 设置）".to_string());
            self.fields[1].value = bin_text;
            self.fields[2].value = run_status_text(st.settings.run_mode, rs);
            self.cursor[1] = self.fields[1].value.len();
            self.cursor[2] = self.fields[2].value.len();
        }
    }

    /// 保存（可选应用）。校验失败/落盘失败/合并失败 → 弹窗，内容保留。
    fn save(&mut self, st: &mut AppState, apply: bool) -> Option<UiCommand> {
        match apply_values(&self.fields) {
            Err(e) => {
                if let Some(i) = self.fields.iter().position(|f| f.label == e.label) {
                    self.focused = i;
                }
                self.popup = Some(MessagePopup::new("校验失败".into(), vec![e.to_string()]));
                None
            }
            Ok(s) => {
                let old_mode = st.settings.run_mode;
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
                // 模式切换 systemd ← direct 且进程实例运行中：仅 Ctrl+S（apply=false）时
                // 自动停止（防止双实例）；apply=true 不拦截，继续走 apply 链路——
                // mihomo-apply 的进程守卫会停掉进程实例（spec 已设计）。
                let stop_proc = s.run_mode == RunMode::Systemd
                    && old_mode == RunMode::Direct
                    && st
                        .run_status
                        .as_ref()
                        .and_then(|rs| rs.proc.as_ref())
                        .map(|p| p.running)
                        .unwrap_or(false);
                if stop_proc && !apply {
                    st.notice("[!] 已切换到 systemd 模式，正在停止进程实例…".to_string());
                    return Some(UiCommand::ProcAction(ProcOp::Stop));
                }
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

/// 运行状态文本（设置页 status 字段显示）。
pub(crate) fn run_status_text(mode: RunMode, rs: &RunStatus) -> String {
    match mode {
        RunMode::Systemd => {
            let svc = match (rs.service_unit, rs.service_active) {
                // 服务 active 是最强证据（active 必然意味着单元存在），优先于单元检测
                (_, Some(true)) => "服务运行中".to_string(),
                (Some(false), _) => "未安装 mihomo.service（Enter 查看指引）".to_string(),
                (_, _) => "服务未运行（Enter 启动）".to_string(),
            };
            match rs.proc.as_ref() {
                Some(p) if p.running => {
                    format!("{svc}；进程实例运行中（PID {}）", p.pid.unwrap_or(0))
                }
                _ => svc,
            }
        }
        RunMode::Direct => match rs.proc.as_ref() {
            None => "查询失败（未安装提权组件？仪表盘按 i 重新安装）".to_string(),
            Some(p) if p.bin.is_none() => "未设置路径（Enter 设置）".to_string(),
            Some(p) if p.running => format!("运行中（PID {}）", p.pid.unwrap_or(0)),
            Some(_) => "未运行（无开机自启，重启系统后需手动启动）".to_string(),
        },
    }
}

impl Page for SettingsPage {
    fn popup_open(&self) -> bool {
        self.popup.is_some() || self.path_popup.is_some()
    }

    /// 编辑模式接管全局键：输入任意字符（数字/字母/符号）不会触发
    /// 全局的 q 退出、数字切页、? 帮助、Tab 切页等。
    fn consumes_global_keys(&self) -> bool {
        self.editing
    }

    fn on_enter(&mut self, st: &AppState) {
        self.sync_from_settings(st);
    }

    /// RunStatusDone/ProcActionDone 后的状态刷新：仅覆盖 f[1]/f[2] 显示值，
    /// 不动 focused/offset/editing/dirty（避免打断编辑或清掉未保存标记）。
    fn refresh_state(&mut self, st: &AppState) {
        if self.fields.len() < 3 {
            return;
        }
        self.apply_status_display(st);
    }

    fn handle_key(&mut self, key: KeyEvent, st: &mut AppState) -> Option<UiCommand> {
        // 未同步（未 on_enter）时无可操作字段：直接返回，防止越界 panic
        if self.fields.is_empty() {
            return None;
        }
        // 错误弹窗优先（关闭后回到表单，内容保留）
        if let Some(mut popup) = self.popup.take() {
            if !popup.handle_key(key) {
                self.popup = Some(popup);
            }
            return None;
        }
        // 路径输入弹窗优先：Confirm → 校验路径 → 通过则提权保存命令
        if let Some(mut popup) = self.path_popup.take() {
            match popup.handle_key(key) {
                Some(FormAction::Confirm) => {
                    let path = popup.value(0).trim().to_string();
                    if let Err(e) = validate_mihomo_bin(&path) {
                        self.popup = Some(MessagePopup::new("路径无效".into(), vec![e]));
                        return None;
                    }
                    return Some(UiCommand::SaveMihomoBin(path));
                }
                Some(FormAction::Cancel) => {}
                None => self.path_popup = Some(popup),
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
                KeyCode::Char(c) => {
                    // Number 字段过滤非数字（与 FormPopup 对齐）：
                    // 避免 "abc" 混入后再校验失败，输入期即拦截
                    let is_number = matches!(self.fields[self.focused].kind, FieldKind::Number);
                    if !is_number || c.is_ascii_digit() {
                        self.insert_char(c);
                    }
                }
                _ => {}
            }
            return None;
        }
        match key.code {
            KeyCode::Up => self.focus_move(-1),
            KeyCode::Down => self.focus_move(1),
            KeyCode::Home => self.focused = 0,
            KeyCode::End => self.focused = FIELD_COUNT - 1,
            KeyCode::Enter => match &self.fields[self.focused].kind {
                FieldKind::ReadOnly => match self.focused {
                    // secret：重新生成（32 hex）
                    i if i == FIELD_COUNT - 1 => {
                        self.fields[i].value = generate_secret();
                        self.cursor[i] = self.fields[i].value.len();
                    }
                    // mihomo 路径：打开输入弹窗（预填已有路径或 which 结果）
                    1 => {
                        let current = self.fields[1].value.clone();
                        let prefill = if current.starts_with('/') {
                            current
                        } else {
                            find_mihomo_in_path().unwrap_or_default()
                        };
                        self.path_popup = Some(FormPopup::new(
                            "设置 mihomo 路径".into(),
                            vec![FormField {
                                label: "路径".into(),
                                value: prefill,
                                kind: FieldKind::Text,
                            }],
                        ));
                    }
                    // 状态行：按模式与状态分派
                    2 => {
                        let rs = st.run_status.clone();
                        let mode = st.settings.run_mode;
                        match mode {
                            RunMode::Direct => return Some(UiCommand::RefreshStatus),
                            RunMode::Systemd => match rs {
                                Some(rs) if rs.service_unit == Some(false) => {
                                    self.popup = Some(MessagePopup::new(
                                        "未安装 mihomo.service".into(),
                                        vec![
                                            "未检测到 systemd 单元。可选方案：".into(),
                                            "1. 参照 README「手动安装」创建 mihomo.service 后：sudo systemctl daemon-reload".into(),
                                            "2. 仪表盘按 i 安装提权组件（仍要求单元存在）".into(),
                                            "3. Enter mihomo-bin 设置路径，切换 direct 模式（无需 systemd）".into(),
                                        ],
                                    ));
                                }
                                Some(rs) if rs.service_active == Some(true) => {
                                    return Some(UiCommand::RefreshStatus)
                                }
                                _ => {
                                    // systemd 服务未运行：直接 systemctl start（polkit 弹窗认证）
                                    return Some(UiCommand::SystemdAction(ProcOp::Start));
                                }
                            },
                        }
                    }
                    _ => {}
                },
                FieldKind::Action => {
                    // 以 run-mode 字段（f[0]）为准选择执行通道：f[3..6] 按钮值在
                    // Ctrl+S 保存后不会重新生成，可能残留旧模式文案。
                    // systemd → systemctl（polkit 弹窗认证）；direct → mihomo-proc。
                    let direct = self.fields[0].value == "direct";
                    let op = match self.focused {
                        3 => Some(ProcOp::Start),
                        4 => Some(ProcOp::Stop),
                        5 => Some(ProcOp::Restart),
                        _ => None,
                    };
                    if let Some(op) = op {
                        return Some(if direct {
                            UiCommand::ProcAction(op)
                        } else {
                            UiCommand::SystemdAction(op)
                        });
                    }
                }
                FieldKind::Dropdown(_) => self.cycle_dropdown(1),
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
        // 状态行：未保存标记 + 编辑中标记 + 按键提示 + 焦点字段提示
        let dirty = self.dirty();
        let hint = "↑↓ 移动 · Enter 编辑/循环/执行 · Ctrl+S 保存 · Ctrl+A 保存并应用";
        let focus = self
            .fields
            .get(self.focused)
            .map(|f| format!(" · 当前: {}", f.label))
            .unwrap_or_default();
        let status_text = format!(
            "{}{}{}{}",
            if dirty { "[未保存] " } else { "" },
            if self.editing { "[编辑中] " } else { "" },
            hint,
            focus
        );
        let status_fg = if dirty {
            Color::Yellow
        } else if self.editing {
            Color::Cyan
        } else {
            Color::DarkGray
        };
        f.render_widget(
            Paragraph::new(Span::styled(status_text, Style::default().fg(status_fg))),
            status,
        );

        // 尚未同步过（未调用 on_enter）：无可渲染字段
        if self.fields.is_empty() {
            return;
        }
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
                            Style::default()
                                .fg(Color::Cyan)
                                .add_modifier(Modifier::BOLD),
                        )),
                        Rect::new(body.x, y, body.width, 1),
                    );
                }
                RenderRow::Field(idx) => {
                    let focused = *idx == self.focused;
                    let field = &self.fields[*idx];
                    let label = if field.label.len() as u16 > label_w {
                        field
                            .label
                            .chars()
                            .take(label_w as usize)
                            .collect::<String>()
                    } else {
                        field.label.clone()
                    };
                    let label_style = if focused {
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD)
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
                        FieldKind::Action => {
                            // 两种模式按钮均可用：systemd → systemctl（polkit 弹窗认证），
                            // direct → mihomo-proc；分派见 handle_key
                            let text = format!("[ {} ]", field.value);
                            f.render_widget(
                                Paragraph::new(Span::styled(text, value_style)),
                                Rect::new(vx, y, vw, 1),
                            );
                        }
                        _ => {
                            // 光标字节位置按值长度钳位（值可能被状态刷新覆盖，防御越界）
                            let cur = self.cursor[*idx].min(field.value.len());
                            let cur_chars = field.value[..cur].chars().count();
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
                                f.set_cursor_position(Position::new(cur_x, y));
                            }
                        }
                    }
                }
            }
        }

        if let Some(popup) = &mut self.popup {
            popup.render(f, area);
        }
        if let Some(popup) = &mut self.path_popup {
            popup.render(f, area);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 固定设置（不用 Default——secret 每次 Default 重新生成，无法比较）。
    /// 全字段显式给出（避免 clippy field_reassign_with_default/needless_update）。
    fn fixed_settings() -> NetworkSettings {
        NetworkSettings {
            secret: "a".repeat(32),
            run_mode: RunMode::Systemd,
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
        assert_eq!(back.run_mode, RunMode::Systemd);
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
        let s = NetworkSettings {
            secret: "b".repeat(32),
            ..NetworkSettings::default()
        };
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
        fields[9].value = "".into();
        let e = apply_values(&fields).unwrap_err();
        assert_eq!(e.label, "port");
        assert!(e.to_string().contains("port"));
        // 越界
        fields[9].value = "65536".into();
        assert_eq!(apply_values(&fields).unwrap_err().label, "port");
        // 非数字
        fields[9].value = "abc".into();
        assert_eq!(apply_values(&fields).unwrap_err().label, "port");
        // 空 CSV（先恢复合法端口，否则 port 先报错）
        fields[9].value = "1080".into();
        fields[22].value = " , , ".into();
        let e = apply_values(&fields).unwrap_err();
        assert_eq!(e.label, "dns.nameserver");
        // 空文本
        fields[22].value = "1.1.1.1".into();
        fields[19].value = "".into();
        assert_eq!(apply_values(&fields).unwrap_err().label, "dns.listen");
        // 非法枚举（绕过 UI 直接改值）
        fields[19].value = "0.0.0.0:1053".into();
        fields[6].value = "hack".into();
        assert_eq!(apply_values(&fields).unwrap_err().label, "mode");
    }

    /// secret 字段：ReadOnly + 值透传。
    #[test]
    fn secret_field_is_readonly() {
        let s = fixed_settings();
        let fields = field_values(&s);
        assert_eq!(fields[27].label, "secret");
        assert_eq!(fields[27].value, "a".repeat(32));
        assert_eq!(fields[27].kind, FieldKind::ReadOnly);
    }

    /// split_csv：分割、trim、去空项。
    #[test]
    fn split_csv_trims_and_drops_empty() {
        assert_eq!(split_csv(" a, b ,,c "), vec!["a", "b", "c"]);
        assert_eq!(split_csv(""), Vec::<String>::new());
        assert_eq!(split_csv(" , "), Vec::<String>::new());
    }

    // ---- SettingsPage 整页表单 ----

    use crate::app::{AppState, UiCommand};
    use crate::core::apply::ProcStatus;
    use crate::core::client::RuntimeConfig;
    use crate::core::settings::{load_settings, save_settings, with_settings_dir};
    use crate::ui::Page;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
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
            run_status: None,
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
        assert_eq!(p.fields[9].value, "8888");
        assert!(!p.dirty(), "同步后不应是未保存状态");
    }

    /// dirty 时 on_enter 不覆盖（未保存编辑保留）。
    #[test]
    fn on_enter_keeps_dirty_edits() {
        let mut st = test_state();
        let mut p = page_with_state(&st);
        // 编辑 port 字段：选中 → Enter 进编辑 → 输入 9
        p.focused = 9;
        press(
            &mut p,
            &mut st,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );
        press(
            &mut p,
            &mut st,
            KeyEvent::new(KeyCode::Char('9'), KeyModifiers::NONE),
        );
        assert!(p.dirty());
        // st.settings 被外部改动（如仪表盘热切）后 on_enter 不应覆盖编辑
        st.settings.port = 7777;
        p.on_enter(&st);
        assert!(p.fields[9].value.contains('9'), "dirty 时不应重新同步");
    }

    /// Ctrl+S 仅保存：落盘 + st.settings 同步 + 无命令 + 清除 dirty。
    #[test]
    fn ctrl_s_saves_without_applying() {
        with_settings_dir(|| {
            let mut st = test_state();
            save_settings(&st.settings).unwrap();
            let mut p = page_with_state(&st);
            p.focused = 9;
            press(
                &mut p,
                &mut st,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            );
            // 编辑模式为追加：先清空原值（7890）再输入新端口
            for _ in 0..4 {
                press(
                    &mut p,
                    &mut st,
                    KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
                );
            }
            press(
                &mut p,
                &mut st,
                KeyEvent::new(KeyCode::Char('9'), KeyModifiers::NONE),
            );
            press(
                &mut p,
                &mut st,
                KeyEvent::new(KeyCode::Char('0'), KeyModifiers::NONE),
            );
            press(
                &mut p,
                &mut st,
                KeyEvent::new(KeyCode::Char('8'), KeyModifiers::NONE),
            );
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
            p.focused = 9;
            press(
                &mut p,
                &mut st,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            );
            for _ in 0..4 {
                press(
                    &mut p,
                    &mut st,
                    KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
                );
            }
            press(
                &mut p,
                &mut st,
                KeyEvent::new(KeyCode::Char('9'), KeyModifiers::NONE),
            );
            press(
                &mut p,
                &mut st,
                KeyEvent::new(KeyCode::Char('0'), KeyModifiers::NONE),
            );
            press(
                &mut p,
                &mut st,
                KeyEvent::new(KeyCode::Char('8'), KeyModifiers::NONE),
            );
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
            p.focused = 9;
            press(
                &mut p,
                &mut st,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            );
            for _ in 0..4 {
                press(
                    &mut p,
                    &mut st,
                    KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
                );
            }
            let cmd = press(&mut p, &mut st, ctrl('s'));
            assert!(cmd.is_none());
            assert!(p.popup.is_some(), "应有错误弹窗");
            assert_eq!(p.focused, 9, "焦点应留在出错字段");
            assert!(p.fields[9].value.is_empty(), "已填内容应保留");
            let back = load_settings().unwrap();
            assert_eq!(back.port, 7890, "失败时不应落盘");
            // 关闭弹窗后仍可继续编辑（内容未丢）
            press(
                &mut p,
                &mut st,
                KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            );
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
            press(
                &mut p,
                &mut st,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            );
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

    /// 下拉循环：Enter 循环选项，↓ 移动选中行（Tab/←→ 让位给全局切页）。
    #[test]
    fn dropdown_cycle_and_navigation() {
        let mut st = test_state();
        let mut p = page_with_state(&st);
        assert_eq!(p.focused, 0);
        // ↓ 从 run-mode 走到 mode（跳过运行方式区块前 6 字段）
        for _ in 0..6 {
            press(
                &mut p,
                &mut st,
                KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
            );
        }
        assert_eq!(p.focused, 6);
        // mode → global → direct（Enter 循环）
        press(
            &mut p,
            &mut st,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );
        assert_eq!(p.fields[6].value, "global");
        press(
            &mut p,
            &mut st,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );
        assert_eq!(p.fields[6].value, "direct");
        // ↓ 走到下一个字段
        press(
            &mut p,
            &mut st,
            KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
        );
        assert_eq!(p.focused, 7);
    }

    /// 文本字段编辑模式：Enter 进入、End 光标定位末尾、输入、Esc 退出。
    #[test]
    fn text_field_edit_mode() {
        let mut st = test_state();
        let mut p = page_with_state(&st);
        // dns.listen（index 19）进入编辑后追加端口
        p.focused = 19;
        press(
            &mut p,
            &mut st,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );
        assert!(p.editing, "Enter 应进入编辑模式");
        press(
            &mut p,
            &mut st,
            KeyEvent::new(KeyCode::End, KeyModifiers::NONE),
        );
        press(
            &mut p,
            &mut st,
            KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE),
        );
        press(
            &mut p,
            &mut st,
            KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE),
        );
        press(
            &mut p,
            &mut st,
            KeyEvent::new(KeyCode::Char('5'), KeyModifiers::NONE),
        );
        press(
            &mut p,
            &mut st,
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
        );
        assert_eq!(p.fields[19].value, "0.0.0.0:1053:15");
        assert!(!p.editing, "Esc 退出编辑模式");
    }

    /// Number 字段编辑模式过滤非数字字符（与 FormPopup 对齐，P2-1）。
    #[test]
    fn number_field_edit_filters_non_digits() {
        let mut st = test_state();
        let mut p = page_with_state(&st);
        p.focused = 9; // port
        press(
            &mut p,
            &mut st,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );
        press(
            &mut p,
            &mut st,
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE),
        );
        assert_eq!(p.fields[9].value, "7890", "非数字不应插入");
        press(
            &mut p,
            &mut st,
            KeyEvent::new(KeyCode::Char('-'), KeyModifiers::NONE),
        );
        assert_eq!(p.fields[9].value, "7890", "符号也不应插入");
        press(
            &mut p,
            &mut st,
            KeyEvent::new(KeyCode::Char('5'), KeyModifiers::NONE),
        );
        assert_eq!(p.fields[9].value, "78905", "数字应插入");
        // Text 字段不受限（数字字母均可）
        press(
            &mut p,
            &mut st,
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
        );
        p.focused = 19;
        press(
            &mut p,
            &mut st,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );
        press(
            &mut p,
            &mut st,
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE),
        );
        assert_eq!(
            p.fields[19].value, "0.0.0.0:1053a",
            "Text 字段任意字符可插入"
        );
    }

    /// 未同步（fields 为空，未 on_enter）时 handle_key 安全返回 None（P3 守卫）。
    #[test]
    fn handle_key_without_sync_is_noop() {
        let mut st = test_state();
        let mut p = SettingsPage::new(); // 未 on_enter：fields 为空
        assert!(p.fields.is_empty());
        for key in [
            KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('5'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
        ] {
            assert!(press(&mut p, &mut st, key).is_none(), "{key:?} 应无操作");
        }
        assert!(p.fields.is_empty(), "不应产生字段");
    }

    /// 运行方式区块：run-mode 下拉循环 + Ctrl+S 持久化 run_mode。
    #[test]
    fn run_mode_dropdown_cycles_and_persists() {
        with_settings_dir(|| {
            let mut st = test_state();
            save_settings(&st.settings).unwrap();
            let mut p = page_with_state(&st);
            assert_eq!(p.fields[0].value, "systemd");
            press(
                &mut p,
                &mut st,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            );
            assert_eq!(p.fields[0].value, "direct");
            assert!(p.dirty(), "切换后应标记未保存");
            press(&mut p, &mut st, ctrl('s'));
            let back = load_settings().unwrap();
            assert_eq!(back.run_mode, RunMode::Direct);
        });
    }

    /// dirty 排除规则：状态显示值变化（模拟刷新）不污染未保存标记。
    #[test]
    fn status_field_changes_do_not_mark_dirty() {
        let st = test_state();
        let mut p = page_with_state(&st);
        // 模拟状态刷新写入 f[1]/f[2]
        p.fields[1].value = "/usr/bin/mihomo".into();
        p.fields[2].value = "运行中（PID 1234）".into();
        assert!(!p.dirty(), "状态字段变化不应触发未保存标记");
        // 但 run-mode 变化会
        p.fields[0].value = "direct".into();
        assert!(p.dirty());
    }

    /// 路径字段 Enter → 打开路径输入弹窗（预填 which 结果或已有路径）。
    #[test]
    fn bin_field_enter_opens_path_popup_with_prefill() {
        let mut st = test_state();
        st.run_status = Some(RunStatus {
            service_unit: Some(true),
            service_active: Some(true),
            proc: Some(ProcStatus {
                bin: Some("/usr/bin/mihomo".into()),
                pid: None,
                running: false,
            }),
        });
        let mut p = page_with_state(&st);
        p.focused = 1;
        press(
            &mut p,
            &mut st,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );
        let popup = p.path_popup.as_ref().expect("应打开路径弹窗");
        assert_eq!(popup.value(0), "/usr/bin/mihomo", "已有路径应预填");
        // 未配置时预填 which mihomo 结果（环境自适应）
        st.run_status = None;
        let mut p2 = page_with_state(&st);
        p2.focused = 1;
        press(
            &mut p2,
            &mut st,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );
        let popup = p2.path_popup.as_ref().expect("应打开路径弹窗");
        let expected = crate::core::apply::find_mihomo_in_path().unwrap_or_default();
        assert_eq!(popup.value(0), expected);
    }

    /// 路径弹窗确认：校验失败 → 页内错误弹窗不返回命令；校验通过 → SaveMihomoBin 命令。
    #[test]
    fn path_popup_confirm_validates_and_returns_command() {
        let mut st = test_state();
        let mut p = page_with_state(&st);
        p.focused = 1;
        press(
            &mut p,
            &mut st,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );
        // 清空预填（反复 Backspace）→ 空路径确认 → 校验错误弹窗、无命令
        {
            let n = p.path_popup.as_ref().unwrap().value(0).len();
            for _ in 0..n {
                press(
                    &mut p,
                    &mut st,
                    KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
                );
            }
        }
        let cmd = press(
            &mut p,
            &mut st,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );
        assert!(cmd.is_none(), "空路径不应返回命令");
        assert!(p.popup.is_some(), "应有校验错误弹窗");
        // 关闭错误弹窗
        press(
            &mut p,
            &mut st,
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
        );
        assert!(p.popup.is_none());
        // 环境自适应：PATH 中能找到 mihomo 时，重新打开弹窗预填合法路径，直接确认即可提交
        if crate::core::apply::find_mihomo_in_path().is_some() {
            press(
                &mut p,
                &mut st,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            );
            let cmd = press(
                &mut p,
                &mut st,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            );
            match cmd {
                Some(UiCommand::SaveMihomoBin(path)) => {
                    assert!(path.starts_with('/'));
                    assert!(
                        std::path::Path::new(&path).exists(),
                        "预填路径应存在: {path}"
                    );
                }
                other => panic!("应返回 SaveMihomoBin: {other:?}"),
            }
            assert!(p.path_popup.is_none(), "确认后弹窗应关闭");
        }
    }

    /// Action 字段：direct 模式 Enter → ProcAction（mihomo-proc）；systemd 模式 → SystemdAction（systemctl/polkit）。
    #[test]
    fn action_fields_dispatch_by_mode() {
        let mut st = test_state();
        st.settings.run_mode = RunMode::Direct;
        let mut p = page_with_state(&st);
        p.focused = 3;
        let cmd = press(
            &mut p,
            &mut st,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );
        assert!(matches!(cmd, Some(UiCommand::ProcAction(ProcOp::Start))));
        p.focused = 4;
        let cmd = press(
            &mut p,
            &mut st,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );
        assert!(matches!(cmd, Some(UiCommand::ProcAction(ProcOp::Stop))));
        p.focused = 5;
        let cmd = press(
            &mut p,
            &mut st,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );
        assert!(matches!(cmd, Some(UiCommand::ProcAction(ProcOp::Restart))));
        // systemd 模式：按钮可用，走 systemctl（polkit 弹窗认证）
        let mut st2 = test_state();
        st2.settings.run_mode = RunMode::Systemd;
        let mut p2 = page_with_state(&st2);
        assert_eq!(p2.fields[3].value, "启动", "systemd 模式按钮应可用");
        p2.focused = 3;
        let cmd = press(
            &mut p2,
            &mut st2,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );
        assert!(
            matches!(cmd, Some(UiCommand::SystemdAction(ProcOp::Start))),
            "systemd 模式应派发 SystemdAction: {cmd:?}"
        );
        p2.focused = 4;
        let cmd = press(
            &mut p2,
            &mut st2,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );
        assert!(matches!(cmd, Some(UiCommand::SystemdAction(ProcOp::Stop))));
        p2.focused = 5;
        let cmd = press(
            &mut p2,
            &mut st2,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );
        assert!(matches!(
            cmd,
            Some(UiCommand::SystemdAction(ProcOp::Restart))
        ));
    }

    /// status 字段 Enter 分派：systemd+active → 刷新；systemd+inactive → SystemdAction(Start)（polkit）；
    /// 单元缺失 → 页内指引弹窗；direct → 刷新。
    #[test]
    fn status_field_enter_dispatches() {
        // systemd + 运行中 → RefreshStatus
        let mut st = test_state();
        st.run_status = Some(RunStatus {
            service_unit: Some(true),
            service_active: Some(true),
            proc: None,
        });
        let mut p = page_with_state(&st);
        p.focused = 2;
        let cmd = press(
            &mut p,
            &mut st,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );
        assert!(
            matches!(cmd, Some(UiCommand::RefreshStatus)),
            "运行中应刷新: {cmd:?}"
        );
        // systemd + 未运行 → SystemdAction(Start)（systemctl 直接执行，polkit 弹窗认证）
        st.run_status = Some(RunStatus {
            service_unit: Some(true),
            service_active: Some(false),
            proc: None,
        });
        let mut p = page_with_state(&st);
        p.focused = 2;
        let cmd = press(
            &mut p,
            &mut st,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );
        assert!(
            matches!(cmd, Some(UiCommand::SystemdAction(ProcOp::Start))),
            "未运行应派发 SystemdAction(Start): {cmd:?}"
        );
        // 单元缺失 → 指引弹窗（无命令）
        st.run_status = Some(RunStatus {
            service_unit: Some(false),
            service_active: Some(false),
            proc: None,
        });
        let mut p = page_with_state(&st);
        p.focused = 2;
        let cmd = press(
            &mut p,
            &mut st,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );
        assert!(cmd.is_none(), "单元缺失应只弹指引: {cmd:?}");
        assert!(p.popup.is_some());
        // direct → 刷新
        st.settings.run_mode = RunMode::Direct;
        let mut p = page_with_state(&st);
        p.focused = 2;
        let cmd = press(
            &mut p,
            &mut st,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );
        assert!(matches!(cmd, Some(UiCommand::RefreshStatus)));
    }

    /// 模式切换 systemd ← direct 且进程运行中：Ctrl+S 保存后返回 ProcAction(Stop)。
    #[test]
    fn mode_switch_to_systemd_returns_stop_when_proc_running() {
        with_settings_dir(|| {
            let mut st = test_state();
            st.settings.run_mode = RunMode::Direct;
            st.run_status = Some(RunStatus {
                service_unit: Some(true),
                service_active: Some(false),
                proc: Some(ProcStatus {
                    bin: Some("/usr/bin/mihomo".into()),
                    pid: Some(42),
                    running: true,
                }),
            });
            save_settings(&st.settings).unwrap();
            let mut p = page_with_state(&st);
            // 切回 systemd
            p.focused = 0;
            press(
                &mut p,
                &mut st,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            ); // direct → systemd
            let cmd = press(&mut p, &mut st, ctrl('s'));
            assert!(
                matches!(cmd, Some(UiCommand::ProcAction(ProcOp::Stop))),
                "进程运行中切 systemd 应自动停止: {cmd:?}"
            );
            let back = load_settings().unwrap();
            assert_eq!(back.run_mode, RunMode::Systemd);
            // 进程未运行时切换：无 stop 命令
            let mut st2 = test_state();
            st2.settings.run_mode = RunMode::Direct;
            st2.run_status = Some(RunStatus {
                service_unit: Some(true),
                service_active: Some(false),
                proc: Some(ProcStatus {
                    bin: Some("/usr/bin/mihomo".into()),
                    pid: None,
                    running: false,
                }),
            });
            save_settings(&st2.settings).unwrap();
            let mut p2 = page_with_state(&st2);
            p2.focused = 0;
            press(
                &mut p2,
                &mut st2,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            );
            let cmd = press(&mut p2, &mut st2, ctrl('s'));
            assert!(cmd.is_none(), "未运行无需停止: {cmd:?}");
        });
    }

    /// 模式切换 systemd ← direct 且进程运行中 + Ctrl+A（apply=true）：
    /// 不拦截返回 Stop，继续 apply 链路（mihomo-apply 的进程守卫会停掉实例）。
    #[test]
    fn mode_switch_to_systemd_ctrl_a_continues_to_apply() {
        with_settings_dir(|| {
            let mut st = test_state();
            st.settings.run_mode = RunMode::Direct;
            st.run_status = Some(RunStatus {
                service_unit: Some(true),
                service_active: Some(false),
                proc: Some(ProcStatus {
                    bin: Some("/usr/bin/mihomo".into()),
                    pid: Some(42),
                    running: true,
                }),
            });
            save_settings(&st.settings).unwrap();
            let mut p = page_with_state(&st);
            // 切回 systemd
            p.focused = 0;
            press(
                &mut p,
                &mut st,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            ); // direct → systemd
            let cmd = press(&mut p, &mut st, ctrl('a'));
            assert!(
                matches!(cmd, Some(UiCommand::ApplyConfig(_))),
                "apply=true 应返回 ApplyConfig 而非 Stop: {cmd:?}"
            );
            let back = load_settings().unwrap();
            assert_eq!(back.run_mode, RunMode::Systemd);
        });
    }

    /// Action 分派以 run-mode 字段（f[0]）为准：切 systemd + Ctrl+S 后按钮值陈旧
    /// （仍为“启动/停止/重启”）也不应派发 ProcAction；切回 direct 应恢复派发。
    #[test]
    fn action_dispatch_ignores_stale_button_values() {
        with_settings_dir(|| {
            let mut st = test_state();
            st.settings.run_mode = RunMode::Direct;
            save_settings(&st.settings).unwrap();
            let mut p = page_with_state(&st);
            // 切 systemd + Ctrl+S：fields[3..6] 不重新生成，保持 direct 文案
            p.focused = 0;
            press(
                &mut p,
                &mut st,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            ); // direct → systemd
            let cmd = press(&mut p, &mut st, ctrl('s'));
            assert!(cmd.is_none(), "仅保存不应返回命令: {cmd:?}");
            assert_eq!(p.fields[0].value, "systemd");
            assert_eq!(p.fields[3].value, "启动", "按钮显示值未重新生成（陈旧）");
            // systemd 模式：陈旧按钮值不误派发 ProcAction，正确走 SystemdAction（polkit）
            for (idx, op) in [(3, ProcOp::Start), (4, ProcOp::Stop), (5, ProcOp::Restart)] {
                p.focused = idx;
                let cmd = press(
                    &mut p,
                    &mut st,
                    KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                );
                let dispatched = matches!(cmd, Some(UiCommand::SystemdAction(o)) if o == op);
                assert!(
                    dispatched,
                    "f[{idx}] systemd 模式应派发 SystemdAction({op:?}): {cmd:?}"
                );
            }
            // 切回 direct（未重新同步，按钮值仍陈旧）→ 按 f[0] 判定应恢复派发
            p.focused = 0;
            press(
                &mut p,
                &mut st,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            ); // systemd → direct
            p.focused = 3;
            let cmd = press(
                &mut p,
                &mut st,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            );
            assert!(
                matches!(cmd, Some(UiCommand::ProcAction(ProcOp::Start))),
                "direct 模式应派发 Start: {cmd:?}"
            );
        });
    }

    /// run_status_text 判定顺序：服务 active 优先于单元检测（active 即单元存在的最强证据）。
    #[test]
    fn run_status_text_active_beats_unit_missing() {
        // 单元检测异常为 false 但服务实际 active → 显示运行中（回归：曾优先显示"未安装"）
        let rs = RunStatus {
            service_unit: Some(false),
            service_active: Some(true),
            proc: None,
        };
        assert_eq!(run_status_text(RunMode::Systemd, &rs), "服务运行中");
        // 单元缺失 + 服务未运行 → 指引
        let rs = RunStatus {
            service_unit: Some(false),
            service_active: Some(false),
            proc: None,
        };
        assert_eq!(
            run_status_text(RunMode::Systemd, &rs),
            "未安装 mihomo.service（Enter 查看指引）"
        );
        // 单元存在 + 未运行 → 可启动
        let rs = RunStatus {
            service_unit: Some(true),
            service_active: Some(false),
            proc: None,
        };
        assert_eq!(
            run_status_text(RunMode::Systemd, &rs),
            "服务未运行（Enter 启动）"
        );
        // direct 模式：未设置路径
        let rs = RunStatus {
            service_unit: Some(true),
            service_active: Some(true),
            proc: Some(ProcStatus {
                bin: None,
                pid: None,
                running: false,
            }),
        };
        assert_eq!(
            run_status_text(RunMode::Direct, &rs),
            "未设置路径（Enter 设置）"
        );
    }

    /// refresh_state：仅覆盖 f[1]/f[2] 显示值，不动 dirty/focused/editing；
    /// 未同步（fields 为空）时安全无操作。
    #[test]
    fn refresh_state_updates_status_display_only() {
        let mut st = test_state();
        st.settings.run_mode = RunMode::Direct;
        let mut p = page_with_state(&st);
        // 进入编辑（port 追加 9）→ dirty + editing + focused 保持
        p.focused = 9;
        press(
            &mut p,
            &mut st,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );
        press(
            &mut p,
            &mut st,
            KeyEvent::new(KeyCode::Char('9'), KeyModifiers::NONE),
        );
        assert!(p.dirty());
        assert!(p.editing);
        // 状态刷新：模拟主循环 RunStatusDone → refresh_state
        st.run_status = Some(RunStatus {
            service_unit: Some(true),
            service_active: Some(true),
            proc: Some(ProcStatus {
                bin: Some("/opt/mihomo".into()),
                pid: Some(777),
                running: true,
            }),
        });
        p.refresh_state(&st);
        assert_eq!(p.fields[1].value, "/opt/mihomo", "路径显示应刷新");
        assert_eq!(p.fields[2].value, "运行中（PID 777）", "状态显示应刷新");
        // 回归：值被覆盖后光标数组必须同步（否则渲染切片越界 panic，
        // 曾因占位值 27 字节被刷新为 15 字节路径而崩溃）
        assert_eq!(
            p.cursor[1],
            p.fields[1].value.len(),
            "cursor[1] 应与新值同步"
        );
        assert_eq!(
            p.cursor[2],
            p.fields[2].value.len(),
            "cursor[2] 应与新值同步"
        );
        // 渲染路径不再越界：模拟 render 的光标切片计算
        for i in 0..FIELD_COUNT {
            let cur = p.cursor[i].min(p.fields[i].value.len());
            let _ = p.fields[i].value[..cur].chars().count();
        }
        assert!(p.dirty(), "dirty 不应受影响");
        assert!(p.editing, "editing 不应受影响");
        assert_eq!(p.focused, 9, "focused 不应受影响");
        assert!(p.popup.is_none());
        // 未同步（fields 为空）时 refresh_state 安全无操作
        let mut p2 = SettingsPage::new();
        p2.refresh_state(&st);
        assert!(p2.fields.is_empty());
    }
}
