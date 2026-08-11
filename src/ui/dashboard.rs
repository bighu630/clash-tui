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
        st.settings = s;
        Ok(UiCommand::ApplyConfig(out.config))
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
                    let next = match st.runtime.mode.as_str() {
                        "global" => "direct",
                        "direct" => "rule",
                        _ => "global",
                    };
                    st.runtime.mode = next.to_string();
                    return Some(UiCommand::PatchConfigs(serde_json::json!({"mode": next})));
                }
                // TUN 热切
                KeyCode::Char('t') => {
                    let enable = !st.runtime.tun_enable;
                    st.runtime.tun_enable = enable;
                    return Some(UiCommand::PatchConfigs(serde_json::json!({"tun": {"enable": enable}})));
                }
                // IPv6 热切
                KeyCode::Char('6') => {
                    let enable = !st.runtime.ipv6;
                    st.runtime.ipv6 = enable;
                    return Some(UiCommand::PatchConfigs(serde_json::json!({"ipv6": enable})));
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
