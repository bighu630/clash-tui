//! 通用 UI 组件：弹窗（表单/多选/确认/消息）、选择列表、按键提示、布局与格式化工具。
//! API 按 docs/superpowers/plans/2026-08-10-mihomo-tui.md §3 UI 契约实现。

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// 表单弹窗的关闭方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormAction {
    Confirm,
    Cancel,
}

/// 表单字段类型。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldKind {
    /// 自由文本（支持字符输入/退格/Delete/←→/Home/End）
    Text,
    /// 下拉选项，←/→ 循环切换
    Dropdown(Vec<String>),
    /// 数字（仅允许 0-9 输入）
    Number,
    /// 只读展示（如 secret）：不响应任何编辑按键
    ReadOnly,
    /// 动作按钮（如启动/停止/重启）：Enter 触发页面定义的动作
    Action,
}

/// 表单字段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormField {
    pub label: String,
    pub value: String,
    pub kind: FieldKind,
}

/// 居中表单弹窗：Tab/↑↓ 切换字段，Enter 确认，Esc 取消。
pub struct FormPopup {
    title: String,
    fields: Vec<FormField>,
    focused: usize,
    /// 每个字段的编辑光标（字节位置，恒在字符边界上）
    cursor: Vec<usize>,
}

impl FormPopup {
    pub fn new(title: String, fields: Vec<FormField>) -> Self {
        let cursor = fields.iter().map(|f| f.value.len()).collect();
        Self {
            title,
            fields,
            focused: 0,
            cursor,
        }
    }

    /// 处理按键。返回 `Some(Confirm/Cancel)` 表示弹窗关闭。
    pub fn handle_key(&mut self, key: KeyEvent) -> Option<FormAction> {
        let is_dropdown = matches!(self.fields[self.focused].kind, FieldKind::Dropdown(_));
        let is_readonly = matches!(self.fields[self.focused].kind, FieldKind::ReadOnly);
        match key.code {
            KeyCode::Esc => return Some(FormAction::Cancel),
            KeyCode::Enter => return Some(FormAction::Confirm),
            KeyCode::Tab | KeyCode::Down => {
                self.focused = (self.focused + 1) % self.fields.len();
            }
            KeyCode::BackTab | KeyCode::Up => {
                self.focused = (self.focused + self.fields.len() - 1) % self.fields.len();
            }
            KeyCode::Left => {
                if is_dropdown {
                    self.cycle_dropdown(-1);
                } else if !is_readonly {
                    self.move_cursor(-1);
                }
            }
            KeyCode::Right => {
                if is_dropdown {
                    self.cycle_dropdown(1);
                } else if !is_readonly {
                    self.move_cursor(1);
                }
            }
            KeyCode::Home => {
                if !is_dropdown && !is_readonly {
                    self.cursor[self.focused] = 0;
                }
            }
            KeyCode::End => {
                if !is_dropdown && !is_readonly {
                    self.cursor[self.focused] = self.fields[self.focused].value.len();
                }
            }
            KeyCode::Backspace => {
                if !is_dropdown && !is_readonly {
                    self.backspace();
                }
            }
            KeyCode::Delete => {
                if !is_dropdown && !is_readonly {
                    self.delete_at_cursor();
                }
            }
            KeyCode::Char(c) => match &self.fields[self.focused].kind {
                FieldKind::Dropdown(_) => {}
                FieldKind::Number => {
                    if c.is_ascii_digit() {
                        self.insert_char(c);
                    }
                }
                FieldKind::Text => self.insert_char(c),
                FieldKind::ReadOnly => {}
                FieldKind::Action => {}
            },
            _ => {}
        }
        None
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
        let f = self.focused;
        if let FieldKind::Dropdown(options) = &self.fields[f].kind {
            if options.is_empty() {
                return;
            }
            let idx = options
                .iter()
                .position(|o| o == &self.fields[f].value)
                .unwrap_or(0);
            let len = options.len() as i32;
            let next = (idx as i32 + dir).rem_euclid(len) as usize;
            self.fields[f].value = options[next].clone();
        }
    }

    /// 与 fields 顺序一致的当前值。
    pub fn values(&self) -> Vec<String> {
        self.fields.iter().map(|f| f.value.clone()).collect()
    }

    /// 读取字段当前值（测试与确认流程用）。
    pub fn value(&self, idx: usize) -> &str {
        self.fields.get(idx).map(|f| f.value.as_str()).unwrap_or("")
    }

