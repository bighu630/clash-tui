//! 规则组页：只读展示运行时策略组（GET /proxies），select 组可切换节点，
//! 自动选择组（url-test/fallback 等）展示但禁选并提示；支持整组延迟测试。
//!
//! 数据源优先级：运行时策略组（含当前选择 now/成员 all）→ 激活订阅缓存组
//! （API 不可用时降级展示名称/类型/成员数，无当前选择）。

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
use ratatui::Frame;

use crate::app::{AppState, UiCommand};
use crate::core::client::GroupInfo;
use crate::ui::widgets::{centered_rect, MessagePopup, SelectList};
use crate::ui::Page;

/// 页面内弹窗状态机
enum GroupPopup {
    /// select 组节点单选
    Selector(SelectorPopup),
    /// 错误/提示
    Message(MessagePopup),
}

pub struct GroupsPage {
    list: SelectList,
    popup: Option<GroupPopup>,
    /// 弹窗对应的组索引
    pending: Option<usize>,
    /// 列表数据签名：内容变化时重建 SelectList
    sig: String,
}

impl Default for GroupsPage {
    fn default() -> Self {
        Self::new()
    }
}

impl GroupsPage {
    pub fn new() -> Self {
        Self {
            list: SelectList::new(Vec::new()),
            popup: None,
            pending: None,
            sig: String::new(),
        }
    }

    /// 运行时组类型 → 展示用 kebab 形式（与订阅 YAML 的 type 一致）。
    fn type_display(t: &str) -> String {
        match t {
            "Selector" => "select".to_string(),
            "URLTest" => "url-test".to_string(),
            "Fallback" => "fallback".to_string(),
            "LoadBalance" => "load-balance".to_string(),
            "Relay" => "relay".to_string(),
            "Compatible" => "compatible".to_string(),
            "Pass" => "pass".to_string(),
            other => other.to_string(),
        }
    }

    /// 是否可手动切换：仅 Selector（select）组。
    fn is_switchable(t: &str) -> bool {
        t == "Selector"
    }

    /// 运行时行：`名称 | select | 当前: 节点X`
    fn row(g: &GroupInfo) -> String {
        let now = g.now.as_deref().unwrap_or("-");
        format!("{} | {} | 当前: {}", g.name, Self::type_display(&g.group_type), now)
    }

    /// 降级行（订阅缓存）：`名称 | select | 成员5 | 当前: -`
    fn fallback_row(name: &str, group_type: &str, members: usize) -> String {
        format!("{name} | {group_type} | 成员{members} | 当前: -")
    }

    /// 订阅缓存组签名：激活订阅 proxy_groups 的 name/type/成员数。
    fn fallback_sig(st: &AppState) -> String {
        let mut items: Vec<(String, String, usize)> = Vec::new();
        if let Some(act) = st.subs.iter().find(|s| s.active) {
            if let Some(cache) = &act.cache {
                for g in &cache.proxy_groups {
                    let m = match g.as_mapping() {
                        Some(m) => m,
                        None => continue,
                    };
                    let name = m
                        .get(serde_yaml::Value::String("name".into()))
                        .and_then(|v| v.as_str())
                        .unwrap_or("?")
                        .to_string();
                    let t = m
                        .get(serde_yaml::Value::String("type".into()))
                        .and_then(|v| v.as_str())
                        .unwrap_or("?")
                        .to_string();
                    let n = m
                        .get(serde_yaml::Value::String("proxies".into()))
                        .and_then(|v| v.as_sequence())
                        .map(|s| s.len())
                        .unwrap_or(0);
                    items.push((name, t, n));
                }
            }
        }
        format!("{items:?}")
    }

    fn sig_of(st: &AppState) -> String {
        format!("{:?}|{}", st.proxy_groups, Self::fallback_sig(st))
    }

    fn rebuild_list(st: &AppState) -> SelectList {
        if !st.proxy_groups.is_empty() {
            let rows: Vec<String> = st.proxy_groups.iter().map(Self::row).collect();
            SelectList::new(rows).with_title(" 规则组（运行时，Enter 切换 / r 测速 / R 刷新） ".to_string())
        } else {
            let mut rows: Vec<String> = Vec::new();
            if let Some(act) = st.subs.iter().find(|s| s.active) {
                if let Some(cache) = &act.cache {
                    for g in &cache.proxy_groups {
                        let m = match g.as_mapping() {
                            Some(m) => m,
                            None => continue,
                        };
                        let name = m
                            .get(serde_yaml::Value::String("name".into()))
                            .and_then(|v| v.as_str())
                            .unwrap_or("?")
                            .to_string();
                        let t = m
                            .get(serde_yaml::Value::String("type".into()))
                            .and_then(|v| v.as_str())
                            .unwrap_or("?")
                            .to_string();
                        let n = m
                            .get(serde_yaml::Value::String("proxies".into()))
                            .and_then(|v| v.as_sequence())
                            .map(|s| s.len())
                            .unwrap_or(0);
                        rows.push(Self::fallback_row(&name, &t, n));
                    }
                }
            }
            SelectList::new(rows).with_title(" 规则组（API 不可用，展示订阅缓存） ".to_string())
        }
    }

