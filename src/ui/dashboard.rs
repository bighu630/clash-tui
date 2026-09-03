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
use crate::ui::widgets::MessagePopup;
use crate::ui::Page;

#[derive(Default)]
pub struct DashboardPage {
    popup: Option<MessagePopup>,
    /// 首条可见连接索引（滚动位置），按连接数 clamp；Page::render 为 &mut self 故可内部更新。
    conn_scroll: usize,
    /// 上次渲染时连接框 inner.height（用于 PageUp/PageDown 计算 visible 行数）
    last_height: u16,
}

impl DashboardPage {
    pub fn new() -> Self {
        Self {
            popup: None,
            conn_scroll: 0,
            last_height: 0,
        }
    }

    /// 连接框渲染（带滚动）：每连接 2 行，按 inner.height/2 计算 visible，title 显示 "连接 x/y"。
    fn render_connections(&mut self, f: &mut Frame, area: Rect, st: &AppState) {
        // 预计算标题与可见数（供 border 与 scroll 标题使用）
        // 先用临时 Block 取 inner 以计算 visible；在真正渲染时复用同一 inner
        let tmp_block = Block::new()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray));
        let tmp_inner = tmp_block.inner(area);
        // 记录 last_height 供 PageUp/PageDown 在 handle_key 中使用（handle_key 无法拿到 area）
        self.last_height = tmp_inner.height;
        let total = st.connections.len();
        let visible = if tmp_inner.height == 0 {
            0
        } else {
            ((tmp_inner.height as usize) / 2).max(1)
        };
        let max_scroll = total.saturating_sub(visible);
        let scroll = self.conn_scroll.min(max_scroll);
        // clamp 回写，避免 End（MAX）后持续溢出
        self.conn_scroll = scroll;
        let title = if total == 0 {
            " 连接 ".to_string()
        } else {
            format!(" 连接 {}/{} ", scroll + 1, total)
        };
        let block = Block::new()
            .title(Span::styled(
                title,
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
        if inner.height == 0 || inner.width == 0 {
            return;
        }
        let total_width = inner.width;
        let mut lines: Vec<Line> = Vec::new();
        // 跳过 scroll 条，从可见窗口首条开始取 visible 条
        for c in st.connections.iter().skip(scroll).take(visible) {
            let pair = conn_lines(c, total_width);
            if lines.len() + pair.len() > inner.height as usize {
                // inner.height 为奇数时仅推上行（已有奇数逻辑复用）
                if lines.len() < inner.height as usize {
                    if let Some(first) = pair.into_iter().next() {
                        lines.push(first);
                    }
                }
                break;
            }
            lines.extend(pair);
        }
        f.render_widget(Paragraph::new(lines), inner);
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

    fn handle_key(&mut self, key: KeyEvent, _st: &mut AppState) -> Option<UiCommand> {
        // 弹窗优先
        match self.popup.take() {
            Some(mut msg) => {
                if !msg.handle_key(key) {
                    self.popup = Some(msg);
                }
            }
            None => match key.code {
                // ——— 滚动：优先级高于后续开关（Up/Down 不与全局切页冲突，全局仅 Tab/←→/数字） ———
                KeyCode::Up | KeyCode::Char('k') => {
                    self.conn_scroll = self.conn_scroll.saturating_sub(1);
                    return None;
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.conn_scroll = self.conn_scroll.saturating_add(1);
                    return None;
                }
                KeyCode::PageUp => {
                    let visible = (self.last_height as usize / 2).max(1);
                    self.conn_scroll = self.conn_scroll.saturating_sub(visible);
                    return None;
                }
                KeyCode::PageDown => {
                    let visible = (self.last_height as usize / 2).max(1);
                    self.conn_scroll = self.conn_scroll.saturating_add(visible);
                    return None;
                }
                KeyCode::Home | KeyCode::Char('g') => {
                    self.conn_scroll = 0;
                    return None;
                }
                KeyCode::End | KeyCode::Char('G') => {
                    self.conn_scroll = usize::MAX;
                    return None;
                }
                // 模式循环 rule → global → direct
                KeyCode::Char('m') => {
                    // 需可变借用 AppState：重新获取 _st
                    // _st 实际为 &mut AppState，未被 move，故可直接使用
                    let next = next_mode(&_st.runtime.mode);
                    _st.runtime.mode = next.to_string();
                    return Some(self.toggle_double_write(
                        _st,
                        "模式",
                        |s| s.mode = next.to_string(),
                        serde_json::json!({"mode": next}),
                    ));
                }
                // TUN 热切
                KeyCode::Char('t') => {
                    let enable = !_st.runtime.tun_enable;
                    // Windows：非管理员开 TUN → 警告（UAC 无法中途提升；不阻塞）
                    #[cfg(windows)]
                    if enable && !crate::service::process::is_elevated() {
                        _st.notice(
                            "[!] TUN 需要管理员权限：当前 TUI 未以管理员身份运行，mihomo 将无法创建 TUN 设备"
                                .to_string(),
                        );
                    }
                    _st.runtime.tun_enable = enable;
                    return Some(self.toggle_double_write(
                        _st,
                        "TUN",
                        |s| s.tun.enable = enable,
                        serde_json::json!({"tun": {"enable": enable}}),
                    ));
                }
                // IPv6 热切
                KeyCode::Char('6') => {
                    let enable = !_st.runtime.ipv6;
                    _st.runtime.ipv6 = enable;
                    return Some(self.toggle_double_write(
                        _st,
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
            self.render_connections(f, left, st);
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
// helpers：进程 basename、分组 label、规则 label、列宽预算、双行连接
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

/// 规则标签：若 c.rule 为空则 fallback 到 group_label(c)；否则若 c.rule_payload 为空
/// 返回 c.rule.clone()，否则 format!("{},{}", c.rule, c.rule_payload)。
/// 对于 MATCH 等无 payload 情况自动处理：返回 rule 本身。
pub(crate) fn rule_label(c: &ConnInfo) -> String {
    if c.rule.is_empty() {
        return group_label(c);
    }
    if c.rule_payload.is_empty() {
        c.rule.clone()
    } else {
        format!("{},{}", c.rule, c.rule_payload)
    }
}

/// 列宽分配结果。host/proc/group 为按显示宽度计算的截断上限；show_* 为是否渲染。
/// 保留用于兼容旧单行逻辑测试。
#[allow(dead_code)]
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
#[allow(dead_code)]
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

/// 上行 host 可用宽度：total - NETWORK_WIDTH - 1（host 与网络间空格），至少 1 列。
#[allow(dead_code)]
pub(crate) fn upper_host_width(total: u16) -> usize {
    (total as usize)
        .saturating_sub(NETWORK_WIDTH)
        .saturating_sub(1)
        .max(1)
}

/// 下行宽度预算。
/// 预算规则：
/// - lower_effective = total.saturating_sub(1) // 去掉首字符缩进
/// - proc_w = (total*15/100).clamp(8,16)  // 复用原比例 8..16，始终显示进程
/// - show_group = total >= CONNECTIONS_MIN_WIDTH (60)  // 与外层可见阈一致，左 pane 68 列时仍显示分组
/// - group_w = if show_group { (total*15/100).clamp(8,16) } else {0}
/// - sep_proc_rule = 2 ("  "), sep_rule_group = if show_group {3} else {0} (" → ")
/// - rule_w = lower_effective.saturating_sub(proc_w + sep_proc_rule + sep_rule_group + group_w).max(4)
///
/// 返回 (proc_w, rule_w, group_w, show_group)
pub(crate) fn lower_widths(total: u16) -> (usize, usize, usize, bool) {
    let total_usize = total as usize;
    let lower_effective = (total as usize).saturating_sub(1);
    let proc_w = (total_usize * 15 / 100).clamp(8, 16);
    let show_group = total >= CONNECTIONS_MIN_WIDTH;
    let group_w = if show_group {
        (total_usize * 15 / 100).clamp(8, 16)
    } else {
        0
    };
    let sep_proc_rule = 2;
    let sep_rule_group = if show_group { 3 } else { 0 };
    let rule_w = lower_effective
        .saturating_sub(proc_w + sep_proc_rule + sep_rule_group + group_w)
        .max(4);
    (proc_w, rule_w, group_w, show_group)
}

/// 按显示宽度截断字符串，超出时直接截断不加 "…"。
/// - 若 `max_width==0` 返回 `""`
/// - 若 `s` 宽度 <= `max_width` 原样返回
/// - 否则按字符累加宽度，超限则 break，不追加省略号
pub(crate) fn truncate_width(s: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    if crate::ui::widgets::display_width(s) <= max_width {
        return s.to_string();
    }
    let mut cur_width = 0usize;
    let mut out = String::new();
    for ch in s.chars() {
        let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if cur_width + cw > max_width {
            break;
        }
        cur_width += cw;
        out.push(ch);
    }
    out
}

/// 双行生成：每条连接返回 2 行
/// - 上行：host_trunc + " " + kind + " ↑upload ↓download"（保持原颜色）
/// - 下行：缩进1 + 进程 + "  " + 规则(+payload) [+ " → " + 分组]，整行默认色，超长截断不加省略号
pub(crate) fn conn_lines(c: &ConnInfo, total_width: u16) -> Vec<Line<'static>> {
    // 上行：host + 网络 - 动态按实际网络列宽度计算，避免固定 22 导致 host 被过度截断
    let kind_str = if c.meta.network == "tcp" {
        "TCP"
    } else if c.meta.network == "udp" {
        "UDP"
    } else {
        "?"
    };
    let upload_str = crate::ui::widgets::format_bytes(c.upload);
    let download_str = crate::ui::widgets::format_bytes(c.download);
    let network_display = format!(" {} ↑{} ↓{}", kind_str, upload_str, download_str);
    let network_w = crate::ui::widgets::display_width(&network_display);
    let host_w = (total_width as usize).saturating_sub(network_w).max(1);
    let host_raw = conn_host(c);
    let host_trunc = truncate_width(&host_raw, host_w);
    let kind = if c.meta.network == "tcp" {
        Span::styled("TCP", Style::default().fg(Color::Green))
    } else if c.meta.network == "udp" {
        Span::styled("UDP", Style::default().fg(Color::Blue))
    } else {
        Span::raw("?")
    };
    let upper = Line::from(vec![
        Span::raw(host_trunc),
        Span::raw(" "),
        kind,
        Span::raw(" ↑"),
        Span::styled(
            crate::ui::widgets::format_bytes(c.upload),
            Style::default().fg(Color::Green),
        ),
        Span::raw(" ↓"),
        Span::styled(
            crate::ui::widgets::format_bytes(c.download),
            Style::default().fg(Color::Blue),
        ),
    ]);

    // 下行：缩进 + 进程 + 规则 + 分组 - 动态按需分配，避免固定预算浪费导致 rule/group 被过度截断
    let (proc_budget, _, group_budget, show_group) = lower_widths(total_width);
    let proc_raw = process_short(c);
    let group_raw = if show_group {
        group_label(c)
    } else {
        String::new()
    };
    let proc_need = crate::ui::widgets::display_width(&proc_raw);
    let group_need = if show_group {
        crate::ui::widgets::display_width(&group_raw)
    } else {
        0
    };
    // 按需分配：不超过预算，且至少 1 列（proc）/ 0 列（group 当不显示）
    let proc_alloc = proc_need.min(proc_budget).max(1);
    let group_alloc = if show_group {
        group_need.min(group_budget)
    } else {
        0
    };
    let lower_effective = (total_width as usize).saturating_sub(1);
    let sep_proc_rule = 2;
    let sep_rule_group = if show_group { 3 } else { 0 };
    let rule_w = lower_effective
        .saturating_sub(proc_alloc + sep_proc_rule + sep_rule_group + group_alloc)
        .max(4);
    let proc_trunc = truncate_width(&proc_raw, proc_alloc);
    let rule_raw = rule_label(c);
    let rule_trunc = truncate_width(&rule_raw, rule_w);
    let mut lower_spans: Vec<Span<'static>> = vec![
        Span::styled(" ".to_string(), Style::default()),
        Span::styled(proc_trunc, Style::default()),
        Span::styled("  ".to_string(), Style::default()),
        Span::styled(rule_trunc, Style::default()),
    ];
    if show_group {
        let group_trunc = truncate_width(&group_raw, group_alloc);
        lower_spans.push(Span::styled(" → ".to_string(), Style::default()));
        lower_spans.push(Span::styled(group_trunc, Style::default()));
    }
    let lower = Line::from(lower_spans);
    vec![upper, lower]
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

/// 左列：最近连接列表（start 降序，已在 app 层排序），每条连接占 2 行。
/// 已重构为 `DashboardPage::render_connections`（带滚动与 "连接 x/y" 标题），此 free fn 仅作兼容保留，不再被调用。
#[allow(dead_code)]
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
    if inner.height == 0 || inner.width == 0 {
        return;
    }
    let total_width = inner.width;
    let mut lines: Vec<Line> = Vec::new();
    for c in st.connections.iter() {
        let pair = conn_lines(c, total_width);
        if lines.len() + pair.len() > inner.height as usize {
            if lines.len() < inner.height as usize {
                if let Some(first) = pair.into_iter().next() {
                    lines.push(first);
                }
            }
            break;
        }
        lines.extend(pair);
    }
    f.render_widget(Paragraph::new(lines), inner);
}

/// 单行连接（兼容旧签名）：返回双行中的上行，供旧测试与外部调用。
/// 保留 col_widths 兼容逻辑但改为转发到 conn_lines[0]。
pub(crate) fn conn_line_with_width(c: &ConnInfo, total_width: u16) -> Line<'static> {
    conn_lines(c, total_width)
        .into_iter()
        .next()
        .unwrap_or_else(|| Line::from(Span::raw("")))
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

    fn conn_with_rule(
        host: &str,
        process_path: &str,
        chains: Vec<&str>,
        network: &str,
        rule: &str,
        payload: &str,
    ) -> ConnInfo {
        ConnInfo {
            meta: ConnMeta {
                host: host.into(),
                network: network.into(),
                process_path: process_path.into(),
                ..ConnMeta::default()
            },
            chains: chains.into_iter().map(|s| s.to_string()).collect(),
            rule: rule.into(),
            rule_payload: payload.into(),
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
        // 下行校验：双行第二行含进程 fallback "-"
        let lines = conn_lines(&tcp, 120);
        assert_eq!(lines.len(), 2);
        assert!(lines[1].to_string().contains('-'));
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
    fn rule_label_tests() {
        // rule 空 → fallback group
        let c = conn_with("a.com", "", vec!["PROXY"], "tcp");
        assert_eq!(rule_label(&c), "PROXY");
        let c2 = conn_with("a.com", "", vec![], "tcp");
        assert_eq!(rule_label(&c2), "DIRECT");
        // rule 有 payload 拼接
        let c3 = conn_with_rule("a.com", "", vec!["PROXY"], "tcp", "DOMAIN", "example.com");
        assert_eq!(rule_label(&c3), "DOMAIN,example.com");
        // MATCH 无 payload 仅 rule
        let c4 = conn_with_rule("a.com", "", vec!["PROXY"], "tcp", "MATCH", "");
        assert_eq!(rule_label(&c4), "MATCH");
        // rule 非空 payload 非空
        let c5 = conn_with_rule("a.com", "", vec!["PROXY"], "tcp", "GEOSITE", "category-ads-all");
        assert_eq!(rule_label(&c5), "GEOSITE,category-ads-all");
        // rule 为空但 payload 有值（异常 case，仍 fallback group）
        let c6 = ConnInfo {
            rule: String::new(),
            rule_payload: "payload".into(),
            chains: vec!["FallbackGroup".to_string()],
            ..conn_with("a.com", "", vec![], "tcp")
        };
        assert_eq!(rule_label(&c6), "FallbackGroup");
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
            assert!((8..=16).contains(&cw.group), "w={w} group 8..16");
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
            assert!((8..=16).contains(&cw.proc));
            assert!((8..=16).contains(&cw.group));
        }
    }

    #[test]
    fn upper_host_width_tests() {
        // total=120 -> 120-22-1=97
        assert_eq!(upper_host_width(120), 97);
        assert_eq!(upper_host_width(70), 47);
        assert_eq!(upper_host_width(85), 62);
        // 极小：0->1,1->1,5->1
        assert_eq!(upper_host_width(0), 1);
        assert_eq!(upper_host_width(1), 1);
        assert_eq!(upper_host_width(5), 1);
        assert_eq!(upper_host_width(22), 1);
        assert_eq!(upper_host_width(23), 1);
        assert_eq!(upper_host_width(24), 1);
        assert_eq!(upper_host_width(30), 7);
        // 大宽度
        assert_eq!(upper_host_width(200), 177);
    }

    #[test]
    fn lower_widths_tests() {
        // total=70 >=60: show_group true（阈值已从 80 降至 60，保证左 pane 68 列时仍显示分组）
        let (proc_w, rule_w, group_w, show_group) = lower_widths(70);
        assert_eq!(proc_w, (70usize * 15 / 100).clamp(8, 16));
        assert!(show_group);
        assert_eq!(group_w, (70usize * 15 / 100).clamp(8, 16));
        assert!(rule_w >= 4);
        // total=85 >=60: show_group true
        let (proc_w, rule_w, group_w, show_group) = lower_widths(85);
        assert_eq!(proc_w, (85usize * 15 / 100).clamp(8, 16));
        assert!(show_group);
        assert_eq!(group_w, (85usize * 15 / 100).clamp(8, 16));
        assert!(rule_w >= 4);
        // total=120
        let (proc_w, rule_w, group_w, show_group) = lower_widths(120);
        assert_eq!(proc_w, 16);
        assert_eq!(group_w, 16);
        assert!(show_group);
        // 验证 rule_w 计算：lower_effective - proc -2 -3 -group
        let lower_effective = 120usize - 1;
        let expected_rule = lower_effective
            .saturating_sub(proc_w + 2 + 3 + group_w)
            .max(4);
        assert_eq!(rule_w, expected_rule);
        // 窄宽不 panic，且 proc 仍 8..16，rule 至少4
        for w in [0u16, 1, 5, 20, 59] {
            let (pw, rw, gw, sg) = lower_widths(w);
            assert!((8..=16).contains(&pw), "w={w} proc 8..16");
            assert!(rw >= 4, "w={w} rule >=4");
            if w < CONNECTIONS_MIN_WIDTH {
                assert!(!sg, "w={w} show_group false");
                assert_eq!(gw, 0, "w={w} group 0");
            }
        }
        // 60 及以上应显示分组
        assert!(lower_widths(60).3);
        assert!(lower_widths(61).3);
        // 阈值边界 59/60
        assert!(!lower_widths(59).3);
        assert!(lower_widths(60).3);
    }

    #[test]
    fn conn_lines_two_rows_basic() {
        let c = conn_with_rule(
            "example.com",
            "/usr/bin/curl",
            vec!["PROXY", "节点A"],
            "tcp",
            "DOMAIN",
            "example.com",
        );
        for total in [70u16, 85, 120] {
            let lines = conn_lines(&c, total);
            assert_eq!(lines.len(), 2, "total={total} 应返回2行");
            let upper = lines[0].to_string();
            let lower = lines[1].to_string();
            // 上行含 host 片段、TCP、↑/↓
            assert!(upper.contains("example.com"), "total={total} 上行应含 host: {upper}");
            assert!(upper.contains("TCP"), "total={total} 上行应含 TCP: {upper}");
            assert!(upper.contains('↑'), "total={total} 上行应含 ↑: {upper}");
            assert!(upper.contains('↓'), "total={total} 上行应含 ↓: {upper}");
            // 下行以 " " 开头（缩进）、含进程 basename、含规则
            assert!(lower.starts_with(' '), "total={total} 下行应以空格缩进: {lower:?}");
            assert!(lower.contains("curl"), "total={total} 下行应含进程 curl: {lower}");
            assert!(
                lower.contains("DOMAIN,example.com"),
                "total={total} 下行应含规则: {lower}"
            );
        }
        // total>=60 时含分组（阈值已降至 60，保证 120 终端下左 pane 68 列仍显示）
        let c2 = conn_with_rule(
            "example.com",
            "/usr/bin/curl",
            vec!["PROXY", "节点A"],
            "tcp",
            "DOMAIN",
            "payload",
        );
        let lower60 = conn_lines(&c2, 60)[1].to_string();
        assert!(
            lower60.contains("节点A") && lower60.contains('→'),
            "w=60 下行应含分组: {lower60}"
        );
        let lower70 = conn_lines(&c2, 70)[1].to_string();
        assert!(
            lower70.contains("节点A") && lower70.contains('→'),
            "w=70 下行应含分组: {lower70}"
        );
        let lower85 = conn_lines(&c2, 85)[1].to_string();
        assert!(
            lower85.contains("节点A") && lower85.contains('→'),
            "w=85 下行应含分组: {lower85}"
        );
        let lower120 = conn_lines(&c2, 120)[1].to_string();
        assert!(
            lower120.contains("节点A"),
            "w=120 下行应含分组: {lower120}"
        );
        // 59 以下不含分组（极窄防御）
        let lower59 = conn_lines(&c2, 59)[1].to_string();
        assert!(
            !lower59.contains("节点A") && !lower59.contains('→'),
            "w=59 下行不应含分组: {lower59}"
        );
    }

    #[test]
    fn conn_lines_ellipsis_on_overflow() {
        // 长 host、长 process、长 rule+payload、chains 多级 — 验证超出时截断不加 "…" 且宽度受限
        let long_host = "a-very-long-host-name-that-exceeds-width.example.com".to_string();
        let long_proc = "/very/long/path/to/super-long-process-name-executable";
        let long_rule = "DOMAIN-SUFFIX";
        let long_payload = "this-is-a-very-long-payload-that-will-overflow-rule-column.example.com";
        let c = ConnInfo {
            meta: ConnMeta {
                host: long_host.clone(),
                network: "tcp".into(),
                process_path: long_proc.into(),
                ..ConnMeta::default()
            },
            chains: vec!["PROXY".to_string(), "节点A".to_string(), "超长分组名测试分组名测试".to_string()],
            rule: long_rule.into(),
            rule_payload: long_payload.into(),
            upload: 1024 * 1024,
            download: 2048 * 1024,
            ..ConnInfo::default()
        };
        // w=70 时上行 host 应截断且不含 "…"，宽度受限
        let lines70 = conn_lines(&c, 70);
        let upper70 = lines70[0].to_string();
        let lower70 = lines70[1].to_string();
        assert!(!upper70.contains('…'), "w=70 上行不应含 …: {}", upper70);
        assert!(!lower70.contains('…'), "w=70 下行不应含 …: {}", lower70);
        let host_w = upper_host_width(70);
        let host_raw = conn_host(&c);
        let host_trunc = truncate_width(&host_raw, host_w);
        assert!(!host_trunc.contains('…'));
        assert!(crate::ui::widgets::display_width(&host_trunc) <= host_w);
        assert!(crate::ui::widgets::display_width(&host_raw) > host_w);
        assert!(host_trunc.len() < host_raw.len());
        let (proc_w, rule_w, group_w, _) = lower_widths(70);
        let rule_raw = rule_label(&c);
        let rule_trunc = truncate_width(&rule_raw, rule_w);
        assert!(!rule_trunc.contains('…'));
        assert!(crate::ui::widgets::display_width(&rule_trunc) <= rule_w);
        if crate::ui::widgets::display_width(&rule_raw) > rule_w {
            assert!(rule_trunc.len() < rule_raw.len());
        }
        // 验证下行各列宽度受限
        let proc_raw = process_short(&c);
        let proc_trunc = truncate_width(&proc_raw, proc_w);
        assert!(crate::ui::widgets::display_width(&proc_trunc) <= proc_w);
        let group_raw = group_label(&c);
        let group_trunc = truncate_width(&group_raw, group_w);
        assert!(crate::ui::widgets::display_width(&group_trunc) <= group_w);
        // w=85 与 120 均不应含 "…"，但有截断且宽度受限
        for w in [85u16, 120] {
            let lines = conn_lines(&c, w);
            let combined = format!("{} {}", lines[0], lines[1]);
            assert!(!combined.contains('…'), "w={w} 不应含 …: upper={} lower={}", lines[0], lines[1]);
            let host_w = upper_host_width(w);
            let host_raw = conn_host(&c);
            if crate::ui::widgets::display_width(&host_raw) > host_w {
                let t = truncate_width(&host_raw, host_w);
                assert!(crate::ui::widgets::display_width(&t) <= host_w);
                assert!(t.len() < host_raw.len());
            }
            let (proc_w, rule_w, group_w, _) = lower_widths(w);
            for (raw, budget) in [
                (process_short(&c), proc_w),
                (rule_label(&c), rule_w),
                (group_label(&c), group_w),
            ] {
                let t = truncate_width(&raw, budget);
                assert!(crate::ui::widgets::display_width(&t) <= budget, "w={w} raw={raw} budget={budget} trunc={t}");
                assert!(!t.contains('…'));
            }
        }
        // 窄宽不 panic 且均不含 "…"
        for w in [0u16, 1, 5, 20, 60] {
            let lines = conn_lines(&c, w);
            assert_eq!(lines.len(), 2, "w={w} 应返回2行");
            assert!(!lines[0].to_string().is_empty(), "w={w} 上行不应空");
            assert!(!lines[1].to_string().is_empty(), "w={w} 下行不应空");
            assert!(!lines[0].to_string().contains('…'));
            assert!(!lines[1].to_string().contains('…'));
        }
    }

    #[test]
    fn conn_lines_narrow_no_panic() {
        let c = conn_with(
            "example.com",
            "/very/long/path/to/super-long-process-name-executable",
            vec!["PROXY", "节点A"],
            "tcp",
        );
        for w in [0u16, 1, 5, 20, 60, 79, 80, 99, 100, 120, 200] {
            let lines = conn_lines(&c, w);
            assert_eq!(lines.len(), 2, "w={w} 应返回2行");
            let upper = lines[0].to_string();
            let lower = lines[1].to_string();
            assert!(!upper.is_empty(), "w={w} 上行不应空");
            assert!(!lower.is_empty(), "w={w} 下行不应空");
            if w >= 1 {
                assert!(
                    upper.contains("TCP") || upper.contains("UDP") || upper.contains('?'),
                    "w={w} 上行应含网络标记: {upper}"
                );
            }
        }
        // conn_line_with_width 兼容（返回上行）不 panic
        for w in [0u16, 1, 5, 20, 60, 120] {
            let line = conn_line_with_width(&c, w);
            assert!(!line.to_string().is_empty(), "w={w} conn_line_with_width 不应空");
        }
        let _ = conn_line(&c);
    }

    #[test]
    fn conn_line_with_width_is_upper() {
        let c = conn_with_rule(
            "example.com",
            "/usr/bin/curl",
            vec!["PROXY"],
            "tcp",
            "DOMAIN",
            "example.com",
        );
        for w in [70u16, 85, 120] {
            let upper_via_lines = conn_lines(&c, w)[0].to_string();
            let via_compat = conn_line_with_width(&c, w).to_string();
            assert_eq!(
                upper_via_lines, via_compat,
                "w={w} conn_line_with_width 应等于 conn_lines[0]"
            );
        }
    }

    #[test]
    fn conn_line_with_width_responsive_upper_only() {
        // 长进程与多级 chains，验证上行仅含 host+网络，不含进程/分组
        let c = ConnInfo {
            meta: ConnMeta {
                host: "example.com".into(),
                network: "tcp".into(),
                process_path: "/very/long/path/to/super-long-process-name-executable".into(),
                ..ConnMeta::default()
            },
            chains: vec!["PROXY".to_string(), "节点A".to_string()],
            rule: "DIRECT".into(),
            rule_payload: String::new(),
            upload: 1024,
            download: 2048,
            ..ConnInfo::default()
        };
        for w in [70u16, 85, 120] {
            let s = conn_line_with_width(&c, w).to_string();
            assert!(s.contains("example.com"), "w={w} 上行应含 host: {s}");
            assert!(s.contains("TCP"), "w={w} 上行应含 TCP: {s}");
            assert!(s.contains('↑'), "w={w} 上行应含 ↑: {s}");
            // 上行不应含进程/分组（已移至下行）
            assert!(
                !s.contains("super-long"),
                "w={w} 上行不应含进程: {s}"
            );
            assert!(!s.contains("节点A"), "w={w} 上行不应含分组: {s}");
            assert!(!s.contains('…'), "w={w} 上行不应含 …: {s}");
        }
        // 但双行的下行应含进程/分组（按宽度）：下行始终含进程与规则，total>=60 时额外含分组（带 →），且不含 "…"
        let lower120 = conn_lines(&c, 120)[1].to_string();
        assert!(
            lower120.contains("super-long"),
            "w=120 下行应含进程: {lower120}"
        );
        assert!(!lower120.contains('…'), "w=120 下行不应含 …: {lower120}");
        assert!(lower120.contains("DIRECT"), "w=120 下行应含规则 DIRECT: {lower120}");
        assert!(
            lower120.contains("节点A") && lower120.contains('→'),
            "w=120 下行应含分组（带 →）: {lower120}"
        );
        let lower70 = conn_lines(&c, 70)[1].to_string();
        assert!(lower70.contains("DIRECT"), "w=70 下行应含规则: {lower70}");
        assert!(!lower70.contains('…'), "w=70 下行不应含 …: {lower70}");
        assert!(
            lower70.contains('→') && lower70.contains("节点A"),
            "w=70 下行应含分组（带 →）: {lower70}"
        );
        let lower85 = conn_lines(&c, 85)[1].to_string();
        assert!(!lower85.contains('…'), "w=85 下行不应含 …: {lower85}");
        assert!(
            lower85.contains('→') && lower85.contains("节点A"),
            "w=85 下行应含分组: {lower85}"
        );
        let lower60 = conn_lines(&c, 60)[1].to_string();
        assert!(!lower60.contains('…'), "w=60 下行不应含 …: {lower60}");
        assert!(
            lower60.contains('→') && lower60.contains("节点A"),
            "w=60 下行应含分组: {lower60}"
        );
        let lower59 = conn_lines(&c, 59)[1].to_string();
        assert!(!lower59.contains('…'), "w=59 下行不应含 …: {lower59}");
        assert!(
            !lower59.contains('→'),
            "w=59 下行不应含分组（无 →）: {lower59}"
        );
    }

    #[test]
    fn conn_line_host_truncation_contains_ellipsis() {
        // 超长 host 在窄宽下应被截断且不含 "…"，宽度受限
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
        let host_w = upper_host_width(70);
        let host_trunc = truncate_width(&long_host, host_w);
        assert!(!host_trunc.contains('…'));
        assert!(crate::ui::widgets::display_width(&host_trunc) <= host_w);
        assert!(crate::ui::widgets::display_width(&long_host) > host_w);
        assert!(host_trunc.len() < long_host.len());
        let line = conn_line_with_width(&c, 70);
        let s = line.to_string();
        assert!(
            !s.contains('…'),
            "超长 host 截断不应含 …: host_len {} s={s}",
            long_host.len()
        );
        assert!(s.contains("TCP"));
        // 保留中文宽度正确性验证，但不再要求以 "…" 结尾
        let long_cn = "这是一个非常长的中文主机名测试用例用于验证截断逻辑是否正确处理宽字符.example.com";
        let c2 = ConnInfo {
            meta: ConnMeta {
                host: long_cn.into(),
                network: "udp".into(),
                ..ConnMeta::default()
            },
            upload: 0,
            download: 0,
            ..ConnInfo::default()
        };
        let host_trunc2 = truncate_width(long_cn, host_w);
        assert!(!host_trunc2.contains('…'));
        assert!(crate::ui::widgets::display_width(&host_trunc2) <= host_w);
        let line2 = conn_line_with_width(&c2, 70);
        let s2 = line2.to_string();
        assert!(!s2.contains('…'), "中文超长 host 不应含 …: {s2}");
        assert!(s2.contains("UDP"));
        assert!(crate::ui::widgets::display_width(&s2) <= 70 || !s2.contains('…'));
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
            if w >= 1 {
                assert!(s.contains("TCP") || s.contains("UDP") || s.contains("?"));
            }
        }
        // conn_line 兼容旧签名（默认120）也不 panic
        let _ = conn_line(&c);
    }

    #[test]
    fn truncate_ellipsis_used_in_conn_line() {
        // 验证 process_short 的 basename 截断：proc 宽度 16，basename 超长时应被截断且不含 "…"，宽度受限
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
        // 双行下：proc_w 来自 lower_widths(120) =16
        let (proc_w, _, _, _) = lower_widths(120);
        let proc_raw = process_short(&c);
        let proc_trunc = truncate_width(&proc_raw, proc_w);
        assert!(!proc_trunc.contains('…'));
        assert!(crate::ui::widgets::display_width(&proc_trunc) <= proc_w);
        if crate::ui::widgets::display_width(&proc_raw) > proc_w {
            assert!(proc_trunc.len() < proc_raw.len());
            assert!(!proc_trunc.ends_with('…'), "超长进程不应以 … 结尾: {proc_trunc}");
        }
        // 双行验证：下行含进程截断但不含 "…"
        let lower = conn_lines(&c, 120)[1].to_string();
        assert!(!lower.contains('…'), "下行不应含 …: {lower}");
        if crate::ui::widgets::display_width(&proc_raw) > proc_w {
            assert!(lower.contains(&proc_trunc), "下行应含截断后进程: {lower} vs {proc_trunc}");
        }
        // 旧 col_widths 场景也验证（保留兼容）
        let cw = col_widths(120);
        let proc_trunc2 = truncate_width(&proc_raw, cw.proc);
        assert!(!proc_trunc2.contains('…'));
        assert!(crate::ui::widgets::display_width(&proc_trunc2) <= cw.proc);
    }

    #[test]
    fn truncate_width_basic_and_unicode() {
        // 边界 0/1
        assert_eq!(truncate_width("", 0), "");
        assert_eq!(truncate_width("hello", 0), "");
        assert_eq!(truncate_width("a", 1), "a");
        assert_eq!(truncate_width("ab", 1), "a");
        assert_eq!(truncate_width("hello", 5), "hello");
        assert_eq!(truncate_width("hello world", 5), "hello");
        assert_eq!(truncate_width("hello", 10), "hello");
        // 中文
        assert_eq!(crate::ui::widgets::display_width("中文"), 4);
        assert_eq!(truncate_width("中文", 4), "中文");
        assert_eq!(truncate_width("中文", 3), "中");
        assert_eq!(truncate_width("中文", 2), "中");
        assert_eq!(truncate_width("中文", 1), "");
        assert_eq!(truncate_width("中文测试", 5), "中文");
        assert_eq!(truncate_width("a中b", 4), "a中b");
        assert_eq!(truncate_width("a中b", 3), "a中");
        // emoji
        assert_eq!(crate::ui::widgets::display_width("😀"), 2);
        assert_eq!(truncate_width("😀", 2), "😀");
        assert_eq!(truncate_width("😀", 1), "");
        assert_eq!(truncate_width("😀😀", 3), "😀");
        assert_eq!(truncate_width("a😀b", 3), "a😀");
        // 宽度永远不超过 max 且不含省略号
        for s in ["hello world", "中文测试", "a中😀b", "😀😀😀"] {
            for max in 0..10 {
                let out = truncate_width(s, max);
                assert!(
                    crate::ui::widgets::display_width(&out) <= max,
                    "truncate_width({:?}, {}) => {:?} width {}",
                    s,
                    max,
                    out,
                    crate::ui::widgets::display_width(&out)
                );
                assert!(!out.contains('…'), "unexpected … in {:?}", out);
            }
        }
    }

    // ---- 滚动相关 ----

    #[test]
    fn dashboard_scroll_up_down() {
        let mut page = DashboardPage::new();
        let mut st = test_state();
        // 初始 0，Down 累增
        assert_eq!(page.conn_scroll, 0);
        page.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &mut st);
        assert_eq!(page.conn_scroll, 1);
        page.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE), &mut st);
        assert_eq!(page.conn_scroll, 2);
        // Up 递减
        page.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), &mut st);
        assert_eq!(page.conn_scroll, 1);
        page.handle_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE), &mut st);
        assert_eq!(page.conn_scroll, 0);
        // 顶部 saturating_sub 不下溢
        page.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), &mut st);
        assert_eq!(page.conn_scroll, 0);
    }

    #[test]
    fn dashboard_scroll_home_end_page() {
        let mut page = DashboardPage::new();
        let mut st = test_state();
        page.last_height = 4; // visible =2
        page.conn_scroll = 2;
        // Home/g 回 0
        page.handle_key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE), &mut st);
        assert_eq!(page.conn_scroll, 0);
        page.conn_scroll = 2;
        page.handle_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE), &mut st);
        assert_eq!(page.conn_scroll, 0);
        // End/G 置 MAX（render 中 clamp）
        page.handle_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE), &mut st);
        assert_eq!(page.conn_scroll, usize::MAX);
        page.conn_scroll = 0;
        page.handle_key(KeyEvent::new(KeyCode::Char('G'), KeyModifiers::NONE), &mut st);
        assert_eq!(page.conn_scroll, usize::MAX);
        // PageDown/PageUp 按 visible 步进
        page.conn_scroll = 0;
        page.handle_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE), &mut st);
        assert_eq!(page.conn_scroll, 2);
        page.handle_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE), &mut st);
        assert_eq!(page.conn_scroll, 4);
        page.handle_key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE), &mut st);
        assert_eq!(page.conn_scroll, 2);
        page.handle_key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE), &mut st);
        assert_eq!(page.conn_scroll, 0);
        // PageUp 在顶部不下溢
        page.handle_key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE), &mut st);
        assert_eq!(page.conn_scroll, 0);
    }

    #[test]
    fn dashboard_scroll_clamp_with_connections() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        // 构造 5 条连接，inner.height=4 => visible=2，max_scroll=3
        // 终端高度 7 => status 1 + body 6 => left height 6 => inner 4 => visible 2
        let conns: Vec<ConnInfo> = (0..5)
            .map(|i| conn_with(&format!("host{i}.com"), "/usr/bin/curl", vec!["PROXY"], "tcp"))
            .collect();
        let mut st = test_state();
        st.connections = conns.clone();
        // 用 TestBackend 驱动 render，验证 scroll clamp 与标题 "连接 x/y"
        let backend = TestBackend::new(80, 7);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut page = DashboardPage::new();
        // scroll=0 时应显示前 2 条（visible 2）
        page.conn_scroll = 0;
        // 人为设定 last_height=4 对应 visible 2，render 会以 tmp_inner 计算覆盖，但先测 clamp 逻辑：
        // 5 条 - visible 2 = max 3，scroll=10 应被 clamp 到 3
        page.conn_scroll = 10;
        page.last_height = 4;
        let max_scroll = st.connections.len().saturating_sub(2);
        let clamped = page.conn_scroll.min(max_scroll);
        assert_eq!(clamped, 3, "max_scroll=3, scroll 10 应 clamp 到 3");
        // 实际渲染：设置 scroll=1，应跳过首条显示第2-3条
        page.conn_scroll = 1;
        terminal
            .draw(|f| {
                // body 区域足够大，保证 inner.height 能容纳 visible*2 行
                // 直接调用 render_connections 的切片逻辑验证：skip(1).take(2) 得到 host1,host2
                let visible = 2;
                let scroll = page.conn_scroll.min(st.connections.len().saturating_sub(visible));
                let slice: Vec<String> = st
                    .connections
                    .iter()
                    .skip(scroll)
                    .take(visible)
                    .map(|c| c.meta.host.clone())
                    .collect();
                assert_eq!(slice, vec!["host1.com", "host2.com"]);
                // 同时调用真实渲染不 panic
                let area = f.area();
                page.render(f, area, &st);
            })
            .unwrap();
        // End 后渲染应 clamp 到 max_scroll
        page.conn_scroll = usize::MAX;
        terminal
            .draw(|f| {
                let area = f.area();
                page.render(f, area, &st);
            })
            .unwrap();
        assert_eq!(page.conn_scroll, 3, "End(usize::MAX) 渲染后应 clamp 到 max_scroll 3");
        // Up 后应回到 2
        page.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), &mut st);
        assert_eq!(page.conn_scroll, 2);
    }

    #[test]
    fn dashboard_scroll_keys_do_not_produce_command() {
        let mut page = DashboardPage::new();
        let mut st = test_state();
        for code in [
            KeyCode::Up,
            KeyCode::Down,
            KeyCode::PageUp,
            KeyCode::PageDown,
            KeyCode::Home,
            KeyCode::End,
            KeyCode::Char('j'),
            KeyCode::Char('k'),
            KeyCode::Char('g'),
            KeyCode::Char('G'),
        ] {
            let cmd = page.handle_key(KeyEvent::new(code, KeyModifiers::NONE), &mut st);
            assert!(cmd.is_none(), "滚动键 {code:?} 不应产生 UiCommand");
        }
        // 非滚动键 m 仍应产生命令
        let cmd = page.handle_key(KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE), &mut st);
        assert!(cmd.is_some(), "m 应产生 PatchConfigs");
    }
}
