//! UI 层：Page trait、页面模块与通用组件。
//! 契约见 docs/superpowers/plans/2026-08-10-mihomo-tui.md §3。

pub mod dashboard;
pub mod groups;
pub mod logs;
pub mod rules;
pub mod settings;
pub mod subscriptions;
pub mod widgets;

use crossterm::event::KeyEvent;
use ratatui::layout::Rect;
use ratatui::Frame;

use crate::app::{AppState, UiCommand};

/// 页面抽象：处理按键 + 渲染。页面内部可持有自己的弹窗状态。
pub trait Page {
    /// 处理按键。返回 `Some(UiCommand)` 表示需要主循环执行的异步操作。
    /// 页面应先把自己的弹窗（FormPopup/MessagePopup 等）喂给按键，弹窗打开时
    /// 返回 None 且不处理任何页面逻辑。
    fn handle_key(&mut self, key: KeyEvent, st: &mut AppState) -> Option<UiCommand>;
    /// 渲染页面内容（含页面内部弹窗，弹窗最后绘制）。
    fn render(&mut self, f: &mut Frame, area: Rect, st: &AppState);
    /// 页面内部是否有弹窗打开。主循环据此在弹窗打开时把按键全部交给页面
    /// （全局键 q/Esc/Tab/←→/1-5/? 不生效），避免误触退出/切页。
    /// 默认 false；有内部弹窗的页面必须实现。
    fn popup_open(&self) -> bool {
        false
    }

    /// 切页进入时的回调（默认无操作）。设置页用它从 st.settings 重新同步字段。
    fn on_enter(&mut self, _st: &AppState) {}
}
