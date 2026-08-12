# 日志页 + 通知自动隐藏 设计文档

日期：2026-08-12
状态：已与用户对齐

## 背景

用户希望新增一个专门的"日志"tab 页面展示 mihomo 完整日志流（当前项目**没有**任何日志展示：底栏只有操作通知条 `[✓]/[✗]/[!]` + 按键提示，`/logs` 端点尚未接入）。同时用户认为底部通知条"占地方"，希望通知超时后自动消失。

## 一、通知条改造（app.rs 底栏）

### 现状
- `AppState.notices: VecDeque<String>`，最多 5 条，渲染取最近 3 条，永不自动消失（只被新通知顶掉）。
- 底栏固定 `Constraint::Length(4)`：最多 3 行通知 + 1 行按键提示。

### 目标行为
1. **整组共享截止时间**：每条通知带到达时刻 `Instant`。组截止时间 = 组内所有通知中 `到达时刻 + 类型时长` 的最大值（锚定"时间最长的一条"，不管新旧）。到期后**整组通知同时消失**（避免一条条过期触发多次重绘）。
2. **类型时长**：`[✓]` 及普通通知 5s；`[✗]`/`[!]` 错误警告 10s（`[✗]` 前缀 `"[✗]"`、警告前缀 `"[!]"`，其余视为普通）。
3. **新通知到达时重新计算截止时间**：若新通知的 `到达+时长` 大于当前截止，截止延后；持续来通知则整组持续显示。
4. **底栏动态高度**：有可见通知时 = `1 + min(可见条数, 3)` 行（通知 + 提示）；全部过期时收成 **1 行（仅按键提示）**，内容区自动多出空间。按键提示行永驻。
5. 重要错误的弹窗兜底不变（应用失败/出口 IP 失败等 `result_popup` 逻辑不动）。

### 实现要点
- `AppState.notices` 类型改为 `VecDeque<(Instant, String)>`（容量仍 5）。
- `notice()` 记录 `Instant::now()`。
- `draw()` 中：计算可见通知集（`now - at < 时长`），若有则整组渲染（取最近 3 条）+ 提示行；若无则底栏仅提示行。渲染时顺带清理已过期条目。
- `bottom_bar_rows` 纯函数改为接收"可见通知条数"，返回 (通知行数, hint_y)；小终端 clamp 逻辑保留。
- 测试更新：`test_app` 的 `notices` 构造、`bottom_bar_rows` 越界回归（动态高度下 h=0..30 全扫）、通知过期隐藏行为测试。

## 二、日志页（第 5 个 tab "日志"）

### 数据源
- `Client::log_stream(level: LogLevel)`：GET `/logs?level={level}`，Bearer secret（复用 `stream_response` + `LineStream` 按行解析，与 `/traffic`、`/memory` 同模式）。
- 后台任务 `spawn_logs_task`：循环拉流，失败/断开 sleep 2s 重连（同 traffic 任务）。级别切换由任务按最新目标级别重连。
- 条目解析兼容两种格式：
  - 标准：`{"type":"info","payload":"..."}` → level=type, message=payload
  - structured：`{"time":"HH:MM:SS","level":"info","message":"...","fields":[]}` → 含时间
  - 解析失败的行降级为原始文本显示（level=debug 灰色兜底，不丢日志）。
- 级别：`error < warning < info < debug`，缺省 `info`（对齐 mihomo 默认）。服务端按阈值下发。

### UI（`src/ui/logs.rs` 新文件）
- 页面标题栏显示当前级别 + 缓冲条数。
- 渲染：底部跟随（自动滚动）；回溯（`↑/↓` 逐行、`PgUp/PgDn` 翻页）时暂停跟随；`f` 或 `End` 恢复跟随并跳到底部。
- 按键：`e` 循环切换级别 error→warning→info→debug（重连拉取）；`c` 清空缓冲；`q/Esc/Tab/1-5` 等全局键不变。
- 空状态提示："等待 mihomo 日志……（按 e 切换级别）"。
- 颜色：error=红、warning=黄、info=默认、debug=灰；structured 格式带时间前缀，标准格式无时间。

### 状态与容量
- 环形缓冲（`VecDeque`）上限 **1000 条**，超出淘汰最旧。
- 缓冲挂 **AppState.logs: VecDeque<LogEntry>**（与 `traffic`/`mem_history` 同一模式：后台任务 → 主循环 → AppState，页面 render 时只读）；切页不丢、退出清空。
- 日志页自身仅持有**视图状态**：当前级别、跟随/滚动偏移。
- 日志数据流：`spawn_logs_task` → `UiEvent::LogLine(LogEntry)` → 主循环 `on_ui_event` 推入 `AppState.logs`；级别切换：页面按 `e` 后发 `UiCommand::SetLogLevel(LogLevel)` → 主循环经专用 mpsc 通道（类似 exit_trigger）通知任务重连。
- 任务内部 `tokio::select!` 合并日志流与级别通道：级别变化时断开当前流、以新 `?level=` 重连（保留未消费的旧缓冲丢弃策略：直接重建流）。

### 与既有页面的关系
- 底栏通知条是**操作反馈**（应用配置、订阅刷新等），日志页是 **mihomo 运行时日志**，两者并存、职责分离。

## 三、Tab 导航扩展

- `TABS` 4 → 5：`["仪表盘", "订阅", "规则组", "规则", "日志"]`。
- 数字键 `1`..`'5'` 切换；Tab/←→/BackTab 循环逻辑不变（基于 `pages.len()`）。
- `page_hints` 增加第 5 页：`e` 级别 / `c` 清空 / `f` 跟随 / `↑↓` 滚动。
- `HELP_LINES` 增加"日志:"小节。

## 四、测试

1. **client**：`LogEntry` 两种格式解析（标准/structured）、字段缺失容忍、`log_stream` 与假服务器联测（含 `?level=` 查询参数断言、Bearer 头）。
2. **日志页**：级别循环、滚动暂停/恢复、清空、环形缓冲 1000 条截断（纯逻辑函数可单测）。
3. **app**：通知整组过期（5s/10s 锚定最长一条）、底栏动态高度（h=0..30 越界回归）、tab 切换 1-5。
4. **回归**：现有全部测试保持绿色。

## 五、实施协调

- 在独立 git worktree（基于 main 最新）开发，与 `feature/dashboard-connections`（仪表盘连接框，其 worktree 已存在）隔离。
- 冲突面：`src/app.rs`（tab/通知/事件循环）、`src/ui/mod.rs`（模块注册）。合并期协调。
- "规则组重定义"会话暂无 worktree/分支，若其开始开发需提前协调 app.rs 归属。

## 六、文档

- README：功能列表加"日志页（实时日志流、级别过滤、回溯）"；使用指南加日志页小节；按键表补日志页按键与通知自动隐藏说明。
