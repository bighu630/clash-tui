//! 规则管理页：自定义规则的新建、编辑、删除、上移/下移（顺序即优先级）。
//!
//! 契约见 docs/superpowers/plans/2026-08-10-mihomo-tui.md §2/§3。
//! 列表行：`DOMAIN, example.com, 🚀 节点选择`。

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::app::{AppState, UiCommand};
use crate::core::merger::{merge, MergeContext};
use crate::core::models::{UserRule, BUILTIN_TARGETS};
use crate::core::settings::save_overrides;
use crate::ui::widgets::{
    centered_rect, ConfirmPopup, FieldKind, FormAction, FormField, FormPopup, MessagePopup,
    SelectList,
};
use crate::ui::Page;

/// 规则类型下拉选项（MATCH 无 payload）
const RULE_TYPES: [&str; 10] = [
    "DOMAIN",
    "DOMAIN-SUFFIX",
    "DOMAIN-KEYWORD",
    "GEOSITE",
    "GEOIP",
    "IP-CIDR",
    "IP-CIDR6",
    "SRC-IP-CIDR",
    "PROCESS-NAME",
    "MATCH",
];

fn needs_no_resolve(rule_type: &str) -> bool {
    matches!(rule_type.trim(), "IP-CIDR" | "IP-CIDR6" | "SRC-IP-CIDR")
}
fn is_cidr_type(rule_type: &str) -> bool {
    matches!(rule_type.trim(), "IP-CIDR" | "IP-CIDR6" | "SRC-IP-CIDR")
}
fn is_valid_cidr(rule_type: &str, payload: &str) -> bool {
    let payload = payload.trim();
    let mut parts = payload.split('/');
    let ip_str = match parts.next() { Some(s) => s.trim(), None => return false };
    let prefix_str = match parts.next() { Some(s) => s.trim(), None => return false };
    if parts.next().is_some() { return false; }
    let ip: std::net::IpAddr = match ip_str.parse() { Ok(v) => v, Err(_) => return false };
    let prefix: u8 = match prefix_str.parse() { Ok(v) => v, Err(_) => return false };
    match rule_type {
        "IP-CIDR" => ip.is_ipv4() && prefix <= 32,
        "IP-CIDR6" => ip.is_ipv6() && prefix <= 128,
        "SRC-IP-CIDR" => if ip.is_ipv4() { prefix <= 32 } else { prefix <= 128 },
        _ => false,
    }
}

/// 规则串序列化：MATCH → "MATCH,target"；其余 → "TYPE,payload,target"
pub fn rule_to_string(r: &UserRule) -> String {
    let rt = r.rule_type.trim();
    if rt == "MATCH" {
        format!("MATCH,{}", r.target.trim())
    } else if needs_no_resolve(rt) {
        format!("{},{},{},no-resolve", rt, r.payload.trim(), r.target.trim())
    } else {
        format!("{},{},{}", rt, r.payload.trim(), r.target.trim())
    }
}

/// 规则串解析：逗号切分（splitn(3, ',') 兼容 payload 内含逗号）；MATCH 无 payload。
/// 解析失败（空段/字段缺失）返回 None。
pub fn parse_rule(s: &str) -> Option<UserRule> {
    let s = s.trim();
    if s.is_empty() { return None; }
    // 兼容 ",no-resolve" 前后空格、大小写：按逗号切分判断最后一段是否为 no-resolve
    let parts_tmp: Vec<&str> = s.split(',').collect();
    let raw = if parts_tmp.last().map(|last| last.trim().eq_ignore_ascii_case("no-resolve")).unwrap_or(false) {
        if let Some(idx) = s.rfind(',') {
            s[..idx].trim_end()
        } else {
            s
        }
    } else {
        s
    };
    let mut parts = raw.splitn(3, ',');
    let rule_type = parts.next()?.trim();
    if rule_type.is_empty() { return None; }
    if rule_type == "MATCH" {
        let target = parts.next()?.trim();
        if target.is_empty() { return None; }
        Some(UserRule { rule_type: rule_type.to_string(), payload: String::new(), target: target.to_string() })
    } else {
        let payload = parts.next()?.trim();
        let target = parts.next()?.trim();
        if payload.is_empty() || target.is_empty() { return None; }
        Some(UserRule { rule_type: rule_type.to_string(), payload: payload.to_string(), target: target.to_string() })
    }
}

