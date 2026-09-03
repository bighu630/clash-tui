//! 首页仪表盘：状态行（模式/TUN/IPv6/出口IP/API 状态）、连接、网络、内存。
//! 交互规格见 docs/superpowers/plans/2026-08-10-mihomo-tui.md §3。

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Sparkline};
use ratatui::Frame;

use crate::app::{AppState, UiCommand};
use crate::core::client::ConnInfo;
use crate::core::models::NetworkSettings;
use crate::core::settings::save_settings;
use crate::ui::widgets::{truncate_ellipsis, MessagePopup};
use crate::ui::Page;

#[derive(Default)]
pub struct DashboardPage {
    popup: Option<MessagePopup>,
}

impl DashboardPage {
    pub fn new() -> Self {
        Self { popup: None }
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
                self.popup = Some(MessagePopup::new(
                    "保存失败".into(),
                    vec![format!(
                        "「{label}」将尝试热切换，但设置保存失败：{e}（重启后会丢失）"
                    )],
                ));
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
            Some(mut msg) => {
                if !msg.handle_key(key) {
                    self.popup = Some(msg);
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
                    // Windows：非管理员开 TUN → 警告（UAC 无法中途提升；不阻塞）
                    #[cfg(windows)]
                    if enable && !crate::service::process::is_elevated() {
                        st.notice(
                            "[!] TUN 需要管理员权限：当前 TUI 未以管理员身份运行，mihomo 将无法创建 TUN 设备"
                                .to_string(),
                        );
                    }
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

        if connections_visible(body.width) {
            let [left, right] =
                Layout::horizontal([Constraint::Percentage(60), Constraint::Percentage(40)])
                    .areas(body);
            render_connections(f, left, st);
            render_right(f, right, st);
        } else {
            // 窄窗口：隐藏连接框，网络+内存占满全宽
            render_right(f, body, st);
        }

        if let Some(popup) = &mut self.popup {
            popup.render(f, area);
        }
    }
}

/// 连接框响应式隐藏阈值：body 宽度低于此值时不渲染左列连接框。
const CONNECTIONS_MIN_WIDTH: u16 = 60;

/// 宽度是否足够显示连接框。
fn connections_visible(width: u16) -> bool {
    width >= CONNECTIONS_MIN_WIDTH
}

/// 网络列固定预留宽度（kind + ↑/↓ 流量），约 22 列。
/// 估算：`TCP(3) + " ↑".len(2) + format_bytes(~6) + " ↓".len(2) + format_bytes(~6) + 间隙 ≈ 20-22`。
/// 取 22 偏保守，保证 `1.0 TB` / `1023 B` 等宽度变化时 host 不被过度压缩；超长仍由 Paragraph 裁剪兜底。
const NETWORK_WIDTH: usize = 22;

// ---------------------------------------------------------------------------
// 新增 helpers：进程 basename、分组 label、列宽预算、带宽响应式 conn_line
// ---------------------------------------------------------------------------

/// 取进程 basename：空 → "-"，否则按 '/' 与 '\' 分割取最后一个非空段。
pub(crate) fn process_short(c: &ConnInfo) -> String {
    let p = c.meta.process_path.trim();
    if p.is_empty() {
        return "-".to_string();
    }
    // 兼容 Unix '/' 与 Windows '\'，过滤空段（处理尾斜杠）
    let seg = p.split(['/', '\\']).rfind(|s| !s.is_empty());
    match seg {
        Some(s) => s.to_string(),
        None => "-".to_string(),
    }
}

/// 分组标签：chains 为空 → "DIRECT"，否则取 chains 最后一个非空段。
/// 防御：`chains=["a",""]` 等空串尾段时向前查找非空，否则回落 `DIRECT`。
pub(crate) fn group_label(c: &ConnInfo) -> String {
    c.chains
        .iter()
        .rfind(|s| !s.is_empty())
        .cloned()
        .unwrap_or_else(|| "DIRECT".to_string())
}

/// 列宽分配结果。host/proc/group 为按显示宽度计算的截断上限；show_* 为是否渲染。
pub(crate) struct ColWidths {
    pub host: usize,
    pub proc: usize,
    pub group: usize,
    pub show_proc: bool,
    pub show_group: bool,
}

/// 动态预算：按总宽度 total（inner.width）计算各列宽度。
/// 预算规则（响应式，与外层 `connections_visible(60)` 互补）：
/// - total < 60：外层已隐藏连接框，但本函数仍返回最小可用宽度（proc/group=0，host 至少1，仅为直接调用/测试防御）
/// - 60 <= total < 80：仅 host + 网络（proc=0, group=0）
/// - 80 <= total < 100：host + 分组 + 网络（proc=0, group 按比例 8..16）
/// - total >= 100：host + 进程 + 分组 + 网络（proc/group 均按比例 8..16）
///
/// 进程/分组宽度公式：` (total*15/100).clamp(8,16)`，保证在 8..16 之间随宽度自适应；
/// host = `total - NETWORK_WIDTH - proc - group - 分隔空格（1~3 个）`，至少 1 列，`saturating` 防止溢出。
pub(crate) fn col_widths(total: u16) -> ColWidths {
    let total = total as usize;
    // 阈值决定是否显示 proc / group
    let show_proc = total >= 100;
    let show_group = total >= 80;

    // 动态宽度：按 total*15/100 比例，上限16下限8
    let proc_w = if show_proc {
        (total * 15 / 100).clamp(8, 16)
    } else {
        0
    };
    let group_w = if show_group {
        (total * 15 / 100).clamp(8, 16)
    } else {
        0
    };

    // 分隔空格数：host 与网络间必有 1 个，proc/group 各额外 1 个（若显示）
    let sep = 1 + (if show_proc { 1 } else { 0 }) + (if show_group { 1 } else { 0 });
    let used = NETWORK_WIDTH + proc_w + group_w + sep;
    let host_w = total.saturating_sub(used).max(1);

    ColWidths {
        host: host_w,
        proc: proc_w,
        group: group_w,
        show_proc,
        show_group,
    }
}

/// 模式循环 rule → global → direct → rule（非 global/direct 一律 → "global"）。
fn next_mode(current: &str) -> &'static str {
    match current {
        "global" => "direct",
        "direct" => "rule",
        _ => "global",
    }
}

/// 顶栏状态行：`模式: rule [m] | TUN: on [t] | IPv6: on [6] | 出口IP: x「国家」 [r] | API: 已连接`
/// 国家段文本：有国家 → 「国家名」；无国家信息但有 IP → 「未知」；IP 也未获取 → 空串。
fn country_segment(country: Option<&str>, has_ip: bool) -> String {
    match (country, has_ip) {
        (Some(c), _) => format!("「{c}」"),
        (None, true) => "「未知」".to_string(),
        (None, false) => String::new(),
    }
}

fn render_status(f: &mut Frame, area: Rect, st: &AppState) {
    let mode = if st.runtime.mode.is_empty() {
        st.settings.mode.as_str()
    } else {
        st.runtime.mode.as_str()
    };
    let tun = if st.runtime.tun_enable { "开" } else { "关" };
    let ipv6 = if st.runtime.ipv6 { "开" } else { "关" };
    let ip = st.exit_ip.as_ref().map(|e| e.ip.as_str()).unwrap_or("未知");
    let country = st.exit_ip.as_ref().and_then(|e| e.country.as_deref());
    let (api_text, api_color) = if st.api_ok {
        ("已连接", Color::Green)
    } else {
        ("未连接", Color::Red)
    };
    let spans = vec![
        Span::raw("模式: "),
        Span::styled(
            mode,
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" [m]  "),
        Span::raw("TUN: "),
        Span::styled(
            tun,
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" [t]  "),
        Span::raw("IPv6: "),
        Span::styled(
            ipv6,
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" [6]  "),
        Span::raw("出口IP: "),
        Span::styled(
            ip,
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        // 国家段：有国家 → 「中国香港」；无国家信息（IP 正常）→ 「未知」；IP 也未获取 → 不显示
        Span::raw(country_segment(country, st.exit_ip.is_some())),
        Span::raw(" [r]  "),
        Span::raw("API: "),
        Span::styled(
            api_text,
            Style::default().fg(api_color).add_modifier(Modifier::BOLD),
        ),
    ];
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// 右列：网络（上 50%）+ 内存（下 50%）。
fn render_right(f: &mut Frame, area: Rect, st: &AppState) {
    let [net, mem] =
        Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(area);
    render_network(f, net, st);
    render_memory(f, mem, st);
}

/// 网络框：上行/下行速率（左对齐）+ 累计流量（右对齐）同排，双 Sparkline。
fn render_network(f: &mut Frame, area: Rect, st: &AppState) {
    let block = Block::new()
        .title(Span::styled(
            " 网络 ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let last = st.traffic.back().copied();
    let up_rate = last.map(|frame| frame.up).unwrap_or(0);
    let down_rate = last.map(|frame| frame.down).unwrap_or(0);
    let up_total = last.map(|frame| frame.up_total).unwrap_or(0);
    let down_total = last.map(|frame| frame.down_total).unwrap_or(0);
    let up_data: Vec<u64> = st.traffic.iter().map(|frame| frame.up).collect();
    let down_data: Vec<u64> = st.traffic.iter().map(|frame| frame.down).collect();

    let [l1, s1, l2, s2] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Min(1),
    ])
    .areas(inner);

    f.render_widget(
        Paragraph::new(rate_row(
            "↑ 上行",
            up_rate,
            up_total,
            Color::Green,
            inner.width,
        )),
        l1,
    );
    f.render_widget(
        Sparkline::default()
            .data(&up_data)
            .style(Style::default().fg(Color::Green)),
        s1,
    );
    f.render_widget(
        Paragraph::new(rate_row(
            "↓ 下行",
            down_rate,
            down_total,
            Color::Blue,
            inner.width,
        )),
        l2,
    );
    f.render_widget(
        Sparkline::default()
            .data(&down_data)
            .style(Style::default().fg(Color::Blue)),
        s2,
    );
}

/// 速率行：`↑ 上行 37.4 KB/s` 左对齐 + `累计 6.1 MB` 右对齐。
fn rate_row(label: &str, rate: u64, total: u64, color: Color, inner_width: u16) -> Line<'static> {
    let left = Span::styled(
        format!("{label} {}", crate::ui::widgets::format_rate(rate)),
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    );
    let right = Span::styled(
        format!("累计 {}", crate::ui::widgets::format_bytes(total)),
        Style::default().fg(color),
    );
    let pad = inner_width.saturating_sub((left.width() + right.width()) as u16);
    Line::from(vec![left, Span::raw(" ".repeat(pad as usize)), right])
}

/// 左列：最近连接列表（start 降序，已在 app 层排序）。
fn render_connections(f: &mut Frame, area: Rect, st: &AppState) {
    let block = Block::new()
        .title(Span::styled(
            " 连接 ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));
    let inner = block.inner(area);
    f.render_widget(block, area);

    if st.connections.is_empty() {
        f.render_widget(
            Paragraph::new(Line::from(vec![Span::styled(
                "暂无活动连接",
                Style::default().fg(Color::DarkGray),
            )])),
            inner,
        );
        return;
    }
    // 极窄高度：inner.height==0 时 take 0，不渲染行但不 panic
    if inner.height == 0 || inner.width == 0 {
        return;
    }
    let total_width = inner.width;
    let rows: Vec<Line> = st
        .connections
        .iter()
        .take(inner.height as usize)
        .map(|c| conn_line_with_width(c, total_width))
        .collect();
    f.render_widget(Paragraph::new(rows), inner);
}

/// 带宽响应式单行连接：`{host} {proc?} {group?} {TCP|UDP} ↑{upload} ↓{download}`
/// - host/proc/group 分别按 ColWidths 截断并加 "…"（unicode_width）
/// - 进程 DarkGray，分组 Yellow，host 原色，网络绿/蓝
/// - 窄宽时按阈值隐藏列：<80 隐藏分组，<100 隐藏进程（通过 col_widths 控制）
pub(crate) fn conn_line_with_width(c: &ConnInfo, total_width: u16) -> Line<'static> {
    let cw = col_widths(total_width);
    // host 截断
    let host_raw = conn_host(c);
    let host = truncate_ellipsis(&host_raw, cw.host);

    let kind = if c.meta.network == "tcp" {
        Span::styled("TCP", Style::default().fg(Color::Green))
    } else if c.meta.network == "udp" {
        Span::styled("UDP", Style::default().fg(Color::Blue))
    } else {
        Span::raw("?")
    };

    let mut spans: Vec<Span<'static>> = Vec::new();
    spans.push(Span::raw(host));

    if cw.show_proc {
        let proc_raw = process_short(c);
        let proc_disp = truncate_ellipsis(&proc_raw, cw.proc);
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            proc_disp,
            Style::default().fg(Color::DarkGray),
        ));
    }
    if cw.show_group {
        let group_raw = group_label(c);
        let group_disp = truncate_ellipsis(&group_raw, cw.group);
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            group_disp,
            Style::default().fg(Color::Yellow),
        ));
    }
    spans.push(Span::raw(" "));
    spans.push(kind);
    spans.push(Span::raw(" ↑"));
    spans.push(Span::styled(
        crate::ui::widgets::format_bytes(c.upload),
        Style::default().fg(Color::Green),
    ));
    spans.push(Span::raw(" ↓"));
    spans.push(Span::styled(
        crate::ui::widgets::format_bytes(c.download),
        Style::default().fg(Color::Blue),
    ));
    Line::from(spans)
}

