//! 应用主循环：AppState、UiCommand/UiEvent、后台任务（traffic/memory/exit_ip）、
//! tokio::select! 事件分发。契约见 docs/superpowers/plans/2026-08-10-mihomo-tui.md §3。

use std::collections::{HashMap, VecDeque};
use std::io;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event, EventStream, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::execute;
use futures_util::StreamExt;
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Tabs};
use ratatui::{CompletedFrame, Terminal};
use tokio::sync::mpsc;

use crate::core::apply::{
    apply_config, proc_control, proc_status, validate_config, ApplyOutcome, ProcOp, RunStatus,
};
#[cfg(not(windows))]
use crate::core::apply::{service_is_active, service_unit_exists, systemctl_control};
use crate::core::client::GROUP_DELAY_TIMEOUT_MS;
use crate::core::client::{
    Client, ConnInfo, ConnSnapshot, GroupInfo, LogEntry, LogLevel, MemoryFrame, RuntimeConfig,
    TrafficFrame,
};
use crate::core::exit_ip::{self, ExitInfo, ProxyPorts};
#[cfg(not(windows))]
use crate::core::models::RunMode;
use crate::core::models::{NetworkSettings, Overrides, Subscription, SubscriptionCache};
use crate::core::settings::{
    load_overrides, load_settings, load_subscriptions, save_overrides, save_subscriptions,
};
use crate::ui::dashboard::DashboardPage;
use crate::ui::groups::GroupsPage;
use crate::ui::rules::RulesPage;
use crate::ui::subscriptions::SubscriptionsPage;
use crate::ui::widgets::{ConfirmPopup, KeyHints, MessagePopup};
use crate::ui::Page;

/// 主循环统一错误（Cargo.toml 未直接依赖 anyhow，以 Box<dyn Error> 替代；
/// 集成阶段若补充 anyhow 依赖可整体换回 anyhow::Result）。
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// 应用状态（页面只读共享）。
pub struct AppState {
    pub settings: NetworkSettings,
    pub subs: Vec<Subscription>,
    pub overrides: Overrides,
    pub runtime: RuntimeConfig,
    pub api_ok: bool,
    /// 首次 /configs 检查（ConfigsRefreshed）是否已完成：完成前 REST API
    /// 可达性未知，exit_ip 失败不得据此交叉判断（启动竞态）。
    pub api_confirmed: bool,
    pub traffic: VecDeque<TrafficFrame>,
    pub mem_history: VecDeque<u64>,
    pub connections: Vec<ConnInfo>,
    pub exit_ip: Option<ExitInfo>,
    /// 运行时策略组快照（GET /proxies；规则组页数据源）
    pub proxy_groups: Vec<GroupInfo>,
    /// 整组延迟测试结果缓存（组名 → 节点延迟；弹窗内测速静默模式也写入，
    /// 供节点选择弹窗按延迟排序/展示延迟）
    pub group_delays: HashMap<String, Vec<(String, u16)>>,
    /// mihomo 日志环形缓冲（logs 后台任务填充，日志页只读展示）。
    pub logs: VecDeque<LogEntry>,
    /// 通知（带到达时刻）。整组共享截止时间（notice_deadline），到期整组同消。
    pub notices: VecDeque<(Instant, String)>,
    /// 运行状态（设置页运行方式区块显示；RefreshStatus 事件更新）
    pub run_status: Option<RunStatus>,
}

impl AppState {
    fn load() -> Self {
        let mut notices = VecDeque::new();
        let settings = match load_settings() {
            Ok(s) => s,
            Err(e) => {
                notices.push_back((Instant::now(), format!("[✗] 加载设置失败: {e}")));
                NetworkSettings::default()
            }
        };
        let subs = match load_subscriptions() {
            Ok(s) => s,
            Err(e) => {
                notices.push_back((Instant::now(), format!("[✗] 加载订阅失败: {e}")));
                Vec::new()
            }
        };
        let mut overrides = match load_overrides() {
            Ok(o) => o,
            Err(e) => {
                notices.push_back((Instant::now(), format!("[✗] 加载规则覆盖失败: {e}")));
                Overrides::default()
            }
        };
        // 旧版自定义规则组数据迁移（方案 A：清空 + 一次提示）
        if let Some(msg) = migrate_legacy_groups(&mut overrides) {
            if let Err(e) = save_overrides(&overrides) {
                notices.push_back((Instant::now(), format!("[✗] 旧数据清理落盘失败: {e}")));
            }
            notices.push_back((Instant::now(), msg));
        }
        Self {
            settings,
            subs,
            overrides,
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
            notices,
            run_status: None,
        }
    }

    /// 追加通知（记录到达时刻），保留最近 5 条。
    pub fn notice(&mut self, msg: String) {
        self.notices.push_back((Instant::now(), msg));
        while self.notices.len() > 5 {
            self.notices.pop_front();
        }
    }
}

/// 页面 → 主循环的异步操作请求。
/// PatchConfigs 携带双写结果：saved=settings.toml 是否已持久化，label=开关名。
#[derive(Debug)]
pub enum UiCommand {
    PatchConfigs {
        patch: serde_json::Value,
        saved: bool,
        label: String,
    },
    ApplyConfig(String),
    FetchSubscription(usize),
    FetchExitIp,
    ReloadConfigs,
    InstallSetup,
    /// 拉取运行时策略组（GET /proxies）
    RefreshGroups,
    /// 切换 select 组当前节点（PUT /proxies）
    SwitchGroup {
        group: String,
        target: String,
    },
    /// 整组延迟测试（GET /group/{name}/delay）；silent=true 为弹窗内测速
    /// （结果只进 AppState.group_delays 供弹窗展示/排序，不弹结果弹窗）
    TestGroupDelay {
        group: String,
        silent: bool,
    },
    /// 刷新运行状态（systemd 单元/服务 + 直接进程实例）
    RefreshStatus,
    /// 直接进程模式操作（start/stop/restart，经 mihomo-proc 提权脚本）
    ProcAction(ProcOp),
    /// systemd 模式操作（start/stop/restart）：直接 systemctl，桌面 polkit 弹窗认证
    SystemdAction(ProcOp),
    /// 交互式提权保存 mihomo 路径（需 sudo 密码）
    SaveMihomoBin(String),
    /// 重启 mihomo 核心（POST /restart，需二次确认）
    RestartCore,
    /// 日志页切换显示级别：主循环转发给 logs 后台任务触发 ?level= 重连。
    SetLogLevel(LogLevel),
}

/// 后台任务 → 主循环的事件。
/// PatchDone 透传双写结果：saved/label 决定成功/失败时的用户反馈文案。
pub enum UiEvent {
    PatchDone {
        res: Result<(), String>,
        saved: bool,
        label: String,
    },
    ApplyDone(Result<ApplyOutcome, String>),
    SubscriptionFetched(usize, Result<SubscriptionCache, String>),
    ExitIp(Result<ExitInfo, String>),
    ConfigsRefreshed(Result<RuntimeConfig, String>),
    GroupsRefreshed(Result<Vec<GroupInfo>, String>),
    GroupSwitched {
        group: String,
        target: String,
        result: Result<(), String>,
    },
    GroupDelayDone {
        group: String,
        silent: bool,
        result: Result<Vec<(String, u16)>, String>,
    },
    RunStatusDone(Result<RunStatus, String>),
    /// 进程操作结果（direct 模式 mihomo-proc 与 systemd 模式 systemctl 共用）
    ProcActionDone(Result<ApplyOutcome, String>),
    /// 重启核心结果
    RestartDone(Result<(), String>),
    /// 启动时引导通知（systemd 模式服务不可用）
    StartupNotice(String),
    /// logs 后台任务推送的单条日志。
    LogLine(LogEntry),
}

/// traffic 后台任务发往主循环的消息。
enum BgMsg {
    Traffic(TrafficFrame),
    Api(bool),
}

/// 需要交互式终端（离开 raw 模式/AltScreen）执行的任务。
enum InteractiveTask {
    Apply(String),
    /// 提权组件安装（Linux 专属：Windows 无 sudoers 体系）
    #[cfg(not(windows))]
    Install,
    SaveMihomoBin(String),
}

/// 按键处理结果。
enum KeyAction {
    Quit,
    Interactive(InteractiveTask),
}

const TABS: [&str; 6] = ["仪表盘", "订阅", "规则组", "规则", "日志", "设置"];

/// 单个 tab 与 divider 的分隔符（用于命中计算，必须与 draw 中 Tabs::divider 一致）
const TAB_DIVIDER: &str = " │ ";
const TAB_DIVIDER_WIDTH: u16 = 3;

/// 计算每个 tab 的命中 Rect（仅文本区域，不包含 divider）
/// tabs_area 为 Tabs 容器区域（单行高度）
fn compute_tab_hits(tabs: &[&str], tabs_area: Rect) -> Vec<Rect> {
    let mut hits = Vec::with_capacity(tabs.len());
    if tabs_area.width == 0 || tabs_area.height == 0 {
        return hits;
    }
    let mut x = tabs_area.x;
    let end_x = tabs_area.x.saturating_add(tabs_area.width);
    for (i, tab) in tabs.iter().enumerate() {
        let w = Line::raw(*tab).width() as u16;
        if w == 0 {
            hits.push(Rect::new(x, tabs_area.y, 0, 1));
        } else if x.saturating_add(w) > end_x {
            // 剩余空间不足：截断或不再容纳
            let remain = end_x.saturating_sub(x);
            if remain > 0 {
                hits.push(Rect::new(x, tabs_area.y, remain, 1));
            }
            break;
        } else {
            hits.push(Rect::new(x, tabs_area.y, w, 1));
            x = x.saturating_add(w);
        }
        // divider 间隔（最后一个 tab 后无 divider）
        if i + 1 < tabs.len() {
            if x.saturating_add(TAB_DIVIDER_WIDTH) > end_x {
                break;
            }
            x = x.saturating_add(TAB_DIVIDER_WIDTH);
        }
    }
    hits
}

/// 鼠标命中测试：返回命中的 tab 索引（仅文本区域命中，divider/空白返回 None）
fn hit_test(tabs_area: Rect, tab_hits: &[Rect], column: u16, row: u16) -> Option<usize> {
    if row != tabs_area.y {
        return None;
    }
    if column < tabs_area.x || column >= tabs_area.x.saturating_add(tabs_area.width) {
        return None;
    }
    for (idx, hit) in tab_hits.iter().enumerate() {
        if hit.width == 0 {
            continue;
        }
        if column >= hit.x && column < hit.x.saturating_add(hit.width) && row == hit.y {
            return Some(idx);
        }
    }
    None
}

const TRAFFIC_HISTORY: usize = 120;

/// 设置页停留时的运行状态轮询间隔（systemctl/sudo 查询开销小，2s 足够及时）。
const STATUS_REFRESH_INTERVAL: Duration = Duration::from_secs(2);
/// 进程状态查询超时（防止 sudo 异常卡住阻塞状态刷新）。
const PROC_STATUS_TIMEOUT: Duration = Duration::from_secs(2);

/// 是否需要发起运行状态刷新：从未刷新过或距上次刷新已超过间隔。
pub fn should_refresh_status(last: Option<Instant>, now: Instant) -> bool {
    match last {
        None => true,
        Some(t) => now.duration_since(t) >= STATUS_REFRESH_INTERVAL,
    }
}

/// 连接列表保留上限（快照替换天然有界，此处防御性截断）。
const CONNECTIONS_KEEP: usize = 200;
/// /connections 轮询间隔。
const CONNECTIONS_POLL: Duration = Duration::from_secs(3);

/// 日志保留上限（环形缓冲，超出淘汰最旧）。
const LOG_HISTORY: usize = 1000;

/// API 状态通知去抖窗口：同向状态变化在此窗口内不重复入列通知
/// （traffic 流断连与 5s 轮询成功竞态会造成高频翻转刷屏）。
const API_NOTICE_DEBOUNCE: Duration = Duration::from_secs(3);

/// 通知类型时长：`[✗]`/`[!]` 错误警告 10s，其余（成功/普通）5s。
const NOTICE_OK_TTL: Duration = Duration::from_secs(5);
const NOTICE_ERR_TTL: Duration = Duration::from_secs(10);

/// 单条通知的类型时长。
fn notice_ttl(text: &str) -> Duration {
    if text.starts_with("[✗]") || text.starts_with("[!]") {
        NOTICE_ERR_TTL
    } else {
        NOTICE_OK_TTL
    }
}

/// 整组通知截止时间：组内所有 `到达时刻 + 类型时长` 的最大值
/// （锚定"时间最长的一条"，不管新旧）；空组返回 None。
/// 调用方以 `deadline > now` 判定整组是否可见，到期整组同时消失。
/// 入参为通知引用迭代器（&[(Instant, String)] / &VecDeque 均可）。
fn notice_deadline<'a>(
    notices: impl IntoIterator<Item = &'a (Instant, String)>,
) -> Option<Instant> {
    notices
        .into_iter()
        .map(|(at, text)| {
            // 1.88 无 Instant::saturating_add：checked_add 溢出（实际不可能）时
            // 回退到 at 本身（立即到期），不 panic
            (*at).checked_add(notice_ttl(text)).unwrap_or(*at)
        })
        .max()
}

const HELP_LINES: &[&str] = &[
    "全局按键:",
    "  q / Ctrl-C / Esc   退出",
    "  Tab / ← → / 1-6    切换页面",
    "  ?                  显示本帮助",
    "",
    "仪表盘:",
    "  m                  切换模式 (rule → global → direct)",
    "  t                  开关 TUN（热切换）",
    "  6                  开关 IPv6",
    "  r                  刷新出口 IP",
    "  R                  重启核心（需确认）",
    "  s                  跳转设置页",
    "  i                  安装提权组件（首次启动拒绝后的重试入口）",
    "",
    "订阅管理:",
    "  a                  添加订阅",
    "  Enter              激活订阅",
    "  r                  刷新订阅",
    "  d                  删除订阅",
    "",
    "规则组:",
    "  Enter              切换节点（select 组）",
    "  r                  整组延迟测试",
    "  R                  刷新组列表",
    "",
    "规则:",
    "  n                  新建规则",
    "  Enter              编辑规则",
    "  K / J              上移 / 下移",
    "  d                  删除规则",
    "  Ctrl+A             保存并应用",
    "",
    "日志:",
    "  e                  切换级别 (error → warning → info → debug)",
    "  ↑ / ↓ / PgUp / PgDn 回溯日志（暂停跟随）",
    "  f / End             恢复跟随底部",
    "  c                  清空日志",
    "",
    "设置:",
    "  ↑↓                移动字段",
    "  Enter             编辑文本/数字 · 循环下拉 · secret 重新生成",
    "  编辑态: ←→ 移动光标, Esc 退出（Tab/←→/1-6 同全局切页）",
    "  Ctrl+S            仅保存（写 settings.toml，不重启）",
    "  Ctrl+A            保存并应用（合并 → mihomo -t 校验 → 提权重启）",
];