/// 页面内弹窗状态机
enum RulePopup {
    /// 新建/编辑表单
    Form(FormPopup),
    /// 删除确认
    Confirm(ConfirmPopup),
    /// 错误/提示
    Message(MessagePopup),
}

pub struct RulesPage {
    list: SelectList,
    popup: Option<RulePopup>,
    /// 表单对应的规则索引（None=新建）
    pending: Option<usize>,
    /// 当前表单的规则类型（用于 MATCH 时隐藏 payload 字段的字段重建）
    form_type: String,
    /// 当前表单标题（字段重建时保留）
    form_title: String,
    /// 列表数据签名：内容变化时重建 SelectList
    sig: String,
    /// 有未应用（未合并+下发）的变动
    dirty: bool,
}

impl Default for RulesPage {
    fn default() -> Self {
        Self::new()
    }
}

impl RulesPage {
    pub fn new() -> Self {
        Self {
            list: SelectList::new(Vec::new()),
            popup: None,
            pending: None,
            form_type: RULE_TYPES[0].to_string(),
            form_title: String::new(),
            sig: String::new(),
            dirty: false,
        }
    }

    /// 列表行：`DOMAIN, example.com, 🚀 节点选择`
    fn row(r: &UserRule) -> String {
        if r.rule_type == "MATCH" {
            format!("MATCH, {}", r.target)
        } else {
            format!("{}, {}, {}", r.rule_type, r.payload, r.target)
        }
    }

    fn sig_of(st: &AppState) -> String {
        let items: Vec<String> = st.overrides.rules.iter().map(Self::row).collect();
        format!("{items:?}")
    }

    fn rebuild_list(st: &AppState) -> SelectList {
        SelectList::new(st.overrides.rules.iter().map(Self::row).collect())
    }

    /// 目标下拉选项：BUILTIN_TARGETS + 激活订阅组名（去重，保持顺序）。
    /// 激活订阅组名取 cache.proxy_groups 中每个 mapping 的 name 字段。
    fn target_options(st: &AppState) -> Vec<String> {
        let mut seen = std::collections::HashSet::new();
        let mut opts = Vec::new();
        for t in BUILTIN_TARGETS {
            if seen.insert(t.to_string()) {
                opts.push(t.to_string());
            }
        }
        if let Some(act) = st.subs.iter().find(|s| s.active) {
            if let Some(c) = &act.cache {
                for g in &c.proxy_groups {
                    if let Some(name) = g.get("name").and_then(|v| v.as_str()) {
                        if seen.insert(name.to_string()) {
                            opts.push(name.to_string());
                        }
                    }
                }
            }
        }
        opts
    }

    /// 新建/编辑共用表单（编辑时预填）。MATCH 无 payload 字段。
    /// 编辑时若目标不在下拉选项中 → 附加显示。
    fn rule_form(title: &str, rule: Option<&UserRule>, st: &AppState) -> FormPopup {
        let mut targets = Self::target_options(st);
        let (rule_type, payload, target) = match rule {
            Some(r) => (r.rule_type.clone(), r.payload.clone(), r.target.clone()),
            None => (
                RULE_TYPES[0].to_string(),
                String::new(),
                targets.first().cloned().unwrap_or_default(),
            ),
        };
        if !targets.contains(&target) {
            targets.push(target.clone());
        }
        let mut fields = vec![FormField {
            label: "类型".to_string(),
            value: rule_type.clone(),
            kind: FieldKind::Dropdown(RULE_TYPES.iter().map(|s| s.to_string()).collect()),
        }];
        if rule_type != "MATCH" {
            fields.push(FormField {
                label: "payload".to_string(),
                value: payload,
                kind: FieldKind::Text,
            });
        }
        fields.push(FormField {
            label: "目标".to_string(),
            value: target,
            kind: FieldKind::Dropdown(targets),
        });
        FormPopup::new(title.to_string(), fields)
    }

    /// n：新建表单
    fn start_new(&mut self, st: &mut AppState) -> Option<UiCommand> {
        self.pending = None;
        self.form_type = RULE_TYPES[0].to_string();
        self.form_title = "新建规则".to_string();
        self.popup = Some(RulePopup::Form(Self::rule_form("新建规则", None, st)));
        None
    }

