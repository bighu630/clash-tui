//! 设置页：config.yaml 可配置项集中编辑（TUN/DNS/网络/端口/日志/其他）。
//! 交互规格见 docs/superpowers/specs/2026-08-12-settings-tab-design.md。
//! 本文件包含：纯函数（模型↔表单转换与校验）+ SettingsPage 整页表单。

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::app::{AppState, UiCommand};
use crate::core::merger::{merge, MergeContext};
use crate::core::models::{DnsSettings, NetworkSettings, TunSettings};
use crate::core::settings::{generate_secret, save_settings};
use crate::ui::widgets::{FieldKind, FormField, MessagePopup};
use crate::ui::Page;

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
    /// （Page::on_enter 默认方法在 Task 4 加入 trait，此前为固有方法。）
    pub fn on_enter(&mut self, st: &AppState) {
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

    // ---- SettingsPage 整页表单 ----

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use crate::app::{AppState, UiCommand};
    use crate::core::client::RuntimeConfig;
    use crate::core::settings::{load_settings, save_settings, with_settings_dir};
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
            // 编辑模式为追加：先清空原值（7890）再输入新端口
            for _ in 0..4 {
                press(&mut p, &mut st, KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
            }
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
            for _ in 0..4 {
                press(&mut p, &mut st, KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
            }
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

    /// 文本字段编辑模式：Enter 进入、End 光标定位末尾、输入、Esc 退出。
    #[test]
    fn text_field_edit_mode() {
        let mut st = test_state();
        let mut p = page_with_state(&st);
        // dns.listen（index 13）进入编辑后追加端口
        p.focused = 13;
        press(&mut p, &mut st, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(p.editing, "Enter 应进入编辑模式");
        press(&mut p, &mut st, KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
        press(&mut p, &mut st, KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE));
        press(&mut p, &mut st, KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE));
        press(&mut p, &mut st, KeyEvent::new(KeyCode::Char('5'), KeyModifiers::NONE));
        press(&mut p, &mut st, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(p.fields[13].value, "0.0.0.0:1053:15");
        assert!(!p.editing, "Esc 退出编辑模式");
    }
}
