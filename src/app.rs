//! 应用主循环：AppState、UiCommand/UiEvent、后台任务（traffic/memory/exit_ip）、
//! tokio::select! 事件分发。契约见 docs/superpowers/plans/2026-08-10-mihomo-tui.md §3。

use std::collections::VecDeque;
use std::io;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures_util::StreamExt;
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Tabs};
use ratatui::{CompletedFrame, Terminal};
use tokio::sync::mpsc;

use crate::core::apply::{apply_config, validate_config, ApplyOutcome};
use crate::core::client::{Client, ConnInfo, ConnSnapshot, MemoryFrame, RuntimeConfig, TrafficFrame};
use crate::core::exit_ip::{self, ProxyPorts};
use crate::core::models::{NetworkSettings, Overrides, Subscription, SubscriptionCache};
use crate::core::settings::{load_overrides, load_settings, load_subscriptions, save_subscriptions};
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
    pub exit_ip: Option<String>,
    pub notices: VecDeque<String>,
}

impl AppState {
    fn load() -> Self {
        let mut notices = VecDeque::new();
        let settings = match load_settings() {
            Ok(s) => s,
            Err(e) => {
                notices.push_back(format!("[✗] 加载设置失败: {e}"));
                NetworkSettings::default()
            }
        };
        let subs = match load_subscriptions() {
            Ok(s) => s,
            Err(e) => {
                notices.push_back(format!("[✗] 加载订阅失败: {e}"));
                Vec::new()
            }
        };
        let overrides = match load_overrides() {
            Ok(o) => o,
            Err(e) => {
                notices.push_back(format!("[✗] 加载规则覆盖失败: {e}"));
                Overrides::default()
            }
        };
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
            notices,
        }
    }

    /// 追加通知，保留最近 5 条。
    pub fn notice(&mut self, msg: String) {
        self.notices.push_back(msg);
        while self.notices.len() > 5 {
            self.notices.pop_front();
        }
    }
}

/// 页面 → 主循环的异步操作请求。
pub enum UiCommand {
    PatchConfigs(serde_json::Value),
    ApplyConfig(String),
    FetchSubscription(usize),
    FetchExitIp,
    ReloadConfigs,
    InstallSetup,
}

/// 后台任务 → 主循环的事件。
pub enum UiEvent {
    PatchDone(Result<(), String>),
    ApplyDone(Result<ApplyOutcome, String>),
    SubscriptionFetched(usize, Result<SubscriptionCache, String>),
    ExitIp(Result<String, String>),
    ConfigsRefreshed(Result<RuntimeConfig, String>),
}

/// traffic 后台任务发往主循环的消息。
enum BgMsg {
    Traffic(TrafficFrame),
    Api(bool),
}

/// 需要交互式终端（离开 raw 模式/AltScreen）执行的任务。
enum InteractiveTask {
    Apply(String),
    Install,
}

/// 按键处理结果。
enum KeyAction {
    Quit,
    Interactive(InteractiveTask),
}

const TABS: [&str; 4] = ["仪表盘", "订阅", "规则组", "规则"];

const TRAFFIC_HISTORY: usize = 120;

/// 连接列表保留上限（快照替换天然有界，此处防御性截断）。
const CONNECTIONS_KEEP: usize = 200;
/// /connections 轮询间隔。
const CONNECTIONS_POLL: Duration = Duration::from_secs(3);

/// API 状态通知去抖窗口：同向状态变化在此窗口内不重复入列通知
/// （traffic 流断连与 5s 轮询成功竞态会造成高频翻转刷屏）。
const API_NOTICE_DEBOUNCE: Duration = Duration::from_secs(3);