    /// Enter：编辑表单（预填）
    fn start_edit(&mut self, st: &mut AppState) -> Option<UiCommand> {
        let idx = self.list.selected();
        if idx >= st.overrides.rules.len() {
            return None;
        }
        self.pending = Some(idx);
        self.form_type = st.overrides.rules[idx].rule_type.clone();
        self.form_title = "编辑规则".to_string();
        self.popup = Some(RulePopup::Form(Self::rule_form(
            "编辑规则",
            Some(&st.overrides.rules[idx]),
            st,
        )));
        None
    }

    /// 表单确认：校验 → 构建 UserRule → push/替换 + 落盘
    fn submit_form(&mut self, p: &mut FormPopup, st: &mut AppState) -> Option<UiCommand> {
        let v = p.values();
        let rule_type = v.first().map(|s| s.trim().to_string()).unwrap_or_default();
        // MATCH 时表单只有 [类型, 目标]；其余为 [类型, payload, 目标]
        let (payload, target) = if rule_type == "MATCH" {
            (
                String::new(),
                v.get(1).map(|s| s.trim().to_string()).unwrap_or_default(),
            )
        } else {
            (
                v.get(1).map(|s| s.trim().to_string()).unwrap_or_default(),
                v.get(2).map(|s| s.trim().to_string()).unwrap_or_default(),
            )
        };
        if rule_type.is_empty() {
            self.popup = Some(RulePopup::Message(MessagePopup::new(
                "输入有误".to_string(),
                vec!["规则类型不能为空".to_string()],
            )));
            return None;
        }
        if rule_type != "MATCH" && payload.is_empty() {
            self.popup = Some(RulePopup::Message(MessagePopup::new(
                "输入有误".to_string(),
                vec![format!("{rule_type} 规则需要 payload")],
            )));
            return None;
        }
        if is_cidr_type(&rule_type) && !is_valid_cidr(&rule_type, &payload) {
            self.popup = Some(RulePopup::Message(MessagePopup::new(
                "输入有误".to_string(),
                vec![format!("{rule_type} 的 CIDR 格式错误: {payload} (示例 192.168.0.0/16 或 2001:db8::/32)")],
            )));
            return None;
        }
        if target.is_empty() {
            self.popup = Some(RulePopup::Message(MessagePopup::new(
                "输入有误".to_string(),
                vec!["目标不能为空".to_string()],
            )));
            return None;
        }
        let rule = UserRule {
            rule_type,
            payload,
            target,
        };
        match self.pending {
            Some(idx) if idx < st.overrides.rules.len() => st.overrides.rules[idx] = rule,
            _ => st.overrides.rules.push(rule),
        }
        if let Err(e) = save_overrides(&st.overrides) {
            self.popup = Some(RulePopup::Message(MessagePopup::new(
                "保存失败".to_string(),
                vec![e.to_string()],
            )));
        } else {
            self.dirty = true;
        }
        self.pending = None;
        None
    }

    /// K/J：上移/下移（交换顺序 + 落盘）。
    /// SelectList 无选中设置 API：移动后立即重建列表并保持选中在移动后的规则上。
    fn move_rule(&mut self, st: &mut AppState, delta: isize) -> Option<UiCommand> {
        let len = st.overrides.rules.len();
        if len < 2 {
            return None;
        }
        let idx = self.list.selected();
        let new = idx as isize + delta;
        if new < 0 || new >= len as isize {
            return None;
        }
        let target = new as usize;
        st.overrides.rules.swap(idx, target);
        if let Err(e) = save_overrides(&st.overrides) {
            // 落盘失败回滚内存顺序，避免内存与磁盘不一致
            st.overrides.rules.swap(idx, target);
            self.popup = Some(RulePopup::Message(MessagePopup::new(
                "保存失败".to_string(),
                vec![e.to_string()],
            )));
            return None;
        }
        // 同步 sig 并重建，避免 render 再次重建导致选中复位；再用移动键把选中定位到目标
        self.sig = Self::sig_of(st);
        self.list = Self::rebuild_list(st);
        for _ in 0..target {
            self.list
                .handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        }
        self.dirty = true;
        None
    }

    /// d：删除确认 → 移除 + 落盘
    fn start_delete(&mut self, st: &mut AppState) -> Option<UiCommand> {
        let idx = self.list.selected();
        if idx >= st.overrides.rules.len() {
            return None;
        }
        self.pending = Some(idx);
        let name = Self::row(&st.overrides.rules[idx]);
        self.popup = Some(RulePopup::Confirm(ConfirmPopup::new(
            "删除规则".to_string(),
            format!("确定删除规则「{name}」？"),
        )));
        None
    }

