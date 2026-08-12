//! 首页仪表盘：状态行（模式/TUN/IPv6/出口IP/API 状态）、实时网速、总流量、内存。
//! 交互规格见 docs/superpowers/plans/2026-08-10-mihomo-tui.md §3。

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Sparkline};
use ratatui::Frame;

use crate::app::{AppState, UiCommand};
use crate::core::merger::{merge, MergeContext};
use crate::core::models::NetworkSettings;
use crate::core::settings::save_settings;
use crate::ui::widgets::{FormAction, FormField, FieldKind, FormPopup, MessagePopup};
use crate::ui::Page;

#[derive(Default)]
pub struct DashboardPage {
    popup: Option<DashPopup>,
}

enum DashPopup {
    Form(FormPopup),
    Msg(MessagePopup),
}

impl DashboardPage {
    pub fn new() -> Self {
        Self { popup: None }
    }

    /// 网络设置表单 → 保存 settings → 合并 → ApplyConfig。
    fn apply_form(&mut self, form: FormPopup, st: &mut AppState) -> Result<UiCommand, String> {
        let v = form.values();
        let invalid = |label: &str, val: &str| format!("「{label}」数值无效: {val}");
        let mut s = st.settings.clone();
        s.port = v[0].parse().map_err(|_| invalid("port", &v[0]))?;
        s.socks_port = v[1].parse().map_err(|_| invalid("socks-port", &v[1]))?;
        s.mixed_port = v[2].parse().map_err(|_| invalid("mixed-port", &v[2]))?;
        s.allow_lan = v[3] == "是";
        s.log_level = v[4].clone();
        s.tun.stack = v[5].clone();
        s.tun.auto_route = v[6] == "是";
        s.tun.mtu = v[7].parse().map_err(|_| invalid("tun.mtu", &v[7]))?;
        s.tun.dns_hijack = split_csv(&v[8]);
        s.dns.enable = v[9] == "是";
        s.dns.nameserver = split_csv(&v[10]);

        save_settings(&s).map_err(|e| format!("保存设置失败: {e}"))?;
        // 磁盘已落盘新值：立即同步 st.settings（clone 保留局部 s 供 merge 使用），
        // 关闭「保存成功但 merge 失败时内存与磁盘分叉」窗口。
        st.settings = s.clone();

        let active = st.subs.iter().find(|sub| sub.active);
        let out = merge(MergeContext {
            settings: &s,
            overrides: &st.overrides,
            subscription: active,
        })
        .map_err(|e| format!("配置合并失败: {e}"))?;
        if !out.warnings.is_empty() {
            st.notice(format!("[!] 合并警告: {}", out.warnings.join("；")));
        }
        Ok(UiCommand::ApplyConfig(out.config))
    }
    /// 开关双写：先持久化 settings.toml，再返回 PATCH 热切命令。
    /// 保存失败不放弃热切（仍返回 PATCH），但必须弹「保存失败」明确告知；
    /// 保存成功则把新设置写回 st.settings——保证 merger（merge 读 ctx.settings
    /// 生成 config.yaml）在任何后续结构性变更（订阅更新/切换 → 重启）中
    /// 永远读到最新持久化值，开关状态不丢失。
    fn toggle_double_write(
        &mut self,
        st: &mut AppState,
        label: &str,
        apply: impl FnOnce(&mut NetworkSettings),
        patch: serde_json::Value,
    ) -> UiCommand {
        let mut s = st.settings.clone();
        apply(&mut s);
        match save_settings(&s) {
            Ok(()) => {
                // 关键：st.settings 必须同步为已保存值（merger 读取它的字段）。
                st.settings = s;
                UiCommand::PatchConfigs {
                    patch,
                    saved: true,
                    label: label.to_string(),
                }
            }
            Err(e) => {
                self.popup = Some(DashPopup::Msg(MessagePopup::new(
                    "保存失败".into(),
                    vec![format!("「{label}」将尝试热切换，但设置保存失败：{e}（重启后会丢失）")],
                )));
                UiCommand::PatchConfigs {
                    patch,
                    saved: false,
                    label: label.to_string(),
                }
            }
        }
    }
}