    pub fn render(&mut self, f: &mut Frame, area: Rect) {
        let popup = centered_rect(70, 80, area);
        f.render_widget(Clear, popup);
        let block = Block::new()
            .title(Span::styled(
                format!(" {} ", self.title),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan));
        let inner = block.inner(popup);
        f.render_widget(block, popup);

        let total = self.fields.len();
        let rows = inner.height.saturating_sub(2).max(1) as usize;
        let start = self.focused.saturating_sub(rows / 2);
        let end = (start + rows).min(total).max(start);
        let start = end.saturating_sub(rows).min(start);
        let label_w: u16 = 20;
        let vx = inner.x + 1 + label_w + 2;
        let vw = inner.width.saturating_sub(label_w + 4).max(1);

        for (i, idx) in (start..end).enumerate() {
            let y = inner.y + 1 + i as u16;
            let focused = idx == self.focused;
            let label = if (self.fields[idx].label.len() as u16) > label_w {
                self.fields[idx]
                    .label
                    .chars()
                    .take(label_w as usize)
                    .collect::<String>()
            } else {
                self.fields[idx].label.clone()
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
                Rect::new(inner.x + 1, y, label_w + 2, 1),
            );

            let value_style = if focused {
                Style::default().fg(Color::Black).bg(Color::Cyan)
            } else {
                Style::default()
            };
            match &self.fields[idx].kind {
                FieldKind::Dropdown(_) => {
                    let text = format!("◀ {} ▶", self.fields[idx].value);
                    f.render_widget(
                        Paragraph::new(Span::styled(text, value_style)),
                        Rect::new(vx, y, vw, 1),
                    );
                }
                _ => {
                    let cur_chars = self.fields[idx].value[..self.cursor[idx]].chars().count();
                    let start_c = cur_chars.saturating_sub(vw as usize - 1);
                    let shown: String = self.fields[idx]
                        .value
                        .chars()
                        .skip(start_c)
                        .take(vw as usize)
                        .collect();
                    f.render_widget(
                        Paragraph::new(Span::styled(shown, value_style)),
                        Rect::new(vx, y, vw, 1),
                    );
                    if focused {
                        let cur_x = vx + (cur_chars - start_c) as u16;
                        f.set_cursor_position(Position::new(cur_x, y));
                    }
                }
            }
        }

        let hint = if total > rows {
            format!(
                "Tab/↑↓ 切换 · ←→ 编辑 · Enter 确认 · Esc 取消（第 {}/{} 字段）",
                self.focused + 1,
                total
            )
        } else {
            "Tab/↑↓ 切换 · ←→ 编辑 · Enter 确认 · Esc 取消".to_string()
        };
        f.render_widget(
            Paragraph::new(Span::styled(hint, Style::default().fg(Color::DarkGray))),
            Rect::new(
                inner.x + 1,
                inner.y + inner.height - 1,
                inner.width.saturating_sub(2),
                1,
            ),
        );
    }
}

/// 多选列表弹窗的关闭方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckAction {
    Confirm,
    Cancel,
}

/// 多选列表弹窗：j/k/↑↓ 移动，Space 勾选，字符过滤，Enter 确认，Esc 取消。
pub struct CheckboxList {
    title: String,
    items: Vec<String>,
    checked: Vec<bool>,
    selected: usize,
    filter: String,
    /// 渲染时记录的可见行数，用于滚动边界
    rows: usize,
}

impl CheckboxList {
    pub fn new(title: String, items: Vec<String>) -> Self {
        let checked = vec![false; items.len()];
        Self {
            title,
            items,
            checked,
            selected: 0,
            filter: String::new(),
            rows: 10,
        }
    }

    /// 过滤后可见项的原始下标。
    fn visible_indices(&self) -> Vec<usize> {
        let needle = self.filter.to_lowercase();
        self.items
            .iter()
            .enumerate()
            .filter(|(_, name)| needle.is_empty() || name.to_lowercase().contains(&needle))
            .map(|(i, _)| i)
            .collect()
    }