/// 帮助内容（渲染时过滤）：Windows 隐藏「i 安装提权组件」行（该入口为 Linux 专用）；
/// Linux 上 cfg!(windows) 恒为 false，输出与 HELP_LINES 完全一致。
fn help_lines() -> Vec<String> {
    let mut lines: Vec<String> = HELP_LINES.iter().map(|s| s.to_string()).collect();
    if cfg!(windows) {
        lines.retain(|s| s != "  i                  安装提权组件（首次启动拒绝后的重试入口）");
    }
    lines
}

struct App<B: Backend> {
    state: AppState,
    pages: Vec<Box<dyn Page>>,
    current: usize,
    /// 顶部 Tab 命中区域（每帧在 draw() 中重算，供鼠标点击命中测试）
    tab_hits: Vec<Rect>,
    /// 顶部 Tabs 容器区域（tabs_area）
    tabs_area: Rect,
    client: Arc<Client>,
    ui_tx: mpsc::UnboundedSender<UiEvent>,
    cmd_tx: mpsc::UnboundedSender<UiCommand>,
    cmd_rx: mpsc::UnboundedReceiver<UiCommand>,
    sudo_tx: mpsc::UnboundedSender<String>,
    exit_trigger: mpsc::UnboundedSender<()>,
    /// 日志级别切换通道（spawn_command → spawn_logs_task）。
    log_level_tx: mpsc::UnboundedSender<LogLevel>,
    /// 代理端口快照（ApplyDone 成功后更新并触发重测）
    exit_ports: Arc<Mutex<ProxyPorts>>,
    /// 需要用户确认后执行的交互任务（sudo 密码/首次安装）
    pending_confirm: Option<(ConfirmPopup, InteractiveTask)>,
    restart_confirm: Option<ConfirmPopup>,
    restarting: bool,
    help_popup: Option<MessagePopup>,
    result_popup: Option<MessagePopup>,
    /// 出口 IP 探测最近一次是否失败：恢复成功时用于关闭陈旧错误弹窗并通知恢复。
    exit_ip_was_error: bool,
    /// 上次 API 状态通知时间（去抖用）
    api_notice_at: Option<Instant>,
    /// 上次设置页运行状态刷新时间（停留时周期轮询）
    last_status_refresh: Option<Instant>,
    tick_count: u64,
    quit: bool,
    terminal: Terminal<B>,
}

