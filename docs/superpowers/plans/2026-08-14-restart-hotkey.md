# 首页重启 mihomo 核心（快捷键 R）实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 首页按大写 R 经 `POST /restart` 重启 mihomo 核心，二次确认、加载态与通知，成功后刷新 API 状态与流数据

**Architecture:** `Client::restart` 复用 Bearer 鉴权与超时；`DashboardPage` 仅首页响应 `R` 发 `RestartCore`；`App` 集中确认弹窗与 `restarting` 防重入，`spawn_command` 调 `client.restart` 并回 `RestartDone` 刷新 `ReloadConfigs`

**Tech Stack:** Rust (tokio/reqwest/ratatui/crossterm), mihomo external-controller REST API

**工作区：** `/data/code/clash-tui/.worktrees/restart-hotkey`（分支 `feature/restart-hotkey`，基于 dev）

---

### Task 1: core/client.rs 重启接口与单测

**Files:**
- Modify: `src/core/client.rs`

- [ ] **Step 1: 新增 restart 方法（复用鉴权与超时）**

在 `impl Client` 中新增（置于 `get_connections` 之后、`request_text` 之前）：

```rust
/// POST /restart 重启核心（systemd/直连进程/Windows 均经同一 external-controller）。
pub async fn restart(&self) -> Result<(), ApiError> {
    let mut req = self
        .http
        .post(self.url("/restart"))
        .timeout(REQUEST_TIMEOUT)
        .header(CONTENT_TYPE, "application/json");
    if let Some(auth) = self.auth_header() {
        req = req.header(AUTHORIZATION, auth);
    }
    // 兼容 mihomo 两种接受形态：空 JSON 与 {"path":"","payload":""} 均可；此处发空 JSON 对象
    let resp = req
        .body("{}")
        .send()
        .await
        .map_err(|e| ApiError::Conn(e.to_string()))?;
    if !resp.status().is_success() {
        return Err(ApiError::Status(resp.status().as_u16()));
    }
    Ok(())
}
```

- [ ] **Step 2: 扩展 mock server 联测**

在 `tests` 末尾新增（参考现有 `spawn_api_server` 风格，新增 `restart` 分支或新建 `spawn_restart_server` helper）：

```rust
#[tokio::test]
async fn restart_sends_post_with_bearer_auth() {
    let (port, mut rx) = spawn_api_server().await;
    client_on(port).restart().await.unwrap();
    let req = rx.recv().await.unwrap();
    assert!(req.starts_with("POST /restart"));
    assert!(req.to_lowercase().contains("authorization: bearer testsecret"));
}

#[tokio::test]
async fn restart_without_secret_omits_auth() { ... }

#[tokio::test]
async fn restart_http_500_returns_status_error() { ... } // 类似 patch_configs_http_500

#[tokio::test]
async fn restart_conn_error() { ... } // 连接被拒

#[tokio::test]
async fn restart_401_returns_status_error() { ... }
```

- [ ] **Step 3: 验证**

Run: `cargo test core::client -- --nocapture` 预期新增 5 测试通过，存量不变
Run: `cargo fmt && cargo clippy -- -D warnings` 0 警告

- [ ] **Step 4: Commit**

```bash
git add src/core/client.rs
git commit -m "feat: client 增加 restart 接口（POST /restart，复用鉴权与超时）"
```

---

### Task 2: app.rs + ui/dashboard.rs 重启交互与状态刷新

**Files:**
- Modify: `src/ui/dashboard.rs`
- Modify: `src/app.rs`

- [ ] **Step 1: 定义 UiCommand/UiEvent**

`src/app.rs` 中：

```rust
pub enum UiCommand {
    // ... existing
    RestartCore,
}
pub enum UiEvent {
    // ... existing
    RestartDone(Result<(), String>),
}
```

- [ ] **Step 2: dashboard 仅大写 R 触发**

`src/ui/dashboard.rs` 的 `Page::handle_key` 中新增分支（与 `r` 并列，注意大小写）：

```rust
KeyCode::Char('R') => return Some(UiCommand::RestartCore),
KeyCode::Char('r') => return Some(UiCommand::FetchExitIp), // 保持不变
```

且仅在 `Page` 层触发，`app.rs` 再做首页限定（见下一步）。

- [ ] **Step 3: App 状态与弹窗**

`App` struct 新增：

```rust
restart_confirm: Option<ConfirmPopup>,
restarting: bool,
```

初始化置 `None`/`false`，`test_app` helper 同步补字段（搜索 `test_app` 三处：`app.rs:1502` 附近、`dashboard.rs:475`、`settings.rs:721` 的 AppState 构造若涉及时）。

