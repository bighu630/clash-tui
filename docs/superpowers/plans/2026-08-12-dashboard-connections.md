# 仪表盘连接框 + 网速/累计合并 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 仪表盘布局重构——取消「总流量」框，累计流量并入「网络」框（速率左对齐/累计右对齐）；左列整列新增「连接」框（GET /connections 轮询 3s），body 宽 < 60 列时隐藏左列。

**Architecture:** 数据层（core/client.rs 新增 ConnSnapshot/ConnInfo 模型 + get_connections）→ 状态层（app.rs 新增 connections 状态、3s 轮询任务、事件循环分支、排序纯函数）→ 展示层（ui/dashboard.rs 重构布局：连接框 + 网络框 + 内存框，响应式隐藏）。累计流量直接复用 /traffic 流自带 upTotal/downTotal，无新增请求；无新依赖。

**Tech Stack:** Rust 2021, ratatui 0.30, tokio, reqwest, chrono 0.4, serde_json。

**Worktree:** `/data/code/clash-tui/.worktrees/dashboard-connections`（分支 feature/dashboard-connections，基线 174 测试全绿）。所有命令在此目录执行。

**Spec:** `docs/superpowers/specs/2026-08-12-dashboard-connections-design.md`

---

### Task 1: core/client.rs —— 连接模型与 GET /connections

**Files:**
- Modify: `src/core/client.rs`（在 `MemoryFrame` 定义后追加模型；在 `memory_stream` 方法后追加 `get_connections`；在测试模块追加测试）
- Test: `src/core/client.rs` 内 `mod tests`

- [x] **Step 1: 追加连接数据模型**（插在 `MemoryFrame` 定义之后、`ApiError` 之前）

```rust
/// 连接快照（GET /connections，camelCase 键）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ConnSnapshot {
    pub download_total: u64,
    pub upload_total: u64,
    pub connections: Vec<ConnInfo>,
    pub memory: u64,
}

/// 单条连接。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ConnInfo {
    pub id: String,
    pub meta: ConnMeta,
    pub upload: u64,
    pub download: u64,
    /// 连接建立时间（RFC3339）；缺失/解析失败为 None。
    pub start: Option<chrono::DateTime<chrono::Utc>>,
    pub chains: Vec<String>,
    pub rule: String,
    pub rule_payload: String,
    pub dl_speed: u64,
    pub ul_speed: u64,
    pub is_alive: bool,
}

/// 连接元数据（metadata 子对象，全部字符串可缺失）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ConnMeta {
    pub network: String,
    pub host: String,
    pub sniff_host: String,
    pub remote_destination: String,
    pub destination_ip: String,
    pub destination_port: String,
    pub source_ip: String,
    pub source_port: String,
    pub r#type: String,
    pub process_path: String,
}
```

- [x] **Step 2: 追加 `FromStr for ConnSnapshot`**（插在 `impl FromStr for MemoryFrame` 之后）

遵循现有 Value 解析模式（容忍缺失字段）：

```rust
impl FromStr for ConnSnapshot {
    type Err = ApiError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let v: serde_json::Value =
            serde_json::from_str(s).map_err(|e| ApiError::Json(e.to_string()))?;
        let conns = v
            .get("connections")
            .and_then(|x| x.as_array())
            .map(|arr| arr.iter().filter_map(parse_conn).collect())
            .unwrap_or_default();
        Ok(Self {
            download_total: v.get("downloadTotal").and_then(|x| x.as_u64()).unwrap_or(0),
            upload_total: v.get("uploadTotal").and_then(|x| x.as_u64()).unwrap_or(0),
            connections: conns,
            memory: v.get("memory").and_then(|x| x.as_u64()).unwrap_or(0),
        })
    }
}

/// 单条连接解析：非对象元素或 start 非法时该字段置默认/None，不整条丢弃。
fn parse_conn(c: &serde_json::Value) -> Option<ConnInfo> {
    let obj = c.as_object()?;
    let get = |key: &str| obj.get(key).and_then(|x| x.as_str()).unwrap_or_default().to_string();
    let start = obj
        .get("start")
        .and_then(|x| x.as_str())
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&chrono::Utc));
    let meta = c
        .get("metadata")
        .and_then(|m| m.as_object())
        .map(|m| {
            let mget = |key: &str| {
                m.get(key)
                    .and_then(|x| x.as_str())
                    .unwrap_or_default()
                    .to_string()
            };
            ConnMeta {
                network: mget("network"),
                host: mget("host"),
                sniff_host: mget("sniffHost"),
                remote_destination: mget("remoteDestination"),
                destination_ip: mget("destinationIP"),
                destination_port: mget("destinationPort"),
                source_ip: mget("sourceIP"),
                source_port: mget("sourcePort"),
                r#type: mget("type"),
                process_path: mget("processPath"),
            }
        })
        .unwrap_or_default();
    Some(ConnInfo {
        id: get("id"),
        meta,
        upload: obj.get("upload").and_then(|x| x.as_u64()).unwrap_or(0),
        download: obj.get("download").and_then(|x| x.as_u64()).unwrap_or(0),
        start,
        chains: obj
            .get("chains")
            .and_then(|x| x.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|s| s.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
        rule: get("rule"),
        rule_payload: get("rulePayload"),
        dl_speed: obj.get("dlSpeed").and_then(|x| x.as_u64()).unwrap_or(0),
        ul_speed: obj.get("ulSpeed").and_then(|x| x.as_u64()).unwrap_or(0),
        is_alive: obj.get("isAlive").and_then(|x| x.as_bool()).unwrap_or(true),
    })
}
```