impl<B> App<B>
where
    B: Backend,
    // Terminal 操作的错误需能装箱进 BoxError（Box<dyn Error + Send + Sync>）
    B::Error: Send + Sync + 'static,
{
    /// 渲染一帧。返回 CompletedFrame 供回归测试检查 buffer（通知行/按键提示行不越界）。
    fn draw(&mut self) -> Result<CompletedFrame<'_>, BoxError> {
        let tabs: Vec<String> = TABS.iter().map(|s| s.to_string()).collect();
        let current = self.current;
        let now = Instant::now();
        // 整组共享截止时间：到期整组同时清除（避免一条条过期触发多次重绘）
        let notices_visible = notice_deadline(&self.state.notices).is_some_and(|d| d > now);
        if !notices_visible {
            self.state.notices.clear();
        }
        let visible_notice_rows = if notices_visible {
            self.state.notices.len().min(3)
        } else {
            0
        };
        let hints = page_hints(current);
        // 鼠标命中区域：draw 内计算（依赖 f.area()/top），通过外部可变变量带出后写回 self
        let mut new_tabs_area = Rect::default();
        let mut new_hits: Vec<Rect> = Vec::new();

        let frame = self.terminal.draw(|f| {
            let area = f.area();
            let [top, middle, bottom] = Layout::vertical([
                Constraint::Length(3),
                Constraint::Min(1),
                Constraint::Length(1 + visible_notice_rows as u16),
            ])
            .areas(area);

            // 顶栏：边框 + Tabs
            let block = Block::new()
                .title(Span::styled(
                    " mihomo-tui ",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ))
                .borders(Borders::ALL);
            f.render_widget(block, top);
            // 超小终端下 tabs_area 可能与 buffer 无交集（如 h=1 时 top.y+1 已越界）；
            // Tabs 不做 intersection 裁剪，必须提前 clamp 成空区域让它直接返回
            let tabs_area =
                Rect::new(top.x + 1, top.y + 1, top.width.saturating_sub(2), 1).intersection(area);
            new_tabs_area = tabs_area;
            new_hits = compute_tab_hits(&TABS, tabs_area);
            f.render_widget(
                Tabs::new(tabs.iter().map(|t| Line::raw(t.clone())))
                    .select(current)
                    .highlight_style(
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    )
                    .padding("", "")
                    .divider(TAB_DIVIDER),
                tabs_area,
            );

            // 中部：当前页
            let page = &mut self.pages[current];
            let st = &self.state;
            page.render(f, middle, st);

            // 底栏：通知 + 按键提示
            // 通知最多占到底栏高度-1 行（最后一行是按键提示）；终端过小时直接截断，
            // 避免 y 超出 buffer 导致 ratatui Buffer::index_of panic
            let (notice_rows, hint_y) = bottom_bar_rows(bottom, area.height);
            for (i, (_, text)) in st
                .notices
                .iter()
                .rev()
                .take(notice_rows as usize)
                .enumerate()
            {
                let style = if text.starts_with("[✓]") {
                    Style::default().fg(Color::Green)
                } else if text.starts_with("[!]") {
                    Style::default().fg(Color::Yellow)
                } else {
                    Style::default().fg(Color::Red)
                };
                f.render_widget(
                    Paragraph::new(Span::styled(text.clone(), style)),
                    Rect::new(bottom.x, bottom.y + i as u16, bottom.width, 1),
                );
            }
            if let Some(hint_y) = hint_y {
                KeyHints {
                    hints: hints.clone(),
                }
                .render(f, Rect::new(bottom.x, hint_y, bottom.width, 1));
            }

            // 全局弹窗置顶
            if let Some(popup) = &mut self.help_popup {
                popup.render(f, area);
            }
            if let Some((popup, _)) = &mut self.pending_confirm {
                popup.render(f, area);
            }
            if let Some(popup) = &mut self.restart_confirm {
                popup.render(f, area);
            }
            if let Some(popup) = &mut self.result_popup {
                popup.render(f, area);
            }
        })?;
        self.tabs_area = new_tabs_area;
        self.tab_hits = new_hits;
        Ok(frame)
    }

    /// 处理鼠标点击：仅 Left Down 在 tabs 行命中 tab 文本时切换页面
    /// 返回 true 表示已切换并需要重绘
    fn handle_mouse(&mut self, mouse: MouseEvent) -> bool {
        // 仅响应左键按下
        if mouse.kind != MouseEventKind::Down(MouseButton::Left) {
            return false;
        }
        // 弹窗或编辑态时屏蔽鼠标切页，避免误触
        if self.help_popup.is_some()
            || self.pending_confirm.is_some()
            || self.restart_confirm.is_some()
            || self.result_popup.is_some()
            || self.pages[self.current].popup_open()
            || self.pages[self.current].consumes_global_keys()
        {
            return false;
        }
        if let Some(idx) = hit_test(self.tabs_area, &self.tab_hits, mouse.column, mouse.row) {
            if idx < self.pages.len() && idx != self.current {
                self.switch_page(idx);
                return true;
            }
        }
        false
    }

    /// 切页；进入规则组页（index 2）时刷新运行时策略组；
    /// 进入设置页（index 5）时同步字段（页面内部 dirty 时保留编辑）。
    fn switch_page(&mut self, idx: usize) {
        self.current = idx;
        if idx == 2 {
            let _ = self.cmd_tx.send(UiCommand::RefreshGroups);
        }
        if idx == 5 {
            let st = &self.state;
            self.pages[idx].on_enter(st);
            let _ = self.cmd_tx.send(UiCommand::RefreshStatus);
            self.last_status_refresh = Some(Instant::now());
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> Option<KeyAction> {
        // Ctrl-C 永远退出（raw 模式下无 SIGINT）
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Some(KeyAction::Quit);
        }
        // 全局弹窗优先
        if let Some(popup) = &mut self.help_popup {
            if popup.handle_key(key) {
                self.help_popup = None;
            }
            return None;
        }
        if let Some((mut popup, task)) = self.pending_confirm.take() {
            match popup.handle_key(key) {
                Some(true) => return Some(KeyAction::Interactive(task)),
                Some(false) => {
                    self.state.notice("[✗] 已取消".to_string());
                }
                None => {
                    self.pending_confirm = Some((popup, task));
                }
            }
            return None;
        }
        if let Some(popup) = &mut self.result_popup {
            if popup.handle_key(key) {
                self.result_popup = None;
            }
            return None;
        }
        // restart 确认弹窗优先
        if let Some(mut popup) = self.restart_confirm.take() {
            match popup.handle_key(key) {
                Some(true) => {
                    self.restarting = true;
                    self.state.notice("[…] 正在重启...".to_string());
                    let _ = self.cmd_tx.send(UiCommand::RestartCore);
                }
                Some(false) => {
                    self.state.notice("[✗] 已取消重启".to_string());
                }
                None => self.restart_confirm = Some(popup),
            }
            return None;
        }
        if self.restarting && key.code == KeyCode::Char('R') {
            return None;
        }
        if self.current == 0 && key.code == KeyCode::Char('R') && !self.restarting {
            self.restart_confirm = Some(ConfirmPopup::new(
                "重启确认".into(),
                "确认重启 mihomo 核心？".into(),
            ));
            return None;
        }

        // 页面内部弹窗打开时，按键全部交给页面（全局键不生效）
        if self.pages[self.current].popup_open() {
            let page = &mut self.pages[self.current];
            if let Some(cmd) = page.handle_key(key, &mut self.state) {
                let _ = self.cmd_tx.send(cmd);
            }
            return None;
        }

        // 设置页编辑模式：所有键（除 Ctrl-C 已提前处理）归页面，支持输入任意字符
        if self.pages[self.current].consumes_global_keys() {
            let page = &mut self.pages[self.current];
            if let Some(cmd) = page.handle_key(key, &mut self.state) {
                let _ = self.cmd_tx.send(cmd);
            }
            return None;
        }
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return Some(KeyAction::Quit),
            KeyCode::Tab => self.switch_page((self.current + 1) % self.pages.len()),
            KeyCode::BackTab => {
                self.switch_page((self.current + self.pages.len() - 1) % self.pages.len());
            }
            KeyCode::Right => self.switch_page((self.current + 1) % self.pages.len()),
            KeyCode::Left => {
                self.switch_page((self.current + self.pages.len() - 1) % self.pages.len());
            }
            KeyCode::Char(c) if ('1'..='6').contains(&c) => {
                self.switch_page(c.to_digit(10).unwrap_or(1) as usize - 1);
            }
            // s：全局跳转设置页（设置页内不拦截——s 是文本字段输入字符）
            KeyCode::Char('s') if self.current != 5 => self.switch_page(5),
            KeyCode::Char('?') => {
                self.help_popup = Some(MessagePopup::new("帮助".into(), help_lines()));
            }
            _ => {
                let page = &mut self.pages[self.current];
                if let Some(cmd) = page.handle_key(key, &mut self.state) {
                    let _ = self.cmd_tx.send(cmd);
                }
            }
        }
        None
    }

    /// 主循环：tokio::select! 合并事件流、tick、后台通道。
    async fn run_loop(
        &mut self,
        mut traffic_rx: mpsc::UnboundedReceiver<BgMsg>,
        mut memory_rx: mpsc::UnboundedReceiver<MemoryFrame>,
        mut conns_rx: mpsc::UnboundedReceiver<ConnSnapshot>,
        mut logs_rx: mpsc::UnboundedReceiver<LogEntry>,
        mut ui_rx: mpsc::UnboundedReceiver<UiEvent>,
        mut sudo_rx: mpsc::UnboundedReceiver<String>,
    ) -> Result<(), BoxError> {
        let mut events = EventStream::new();
        let mut ticker = tokio::time::interval(Duration::from_secs(1));
        self.draw()?;

        while !self.quit {
            enum Act {
                Key(KeyEvent),
                Mouse(MouseEvent),
                Tick,
                Bg(BgMsg),
                Mem(MemoryFrame),
                Conns(ConnSnapshot),
                Log(LogEntry),
                Ui(UiEvent),
                Cmd(UiCommand),
                Sudo(String),
            }
            let act = tokio::select! {
                ev = events.next() => match ev {
                    Some(Ok(Event::Key(key))) => Act::Key(key),
                    Some(Ok(Event::Mouse(mouse))) => Act::Mouse(mouse),
                    Some(Ok(Event::Resize(_, _))) => Act::Tick,
                    Some(Ok(_)) => continue,
                    Some(Err(_)) => continue,
                    None => break,
                },
                _ = ticker.tick() => Act::Tick,
                msg = traffic_rx.recv() => match msg { Some(m) => Act::Bg(m), None => continue },
                msg = memory_rx.recv() => match msg { Some(m) => Act::Mem(m), None => continue },
                msg = conns_rx.recv() => match msg { Some(m) => Act::Conns(m), None => continue },
                msg = logs_rx.recv() => match msg { Some(m) => Act::Log(m), None => continue },
                ev = ui_rx.recv() => match ev { Some(e) => Act::Ui(e), None => continue },
                cmd = self.cmd_rx.recv() => match cmd { Some(c) => Act::Cmd(c), None => continue },
                yaml = sudo_rx.recv() => match yaml { Some(y) => Act::Sudo(y), None => continue },
            };

            match act {
                Act::Key(key) => {
                    if key.kind == KeyEventKind::Release {
                        continue;
                    }
                    match self.handle_key(key) {
                        Some(KeyAction::Quit) => self.quit = true,
                        Some(KeyAction::Interactive(task)) => {
                            self.run_interactive(task).await?;
                        }
                        None => {}
                    }
                    self.draw()?;
                }
                Act::Mouse(mouse) => {
                    if self.handle_mouse(mouse) {
                        self.draw()?;
                    }
                }
                Act::Tick => {
                    self.tick_count += 1;
                    if self.tick_count.is_multiple_of(5) || !self.state.api_ok {
                        let _ = self.cmd_tx.send(UiCommand::ReloadConfigs);
                    }
                    // 设置页停留：周期性刷新运行状态（用户在终端 systemctl start/stop
                    // 后切回 TUI 时状态行能及时反映，无需重新进入页面）
                    if self.current == 5
                        && should_refresh_status(self.last_status_refresh, Instant::now())
                    {
                        let _ = self.cmd_tx.send(UiCommand::RefreshStatus);
                        self.last_status_refresh = Some(Instant::now());
                    }
                    self.draw()?;
                }
                Act::Bg(msg) => self.on_bg_msg(msg),
                Act::Mem(frame) => self.on_memory(frame),
                Act::Conns(snap) => self.on_conns(snap),
                Act::Log(entry) => self.on_log(entry),
                Act::Ui(ev) => self.on_ui_event(ev),
                Act::Cmd(cmd) => self.spawn_command(cmd),
                Act::Sudo(yaml) => {
                    // 弹确认框前附诊断提示：区分"未重新登录（组未生效）"与
                    // "sudoers 规则未生效"两种根因，引导用户对症处理。
                    // Windows 上 Act::Sudo 永不被触发（apply 链路不产生
                    // SudoNeedsPassword），置 false 仅为让代码可编译。
                    #[cfg(not(windows))]
                    let has_group = crate::service::installer::session_has_admin_group();
                    #[cfg(windows)]
                    let has_group = false;
                    self.pending_confirm = Some((
                        ConfirmPopup::new(
                            "需要 sudo 密码".into(),
                            sudo_password_popup_lines(has_group).join("\n"),
                        ),
                        InteractiveTask::Apply(yaml),
                    ));
                }
            }
        }
        Ok(())
    }

    /// 启动引导：Windows —— TUN 已开启但非管理员 → 通知一次（UAC 无法中途提升）。
    #[cfg(windows)]
    fn spawn_startup_guard(&self) {
        if !self.state.settings.tun.enable {
            return;
        }
        let ui_tx = self.ui_tx.clone();
        tokio::spawn(async move {
            if !crate::service::process::is_elevated() {
                let _ = ui_tx.send(UiEvent::StartupNotice(
                    "TUN 模式需要管理员权限：当前 TUI 未以管理员身份运行，mihomo 将无法创建 \
                     TUN 设备。请关闭 TUN，或退出后右键「以管理员身份运行」本程序"
                        .to_string(),
                ));
            }
        });
    }

    /// 启动引导：Linux —— systemd 模式且服务不可用 → 通知用户
    /// （设置页可启动服务或切换 direct 模式）。非阻塞、无 sudo；检测失败静默。
    #[cfg(not(windows))]
    fn spawn_startup_guard(&self) {
        if self.state.settings.run_mode != RunMode::Systemd {
            return;
        }
        let ui_tx = self.ui_tx.clone();
        tokio::spawn(async move {
            if !service_unit_exists().await {
                let _ = ui_tx.send(UiEvent::StartupNotice(
                    "未检测到 mihomo.service：设置页 Enter 查看指引，或设置 mihomo 路径切换 direct 模式"
                        .to_string(),
                ));
                return;
            }
            if !service_is_active().await {
                let _ = ui_tx.send(UiEvent::StartupNotice(
                    "mihomo 服务未运行：设置页 Enter 启动服务，或设置 mihomo 路径切换 direct 模式"
                        .to_string(),
                ));
            }
        });
    }

    fn on_bg_msg(&mut self, msg: BgMsg) {
        match msg {
            BgMsg::Traffic(frame) => {
                if self.state.traffic.len() >= TRAFFIC_HISTORY {
                    self.state.traffic.pop_front();
                }
                self.state.traffic.push_back(frame);
            }
            BgMsg::Api(ok) => self.set_api_ok(ok, None),
        }
    }

    /// 更新 api_ok 并在状态翻转时通知；通知去抖：3s 内同向变化不重复入列。
    /// （traffic 后台任务 Api(false) 与 5s 轮询 ConfigsRefreshed(Ok) 竞态
    /// 会反复翻转状态，未去抖时 [✗] 中断 / [✓] 已连接 刷屏。）
    fn set_api_ok(&mut self, ok: bool, err: Option<&str>) {
        if self.state.api_ok == ok {
            return;
        }
        self.state.api_ok = ok;
        let now = Instant::now();
        let debounced = self
            .api_notice_at
            .map(|t| now.duration_since(t) < API_NOTICE_DEBOUNCE)
            .unwrap_or(false);
        if debounced {
            return;
        }
        self.api_notice_at = Some(now);
        if ok {
            self.state.notice("[✓] API 已连接".to_string());
        } else if let Some(e) = err {
            self.state.notice(format!("[✗] API 连接失败: {e}"));
        } else {
            self.state.notice("[✗] API 连接中断".to_string());
        }
    }

    fn on_memory(&mut self, frame: MemoryFrame) {
        // 首帧 inuse==0 忽略（mihomo 刚启动时可能读到 0）
        if frame.inuse == 0 && self.state.mem_history.is_empty() {
            return;
        }
        if self.state.mem_history.len() >= TRAFFIC_HISTORY {
            self.state.mem_history.pop_front();
        }
        self.state.mem_history.push_back(frame.inuse);
    }

    fn on_log(&mut self, entry: LogEntry) {
        if self.state.logs.len() >= LOG_HISTORY {
            self.state.logs.pop_front();
        }
        self.state.logs.push_back(entry);
    }

    /// 连接快照 → 排序 → 截断上限 → 替换状态。
    fn on_conns(&mut self, snap: ConnSnapshot) {
        let mut conns = snap.connections;
        sort_connections(&mut conns);
        conns.truncate(CONNECTIONS_KEEP);
        self.state.connections = conns;
    }

    fn on_ui_event(&mut self, ev: UiEvent) {
        match ev {
            UiEvent::PatchDone { res, saved, label } => match res {
                Ok(()) => {
                    if saved {
                        self.state.notice(format!("[✓] 已切换{label}并已保存"));
                    } else {
                        // 热切成功但持久化失败：必须明确告知，禁止静默部分成功
                        self.state
                            .notice(format!("[!] 已切换{label}，但设置未能保存（重启后会丢失）"));
                    }
                }
                Err(e) => {
                    // 任何一步失败都给用户明确反馈；无论 saved 与否，立即拉一次真实
                    // 运行态纠正乐观更新（不等 5s 轮询）。
                    if saved {
                        self.popup_error(
                            "操作失败",
                            format!("「{label}」切换失败: {e}（已保存到配置，下次应用配置时生效）"),
                        );
                    } else {
                        self.popup_error(
                            "操作失败",
                            format!("「{label}」切换失败: {e}（且设置未能保存）"),
                        );
                    }
                    let _ = self.cmd_tx.send(UiCommand::ReloadConfigs);
                }
            },
            UiEvent::ApplyDone(res) => match res {
                Ok(outcome) => {
                    // 网络配置已变：刷新代理端口快照并立即重测一次出口 IP
                    *self.exit_ports.lock().unwrap() =
                        ProxyPorts::from_settings(&self.state.settings);
                    let _ = self.exit_trigger.send(());
                    let stdout = outcome.stdout.trim().to_string();
                    let stderr = outcome.stderr.trim().to_string();
                    if stdout.is_empty() && stderr.is_empty() {
                        self.state.notice("[✓] 配置已应用".to_string());
                    } else {
                        let mut lines: Vec<String> = Vec::new();
                        if !stdout.is_empty() {
                            lines.extend(stdout.lines().map(|s| s.to_string()));
                        }
                        if !stderr.is_empty() {
                            lines.push(format!("stderr: {stderr}"));
                        }
                        self.result_popup = Some(MessagePopup::new("应用结果".into(), lines));
                        self.state.notice("[✓] 配置已应用".to_string());
                    }
                    // 应用后刷新运行状态（进程模式重启后 PID 变化）
                    let _ = self.cmd_tx.send(UiCommand::RefreshStatus);
                    // 全局配置已应用（含各页 overrides），通知各页清除「未应用」类标志
                    for p in &mut self.pages {
                        p.on_apply_done(&self.state);
                    }
                }
                Err(e) => self.popup_error("应用失败", e),
            },
            UiEvent::SubscriptionFetched(idx, res) => match res {
                Ok(cache) => {
                    if let Some(sub) = self.state.subs.get_mut(idx) {
                        sub.cache = Some(cache.clone());
                        sub.last_fetch = Some(now_rfc3339());
                    }
                    if let Err(e) = save_subscriptions(&self.state.subs) {
                        self.state.notice(format!("[✗] 订阅存盘失败: {e}"));
                    }
                    match self.state.subs.get(idx) {
                        Some(sub) => self.state.notice(format!(
                            "[✓] 订阅「{}」已更新: {} 节点 / {} 组 / {} 规则",
                            sub.name,
                            cache.proxies.len(),
                            cache.proxy_groups.len(),
                            cache.rules.len()
                        )),
                        // 拉取期间订阅被删除：不显示空名通知，明确提示
                        None => self
                            .state
                            .notice("[!] 订阅拉取完成，但该订阅已被删除（缓存已丢弃）".to_string()),
                    }
                    // 订阅内容变化可能影响规则组：当前在规则组页则刷新
                    if self.current == 2 {
                        let _ = self.cmd_tx.send(UiCommand::RefreshGroups);
                    }
                }
                Err(e) => self.popup_error("订阅拉取失败", e),
            },
            UiEvent::ExitIp(res) => match res {
                Ok(info) => {
                    // 恢复成功：关闭先前失败留下的陈旧错误弹窗（内容已过时）
                    if self
                        .result_popup
                        .as_ref()
                        .is_some_and(|p| p.title() == "出口 IP 获取失败")
                    {
                        self.result_popup = None;
                    }
                    // 此前有失败：通知恢复；无失败历史时静默更新
                    if self.exit_ip_was_error {
                        self.exit_ip_was_error = false;
                        let label = match (&info.country, info.ip.as_str()) {
                            (Some(c), ip) => format!("{ip}「{c}」"),
                            (None, ip) => ip.to_string(),
                        };
                        self.state.notice(format!("[✓] 出口 IP 恢复: {label}"));
                    }
                    self.state.exit_ip = Some(info);
                }
                Err(e) => {
                    self.exit_ip_was_error = true;
                    self.state.exit_ip = None;
                    // 聚合错误可能很长（多端口 × 多端点）：notice 截断至首行
                    // 约 60 字符，完整错误保留在可滚动的 popup 中。
                    let head: String = e
                        .split('\n')
                        .next()
                        .unwrap_or("")
                        .chars()
                        .take(60)
                        .collect();
                    // 与 REST API 可达性交叉提示：区分"代理端口不通"与"mihomo 未运行"。
                    // api_confirmed 为 false 说明首次 /configs 检查未完成（启动竞态），
                    // REST 可达性未知，不做交叉判断。
                    let hint = if !self.state.api_confirmed {
                        "提示：REST API 状态未知（首次检查未完成），无法交叉判断"
                    } else if self.state.api_ok {
                        "提示：REST API 可达但代理端口不通：检查 mihomo 运行配置的代理端口是否与设置一致（或防火墙拦截）"
                    } else {
                        "提示：REST API 也不可达：mihomo 可能未运行（systemctl status mihomo）"
                    };
                    self.result_popup = Some(MessagePopup::new(
                        "出口 IP 获取失败".into(),
                        vec![e, hint.to_string()],
                    ));
                    self.state.notice(format!("[✗] 出口 IP 获取失败: {head}"));
                }
            },
            UiEvent::ConfigsRefreshed(res) => match res {
                // 无论 Ok/Err，首次 /configs 检查均已完成：REST 可达性已知，
                // 之后 exit_ip 失败可据此交叉判断。
                Ok(runtime) => {
                    self.state.runtime = runtime;
                    self.state.api_confirmed = true;
                    self.set_api_ok(true, None);
                }
                Err(e) => {
                    self.state.api_confirmed = true;
                    self.set_api_ok(false, Some(&e));
                }
            },
            UiEvent::GroupsRefreshed(res) => match res {
                Ok(groups) => self.state.proxy_groups = groups,
                // API 连接失败已有独立通知（set_api_ok），此处静默清空降级到订阅缓存展示
                Err(_) => self.state.proxy_groups.clear(),
            },
            UiEvent::GroupSwitched {
                group,
                target,
                result,
            } => match result {
                Ok(()) => {
                    self.state
                        .notice(format!("[✓] 已切换「{group}」→「{target}」"));
                    let _ = self.cmd_tx.send(UiCommand::RefreshGroups);
                }
                Err(e) => self.popup_error("切换失败", e),
            },
            UiEvent::GroupDelayDone {
                group,
                silent,
                result,
            } => match result {
                Ok(list) => {
                    // 弹窗内测速（silent）只更新缓存供弹窗展示/排序，不弹结果弹窗
                    self.state.group_delays.insert(group.clone(), list.clone());
                    if !silent {
                        self.result_popup = Some(MessagePopup::new(
                            format!("延迟测试：{group}"),
                            delay_lines(&list),
                        ));
                    }
                    self.state.notice(format!("[✓] 延迟测试完成：{group}"));
                    let _ = self.cmd_tx.send(UiCommand::RefreshGroups);
                }
                Err(e) => self.popup_error("延迟测试失败", e),
            },
            UiEvent::RunStatusDone(res) => match res {
                Ok(rs) => {
                    self.state.run_status = Some(rs);
                    // 设置页路径/状态显示值随状态刷新（不动表单编辑状态）
                    if self.current == 5 {
                        self.pages[5].refresh_state(&self.state);
                    }
                }
                Err(_) => self.state.run_status = None,
            },
            UiEvent::ProcActionDone(res) => match res {
                Ok(outcome) => {
                    let stdout = outcome.stdout.trim().to_string();
                    let stderr = outcome.stderr.trim().to_string();
                    if stdout.is_empty() && stderr.is_empty() {
                        self.state.notice("[✓] 操作完成".to_string());
                    } else {
                        let mut lines: Vec<String> = Vec::new();
                        if !stdout.is_empty() {
                            lines.extend(stdout.lines().map(|s| s.to_string()));
                        }
                        if !stderr.is_empty() {
                            lines.push(format!("stderr: {stderr}"));
                        }
                        self.result_popup = Some(MessagePopup::new("操作结果".into(), lines));
                        self.state.notice(format!(
                            "[✓] {}",
                            stdout.lines().next().unwrap_or("操作完成")
                        ));
                    }
                    let _ = self.cmd_tx.send(UiCommand::RefreshStatus);
                }
                Err(e) => self.popup_error("操作失败", e),
            },
            UiEvent::RestartDone(res) => {
                self.restarting = false;
                match res {
                    Ok(()) => {
                        self.state.notice("[✓] 核心已重启".to_string());
                        let _ = self.cmd_tx.send(UiCommand::ReloadConfigs);
                    }
                    Err(e) => {
                        self.popup_error("重启失败", e);
                        let _ = self.cmd_tx.send(UiCommand::ReloadConfigs);
                    }
                }
            }
            UiEvent::StartupNotice(msg) => self.state.notice(msg),
            UiEvent::LogLine(entry) => self.on_log(entry),
        }
    }

    fn popup_error(&mut self, title: &str, msg: String) {
        self.result_popup = Some(MessagePopup::new(title.into(), vec![msg.clone()]));
        self.state
            .notice(format!("[✗] {}", msg.lines().next().unwrap_or("")));
    }

    /// 分发 UiCommand：spawn 异步任务。
    fn spawn_command(&mut self, cmd: UiCommand) {
        let ui_tx = self.ui_tx.clone();
        let client = self.client.clone();
        match cmd {
            UiCommand::PatchConfigs {
                patch,
                saved,
                label,
            } => {
                tokio::spawn(async move {
                    let res =
                        tokio::time::timeout(Duration::from_secs(5), client.patch_configs(patch))
                            .await;
                    let res = match res {
                        Ok(Ok(())) => Ok(()),
                        Ok(Err(e)) => Err(e.to_string()),
                        Err(_) => Err("请求超时（5s）".to_string()),
                    };
                    let _ = ui_tx.send(UiEvent::PatchDone { res, saved, label });
                });
            }
            UiCommand::ApplyConfig(yaml) => {
                let sudo_tx = self.sudo_tx.clone();
                let mode = self.state.settings.run_mode;
                let bin = self.state.settings.mihomo_bin.clone();
                let bin_opt = (!bin.is_empty()).then_some(bin);
                tokio::spawn(async move {
                    // Windows：未设置 mihomo 路径 → 直接引导（Linux 走 PATH 查找，不受影响）
                    #[cfg(windows)]
                    if bin_opt.is_none() {
                        let _ = ui_tx.send(UiEvent::ApplyDone(Err(
                            "未设置 mihomo 路径：请先在设置页 Enter mihomo-bin 设置 mihomo 可执行文件路径"
                                .to_string(),
                        )));
                        return;
                    }
                    // 先 mihomo -t 校验，再非交互 sudo
                    match validate_config(&yaml, bin_opt.as_deref()).await {
                        Err(e) => {
                            let _ = ui_tx.send(UiEvent::ApplyDone(Err(e.to_string())));
                        }
                        Ok(()) => match apply_config(&yaml, true, mode).await {
                            Ok(outcome) => {
                                let _ = ui_tx.send(UiEvent::ApplyDone(Ok(outcome)));
                            }
                            Err(e @ crate::core::apply::ApplyError::NotInSudoers) => {
                                // 用户不在 sudoers：交互重试必败，直接报错并给修复指引
                                let _ = ui_tx.send(UiEvent::ApplyDone(Err(e.to_string())));
                            }
                            Err(crate::core::apply::ApplyError::SudoNeedsPassword) => {
                                // 主循环弹确认框，交互模式重试
                                let _ = sudo_tx.send(yaml);
                            }
                            Err(e) => {
                                let _ = ui_tx.send(UiEvent::ApplyDone(Err(e.to_string())));
                            }
                        },
                    }
                });
            }
            UiCommand::FetchSubscription(idx) => {
                let url = self.state.subs.get(idx).map(|s| s.url.clone());
                let port = self.state.settings.mixed_port;
                tokio::spawn(async move {
                    if let Some(url) = url {
                        let res = crate::core::subscription::fetch_subscription(&url, Some(port))
                            .await
                            .map_err(|e| e.to_string())
                            .and_then(|content| {
                                crate::core::subscription::parse_subscription(&content)
                                    .map_err(|e| e.to_string())
                            });
                        let _ = ui_tx.send(UiEvent::SubscriptionFetched(idx, res));
                    }
                });
            }
            UiCommand::FetchExitIp => {
                let ports = self.exit_ports.clone();
                let ui_tx = ui_tx.clone();
                tokio::spawn(async move {
                    // 锁内 clone 快照再 await（MutexGuard 非 Send，不能跨 await 持锁）；
                    // 重试覆盖 mihomo 重启窗口内的瞬时失败。
                    let r = exit_ip::fetch_exit_ip_retry(ports).await;
                    let _ = ui_tx.send(UiEvent::ExitIp(r));
                });
            }
            UiCommand::ReloadConfigs => {
                tokio::spawn(async move {
                    let res = client.get_configs().await.map_err(|e| e.to_string());
                    let _ = ui_tx.send(UiEvent::ConfigsRefreshed(res));
                });
            }
            UiCommand::RefreshGroups => {
                let ui_tx = ui_tx.clone();
                tokio::spawn(async move {
                    let res = client.get_proxies().await.map_err(|e| e.to_string());
                    let _ = ui_tx.send(UiEvent::GroupsRefreshed(res));
                });
            }
            UiCommand::SwitchGroup { group, target } => {
                let ui_tx = ui_tx.clone();
                tokio::spawn(async move {
                    let res = client
                        .switch_group(&group, &target)
                        .await
                        .map_err(|e| e.to_string());
                    let _ = ui_tx.send(UiEvent::GroupSwitched {
                        group,
                        target,
                        result: res,
                    });
                });
            }
            UiCommand::TestGroupDelay { group, silent } => {
                let ui_tx = ui_tx.clone();
                tokio::spawn(async move {
                    let res = client
                        .test_group_delay(&group)
                        .await
                        .map_err(|e| e.to_string());
                    let _ = ui_tx.send(UiEvent::GroupDelayDone {
                        group,
                        silent,
                        result: res,
                    });
                });
            }
            #[cfg(not(windows))]
            UiCommand::InstallSetup => {
                self.pending_confirm = Some((
                    ConfirmPopup::new(
                        "首次安装".into(),
                        "需要 root 权限执行安装（mihomo-apply 脚本与 sudoers）。是否继续？".into(),
                    ),
                    InteractiveTask::Install,
                ));
            }
            // Windows 无提权组件体系：InstallSetup 命令永不被触发，空实现保持 match 穷尽。
            #[cfg(windows)]
            UiCommand::InstallSetup => {}
            UiCommand::RefreshStatus => {
                let ui_tx = self.ui_tx.clone();
                tokio::spawn(async move {
                    #[cfg(not(windows))]
                    let (unit, active) = (service_unit_exists().await, service_is_active().await);
                    // 进程实例查询失败（未装脚本/未授权/超时）静默置 None，设置页显示"查询失败"
                    let proc = tokio::time::timeout(PROC_STATUS_TIMEOUT, proc_status())
                        .await
                        .ok()
                        .and_then(|r| r.ok());
                    #[cfg(not(windows))]
                    let _ = ui_tx.send(UiEvent::RunStatusDone(Ok(RunStatus {
                        service_unit: Some(unit),
                        service_active: Some(active),
                        proc,
                    })));
                    #[cfg(windows)]
                    let _ = ui_tx.send(UiEvent::RunStatusDone(Ok(RunStatus {
                        service_unit: None,
                        service_active: None,
                        proc,
                    })));
                });
            }
            UiCommand::ProcAction(op) => {
                let ui_tx = self.ui_tx.clone();
                tokio::spawn(async move {
                    match proc_control(op).await {
                        Ok(outcome) => {
                            let _ = ui_tx.send(UiEvent::ProcActionDone(Ok(outcome)));
                        }
                        Err(crate::core::apply::ApplyError::SudoNeedsPassword) => {
                            let _ = ui_tx.send(UiEvent::ProcActionDone(Err(
                                "sudo 需要密码：请确认已安装提权组件（仪表盘按 i）并重新登录终端"
                                    .to_string(),
                            )));
                        }
                        Err(e) => {
                            let _ = ui_tx.send(UiEvent::ProcActionDone(Err(e.to_string())));
                        }
                    }
                });
            }
            #[cfg(not(windows))]
            UiCommand::SystemdAction(op) => {
                let ui_tx = self.ui_tx.clone();
                tokio::spawn(async move {
                    // 直接以当前用户执行 systemctl（不 sudo）：桌面 polkit 代理弹窗认证，
                    // TUI 无需退出 raw 模式；结果复用 ProcActionDone 事件（弹窗/刷新通用）
                    match systemctl_control(op).await {
                        Ok(outcome) => {
                            let _ = ui_tx.send(UiEvent::ProcActionDone(Ok(outcome)));
                        }
                        Err(e) => {
                            let _ = ui_tx.send(UiEvent::ProcActionDone(Err(e.to_string())));
                        }
                    }
                });
            }
            // Windows 无 systemctl：进程操作统一走 ProcAction（process::control），空实现保持 match 穷尽。
            #[cfg(windows)]
            UiCommand::SystemdAction(_) => {}
            UiCommand::SaveMihomoBin(path) => {
                let confirm_text = {
                    #[cfg(windows)]
                    {
                        format!(
                            "将 mihomo 路径保存为 {path}（settings.toml，无需提权）。是否继续？"
                        )
                    }
                    #[cfg(not(windows))]
                    {
                        format!("需要 root 权限写入 {path} 到系统配置。是否继续？")
                    }
                };
                self.pending_confirm = Some((
                    ConfirmPopup::new("保存 mihomo 路径".into(), confirm_text),
                    InteractiveTask::SaveMihomoBin(path),
                ));
            }
            UiCommand::SetLogLevel(level) => {
                let _ = self.log_level_tx.send(level);
            }
            UiCommand::RestartCore => {
                let client = self.client.clone();
                let ui_tx = self.ui_tx.clone();
                tokio::spawn(async move {
                    let res = client.restart().await.map_err(|e| e.to_string());
                    let _ = ui_tx.send(UiEvent::RestartDone(res));
                });
            }
        }
    }

    /// 交互任务：离开 raw 模式/AltScreen → 执行（sudo 交互等）→ 恢复 → 结果弹窗。
    async fn run_interactive(&mut self, task: InteractiveTask) -> Result<(), BoxError> {
        let _ = execute!(io::stdout(), DisableMouseCapture);
        let _ = crossterm::terminal::disable_raw_mode();
        let _ = crossterm::execute!(io::stdout(), crossterm::terminal::LeaveAlternateScreen);

        let result = match task {
            InteractiveTask::Apply(yaml) => {
                let mode = self.state.settings.run_mode;
                apply_config(&yaml, false, mode)
                    .await
                    .map_err(|e| e.to_string())
            }
            #[cfg(not(windows))]
            InteractiveTask::Install => crate::service::installer::install()
                .await
                .map(|lines| ApplyOutcome {
                    success: true,
                    stdout: lines.join("\n"),
                    stderr: String::new(),
                })
                .map_err(|e| e.to_string()),
            InteractiveTask::SaveMihomoBin(path) => {
                #[cfg(windows)]
                {
                    let res = crate::service::process::save_bin(&path)
                        .await
                        .map(|lines| ApplyOutcome {
                            success: true,
                            stdout: lines.join("\n"),
                            stderr: String::new(),
                        })
                        .map_err(|e| e.to_string());
                    // 磁盘 + 内存双写：不同步的话，同会话内后续任何写 settings.toml 的操作
                    // （设置页 Ctrl+S/Ctrl+A、仪表盘 m/t/6 热键双写）会用陈旧空串
                    // 覆盖掉刚保存的路径。
                    if res.is_ok() {
                        self.state.settings.mihomo_bin = path.clone();
                    }
                    res
                }
                #[cfg(not(windows))]
                {
                    crate::service::installer::save_mihomo_bin(&path)
                        .await
                        .map(|lines| ApplyOutcome {
                            success: true,
                            stdout: lines.join("\n"),
                            stderr: String::new(),
                        })
                        .map_err(|e| e.to_string())
                }
            }
        };

        crossterm::terminal::enable_raw_mode()?;
        crossterm::execute!(io::stdout(), crossterm::terminal::EnterAlternateScreen)?;
        let _ = execute!(io::stdout(), EnableMouseCapture);
        self.terminal.clear()?;
        drain_input_queue().await;

        match result {
            Ok(outcome) => {
                let mut lines: Vec<String> = Vec::new();
                let stdout = outcome.stdout.trim();
                let stderr = outcome.stderr.trim();
                if !stdout.is_empty() {
                    lines.extend(stdout.lines().map(|s| s.to_string()));
                }
                if !stderr.is_empty() {
                    lines.push(format!("stderr: {stderr}"));
                }
                if lines.is_empty() {
                    lines.push("操作成功".to_string());
                }
                self.result_popup = Some(MessagePopup::new("执行结果".into(), lines));
                self.state.notice("[✓] 交互操作成功".to_string());
            }
            Err(e) => {
                self.result_popup = Some(MessagePopup::new("操作失败".into(), vec![e.clone()]));
                self.state
                    .notice(format!("[✗] {}", e.lines().next().unwrap_or("")));
            }
        }
        Ok(())
    }
}

