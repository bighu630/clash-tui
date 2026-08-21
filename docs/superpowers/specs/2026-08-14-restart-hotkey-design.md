# 首页重启 mihomo 核心（快捷键 R）设计

日期：2026-08-14
状态：已与用户对齐（仅大写 R + 二次确认 + 通知 + 自动重连）

## 背景与目标

在首页（仪表盘）增加快捷键 R，通过 mihomo external-controller HTTP 接口重启核心，不改动 systemd/直连进程的底层启停脚本，兼顾两种运行模式与 Windows 兼容。

## 接口确认

- 端点：`POST http://{external_controller}/restart`，Header `Authorization: Bearer <secret>`（secret 空时不发），Body 兼容空或 `{"path":"","payload":""}`，复用现有 `auth_header()` 与 `REQUEST_TIMEOUT 5s`
- 路由注册：`hub/route/server.go` 注册 `/restart` POST；鉴权与 `/configs` 一致（Header Bearer 或 ?token=，TUI 统一走 Header）
- 运行模式兼容：systemd 与直连进程均由同一 mihomo 进程提供 API，接口均可用；Windows 进程模式同理
- 失败形态：`ApiError::Conn`（连不上/超时）、`ApiError::Status(401/403)`（鉴权失败）、`Status(500)`（重启失败）、其他非 2xx

## 交互设计

- 触发：仅首页（current==0）响应大写 `R`（Shift+r），小写 `r` 保留“刷新出口 IP”，避免冲突；其他页面按 R 无反应
- 二次确认：弹 `ConfirmPopup` “确认重启 mihomo 核心？” y/Enter 确认，n/Esc 取消（防误触）
- 加载态：确认后 `notice("[…] 正在重启...")`，置 `restarting=true` 防重入，忽略后续 R 直至完成
- 通知：成功 `notice("[✓] 核心已重启")`，失败弹 `MessagePopup("重启失败", [错误详情])` + `notice("[✗] 重启失败: …")`
- 自动刷新：成功后发送 `ReloadConfigs` 刷新 `runtime`/`api_ok`，后台 `traffic/memory/connections` 流在 2s 重连窗口内自动恢复；额外触发 `RefreshGroups` 与 `FetchExitIp` 可选（保持现有行为，最小改动仅 ReloadConfigs）
- 帮助/提示：`page_hints(0)` 新增 `R 重启`，`HELP_LINES` 仪表盘段新增 `R  重启核心（需确认）`，`README.md` 按键表补充

## 组件与数据流

### core/client.rs
- 新增 `pub async fn restart(&self) -> Result<(), ApiError>`：`POST /restart`，Bearer 鉴权，`timeout(REQUEST_TIMEOUT)`，非 2xx → `Status`
- 复用 `auth_header()`、`REQUEST_TIMEOUT`、`ApiError` 分类
- 单测：mock server 覆盖鉴权头、空 secret 不发头、成功 200、500/401 失败、Conn 失败

### ui/dashboard.rs
- `handle_key` 仅在 `current==0` 且无弹窗时响应 `Char('R')`（大写），返回 `UiCommand::RestartCore`；小写 `r` 保持原 `FetchExitIp`

### app.rs
- 新增 `UiCommand::RestartCore`、`UiEvent::RestartDone(Result<(), String>)`
- `App` 新增 `restart_confirm: Option<ConfirmPopup>`、`restarting: bool`（或复用 `pending_confirm` 但独立以不干扰 sudo 流）
- `handle_key`：全局弹窗优先后，若 `current==0` 且 `key == Char('R')` 且 `!restarting` → 弹确认框，不直接发命令
- 确认 `Some(true)` → `restarting=true`、`notice("正在重启...")`、`spawn_command(RestartCore)`
- `spawn_command(RestartCore)` → `client.restart().await` → `ui_tx.send(RestartDone)`
- `on_ui_event(RestartDone)` → `restarting=false`，成功 `notice("核心已重启")` + `cmd_tx.send(ReloadConfigs)`，失败 `popup_error` + 同步 ReloadConfigs 纠正状态
- `draw` 层渲染 `restart_confirm` 置顶（与 help/result/pending_confirm 同层）
- 帮助：`HELP_LINES` 新增行，`page_hints` 新增，`help_lines()` 过滤保持 Windows 兼容

### README
- 按键表补充 `R 重启核心`

## 测试

- client: mock server 单测 5+（鉴权、空 secret、成功、401、500、Conn）
- app: dashboard R 仅首页、r 不触发重启、确认/取消分支、RestartDone 成功/失败通知与 ReloadConfigs
- 存量 331 测试保持绿

## 非目标

- 不改动 `mihomo-apply.sh` / `mihomo-proc.sh` / `systemctl` 底层启停
- 不新增 ?force 参数（若后续发现需要，仅 client 层兼容）
