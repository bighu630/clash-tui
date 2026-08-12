//! 日志页：实时日志流展示（只读）。数据来自 AppState.logs（主循环后台任务
//! 填充），本页仅持有视图状态：当前级别、跟随/回溯偏移。

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::app::{AppState, UiCommand};
use crate::core::client::{LogEntry, LogLevel};
use crate::ui::Page;

/// 日志页视图状态。
pub struct LogsPage {
    /// 当前显示级别（经 UiCommand::SetLogLevel 通知后台任务重连 /logs?level=）。
    pub level: LogLevel,
    /// 是否跟随底部；回溯（↑↓/PgUp/PgDn）时暂停，f/End 恢复。
    pub follow: bool,
    /// 回溯偏移：距最新一条日志的条数（0 = 最新）。
    pub offset: usize,
}

impl LogsPage {
    pub fn new() -> Self {
        Self {
            level: LogLevel::Info,
            follow: true,
            offset: 0,
        }
    }

    /// 向上回溯 n 行（暂停跟随）。
    fn scroll_up(&mut self, n: usize) {
        self.follow = false;
        self.offset = self.offset.saturating_add(n);
    }

    /// 向下回溯 n 行；回到最新（offset=0）时恢复跟随。
    fn scroll_down(&mut self, n: usize) {
        self.follow = false;
        self.offset = self.offset.saturating_sub(n);
        if self.offset == 0 {
            self.follow = true;
        }
    }

    /// 复位视图到底部跟随。
    fn reset_view(&mut self) {
        self.follow = true;
        self.offset = 0;
    }

    /// 可见日志区间 [start, end)（纯函数，供测试）：
    /// follow=true → 最后 min(total, rows) 条；否则从 offset 处向上取 rows 条。
    /// 回溯超过总量（offset >= total）时窗口完全越过开头，钳制到开头显示
    /// 前 min(total, rows) 条（避免空区间）。
    pub fn visible_range(total: usize, rows: usize, follow: bool, offset: usize) -> (usize, usize) {
        if total == 0 || rows == 0 {
            return (0, 0);
        }
        let end = if follow {
            total
        } else {
            let end = total.saturating_sub(offset.min(total));
            if end == 0 {
                rows.min(total)
            } else {
                end
            }
        };
        let start = end.saturating_sub(rows);
        (start, end)
    }

    /// 级别样式（纯函数，供测试）。
    pub fn level_style(level: LogLevel) -> Style {
        match level {
            LogLevel::Error => Style::default().fg(Color::Red),
            LogLevel::Warning => Style::default().fg(Color::Yellow),
            LogLevel::Info => Style::default(),
            LogLevel::Debug => Style::default().fg(Color::Gray),
        }
    }

    /// 单条日志显示行（纯函数，供测试）：structured 带时间前缀，标准格式无时间。
    pub fn format_entry(e: &LogEntry) -> Line<'static> {
        let mut spans = Vec::new();
        if let Some(t) = &e.time {
            spans.push(Span::styled(
                format!("{t} "),
                Style::default().fg(Color::DarkGray),
            ));
        }
        spans.push(Span::styled(e.message.clone(), Self::level_style(e.level)));
        Line::from(spans)
    }
}

impl Default for LogsPage {
    fn default() -> Self {
        Self::new()
    }
}

impl Page for LogsPage {
    fn handle_key(&mut self, key: KeyEvent, st: &mut AppState) -> Option<UiCommand> {
        match key.code {
            KeyCode::Char('e') => {
                self.level = self.level.next();
                self.reset_view();
                Some(UiCommand::SetLogLevel(self.level))
            }
            KeyCode::Char('c') => {
                st.logs.clear();
                self.reset_view();
                None
            }
            KeyCode::Char('f') | KeyCode::End => {
                self.reset_view();
                None
            }
            KeyCode::Up => {
                self.scroll_up(1);
                None
            }
            KeyCode::Down => {
                self.scroll_down(1);
                None
            }
            KeyCode::PageUp => {
                self.scroll_up(10);
                None
            }
            KeyCode::PageDown => {
                self.scroll_down(10);
                None
            }
            _ => None,
        }
    }

    fn render(&mut self, f: &mut Frame, area: Rect, st: &AppState) {
        let title = format!(
            " 日志  [{}]  {} 条 {} ",
            self.level.as_str(),
            st.logs.len(),
            if self.follow { "跟随中" } else { "已暂停" }
        );
        let block = Block::new()
            .title(Span::styled(
                title,
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            ))
            .borders(Borders::ALL);
        let inner = block.inner(area);
        let rows = inner.height as usize;
        let (start, end) = Self::visible_range(st.logs.len(), rows, self.follow, self.offset);
        if start == end {
            let msg = Paragraph::new("等待 mihomo 日志……（按 e 切换级别）")
                .style(Style::default().fg(Color::DarkGray))
                .block(block);
            f.render_widget(msg, area);
            return;
        }
        let lines: Vec<Line> = st
            .logs
            .iter()
            .skip(start)
            .take(end - start)
            .map(Self::format_entry)
            .collect();
        f.render_widget(Paragraph::new(lines).block(block), area);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visible_range_follow_shows_tail() {
        assert_eq!(LogsPage::visible_range(100, 20, true, 0), (80, 100));
        assert_eq!(LogsPage::visible_range(10, 20, true, 0), (0, 10));
        assert_eq!(LogsPage::visible_range(0, 20, true, 0), (0, 0));
    }

    #[test]
    fn visible_range_scrolled_back() {
        // 回溯 30 条：显示 [50, 70)
        assert_eq!(LogsPage::visible_range(100, 20, false, 30), (50, 70));
        // 回溯超过总量：钳制到 [0, 20)
        assert_eq!(LogsPage::visible_range(100, 20, false, 500), (0, 20));
        // 偏移 0 但未跟随：等价于显示底部
        assert_eq!(LogsPage::visible_range(100, 20, false, 0), (80, 100));
    }

    #[test]
    fn level_style_maps() {
        assert_eq!(LogsPage::level_style(LogLevel::Error).fg, Some(Color::Red));
        assert_eq!(LogsPage::level_style(LogLevel::Warning).fg, Some(Color::Yellow));
        assert_eq!(LogsPage::level_style(LogLevel::Info).fg, None);
        assert_eq!(LogsPage::level_style(LogLevel::Debug).fg, Some(Color::Gray));
    }

    #[test]
    fn format_entry_structured_has_time_prefix() {
        let e = LogEntry {
            time: Some("12:00:00".into()),
            level: LogLevel::Warning,
            message: "boom".into(),
        };
        let line = LogsPage::format_entry(&e);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "12:00:00 boom");
    }

    #[test]
    fn format_entry_standard_no_time() {
        let e = LogEntry {
            time: None,
            level: LogLevel::Info,
            message: "hi".into(),
        };
        let line = LogsPage::format_entry(&e);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "hi");
    }

    #[test]
    fn scroll_up_pauses_follow() {
        let mut page = LogsPage::new();
        page.scroll_up(1);
        assert!(!page.follow);
        assert_eq!(page.offset, 1);
    }

    #[test]
    fn scroll_down_back_to_bottom_resumes_follow() {
        let mut page = LogsPage::new();
        page.scroll_up(5);
        page.scroll_down(3);
        assert!(!page.follow);
        assert_eq!(page.offset, 2);
        page.scroll_down(10);
        assert!(page.follow, "回到最新应恢复跟随");
        assert_eq!(page.offset, 0);
    }
}