/// 底栏渲染行计算（纯函数，供小终端回归测试）：
/// 输入底栏区域与全屏高度，返回 (通知可渲染行数, 按键提示行 hint_y)。
/// 保证任意输入下：通知行与 hint_y 都不越出 [0, area_height)；
/// 底栏完全不可见（bottom.y >= area_height）时返回 (0, None)（通知与提示都不渲染）。
/// 此前 bottom.height=0 时 hint_y = bottom.y.saturating_add(0) 可能等于 area_height，
/// 越出 buffer 顶导致 KeyHints 渲染 panic（h=1/2 终端）。
fn bottom_bar_rows(bottom: Rect, area_height: u16) -> (u16, Option<u16>) {
    if area_height == 0 || bottom.y >= area_height {
        return (0, None);
    }
    // 最后一行放按键提示，且必须落在屏幕内
    let hint_y = bottom
        .y
        .saturating_add(bottom.height.saturating_sub(1))
        .min(area_height - 1);
    // 通知从 bottom.y 开始，最多渲染到 hint_y-1（保留最后一行给提示）
    let notice_rows = hint_y.saturating_sub(bottom.y);
    (notice_rows, Some(hint_y))
}

/// sudo 需密码确认弹窗内容（纯函数，供回归测试）：
/// 首行保持原文，空行后附诊断提示（区分"未重新登录组未生效"与"sudoers 规则未生效"）。
fn sudo_password_popup_lines(has_group: bool) -> Vec<String> {
    let hint = if has_group {
        "诊断提示：已在 mihomo-admin 组但仍需密码：sudoers 规则未生效。\
         请在仪表盘页按 i 重新安装提权组件，或检查 /etc/sudoers 是否包含 @includedir /etc/sudoers.d"
    } else {
        "诊断提示：未检测到免密配置：当前会话不在 mihomo-admin 组。\
         请退出并重新登录终端（或执行 newgrp mihomo-admin）后重试，也可按 i 重新安装"
    };
    vec![
        "sudo 需要密码，将以交互模式重试应用配置。是否继续？".to_string(),
        String::new(),
        hint.to_string(),
    ]
}

fn page_hints(current: usize) -> Vec<(String, String)> {
    let mut hints: Vec<(String, String)> = match current {
        0 => {
            let mut v = vec![
                ("m".into(), "模式".into()),
                ("t".into(), "TUN".into()),
                ("6".into(), "IPv6".into()),
                ("r".into(), "出口IP".into()),
                ("R".into(), "重启".into()),
                ("s".into(), "设置".into()),
            ];
            // Windows：隐藏「i 安装」入口（提权组件安装为 Linux 专用）
            if !cfg!(windows) {
                v.push(("i".into(), "安装".into()));
            }
            v
        }
        1 => vec![
            ("a".into(), "添加".into()),
            ("Enter".into(), "激活".into()),
            ("r".into(), "刷新".into()),
            ("d".into(), "删除".into()),
        ],
        2 => vec![
            ("Enter".into(), "切换".into()),
            ("r".into(), "测速".into()),
            ("R".into(), "刷新".into()),
        ],
        3 => vec![
            ("n".into(), "新建".into()),
            ("Enter".into(), "编辑".into()),
            ("K/J".into(), "移动".into()),
            ("d".into(), "删除".into()),
            ("Ctrl+A".into(), "应用".into()),
        ],
        4 => vec![
            ("e".into(), "级别".into()),
            ("c".into(), "清空".into()),
            ("f".into(), "跟随".into()),
            ("↑↓".into(), "滚动".into()),
        ],
        5 => vec![
            ("Ctrl+S".into(), "保存".into()),
            ("Ctrl+A".into(), "应用".into()),
            ("Enter".into(), "编辑".into()),
            ("↑↓".into(), "移动".into()),
        ],
        // 兜底：current 实际恒在 0..=5（由 pages.len() 决定），此处仅满足穷尽性
        _ => vec![],
    };
    hints.push(("Tab".into(), "切页".into()));
    hints.push(("?".into(), "帮助".into()));
    hints.push(("q".into(), "退出".into()));
    hints
}