const HELP_LINES: &[&str] = &[
    "全局按键:",
    "  q / Ctrl-C / Esc   退出",
    "  Tab / ← → / 1-4    切换页面",
    "  ?                  显示本帮助",
    "",
    "仪表盘:",
    "  m                  切换模式 (rule → global → direct)",
    "  t                  开关 TUN（热切换）",
    "  6                  开关 IPv6",
    "  r                  刷新出口 IP",
    "  s                  网络设置（保存后自动合并并应用）",
    "  i                  安装提权组件（首次启动拒绝后的重试入口）",
    "",
    "订阅管理:",
    "  a                  添加订阅",
    "  Enter              激活订阅",
    "  r                  刷新订阅",
    "  d                  删除订阅",
    "",
    "规则组:",
    "  n                  新建组",
    "  Enter              编辑组",
    "  m                  编辑组成员",
    "  d                  删除组",
    "",
    "规则:",
    "  n                  新建规则",
    "  Enter              编辑规则",
    "  K / J              上移 / 下移",
    "  d                  删除规则",
];

struct App<B: Backend> {
    state: AppState,
    pages: Vec<Box<dyn Page>>,
    current: usize,
    client: Arc<Client>,
    ui_tx: mpsc::UnboundedSender<UiEvent>,
    cmd_tx: mpsc::UnboundedSender<UiCommand>,
    cmd_rx: mpsc::UnboundedReceiver<UiCommand>,
    sudo_tx: mpsc::UnboundedSender<String>,
    exit_trigger: mpsc::UnboundedSender<()>,
    /// 代理端口快照（ApplyDone 成功后更新并触发重测）
    exit_ports: Arc<Mutex<ProxyPorts>>,
    /// 需要用户确认后执行的交互任务（sudo 密码/首次安装）
    pending_confirm: Option<(ConfirmPopup, InteractiveTask)>,
    help_popup: Option<MessagePopup>,
    result_popup: Option<MessagePopup>,
    /// 出口 IP 探测最近一次是否失败：恢复成功时用于关闭陈旧错误弹窗并通知恢复。
    exit_ip_was_error: bool,
    /// 上次 API 状态通知时间（去抖用）
    api_notice_at: Option<Instant>,
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
        let notices: Vec<(String, bool)> = self
            .state
            .notices
            .iter()
            .rev()
            .take(3)
            .map(|n| (n.clone(), n.starts_with("[✓]")))
            .collect();
        let hints = page_hints(current);