- [x] **Step 3: 追加 `Client::get_connections`**（插在 `memory_stream` 方法之后）

```rust
    /// GET /connections → 连接快照（全量，一次返回）。
    pub async fn get_connections(&self) -> Result<ConnSnapshot, ApiError> {
        let body = self.request_text(reqwest::Method::GET, "/connections").await?;
        body.parse()
    }
```

- [x] **Step 4: 单元测试**（追加到现有 `mod tests` 内，`use super::*;` 已含新类型）

```rust
    // ---------- /connections 解析 ----------

    #[test]
    fn conn_snapshot_full_json() {
        let snap = ConnSnapshot::from_str(
            r#"{"downloadTotal":111,"uploadTotal":222,"memory":333,"connections":[
                {"id":"c1","metadata":{"network":"tcp","type":"HTTP","sourceIP":"127.0.0.1",
                 "destinationIP":"1.2.3.4","sourcePort":"55555","destinationPort":"443",
                 "host":"example.com","dnsMode":"fake-ip","processPath":"/usr/bin/curl",
                 "remoteDestination":"1.2.3.4:443","sniffHost":""},
                 "upload":100,"download":200,
                 "start":"2026-08-12T10:00:00.000Z",
                 "chains":["DIRECT"],"rule":"DIRECT","rulePayload":"","dlSpeed":5,"ulSpeed":3,"isAlive":true}
            ]}"#,
        )
        .unwrap();
        assert_eq!(snap.download_total, 111);
        assert_eq!(snap.upload_total, 222);
        assert_eq!(snap.memory, 333);
        assert_eq!(snap.connections.len(), 1);
        let c = &snap.connections[0];
        assert_eq!(c.id, "c1");
        assert_eq!(c.meta.host, "example.com");
        assert_eq!(c.meta.network, "tcp");
        assert_eq!(c.meta.destination_ip, "1.2.3.4");
        assert_eq!(c.meta.process_path, "/usr/bin/curl");
        assert_eq!(c.upload, 100);
        assert_eq!(c.download, 200);
        assert!(c.start.is_some());
        assert_eq!(c.chains, vec!["DIRECT".to_string()]);
        assert_eq!(c.rule, "DIRECT");
        assert_eq!(c.dl_speed, 5);
        assert_eq!(c.ul_speed, 3);
        assert!(c.is_alive);
    }

    #[test]
    fn conn_snapshot_missing_and_empty() {
        // 顶层缺字段 + connections 缺失 + 空数组
        let snap = ConnSnapshot::from_str(r#"{}"#).unwrap();
        assert_eq!(snap.download_total, 0);
        assert!(snap.connections.is_empty());
        let snap = ConnSnapshot::from_str(r#"{"connections":[]}"#).unwrap();
        assert!(snap.connections.is_empty());
        let snap = ConnSnapshot::from_str(r#"{"connections":null}"#).unwrap();
        assert!(snap.connections.is_empty());
    }

    #[test]
    fn conn_start_parsing_variants() {
        // 合法 RFC3339
        let snap = ConnSnapshot::from_str(
            r#"{"connections":[{"id":"a","start":"2026-08-12T10:00:00.000Z"}]}"#,
        )
        .unwrap();
        assert!(snap.connections[0].start.is_some());
        // RFC3339Nano（带纳秒）
        let snap = ConnSnapshot::from_str(
            r#"{"connections":[{"id":"b","start":"2026-08-12T10:00:00.123456789Z"}]}"#,
        )
        .unwrap();
        assert!(snap.connections[0].start.is_some());
        // 缺失 / 非法 → None，不整条丢弃
        let snap = ConnSnapshot::from_str(
            r#"{"connections":[{"id":"c"},{"id":"d","start":"not-a-date"}]}"#,
        )
        .unwrap();
        assert!(snap.connections[0].start.is_none());
        assert!(snap.connections[1].start.is_none());
        assert_eq!(snap.connections.len(), 2);
    }

    #[tokio::test]
    async fn get_connections_ok() {
        let (port, _rx) = spawn_api_server().await;
        let snap = client_on(port).get_connections().await.unwrap();
        assert_eq!(snap.connections.len(), 1);
        assert_eq!(snap.connections[0].meta.host, "conn.example.com");
        assert_eq!(snap.connections[0].upload, 77);
    }
```