/// 延迟测试结果行：按延迟升序，超时（>= GROUP_DELAY_TIMEOUT_MS）排最后。
/// 空结果（整组全部超时且 mihomo 省略节点）→ 明确提示，避免空弹窗。
fn delay_lines(list: &[(String, u16)]) -> Vec<String> {
    if list.is_empty() {
        return vec!["全部节点超时".to_string()];
    }
    let mut items: Vec<(&String, u16)> = list.iter().map(|(n, ms)| (n, *ms)).collect();
    items.sort_by_key(|(_, ms)| *ms);
    items
        .iter()
        .map(|(n, ms)| {
            if *ms >= GROUP_DELAY_TIMEOUT_MS {
                format!("{n}  超时")
            } else {
                format!("{n}  {ms}ms")
            }
        })
        .collect()
}

/// 迁移旧版自定义规则组：非空则清空并返回提示（无旧数据返回 None）。纯函数便于测试。
fn migrate_legacy_groups(overrides: &mut Overrides) -> Option<String> {
    if overrides.groups.is_empty() {
        return None;
    }
    let n = overrides.groups.len();
    overrides.groups.clear();
    Some(format!(
        "[!] 已清空 {n} 个旧版自定义规则组（规则组页现只读展示订阅内容）"
    ))
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// 连接排序：最新建立的在前（start 降序），start 缺失排末尾；
/// 同 start 按 upload+download 降序（活跃连接靠前）。
fn sort_connections(conns: &mut [ConnInfo]) {
    conns.sort_by(|a, b| {
        let ka = a.start.map(|t| t.timestamp()).unwrap_or(i64::MIN);
        let kb = b.start.map(|t| t.timestamp()).unwrap_or(i64::MIN);
        kb.cmp(&ka)
            .then_with(|| (b.upload + b.download).cmp(&(a.upload + a.download)))
    });
}

/// traffic 后台任务：流式拉取 /traffic，失败 sleep 2s 重连；API 状态联动。
fn spawn_traffic_task(client: Arc<Client>, tx: mpsc::UnboundedSender<BgMsg>) {
    tokio::spawn(async move {
        loop {
            if let Ok(mut stream) = client.traffic_stream().await {
                let _ = tx.send(BgMsg::Api(true));
                while let Some(item) = stream.next().await {
                    match item {
                        Ok(frame) => {
                            if tx.send(BgMsg::Traffic(frame)).is_err() {
                                return;
                            }
                        }
                        Err(_) => break,
                    }
                }
            }
            let _ = tx.send(BgMsg::Api(false));
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    });
}

/// memory 后台任务：流式拉取 /memory，失败 sleep 2s 重连。
fn spawn_memory_task(client: Arc<Client>, tx: mpsc::UnboundedSender<MemoryFrame>) {
    tokio::spawn(async move {
        let mut first = true;
        loop {
            if let Ok(mut stream) = client.memory_stream().await {
                while let Some(item) = stream.next().await {
                    match item {
                        Ok(frame) => {
                            if first && frame.inuse == 0 {
                                first = false;
                                continue;
                            }
                            first = false;
                            if tx.send(frame).is_err() {
                                return;
                            }
                        }
                        Err(_) => break,
                    }
                }
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    });
}

/// logs 后台任务：流式拉取 /logs?level=；错误/EOF 路径 sleep 2s 重连；
/// 级别切换（level_rx）**立即**以新级别重连（不经过重连等待）。
fn spawn_logs_task(
    client: Arc<Client>,
    mut level_rx: mpsc::UnboundedReceiver<LogLevel>,
    tx: mpsc::UnboundedSender<LogEntry>,
) {
    tokio::spawn(async move {
        let mut level = LogLevel::Info;
        // 非零时表示需要重连等待；等待期间收到级别变化立即重连
        let mut retry_delay = Duration::ZERO;
        loop {
            if !retry_delay.is_zero() {
                tokio::select! {
                    _ = tokio::time::sleep(retry_delay) => {}
                    new_level = level_rx.recv() => match new_level {
                        Some(l) => level = l,
                        None => return,
                    },
                }
            }
            match client.log_stream(level).await {
                Ok(mut stream) => {
                    retry_delay = Duration::ZERO;
                    loop {
                        tokio::select! {
                            new_level = level_rx.recv() => match new_level {
                                Some(l) => {
                                    level = l;
                                    break; // 立即以新级别重连
                                }
                                None => return,
                            },
                            item = stream.next() => match item {
                                Some(Ok(entry)) => {
                                    if tx.send(entry).is_err() {
                                        return;
                                    }
                                }
                                Some(Err(_)) | None => {
                                    retry_delay = Duration::from_secs(2);
                                    break;
                                }
                            },
                        }
                    }
                }
                Err(_) => {
                    retry_delay = Duration::from_secs(2);
                }
            }
        }
    });
}

/// connections 后台任务：每 3s 轮询 /connections 快照；失败静默跳过
/// （下次轮询重试，保留上一次成功数据；API 状态联动由 traffic 任务负责）。
fn spawn_connections_task(client: Arc<Client>, tx: mpsc::UnboundedSender<ConnSnapshot>) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(CONNECTIONS_POLL);
        loop {
            interval.tick().await;
            if let Ok(snap) = client.get_connections().await {
                if tx.send(snap).is_err() {
                    return;
                }
            }
        }
    });
}

/// exit_ip 后台任务：每 60s 定时 + FetchExitIp 命令触发。
fn spawn_exit_ip_task(
    ports: Arc<Mutex<ProxyPorts>>,
    mut trigger: mpsc::UnboundedReceiver<()>,
    tx: mpsc::UnboundedSender<UiEvent>,
) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(60));
        loop {
            tokio::select! {
                _ = interval.tick() => {}
                _ = trigger.recv() => {}
            }
            // fetch_exit_ip_retry 内部锁内 clone 快照（MutexGuard 非 Send，
            // 不能跨 await 持锁），并重试覆盖 mihomo 重启窗口内的瞬时失败。
            let result = exit_ip::fetch_exit_ip_retry(ports.clone()).await;
            if tx.send(UiEvent::ExitIp(result)).is_err() {
                return;
            }
        }
    });
}

/// 丢弃交互阶段（sudo 输入等）遗留的按键队列，避免污染 TUI 状态。
async fn drain_input_queue() {
    tokio::time::sleep(Duration::from_millis(100)).await;
    while let Ok(true) = crossterm::event::poll(Duration::ZERO) {
        let _ = crossterm::event::read();
    }
}

