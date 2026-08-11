//! 规则组管理页：自定义规则组（select/url-test/fallback）的新建、编辑、成员勾选、删除。
//!
//! 契约见 docs/superpowers/plans/2026-08-10-mihomo-tui.md §2/§3。
//! 列表行：`名称 | 类型 | 成员N | url | interval`；无激活订阅时成员编辑不可用。

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
use ratatui::Frame;

use crate::app::{AppState, UiCommand};
use crate::core::models::{default_group_interval, default_test_url, UserGroup};
use crate::core::settings::save_overrides;
use crate::ui::widgets::{
    centered_rect, CheckAction, ConfirmPopup, FormAction, FormField, FieldKind, FormPopup,
    MessagePopup, SelectList,
};
use crate::ui::Page;

/// 页面内弹窗状态机
enum GroupPopup {
    /// 新建/编辑表单
    Form(FormPopup),
    /// 组成员多选
    Members(MemberPopup),
    /// 删除确认
    Confirm(ConfirmPopup),
    /// 错误/提示
    Message(MessagePopup),
}

pub struct GroupsPage {
    list: SelectList,
    popup: Option<GroupPopup>,
    /// 表单/删除/成员操作对应的组索引（None=新建）
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

    /// 列表行：`名称 | 类型 | 成员N | url | interval`
    fn row(g: &UserGroup) -> String {
        format!(
            "{} | {} | 成员{} | {} | {}s",
            g.name, g.group_type, g.proxies.len(), g.url, g.interval
        )
    }

    fn sig_of(st: &AppState) -> String {
        let items: Vec<(String, String, String, u64, u64, usize)> = st
            .overrides
            .groups
            .iter()
            .map(|g| {
                (
                    g.name.clone(),
                    g.group_type.clone(),
                    g.url.clone(),
                    g.interval,
                    g.tolerance,
                    g.proxies.len(),
                )
            })
            .collect();
        format!("{items:?}")
    }

    fn rebuild_list(st: &AppState) -> SelectList {
        SelectList::new(st.overrides.groups.iter().map(Self::row).collect())
    }

    /// 新建/编辑共用表单（编辑时预填；类型下拉 select|url-test|fallback）
    fn group_form(title: &str, g: Option<&UserGroup>) -> FormPopup {
        FormPopup::new(
            title.to_string(),
            vec![
                FormField {
                    label: "名称".to_string(),
                    value: g.map(|x| x.name.clone()).unwrap_or_default(),
                    kind: FieldKind::Text,
                },
                FormField {
                    label: "类型".to_string(),
                    value: g.map(|x| x.group_type.clone()).unwrap_or_else(|| "select".to_string()),
                    kind: FieldKind::Dropdown(vec![
                        "select".to_string(),
                        "url-test".to_string(),
                        "fallback".to_string(),
                    ]),
                },
                FormField {
                    label: "URL".to_string(),
                    value: g.map(|x| x.url.clone()).unwrap_or_else(default_test_url),
                    kind: FieldKind::Text,
                },
                FormField {
                    label: "interval".to_string(),
                    value: g
                        .map(|x| x.interval.to_string())
                        .unwrap_or_else(|| default_group_interval().to_string()),
                    kind: FieldKind::Number,
                },
                FormField {
                    label: "tolerance".to_string(),
                    value: g.map(|x| x.tolerance.to_string()).unwrap_or_else(|| "0".to_string()),
                    kind: FieldKind::Number,
                },
            ],
        )
    }

    /// 表单值解析与校验：名称非空、interval>0、数值合法
    fn parse_group_values(v: &[String]) -> Result<UserGroup, String> {
        if v.len() < 5 {
            return Err("表单字段缺失".to_string());
        }
        let name = v[0].trim();
        let group_type = v[1].trim();
        let url = v[2].trim();
        if name.is_empty() {
            return Err("名称不能为空".to_string());
        }
        if url.is_empty() {
            return Err("URL 不能为空".to_string());
        }
        let interval: u64 = v[3]
            .trim()
            .parse()
            .map_err(|_| format!("interval 必须是正整数（当前：{}）", v[3].trim()))?;
        if interval == 0 {
            return Err("interval 必须大于 0".to_string());
        }
        let tolerance: u64 = v[4]
            .trim()
            .parse()
            .map_err(|_| format!("tolerance 必须是数字（当前：{}）", v[4].trim()))?;
        Ok(UserGroup {
            name: name.to_string(),
            group_type: group_type.to_string(),
            url: url.to_string(),
            interval,
            tolerance,
            proxies: Vec::new(),
        })
    }