impl Page for DashboardPage {
    fn popup_open(&self) -> bool {
        self.popup.is_some()
    }

    fn handle_key(&mut self, key: KeyEvent, st: &mut AppState) -> Option<UiCommand> {
        // 弹窗优先
        match self.popup.take() {
            Some(DashPopup::Form(mut form)) => match form.handle_key(key) {
                Some(FormAction::Confirm) => match self.apply_form(form, st) {
                    Ok(cmd) => return Some(cmd),
                    Err(msg) => {
                        self.popup =
                            Some(DashPopup::Msg(MessagePopup::new("错误".into(), vec![msg])));
                    }
                },
                Some(FormAction::Cancel) => {}
                None => {
                    self.popup = Some(DashPopup::Form(form));
                }
            },
            Some(DashPopup::Msg(mut msg)) => {
                if !msg.handle_key(key) {
                    self.popup = Some(DashPopup::Msg(msg));
                }
            }
            None => match key.code {
                // 模式循环 rule → global → direct
                KeyCode::Char('m') => {
                    let next = next_mode(&st.runtime.mode);
                    st.runtime.mode = next.to_string();
                    return Some(self.toggle_double_write(
                        st,
                        "模式",
                        |s| s.mode = next.to_string(),
                        serde_json::json!({"mode": next}),
                    ));
                }
                // TUN 热切
                KeyCode::Char('t') => {
                    let enable = !st.runtime.tun_enable;
                    st.runtime.tun_enable = enable;
                    return Some(self.toggle_double_write(
                        st,
                        "TUN",
                        |s| s.tun.enable = enable,
                        serde_json::json!({"tun": {"enable": enable}}),
                    ));
                }
                // IPv6 热切
                KeyCode::Char('6') => {
                    let enable = !st.runtime.ipv6;
                    st.runtime.ipv6 = enable;
                    return Some(self.toggle_double_write(
                        st,
                        "IPv6",
                        |s| s.ipv6 = enable,
                        serde_json::json!({"ipv6": enable}),
                    ));
                }
                // 手动刷新出口 IP
                KeyCode::Char('r') => return Some(UiCommand::FetchExitIp),
                // 网络设置表单
                KeyCode::Char('s') => {
                    self.popup = Some(DashPopup::Form(settings_form(&st.settings)));
                }
                // M6 遗留：首启拒绝安装后的重试入口（提权组件缺失时可用）
                KeyCode::Char('i') => return Some(UiCommand::InstallSetup),
                _ => {}
            },
        }
        None
    }

    fn render(&mut self, f: &mut Frame, area: Rect, st: &AppState) {
        let [status, body] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
        render_status(f, status, st);

        let [left, right] =
            Layout::horizontal([Constraint::Percentage(60), Constraint::Percentage(40)])
                .areas(body);
        render_traffic(f, left, st);
        render_totals(f, right, st);

        if let Some(popup) = &mut self.popup {
            match popup {
                DashPopup::Form(form) => form.render(f, area),
                DashPopup::Msg(msg) => msg.render(f, area),
            }
        }
    }
}

fn yes_no(b: bool) -> String {
    if b { "是".to_string() } else { "否".to_string() }
}

fn split_csv(s: &str) -> Vec<String> {
    s.split(',')
        .map(|part| part.trim().to_string())
        .filter(|part| !part.is_empty())
        .collect()
}

