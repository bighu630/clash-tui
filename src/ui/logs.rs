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
    /// 暂停时窗口首行（绝对索引，0 = 缓冲最旧一条）；跟随模式下每帧由 total 重算。
    pub top: usize,
    /// 最近一次渲染的视口行数（handle_key 滚动钳制用）。
    view_rows: usize,
}

impl LogsPage {
    pub fn new() -> Self {
        Self {
            level: LogLevel::Info,
            follow: true,
            top: 0,
            view_rows: 0,
        }
    }

    /// 向上回溯 n 行（暂停跟随，不能越过缓冲开头）。
    fn scroll_up(&mut self, n: usize) {
        self.follow = false;
        self.top = self.top.saturating_sub(n);
    }

    /// 向下滚动 n 行；窗口底部到达最新日志（top >= total - view_rows）时恢复跟随。
    fn scroll_down(&mut self, n: usize, total: usize) {
        self.follow = false;
        let max_top = total.saturating_sub(self.view_rows);
        self.top = self.top.saturating_add(n).min(max_top);
        if self.top >= max_top {
            self.follow = true;
        }
    }

    /// 复位视图到底部跟随。
    fn reset_view(&mut self) {
        self.follow = true;
        self.top = 0;
    }

    /// 可见日志区间 [start, end)（纯函数，供测试）：
    /// follow=true → 最后 min(total, rows) 条；否则以 top 为窗口首行（绝对索引），
    /// 新日志到达不移动窗口（回溯阅读不漂移）；top 越过底部时钳制到末尾。
    pub fn visible_range(total: usize, rows: usize, follow: bool, top: usize) -> (usize, usize) {
        if total == 0 || rows == 0 {
            return (0, 0);
        }
        if follow {
            let start = total.saturating_sub(rows);
            (start, total)
        } else {
            let start = top.min(total.saturating_sub(rows));
            (start, (start + rows).min(total))
        }
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
                self.scroll_down(1, st.logs.len());
                None
            }
            KeyCode::PageUp => {
                self.scroll_up(10);
                None
            }
            KeyCode::PageDown => {
                self.scroll_down(10, st.logs.len());
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
        self.view_rows = rows;
        let (start, end) = Self::visible_range(st.logs.len(), rows, self.follow, self.top);
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
    fn visible_range_paused_shows_window_from_top() {
        // 暂停：top=50 显示 [50, 70)
        assert_eq!(LogsPage::visible_range(100, 20, false, 50), (50, 70));
        // top 越过底部：钳制到末尾
        assert_eq!(LogsPage::visible_range(100, 20, false, 500), (80, 100));
        // top=0：显示缓冲开头
        assert_eq!(LogsPage::visible_range(100, 20, false, 0), (0, 20));
    }

    #[test]
    fn visible_range_paused_no_drift_when_new_logs_arrive() {
        // 暂停在 top=50；新日志到达（total 100→120）窗口不移动
        assert_eq!(LogsPage::visible_range(100, 20, false, 50), (50, 70));
        assert_eq!(LogsPage::visible_range(120, 20, false, 50), (50, 70));
        // 跟随模式：total 增长窗口追尾
        assert_eq!(LogsPage::visible_range(120, 20, true, 0), (100, 120));
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
        page.top = 10;
        page.scroll_up(3);
        assert!(!page.follow);
        assert_eq!(page.top, 7);
        page.scroll_up(100);
        assert_eq!(page.top, 0, "不能越过缓冲开头");
    }

    #[test]
    fn scroll_down_back_to_bottom_resumes_follow() {
        let mut page = LogsPage::new();
        page.view_rows = 20;
        page.top = 50;
        page.scroll_down(3, 100);
        assert!(!page.follow);
        assert_eq!(page.top, 53);
        // 窗口底部到达最新日志（top >= total - view_rows）恢复跟随
        page.scroll_down(100, 100);
        assert!(page.follow, "到达底部应恢复跟随");
        // 越过底部钳制
        let mut page2 = LogsPage::new();
        page2.view_rows = 20;
        page2.top = 90;
        page2.scroll_down(10, 100);
        assert_eq!(page2.top, 80, "应钳制到 total - view_rows");
        assert!(page2.follow);
    }
}