    /// 处理按键。返回 `Some(Confirm/Cancel)` 表示弹窗关闭。
    pub fn handle_key(&mut self, key: KeyEvent) -> Option<CheckAction> {
        let vis = self.visible_indices();
        match key.code {
            KeyCode::Esc => {
                if !self.filter.is_empty() {
                    // 先清空过滤，再次 Esc 才取消
                    self.filter.clear();
                    self.selected = 0;
                } else {
                    return Some(CheckAction::Cancel);
                }
            }
            KeyCode::Enter => return Some(CheckAction::Confirm),
            KeyCode::Up | KeyCode::Char('k') => {
                if !vis.is_empty() {
                    self.selected = self.selected.saturating_sub(1);
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.selected + 1 < vis.len() {
                    self.selected += 1;
                }
            }
            KeyCode::Char(' ') => {
                if let Some(&idx) = vis.get(self.selected) {
                    self.checked[idx] = !self.checked[idx];
                }
            }
            KeyCode::Backspace => {
                self.filter.pop();
                self.selected = 0;
            }
            KeyCode::Char(c) => {
                self.filter.push(c);
                self.selected = 0;
            }
            _ => {}
        }
        None
    }

    /// 当前勾选的项（原始顺序）。
    pub fn selected_items(&self) -> Vec<String> {
        self.items
            .iter()
            .enumerate()
            .filter(|(i, _)| self.checked[*i])
            .map(|(_, s)| s.clone())
            .collect()
    }

    pub fn render(&mut self, f: &mut Frame, area: Rect) {
        let popup = centered_rect(55, 75, area);
        f.render_widget(Clear, popup);
        let block = Block::new()
            .title(Span::styled(
                format!(" {} ", self.title),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan));
        let inner = block.inner(popup);
        f.render_widget(block, popup);

        // 过滤行
        let filter_text = if self.filter.is_empty() {
            "过滤: / 或输入字符".to_string()
        } else {
            format!("过滤: {}", self.filter)
        };
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    filter_text,
                    if self.filter.is_empty() {
                        Style::default().fg(Color::DarkGray)
                    } else {
                        Style::default().fg(Color::Yellow)
                    },
                ),
                Span::styled(" （Esc 清空）", Style::default().fg(Color::DarkGray)),
            ])),
            Rect::new(inner.x + 1, inner.y, inner.width.saturating_sub(2), 1),
        );

        let list_h = inner.height.saturating_sub(3).max(1) as usize;
        self.rows = list_h;
        let vis = self.visible_indices();
        if self.selected >= vis.len() {
            self.selected = vis.len().saturating_sub(1);
        }
        let offset = if self.selected >= list_h {
            self.selected - list_h + 1
        } else {
            0
        };
        for (row, &idx) in vis.iter().enumerate().skip(offset).take(list_h) {
            let y = inner.y + 1 + row as u16;
            let sel = row == self.selected;
            let mark = if self.checked[idx] { "[x]" } else { "[ ]" };
            let style = if sel {
                Style::default().fg(Color::Black).bg(Color::Cyan)
            } else {
                Style::default()
            };
            let text = format!("{} {}", mark, self.items[idx]);
            f.render_widget(
                Paragraph::new(Span::styled(text, style)),
                Rect::new(inner.x + 1, y, inner.width.saturating_sub(2), 1),
            );
        }

        f.render_widget(
            Paragraph::new(Span::styled(
                "j/k/↑↓ 移动 · Space 勾选 · Enter 确认 · Esc 取消",
                Style::default().fg(Color::DarkGray),
            )),
            Rect::new(
                inner.x + 1,
                inner.y + inner.height - 1,
                inner.width.saturating_sub(2),
                1,
            ),
        );
    }
}

/// 确认弹窗：y/Enter = 是，n/Esc = 否。
pub struct ConfirmPopup {
    title: String,
    message: String,
}

impl ConfirmPopup {
    pub fn new(title: String, message: String) -> Self {
        Self { title, message }
    }

    /// 返回 `Some(true)` 确认、`Some(false)` 拒绝、`None` 继续等待。
    pub fn handle_key(&mut self, key: KeyEvent) -> Option<bool> {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => Some(true),
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => Some(false),
            _ => None,
        }
    }

    pub fn render(&mut self, f: &mut Frame, area: Rect) {
        let popup = centered_rect(55, 30, area);
        f.render_widget(Clear, popup);
        let block = Block::new()
            .title(Span::styled(
                format!(" {} ", self.title),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Yellow));
        let inner = block.inner(popup);
        f.render_widget(block, popup);
        f.render_widget(
            Paragraph::new(Line::from(self.message.clone()))
                .wrap(ratatui::widgets::Wrap { trim: false }),
            Rect::new(
                inner.x + 1,
                inner.y,
                inner.width.saturating_sub(2),
                inner.height.saturating_sub(2),
            ),
        );
        f.render_widget(
            Paragraph::new(Span::styled(
                "[y] 是   [n] 否",
                Style::default().fg(Color::DarkGray),
            )),
            Rect::new(
                inner.x + 1,
                inner.y + inner.height - 1,
                inner.width.saturating_sub(2),
                1,
            ),
        );
    }
}

/// 消息弹窗：Esc/Enter/q 关闭，↑↓/PgUp/PgDn 滚动。
pub struct MessagePopup {
    title: String,
    lines: Vec<String>,
    scroll: usize,
    /// 渲染时记录的可见行数与行宽
    rows: usize,
    width: usize,
}

impl MessagePopup {
    pub fn new(title: String, lines: Vec<String>) -> Self {
        Self {
            title,
            lines,
            scroll: 0,
            rows: 10,
            width: 40,
        }
    }