fn settings_form(s: &NetworkSettings) -> FormPopup {
    FormPopup::new(
        "网络设置".into(),
        vec![
            FormField { label: "port".into(), value: s.port.to_string(), kind: FieldKind::Number },
            FormField { label: "socks-port".into(), value: s.socks_port.to_string(), kind: FieldKind::Number },
            FormField { label: "mixed-port".into(), value: s.mixed_port.to_string(), kind: FieldKind::Number },
            FormField { label: "allow-lan".into(), value: yes_no(s.allow_lan), kind: FieldKind::Dropdown(vec!["是".into(), "否".into()]) },
            FormField { label: "log-level".into(), value: s.log_level.clone(), kind: FieldKind::Dropdown(vec!["silent".into(), "error".into(), "warning".into(), "info".into(), "debug".into()]) },
            FormField { label: "tun.stack".into(), value: s.tun.stack.clone(), kind: FieldKind::Dropdown(vec!["system".into(), "gvisor".into(), "mixed".into()]) },
            FormField { label: "tun.auto-route".into(), value: yes_no(s.tun.auto_route), kind: FieldKind::Dropdown(vec!["是".into(), "否".into()]) },
            FormField { label: "tun.mtu".into(), value: s.tun.mtu.to_string(), kind: FieldKind::Number },
            FormField { label: "tun.dns-hijack".into(), value: s.tun.dns_hijack.join(","), kind: FieldKind::Text },
            FormField { label: "dns.enable".into(), value: yes_no(s.dns.enable), kind: FieldKind::Dropdown(vec!["是".into(), "否".into()]) },
            FormField { label: "dns.nameserver".into(), value: s.dns.nameserver.join(","), kind: FieldKind::Text },
        ],
    )
}

/// 模式循环 rule → global → direct → rule（非 global/direct 一律 → "global"）。
fn next_mode(current: &str) -> &'static str {
    match current {
        "global" => "direct",
        "direct" => "rule",
        _ => "global",
    }
}

/// 顶栏状态行：`模式: rule [m] | TUN: on [t] | IPv6: on [6] | 出口IP: x [r] | API: 已连接`
fn render_status(f: &mut Frame, area: Rect, st: &AppState) {
    let mode = if st.runtime.mode.is_empty() {
        st.settings.mode.as_str()
    } else {
        st.runtime.mode.as_str()
    };
    let tun = if st.runtime.tun_enable { "开" } else { "关" };
    let ipv6 = if st.runtime.ipv6 { "开" } else { "关" };
    let ip = st.exit_ip.as_deref().unwrap_or("未知");
    let (api_text, api_color) = if st.api_ok {
        ("已连接", Color::Green)
    } else {
        ("未连接", Color::Red)
    };
    let spans = vec![
        Span::raw("模式: "),
        Span::styled(mode, Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
        Span::raw(" [m]  "),
        Span::raw("TUN: "),
        Span::styled(tun, Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
        Span::raw(" [t]  "),
        Span::raw("IPv6: "),
        Span::styled(ipv6, Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
        Span::raw(" [6]  "),
        Span::raw("出口IP: "),
        Span::styled(ip, Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
        Span::raw(" [r]  "),
        Span::raw("API: "),
        Span::styled(api_text, Style::default().fg(api_color).add_modifier(Modifier::BOLD)),
    ];
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// 左 60%：实时网速双 Sparkline。
fn render_traffic(f: &mut Frame, area: Rect, st: &AppState) {
    let block = Block::new()
        .title(Span::styled(" 实时网速 ", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let last = st.traffic.back().copied();
    let up_rate = last.map(|frame| frame.up).unwrap_or(0);
    let down_rate = last.map(|frame| frame.down).unwrap_or(0);
    let up_data: Vec<u64> = st.traffic.iter().map(|frame| frame.up).collect();
    let down_data: Vec<u64> = st.traffic.iter().map(|frame| frame.down).collect();

    let [l1, s1, l2, s2, _rest] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(3),
        Constraint::Length(1),
        Constraint::Length(3),
        Constraint::Min(0),
    ])
    .areas(inner);

    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("↑ 上行 ", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
            Span::styled(crate::ui::widgets::format_rate(up_rate), Style::default().fg(Color::Green)),
        ])),
        l1,
    );
    f.render_widget(
        Sparkline::default().data(&up_data).style(Style::default().fg(Color::Green)),
        s1,
    );
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("↓ 下行 ", Style::default().fg(Color::Blue).add_modifier(Modifier::BOLD)),
            Span::styled(crate::ui::widgets::format_rate(down_rate), Style::default().fg(Color::Blue)),
        ])),
        l2,
    );
    f.render_widget(
        Sparkline::default().data(&down_data).style(Style::default().fg(Color::Blue)),
        s2,
    );
}