    /// n：新建表单
    fn start_new(&mut self) -> Option<UiCommand> {
        self.pending = None;
        self.popup = Some(GroupPopup::Form(Self::group_form("新建规则组", None)));
        None
    }

    /// Enter：编辑表单（预填，名称可改）
    fn start_edit(&mut self, st: &mut AppState) -> Option<UiCommand> {
        let idx = self.list.selected();
        if idx >= st.overrides.groups.len() {
            return None;
        }
        self.pending = Some(idx);
        self.popup =
            Some(GroupPopup::Form(Self::group_form("编辑规则组", Some(&st.overrides.groups[idx]))));
        None
    }

    /// 表单确认：校验 → push/替换 + 落盘
    fn submit_form(&mut self, p: &mut FormPopup, st: &mut AppState) -> Option<UiCommand> {
        let v = p.values();
        match Self::parse_group_values(&v) {
            Err(msg) => {
                self.popup = Some(GroupPopup::Message(MessagePopup::new("输入有误".to_string(), vec![msg])));
            }
            Ok(mut g) => {
                match self.pending {
                    Some(idx) if idx < st.overrides.groups.len() => {
                        // 编辑保留原成员
                        g.proxies = st.overrides.groups[idx].proxies.clone();
                        st.overrides.groups[idx] = g;
                    }
                    _ => {
                        if g.proxies.is_empty() {
                            st.notice(format!(
                                "[!] 规则组「{}」暂无成员，按 m 勾选节点后即可应用",
                                g.name
                            ));
                        }
                        st.overrides.groups.push(g);
                    }
                }
                if let Err(e) = save_overrides(&st.overrides) {
                    self.popup = Some(GroupPopup::Message(MessagePopup::new(
                        "保存失败".to_string(),
                        vec![e.to_string()],
                    )));
                }
            }
        }
        self.pending = None;
        None
    }

    /// m：成员多选（items=激活订阅缓存节点名，预勾选当前组成员；无激活订阅 → 提示）
    fn start_members(&mut self, st: &mut AppState) -> Option<UiCommand> {
        let idx = self.list.selected();
        if idx >= st.overrides.groups.len() {
            return None;
        }
        let Some(act) = st.subs.iter().find(|s| s.active) else {
            self.popup = Some(GroupPopup::Message(MessagePopup::new(
                "编辑成员".to_string(),
                vec!["没有激活的订阅，无法编辑组成员".to_string()],
            )));
            return None;
        };
        let Some(cache) = &act.cache else {
            self.popup = Some(GroupPopup::Message(MessagePopup::new(
                "编辑成员".to_string(),
                vec!["激活订阅尚未拉取成功，无法编辑组成员".to_string()],
            )));
            return None;
        };
        let items: Vec<String> = cache.proxies.iter().map(|p| p.name.clone()).collect();
        if items.is_empty() {
            self.popup = Some(GroupPopup::Message(MessagePopup::new(
                "编辑成员".to_string(),
                vec!["激活订阅中没有节点".to_string()],
            )));
            return None;
        }
        let current: Vec<String> = st.overrides.groups[idx].proxies.clone();
        self.pending = Some(idx);
        self.popup = Some(GroupPopup::Members(MemberPopup::new(
            format!("选择组员：{}", st.overrides.groups[idx].name),
            items,
            current,
        )));
        None
    }

    /// 成员确认：更新组.proxies + 落盘。checked 为空 → 阻止保存（不更新、不落盘）并提示
    fn apply_members(&mut self, checked: Vec<String>, st: &mut AppState) -> Option<UiCommand> {
        if let Some(idx) = self.pending {
            if idx < st.overrides.groups.len() {
                if checked.is_empty() {
                    self.pending = None;
                    self.popup = Some(GroupPopup::Message(MessagePopup::new(
                        "成员不能为空".to_string(),
                        vec![
                            "规则组至少需要一个成员，mihomo 校验会拒绝空组。请至少勾选一个节点。".to_string(),
                        ],
                    )));
                    return None;
                }
                st.overrides.groups[idx].proxies = checked;
                if let Err(e) = save_overrides(&st.overrides) {
                    self.popup = Some(GroupPopup::Message(MessagePopup::new(
                        "保存失败".to_string(),
                        vec![e.to_string()],
                    )));
                }
            }
        }
        self.pending = None;
        None
    }