- [x] **Step 5: 假服务器增加 /connections 路由**（在 `spawn_api_server` 内 `"/memory" =>` 分支之后追加，写成与 `/traffic` 分支相同的"直接 write 后 return"风格，不并入底部 `let body` match）

```rust
                        "/connections" => {
                            let payload = "{\"downloadTotal\":9,\"uploadTotal\":8,\"memory\":7,\"connections\":[{\"id\":\"conn1\",\"metadata\":{\"network\":\"tcp\",\"host\":\"conn.example.com\",\"destinationIP\":\"9.9.9.9\",\"destinationPort\":\"443\"},\"upload\":77,\"download\":88,\"start\":\"2026-08-12T10:00:00.000Z\",\"chains\":[\"DIRECT\"],\"rule\":\"DIRECT\",\"rulePayload\":\"\"}]}";
                            let _ = sock
                                .write_all(
                                    format!(
                                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                                        payload.len()
                                    )
                                    .as_bytes(),
                                )
                                .await;
                            return;
                        }
```

- [x] **Step 6: 运行测试并提交**

Run: `cargo test client 2>&1 | tail -5`
Expected: `test result: ok.` 全部通过（原 174 + 新增 4 = 178：conn_snapshot_full_json / conn_snapshot_missing_and_empty / conn_start_parsing_variants / get_connections_ok）

```bash
git add src/core/client.rs
git commit -m "feat: client 新增 /connections 快照模型与 get_connections"
```

---

### Task 2: app.rs —— 连接状态、3s 轮询任务、事件循环

**Files:**
- Modify: `src/app.rs`
- Test: `src/app.rs` 内 `mod tests`

前置依赖：Task 1 已完成（`ConnSnapshot`/`ConnInfo` 已在 client.rs 导出，`use crate::core::client::{Client, ConnInfo, MemoryFrame, RuntimeConfig, TrafficFrame};` 需追加 `ConnInfo`——注意 Task 2 只用到 `ConnInfo`，`ConnSnapshot` 仅存在于 channel 类型中，需同样 import）。

- [x] **Step 1: 更新 import**

```rust
use crate::core::client::{Client, ConnInfo, ConnSnapshot, MemoryFrame, RuntimeConfig, TrafficFrame};
```

- [x] **Step 2: AppState 增加 connections 字段**（`mem_history` 字段后追加；`load()` 与测试构造器同步）

```rust
    pub connections: Vec<ConnInfo>,
```

`load()` 中：`mem_history: VecDeque::new(),` 后加 `connections: Vec::new(),`。

- [x] **Step 3: 常量**（`TRAFFIC_HISTORY` 后追加）

```rust
/// 连接列表保留上限（快照替换天然有界，此处防御性截断）。
const CONNECTIONS_KEEP: usize = 200;
/// /connections 轮询间隔。
const CONNECTIONS_POLL: Duration = Duration::from_secs(3);
```

- [x] **Step 4: 排序纯函数**（放在 `spawn_traffic_task` 之前的空闲位置，如 `now_rfc3339()` 附近）

```rust
/// 连接排序：最新建立的在前（start 降序），start 缺失排末尾；
/// 同 start 按 upload+download 降序（活跃连接靠前）。
fn sort_connections(conns: &mut [ConnInfo]) {
    conns.sort_by(|a, b| {
        let ka = a.start.map(|t| t.timestamp()).unwrap_or(i64::MIN);
        let kb = b.start.map(|t| t.timestamp()).unwrap_or(i64::MIN);
        kb.cmp(&ka).then_with(|| (b.upload + b.download).cmp(&(a.upload + a.download)))
    });
}
```