/// 右 40%：总流量 + 内存。
fn render_totals(f: &mut Frame, area: Rect, st: &AppState) {
    let [tot, mem] =
        Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(area);

    // 总流量
    let block = Block::new()
        .title(Span::styled(" 总流量 ", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));
    let inner = block.inner(tot);
    f.render_widget(block, tot);
    let last = st.traffic.back().copied();
    let (up_total, down_total) = last
        .map(|frame| (frame.up_total, frame.down_total))
        .unwrap_or((0, 0));
    let [t1, t2] = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(inner);
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("↑ ", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
            Span::styled(
                crate::ui::widgets::format_bytes(up_total),
                Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
            ),
        ])),
        t1,
    );
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("↓ ", Style::default().fg(Color::Blue).add_modifier(Modifier::BOLD)),
            Span::styled(
                crate::ui::widgets::format_bytes(down_total),
                Style::default().fg(Color::Blue).add_modifier(Modifier::BOLD),
            ),
        ])),
        t2,
    );

    // 内存
    let block = Block::new()
        .title(Span::styled(" 内存 ", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));
    let inner = block.inner(mem);
    f.render_widget(block, mem);
    let inuse = st.mem_history.back().copied().unwrap_or(0);
    let mem_data: Vec<u64> = st.mem_history.iter().copied().collect();
    let [m1, m2] = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(inner);
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                crate::ui::widgets::format_bytes(inuse),
                Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" inuse", Style::default().fg(Color::DarkGray)),
        ])),
        m1,
    );
    f.render_widget(
        Sparkline::default().data(&mem_data).style(Style::default().fg(Color::Magenta)),
        m2,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::client::RuntimeConfig;
    use crate::core::models::Overrides;
    use crate::core::settings::{load_settings, settings_path, with_settings_dir};
    use crossterm::event::KeyModifiers;
    use std::collections::VecDeque;

    /// 构造最小 AppState（字段全 pub，参照 app.rs test_app 的构造）。
    fn test_state() -> AppState {
        AppState {
            settings: NetworkSettings::default(),
            subs: Vec::new(),
            overrides: Overrides::default(),
            runtime: RuntimeConfig::default(),
            api_ok: false,
            api_confirmed: false,
            traffic: VecDeque::new(),
            mem_history: VecDeque::new(),
            exit_ip: None,
            notices: VecDeque::new(),
        }
    }

    fn press(page: &mut DashboardPage, st: &mut AppState, code: KeyCode) -> UiCommand {
        page.handle_key(KeyEvent::new(code, KeyModifiers::NONE), st)
            .expect("开关按键应返回命令")
    }

    /// 断言返回 PatchConfigs 并解构出 (patch, saved, label)。
    fn expect_patch(cmd: &UiCommand) -> (&serde_json::Value, bool, &str) {
        match cmd {
            UiCommand::PatchConfigs { patch, saved, label } => (patch, *saved, label),
            _ => panic!("期望 PatchConfigs"),
        }
    }

    // ---- next_mode 纯函数 ----

    #[test]
    fn next_mode_cycles_rule_global_direct() {
        assert_eq!(next_mode("rule"), "global");
        assert_eq!(next_mode("global"), "direct");
        assert_eq!(next_mode("direct"), "rule");
        // 空串/未知值一律 → global（保留原 match 语义）
        assert_eq!(next_mode(""), "global");
        assert_eq!(next_mode("unknown"), "global");
    }

    // ---- 双写成功：settings.toml 落盘 + 热切 PATCH ----

    #[test]
    fn toggle_t_persists_settings_and_patches() {
        with_settings_dir(|| {
            let mut st = test_state();
            let mut page = DashboardPage::new();
            let cmd = press(&mut page, &mut st, KeyCode::Char('t'));
            let (patch, saved, label) = expect_patch(&cmd);
            assert_eq!(*patch, serde_json::json!({"tun": {"enable": true}}));
            assert!(saved, "保存应成功");
            assert_eq!(label, "TUN");
            // 内存双通道同步
            assert!(st.settings.tun.enable, "st.settings 应同步为持久化值");
            assert!(st.runtime.tun_enable, "运行时乐观更新");
            // 磁盘：重新加载确认落盘
            let back = load_settings().expect("应能重新加载");
            assert!(back.tun.enable, "磁盘 settings.toml 应已更新");
        });
    }

    #[test]
    fn toggle_6_persists_settings_and_patches() {
        with_settings_dir(|| {
            let mut st = test_state();
            let mut page = DashboardPage::new();
            let cmd = press(&mut page, &mut st, KeyCode::Char('6'));
            let (patch, saved, label) = expect_patch(&cmd);
            assert_eq!(*patch, serde_json::json!({"ipv6": true}));
            assert!(saved, "保存应成功");
            assert_eq!(label, "IPv6");
            assert!(st.settings.ipv6, "st.settings 应同步为持久化值");
            assert!(st.runtime.ipv6, "运行时乐观更新");
            let back = load_settings().expect("应能重新加载");
            assert!(back.ipv6, "磁盘 settings.toml 应已更新");
        });
    }

    #[test]
    fn toggle_m_persists_settings_and_patches() {
        with_settings_dir(|| {
            let mut st = test_state();
            st.runtime.mode = "global".into(); // 期望循环到 direct
            let mut page = DashboardPage::new();
            let cmd = press(&mut page, &mut st, KeyCode::Char('m'));
            let (patch, saved, label) = expect_patch(&cmd);
            assert_eq!(*patch, serde_json::json!({"mode": "direct"}));
            assert!(saved, "保存应成功");
            assert_eq!(label, "模式");
            assert_eq!(st.settings.mode, "direct", "st.settings 应同步为持久化值");
            assert_eq!(st.runtime.mode, "direct", "运行时乐观更新");
            let back = load_settings().expect("应能重新加载");
            assert_eq!(back.mode, "direct", "磁盘 settings.toml 应已更新");
        });
    }

    // ---- 保存失败：热切不放弃 + 明确弹窗反馈 ----

    #[test]
    fn toggle_t_save_failure_still_patches_with_popup() {
        with_settings_dir(|| {
            // settings.toml 建成目录 → save_settings 的 rename 必然失败
            std::fs::create_dir_all(settings_path()).unwrap();
            let mut st = test_state();
            let mut page = DashboardPage::new();
            let cmd = press(&mut page, &mut st, KeyCode::Char('t'));
            let (patch, saved, label) = expect_patch(&cmd);
            assert_eq!(*patch, serde_json::json!({"tun": {"enable": true}}));
            assert!(!saved, "保存失败时 saved 应为 false");
            assert_eq!(label, "TUN");
            // 热切不因保存失败而放弃：运行时乐观更新照常
            assert!(st.runtime.tun_enable, "热切应照常进行");
            // 明确反馈：弹窗「保存失败」（禁止静默部分成功）
            match &page.popup {
                Some(DashPopup::Msg(m)) => assert_eq!(m.title(), "保存失败"),
                _ => panic!("保存失败时应弹「保存失败」弹窗"),
            }
            // 设置确实未持久化：settings.toml 仍是目录（无文件落盘）
            assert!(!settings_path().is_file(), "失败时不应有文件落盘");
            // 关键不变量：保存失败时 st.settings 不应更新——内存与磁盘一致，仍为旧值
            assert!(!st.settings.tun.enable, "保存失败时 st.settings 不应更新");
        });
    }
}
