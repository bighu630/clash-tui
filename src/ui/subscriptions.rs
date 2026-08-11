//! 订阅管理页：列表展示订阅，支持添加（表单+拉取）、激活（合并+应用）、刷新、删除。
//!
//! 契约见 docs/superpowers/plans/2026-08-10-mihomo-tui.md §2/§3。
//! 列表行：`[★] 名称 | 节点N 组N 规则N | 上次拉取`；无订阅时提示按 a 添加。

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::app::{AppState, UiCommand};
use crate::core::merger::{merge, MergeContext};
use crate::core::models::Subscription;
use crate::core::settings::save_subscriptions;
use crate::ui::widgets::{
    centered_rect, ConfirmPopup, FormAction, FormField, FieldKind, FormPopup, MessagePopup,
    SelectList,
};
use crate::ui::Page;

/// 页面内弹窗状态机
enum SubPopup {
    /// 添加订阅表单
    Form(FormPopup),
    /// 删除确认
    Confirm(ConfirmPopup),
    /// 错误/提示
    Message(MessagePopup),
}

pub struct SubscriptionsPage {
    list: SelectList,
    popup: Option<SubPopup>,
    /// 待删除订阅索引（删除确认打开时记录）
    pending_idx: usize,
    /// 列表数据签名：内容变化时重建 SelectList
    sig: String,
}

impl Default for SubscriptionsPage {
    fn default() -> Self {
        Self::new()
    }
}

impl SubscriptionsPage {
    pub fn new() -> Self {
        Self {
            list: SelectList::new(Vec::new()),
            popup: None,
            pending_idx: 0,
            sig: String::new(),
        }
    }

    /// 列表行：`[★] 名称 | 节点N 组N 规则N | 上次拉取`
    fn row(s: &Subscription) -> String {
        let mark = if s.active { "★" } else { " " };
        let counts = match &s.cache {
            Some(c) => format!(
                "节点{} 组{} 规则{}",
                c.proxies.len(),
                c.proxy_groups.len(),
                c.rules.len()
            ),
            None => "节点- 组- 规则-".to_string(),
        };
        let fetched = s
            .last_fetch
            .clone()
            .or_else(|| s.cache.as_ref().map(|c| c.fetched_at.clone()))
            .unwrap_or_else(|| "未拉取".to_string());
        format!("[{mark}] {} | {counts} | {fetched}", s.name)
    }

    fn sig_of(st: &AppState) -> String {
        type Row = (String, bool, Option<String>, Option<(usize, usize, usize)>);
        let items: Vec<Row> = st
            .subs
            .iter()
            .map(|s| {
                let counts = s
                    .cache
                    .as_ref()
                    .map(|c| (c.proxies.len(), c.proxy_groups.len(), c.rules.len()));
                (s.name.clone(), s.active, s.last_fetch.clone(), counts)
            })
            .collect();
        format!("{items:?}")
    }

    fn rebuild_list(st: &AppState) -> SelectList {
        SelectList::new(st.subs.iter().map(Self::row).collect())
    }

    /// a：打开添加表单
    fn start_add(&mut self) -> Option<UiCommand> {
        self.popup = Some(SubPopup::Form(FormPopup::new(
            "添加订阅".to_string(),
            vec![
                FormField {
                    label: "名称".to_string(),
                    value: String::new(),
                    kind: FieldKind::Text,
                },
                FormField {
                    label: "URL".to_string(),
                    value: String::new(),
                    kind: FieldKind::Text,
                },
            ],
        )));
        None
    }

    /// 表单确认：校验非空 → 占位 Subscription 入列 + 落盘 → FetchSubscription(新索引)。
    /// 拉取完成事件（SubscriptionFetched）回来时由主循环更新 cache/last_fetch。
    fn submit_add(&mut self, p: &mut FormPopup, st: &mut AppState) -> Option<UiCommand> {
        let v = p.values();
        let name = v.first().map(|s| s.trim().to_string()).unwrap_or_default();
        let url = v.get(1).map(|s| s.trim().to_string()).unwrap_or_default();
        if name.is_empty() || url.is_empty() {
            self.popup = Some(SubPopup::Message(MessagePopup::new(
                "添加失败".to_string(),
                vec!["名称和 URL 都不能为空".to_string()],
            )));
            return None;
        }
        let idx = st.subs.len();
        st.subs.push(Subscription {
            name,
            url,
            last_fetch: None,
            active: false,
            cache: None, // 占位：拉取完成后更新
        });
        if let Err(e) = save_subscriptions(&st.subs) {
            st.subs.pop(); // 落盘失败回滚内存状态
            self.popup = Some(SubPopup::Message(MessagePopup::new(
                "保存失败".to_string(),
                vec![e.to_string()],
            )));
            return None;
        }
        Some(UiCommand::FetchSubscription(idx))
    }