    /// d：删除确认。组被规则引用时在确认里提示（仍可删除，合并校验会兜底报错）
    fn start_delete(&mut self, st: &mut AppState) -> Option<UiCommand> {
        let idx = self.list.selected();
        if idx >= st.overrides.groups.len() {
            return None;
        }
        let name = st.overrides.groups[idx].name.clone();
        let refs = st.overrides.rules.iter().filter(|r| r.target == name).count();
        let msg = if refs > 0 {
            format!(
                "规则组「{name}」被 {refs} 条规则引用。\n删除后这些规则的 target 将不存在，合并校验会报错（需同步修改规则）。仍要删除吗？"
            )
        } else {
            format!("确定删除规则组「{name}」？")
        };
        self.pending = Some(idx);
        self.popup = Some(GroupPopup::Confirm(ConfirmPopup::new("删除规则组".to_string(), msg)));
        None
    }

    fn confirm_delete(&mut self, st: &mut AppState) -> Option<UiCommand> {
        if let Some(idx) = self.pending {
            if idx < st.overrides.groups.len() {
                st.overrides.groups.remove(idx);
                if let Err(e) = save_overrides(&st.overrides) {
                    self.popup = Some(GroupPopup::Message(MessagePopup::new(
                        "保存失败".to_string(),
                        vec![e.to_string()],
                    )));
                }
            }
        }
        self.pending = None;
        None
    }

