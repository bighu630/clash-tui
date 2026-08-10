fn main() -> anyhow::Result<()> {
    // Worker B1: 终端初始化（panic hook / raw mode / alternate screen）后调用
    mihomo_tui::app::run()
}
