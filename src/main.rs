//! 终端初始化/恢复、panic hook，然后进入 app::run 主循环。
//! 契约见 docs/superpowers/plans/2026-08-10-mihomo-tui.md §3。

use std::io;

use crossterm::cursor::Show;
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};

fn main() -> Result<(), mihomo_tui::app::BoxError> {
    install_panic_hook();

    execute!(io::stdout(), EnterAlternateScreen)?;
    enable_raw_mode()?;

    let result = run_blocking();

    // finally 恢复终端
    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), LeaveAlternateScreen, Show);
    result
}

fn run_blocking() -> Result<(), mihomo_tui::app::BoxError> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(mihomo_tui::app::run())
}

/// panic 时尽力恢复终端，再交给默认 hook 打印。
fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen, Show);
        default_hook(info);
    }));
}