    fn confirm_delete(&mut self, st: &mut AppState) -> Option<UiCommand> {
        if let Some(idx) = self.pending {
            if idx < st.overrides.rules.len() {
                st.overrides.rules.remove(idx);
                if let Err(e) = save_overrides(&st.overrides) {
                    self.popup = Some(RulePopup::Message(MessagePopup::new(
                        "保存失败".to_string(),
                        vec![e.to_string()],
                    )));
                } else {
                    self.dirty = true;
                }
            }
        }
        self.pending = None;
        None
    }

    /// Ctrl+A：将落盘后的规则合并进配置并应用。无变动时不触发，避免空刷。
    fn save_and_apply(&mut self, st: &mut AppState) -> Option<UiCommand> {
        if !self.dirty {
            return None;
        }
        let active = st.subs.iter().find(|s| s.active);
        match merge(MergeContext {
            settings: &st.settings,
            overrides: &st.overrides,
            subscription: active,
        }) {
            Err(e) => {
                let lines: Vec<String> = e.to_string().lines().map(String::from).collect();
                self.popup = Some(RulePopup::Message(MessagePopup::new(
                    "合并失败".into(),
                    lines,
                )));
                None
            }
            Ok(out) => {
                if !out.warnings.is_empty() {
                    st.notice(format!("[!] 合并警告: {}", out.warnings.join("；")));
                } else {
                    st.notice("[✓] 配置已合并，正在应用".to_string());
                }
                self.dirty = false;
                Some(UiCommand::ApplyConfig(out.config))
            }
        }
    }

    /// popup 打开期间：按键优先喂 popup，popup 关闭后才恢复页面按键。
    /// 类型切换为 MATCH 时隐藏 payload 字段（重建表单、保留已填值）。
    fn handle_popup(
        &mut self,
        popup: RulePopup,
        key: KeyEvent,
        st: &mut AppState,
    ) -> Option<UiCommand> {
        match popup {
            RulePopup::Form(mut p) => match p.handle_key(key) {
                Some(FormAction::Confirm) => self.submit_form(&mut p, st),
                Some(FormAction::Cancel) => {
                    self.pending = None;
                    None
                }
                None => {
                    let vals = p.values();
                    let current_type = vals.first().cloned().unwrap_or_default();
                    if current_type != self.form_type {
                        // 类型变化：按新类型重建字段（保留 payload/目标已填值）
                        let was_match = self.form_type == "MATCH";
                        self.form_type = current_type.clone();
                        // 从 MATCH 切回普通类型时 payload 无旧值可保留（MATCH 表单无 payload 字段）
                        let payload = if was_match {
                            String::new()
                        } else {
                            vals.get(1).cloned().unwrap_or_default()
                        };
                        let target = vals.last().cloned().unwrap_or_default();
                        let mut targets = Self::target_options(st);
                        if !targets.contains(&target) {
                            targets.push(target.clone());
                        }
                        let mut fields = vec![FormField {
                            label: "类型".to_string(),
                            value: current_type,
                            kind: FieldKind::Dropdown(
                                RULE_TYPES.iter().map(|s| s.to_string()).collect(),
                            ),
                        }];
                        if self.form_type != "MATCH" {
                            fields.push(FormField {
                                label: "payload".to_string(),
                                value: payload,
                                kind: FieldKind::Text,
                            });
                        }
                        fields.push(FormField {
                            label: "目标".to_string(),
                            value: target,
                            kind: FieldKind::Dropdown(targets),
                        });
                        let title = self.form_title.clone();
                        self.popup = Some(RulePopup::Form(FormPopup::new(title, fields)));
                    } else {
                        self.popup = Some(RulePopup::Form(p));
                    }
                    None
                }
            },
            RulePopup::Confirm(mut p) => match p.handle_key(key) {
                Some(true) => self.confirm_delete(st),
                Some(false) => {
                    self.pending = None;
                    None
                }
                None => {
                    self.popup = Some(RulePopup::Confirm(p));
                    None
                }
            },
            RulePopup::Message(mut p) => {
                if p.handle_key(key) {
                    None // 关闭
                } else {
                    self.popup = Some(RulePopup::Message(p));
                    None
                }
            }
        }
    }
}