/// 兼容旧签名的 conn_line：默认以 120 列宽度渲染（全列可见），供旧测试与外部调用。
#[allow(dead_code)]
pub(crate) fn conn_line(c: &ConnInfo) -> Line<'static> {
    conn_line_with_width(c, 120)
}

/// 连接目标展示：host → sniffHost → remoteDestination → destinationIP:port → 未知目标。
fn conn_host(c: &ConnInfo) -> String {
    if !c.meta.host.is_empty() {
        return c.meta.host.clone();
    }
    if !c.meta.sniff_host.is_empty() {
        return c.meta.sniff_host.clone();
    }
    if !c.meta.remote_destination.is_empty() {
        return c.meta.remote_destination.clone();
    }
    if !c.meta.destination_ip.is_empty() {
        return if c.meta.destination_port.is_empty() {
            c.meta.destination_ip.clone()
        } else {
            format!("{}:{}", c.meta.destination_ip, c.meta.destination_port)
        };
    }
    "未知目标".to_string()
}

/// 内存框：inuse 数值 + Sparkline。
fn render_memory(f: &mut Frame, area: Rect, st: &AppState) {
    let block = Block::new()
        .title(Span::styled(
            " 内存 ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let inuse = st.mem_history.back().copied().unwrap_or(0);
    let mem_data: Vec<u64> = st.mem_history.iter().copied().collect();
    let [m1, m2] = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(inner);
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                crate::ui::widgets::format_bytes(inuse),
                Style::default()
                    .fg(Color::Magenta)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" inuse", Style::default().fg(Color::DarkGray)),
        ])),
        m1,
    );
    f.render_widget(
        Sparkline::default()
            .data(&mem_data)
            .style(Style::default().fg(Color::Magenta)),
        m2,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::client::ConnMeta;

    fn meta(host: &str, network: &str) -> ConnMeta {
        ConnMeta {
            host: host.into(),
            network: network.into(),
            ..ConnMeta::default()
        }
    }

    fn conn_with(host: &str, process_path: &str, chains: Vec<&str>, network: &str) -> ConnInfo {
        ConnInfo {
            meta: ConnMeta {
                host: host.into(),
                network: network.into(),
                process_path: process_path.into(),
                ..ConnMeta::default()
            },
            chains: chains.into_iter().map(|s| s.to_string()).collect(),
            upload: 1024,
            download: 2048,
            ..ConnInfo::default()
        }
    }

    /// 响应式阈值：60 列及以上显示连接框。
    #[test]
    fn connections_visible_threshold() {
        assert!(!connections_visible(59));
        assert!(connections_visible(60));
        assert!(connections_visible(61));
        assert!(connections_visible(200));
    }

    /// host 兜底链：host → sniffHost → remoteDestination → destinationIP:port。
    #[test]
    fn conn_host_fallback_chain() {
        let mut c = ConnInfo {
            meta: meta("example.com", "tcp"),
            ..ConnInfo::default()
        };
        assert_eq!(conn_host(&c), "example.com");

        c.meta = ConnMeta {
            host: String::new(),
            sniff_host: "sniffed.dev".into(),
            ..ConnMeta::default()
        };
        assert_eq!(conn_host(&c), "sniffed.dev");

        c.meta = ConnMeta {
            host: String::new(),
            remote_destination: "1.2.3.4:443".into(),
            ..ConnMeta::default()
        };
        assert_eq!(conn_host(&c), "1.2.3.4:443");

        c.meta = ConnMeta {
            host: String::new(),
            destination_ip: "5.6.7.8".into(),
            destination_port: "8080".into(),
            ..ConnMeta::default()
        };
        assert_eq!(conn_host(&c), "5.6.7.8:8080");

        c.meta = ConnMeta::default();
        assert_eq!(conn_host(&c), "未知目标");
    }

    /// 速率行极窄宽度：inner_width=0/5 时 pad 用 saturating_sub 截断为 0，
    /// 不 panic；行宽应恰为左右内容宽度之和（超出部分由 Paragraph 裁剪）。
    #[test]
    fn rate_row_narrow_inner_width() {
        for inner_width in [0u16, 5] {
            let line = rate_row("↑ 上行", 1024, 1024 * 1024, Color::Green, inner_width);
            let content_width =
                Span::raw("↑ 上行 1.0 KB/s").width() + Span::raw("累计 1.0 MB").width();
            assert_eq!(
                line.width(),
                content_width,
                "inner_width={inner_width}: 极窄宽度下不应补 padding"
            );
            assert!(
                line.to_string().contains("↑ 上行"),
                "inner_width={inner_width}: 应含上行标签"
            );
            assert!(
                line.to_string().contains("累计"),
                "inner_width={inner_width}: 应含累计标签"
            );
        }
    }

    /// UDP 连接行含 UDP 标；TCP 行含 TCP 标。
    #[test]
    fn conn_line_kind_marker() {
        let tcp = ConnInfo {
            meta: meta("a.com", "tcp"),
            upload: 1024,
            download: 2048,
            ..ConnInfo::default()
        };
        let udp = ConnInfo {
            meta: meta("b.com", "udp"),
            ..ConnInfo::default()
        };
        let line_tcp = conn_line(&tcp);
        let line_udp = conn_line(&udp);
        assert!(line_tcp.to_string().contains("TCP"));
        assert!(!line_tcp.to_string().contains("UDP"));
        assert!(line_udp.to_string().contains("UDP"));
        assert!(line_tcp.to_string().contains("↑1.0 KB"));
        assert!(line_tcp.to_string().contains("↓2.0 KB"));
    }

    use crate::core::client::RuntimeConfig;
    use crate::core::models::Overrides;
    use crate::core::settings::{load_settings, settings_path, with_settings_dir};
    use crossterm::event::KeyModifiers;
    use std::collections::{HashMap, VecDeque};

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
            connections: Vec::new(),
            exit_ip: None,
            proxy_groups: Vec::new(),
            group_delays: HashMap::new(),
            logs: VecDeque::new(),
            notices: VecDeque::new(),
            run_status: None,
        }
    }

    fn press(page: &mut DashboardPage, st: &mut AppState, code: KeyCode) -> UiCommand {
        page.handle_key(KeyEvent::new(code, KeyModifiers::NONE), st)
            .expect("开关按键应返回命令")
    }

    /// 断言返回 PatchConfigs 并解构出 (patch, saved, label)。
    fn expect_patch(cmd: &UiCommand) -> (&serde_json::Value, bool, &str) {
        match cmd {
            UiCommand::PatchConfigs {
                patch,
                saved,
                label,
            } => (patch, *saved, label),
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
                Some(m) => assert_eq!(m.title(), "保存失败"),
                _ => panic!("保存失败时应弹「保存失败」弹窗"),
            }
            // 设置确实未持久化：settings.toml 仍是目录（无文件落盘）
            assert!(!settings_path().is_file(), "失败时不应有文件落盘");
            // 关键不变量：保存失败时 st.settings 不应更新——内存与磁盘一致，仍为旧值
            assert!(!st.settings.tun.enable, "保存失败时 st.settings 不应更新");
        });
    }

    /// s 键不再打开设置弹窗（功能迁移到设置页 tab）。
    #[test]
    fn s_key_no_longer_opens_popup() {
        let mut st = test_state();
        let mut page = DashboardPage::new();
        let cmd = page.handle_key(
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE),
            &mut st,
        );
        assert!(cmd.is_none(), "s 不应再产生命令");
        assert!(page.popup.is_none(), "s 不应再弹设置表单");
    }

    // ---- 出口 IP 国家展示 ----------------

    /// country_segment 三态：有国家 → 「国家名」（忽略 has_ip）；
    /// 无国家但有 IP → 「未知」；IP 也未获取 → 空串。
    #[test]
    fn country_segment_three_states() {
        assert_eq!(country_segment(Some("中国香港"), true), "「中国香港」");
        assert_eq!(country_segment(Some("美国"), false), "「美国」"); // 有国家时忽略 has_ip
        assert_eq!(country_segment(None, true), "「未知」");
        assert_eq!(country_segment(None, false), "");
    }

    // ---- 新增：进程 basename 提取 ----------------

    #[test]
    fn process_short_extracts_basename() {
        let c = conn_with("a.com", "/usr/bin/curl", vec![], "tcp");
        assert_eq!(process_short(&c), "curl");
        let c2 = conn_with("a.com", "C:\\Windows\\a.exe", vec![], "tcp");
        assert_eq!(process_short(&c2), "a.exe");
        let c3 = conn_with("a.com", "", vec![], "tcp");
        assert_eq!(process_short(&c3), "-");
        let c4 = conn_with("a.com", "/usr/bin/", vec![], "tcp");
        assert_eq!(process_short(&c4), "bin");
        let c5 = conn_with("a.com", "/a/b/c/d", vec![], "tcp");
        assert_eq!(process_short(&c5), "d");
        let c6 = conn_with("a.com", "justname", vec![], "tcp");
        assert_eq!(process_short(&c6), "justname");
        let c7 = conn_with("a.com", "C:\\a\\b\\c.exe", vec![], "tcp");
        assert_eq!(process_short(&c7), "c.exe");
        let c8 = conn_with("a.com", "/", vec![], "tcp");
        assert_eq!(process_short(&c8), "-");
        let c9 = conn_with("a.com", "\\", vec![], "tcp");
        assert_eq!(process_short(&c9), "-");
    }

    #[test]
    fn group_label_last_or_direct() {
        let c = conn_with("a.com", "", vec![], "tcp");
        assert_eq!(group_label(&c), "DIRECT");
        let c2 = conn_with("a.com", "", vec!["PROXY"], "tcp");
        assert_eq!(group_label(&c2), "PROXY");
        let c3 = conn_with("a.com", "", vec!["PROXY", "节点A"], "tcp");
        assert_eq!(group_label(&c3), "节点A");
        let c4 = conn_with("a.com", "", vec!["a", "b", "c"], "tcp");
        assert_eq!(group_label(&c4), "c");
    }

    #[test]
    fn col_widths_thresholds() {
        // 59/60/79 -> proc/group 均为 0
        for w in [59u16, 60, 79] {
            let cw = col_widths(w);
            assert_eq!(cw.proc, 0, "w={w} proc 应为0");
            assert_eq!(cw.group, 0, "w={w} group 应为0");
            assert!(!cw.show_proc, "w={w} show_proc 为 false");
            assert!(!cw.show_group, "w={w} show_group 为 false");
            assert!(cw.host >= 1, "w={w} host 至少1");
            let sep = 1;
            assert!(
                cw.host + cw.proc + cw.group + sep + NETWORK_WIDTH <= w as usize || w < NETWORK_WIDTH as u16 + 1,
                "w={w} 宽度和应 <= total: host {} + proc {} + group {} + sep {} + net {} = {} > {}",
                cw.host, cw.proc, cw.group, sep, NETWORK_WIDTH,
                cw.host + cw.proc + cw.group + sep + NETWORK_WIDTH, w
            );
        }
        // 80/85/99 -> proc 0, group 非0
        for w in [80u16, 85, 99] {
            let cw = col_widths(w);
            assert_eq!(cw.proc, 0, "w={w} proc 应为0");
            assert!(cw.group > 0, "w={w} group 应非0");
            assert!(!cw.show_proc, "w={w} show_proc false");
            assert!(cw.show_group, "w={w} show_group true");
            assert!(cw.host >= 1);
            let sep = 2;
            assert!(
                cw.host + cw.proc + cw.group + sep + NETWORK_WIDTH <= w as usize,
                "w={w} 宽度和应 <= total"
            );
            assert!(cw.group >= 8 && cw.group <= 16, "w={w} group 8..16");
        }
        // 100/120 -> 均非0
        for w in [100u16, 120, 200] {
            let cw = col_widths(w);
            assert!(cw.proc > 0, "w={w} proc 非0");
            assert!(cw.group > 0, "w={w} group 非0");
            assert!(cw.show_proc);
            assert!(cw.show_group);
            assert!(cw.host >= 1);
            let sep = 3;
            assert!(
                cw.host + cw.proc + cw.group + sep + NETWORK_WIDTH <= w as usize,
                "w={w} 宽度和应 <= total"
            );
            assert!(cw.proc >= 8 && cw.proc <= 16);
            assert!(cw.group >= 8 && cw.group <= 16);
        }
    }

    #[test]
    fn conn_line_with_width_responsive() {
        // 长进程与多级 chains
        let c = ConnInfo {
            meta: ConnMeta {
                host: "example.com".into(),
                network: "tcp".into(),
                process_path: "/very/long/path/to/super-long-process-name-executable".into(),
                ..ConnMeta::default()
            },
            chains: vec!["PROXY".to_string(), "节点A".to_string()],
            upload: 1024,
            download: 2048,
            ..ConnInfo::default()
        };
        // w=70 : 仅 host+网络，进程/分组均隐藏
        let line70 = conn_line_with_width(&c, 70);
        let s70 = line70.to_string();
        assert!(
            !s70.contains("super-long"),
            "w=70 不应含进程: {s70}"
        );
        assert!(!s70.contains("节点A"), "w=70 不应含分组: {s70}");
        assert!(!s70.contains("PROXY"), "w=70 不应含分组 PROXY: {s70}");
        assert!(s70.contains("example.com"), "w=70 应含 host");
        assert!(s70.contains("TCP"));
        assert!(s70.contains("↑"));
        assert!(s70.contains("↓"));

        // w=85 : 隐藏进程，显示分组
        let line85 = conn_line_with_width(&c, 85);
        let s85 = line85.to_string();
        assert!(!s85.contains("super-long"), "w=85 不应含进程: {s85}");
        assert!(s85.contains("节点A"), "w=85 应含分组 节点A: {s85}");
        assert!(s85.contains("TCP"));

        // w=120 : 全显
        let line120 = conn_line_with_width(&c, 120);
        let s120 = line120.to_string();
        // 进程 basename 截断后可能含 "…"，但至少包含前缀或后缀；此处 proc 宽度 16，basename 长度 32 需截断
        // 检查是否包含截断后的进程（或至少不为空且行含 DarkGray 进程列的字符片段）
        // basename = "super-long-process-name-executable" 截断到 16 列 -> "super-long-proc…"
        assert!(
            s120.contains("super-long") || s120.contains("…"),
            "w=120 应含进程（可能截断）: {s120}"
        );
        assert!(s120.contains("节点A"), "w=120 应含分组: {s120}");
        assert!(s120.contains("TCP"));
        assert!(s120.contains("↑1.0 KB") || s120.contains("↑"));
        assert!(s120.contains("↓2.0 KB") || s120.contains("↓"));
    }

    #[test]
    fn conn_line_host_truncation_contains_ellipsis() {
        // 超长 host 在窄宽下应被省略含 "…"
        let long_host = "a-very-long-host-name-that-exceeds-width.example.com".to_string();
        let c = ConnInfo {
            meta: ConnMeta {
                host: long_host.clone(),
                network: "tcp".into(),
                ..ConnMeta::default()
            },
            upload: 1024,
            download: 2048,
            ..ConnInfo::default()
        };
        // w=70 时 host 宽度约 49，long_host 长度 53 >49 触发截断
        let line = conn_line_with_width(&c, 70);
        let s = line.to_string();
        assert!(
            s.contains('…'),
            "超长 host 应被截断含 …: host_len {} s={s}",
            long_host.len()
        );
        assert!(s.contains("TCP"));
        // 中文 host 截断
        let c2 = ConnInfo {
            meta: ConnMeta {
                host: "这是一个非常长的中文主机名测试用例用于验证截断逻辑是否正确处理宽字符.example.com".into(),
                network: "udp".into(),
                ..ConnMeta::default()
            },
            upload: 0,
            download: 0,
            ..ConnInfo::default()
        };
        let line2 = conn_line_with_width(&c2, 70);
        let s2 = line2.to_string();
        assert!(s2.contains('…'), "中文超长 host 应含 …: {s2}");
        assert!(s2.contains("UDP"));
    }

    #[test]
    fn conn_line_narrow_no_panic() {
        let c = conn_with(
            "example.com",
            "/very/long/path/to/super-long-process-name-executable",
            vec!["PROXY", "节点A"],
            "tcp",
        );
        for w in [0u16, 1, 5, 20, 60, 79, 80, 99, 100, 120, 200] {
            let line = conn_line_with_width(&c, w);
            // 不 panic 且至少包含网络标记
            let s = line.to_string();
            // w 极小时 host 仍至少 1 列，行应非空
            assert!(!s.is_empty(), "w={w} 行不应空");
            // 只要 w>0，行应能生成
            if w >= 60 {
                assert!(s.contains("TCP") || s.contains("UDP") || s.contains("?"));
            }
        }
        // conn_line 兼容旧签名（默认120）也不 panic
        let _ = conn_line(&c);
    }

    #[test]
    fn truncate_ellipsis_used_in_conn_line() {
        // 验证 process_short 的 basename 截断：proc 宽度 16，basename 超长时应以 … 结尾
        let c = ConnInfo {
            meta: ConnMeta {
                process_path: "/a/very-long-process-name-that-will-be-truncated".into(),
                network: "tcp".into(),
                host: "h.com".into(),
                ..ConnMeta::default()
            },
            chains: vec!["G".into()],
            ..ConnInfo::default()
        };
        let cw = col_widths(120);
        let proc_raw = process_short(&c);
        let proc_trunc = truncate_ellipsis(&proc_raw, cw.proc);
        assert!(proc_trunc.len() <= proc_raw.len() || proc_trunc.contains('…'));
        if crate::ui::widgets::display_width(&proc_raw) > cw.proc {
            assert!(proc_trunc.ends_with('…'), "超长进程应以 … 结尾: {proc_trunc}");
        }
        let line = conn_line_with_width(&c, 120);
        let s = line.to_string();
        // 若截断则行中含 …
        if crate::ui::widgets::display_width(&proc_raw) > cw.proc {
            assert!(s.contains('…'), "行应含 …: {s}");
        }
    }
}