    /// 当前选中组：运行时优先。
    fn current_group<'a>(&self, st: &'a AppState) -> Option<&'a GroupInfo> {
        let idx = self.list.selected();
        st.proxy_groups.get(idx)
    }

    /// Enter：select 组 → 单选弹窗；自动组/降级 → 提示。
    fn start_select(&mut self, st: &mut AppState) -> Option<UiCommand> {
        let Some(g) = self.current_group(st) else {
            self.popup = Some(GroupPopup::Message(MessagePopup::new(
                "无法切换".to_string(),
                vec![
                    "运行时 API 不可用（mihomo 未运行或未连接），无法获取/切换节点。".to_string(),
                    "请确认 mihomo 服务已启动，或按 R 刷新。".to_string(),
                ],
            )));
            return None;
        };
        if !Self::is_switchable(&g.group_type) {
            self.popup = Some(GroupPopup::Message(MessagePopup::new(
                "不可手动切换".to_string(),
                vec![format!(
                    "「{}」是 {} 自动选择组，节点由 mihomo 自动测速/健康检查决定，不可手动切换。",
                    g.name,
                    Self::type_display(&g.group_type)
                )],
            )));
            return None;
        }
        if g.all.is_empty() {
            self.popup = Some(GroupPopup::Message(MessagePopup::new(
                "没有可选节点".to_string(),
                vec![format!("「{}」没有可切换的成员。", g.name)],
            )));
            return None;
        }
        let idx = self.list.selected();
        self.pending = Some(idx);
        self.popup = Some(GroupPopup::Selector(SelectorPopup::new(
            format!("选择节点：{}", g.name),
            g.all.clone(),
            g.now.clone(),
        )));
        None
    }

    /// r：整组延迟测试（需要选中组；降级模式提示）。
    fn start_delay_test(&mut self, st: &AppState) -> Option<UiCommand> {
        let Some(g) = self.current_group(st) else {
            self.popup = Some(GroupPopup::Message(MessagePopup::new(
                "无法测速".to_string(),
                vec!["运行时 API 不可用，无法执行延迟测试。".to_string()],
            )));
            return None;
        };
        Some(UiCommand::TestGroupDelay(g.name.clone()))
    }

    /// 单选确认：发切换命令。
    fn confirm_select(&mut self, target: String, st: &mut AppState) -> Option<UiCommand> {
        let group = self
            .pending
            .and_then(|idx| st.proxy_groups.get(idx))
            .map(|g| g.name.clone());
        self.pending = None;
        match group {
            Some(name) => Some(UiCommand::SwitchGroup { group: name, target }),
            None => None,
        }
    }

    fn handle_popup(&mut self, popup: GroupPopup, key: KeyEvent, st: &mut AppState) -> Option<UiCommand> {
        match popup {
            GroupPopup::Selector(mut p) => match p.handle_key(key) {
                Some(SelectAction::Confirm(target)) => self.confirm_select(target, st),
                Some(SelectAction::Cancel) => {
                    self.pending = None;
                    None
                }
                None => {
                    self.popup = Some(GroupPopup::Selector(p));
                    None
                }
            },
            GroupPopup::Message(mut p) => {
                if p.handle_key(key) {
                    None // 关闭
                } else {
                    self.popup = Some(GroupPopup::Message(p));
                    None
                }
            }
        }
    }
}

impl Page for GroupsPage {
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
        match key.code {
            KeyCode::Char('j') | KeyCode::Down | KeyCode::Char('k') | KeyCode::Up => {
                self.list.handle_key(key);
                None
            }
            KeyCode::Enter => self.start_select(st),
            KeyCode::Char('r') => self.start_delay_test(st),
            KeyCode::Char('R') => Some(UiCommand::RefreshGroups),
            _ => None,
        }
    }

    fn render(&mut self, f: &mut Frame, area: Rect, st: &AppState) {
        let sig = Self::sig_of(st);
        if sig != self.sig {
            self.sig = sig;
            self.list = Self::rebuild_list(st);
        }
        if self.list.is_empty() {
            let hint = Paragraph::new(Line::from(
                "无可用规则组（无激活订阅或 mihomo 未运行），按 R 刷新",
            ))
            .block(Block::default().borders(Borders::ALL).title(" 规则组 "));
            f.render_widget(hint, centered_rect(60, 30, area));
        } else {
            self.list.render(f, area);
        }
        if let Some(popup) = &mut self.popup {
            match popup {
                GroupPopup::Selector(p) => p.render(f, area),
                GroupPopup::Message(p) => p.render(f, area),
            }
        }
    }
}