impl Page for RulesPage {
    /// 页面内部弹窗打开时，全局键（Esc/Tab 等）交给页面处理。
    fn popup_open(&self) -> bool {
        self.popup.is_some()
    }

    fn handle_key(&mut self, key: KeyEvent, st: &mut AppState) -> Option<UiCommand> {
        if key.kind == KeyEventKind::Release {
            return None;
        }
        if let Some(popup) = self.popup.take() {
            return self.handle_popup(popup, key, st);
        }
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('a') | KeyCode::Char('A'))
        {
            return self.save_and_apply(st);
        }
        match key.code {
            KeyCode::Char('j') | KeyCode::Down | KeyCode::Char('k') | KeyCode::Up => {
                self.list.handle_key(key);
                None
            }
            KeyCode::Char('n') => self.start_new(st),
            KeyCode::Enter => self.start_edit(st),
            KeyCode::Char('d') => self.start_delete(st),
            KeyCode::Char('K') => self.move_rule(st, -1),
            KeyCode::Char('J') => self.move_rule(st, 1),
            _ => None,
        }
    }

    /// 全局配置应用成功后清除 dirty：任何页面的应用都已包含当前规则 overrides。
    fn on_apply_done(&mut self, _st: &AppState) {
        self.dirty = false;
    }

    fn render(&mut self, f: &mut Frame, area: Rect, st: &AppState) {
        let [body, status] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(area);
        let status_text = if self.dirty {
            "[未应用] Ctrl+A 保存并应用  ·  n 新建 · Enter 编辑 · K/J 移动 · d 删除"
        } else {
            "n 新建 · Enter 编辑 · K/J 移动 · d 删除"
        };
        let status_style = if self.dirty {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        f.render_widget(
            Paragraph::new(Span::styled(status_text, status_style)),
            status,
        );
        let sig = Self::sig_of(st);
        if sig != self.sig {
            self.sig = sig;
            self.list = Self::rebuild_list(st);
        }
        if st.overrides.rules.is_empty() {
            let hint = Paragraph::new(Line::from("无规则，按 n 新建（顺序即优先级）"))
                .block(Block::default().borders(Borders::ALL).title(" 规则 "));
            f.render_widget(hint, centered_rect(50, 30, body));
        } else {
            self.list.render(f, body);
        }
        // popup 置顶绘制
        if let Some(popup) = &mut self.popup {
            match popup {
                RulePopup::Form(p) => p.render(f, area),
                RulePopup::Confirm(p) => p.render(f, area),
                RulePopup::Message(p) => p.render(f, area),
            }
        }
    }
}