    /// 弹窗标题（供主循环按类型判断弹窗，如关闭陈旧的出口 IP 失败弹窗）。
    pub fn title(&self) -> &str {
        &self.title
    }

    /// 返回 `true` 表示弹窗应关闭。
    pub fn handle_key(&mut self, key: KeyEvent) -> bool {
        let max_scroll = self.max_scroll();
        match key.code {
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => true,
            KeyCode::Up | KeyCode::Char('k') => {
                self.scroll = self.scroll.saturating_sub(1);
                false
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.scroll = (self.scroll + 1).min(max_scroll);
                false
            }
            KeyCode::PageUp => {
                self.scroll = self.scroll.saturating_sub(self.rows.max(1));
                false
            }
            KeyCode::PageDown => {
                self.scroll = (self.scroll + self.rows.max(1)).min(max_scroll);
                false
            }
            _ => false,
        }
    }

    /// 包裹后的总行数（按上次渲染的宽度估算）。
    fn wrapped_lines(&self) -> usize {
        let w = self.width.max(1);
        self.lines
            .iter()
            .map(|l| {
                let lw = Line::raw(l.clone()).width();
                if lw == 0 {
                    1
                } else {
                    lw.div_ceil(w)
                }
            })
            .sum()
    }

    fn max_scroll(&self) -> usize {
        self.wrapped_lines().saturating_sub(self.rows.max(1))
    }

    pub fn render(&mut self, f: &mut Frame, area: Rect) {
        let popup = centered_rect(70, 70, area);
        f.render_widget(Clear, popup);
        let block = Block::new()
            .title(Span::styled(
                format!(" {} ", self.title),
                Style::default()
                    .fg(Color::Magenta)
                    .add_modifier(Modifier::BOLD),
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Magenta));
        let inner = block.inner(popup);
        f.render_widget(block, popup);

        self.rows = inner.height.saturating_sub(2).max(1) as usize;
        self.width = inner.width.saturating_sub(2).max(1) as usize;
        let max_scroll = self.max_scroll();
        if self.scroll > max_scroll {
            self.scroll = max_scroll;
        }

        let para = Paragraph::new(Text::from(
            self.lines
                .iter()
                .map(|s| Line::raw(s.clone()))
                .collect::<Vec<Line>>(),
        ))
        .wrap(ratatui::widgets::Wrap { trim: false })
        .scroll((self.scroll as u16, 0));
        f.render_widget(
            para,
            Rect::new(
                inner.x + 1,
                inner.y,
                inner.width.saturating_sub(2),
                inner.height.saturating_sub(2),
            ),
        );

        let hint = if max_scroll > 0 {
            format!(
                "↑↓ 滚动（{}/{}）· Esc/Enter/q 关闭",
                self.scroll, max_scroll
            )
        } else {
            "Esc/Enter/q 关闭".to_string()
        };
        f.render_widget(
            Paragraph::new(Span::styled(hint, Style::default().fg(Color::DarkGray))),
            Rect::new(
                inner.x + 1,
                inner.y + inner.height - 1,
                inner.width.saturating_sub(2),
                1,
            ),
        );
    }
}

/// 单列选择列表（页面正文用）：j/k/↑↓ 移动并滚动。
pub struct SelectList {
    items: Vec<String>,
    selected: usize,
    offset: usize,
    title: Option<String>,
    /// 渲染时记录的可见行数
    rows: usize,
}

impl SelectList {
    pub fn new(items: Vec<String>) -> Self {
        Self {
            items,
            selected: 0,
            offset: 0,
            title: None,
            rows: 10,
        }
    }

    /// 设置标题（渲染为边框标题）。
    pub fn with_title(mut self, title: String) -> Self {
        self.title = Some(title);
        self
    }

    /// 整体替换列表项，选中项复位。
    pub fn set_items(&mut self, items: Vec<String>) {
        self.items = items;
        self.selected = 0;
        self.offset = 0;
    }

