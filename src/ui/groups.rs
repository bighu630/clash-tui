//! 规则组页：只读展示运行时策略组（GET /proxies），select 组可切换节点，
//! 自动选择组（url-test/fallback 等）展示但禁选并提示；支持整组延迟测试。
//!
//! 数据源优先级：运行时策略组（含当前选择 now/成员 all）→ 激活订阅缓存组
//! （API 不可用时降级展示名称/类型/成员数，无当前选择）。

use std::collections::HashMap;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};
use ratatui::Frame;

use crate::app::{AppState, UiCommand};
use crate::core::client::{GroupInfo, GROUP_DELAY_TIMEOUT_MS};
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
    /// 弹窗对应的组名（弹窗期间列表可能因 GroupsRefreshed 重建，
    /// 按名定位避免索引漂移切错组）
    pending: Option<String>,
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
        self.pending = Some(g.name.clone());
        self.popup = Some(GroupPopup::Selector(SelectorPopup::new(
            format!("选择节点：{}", g.name),
            g.name.clone(),
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
        Some(UiCommand::TestGroupDelay {
            group: g.name.clone(),
            silent: false,
        })
    }

    /// 单选确认：按组名在运行时列表中定位后发切换命令；
    /// 弹窗期间列表已变化（组被移除）→ 提示重试。
    fn confirm_select(&mut self, target: String, st: &mut AppState) -> Option<UiCommand> {
        // 无 pending（理论上不会发生）时直接返回
        let name = self.pending.take()?;
        if st.proxy_groups.iter().any(|g| g.name == name) {
            return Some(UiCommand::SwitchGroup { group: name, target });
        }
        // popup 已被 take，需重新挂回提示
        self.popup = Some(GroupPopup::Message(MessagePopup::new(
            "组列表已变化".to_string(),
            vec!["组列表已变化，请重试。".to_string()],
        )));
        None
    }

    fn handle_popup(&mut self, popup: GroupPopup, key: KeyEvent, st: &mut AppState) -> Option<UiCommand> {
        match popup {
            GroupPopup::Selector(mut p) => match p.handle_key(key) {
                Some(SelectAction::Confirm(target)) => self.confirm_select(target, st),
                Some(SelectAction::Cancel) => {
                    self.pending = None;
                    None
                }
                Some(SelectAction::TestDelay) => {
                    // 弹窗内测速：静默模式（结果只进 AppState.group_delays，
                    // 由弹窗 render 时 sync_delays 读取展示/排序），弹窗保持打开
                    let cmd = self.pending.clone().map(|group| UiCommand::TestGroupDelay {
                        group,
                        silent: true,
                    });
                    self.popup = Some(GroupPopup::Selector(p));
                    cmd
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
                GroupPopup::Selector(p) => p.render(f, area, st),
                GroupPopup::Message(p) => p.render(f, area),
            }
        }
    }
}

/// 排序模式。
#[derive(Debug, Clone, Copy, PartialEq)]
enum SortMode {
    /// 订阅/接口原始顺序
    Original,
    /// 按名称字母序（to_lowercase 不区分大小写）
    Name,
    /// 按延迟升序（超时排最后，无数据排最后）
    Delay,
}

/// 单选弹窗动作。
enum SelectAction {
    Confirm(String),
    Cancel,
    TestDelay,
}

/// select 组节点单选弹窗：j/k 移动、Enter 确认、Esc 取消、当前项 ▶ 标记；
/// s 测速（结果行尾显示延迟）、n 按名称排序、d 按延迟排序。
struct SelectorPopup {
    title: String,
    /// 所属组名（用于从 AppState.group_delays 读延迟）
    group_name: String,
    items: Vec<String>,
    now: Option<String>,
    /// 显示顺序（items 的索引排列）
    order: Vec<usize>,
    /// 选中项在 order 中的位置
    selected: usize,
    mode: SortMode,
    /// 组内延迟（节点名 → ms，>= GROUP_DELAY_TIMEOUT_MS 为超时）
    delays: HashMap<String, u16>,
    /// 临时提示（如 d 无延迟数据时），显示在 footer 行，导航后清除
    hint: Option<String>,
}

impl SelectorPopup {
    fn new(title: String, group_name: String, items: Vec<String>, now: Option<String>) -> Self {
        let order: Vec<usize> = (0..items.len()).collect();
        // 初始定位到当前节点（now 缺失或不在列表中时回退第一项）
        let selected = now
            .as_ref()
            .and_then(|n| order.iter().position(|&i| items[i] == *n))
            .unwrap_or(0);
        Self {
            title,
            group_name,
            items,
            now,
            order,
            selected,
            mode: SortMode::Original,
            delays: HashMap::new(),
            hint: None,
        }
    }

    /// 按 mode 重建显示顺序：记录当前选中节点名，排序后重定位；
    /// Delay 模式无数据节点视为 u16::MAX 排最后，延迟相等时按名称序保证稳定可预期。
    fn reorder(&mut self) {
        let current = self.items[self.order[self.selected]].clone();
        let n = self.items.len();
        self.order = (0..n).collect();
        match self.mode {
            SortMode::Original => {}
            SortMode::Name => {
                self.order.sort_by(|&a, &b| {
                    self.items[a].to_lowercase().cmp(&self.items[b].to_lowercase())
                });
            }
            SortMode::Delay => {
                self.order.sort_by(|&a, &b| {
                    let da = self.delays.get(&self.items[a]).copied().unwrap_or(u16::MAX);
                    let db = self.delays.get(&self.items[b]).copied().unwrap_or(u16::MAX);
                    da.cmp(&db)
                        .then_with(|| self.items[a].to_lowercase().cmp(&self.items[b].to_lowercase()))
                });
            }
        }
        self.selected = self
            .order
            .iter()
            .position(|&i| self.items[i] == current)
            .unwrap_or(0);
    }

    /// 从 AppState.group_delays 同步本组延迟；延迟数据到达后清除「请先测速」提示，
    /// 且若处于 Delay 模式则重排（数据变化后保持有序）。
    fn sync_delays(&mut self, st: &AppState) {
        if let Some(list) = st.group_delays.get(&self.group_name) {
            let m: HashMap<String, u16> = list.iter().cloned().collect();
            if !m.is_empty() {
                self.delays = m;
                self.hint = None;
            }
        }
        if self.mode == SortMode::Delay && !self.delays.is_empty() {
            self.reorder();
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> Option<SelectAction> {
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                if self.selected + 1 < self.order.len() {
                    self.selected += 1;
                }
                self.hint = None;
                None
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.selected = self.selected.saturating_sub(1);
                self.hint = None;
                None
            }
            KeyCode::Enter => {
                Some(SelectAction::Confirm(self.items[self.order[self.selected]].clone()))
            }
            KeyCode::Esc => Some(SelectAction::Cancel),
            KeyCode::Char('s') => Some(SelectAction::TestDelay),
            KeyCode::Char('n') => {
                self.mode = SortMode::Name;
                self.reorder();
                self.hint = None;
                None
            }
            KeyCode::Char('d') => {
                if self.delays.is_empty() {
                    // 无延迟数据：提示先测速，保持原顺序
                    self.hint = Some("请先按 s 测速".to_string());
                    None
                } else {
                    self.mode = SortMode::Delay;
                    self.reorder();
                    self.hint = None;
                    None
                }
            }
            _ => None,
        }
    }

    fn render(&mut self, f: &mut Frame, area: Rect, st: &AppState) {
        self.sync_delays(st);
        // 关键修复 1：弹窗矩形先清底，避免未填充行透出下层主列表文字
        let rect = centered_rect(60, 60, area);
        f.render_widget(Clear, rect);
        // 关键修复 2：list 只渲染 chunks[0]，footer 独占最后一行，互不覆盖
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(1)])
            .split(rect);
        let items: Vec<ListItem> = self
            .order
            .iter()
            .map(|&i| {
                let n = &self.items[i];
                let mark = if self.now.as_deref() == Some(n.as_str()) { "▶ " } else { "  " };
                let suffix = match self.delays.get(n) {
                    Some(ms) if *ms >= GROUP_DELAY_TIMEOUT_MS => "  超时".to_string(),
                    Some(ms) => format!("  {ms}ms"),
                    None => String::new(),
                };
                ListItem::new(format!("{mark}{n}{suffix}"))
            })
            .collect();
        let list = List::new(items)
            .block(Block::default().borders(Borders::ALL).title(self.title.clone()))
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
        let mut state = ListState::default();
        state.select(Some(self.selected));
        f.render_stateful_widget(list, chunks[0], &mut state);
        let footer_text = match &self.hint {
            Some(h) => h.clone(),
            None => "j/k 移动  Enter 切换  s 测速  n 按名  d 按延迟  Esc 取消".to_string(),
        };
        let footer = Paragraph::new(Line::from(footer_text));
        f.render_widget(footer, chunks[1]);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, VecDeque};

    use super::*;
    use crate::core::client::RuntimeConfig;
    use crate::core::models::{NetworkSettings, Overrides};
    use crossterm::event::KeyModifiers;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    /// 构造含运行时组的 AppState（其余字段默认）。
    fn state_with_groups(groups: Vec<GroupInfo>) -> AppState {
        AppState {
            settings: NetworkSettings::default(),
            subs: Vec::new(),
            overrides: Overrides::default(),
            runtime: RuntimeConfig::default(),
            api_ok: false,
            api_confirmed: false,
            traffic: VecDeque::new(),
            mem_history: VecDeque::new(),
            connections: Vec::new(),
            exit_ip: None,
            proxy_groups: groups,
            group_delays: HashMap::new(),
            notices: VecDeque::new(),
        }
    }

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
            "g".into(),
            vec!["A".into(), "B".into(), "C".into()],
            Some("B".into()),
        );
        // 初始选中当前项（now 定位到 B），直接 Enter 保持当前选择
        assert!(matches!(
            p.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Some(SelectAction::Confirm(ref t)) if t == "B"
        ));
        // 移到 C 再确认
        let _ = p.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        assert!(matches!(
            p.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Some(SelectAction::Confirm(ref t)) if t == "C"
        ));
        // Esc 取消
        assert!(matches!(
            p.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            Some(SelectAction::Cancel)
        ));
        // now 不在列表中 → 回退第一项
        let mut p2 = SelectorPopup::new("t".into(), "g".into(), vec!["A".into(), "B".into()], Some("X".into()));
        assert!(matches!(
            p2.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Some(SelectAction::Confirm(ref t)) if t == "A"
        ));
        // 越界保护
        let mut p3 = SelectorPopup::new("t".into(), "g".into(), vec!["A".into()], None);
        let _ = p3.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        assert!(matches!(
            p3.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Some(SelectAction::Confirm(ref t)) if t == "A"
        ));
    }

    /// 极小终端回归：h=0/1/2 时页面区域为空/极小，整页 render 不 panic。
    #[test]
    fn groups_page_tiny_terminal_no_panic() {
        for h in [0u16, 1, 2, 24] {
            let state = state_with_groups(vec![GroupInfo {
                name: "g".into(),
                group_type: "Selector".into(),
                now: Some("a".into()),
                all: vec!["a".into(), "b".into()],
            }]);
            let mut page = GroupsPage::new();
            let mut terminal = Terminal::new(TestBackend::new(30, h)).unwrap();
            terminal
                .draw(|f| page.render(f, f.area(), &state))
                .expect("render 不应失败");
        }
    }

    /// SelectorPopup 极小终端渲染不 panic（渲染签名带 AppState）。
    #[test]
    fn selector_popup_tiny_terminal_no_panic() {
        for h in [0u16, 1, 2, 24] {
            let mut popup = SelectorPopup::new(
                "选择节点：g".into(),
                "g".into(),
                vec!["a".into(), "b".into()],
                Some("a".into()),
            );
            let state = state_with_groups(Vec::new());
            let mut terminal = Terminal::new(TestBackend::new(30, h)).unwrap();
            terminal
                .draw(|f| popup.render(f, f.area(), &state))
                .expect("render 不应失败");
        }
    }

    /// n 按名称排序（to_lowercase 不区分大小写）：b,a,C → a,b,C；
    /// 选中项跟随原节点（初始选中 b → 排序后仍在 b），确认返回正确节点名。
    #[test]
    fn selector_popup_name_sort_reorders() {
        let mut p = SelectorPopup::new(
            "t".into(),
            "g".into(),
            vec!["b".into(), "a".into(), "C".into()],
            None,
        );
        // 初始选中第一项 b，移到第二项 a
        let _ = p.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        let _ = p.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        let names: Vec<&str> = p.order.iter().map(|&i| p.items[i].as_str()).collect();
        assert_eq!(names, vec!["a", "b", "C"], "按名称升序: {names:?}");
        // selected 重定位到排序前的节点 a
        assert_eq!(p.selected, 0);
        assert!(matches!(
            p.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Some(SelectAction::Confirm(ref t)) if t == "a"
        ));
    }

    /// d 按延迟排序：升序、超时（>=5000）排后、无数据排最后、相等按名称序。
    #[test]
    fn selector_popup_delay_sort() {
        let mut p = SelectorPopup::new(
            "t".into(),
            "g".into(),
            vec!["c".into(), "a".into(), "b".into(), "x".into(), "d".into()],
            None,
        );
        p.delays.insert("c".into(), 100);
        p.delays.insert("a".into(), 300);
        p.delays.insert("d".into(), 300);
        p.delays.insert("b".into(), 8000); // 超时
        let _ = p.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE));
        let names: Vec<&str> = p.order.iter().map(|&i| p.items[i].as_str()).collect();
        assert_eq!(names, vec!["c", "a", "d", "b", "x"], "延迟升序: {names:?}");
        assert!(matches!(p.mode, SortMode::Delay));
    }

    /// d 无延迟数据：不排序（保持原始顺序），提示先测速，不切换模式。
    #[test]
    fn selector_popup_delay_sort_without_data_keeps_order() {
        let mut p = SelectorPopup::new(
            "t".into(),
            "g".into(),
            vec!["b".into(), "a".into()],
            None,
        );
        let _ = p.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE));
        let names: Vec<&str> = p.order.iter().map(|&i| p.items[i].as_str()).collect();
        assert_eq!(names, vec!["b", "a"], "无数据不应排序: {names:?}");
        assert!(matches!(p.mode, SortMode::Original));
        assert_eq!(p.hint.as_deref(), Some("请先按 s 测速"));
        // 导航后提示清除
        let _ = p.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        assert!(p.hint.is_none(), "导航后提示应清除");
    }

    /// sync_delays：从 AppState.group_delays 读入本组延迟；数据到达后 d 可排序。
    #[test]
    fn selector_popup_sync_delays_from_state() {
        let mut st = state_with_groups(Vec::new());
        st.group_delays.insert(
            "g".into(),
            vec![("a".into(), 300u16), ("b".into(), 100u16)],
        );
        let mut p = SelectorPopup::new(
            "t".into(),
            "g".into(),
            vec!["a".into(), "b".into()],
            None,
        );
        // 先按 d：无数据 → 提示
        let _ = p.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE));
        assert!(p.hint.is_some());
        // render 时 sync_delays 读到数据（模拟测速完成），提示清除
        p.sync_delays(&st);
        assert_eq!(p.delays.get("b"), Some(&100), "延迟应同步");
        assert!(p.hint.is_none(), "数据到达后提示应清除");
        // 再按 d 排序
        let _ = p.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE));
        let names: Vec<&str> = p.order.iter().map(|&i| p.items[i].as_str()).collect();
        assert_eq!(names, vec!["b", "a"]);
    }

    /// Enter：URLTest 自动组弹「不可手动切换」提示，不发切换命令。
    #[test]
    fn start_select_auto_group_blocks_with_message() {
        let mut page = GroupsPage::new();
        let mut st = state_with_groups(vec![GroupInfo {
            name: "自动组".into(),
            group_type: "URLTest".into(),
            now: Some("a".into()),
            all: vec!["a".into(), "b".into()],
        }]);
        let cmd = page.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &mut st);
        assert!(cmd.is_none(), "自动组不应发切换命令");
        assert!(page.popup_open(), "应弹出提示");
        // 弹窗内容为 Message 类型且渲染出「不可手动切换」标题
        let mut terminal = Terminal::new(TestBackend::new(60, 24)).unwrap();
        let frame = terminal
            .draw(|f| page.render(f, f.area(), &st))
            .expect("render 不应失败");
        let text: String = (0..frame.buffer.area.height)
            .flat_map(|y| (0..frame.buffer.area.width).map(move |x| {
                frame
                    .buffer
                    .cell((x, y))
                    .map(|c| c.symbol().to_string())
                    .unwrap_or_default()
            }))
            .collect::<String>()
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        assert!(text.contains("不可手动切换"), "应显示不可手动切换提示");
        assert!(matches!(page.popup, Some(GroupPopup::Message(_))));
    }

    /// Enter：Selector 组打开节点单选弹窗，不发切换命令。
    #[test]
    fn start_select_selector_opens_selector_popup() {
        let mut page = GroupsPage::new();
        let mut st = state_with_groups(vec![GroupInfo {
            name: "手动组".into(),
            group_type: "Selector".into(),
            now: Some("a".into()),
            all: vec!["a".into(), "b".into()],
        }]);
        let cmd = page.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &mut st);
        assert!(cmd.is_none(), "弹窗打开阶段不发命令");
        assert!(matches!(page.popup, Some(GroupPopup::Selector(_))), "应弹单选弹窗");
    }

    /// 确认切换：按组名定位成功 → 发 SwitchGroup 命令并关闭弹窗。
    #[test]
    fn confirm_select_sends_switch_group() {
        let mut page = GroupsPage::new();
        let mut st = state_with_groups(vec![GroupInfo {
            name: "手动组".into(),
            group_type: "Selector".into(),
            now: Some("a".into()),
            all: vec!["a".into(), "b".into()],
        }]);
        let _ = page.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &mut st);
        let cmd = page.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &mut st);
        match &cmd {
            Some(UiCommand::SwitchGroup { group, target }) => {
                assert_eq!(group.as_str(), "手动组");
                assert_eq!(target.as_str(), "a");
            }
            _ => panic!("应发切换命令: {cmd:?}"),
        }
        assert!(!page.popup_open(), "确认后弹窗应关闭");
        assert!(page.pending.is_none(), "pending 应已清空");
    }

    /// 弹窗期间列表变化（组被移除）：按组名找不到 → 提示重试，不发切换命令。
    #[test]
    fn confirm_select_stale_group_shows_retry_message() {
        let mut page = GroupsPage::new();
        let mut st = state_with_groups(vec![GroupInfo {
            name: "手动组".into(),
            group_type: "Selector".into(),
            now: Some("a".into()),
            all: vec!["a".into(), "b".into()],
        }]);
        let _ = page.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &mut st);
        // 弹窗期间 GroupsRefreshed 重建列表（组被移除）
        st.proxy_groups.clear();
        let cmd = page.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &mut st);
        assert!(cmd.is_none(), "列表变化后不应发切换命令");
        assert!(matches!(page.popup, Some(GroupPopup::Message(_))), "应提示重试");
        // Esc 关闭提示
        let _ = page.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &mut st);
        assert!(!page.popup_open(), "Esc 应关闭提示");
    }
}