#[cfg(test)]
mod rules_tests {
    use super::*;
    use crate::core::models::UserRule;
    #[test]
    fn rule_to_string_ip_cidr_appends_no_resolve() {
        let r = UserRule { rule_type: "IP-CIDR".into(), payload: "192.168.0.0/16".into(), target: "DIRECT".into() };
        assert_eq!(rule_to_string(&r), "IP-CIDR,192.168.0.0/16,DIRECT,no-resolve");
    }
    #[test]
    fn rule_to_string_ip_cidr6_appends_no_resolve() {
        let r = UserRule { rule_type: "IP-CIDR6".into(), payload: "2001:db8::/32".into(), target: "DIRECT".into() };
        assert_eq!(rule_to_string(&r), "IP-CIDR6,2001:db8::/32,DIRECT,no-resolve");
    }
    #[test]
    fn rule_to_string_src_ip_cidr_appends_no_resolve() {
        let r = UserRule { rule_type: "SRC-IP-CIDR".into(), payload: "10.0.0.0/8".into(), target: "DIRECT".into() };
        assert_eq!(rule_to_string(&r), "SRC-IP-CIDR,10.0.0.0/8,DIRECT,no-resolve");
    }
    #[test]
    fn rule_to_string_geosite_no_no_resolve() {
        let r = UserRule { rule_type: "GEOSITE".into(), payload: "google".into(), target: "DIRECT".into() };
        assert_eq!(rule_to_string(&r), "GEOSITE,google,DIRECT");
    }
    #[test]
    fn parse_rule_strips_no_resolve() {
        let r = parse_rule("IP-CIDR,192.168.0.0/16,DIRECT,no-resolve").unwrap();
        assert_eq!(r.rule_type, "IP-CIDR");
        assert_eq!(r.payload, "192.168.0.0/16");
        assert_eq!(r.target, "DIRECT");
    }
    #[test]
    fn parse_rule_no_resolve_case_insensitive() {
        let r = parse_rule("IP-CIDR6,2001:db8::/32,DIRECT,NO-RESOLVE").unwrap();
        assert_eq!(r.rule_type, "IP-CIDR6");
        assert_eq!(r.payload, "2001:db8::/32");
    }
    #[test]
    fn cidr_validation() {
        assert!(is_valid_cidr("IP-CIDR", "192.168.0.0/16"));
        assert!(is_valid_cidr("IP-CIDR", "1.1.1.1/32"));
        assert!(!is_valid_cidr("IP-CIDR", "2001:db8::/32"));
        assert!(is_valid_cidr("IP-CIDR6", "2001:db8::/32"));
        assert!(is_valid_cidr("IP-CIDR6", "::1/128"));
        assert!(!is_valid_cidr("IP-CIDR6", "192.168.0.0/16"));
        assert!(is_valid_cidr("SRC-IP-CIDR", "10.0.0.0/8"));
        assert!(is_valid_cidr("SRC-IP-CIDR", "2001:db8::/32"));
        assert!(!is_valid_cidr("IP-CIDR", "192.168.0.0/33"));
        assert!(!is_valid_cidr("IP-CIDR", "999.0.0.0/16"));
        assert!(!is_valid_cidr("IP-CIDR", "192.168.0.0"));
        assert!(!is_valid_cidr("IP-CIDR", "192.168.0.0/"));
        assert!(!is_valid_cidr("IP-CIDR", "/16"));
        assert!(is_valid_cidr("IP-CIDR", " 192.168.0.0/16 "));
        assert!(!is_valid_cidr("IP-CIDR6", "2001:db8::/129"));
        assert!(!is_valid_cidr("SRC-IP-CIDR", "10.0.0.0/33"));
        assert!(!is_valid_cidr("SRC-IP-CIDR", "2001:db8::/129"));
        assert!(is_valid_cidr("IP-CIDR", "0.0.0.0/0"));
        assert!(is_valid_cidr("IP-CIDR6", "::/0"));
    }
    #[test]
    fn parse_rule_strips_no_resolve_with_space() {
        let r = parse_rule("IP-CIDR,192.168.0.0/16,DIRECT, no-resolve").unwrap();
        assert_eq!(r.target, "DIRECT");
    }
    #[test]
    fn parse_rule_strips_no_resolve_case_insensitive_with_space() {
        let r = parse_rule("IP-CIDR,192.168.0.0/16,DIRECT, No-Resolve ").unwrap();
        assert_eq!(r.rule_type, "IP-CIDR");
        assert_eq!(r.payload, "192.168.0.0/16");
        assert_eq!(r.target, "DIRECT");
    }
    #[test]
    fn needs_no_resolve_trim_consistency() {
        assert!(needs_no_resolve(" IP-CIDR "));
        assert!(needs_no_resolve(" IP-CIDR6 "));
        assert!(needs_no_resolve(" SRC-IP-CIDR "));
        assert!(!needs_no_resolve(" GEOSITE "));
    }
    #[test]
    fn is_cidr_type_trim_consistency() {
        assert!(is_cidr_type(" IP-CIDR "));
        assert!(is_cidr_type(" IP-CIDR6 "));
        assert!(!is_cidr_type(" DOMAIN "));
    }
    #[test]
    fn rule_to_string_trims_whitespace() {
        let r = UserRule { rule_type: " IP-CIDR ".into(), payload: " 192.168.0.0/16 ".into(), target: " DIRECT ".into() };
        assert_eq!(rule_to_string(&r), "IP-CIDR,192.168.0.0/16,DIRECT,no-resolve");
        let r2 = UserRule { rule_type: " MATCH ".into(), payload: "".into(), target: " DIRECT ".into() };
        assert_eq!(rule_to_string(&r2), "MATCH,DIRECT");
        let r3 = UserRule { rule_type: " GEOSITE ".into(), payload: " google ".into(), target: " DIRECT ".into() };
        assert_eq!(rule_to_string(&r3), "GEOSITE,google,DIRECT");
    }
}