- [x] **Step 5: 轮询后台任务**（`spawn_memory_task` 之后追加）

```rust
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
```

- [x] **Step 6: run_loop 接入**（三处修改）

(a) 签名追加参数：
```rust
    async fn run_loop(
        &mut self,
        mut traffic_rx: mpsc::UnboundedReceiver<BgMsg>,
        mut memory_rx: mpsc::UnboundedReceiver<MemoryFrame>,
        mut conns_rx: mpsc::UnboundedReceiver<ConnSnapshot>,
        mut ui_rx: mpsc::UnboundedReceiver<UiEvent>,
        mut sudo_rx: mpsc::UnboundedReceiver<String>,
    ) -> Result<(), BoxError> {
```

(b) `enum Act` 增加 `Conns(ConnSnapshot)`；`tokio::select!` 增加分支（放在 memory 分支后）：
```rust
                msg = conns_rx.recv() => match msg { Some(m) => Act::Conns(m), None => continue },
```

(c) match 分发增加：
```rust
                Act::Conns(snap) => self.on_conns(snap),
```

- [x] **Step 7: on_conns 处理器**（`on_memory` 之后追加）

```rust
    /// 连接快照 → 排序 → 截断上限 → 替换状态。
    fn on_conns(&mut self, snap: ConnSnapshot) {
        let mut conns = snap.connections;
        sort_connections(&mut conns);
        conns.truncate(CONNECTIONS_KEEP);
        self.state.connections = conns;
    }
```

- [x] **Step 8: run() 入口接线**（两处修改）

(a) channel 创建（`let (memory_tx, memory_rx) = ...` 后）：
```rust
    let (conns_tx, conns_rx) = mpsc::unbounded_channel();
```

(b) 任务 spawn（`spawn_memory_task` 后）：
```rust
    spawn_connections_task(client.clone(), conns_tx);
```

(c) run_loop 调用（`app.run_loop(traffic_rx, memory_rx, ui_rx, sudo_rx)` 改为）：
```rust
    let result = app.run_loop(traffic_rx, memory_rx, conns_rx, ui_rx, sudo_rx).await;
```

- [x] **Step 9: 测试构造器与单测**

(a) `test_app` 中 `AppState { ... }` 构造：`mem_history: VecDeque::new(),` 后加 `connections: Vec::new(),`。

(b) 追加排序单测（`mod tests` 内）：

```rust
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
```

- [x] **Step 10: 运行测试并提交**

Run: `cargo test 2>&1 | grep -E "^test result|FAILED|error" | head`
Expected: `test result: ok. 180 passed`（178 + sort_connections_order + on_conns_truncates_and_sorts）

```bash
git add src/app.rs
git commit -m "feat: app 接入 /connections 3s 轮询、连接排序与状态"
```

---

### Task 3: ui/dashboard.rs —— 布局重构（连接框 / 网络框 / 内存框）

**Files:**
- Modify: `src/ui/dashboard.rs`
- Test: `src/ui/dashboard.rs` 内追加 `mod tests`（文件当前无测试模块，需新建；注意文件已有 `use crate::app::{AppState, UiCommand};` 等 import）

前置依赖：Task 2 已完成（`st.connections: Vec<ConnInfo>` 存在）。

- [x] **Step 1: import 追加**

`use crate::core::client::ConnInfo;`（追加到现有 `use crate::core::merger::...` 附近，与现有 import 风格一致）。

- [x] **Step 2: 布局常量与宽度判定纯函数**（`const` 或文件级 fn，放在 `render_status` 前）

```rust
/// 连接框响应式隐藏阈值：body 宽度低于此值时不渲染左列连接框。
const CONNECTIONS_MIN_WIDTH: u16 = 60;

/// 宽度是否足够显示连接框。
fn connections_visible(width: u16) -> bool {
    width >= CONNECTIONS_MIN_WIDTH
}
```

- [x] **Step 3: render() 重构**（替换原 `render` 方法体——原实现为 status + `[left, right]` 60/40 后调 `render_traffic(f, left, st)` 与 `render_totals(f, right, st)`；新实现如下）