    pub fn items(&self) -> &[String] {
        &self.items
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                if self.selected > 0 {
                    self.selected -= 1;
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.selected + 1 < self.items.len() {
                    self.selected += 1;
                }
            }
            KeyCode::Home => self.selected = 0,
            KeyCode::End => self.selected = self.items.len().saturating_sub(1),
            _ => {}
        }
        let rows = self.rows.max(1);
        if self.selected < self.offset {
            self.offset = self.selected;
        } else if self.selected >= self.offset + rows {
            self.offset = self.selected - rows + 1;
        }
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    pub fn render(&mut self, f: &mut Frame, area: Rect) {
        let mut block = Block::new().borders(Borders::ALL);
        if let Some(title) = &self.title {
            block = block.title(Span::styled(
                format!(" {title} "),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ));
        }
        let inner = block.inner(area);
        f.render_widget(block, area);
        // 终端过小（块内无可用行）时跳过列表渲染，避免 y+1 越界 panic
        if inner.height == 0 {
            return;
        }

        self.rows = inner.height as usize;
        if self.selected >= self.items.len() {
            self.selected = self.items.len().saturating_sub(1);
        }
        let rows = self.rows.max(1);
        if self.selected < self.offset {
            self.offset = self.selected;
        } else if self.selected >= self.offset + rows {
            self.offset = self.selected - rows + 1;
        }
        for (row, item) in self.items.iter().enumerate().skip(self.offset).take(rows) {
            let y = inner.y + row as u16;
            let sel = row == self.selected;
            let style = if sel {
                Style::default().fg(Color::Black).bg(Color::Cyan)
            } else {
                Style::default()
            };
            f.render_widget(
                Paragraph::new(Span::styled(item.clone(), style)),
                Rect::new(inner.x + 1, y, inner.width.saturating_sub(2), 1),
            );
        }
    }
}

/// 底栏按键提示。
pub struct KeyHints {
    pub hints: Vec<(String, String)>,
}

impl KeyHints {
    pub fn render(&self, f: &mut Frame, area: Rect) {
        let mut spans: Vec<Span> = Vec::new();
        for (key, desc) in &self.hints {
            spans.push(Span::styled(
                format!("[{key}] "),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::raw(format!("{desc}   ")));
        }
        f.render_widget(Paragraph::new(Line::from(spans)), area);
    }
}

/// 以百分比在 area 内生成居中矩形。
pub fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::vertical([
        Constraint::Percentage((100 - percent_y) / 2),
        Constraint::Percentage(percent_y),
        Constraint::Percentage((100 - percent_y) / 2),
    ])
    .split(area);
    Layout::horizontal([
        Constraint::Percentage((100 - percent_x) / 2),
        Constraint::Percentage(percent_x),
        Constraint::Percentage((100 - percent_x) / 2),
    ])
    .split(vertical[1])[1]
}

/// 字节数格式化：B/KB/MB/GB/TB，1 位小数，如 "1.2 GB"。
pub fn format_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{:.1} {}", value, UNITS[unit])
    }
}

/// 速率格式化：format_bytes + "/s"。
pub fn format_rate(n: u64) -> String {
    format!("{}/s", format_bytes(n))
}

/// 计算字符串的显示宽度（按 unicode-width 规则，中文/emoji 占 2 列等）。
pub fn display_width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