        let frame = self.terminal.draw(|f| {
            let area = f.area();
            let [top, middle, bottom] = Layout::vertical([
                Constraint::Length(3),
                Constraint::Min(1),
                Constraint::Length(4),
            ])
            .areas(area);

            // 顶栏：边框 + Tabs
            let block = Block::new()
                .title(Span::styled(
                    " mihomo-tui ",
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                ))
                .borders(Borders::ALL);
            f.render_widget(block, top);
            // 超小终端下 tabs_area 可能与 buffer 无交集（如 h=1 时 top.y+1 已越界）；
            // Tabs 不做 intersection 裁剪，必须提前 clamp 成空区域让它直接返回
            let tabs_area =
                Rect::new(top.x + 1, top.y + 1, top.width.saturating_sub(2), 1).intersection(area);
            f.render_widget(
                Tabs::new(tabs.iter().map(|t| Line::raw(t.clone())))
                    .select(current)
                    .highlight_style(
                        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                    )
                    .divider(" │ "),
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
            for (i, (text, ok)) in notices.iter().take(notice_rows as usize).enumerate() {
                let style = if *ok {
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
                KeyHints { hints: hints.clone() }
                    .render(f, Rect::new(bottom.x, hint_y, bottom.width, 1));
            }

            // 全局弹窗置顶
            if let Some(popup) = &mut self.help_popup {
                popup.render(f, area);
            }
            if let Some((popup, _)) = &mut self.pending_confirm {
                popup.render(f, area);
            }
            if let Some(popup) = &mut self.result_popup {
                popup.render(f, area);
            }
        })?;
        Ok(frame)
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

        // 页面内部弹窗打开时，按键全部交给页面（全局键不生效）
        if self.pages[self.current].popup_open() {
            let page = &mut self.pages[self.current];
            if let Some(cmd) = page.handle_key(key, &mut self.state) {
                let _ = self.cmd_tx.send(cmd);
            }
            return None;
        }

        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return Some(KeyAction::Quit),
            KeyCode::Tab => {
                self.current = (self.current + 1) % self.pages.len();
            }
            KeyCode::BackTab => {
                self.current = (self.current + self.pages.len() - 1) % self.pages.len();
            }
            KeyCode::Right => {
                self.current = (self.current + 1) % self.pages.len();
            }
            KeyCode::Left => {
                self.current = (self.current + self.pages.len() - 1) % self.pages.len();
            }
            KeyCode::Char(c) if ('1'..='4').contains(&c) => {
                self.current = c.to_digit(10).unwrap_or(1) as usize - 1;
            }
            KeyCode::Char('?') => {
                self.help_popup = Some(MessagePopup::new(
                    "帮助".into(),
                    HELP_LINES.iter().map(|s| s.to_string()).collect(),
                ));
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
        mut ui_rx: mpsc::UnboundedReceiver<UiEvent>,
        mut sudo_rx: mpsc::UnboundedReceiver<String>,
    ) -> Result<(), BoxError> {
        let mut events = EventStream::new();
        let mut ticker = tokio::time::interval(Duration::from_secs(1));
        self.draw()?;

        while !self.quit {
            enum Act {
                Key(KeyEvent),
                Tick,
                Bg(BgMsg),
                Mem(MemoryFrame),
                Conns(ConnSnapshot),
                Ui(UiEvent),
                Cmd(UiCommand),
                Sudo(String),
            }
            let act = tokio::select! {
                ev = events.next() => match ev {
                    Some(Ok(Event::Key(key))) => Act::Key(key),
                    Some(Ok(Event::Resize(_, _))) => Act::Tick,
                    Some(Ok(_)) => continue,
                    Some(Err(_)) => continue,
                    None => break,
                },
                _ = ticker.tick() => Act::Tick,
                msg = traffic_rx.recv() => match msg { Some(m) => Act::Bg(m), None => continue },
                msg = memory_rx.recv() => match msg { Some(m) => Act::Mem(m), None => continue },
                msg = conns_rx.recv() => match msg { Some(m) => Act::Conns(m), None => continue },
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
                Act::Tick => {
                    self.tick_count += 1;
                    if self.tick_count % 5 == 0 || !self.state.api_ok {
                        let _ = self.cmd_tx.send(UiCommand::ReloadConfigs);
                    }
                    self.draw()?;
                }
                Act::Bg(msg) => self.on_bg_msg(msg),
                Act::Mem(frame) => self.on_memory(frame),
                Act::Conns(snap) => self.on_conns(snap),
                Act::Ui(ev) => self.on_ui_event(ev),
                Act::Cmd(cmd) => self.spawn_command(cmd),
                Act::Sudo(yaml) => {
                    // 弹确认框前附诊断提示：区分"未重新登录（组未生效）"与
                    // "sudoers 规则未生效"两种根因，引导用户对症处理。
                    let has_group = crate::service::installer::session_has_admin_group();
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

    /// 连接快照 → 排序 → 截断上限 → 替换状态。
    fn on_conns(&mut self, snap: ConnSnapshot) {
        let mut conns = snap.connections;
        sort_connections(&mut conns);
        conns.truncate(CONNECTIONS_KEEP);
        self.state.connections = conns;
    }

    fn on_ui_event(&mut self, ev: UiEvent) {
        match ev {
            UiEvent::PatchDone(res) => match res {
                Ok(()) => self.state.notice("[✓] 已应用运行时配置".to_string()),
                Err(e) => self.popup_error("操作失败", e),
            },
            UiEvent::ApplyDone(res) => match res {
                Ok(outcome) => {
                    // 网络配置已变：刷新代理端口快照并立即重测一次出口 IP
                    *self.exit_ports.lock().unwrap() = ProxyPorts::from_settings(&self.state.settings);
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
                        None => self.state.notice(
                            "[!] 订阅拉取完成，但该订阅已被删除（缓存已丢弃）".to_string(),
                        ),
                    }
                }
                Err(e) => self.popup_error("订阅拉取失败", e),
            },
            UiEvent::ExitIp(res) => match res {
                Ok(ip) => {
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
                        self.state.notice(format!("[✓] 出口 IP 恢复: {ip}"));
                    }
                    self.state.exit_ip = Some(ip);
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
        }
    }

    fn popup_error(&mut self, title: &str, msg: String) {
        self.result_popup = Some(MessagePopup::new(title.into(), vec![msg.clone()]));
        self.state.notice(format!("[✗] {}", msg.lines().next().unwrap_or("")));
    }

    /// 分发 UiCommand：spawn 异步任务。
    fn spawn_command(&mut self, cmd: UiCommand) {
        let ui_tx = self.ui_tx.clone();
        let client = self.client.clone();        match cmd {
            UiCommand::PatchConfigs(patch) => {
                tokio::spawn(async move {
                    let res = tokio::time::timeout(
                        Duration::from_secs(5),
                        client.patch_configs(patch),
                    )
                    .await;
                    let res = match res {
                        Ok(Ok(())) => Ok(()),
                        Ok(Err(e)) => Err(e.to_string()),
                        Err(_) => Err("请求超时（5s）".to_string()),
                    };
                    let _ = ui_tx.send(UiEvent::PatchDone(res));
                });
            }
            UiCommand::ApplyConfig(yaml) => {
                let sudo_tx = self.sudo_tx.clone();
                tokio::spawn(async move {
                    // 先 mihomo -t 校验，再非交互 sudo
                    match validate_config(&yaml).await {
                        Err(e) => {
                            let _ = ui_tx.send(UiEvent::ApplyDone(Err(e.to_string())));
                        }
                        Ok(()) => match apply_config(&yaml, true).await {
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
            UiCommand::InstallSetup => {
                self.pending_confirm = Some((
                    ConfirmPopup::new(
                        "首次安装".into(),
                        "需要 root 权限执行安装（mihomo-apply 脚本与 sudoers）。是否继续？".into(),
                    ),
                    InteractiveTask::Install,
                ));
            }
        }
    }

    /// 交互任务：离开 raw 模式/AltScreen → 执行（sudo 交互等）→ 恢复 → 结果弹窗。
    async fn run_interactive(&mut self, task: InteractiveTask) -> Result<(), BoxError> {
        let _ = crossterm::terminal::disable_raw_mode();
        let _ = crossterm::execute!(io::stdout(), crossterm::terminal::LeaveAlternateScreen);

        let result = match task {
            InteractiveTask::Apply(yaml) => apply_config(&yaml, false)
                .await
                .map_err(|e| e.to_string()),
            InteractiveTask::Install => {
                crate::service::installer::install()
                    .await
                    .map(|lines| ApplyOutcome {
                        success: true,
                        stdout: lines.join("\n"),
                        stderr: String::new(),
                    })
                    .map_err(|e| e.to_string())
            }
        };

        crossterm::terminal::enable_raw_mode()?;
        crossterm::execute!(io::stdout(), crossterm::terminal::EnterAlternateScreen)?;
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
                self.state.notice(format!("[✗] {}", e.lines().next().unwrap_or("")));
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
        0 => vec![
            ("m".into(), "模式".into()),
            ("t".into(), "TUN".into()),
            ("6".into(), "IPv6".into()),
            ("r".into(), "出口IP".into()),
            ("s".into(), "设置".into()),
            ("i".into(), "安装".into()),
        ],
        1 => vec![
            ("a".into(), "添加".into()),
            ("Enter".into(), "激活".into()),
            ("r".into(), "刷新".into()),
            ("d".into(), "删除".into()),
        ],
        2 => vec![
            ("n".into(), "新建".into()),
            ("Enter".into(), "编辑".into()),
            ("m".into(), "成员".into()),
            ("d".into(), "删除".into()),
        ],
        _ => vec![
            ("n".into(), "新建".into()),
            ("Enter".into(), "编辑".into()),
            ("K/J".into(), "移动".into()),
            ("d".into(), "删除".into()),
        ],
    };
    hints.push(("Tab".into(), "切页".into()));
    hints.push(("?".into(), "帮助".into()));
    hints.push(("q".into(), "退出".into()));
    hints
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
        kb.cmp(&ka).then_with(|| (b.upload + b.download).cmp(&(a.upload + a.download)))
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
    let (sudo_tx, sudo_rx) = mpsc::unbounded_channel();
    let (exit_trigger, trigger_rx) = mpsc::unbounded_channel();
    let exit_ports = Arc::new(Mutex::new(ProxyPorts::from_settings(&state.settings)));

    spawn_traffic_task(client.clone(), traffic_tx);
    spawn_memory_task(client.clone(), memory_tx);
    spawn_connections_task(client.clone(), conns_tx);
    spawn_exit_ip_task(exit_ports.clone(), trigger_rx, ui_tx.clone());

    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    terminal.hide_cursor()?;

    let pages: Vec<Box<dyn Page>> = vec![
        Box::new(DashboardPage::new()),
        Box::new(SubscriptionsPage::new()),
        Box::new(GroupsPage::new()),
        Box::new(RulesPage::new()),
    ];

    let mut app = App {
        state,
        pages,
        current: 0,
        client,
        ui_tx,
        cmd_tx,
        cmd_rx,
        sudo_tx,
        exit_trigger,
        exit_ports,
        pending_confirm: None,
        help_popup: None,
        result_popup: None,
        exit_ip_was_error: false,
        api_notice_at: None,
        tick_count: 0,
        quit: false,
        terminal,
    };
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
    let result = app.run_loop(traffic_rx, memory_rx, conns_rx, ui_rx, sudo_rx).await;
    let _ = app.terminal.show_cursor();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    /// 底栏行计算纯函数：任意终端高度下提示行与通知行数都不越界。
    /// 回归：h=1/2 时 bottom.height=0，此前 hint_y=bottom.y 越出 buffer 顶。
    #[test]
    fn bottom_bar_rows_always_in_bounds() {
        for h in 0..=30u16 {
            let area = Rect::new(0, 0, 30, h);
            let [_, _, bottom] = Layout::vertical([
                Constraint::Length(3),
                Constraint::Min(1),
                Constraint::Length(4),
            ])
            .areas(area);
            let (notice_rows, hint_y) = bottom_bar_rows(bottom, h);
            assert!(notice_rows <= h, "h={h}: notice_rows {notice_rows} > 高度");
            match hint_y {
                Some(y) => {
                    assert!(y < h, "h={h}: hint_y {y} >= 高度");
                    assert!(y >= bottom.y, "h={h}: hint_y {y} < bottom.y {}", bottom.y);
                    assert_eq!(
                        y - bottom.y,
                        notice_rows,
                        "h={h}: 通知行数应等于 hint_y - bottom.y"
                    );
                }
                None => assert_eq!(notice_rows, 0, "h={h}: 无提示行时通知也应为 0"),
            }
        }
    }

    /// 小终端回归：h=0/1/2（及常规高度）整帧渲染不 panic 且按键提示行不越界。
    /// 此前 h=1/2 时 hint_y 越出 buffer，KeyHints 渲染触发 ratatui Buffer::index_of panic。
    #[test]
    fn draw_tiny_terminal_no_panic() {
        for h in [0u16, 1, 2, 3, 4, 5, 6, 7, 24] {
            let mut app = test_app(h);
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
    fn test_app(h: u16) -> App<TestBackend> {
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
            notices: VecDeque::new(),
        };
        let (ui_tx, _) = mpsc::unbounded_channel();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (sudo_tx, _) = mpsc::unbounded_channel();
        let (exit_trigger, _) = mpsc::unbounded_channel();
        let client = Arc::new(Client::new(&state.settings));
        let exit_ports = Arc::new(Mutex::new(ProxyPorts::from_settings(&state.settings)));
        App {
            state,
            pages: vec![
                Box::new(DashboardPage::new()),
                Box::new(SubscriptionsPage::new()),
                Box::new(GroupsPage::new()),
                Box::new(RulesPage::new()),
            ],
            current: 0,
            client,
            ui_tx,
            cmd_tx,
            cmd_rx,
            sudo_tx,
            exit_trigger,
            exit_ports,
            pending_confirm: None,
            help_popup: None,
            result_popup: None,
            exit_ip_was_error: false,
            api_notice_at: None,
            tick_count: 0,
            quit: false,
            terminal: Terminal::new(TestBackend::new(30, h)).unwrap(),
        }
    }

    /// 出口 IP 失败后恢复：关闭陈旧错误弹窗 + 通知恢复；再次成功（无失败历史）
    /// 不重复通知。
    #[test]
    fn exit_ip_recovery_closes_stale_popup_and_notices() {
        let mut app = test_app(24);
        // 失败：弹出「出口 IP 获取失败」弹窗
        app.on_ui_event(UiEvent::ExitIp(Err("出口 IP 获取失败: 连接被拒".into())));
        assert!(app.result_popup.is_some(), "失败应弹出错误弹窗");
        assert_eq!(
            app.result_popup.as_ref().unwrap().title(),
            "出口 IP 获取失败"
        );
        assert!(app.state.exit_ip.is_none());
        // 恢复：陈旧弹窗关闭 + 通知恢复
        app.on_ui_event(UiEvent::ExitIp(Ok("1.2.3.4".into())));
        assert!(app.result_popup.is_none(), "恢复成功后应关闭陈旧错误弹窗");
        assert_eq!(app.state.exit_ip.as_deref(), Some("1.2.3.4"));
        assert!(
            app.state.notices.iter().any(|n| n.contains("[✓] 出口 IP 恢复: 1.2.3.4")),
            "应通知恢复: {:?}",
            app.state.notices
        );
        // 再次成功：无失败历史，静默更新不重复通知
        app.on_ui_event(UiEvent::ExitIp(Ok("5.6.7.8".into())));
        assert_eq!(app.state.exit_ip.as_deref(), Some("5.6.7.8"));
        assert!(
            !app.state.notices.iter().any(|n| n.contains("5.6.7.8")),
            "无失败历史时不应再通知恢复: {:?}",
            app.state.notices
        );
    }

    /// 恢复成功只关闭「出口 IP 获取失败」弹窗：其他弹窗（如应用结果）不受影响。
    #[test]
    fn exit_ip_recovery_keeps_unrelated_popup() {
        let mut app = test_app(24);
        app.on_ui_event(UiEvent::ExitIp(Err("出口 IP 获取失败: 连接被拒".into())));
        assert!(app.exit_ip_was_error, "失败应置位 exit_ip_was_error");
        // 用户随后打开了另一个弹窗
        app.result_popup = Some(MessagePopup::new("应用结果".into(), vec!["x".into()]));
        app.on_ui_event(UiEvent::ExitIp(Ok("1.2.3.4".into())));
        assert!(
            app.result_popup.is_some(),
            "非出口 IP 弹窗不应被关闭"
        );
        assert_eq!(
            app.result_popup.as_ref().unwrap().title(),
            "应用结果",
            "应用结果弹窗应原样保留"
        );
        assert!(!app.exit_ip_was_error, "恢复后应清除失败标记");
        assert!(
            app.state.notices.iter().any(|n| n.contains("[✓] 出口 IP 恢复: 1.2.3.4")),
            "应通知恢复: {:?}",
            app.state.notices
        );
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

    /// on_conns：快照替换 + 上限截断 200。
    #[test]
    fn on_conns_truncates_and_sorts() {
        let mut app = test_app(24);
        let mut snap = ConnSnapshot::default();
        snap.connections = (0..250)
            .map(|i| conn(&format!("c{i}"), Some("2026-08-12T10:00:00Z"), i, 0))
            .collect();
        app.on_conns(snap);
        assert_eq!(app.state.connections.len(), CONNECTIONS_KEEP);
        // 排序后最新在上：所有连接 start 相同 → 流量降序 → c249 在最前
        assert_eq!(app.state.connections[0].id, "c249");
    }
}