- [ ] **Step 4: handle_key 仅首页响应 R 且经确认**

在 `App::handle_key` 的全局弹窗优先后、页面 `popup_open/consumes_global_keys` 检查后、全局 `match key.code` 之前或之中插入：

```rust
// 重启确认弹窗优先
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
if self.restarting {
    // 重启中忽略 R
    if key.code == KeyCode::Char('R') { return None; }
}
// 仅首页 R 弹确认
if self.current == 0 && key.code == KeyCode::Char('R') && !self.restarting {
    // 若 App 内已将 current 0 的 R 下发给页面会重复，需在页面 handle 前拦截
    self.restart_confirm = Some(ConfirmPopup::new(
        "重启确认".into(),
        "确认重启 mihomo 核心？".into(),
    ));
    return None;
}
```

并确保 `draw` 中渲染 `restart_confirm`（置顶，与 help/pending_confirm 同层）。

注意：若保留 `dashboard.rs` 触发 `RestartCore`，则 `app.rs` 的 `current==0` 检查应在 `page.handle_key` 返回 `RestartCore` 后转确认，而非直接按键拦截。二选一实现，保持不重复：推荐 `dashboard.rs` 仅首页？但 dashboard 不知 current，故由 `app.rs` 统一拦截按键更稳妥，此时 `dashboard.rs` 的 `Char('R')` 分支可保留但会被 app 前置拦截覆盖；或让 `dashboard.rs` 不处理 R，全由 app 处理。任一保持“仅首页 R 生效”。

- [ ] **Step 5: spawn_command 与 on_ui_event**

```rust
UiCommand::RestartCore => {
    let client = self.client.clone();
    let ui_tx = self.ui_tx.clone();
    tokio::spawn(async move {
        let res = client.restart().await.map_err(|e| e.to_string());
        let _ = ui_tx.send(UiEvent::RestartDone(res));
    });
}
```

```rust
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
```

- [ ] **Step 6: 文案**

`src/app.rs` 中：

`HELP_LINES` 仪表盘段在 `r 刷新出口 IP` 后新增：`R  重启核心（需确认）`

`page_hints(0)` 在 `r/s` 之后 `i` 之前插入 `("R","重启")`

- [ ] **Step 7: 单测**

在 `src/app.rs` tests 新增：

```rust
#[test] fn restart_only_on_dashboard() { // current !=0 按 R 不弹确认 }
#[test] fn restart_confirm_and_cancel() { // R -> confirm, y -> RestartCore, n -> cancel }
#[test] fn restart_restarting_blocks_reentry() { ... }
#[test] fn restart_done_success_notices_and_reloads() { ... }
#[test] fn restart_done_failure_popup_and_reloads() { ... }
#[test] fn dashboard_r_lowercase_not_restart() { // dashboard page Char('r') -> FetchExitIp }
```

- [ ] **Step 8: 验证**

Run: `cargo test` 全绿, `cargo fmt`, `cargo clippy -- -D warnings` 0 警告, `cargo build` 通过

- [ ] **Step 9: Commit**

```bash
git add src/app.rs src/ui/dashboard.rs
git commit -m "feat: 首页 R 重启核心（确认弹窗、加载态、通知与状态刷新）"
```

---

### Task 3: README 与帮助文案收尾

**Files:**
- Modify: `README.md`

- [ ] **Step 1: 更新按键表**

`README.md` 仪表盘按键表补充一行：`| R | 重启核心（需确认，经 POST /restart，systemd/直连进程/Windows 通用）|`

- [ ] **Step 2: 验证**

`cargo test` 全绿，`cargo fmt`

- [ ] **Step 3: Commit**

```bash
git add README.md docs/superpowers/specs/2026-08-14-restart-hotkey-design.md docs/superpowers/plans/2026-08-14-restart-hotkey.md
git commit -m "docs: 更新重启快捷键说明与设计/计划文档"
```

---

### Task 4: 全量验证与推送

**Files:** 无新增，仅验证

- [ ] **Step 1: 全量验证**

Run:
```
cargo fmt --check
cargo clippy -- -D warnings
cargo test
cargo build --release
```

- [ ] **Step 2: 推送**

```bash
git push -u origin feature/restart-hotkey
```

---

## Self-Review

- Spec coverage: 4 节均有点对点任务
- Placeholder: 无
- Type consistency: UiCommand::RestartCore / UiEvent::RestartDone / restart_confirm / restarting 命名统一