    /// popup 打开期间：按键优先喂 popup，popup 关闭后才恢复页面按键
    fn handle_popup(&mut self, popup: GroupPopup, key: KeyEvent, st: &mut AppState) -> Option<UiCommand> {
        match popup {
            GroupPopup::Form(mut p) => match p.handle_key(key) {
                Some(FormAction::Confirm) => self.submit_form(&mut p, st),
                Some(FormAction::Cancel) => {
                    self.pending = None;
                    None
                }
                None => {
                    self.popup = Some(GroupPopup::Form(p));
                    None
                }
            },
            GroupPopup::Members(mut p) => match p.handle_key(key) {
                Some(CheckAction::Confirm) => {
                    let checked = p.selected_items();
                    self.apply_members(checked, st)
                }
                Some(CheckAction::Cancel) => {
                    self.pending = None;
                    None
                }
                None => {
                    self.popup = Some(GroupPopup::Members(p));
                    None
                }
            },
            GroupPopup::Confirm(mut p) => match p.handle_key(key) {
                Some(true) => self.confirm_delete(st),
                Some(false) => {
                    self.pending = None;
                    None
                }
                None => {
                    self.popup = Some(GroupPopup::Confirm(p));
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
        match key.code {
            KeyCode::Char('j') | KeyCode::Down | KeyCode::Char('k') | KeyCode::Up => {
                self.list.handle_key(key);
                None
            }
            KeyCode::Char('n') => self.start_new(),
            KeyCode::Enter => self.start_edit(st),
            KeyCode::Char('m') => self.start_members(st),
            KeyCode::Char('d') => self.start_delete(st),
            _ => None,
        }
    }

    fn render(&mut self, f: &mut Frame, area: Rect, st: &AppState) {
        let sig = Self::sig_of(st);
        if sig != self.sig {
            self.sig = sig;
            self.list = Self::rebuild_list(st);
        }
        if st.overrides.groups.is_empty() {
            let hint = Paragraph::new(Line::from("无规则组，按 n 新建"))
                .block(Block::default().borders(Borders::ALL).title(" 规则组 "));
            f.render_widget(hint, centered_rect(50, 30, area));
        } else {
            self.list.render(f, area);
        }
        // popup 置顶绘制
        if let Some(popup) = &mut self.popup {
            match popup {
                GroupPopup::Form(p) => p.render(f, area),
                GroupPopup::Members(p) => p.render(f, area),
                GroupPopup::Confirm(p) => p.render(f, area),
                GroupPopup::Message(p) => p.render(f, area),
            }
        }
    }
}

/// 组成员多选弹窗。
///
/// 说明：B1 的 CheckboxList 契约（计划 §3）的 `new()` 全部默认未选中，无预勾选 API；
/// 为满足规格"预勾选当前组成员"，本页自实现同语义弹窗：
/// j/k/↑↓ 移动、Space 勾选、/ 或字母过滤（勾选状态保留）、Enter 确认、Esc 取消。
/// 若 B1 后续给 CheckboxList 增加 set_checked 之类 API，可替换回用。
struct MemberPopup {
    title: String,
    items: Vec<String>,
    checked: Vec<bool>,
    selected: usize,
    filter: String,
}

impl MemberPopup {
    fn new(title: String, items: Vec<String>, pre_checked: Vec<String>) -> Self {
        let checked = items
            .iter()
            .map(|n| pre_checked.iter().any(|c| c == n))
            .collect();
        Self {
            title,
            items,
            checked,
            selected: 0,
            filter: String::new(),
        }
    }

    /// 过滤匹配文本（开头的 '/' 仅作为过滤模式提示，不参与匹配）
    fn match_text(&self) -> &str {
        self.filter.trim_start_matches('/')
    }

    /// 当前可见项（过滤后）在 items 中的索引
    fn visible(&self) -> Vec<usize> {
        let needle = self.match_text().to_lowercase();
        self.items
            .iter()
            .enumerate()
            .filter(|(_, n)| needle.is_empty() || n.to_lowercase().contains(&needle))
            .map(|(i, _)| i)
            .collect()
    }

    fn selected_items(&self) -> Vec<String> {
        self.items
            .iter()
            .enumerate()
            .filter(|(i, _)| self.checked[*i])
            .map(|(_, n)| n.clone())
            .collect()
    }

    fn handle_key(&mut self, key: KeyEvent) -> Option<CheckAction> {
        let visible_len = self.visible().len();
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                if visible_len > 0 && self.selected + 1 < visible_len {
                    self.selected += 1;
                }
                None
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.selected = self.selected.saturating_sub(1);
                None
            }
            KeyCode::Char(' ') => {
                if let Some(idx) = self.visible().get(self.selected) {
                    self.checked[*idx] = !self.checked[*idx];
                }
                None
            }
            KeyCode::Char('/') => {
                self.filter.push('/');
                self.selected = 0;
                None
            }
            KeyCode::Char(c) if c.is_alphanumeric() => {
                self.filter.push(c);
                self.selected = 0;
                None
            }
            KeyCode::Backspace => {
                self.filter.pop();
                self.selected = 0;
                None
            }
            KeyCode::Enter => Some(CheckAction::Confirm),
            KeyCode::Esc => Some(CheckAction::Cancel),
            _ => None,
        }
    }

    fn render(&mut self, f: &mut Frame, area: Rect) {
        let rect = centered_rect(60, 70, area);
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(3), Constraint::Length(1)])
            .split(rect);
        let visible = self.visible();
        if visible.is_empty() {
            self.selected = 0;
        } else {
            // List 渲染时自动滚动到选中项可见（ratatui ListState.offset 由控件维护）
            self.selected = self.selected.min(visible.len() - 1);
        }
        let items: Vec<ListItem> = visible
            .iter()
            .map(|&i| {
                let mark = if self.checked[i] { "[x]" } else { "[ ]" };
                ListItem::new(format!("{mark} {}", self.items[i]))
            })
            .collect();
        let list = List::new(items)
            .block(Block::default().borders(Borders::ALL).title(self.title.clone()))
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
        let mut state = ListState::default();
        if !visible.is_empty() {
            state.select(Some(self.selected));
        }
        f.render_stateful_widget(list, chunks[0], &mut state);
        let filter_display = if self.filter.is_empty() {
            "（无）".to_string()
        } else {
            self.filter.clone()
        };
        let footer = Paragraph::new(Line::from(format!(
            "过滤: {filter_display}   j/k 移动  Space 勾选  /或字母 过滤  Enter 确定  Esc 取消"
        )));
        f.render_widget(footer, chunks[1]);
    }
}