/// 单选弹窗动作。
enum SelectAction {
    Confirm(String),
    Cancel,
}

/// select 组节点单选弹窗：j/k 移动、Enter 确认、Esc 取消、当前项 ▶ 标记。
struct SelectorPopup {
    title: String,
    items: Vec<String>,
    now: Option<String>,
    selected: usize,
}

impl SelectorPopup {
    fn new(title: String, items: Vec<String>, now: Option<String>) -> Self {
        Self {
            title,
            items,
            now,
            selected: 0,
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> Option<SelectAction> {
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                if self.selected + 1 < self.items.len() {
                    self.selected += 1;
                }
                None
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.selected = self.selected.saturating_sub(1);
                None
            }
            KeyCode::Enter => Some(SelectAction::Confirm(self.items[self.selected].clone())),
            KeyCode::Esc => Some(SelectAction::Cancel),
            _ => None,
        }
    }

    fn render(&mut self, f: &mut Frame, area: Rect) {
        let rect = centered_rect(60, 60, area);
        let items: Vec<ListItem> = self
            .items
            .iter()
            .map(|n| {
                let mark = if self.now.as_deref() == Some(n.as_str()) { "▶ " } else { "  " };
                ListItem::new(format!("{mark}{n}"))
            })
            .collect();
        let list = List::new(items)
            .block(Block::default().borders(Borders::ALL).title(self.title.clone()))
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
        let mut state = ListState::default();
        state.select(Some(self.selected));
        f.render_stateful_widget(list, rect, &mut state);
        let footer = Paragraph::new(Line::from("j/k 移动  Enter 切换  Esc 取消"));
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(1)])
            .split(rect);
        f.render_widget(footer, chunks[1]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    #[test]
    fn type_display_mapping() {
        assert_eq!(GroupsPage::type_display("Selector"), "select");
        assert_eq!(GroupsPage::type_display("URLTest"), "url-test");
        assert_eq!(GroupsPage::type_display("Fallback"), "fallback");
        assert_eq!(GroupsPage::type_display("LoadBalance"), "load-balance");
        assert_eq!(GroupsPage::type_display("Relay"), "relay");
        assert_eq!(GroupsPage::type_display("Compatible"), "compatible");
        assert_eq!(GroupsPage::type_display("Pass"), "pass");
        assert_eq!(GroupsPage::type_display("未知类型"), "未知类型");
    }

    #[test]
    fn is_switchable_only_selector() {
        assert!(GroupsPage::is_switchable("Selector"));
        assert!(!GroupsPage::is_switchable("URLTest"));
        assert!(!GroupsPage::is_switchable("Fallback"));
        assert!(!GroupsPage::is_switchable("LoadBalance"));
        assert!(!GroupsPage::is_switchable("Relay"));
    }

    #[test]
    fn row_format() {
        let g = GroupInfo {
            name: "手动选择".into(),
            group_type: "Selector".into(),
            now: Some("节点A".into()),
            all: vec!["节点A".into()],
        };
        assert_eq!(GroupsPage::row(&g), "手动选择 | select | 当前: 节点A");
        let g2 = GroupInfo {
            name: "自动".into(),
            group_type: "URLTest".into(),
            now: None,
            all: vec![],
        };
        assert_eq!(GroupsPage::row(&g2), "自动 | url-test | 当前: -");
    }

    #[test]
    fn fallback_row_format() {
        assert_eq!(GroupsPage::fallback_row("订阅组", "select", 3), "订阅组 | select | 成员3 | 当前: -");
    }

    #[test]
    fn selector_popup_navigation_and_confirm() {
        let mut p = SelectorPopup::new(
            "选择节点：g".into(),
            vec!["A".into(), "B".into(), "C".into()],
            Some("B".into()),
        );
        // 初始选中第一项
        assert!(matches!(
            p.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Some(SelectAction::Confirm(ref t)) if t == "A"
        ));
        // 移到 B（当前项）再确认
        let _ = p.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        assert!(matches!(
            p.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Some(SelectAction::Confirm(ref t)) if t == "B"
        ));
        // Esc 取消
        assert!(matches!(
            p.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            Some(SelectAction::Cancel)
        ));
        // 越界保护
        let mut p2 = SelectorPopup::new("t".into(), vec!["A".into()], None);
        let _ = p2.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        assert!(matches!(
            p2.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Some(SelectAction::Confirm(ref t)) if t == "A"
        ));
    }
}
