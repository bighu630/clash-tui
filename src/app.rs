//! 应用主循环：AppState、UiCommand/UiEvent、后台任务（traffic/memory/exit_ip）、
//! tokio::select! 事件分发。契约见 docs/superpowers/plans/2026-08-10-mihomo-tui.md §3。

use std::collections::VecDeque;
use std::io;
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures_util::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Tabs};
use ratatui::Terminal;
use tokio::sync::mpsc;

use crate::core::apply::{apply_config, validate_config, ApplyOutcome};
use crate::core::client::{Client, MemoryFrame, RuntimeConfig, TrafficFrame};
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
    pub traffic: VecDeque<TrafficFrame>,
    pub mem_history: VecDeque<u64>,
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
            traffic: VecDeque::new(),
            mem_history: VecDeque::new(),
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

struct App {
    state: AppState,
    pages: Vec<Box<dyn Page>>,
    current: usize,
    client: Arc<Client>,
    ui_tx: mpsc::UnboundedSender<UiEvent>,
    cmd_tx: mpsc::UnboundedSender<UiCommand>,
    cmd_rx: mpsc::UnboundedReceiver<UiCommand>,
    sudo_tx: mpsc::UnboundedSender<String>,
    exit_trigger: mpsc::UnboundedSender<()>,
    /// 需要用户确认后执行的交互任务（sudo 密码/首次安装）
    pending_confirm: Option<(ConfirmPopup, InteractiveTask)>,
    help_popup: Option<MessagePopup>,
    result_popup: Option<MessagePopup>,
    /// 上次 API 状态通知时间（去抖用）
    api_notice_at: Option<Instant>,
    tick_count: u64,
    quit: bool,
    terminal: Terminal<CrosstermBackend<io::Stdout>>,
}

impl App {
    fn draw(&mut self) -> Result<(), BoxError> {
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

        self.terminal.draw(|f| {
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
            let tabs_area = Rect::new(top.x + 1, top.y + 1, top.width.saturating_sub(2), 1);
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
            let hint_y = bottom.y.saturating_add(bottom.height.saturating_sub(1));
            let mut y = bottom.y;
            for (text, ok) in notices.iter() {
                if y >= hint_y {
                    break;
                }
                let style = if *ok {
                    Style::default().fg(Color::Green)
                } else if text.starts_with("[!]") {
                    Style::default().fg(Color::Yellow)
                } else {
                    Style::default().fg(Color::Red)
                };
                f.render_widget(
                    Paragraph::new(Span::styled(text.clone(), style)),
                    Rect::new(bottom.x, y, bottom.width, 1),
                );
                y += 1;
            }
            KeyHints { hints: hints.clone() }.render(f, Rect::new(bottom.x, hint_y, bottom.width, 1));

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
        Ok(())
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
                Act::Ui(ev) => self.on_ui_event(ev),
                Act::Cmd(cmd) => self.spawn_command(cmd),
                Act::Sudo(yaml) => {
                    self.pending_confirm = Some((
                        ConfirmPopup::new(
                            "需要 sudo 密码".into(),
                            "sudo 需要密码，将以交互模式重试应用配置。是否继续？".into(),
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

    fn on_ui_event(&mut self, ev: UiEvent) {
        match ev {
            UiEvent::PatchDone(res) => match res {
                Ok(()) => self.state.notice("[✓] 已应用运行时配置".to_string()),
                Err(e) => self.popup_error("操作失败", e),
            },
            UiEvent::ApplyDone(res) => match res {
                Ok(outcome) => {
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
                    self.state.exit_ip = Some(ip);
                }
                Err(e) => {
                    self.state.exit_ip = None;
                    self.state.notice(format!("[✗] 出口 IP 获取失败: {e}"));
                }
            },
            UiEvent::ConfigsRefreshed(res) => match res {
                Ok(runtime) => {
                    self.state.runtime = runtime;
                    self.set_api_ok(true, None);
                }
                Err(e) => self.set_api_ok(false, Some(&e)),
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
                let _ = self.exit_trigger.send(());
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

fn page_hints(current: usize) -> Vec<(String, String)> {
    let mut hints: Vec<(String, String)> = match current {
        0 => vec![
            ("m".into(), "模式".into()),
            ("t".into(), "TUN".into()),
            ("6".into(), "IPv6".into()),
            ("r".into(), "出口IP".into()),
            ("s".into(), "设置".into()),
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

/// 出口 IP 获取：经 mixed_port 代理，多 URL 降级。
async fn fetch_exit_ip(port: u16) -> Result<String, String> {
    let proxy_url = format!("http://127.0.0.1:{port}");
    let client = reqwest::Client::builder()
        .proxy(reqwest::Proxy::all(&proxy_url).map_err(|e| e.to_string())?)
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| e.to_string())?;
    let urls = [
        "https://api.ipify.org",
        "http://api.ipify.org",
        "https://ifconfig.me/ip",
    ];
    let mut last_err = String::new();
    for url in urls {
        match client.get(url).send().await {
            Ok(resp) if resp.status().is_success() => {
                if let Ok(text) = resp.text().await {
                    let ip = text.trim().to_string();
                    if !ip.is_empty() && !ip.chars().any(char::is_whitespace) {
                        return Ok(ip);
                    }
                }
                last_err = format!("{url} 返回非文本内容");
            }
            Ok(resp) => last_err = format!("{url} HTTP {}", resp.status()),
            Err(e) => last_err = format!("{url}: {e}"),
        }
    }
    Err(format!("全部尝试失败（{last_err}）"))
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

/// exit_ip 后台任务：每 60s 定时 + FetchExitIp 命令触发。
fn spawn_exit_ip_task(
    port: Arc<AtomicU16>,
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
            let result = fetch_exit_ip(port.load(Ordering::Relaxed)).await;
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
    let (sudo_tx, sudo_rx) = mpsc::unbounded_channel();
    let (exit_trigger, trigger_rx) = mpsc::unbounded_channel();
    let exit_port = Arc::new(AtomicU16::new(state.settings.mixed_port));

    spawn_traffic_task(client.clone(), traffic_tx);
    spawn_memory_task(client.clone(), memory_tx);
    spawn_exit_ip_task(exit_port.clone(), trigger_rx, ui_tx.clone());

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
        pending_confirm: None,
        help_popup: None,
        result_popup: None,
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
    let result = app.run_loop(traffic_rx, memory_rx, ui_rx, sudo_rx).await;
    let _ = app.terminal.show_cursor();
    result
}