/// 应用入口：初始化状态与后台任务，进入主循环。
pub async fn run() -> Result<(), BoxError> {
    let state = AppState::load();
    let client = Arc::new(Client::new(&state.settings));

    let (ui_tx, ui_rx) = mpsc::unbounded_channel();
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (traffic_tx, traffic_rx) = mpsc::unbounded_channel();
    let (memory_tx, memory_rx) = mpsc::unbounded_channel();
    let (conns_tx, conns_rx) = mpsc::unbounded_channel();
    let (log_tx, log_rx) = mpsc::unbounded_channel();
    let (log_level_tx, log_level_rx) = mpsc::unbounded_channel();
    let (sudo_tx, sudo_rx) = mpsc::unbounded_channel();
    let (exit_trigger, trigger_rx) = mpsc::unbounded_channel();
    let exit_ports = Arc::new(Mutex::new(ProxyPorts::from_settings(&state.settings)));

    spawn_traffic_task(client.clone(), traffic_tx);
    spawn_memory_task(client.clone(), memory_tx);
    spawn_connections_task(client.clone(), conns_tx);
    spawn_logs_task(client.clone(), log_level_rx, log_tx);
    spawn_exit_ip_task(exit_ports.clone(), trigger_rx, ui_tx.clone());

    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    terminal.hide_cursor()?;
    let _ = execute!(io::stdout(), EnableMouseCapture);

    let pages: Vec<Box<dyn Page>> = vec![
        Box::new(DashboardPage::new()),
        Box::new(SubscriptionsPage::new()),
        Box::new(GroupsPage::new()),
        Box::new(RulesPage::new()),
        Box::new(crate::ui::logs::LogsPage::new()),
        Box::new(crate::ui::settings::SettingsPage::new()),
    ];

    let mut app = App {
        state,
        pages,
        current: 0,
        tab_hits: Vec::new(),
        tabs_area: Rect::default(),
        client,
        ui_tx,
        cmd_tx,
        cmd_rx,
        sudo_tx,
        exit_trigger,
        log_level_tx,
        exit_ports,
        pending_confirm: None,
        restart_confirm: None,
        restarting: false,
        help_popup: None,
        result_popup: None,
        exit_ip_was_error: false,
        api_notice_at: None,
        last_status_refresh: None,
        tick_count: 0,
        quit: false,
        terminal,
    };
    // M6: 首次启动自动检测提权组件（README 承诺）。缺失时挂起确认框；
    // 用户确认后由 run_interactive 离开 raw 模式/AltScreen 执行交互式 sudo 安装，
    // 结束后恢复终端并弹结果（成功列日志行，失败列错误）。
    #[cfg(not(windows))]
    {
        // M6: 首次启动自动检测提权组件（README 承诺）。缺失时挂起确认框；
        // 用户确认后由 run_interactive 离开 raw 模式/AltScreen 执行交互式 sudo 安装，
        // 结束后恢复终端并弹结果（成功列日志行，失败列错误）。
        if crate::service::installer::needs_install().await {
            app.pending_confirm = Some((
                ConfirmPopup::new(
                    "首次安装".to_string(),
                    "检测到首次运行：缺少提权组件。\n\
                     将安装 /usr/local/sbin/mihomo-apply 提权脚本与\n\
                     /etc/sudoers.d/99-mihomo 规则（期间需要 sudo 密码）。\n\
                     是否继续？"
                        .to_string(),
                ),
                InteractiveTask::Install,
            ));
        }
    }
    app.spawn_startup_guard();
    let result = app
        .run_loop(traffic_rx, memory_rx, conns_rx, log_rx, ui_rx, sudo_rx)
        .await;
    let _ = execute!(io::stdout(), DisableMouseCapture);
    let _ = app.terminal.show_cursor();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::apply::ProcStatus;
    use crate::core::models::UserGroup;
    use crate::core::settings::with_settings_dir;
    use ratatui::backend::TestBackend;

    /// 底栏行计算纯函数：任意终端高度下提示行与通知行数都不越界。
    /// 回归：h=1/2 时 bottom.height=0，此前 hint_y=bottom.y 越出 buffer 顶；
    /// 动态底栏：可见通知 0..=3 行（通知 + 提示）全扫。
    #[test]
    fn bottom_bar_rows_always_in_bounds() {
        for n in 0..=3u16 {
            for h in 0..=30u16 {
                let area = Rect::new(0, 0, 30, h);
                let [_, _, bottom] = Layout::vertical([
                    Constraint::Length(3),
                    Constraint::Min(1),
                    Constraint::Length(1 + n),
                ])
                .areas(area);
                let (notice_rows, hint_y) = bottom_bar_rows(bottom, h);
                assert!(
                    notice_rows <= h,
                    "n={n} h={h}: notice_rows {notice_rows} > 高度"
                );
                match hint_y {
                    Some(y) => {
                        assert!(y < h, "n={n} h={h}: hint_y {y} >= 高度");
                        assert!(
                            y >= bottom.y,
                            "n={n} h={h}: hint_y {y} < bottom.y {}",
                            bottom.y
                        );
                        assert_eq!(
                            y - bottom.y,
                            notice_rows,
                            "n={n} h={h}: 通知行数应等于 hint_y - bottom.y"
                        );
                    }
                    None => assert_eq!(notice_rows, 0, "n={n} h={h}: 无提示行时通知也应为 0"),
                }
            }
        }
    }

    /// 小终端回归：h=0/1/2（及常规高度）整帧渲染不 panic 且按键提示行不越界。
    /// 此前 h=1/2 时 hint_y 越出 buffer，KeyHints 渲染触发 ratatui Buffer::index_of panic。
    #[test]
    fn draw_tiny_terminal_no_panic() {
        for h in [0u16, 1, 2, 3, 4, 5, 6, 7, 24] {
            let (mut app, _rx) = test_app(h);
            app.state.notice("[✓] 通知一".to_string());
            app.state.notice("[✗] 通知二".to_string());
            app.state.notice("[!] 通知三".to_string());
            let frame = app.draw().expect("draw 不应失败");
            // 高度 >= 3 时按键提示应渲染在最后一行（以 [ 起始），且不越界
            if h >= 3 {
                let cell = frame
                    .buffer
                    .cell((0, h - 1))
                    .expect("提示行应落在 buffer 内");
                assert!(
                    cell.symbol().starts_with('['),
                    "h={h}: 最后一行应为按键提示，实际 {:?}",
                    cell.symbol()
                );
            }
        }
    }

    /// 连接框可见路径回归：终端宽 60（>= 阈值 60），连接框应实际渲染；
    /// h=0/1/2 极小高度整帧渲染不 panic。
    #[test]
    fn draw_wide_terminal_connections_visible_no_panic() {
        for h in [0u16, 1, 2, 3, 4, 5, 6, 7, 24] {
            let (mut app, _rx) = test_app_with_width(60, h);
            app.state.connections = vec![
                conn("example.com", Some("2026-08-12T10:00:00Z"), 1024, 2048),
                conn("1.2.3.4:443", None, 0, 0),
            ];
            let frame = app.draw().expect("draw 不应失败");
            // 常规高度（h=24：top 3 + middle 1 + bottom 4 全部足额）：
            // 连接框标题应渲染在 body 首行（y=4, x=2 起为「连接」）
            if h == 24 {
                assert_eq!(
                    frame
                        .buffer
                        .cell((2, 4))
                        .expect("连接框标题应落在 buffer 内")
                        .symbol(),
                    "连",
                    "宽 60 终端应渲染连接框标题"
                );
            }
            // 高度 >= 3 时按键提示应渲染在最后一行（以 [ 起始），且不越界
            if h >= 3 {
                let cell = frame
                    .buffer
                    .cell((0, h - 1))
                    .expect("提示行应落在 buffer 内");
                assert!(
                    cell.symbol().starts_with('['),
                    "h={h}: 最后一行应为按键提示，实际 {:?}",
                    cell.symbol()
                );
            }
        }
    }

    /// sudo 密码确认弹窗：首行保持原文；两条诊断提示非空、互不相同，
    /// 且分别含"重新登录"/"重新安装"关键词；原文与提示间有分隔空行。
    #[test]
    fn sudo_password_popup_lines_hints() {
        let original = "sudo 需要密码，将以交互模式重试应用配置。是否继续？";
        let no_group = sudo_password_popup_lines(false);
        let has_group = sudo_password_popup_lines(true);

        assert_eq!(no_group.first().map(String::as_str), Some(original));
        assert_eq!(has_group.first().map(String::as_str), Some(original));
        assert_eq!(no_group.get(1).map(String::as_str), Some(""));
        assert_eq!(has_group.get(1).map(String::as_str), Some(""));

        let hint_no = no_group
            .iter()
            .find(|l| l.starts_with("诊断提示："))
            .expect("无组时应附诊断提示");
        let hint_yes = has_group
            .iter()
            .find(|l| l.starts_with("诊断提示："))
            .expect("有组时应附诊断提示");
        assert!(!hint_no.is_empty());
        assert!(!hint_yes.is_empty());
        assert_ne!(hint_no, hint_yes);
        assert!(hint_no.contains("重新登录"));
        assert!(hint_yes.contains("重新安装"));
    }

    /// 构造最小 App（TestBackend 终端），不触盘、不建后台任务。
    /// 返回 (App, probe_rx)：App 的 cmd_tx 被替换为 probe 通道发送端，
    /// 测试从 probe_rx 观察 App 发出的 UiCommand（App 自身 cmd_rx 无人消费无妨）。
    fn test_app(h: u16) -> (App<TestBackend>, mpsc::UnboundedReceiver<UiCommand>) {
        test_app_with_width(30, h)
    }

    /// 同 `test_app`，但可指定终端宽度（用于连接框可见/隐藏两条布局路径）。
    fn test_app_with_width(
        w: u16,
        h: u16,
    ) -> (App<TestBackend>, mpsc::UnboundedReceiver<UiCommand>) {
        let state = AppState {
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
        };
        let (ui_tx, _) = mpsc::unbounded_channel();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (sudo_tx, _) = mpsc::unbounded_channel();
        let (exit_trigger, _) = mpsc::unbounded_channel();
        let (log_level_tx, _) = mpsc::unbounded_channel();
        let client = Arc::new(Client::new(&state.settings));
        let exit_ports = Arc::new(Mutex::new(ProxyPorts::from_settings(&state.settings)));
        let mut app = App {
            state,
            pages: vec![
                Box::new(DashboardPage::new()),
                Box::new(SubscriptionsPage::new()),
                Box::new(GroupsPage::new()),
                Box::new(RulesPage::new()),
                Box::new(crate::ui::logs::LogsPage::new()),
                Box::new(crate::ui::settings::SettingsPage::new()),
            ],
            current: 0,
            tab_hits: Vec::new(),
            tabs_area: Rect::default(),
            client,
            ui_tx,
            cmd_tx,
            cmd_rx,
            sudo_tx,
            exit_trigger,
            log_level_tx,
            exit_ports,
            pending_confirm: None,
            restart_confirm: None,
            restarting: false,
            help_popup: None,
            result_popup: None,
            exit_ip_was_error: false,
            api_notice_at: None,
            last_status_refresh: None,
            tick_count: 0,
            quit: false,
            terminal: Terminal::new(TestBackend::new(w, h)).unwrap(),
        };
        let (probe_tx, probe_rx) = mpsc::unbounded_channel();
        app.cmd_tx = probe_tx;
        (app, probe_rx)
    }

    /// PatchDone 成功且已保存：正常成功通知。
    #[test]
    fn patch_done_ok_saved_notices_success() {
        let (mut app, _rx) = test_app(24);
        app.on_ui_event(UiEvent::PatchDone {
            res: Ok(()),
            saved: true,
            label: "TUN".into(),
        });
        assert!(
            app.state
                .notices
                .iter()
                .any(|(_, t)| t.contains("[✓] 已切换TUN并已保存")),
            "应通知成功并保存: {:?}",
            app.state.notices
        );
    }

    /// PatchDone 成功但设置未保存：必须明确告知「未能保存（重启后会丢失）」，
    /// 禁止静默部分成功。
    #[test]
    fn patch_done_ok_not_saved_notices_warning() {
        let (mut app, _rx) = test_app(24);
        app.on_ui_event(UiEvent::PatchDone {
            res: Ok(()),
            saved: false,
            label: "TUN".into(),
        });
        assert!(
            app.state
                .notices
                .iter()
                .any(|(_, t)| t.contains("已切换TUN") && t.contains("未能保存")),
            "应通知已切换但未能保存: {:?}",
            app.state.notices
        );
    }

    /// PatchDone 失败但已保存：错误弹窗提示「已保存到配置，下次应用配置时生效」，
    /// 并立即 ReloadConfigs 拉取真实运行态纠正乐观更新（不等 5s 轮询）。
    #[test]
    fn patch_done_err_saved_shows_popup_and_reloads() {
        let (mut app, mut rx) = test_app(24);
        app.on_ui_event(UiEvent::PatchDone {
            res: Err("x".into()),
            saved: true,
            label: "TUN".into(),
        });
        assert_eq!(app.result_popup.as_ref().unwrap().title(), "操作失败");
        // 弹窗内容（MessagePopup 字段私有，通过渲染 buffer 重建文本断言）
        let text = buffer_text(&mut app);
        assert!(text.contains("已保存到配置"), "弹窗应含已保存提示: {text}");
        // 换行可能截断短语（如“下次应用配置时生/效）”），断言用不跨行的片段
        assert!(
            text.contains("下次应用配置"),
            "弹窗应含下次应用配置生效提示: {text}"
        );
        assert!(
            !text.contains("重启后生效"),
            "文案不应再误导为裸重启即生效: {text}"
        );
        // 失败后立即拉真实运行态
        assert!(
            matches!(rx.try_recv(), Ok(UiCommand::ReloadConfigs)),
            "失败后应立即发送 ReloadConfigs"
        );
    }

    /// PatchDone 失败且未保存：错误弹窗提示「且设置未能保存」，同样立即 ReloadConfigs。
    #[test]
    fn patch_done_err_not_saved_shows_popup_and_reloads() {
        let (mut app, mut rx) = test_app(24);
        app.on_ui_event(UiEvent::PatchDone {
            res: Err("x".into()),
            saved: false,
            label: "IPv6".into(),
        });
        assert_eq!(app.result_popup.as_ref().unwrap().title(), "操作失败");
        let text = buffer_text(&mut app);
        assert!(
            text.contains("且设置未能保存"),
            "弹窗应含未保存提示: {text}"
        );
        assert!(
            matches!(rx.try_recv(), Ok(UiCommand::ReloadConfigs)),
            "失败后应立即发送 ReloadConfigs"
        );
    }

    /// 渲染后从 buffer 重建可见文本（行间不加分隔符、忽略空白）：
    /// 弹窗内容按宽度换行，行拼接后即可还原原文，用于断言私有字段内容。
    fn buffer_text(app: &mut App<TestBackend>) -> String {
        let frame = app.draw().expect("draw 应成功");
        (0..frame.buffer.area.height)
            .flat_map(|y| {
                (0..frame.buffer.area.width).map(move |x| {
                    frame
                        .buffer
                        .cell((x, y))
                        .map(|c| c.symbol().to_string())
                        .unwrap_or_default()
                })
            })
            .collect::<String>()
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect()
    }

    /// 出口 IP 失败后恢复：关闭陈旧错误弹窗 + 通知恢复；再次成功（无失败历史）
    /// 不重复通知。
    #[test]
    fn exit_ip_recovery_closes_stale_popup_and_notices() {
        let (mut app, _rx) = test_app(24);
        // 失败：弹出「出口 IP 获取失败」弹窗
        app.on_ui_event(UiEvent::ExitIp(Err("出口 IP 获取失败: 连接被拒".into())));
        assert!(app.result_popup.is_some(), "失败应弹出错误弹窗");
        assert_eq!(
            app.result_popup.as_ref().unwrap().title(),
            "出口 IP 获取失败"
        );
        assert!(app.state.exit_ip.is_none());
        // 恢复：陈旧弹窗关闭 + 通知恢复
        app.on_ui_event(UiEvent::ExitIp(Ok(ExitInfo {
            ip: "1.2.3.4".into(),
            country: None,
        })));
        assert!(app.result_popup.is_none(), "恢复成功后应关闭陈旧错误弹窗");
        assert_eq!(
            app.state.exit_ip.as_ref().map(|e| e.ip.as_str()),
            Some("1.2.3.4")
        );
        assert_eq!(
            app.state
                .exit_ip
                .as_ref()
                .and_then(|e| e.country.as_deref()),
            None
        );
        assert!(
            app.state
                .notices
                .iter()
                .any(|(_, t)| t.contains("[✓] 出口 IP 恢复: 1.2.3.4")),
            "应通知恢复: {:?}",
            app.state.notices
        );
        // 再次成功：无失败历史，静默更新不重复通知
        app.on_ui_event(UiEvent::ExitIp(Ok(ExitInfo {
            ip: "5.6.7.8".into(),
            country: None,
        })));
        assert_eq!(
            app.state.exit_ip.as_ref().map(|e| e.ip.as_str()),
            Some("5.6.7.8")
        );
        assert!(
            !app.state.notices.iter().any(|(_, t)| t.contains("5.6.7.8")),
            "无失败历史时不应再通知恢复: {:?}",
            app.state.notices
        );
    }

    /// 恢复成功只关闭「出口 IP 获取失败」弹窗：其他弹窗（如应用结果）不受影响。
    #[test]
    fn exit_ip_recovery_keeps_unrelated_popup() {
        let (mut app, _rx) = test_app(24);
        app.on_ui_event(UiEvent::ExitIp(Err("出口 IP 获取失败: 连接被拒".into())));
        assert!(app.exit_ip_was_error, "失败应置位 exit_ip_was_error");
        // 用户随后打开了另一个弹窗
        app.result_popup = Some(MessagePopup::new("应用结果".into(), vec!["x".into()]));
        app.on_ui_event(UiEvent::ExitIp(Ok(ExitInfo {
            ip: "1.2.3.4".into(),
            country: None,
        })));
        assert!(app.result_popup.is_some(), "非出口 IP 弹窗不应被关闭");
        assert_eq!(
            app.result_popup.as_ref().unwrap().title(),
            "应用结果",
            "应用结果弹窗应原样保留"
        );
        assert!(!app.exit_ip_was_error, "恢复后应清除失败标记");
        assert!(
            app.state
                .notices
                .iter()
                .any(|(_, t)| t.contains("[✓] 出口 IP 恢复: 1.2.3.4")),
            "应通知恢复: {:?}",
            app.state.notices
        );
    }

    /// 恢复通知带国家名：country 存在时通知为 `IP「国家」` 格式。
    #[test]
    fn exit_ip_recovery_notice_includes_country() {
        let (mut app, _rx) = test_app(24);
        app.on_ui_event(UiEvent::ExitIp(Err("出口 IP 获取失败: 连接被拒".into())));
        app.on_ui_event(UiEvent::ExitIp(Ok(ExitInfo {
            ip: "43.243.192.97".into(),
            country: Some("中国香港".into()),
        })));
        assert!(
            app.state
                .notices
                .iter()
                .any(|n| n.1.contains("[✓] 出口 IP 恢复: 43.243.192.97「中国香港」")),
            "应通知恢复且带国家: {:?}",
            app.state.notices
        );
    }

    /// 迁移纯函数：无旧数据不提示；有旧数据清空并返回含数量的提示。
    #[test]
    fn migrate_legacy_groups_clears_and_reports() {
        let mut o = Overrides::default();
        assert_eq!(migrate_legacy_groups(&mut o), None, "无旧数据不提示");
        o.groups.push(UserGroup {
            name: "旧组".into(),
            group_type: "select".into(),
            url: String::new(),
            interval: 0,
            tolerance: 0,
            proxies: vec!["节点1".into()],
        });
        let msg = migrate_legacy_groups(&mut o).expect("有旧数据应提示");
        assert!(o.groups.is_empty(), "应清空");
        assert!(msg.contains("1 个旧版自定义规则组"), "提示应含数量: {msg}");
    }

    /// GroupsRefreshed Ok 更新状态；Err 清空降级（订阅缓存展示）。
    #[test]
    fn groups_refreshed_updates_state() {
        let (mut app, _rx) = test_app(24);
        let groups = vec![GroupInfo {
            name: "手动选择".into(),
            group_type: "Selector".into(),
            now: Some("节点A".into()),
            all: vec!["节点A".into(), "DIRECT".into()],
        }];
        app.on_ui_event(UiEvent::GroupsRefreshed(Ok(groups.clone())));
        assert_eq!(app.state.proxy_groups, groups);
        app.on_ui_event(UiEvent::GroupsRefreshed(Err("连接失败".into())));
        assert!(app.state.proxy_groups.is_empty(), "失败应清空降级");
    }

    /// 切换成功：成功通知 + 发送 RefreshGroups 命令。
    #[test]
    fn group_switched_success_notices_and_refreshes() {
        let (mut app, mut rx) = test_app(24);
        app.on_ui_event(UiEvent::GroupSwitched {
            group: "手动选择".into(),
            target: "节点A".into(),
            result: Ok(()),
        });
        assert!(
            app.state
                .notices
                .iter()
                .any(|n| n.1.contains("已切换「手动选择」→「节点A」")),
            "应通知切换成功: {:?}",
            app.state.notices
        );
        let cmd = rx.try_recv().expect("应发送刷新命令");
        assert!(matches!(cmd, UiCommand::RefreshGroups), "命令: {cmd:?}");
    }

    /// 切换失败：错误弹窗，不发刷新命令。
    #[test]
    fn group_switched_failure_popup() {
        let (mut app, _rx) = test_app(24);
        app.on_ui_event(UiEvent::GroupSwitched {
            group: "g".into(),
            target: "x".into(),
            result: Err("HTTP 状态 400".into()),
        });
        assert_eq!(app.result_popup.as_ref().unwrap().title(), "切换失败");
    }

    /// 延迟测试完成：结果弹窗 + 缓存 + 刷新命令。
    #[test]
    fn group_delay_done_popup_and_refresh() {
        let (mut app, mut rx) = test_app(24);
        let list = vec![("节点B".to_string(), 8000), ("节点A".to_string(), 123)];
        app.on_ui_event(UiEvent::GroupDelayDone {
            group: "自动选择".into(),
            silent: false,
            result: Ok(list.clone()),
        });
        let popup = app.result_popup.as_ref().expect("应有结果弹窗");
        assert_eq!(popup.title(), "延迟测试：自动选择");
        assert_eq!(
            app.state.group_delays.get("自动选择"),
            Some(&list),
            "结果应缓存"
        );
        let _ = rx.try_recv().expect("应发送刷新命令");
    }

    /// 静默测速完成（弹窗内 s）：不弹结果弹窗，但结果存入 group_delays 且仍刷新。
    #[test]
    fn group_delay_done_silent_no_popup_but_stored() {
        let (mut app, mut rx) = test_app(24);
        let list = vec![("节点A".to_string(), 123), ("节点B".to_string(), 8000)];
        app.on_ui_event(UiEvent::GroupDelayDone {
            group: "手动选择".into(),
            silent: true,
            result: Ok(list.clone()),
        });
        assert!(app.result_popup.is_none(), "静默测速不应弹结果弹窗");
        assert_eq!(
            app.state.group_delays.get("手动选择"),
            Some(&list),
            "结果应入缓存"
        );
        assert!(
            app.state
                .notices
                .iter()
                .any(|n| n.1.contains("延迟测试完成")),
            "应有完成通知: {:?}",
            app.state.notices
        );
        let cmd = rx.try_recv().expect("静默测速仍应发送刷新命令");
        assert!(matches!(cmd, UiCommand::RefreshGroups), "命令: {cmd:?}");
    }

    /// 延迟结果行：按延迟升序，超时（>= GROUP_DELAY_TIMEOUT_MS）标记并排最后。
    #[test]
    fn delay_lines_sort_and_timeout() {
        let lines = delay_lines(&[
            ("B".to_string(), 8000),
            ("A".to_string(), 123),
            ("C".to_string(), 5000),
        ]);
        assert_eq!(
            lines,
            vec![
                "A  123ms".to_string(),
                "C  超时".to_string(),
                "B  超时".to_string()
            ],
            "升序 + 超时标记: {lines:?}"
        );
    }

    /// 空结果（整组全部超时且 mihomo 省略节点）→ 明确提示而非空弹窗。
    #[test]
    fn delay_lines_empty_shows_all_timeout() {
        assert_eq!(delay_lines(&[]), vec!["全部节点超时".to_string()]);
    }

    /// 切页触发刷新：进入规则组页（2）发 RefreshGroups；切到其他页不发。
    #[test]
    fn switch_page_refreshes_groups() {
        let (mut app, mut rx) = test_app(24);
        app.switch_page(2);
        assert!(matches!(rx.try_recv(), Ok(UiCommand::RefreshGroups)));
        app.switch_page(0);
        assert!(rx.try_recv().is_err(), "非规则组页不应发刷新");
    }

    /// RunStatusDone 更新 run_status；失败置 None。
    #[test]
    fn run_status_done_updates_state() {
        let (mut app, _rx) = test_app(20);
        app.on_ui_event(UiEvent::RunStatusDone(Ok(RunStatus {
            service_unit: Some(true),
            service_active: Some(true),
            proc: Some(ProcStatus {
                bin: Some("/usr/bin/mihomo".into()),
                pid: Some(1),
                running: true,
            }),
        })));
        let rs = app.state.run_status.as_ref().expect("应更新 run_status");
        assert_eq!(rs.service_active, Some(true));
        assert_eq!(rs.proc.as_ref().unwrap().pid, Some(1));
        app.on_ui_event(UiEvent::RunStatusDone(Err("查询失败".into())));
        assert!(app.state.run_status.is_none(), "失败应清空");
    }

    /// ProcActionDone 成功 → notice + 触发 RefreshStatus（cmd 队列出现）。
    #[test]
    fn proc_action_done_notices_and_refreshes() {
        let (mut app, mut cmd_rx) = test_app(20);
        app.on_ui_event(UiEvent::ProcActionDone(Ok(ApplyOutcome {
            success: true,
            stdout: "OK: mihomo 已停止".into(),
            stderr: String::new(),
        })));
        assert!(app
            .state
            .notices
            .iter()
            .any(|(_, t)| t.contains("mihomo 已停止")));
        // 状态刷新命令已入队
        let got = cmd_rx.try_recv();
        assert!(
            matches!(got, Ok(UiCommand::RefreshStatus)),
            "应触发状态刷新: {got:?}"
        );
    }

    /// StartupNotice → notice 入列。
    #[test]
    fn startup_notice_queues_notice() {
        let (mut app, _rx) = test_app(20);
        app.on_ui_event(UiEvent::StartupNotice("mihomo 服务未运行".into()));
        assert!(app
            .state
            .notices
            .iter()
            .any(|(_, t)| t.contains("mihomo 服务未运行")));
    }

    fn conn(id: &str, start: Option<&str>, upload: u64, download: u64) -> ConnInfo {
        ConnInfo {
            id: id.into(),
            start: start
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .map(|dt| dt.with_timezone(&chrono::Utc)),
            upload,
            download,
            ..ConnInfo::default()
        }
    }

    /// 排序：最新建立的在前；缺失 start 排末尾；同 start 按流量降序。
    #[test]
    fn sort_connections_order() {
        let mut conns = vec![
            conn("old", Some("2026-08-12T10:00:00Z"), 1, 1),
            conn("missing", None, 999, 999),
            conn("new", Some("2026-08-12T11:00:00Z"), 0, 0),
            conn("same-a", Some("2026-08-12T10:30:00Z"), 5, 5),
            conn("same-b", Some("2026-08-12T10:30:00Z"), 100, 100),
        ];
        sort_connections(&mut conns);
        let ids: Vec<&str> = conns.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, vec!["new", "same-b", "same-a", "old", "missing"]);
    }

    /// 排序稳定性：start 与 upload+download 完全相同的连接，
    /// 排序后保持原始相对顺序（sort_by 为稳定排序）。
    #[test]
    fn sort_connections_stable_for_identical_keys() {
        let mut conns = vec![
            conn("dup-1", Some("2026-08-12T10:00:00Z"), 10, 10),
            conn("dup-2", Some("2026-08-12T10:00:00Z"), 10, 10),
            conn("dup-3", Some("2026-08-12T10:00:00Z"), 10, 10),
        ];
        sort_connections(&mut conns);
        let ids: Vec<&str> = conns.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, vec!["dup-1", "dup-2", "dup-3"]);
    }

    /// 组截止时间 = 组内「到达+时长」最大的一条（锚定时间最长，不管新旧）。
    #[test]
    fn notice_deadline_anchors_longest() {
        let now = Instant::now();
        // 仅成功通知：5s
        let d = notice_deadline(&[(now, "[✓] a".to_string())]).unwrap();
        assert_eq!(d, now + NOTICE_OK_TTL);
        // 错误通知：10s
        let d = notice_deadline(&[(now, "[✗] a".to_string())]).unwrap();
        assert_eq!(d, now + NOTICE_ERR_TTL);
        // 旧成功 + 新错误：锚定新错误的 10s
        let old = now - Duration::from_secs(3);
        let d =
            notice_deadline(&[(old, "[✓] old".to_string()), (now, "[✗] new".to_string())]).unwrap();
        assert_eq!(d, now + NOTICE_ERR_TTL);
        // 旧错误 + 新成功：仍锚定错误（到达早，但时长最长）
        let old_err = now - Duration::from_secs(2);
        let d = notice_deadline(&[
            (old_err, "[✗] old".to_string()),
            (now, "[✓] new".to_string()),
        ])
        .unwrap();
        assert_eq!(d, old_err + NOTICE_ERR_TTL);
        // 空组：None
        assert_eq!(notice_deadline(&[]), None);
    }

    /// 状态刷新节流：未刷新过 → 立即；间隔内 → 不刷；超过间隔 → 刷。
    #[test]
    fn should_refresh_status_throttles() {
        let now = Instant::now();
        assert!(should_refresh_status(None, now), "从未刷新应立即刷新");
        assert!(
            !should_refresh_status(Some(now), now),
            "刚刷新过不应立即再刷"
        );
        let t1 = now + Duration::from_millis(1900);
        assert!(!should_refresh_status(Some(now), t1), "间隔内不应刷新");
        let t2 = now + Duration::from_secs(2) + Duration::from_millis(50);
        assert!(should_refresh_status(Some(now), t2), "超过间隔应刷新");
    }

    /// 过期整组在 draw 时同时清除（不留半组）。
    #[test]
    fn expired_notices_cleared_on_draw() {
        let (mut app, _rx) = test_app(24);
        app.state.notices.push_back((
            Instant::now() - Duration::from_secs(60),
            "[✓] 旧通知".to_string(),
        ));
        app.state.notices.push_back((
            Instant::now() - Duration::from_secs(60),
            "[✗] 更旧".to_string(),
        ));
        let _ = app.draw().expect("draw 不应失败");
        assert!(app.state.notices.is_empty(), "过期整组应被清除");
    }

    /// 无通知时底栏收成 1 行（仅按键提示）；有通知时通知文本可见。
    #[test]
    fn bottom_bar_collapses_without_notices() {
        let (mut app, _rx) = test_app(24);
        let frame = app.draw().expect("draw 不应失败");
        let cell = frame.buffer.cell((0, 23)).expect("提示行应在最后一行");
        assert!(
            cell.symbol().starts_with('['),
            "无通知时最后一行应为按键提示: {:?}",
            cell.symbol()
        );
        // 有通知：通知文本渲染可见
        app.state.notice("[✓] 测试通知".to_string());
        let text = buffer_text(&mut app);
        assert!(text.contains("测试通知"), "通知应可见: {text}");
    }

    /// 数字键 5 切到日志页（index 4）。
    #[test]
    fn tab_key_5_switches_to_logs_page() {
        let (mut app, _rx) = test_app(24);
        assert_eq!(app.pages.len(), 6);
        let _ = app.handle_key(KeyEvent::new(KeyCode::Char('5'), KeyModifiers::NONE));
        assert_eq!(app.current, 4);
    }

    /// 数字键 6 切到设置页（index 5）。
    #[test]
    fn tab_key_6_switches_to_settings_page() {
        let (mut app, _rx) = test_app(24);
        assert_eq!(app.pages.len(), 6, "应挂载 6 个页面");
        let _ = app.handle_key(KeyEvent::new(KeyCode::Char('6'), KeyModifiers::NONE));
        assert_eq!(app.current, 5);
    }

    /// s 全局跳转设置页；已在设置页时按 s 不切走（s 供字段输入）。
    #[test]
    fn s_key_switches_to_settings_page() {
        let (mut app, _rx) = test_app(24);
        assert_eq!(app.current, 0);
        let _ = app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE));
        assert_eq!(app.current, 5, "s 应全局跳转设置页");
        let _ = app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE));
        assert_eq!(app.current, 5, "设置页内 s 不应切走");
    }

    /// switch_page(5) 触发 on_enter：设置页字段从 st.settings 同步。
    /// 验证方式：先改 st.settings，切页后 Ctrl+S 落盘的应是新值。
    #[test]
    fn switch_page_syncs_settings_page_fields() {
        with_settings_dir(|| {
            let (mut app, _rx) = test_app(24);
            app.state.settings.port = 9999;
            app.switch_page(5);
            // 页面字段应已同步为 9999：Ctrl+S 直接落盘
            let cmd = app.pages[5].handle_key(
                KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
                &mut app.state,
            );
            assert!(cmd.is_none(), "仅保存不应返回命令");
            let loaded = crate::core::settings::load_settings().unwrap();
            assert_eq!(loaded.port, 9999, "on_enter 同步后 Ctrl+S 应落盘新值");
        });
    }

    /// 进入设置页编辑模式后 Esc 退出编辑模式而非退出程序（P0-1 回归）。
    /// 全链路走 app.handle_key：Down×21 聚焦 dns.listen（Text，index 21）、
    /// Enter 进编辑、输入 x、Esc；退出编辑后 x 不再插入，Ctrl+S 落盘验证。
    #[test]
    fn esc_in_edit_mode_does_not_quit() {
        with_settings_dir(|| {
            let (mut app, _rx) = test_app(24);
            app.switch_page(5);
            // Down×21：focused 0 → 21（dns.listen，Text 字段）
            for _ in 0..21 {
                assert!(app
                    .handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
                    .is_none());
            }
            // Enter 进入编辑（Enter/Down 不在全局 match，走 _ 兜底到页面）
            assert!(app
                .handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
                .is_none());
            // 编辑模式：x 是输入字符，不触发任何全局键
            assert!(app
                .handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE))
                .is_none());
            // Esc 必须退出编辑模式而非退出程序
            assert!(!matches!(
                app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
                Some(KeyAction::Quit)
            ));
            assert_eq!(app.current, 5, "Esc 不应切页");
            assert!(!app.quit, "Esc 不应退出程序");
            // 已退出编辑：再按 x 不再插入；Ctrl+S（经 app.handle_key）落盘验证
            // 值只含编辑期插入的一个 x（"0.0.0.0:1053x"）
            assert!(app
                .handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE))
                .is_none());
            assert!(app
                .handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL))
                .is_none());
            let loaded = crate::core::settings::load_settings().unwrap();
            assert_eq!(loaded.dns.listen, "0.0.0.0:1053x", "Esc 后 x 不应再插入");
        });
    }

    /// 设置页导航态 Tab/←→/数字与其他页一致：全局切页（新契约回归，替代旧的
    /// “设置页拦截切页键做字段操作”行为）。每次切走后重新 switch_page(5) 回设置页。
    #[test]
    fn tab_arrows_digits_switch_page_from_settings() {
        let (mut app, _rx) = test_app(24);
        app.switch_page(5);
        assert_eq!(app.current, 5);
        // Tab：设置页 → 仪表盘（index 0）
        assert!(app
            .handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
            .is_none());
        assert_eq!(app.current, 0, "设置页导航态 Tab 应全局切页");
        // 回到设置页，Left：→ 日志（index 4）
        app.switch_page(5);
        assert!(app
            .handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE))
            .is_none());
        assert_eq!(app.current, 4, "设置页导航态 Left 应全局切页");
        // 回到设置页，'3'：→ 规则组（index 2）
        app.switch_page(5);
        assert!(app
            .handle_key(KeyEvent::new(KeyCode::Char('3'), KeyModifiers::NONE))
            .is_none());
        assert_eq!(app.current, 2, "设置页导航态数字键应全局切页");
    }

    /// 编辑 Number 字段时输入数字不切页（P0-1 回归）：
    /// port（index 9）编辑中输入 '5' 保持当前页、不退出。
    #[test]
    fn digits_typed_in_edit_mode_do_not_switch() {
        let (mut app, _rx) = test_app(24);
        app.switch_page(5);
        // Down×9：focused 0 → 9（port，Number 字段）
        for _ in 0..9 {
            app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        }
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(!matches!(
            app.handle_key(KeyEvent::new(KeyCode::Char('5'), KeyModifiers::NONE)),
            Some(KeyAction::Quit)
        ));
        assert_eq!(app.current, 5, "编辑模式输入数字不应切页");
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    }

    /// 编辑 Text 字段时输入 'q' 不退出程序（P0-1 回归）：
    /// nameserver（index 24，Text）编辑中输入 'q' 保持运行（dns-query 可完整输入）。
    #[test]
    fn q_typed_in_edit_mode_does_not_quit() {
        let (mut app, _rx) = test_app(24);
        app.switch_page(5);
        // Down×24：focused 0 → 24（dns.nameserver，Text 字段）
        for _ in 0..24 {
            app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        }
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(!matches!(
            app.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)),
            Some(KeyAction::Quit)
        ));
        assert_eq!(app.current, 5, "编辑模式输入 q 不应切页");
        assert!(!app.quit, "编辑模式输入 q 不应退出程序");
    }

    /// 状态行焦点字段提示（P2-2，spec §2）：设置页状态行渲染「当前: run-mode」。
    #[test]
    fn settings_status_line_shows_focused_field() {
        let (mut app, _rx) = test_app_with_width(120, 24);
        app.switch_page(5);
        let text = buffer_text(&mut app);
        assert!(
            text.contains("当前:run-mode"),
            "状态行应含焦点字段提示: {text}"
        );
    }

    /// 状态行编辑态「编辑中」标记（新契约）：Enter 进编辑后状态行渲染 [编辑中]。
    #[test]
    fn settings_status_line_shows_editing_marker() {
        let (mut app, _rx) = test_app_with_width(120, 24);
        app.switch_page(5);
        assert!(
            !buffer_text(&mut app).contains("编辑中"),
            "导航态不应显示编辑中"
        );
        // Down×9 聚焦 port（Number 字段）再 Enter 进编辑态
        for _ in 0..9 {
            app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        }
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(
            buffer_text(&mut app).contains("编辑中"),
            "编辑态状态行应含 [编辑中] 标记"
        );
    }

    /// 日志页 handle_key('e') 发 SetLogLevel 命令并循环级别
    /// （Info.next()=Debug，见 client.rs log_level_cycle_and_str 固定的循环契约）。
    #[test]
    fn logs_page_e_key_cycles_level() {
        let (mut app, _rx) = test_app(24);
        app.current = 4;
        let cmd = app.pages[4].handle_key(
            KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
            &mut app.state,
        );
        assert!(matches!(cmd, Some(UiCommand::SetLogLevel(LogLevel::Debug))));
    }

    /// 日志环形缓冲：超过 LOG_HISTORY 淘汰最旧。
    #[test]
    fn logs_push_truncates_at_cap() {
        let (mut app, _rx) = test_app(24);
        for i in 0..(LOG_HISTORY + 100) {
            app.on_log(LogEntry {
                time: None,
                level: LogLevel::Info,
                message: format!("m{i}"),
            });
        }
        assert_eq!(app.state.logs.len(), LOG_HISTORY);
        assert_eq!(app.state.logs.front().unwrap().message, "m100");
        assert_eq!(
            app.state.logs.back().unwrap().message,
            format!("m{}", LOG_HISTORY + 99)
        );
    }

    /// SetLogLevel 命令转发到 logs 后台任务通道。
    #[test]
    fn set_log_level_forwards_to_task_channel() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (mut app, _rx) = test_app(24);
        app.log_level_tx = tx;
        app.spawn_command(UiCommand::SetLogLevel(LogLevel::Debug));
        assert_eq!(rx.try_recv(), Ok(LogLevel::Debug));
    }

    /// on_conns：快照替换 + 上限截断 200。
    #[test]
    fn on_conns_truncates_and_sorts() {
        let (mut app, _rx) = test_app(24);
        let snap = ConnSnapshot {
            connections: (0..250)
                .map(|i| conn(&format!("c{i}"), Some("2026-08-12T10:00:00Z"), i, 0))
                .collect(),
            ..ConnSnapshot::default()
        };
        app.on_conns(snap);
        assert_eq!(app.state.connections.len(), CONNECTIONS_KEEP);
        // 排序后最新在上：所有连接 start 相同 → 流量降序 → c249 在最前
        assert_eq!(app.state.connections[0].id, "c249");
    }

    #[test]
    fn restart_only_on_dashboard() {
        let (mut app, mut rx) = test_app(24);
        app.current = 1;
        let result = app.handle_key(KeyEvent::new(KeyCode::Char('R'), KeyModifiers::NONE));
        assert!(result.is_none());
        assert!(app.restart_confirm.is_none(), "非首页按 R 不应弹确认");
        assert!(rx.try_recv().is_err(), "非首页不应发送 RestartCore");
        // 首页应弹确认
        app.current = 0;
        let result = app.handle_key(KeyEvent::new(KeyCode::Char('R'), KeyModifiers::NONE));
        assert!(result.is_none());
        assert!(app.restart_confirm.is_some(), "首页按 R 应弹确认");
        // 小写 r 不应触发重启确认（继续走 dashboard FetchExitIp）
        let (mut app2, _rx2) = test_app(24);
        app2.current = 0;
        let _ = app2.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE));
        assert!(app2.restart_confirm.is_none(), "小写 r 不应弹重启确认");
    }

    #[test]
    fn restart_confirm_and_cancel() {
        // y -> RestartCore
        let (mut app, mut rx) = test_app(24);
        app.current = 0;
        app.handle_key(KeyEvent::new(KeyCode::Char('R'), KeyModifiers::NONE));
        assert!(app.restart_confirm.is_some());
        app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
        assert!(app.restart_confirm.is_none(), "确认后弹窗应关闭");
        assert!(app.restarting, "确认后 restarting 应为 true");
        assert!(
            app.state
                .notices
                .iter()
                .any(|(_, t)| t.contains("正在重启")),
            "应有正在重启通知: {:?}",
            app.state.notices
        );
        assert!(
            matches!(rx.try_recv(), Ok(UiCommand::RestartCore)),
            "应发送 RestartCore"
        );
        // n -> cancel + notice, 不发 RestartCore
        let (mut app2, mut rx2) = test_app(24);
        app2.current = 0;
        app2.handle_key(KeyEvent::new(KeyCode::Char('R'), KeyModifiers::NONE));
        app2.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        assert!(app2.restart_confirm.is_none());
        assert!(!app2.restarting);
        assert!(
            app2.state
                .notices
                .iter()
                .any(|(_, t)| t.contains("已取消重启")),
            "取消应有通知: {:?}",
            app2.state.notices
        );
        assert!(rx2.try_recv().is_err(), "取消不应发送 RestartCore");
    }

    #[test]
    fn restart_restarting_blocks_reentry() {
        let (mut app, mut rx) = test_app(24);
        app.current = 0;
        app.restarting = true;
        let result = app.handle_key(KeyEvent::new(KeyCode::Char('R'), KeyModifiers::NONE));
        assert!(result.is_none());
        assert!(app.restart_confirm.is_none(), "restarting 时 R 应被忽略");
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn restart_done_success_notices_and_reloads() {
        let (mut app, mut rx) = test_app(24);
        app.restarting = true;
        app.on_ui_event(UiEvent::RestartDone(Ok(())));
        assert!(!app.restarting, "RestartDone 后 restarting 应为 false");
        assert!(
            app.state
                .notices
                .iter()
                .any(|(_, t)| t.contains("核心已重启")),
            "成功应通知核心已重启: {:?}",
            app.state.notices
        );
        assert!(
            matches!(rx.try_recv(), Ok(UiCommand::ReloadConfigs)),
            "成功后应发送 ReloadConfigs"
        );
    }

    #[test]
    fn restart_done_failure_popup_and_reloads() {
        let (mut app, mut rx) = test_app(24);
        app.restarting = true;
        app.on_ui_event(UiEvent::RestartDone(Err("500".into())));
        assert!(!app.restarting);
        assert_eq!(app.result_popup.as_ref().unwrap().title(), "重启失败");
        assert!(
            matches!(rx.try_recv(), Ok(UiCommand::ReloadConfigs)),
            "失败后应发送 ReloadConfigs"
        );
    }

    // ---- 鼠标点击顶部 Tab ----

    #[test]
    fn compute_tab_hits_basic() {
        let tabs_area = Rect::new(1, 1, 50, 1);
        let hits = compute_tab_hits(&TABS, tabs_area);
        // 6 个 tab 全部容纳（总宽 43 <= 50）
        assert_eq!(hits.len(), 6);
        // 仪表盘(6) @ x=1
        assert_eq!(hits[0], Rect::new(1, 1, 6, 1));
        // 订阅(4) @ x=1+6+3=10
        assert_eq!(hits[1], Rect::new(10, 1, 4, 1));
        // 规则组(6) @ x=17
        assert_eq!(hits[2], Rect::new(17, 1, 6, 1));
        // 规则(4) @ x=26
        assert_eq!(hits[3], Rect::new(26, 1, 4, 1));
        // 日志(4) @ x=33
        assert_eq!(hits[4], Rect::new(33, 1, 4, 1));
        // 设置(4) @ x=40
        assert_eq!(hits[5], Rect::new(40, 1, 4, 1));
    }

    #[test]
    fn compute_tab_hits_narrow_truncates() {
        let tabs_area = Rect::new(1, 1, 30, 1);
        let hits = compute_tab_hits(&TABS, tabs_area);
        // 30 宽仅容纳 4 个 tab（6+3+4+3+6+3+4=29，下一 divider 已越界）
        assert_eq!(hits.len(), 4);
        assert_eq!(hits[0], Rect::new(1, 1, 6, 1));
        assert_eq!(hits[1], Rect::new(10, 1, 4, 1));
        assert_eq!(hits[2], Rect::new(17, 1, 6, 1));
        assert_eq!(hits[3], Rect::new(26, 1, 4, 1));
    }

    #[test]
    fn compute_tab_hits_empty_area() {
        assert!(compute_tab_hits(&TABS, Rect::new(0, 0, 0, 1)).is_empty());
        assert!(compute_tab_hits(&TABS, Rect::new(0, 0, 0, 0)).is_empty());
    }

    #[test]
    fn hit_test_text_and_divider() {
        let tabs_area = Rect::new(1, 1, 50, 1);
        let hits = compute_tab_hits(&TABS, tabs_area);
        // 文本区域命中
        assert_eq!(hit_test(tabs_area, &hits, 1, 1), Some(0)); // 仪表盘首列
        assert_eq!(hit_test(tabs_area, &hits, 6, 1), Some(0)); // 仪表盘末列
        assert_eq!(hit_test(tabs_area, &hits, 10, 1), Some(1)); // 订阅
                                                                // divider 区域不命中
        assert_eq!(hit_test(tabs_area, &hits, 7, 1), None); // 第一 divider 首列
        assert_eq!(hit_test(tabs_area, &hits, 9, 1), None); // 第一 divider 末列
                                                            // 空白区不命中
        assert_eq!(hit_test(tabs_area, &hits, 44, 1), None); // 放到 50 宽末尾空白
        assert_eq!(hit_test(tabs_area, &hits, 0, 1), None); // 区域左侧外
        assert_eq!(hit_test(tabs_area, &hits, 1, 0), None); // 行不匹配
        assert_eq!(hit_test(tabs_area, &hits, 1, 2), None);
    }

    #[test]
    fn handle_mouse_switches_page() {
        let (mut app, mut rx) = test_app(24);
        // 通过 draw 初始化 tab_hits/tabs_area（宽 30 终端：4 个 hit）
        let _ = app.draw().unwrap();
        assert!(!app.tab_hits.is_empty(), "draw 应填充 tab_hits");
        let first_hit_x = app.tab_hits[0].x;
        let tabs_y = app.tabs_area.y;
        // 当前 0，切到 1（订阅）：点击第二 tab（订阅页不触发 RefreshGroups）
        app.current = 0;
        let second_x = app.tab_hits[1].x;
        let switched = app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: second_x,
            row: tabs_y,
            modifiers: KeyModifiers::NONE,
        });
        assert!(switched, "点击不同 tab 应切换");
        assert_eq!(app.current, 1);
        assert!(rx.try_recv().is_err(), "切到订阅页不应发 RefreshGroups");
        // 点击已选中页：不切换
        let switched2 = app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: second_x,
            row: tabs_y,
            modifiers: KeyModifiers::NONE,
        });
        assert!(!switched2, "点击已选中 tab 不应重复切换");
        assert_eq!(app.current, 1);
        // 切到 2（规则组）应发 RefreshGroups
        let third_x = app.tab_hits[2].x;
        let switched_to_groups = app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: third_x,
            row: tabs_y,
            modifiers: KeyModifiers::NONE,
        });
        assert!(switched_to_groups);
        assert_eq!(app.current, 2);
        assert!(matches!(rx.try_recv(), Ok(UiCommand::RefreshGroups)));
        // divider 点击不切换（当前已在 2，点 divider 不应切）
        let divider_x = first_hit_x + app.tab_hits[0].width; // 第一 divider 首列
        let switched3 = app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: divider_x,
            row: tabs_y,
            modifiers: KeyModifiers::NONE,
        });
        assert!(!switched3);
        assert_eq!(app.current, 2);
        // 非 Left / 非 Down 不切换
        assert!(!app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: second_x,
            row: tabs_y,
            modifiers: KeyModifiers::NONE,
        }));
        assert!(!app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Right),
            column: second_x,
            row: tabs_y,
            modifiers: KeyModifiers::NONE,
        }));
        // 行不匹配不切换
        assert!(!app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: second_x,
            row: tabs_y + 1,
            modifiers: KeyModifiers::NONE,
        }));
    }

    #[test]
    fn handle_mouse_blocked_when_popup_open() {
        let (mut app, _rx) = test_app(24);
        let _ = app.draw().unwrap();
        let tabs_y = app.tabs_area.y;
        let target_x = app.tab_hits[1].x;
        app.current = 0;
        // help popup 打开时屏蔽
        app.help_popup = Some(MessagePopup::new("帮助".into(), vec!["x".into()]));
        assert!(!app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: target_x,
            row: tabs_y,
            modifiers: KeyModifiers::NONE,
        }));
        assert_eq!(app.current, 0);
        app.help_popup = None;
        // result popup 打开时屏蔽
        app.result_popup = Some(MessagePopup::new("ok".into(), vec!["x".into()]));
        assert!(!app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: target_x,
            row: tabs_y,
            modifiers: KeyModifiers::NONE,
        }));
        assert_eq!(app.current, 0);
    }

    #[test]
    fn handle_mouse_blocked_in_edit_mode() {
        let (mut app, _rx) = test_app(24);
        app.switch_page(5);
        let _ = app.draw().unwrap();
        // 进入编辑模式：Down×9 到 port，Enter 编辑
        for _ in 0..9 {
            app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        }
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.pages[5].consumes_global_keys(), "应进入编辑模式");
        let tabs_y = app.tabs_area.y;
        let target_x = app.tab_hits[0].x;
        assert!(!app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: target_x,
            row: tabs_y,
            modifiers: KeyModifiers::NONE,
        }));
        assert_eq!(app.current, 5);
    }

    #[test]
    fn draw_updates_hits_on_resize() {
        let (mut app, _rx) = test_app_with_width(50, 24);
        let _ = app.draw().unwrap();
        assert_eq!(app.tab_hits.len(), 6, "宽 50 应容纳全部");
        // 窄终端重建：hits 随 draw 重算
        let (mut app2, _rx2) = test_app_with_width(20, 24);
        let _ = app2.draw().unwrap();
        assert!(app2.tab_hits.len() < 6, "窄终端应截断");
        // y 应恒为 1（top.y+1）
        assert_eq!(app.tabs_area.y, 1);
        assert_eq!(app2.tabs_area.y, 1);
    }

    #[test]
    fn draw_tiny_terminal_mouse_no_panic() {
        for h in [0u16, 1, 2, 3] {
            let (mut app, _rx) = test_app(h);
            let _ = app.draw().unwrap();
            // 空区域点击不 panic 且不切页
            let switched = app.handle_mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 0,
                row: 0,
                modifiers: KeyModifiers::NONE,
            });
            assert!(!switched, "h={h} 空区域点击不应切换");
        }
    }
}