```rust
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
            match popup {
                DashPopup::Form(form) => form.render(f, area),
                DashPopup::Msg(msg) => msg.render(f, area),
            }
        }
    }
```

- [x] **Step 4: render_right / render_network**（将原 `render_traffic` 整体替换为以下两个函数；标题由 ` 实时网速 ` 改为 ` 网络 `；速率行追加右对齐累计）

```rust
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
        .title(Span::styled(" 网络 ", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)))
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

    let [l1, s1, l2, s2, _rest] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(3),
        Constraint::Length(1),
        Constraint::Length(3),
        Constraint::Min(0),
    ])
    .areas(inner);

    f.render_widget(
        Paragraph::new(rate_row("↑ 上行", up_rate, up_total, Color::Green, inner.width)),
        l1,
    );
    f.render_widget(
        Sparkline::default().data(&up_data).style(Style::default().fg(Color::Green)),
        s1,
    );
    f.render_widget(
        Paragraph::new(rate_row("↓ 下行", down_rate, down_total, Color::Blue, inner.width)),
        l2,
    );
    f.render_widget(
        Sparkline::default().data(&down_data).style(Style::default().fg(Color::Blue)),
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
    let pad = inner_width
        .saturating_sub((left.width() + right.width()) as u16);
    Line::from(vec![left, Span::raw(" ".repeat(pad as usize)), right])
}
```

注意：`Span::width()` 是 ratatui 提供的方法（`ratatui::text::Span::width(&self) -> usize`），无需额外 import。

- [x] **Step 5: render_connections**（新增，放在 `render_network` 之后；同时把原 `render_totals` 中"内存"部分抽成 `render_memory`，删除整个 `render_totals`）

```rust
/// 左列：最近连接列表（start 降序，已在 app 层排序）。
fn render_connections(f: &mut Frame, area: Rect, st: &AppState) {
    let block = Block::new()
        .title(Span::styled(" 连接 ", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)))
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
    let rows: Vec<Line> = st
        .connections
        .iter()
        .take(inner.height as usize)
        .map(conn_line)
        .collect();
    f.render_widget(Paragraph::new(rows), inner);
}

/// 单行连接：`{host} {TCP|UDP} ↑{upload} ↓{download}`；过长由 Paragraph 自动裁剪。
fn conn_line(c: &ConnInfo) -> Line<'static> {
    let kind = if c.meta.network == "tcp" {
        Span::styled("TCP", Style::default().fg(Color::Green))
    } else if c.meta.network == "udp" {
        Span::styled("UDP", Style::default().fg(Color::Blue))
    } else {
        Span::raw("?")
    };
    Line::from(vec![
        Span::raw(conn_host(c)),
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
    ])
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
```

- [x] **Step 6: render_memory**（从原 `render_totals` 中抽取内存部分，删除 `render_totals` 整体）

```rust
/// 内存框：inuse 数值 + Sparkline。
fn render_memory(f: &mut Frame, area: Rect, st: &AppState) {
    let block = Block::new()
        .title(Span::styled(" 内存 ", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)))
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
```

- [x] **Step 7: 单元测试**（文件末尾追加 `#[cfg(test)] mod tests`）

```rust
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

        c.meta = ConnMeta { host: String::new(), sniff_host: "sniffed.dev".into(), ..ConnMeta::default() };
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

    /// UDP 连接行含 UDP 标；TCP 行含 TCP 标。
    #[test]
    fn conn_line_kind_marker() {
        let tcp = ConnInfo { meta: meta("a.com", "tcp"), upload: 1024, download: 2048, ..ConnInfo::default() };
        let udp = ConnInfo { meta: meta("b.com", "udp"), ..ConnInfo::default() };
        let line_tcp = conn_line(&tcp);
        let line_udp = conn_line(&udp);
        assert!(line_tcp.to_string().contains("TCP"));
        assert!(!line_tcp.to_string().contains("UDP"));
        assert!(line_udp.to_string().contains("UDP"));
        assert!(line_tcp.to_string().contains("↑1.0 KB"));
        assert!(line_tcp.to_string().contains("↓2.0 KB"));
    }
}
```

注意：`Line::to_string()` 将 spans 拼接为字符串（不含样式），可用于断言。

- [x] **Step 8: 运行测试并提交**

Run: `cargo test 2>&1 | grep -E "^test result|FAILED|error\[|error:" | head -20`
Expected: `test result: ok. 183 passed`（180 + 3 新增）。app.rs 既有"小终端回归整帧渲染不 panic"测试（TestBackend 宽 30 < 60）会覆盖窄窗口路径；若有任何 test 失败或编译错误，修复后重跑。