/// 按显示宽度截断字符串，超出时以 "…"（占 1 列）结尾。
/// - "…" 占 1 列；若 `s` 宽度 <= `max_width` 原样返回
/// - 若 `max_width == 0` 返回 `""`
/// - 若 `max_width == 1` 且 `s` 非空且宽度 > 1 返回 `"…"`
/// - 否则截断到 `max_width - 1` 宽度后追加 `"…"`
///
/// 正确处理多字节、宽字符（中文 2 列、emoji 等）。
pub fn truncate_ellipsis(s: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    let width = UnicodeWidthStr::width(s);
    if width <= max_width {
        return s.to_string();
    }
    if max_width == 1 {
        // width > 1 且 s 非空（若 s 为空则 width==0 已在前面返回）
        return "…".to_string();
    }
    let target = max_width - 1;
    let mut cur_width = 0usize;
    let mut out = String::new();
    for ch in s.chars() {
        let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
        if cur_width + cw > target {
            break;
        }
        cur_width += cw;
        out.push(ch);
    }
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, crossterm::event::KeyModifiers::NONE)
    }

    #[test]
    fn format_bytes_units() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1024), "1.0 KB");
        assert_eq!(format_bytes(1536), "1.5 KB");
        assert_eq!(format_bytes(1024 * 1024), "1.0 MB");
        assert_eq!(format_bytes(1024 * 1024 * 1024), "1.0 GB");
        assert_eq!(format_bytes(1024u64.pow(4)), "1.0 TB");
    }

    #[test]
    fn format_rate_suffix() {
        assert_eq!(format_rate(2048), "2.0 KB/s");
    }

    #[test]
    fn form_popup_text_editing() {
        let mut popup = FormPopup::new(
            "测试".into(),
            vec![FormField {
                label: "名字".into(),
                value: String::new(),
                kind: FieldKind::Text,
            }],
        );
        assert_eq!(popup.handle_key(key(KeyCode::Char('a'))), None);
        assert_eq!(popup.handle_key(key(KeyCode::Char('b'))), None);
        assert_eq!(popup.values(), vec!["ab".to_string()]);
        // 光标在末尾，退格删除 b
        assert_eq!(popup.handle_key(key(KeyCode::Backspace)), None);
        assert_eq!(popup.values(), vec!["a".to_string()]);
        // Enter 确认
        assert_eq!(
            popup.handle_key(key(KeyCode::Enter)),
            Some(FormAction::Confirm)
        );
        // Esc 取消
        let mut popup2 = FormPopup::new(
            "测试".into(),
            vec![FormField {
                label: "x".into(),
                value: "1".into(),
                kind: FieldKind::Number,
            }],
        );
        assert_eq!(
            popup2.handle_key(key(KeyCode::Esc)),
            Some(FormAction::Cancel)
        );
    }

    #[test]
    fn form_popup_number_filter() {
        let mut popup = FormPopup::new(
            "端口".into(),
            vec![FormField {
                label: "port".into(),
                value: String::new(),
                kind: FieldKind::Number,
            }],
        );
        popup.handle_key(key(KeyCode::Char('a')));
        popup.handle_key(key(KeyCode::Char('7')));
        popup.handle_key(key(KeyCode::Char('8')));
        assert_eq!(popup.values(), vec!["78".to_string()]);
    }

    #[test]
    fn form_popup_dropdown_cycle() {
        let mut popup = FormPopup::new(
            "模式".into(),
            vec![FormField {
                label: "mode".into(),
                value: "rule".into(),
                kind: FieldKind::Dropdown(vec!["rule".into(), "global".into(), "direct".into()]),
            }],
        );
        popup.handle_key(key(KeyCode::Right));
        assert_eq!(popup.values(), vec!["global".to_string()]);
        popup.handle_key(key(KeyCode::Right));
        assert_eq!(popup.values(), vec!["direct".to_string()]);
        popup.handle_key(key(KeyCode::Right));
        assert_eq!(popup.values(), vec!["rule".to_string()]);
        popup.handle_key(key(KeyCode::Left));
        assert_eq!(popup.values(), vec!["direct".to_string()]);
    }

    #[test]
    fn form_popup_tab_moves_focus() {
        let mut popup = FormPopup::new(
            "测试".into(),
            vec![
                FormField {
                    label: "a".into(),
                    value: String::new(),
                    kind: FieldKind::Text,
                },
                FormField {
                    label: "b".into(),
                    value: String::new(),
                    kind: FieldKind::Text,
                },
            ],
        );
        // 焦点在字段 0 时输入
        popup.handle_key(key(KeyCode::Char('x')));
        assert_eq!(popup.values(), vec!["x".to_string(), String::new()]);
        // Tab 到字段 1
        popup.handle_key(key(KeyCode::Tab));
        popup.handle_key(key(KeyCode::Char('y')));
        assert_eq!(popup.values(), vec!["x".to_string(), "y".to_string()]);
        // ↑ 回到字段 0，↓ 到字段 1
        popup.handle_key(key(KeyCode::Up));
        popup.handle_key(key(KeyCode::Char('z')));
        assert_eq!(popup.values(), vec!["xz".to_string(), "y".to_string()]);
        popup.handle_key(key(KeyCode::Down));
        popup.handle_key(key(KeyCode::Char('w')));
        assert_eq!(popup.values(), vec!["xz".to_string(), "yw".to_string()]);
    }

    #[test]
    fn checkbox_list_basic() {
        let mut list = CheckboxList::new("成员".into(), vec!["a".into(), "b".into(), "c".into()]);
        assert!(list.selected_items().is_empty());
        // 勾选 a
        list.handle_key(key(KeyCode::Char(' ')));
        assert_eq!(list.selected_items(), vec!["a".to_string()]);
        // 下移到 b 并勾选
        list.handle_key(key(KeyCode::Char('j')));
        list.handle_key(key(KeyCode::Char(' ')));
        assert_eq!(
            list.selected_items(),
            vec!["a".to_string(), "b".to_string()]
        );
        // 过滤：只显示 b，勾选它（已勾选则取消）
        list.handle_key(key(KeyCode::Char('b')));
        list.handle_key(key(KeyCode::Char(' ')));
        assert_eq!(list.selected_items(), vec!["a".to_string()]);
        // 清空过滤（Esc），再按 Esc 取消
        assert_eq!(list.handle_key(key(KeyCode::Esc)), None);
        assert_eq!(
            list.handle_key(key(KeyCode::Esc)),
            Some(CheckAction::Cancel)
        );
        // Enter 确认
        list.handle_key(key(KeyCode::Char('j')));
        list.handle_key(key(KeyCode::Char(' ')));
        assert_eq!(
            list.handle_key(key(KeyCode::Enter)),
            Some(CheckAction::Confirm)
        );
        assert_eq!(
            list.selected_items(),
            vec!["a".to_string(), "b".to_string()]
        );
    }

    #[test]
    fn confirm_popup_keys() {
        let mut popup = ConfirmPopup::new("确认".into(), "继续?".into());
        assert_eq!(popup.handle_key(key(KeyCode::Char('y'))), Some(true));
        assert_eq!(popup.handle_key(key(KeyCode::Char('n'))), Some(false));
        assert_eq!(popup.handle_key(key(KeyCode::Enter)), Some(true));
        assert_eq!(popup.handle_key(key(KeyCode::Esc)), Some(false));
        assert_eq!(popup.handle_key(key(KeyCode::Char('x'))), None);
    }

    #[test]
    fn message_popup_scroll_and_close() {
        let lines: Vec<String> = (0..30).map(|i| format!("line {i}")).collect();
        let mut popup = MessagePopup::new("消息".into(), lines);
        popup.rows = 10;
        popup.width = 20;
        assert!(!popup.handle_key(key(KeyCode::Down)));
        assert_eq!(popup.scroll, 1);
        popup.handle_key(key(KeyCode::PageDown));
        assert!(popup.scroll > 1);
        // 滚动不能超过底部
        for _ in 0..100 {
            popup.handle_key(key(KeyCode::Down));
        }
        assert_eq!(popup.scroll, popup.max_scroll());
        assert!(popup.handle_key(key(KeyCode::Esc)));
        let mut popup2 = MessagePopup::new("消息".into(), vec!["x".into()]);
        assert!(popup2.handle_key(key(KeyCode::Enter)));
        let mut popup3 = MessagePopup::new("消息".into(), vec!["x".into()]);
        assert!(popup3.handle_key(key(KeyCode::Char('q'))));
    }

    #[test]
    fn select_list_navigation() {
        let mut list = SelectList::new(vec!["a".into(), "b".into(), "c".into()]);
        assert_eq!(list.selected(), 0);
        list.handle_key(key(KeyCode::Char('j')));
        assert_eq!(list.selected(), 1);
        list.handle_key(key(KeyCode::Char('k')));
        assert_eq!(list.selected(), 0);
        list.handle_key(key(KeyCode::Char('j')));
        list.handle_key(key(KeyCode::Char('j')));
        list.handle_key(key(KeyCode::Char('j'))); // 越界不动
        assert_eq!(list.selected(), 2);
    }

    /// ReadOnly 字段：FormPopup 中按键不修改值、不移动光标。
    #[test]
    fn readonly_field_ignores_keys() {
        let mut form = FormPopup::new(
            "测试".into(),
            vec![FormField {
                label: "secret".into(),
                value: "abc".into(),
                kind: FieldKind::ReadOnly,
            }],
        );
        // 各种编辑键均不应改变值
        for k in [
            KeyCode::Char('x'),
            KeyCode::Backspace,
            KeyCode::Delete,
            KeyCode::Left,
            KeyCode::Right,
        ] {
            form.handle_key(key(k));
            assert_eq!(form.values(), vec!["abc".to_string()]);
        }
    }

    #[test]
    fn select_list_render_tiny_area_no_panic() {
        use ratatui::backend::TestBackend;
        // 高度 0/1/2 时块内无可用行（inner.height == 0），早期返回不 panic
        for h in [0u16, 1, 2, 3] {
            let backend = TestBackend::new(20, h);
            let mut terminal = ratatui::Terminal::new(backend).unwrap();
            let mut list = SelectList::new(vec!["a".into(), "b".into(), "c".into()]);
            terminal.draw(|f| list.render(f, f.area())).unwrap();
        }
    }

    #[test]
    fn truncate_ellipsis_ascii() {
        // 无需截断
        assert_eq!(truncate_ellipsis("hello", 10), "hello");
        // 恰好等于宽度
        assert_eq!(truncate_ellipsis("hello", 5), "hello");
        // 超长截断
        assert_eq!(truncate_ellipsis("hello world", 5), "hell…");
        assert_eq!(truncate_ellipsis("hello world", 8), "hello w…");
        assert_eq!(truncate_ellipsis("abcdef", 5), "abcd…");
        // 刚好超 1
        assert_eq!(truncate_ellipsis("abcdef", 6), "abcdef");
        assert_eq!(truncate_ellipsis("abcdef", 4), "abc…");
    }

    #[test]
    fn truncate_ellipsis_boundary_zero_one() {
        // max_width == 0 恒返回空
        assert_eq!(truncate_ellipsis("hello", 0), "");
        assert_eq!(truncate_ellipsis("", 0), "");
        assert_eq!(truncate_ellipsis("中文", 0), "");
        assert_eq!(truncate_ellipsis("😀", 0), "");
        // max_width == 1 且原串宽度<=1 原样返回
        assert_eq!(truncate_ellipsis("a", 1), "a");
        assert_eq!(truncate_ellipsis("", 1), "");
        assert_eq!(truncate_ellipsis("é", 1), "é");
        // max_width == 1 且宽度>1 返回 "…"
        assert_eq!(truncate_ellipsis("ab", 1), "…");
        assert_eq!(truncate_ellipsis("hello", 1), "…");
        assert_eq!(truncate_ellipsis("中", 1), "…");
        assert_eq!(truncate_ellipsis("😀", 1), "…");
        assert_eq!(truncate_ellipsis("中文", 1), "…");
    }

    #[test]
    fn truncate_ellipsis_chinese() {
        // 中文每个 2 列
        assert_eq!(display_width("中文"), 4);
        assert_eq!(truncate_ellipsis("中文", 4), "中文");
        assert_eq!(truncate_ellipsis("中文", 5), "中文");
        assert_eq!(truncate_ellipsis("中文", 3), "中…");
        assert_eq!(truncate_ellipsis("中文", 2), "…");
        assert_eq!(truncate_ellipsis("中文测试", 8), "中文测试");
        assert_eq!(truncate_ellipsis("中文测试", 5), "中文…");
        assert_eq!(truncate_ellipsis("中文测试", 6), "中文…");
        assert_eq!(truncate_ellipsis("中文测试", 7), "中文测…");
        // 混合 ascii + 中文
        assert_eq!(truncate_ellipsis("a中b", 4), "a中b");
        assert_eq!(truncate_ellipsis("a中b", 3), "a…");
        assert_eq!(truncate_ellipsis("ab中", 4), "ab中");
        assert_eq!(truncate_ellipsis("ab中", 3), "ab…");
        // max_width 2 且首字符为宽字符时无法容纳
        assert_eq!(truncate_ellipsis("中a", 2), "…");
    }

    #[test]
    fn truncate_ellipsis_emoji_mixed() {
        // emoji 占 2 列
        assert_eq!(display_width("😀"), 2);
        assert_eq!(truncate_ellipsis("😀", 2), "😀");
        assert_eq!(truncate_ellipsis("😀😀", 3), "😀…");
        assert_eq!(truncate_ellipsis("😀😀", 4), "😀😀");
        // 混合 ascii + emoji
        assert_eq!(truncate_ellipsis("a😀b", 4), "a😀b");
        assert_eq!(truncate_ellipsis("a😀b", 3), "a…");
        // 中文 + emoji + ascii 混合
        assert_eq!(display_width("a中😀b"), 6);
        assert_eq!(truncate_ellipsis("a中😀b", 6), "a中😀b");
        assert_eq!(truncate_ellipsis("a中😀b", 5), "a中…");
        assert_eq!(truncate_ellipsis("a中😀b", 4), "a中…");
        assert_eq!(truncate_ellipsis("a中😀b", 3), "a…");
        // 含 emoji 的长串
        assert_eq!(truncate_ellipsis("hello😀world", 8), "hello😀…");
        // 👍 也是 2 列
        assert_eq!(display_width("👍"), 2);
        assert_eq!(truncate_ellipsis("a👍b", 3), "a…");
    }

    #[test]
    fn truncate_ellipsis_empty() {
        assert_eq!(truncate_ellipsis("", 0), "");
        assert_eq!(truncate_ellipsis("", 1), "");
        assert_eq!(truncate_ellipsis("", 5), "");
        assert_eq!(truncate_ellipsis("", 10), "");
        assert_eq!(display_width(""), 0);
    }

    #[test]
    fn display_width_basic() {
        assert_eq!(display_width("hello"), 5);
        assert_eq!(display_width("中文"), 4);
        assert_eq!(display_width("a中b"), 4);
        assert_eq!(display_width("😀"), 2);
        assert_eq!(display_width("a😀b"), 4);
        assert_eq!(display_width("…"), 1);
        assert_eq!(display_width(""), 0);
    }

    #[test]
    fn truncate_ellipsis_width_never_exceeds_max() {
        let cases = vec![
            "hello world",
            "中文测试",
            "a中😀b",
            "😀😀😀",
            "ab中",
            "a👍b中😀",
        ];
        for s in cases {
            for max in 0..10 {
                let out = truncate_ellipsis(s, max);
                let w = display_width(&out);
                assert!(
                    w <= max,
                    "truncate_ellipsis({:?}, {}) => {:?} width {} > max {}",
                    s,
                    max,
                    out,
                    w,
                    max
                );
                // 若原串宽度 <= max，应原样返回
                if display_width(s) <= max {
                    assert_eq!(out, s);
                } else if max > 0 {
                    // 截断时应以 … 结尾
                    assert!(out.ends_with('…'), "expected ellipsis for {:?} max {}", s, max);
                }
            }
        }
    }
}