    /// Enter：激活选中。先校验缓存存在（M3：无缓存不进入 merge/apply），
    /// 再 merge（M4：失败不动 active 标记），成功后才置 active 并落盘。
    /// Ok: notice(warnings)+ApplyConfig；Err: MessagePopup 全文
    fn activate_selected(&mut self, st: &mut AppState) -> Option<UiCommand> {
        let idx = self.list.selected();
        if idx >= st.subs.len() {
            return None;
        }
        // M3：无缓存订阅（未拉取过）直接拒绝激活，避免空配置静默替换旧配置
        if st.subs[idx].cache.is_none() {
            self.popup = Some(SubPopup::Message(MessagePopup::new(
                "无法激活".to_string(),
                vec![format!(
                    "订阅「{}」尚未拉取，请先按 r 刷新",
                    st.subs[idx].name
                )],
            )));
            return None;
        }
        // M4：先合并。失败时不改动 active 标记与落盘状态
        match merge(MergeContext {
            settings: &st.settings,
            overrides: &st.overrides,
            subscription: Some(&st.subs[idx]),
        }) {
            Ok(out) => {
                for (i, s) in st.subs.iter_mut().enumerate() {
                    s.active = i == idx;
                }
                if let Err(e) = save_subscriptions(&st.subs) {
                    self.popup = Some(SubPopup::Message(MessagePopup::new(
                        "保存失败".to_string(),
                        vec![e.to_string()],
                    )));
                    return None;
                }
                let msg = if out.warnings.is_empty() {
                    "[✓] 配置已合并，正在应用".to_string()
                } else {
                    format!(
                        "[✓] 合并完成（{} 条警告）：{}",
                        out.warnings.len(),
                        out.warnings.join("；")
                    )
                };
                st.notice(msg);
                Some(UiCommand::ApplyConfig(out.config))
            }
            Err(e) => {
                // 显示 MergeError 全文
                let lines: Vec<String> = e.to_string().lines().map(String::from).collect();
                self.popup = Some(SubPopup::Message(MessagePopup::new("合并失败".to_string(), lines)));
                None
            }
        }
    }

    /// d：删除确认 → 移除 + 落盘
    fn start_delete(&mut self, st: &mut AppState) -> Option<UiCommand> {
        let idx = self.list.selected();
        if idx >= st.subs.len() {
            return None;
        }
        self.pending_idx = idx;
        self.popup = Some(SubPopup::Confirm(ConfirmPopup::new(
            "删除订阅".to_string(),
            format!("确定删除订阅「{}」？", st.subs[idx].name),
        )));
        None
    }

    fn confirm_delete(&mut self, st: &mut AppState) -> Option<UiCommand> {
        let idx = self.pending_idx;
        if idx < st.subs.len() {
            st.subs.remove(idx);
            if let Err(e) = save_subscriptions(&st.subs) {
                self.popup = Some(SubPopup::Message(MessagePopup::new(
                    "保存失败".to_string(),
                    vec![e.to_string()],
                )));
            }
        }
        None
    }

    /// popup 打开期间：按键优先喂 popup，popup 关闭后才恢复页面按键
    fn handle_popup(
        &mut self,
        popup: SubPopup,
        key: KeyEvent,
        st: &mut AppState,
    ) -> Option<UiCommand> {
        match popup {
            SubPopup::Form(mut p) => match p.handle_key(key) {
                Some(FormAction::Confirm) => self.submit_add(&mut p, st),
                Some(FormAction::Cancel) => None,
                None => {
                    self.popup = Some(SubPopup::Form(p));
                    None
                }
            },
            SubPopup::Confirm(mut p) => match p.handle_key(key) {
                Some(true) => self.confirm_delete(st),
                Some(false) => None,
                None => {
                    self.popup = Some(SubPopup::Confirm(p));
                    None
                }
            },
            SubPopup::Message(mut p) => {
                if p.handle_key(key) {
                    None // 关闭
                } else {
                    self.popup = Some(SubPopup::Message(p));
                    None
                }
            }
        }
    }
}

impl Page for SubscriptionsPage {
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
            KeyCode::Char('a') => self.start_add(),
            KeyCode::Enter => self.activate_selected(st),
            KeyCode::Char('r') => {
                let idx = self.list.selected();
                if idx < st.subs.len() {
                    Some(UiCommand::FetchSubscription(idx))
                } else {
                    None
                }
            }
            KeyCode::Char('d') => self.start_delete(st),
            _ => None,
        }
    }

    fn render(&mut self, f: &mut Frame, area: Rect, st: &AppState) {
        let sig = Self::sig_of(st);
        if sig != self.sig {
            // 重建列表时按名称恢复选中（缓存更新等导致 sig 变化时避免选中跳回顶部）
            let prev_name = st
                .subs
                .get(self.list.selected())
                .map(|s| s.name.clone());
            self.sig = sig;
            self.list = Self::rebuild_list(st);
            if let Some(name) = prev_name {
                if let Some(target) = st.subs.iter().position(|s| s.name == name) {
                    for _ in 0..target {
                        self.list
                            .handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
                    }
                }
            }
        }
        if st.subs.is_empty() {
            let hint = Paragraph::new(Line::from("无订阅，按 a 添加"))
                .block(Block::default().borders(Borders::ALL).title(" 订阅 "));
            f.render_widget(hint, centered_rect(50, 30, area));
        } else {
            self.list.render(f, area);
        }
        // popup 置顶绘制
        if let Some(popup) = &mut self.popup {
            match popup {
                SubPopup::Form(p) => p.render(f, area),
                SubPopup::Confirm(p) => p.render(f, area),
                SubPopup::Message(p) => p.render(f, area),
            }
        }
    }
}