```bash
git add src/ui/dashboard.rs
git commit -m "feat: 仪表盘布局重构——连接框（响应式）+ 网络框速率/累计合并 + 内存框"
```

---

### Task 4: README 仪表盘节更新

**Files:**
- Modify: `README.md`

前置：Task 1-3 分支代码已提交（README 描述与代码一致即可，无编译依赖，可与 Task 1/2 并行，但推荐在 Task 3 完成后做以便核对最终渲染效果；若并行则按本任务描述直接改）。

- [x] **Step 1: 更新简介与 ASCII 图**（约 13-42 行区域）

- 第 13 行附近 `**仪表盘（首页）**——模式/TUN/IPv6/出口 IP 热切换、实时网速双曲线、总流量、内存` 改为 `**仪表盘（首页）**——模式/TUN/IPv6/出口 IP 热切换、连接列表、网络速率/累计流量、内存`。
- ASCII 图（约 20-42 行）中：
  - `┌ 实时网速 ──...` 框改为 `┌ 连接 ──...`（左列）+ `┌ 网络 ──...`（右列上）+ `┌ 内存 ──...`（右列下）；
  - 左列连接框示例行：`example.com TCP ↑1.2M ↓300K`、`1.2.3.4:443 UDP ↑0 B ↓88 B`、`暂无活动连接` 三行示意；
  - 网络框内示意：`↑ 37.4 KB/s     累计 6.1 MB`、`↓ 102 KB/s      累计 58.2 MB` 及柱状图 `▁▂▃▅▆▇` 风格行（沿用现有 ASCII 风格）；
  - 内存框示意保持。
  - 图中响应式说明：左列连接框在窗口过窄（<60 列）时隐藏。

- [x] **Step 2: 更新布局要点列表**（约 50-51 行）

- 现 `- 左 60%：实时网速双 Sparkline（上行绿色、下行蓝色，120 样本环形缓冲）+ 当前速率` 改为 `- 左 60%：连接列表（GET /connections 每 3 秒轮询，按建立时间倒序；目标 host + TCP/UDP 色标 + ↑↓ 流量；窗口宽度 < 60 列时自动隐藏，网络/内存占满全宽）`
- 现 `- 右 40%：总流量（upTotal/downTotal 大数字）+ 内存占用（inuse + Sparkline）` 改为 `- 右 40%：网络（上行/下行速率左对齐 + 累计流量右对齐，双 Sparkline；累计来自 /traffic 流 upTotal/downTotal）+ 内存占用（inuse + Sparkline）`

- [x] **Step 3: 全文替换残留的「实时网速」字样**（帮助文本/按键表/FAQ 中如有）

Run: `grep -n "实时网速\|总流量" README.md`
- 所有"实时网速"改为"网络"（描述语境）或"网络框"；"总流量"改为"累计流量"（如提及）。
- 注意：不要改动与本次无关的段落（如 API 表 `PATCH /configs` 等）。`docs/` 目录下其他文档不动。

- [x] **Step 4: 核对并提交**

Run: `grep -n "实时网速" README.md`（应无输出）；`grep -n "连接" README.md | head`（应有新描述）
Expected: 无残留旧词；README 与实现布局一致。

```bash
git add README.md
git commit -m "docs: README 仪表盘节同步连接框/网络框布局"
```

---

### Task 5: 全量验证

**Files:** 无（验证 + 汇总）

- [x] **Step 1: 全量测试 + 构建**

Run: `cargo test 2>&1 | tail -3` 与 `cargo build 2>&1 | tail -1`
Expected: `test result: ok. 183 passed`；`Finished` 无 warning（或仅既有 warning）。

- [x] **Step 2: 更新计划勾选状态**（把本文件中各任务 checkbox 勾选；如用 executing-plans 由执行者处理）

- [x] **Step 3: 汇报**：向 feature_lead 报告：改动文件清单、测试数（174 → 183）、端到端验证建议（连真实 mihomo 时 `GET /connections` 有连接则列表显示；缩窄终端观察连接框隐藏）、README 同步情况、与 `.worktrees/fix-dashboard-toggle-persist` 会话的潜在合入冲突提示（双方同改 dashboard.rs，其改动在 handle_key 区域、本功能在 render 区域，git 三路合并可自动处理非重叠 hunk）。
