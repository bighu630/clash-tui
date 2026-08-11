//! 合并样例工具：加载本地三配置文件（网络设置/订阅/覆盖）→ 合并 → 输出 yaml。
//! 配置目录可用环境变量 MIHOMO_TUI_SETTINGS_DIR 覆盖（缺省 ~/.config/mihomo-tui）。
//! 用法：`cargo run --example merge_sample > /tmp/config.yaml && mihomo -t -f /tmp/config.yaml`

use mihomo_tui::core::merger::{merge, MergeContext};
use mihomo_tui::core::models::Subscription;
use mihomo_tui::core::settings::{load_overrides, load_settings, load_subscriptions};

fn main() {
    let settings = match load_settings() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("加载设置失败: {e}");
            std::process::exit(1);
        }
    };
    let overrides = match load_overrides() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("加载覆盖配置失败: {e}");
            std::process::exit(1);
        }
    };
    let subs = match load_subscriptions() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("加载订阅列表失败: {e}");
            std::process::exit(1);
        }
    };

    let active: Option<&Subscription> = subs.iter().find(|s| s.active);
    if active.is_none() {
        eprintln!("注意: 没有激活的订阅（仅输出网络段与自定义配置）");
    }

    match merge(MergeContext {
        settings: &settings,
        overrides: &overrides,
        subscription: active,
    }) {
        Ok(out) => {
            for w in &out.warnings {
                eprintln!("警告: {w}");
            }
            print!("{}", out.config);
        }
        Err(e) => {
            eprintln!("合并失败: {}", e.message);
            std::process::exit(1);
        }
    }
}
