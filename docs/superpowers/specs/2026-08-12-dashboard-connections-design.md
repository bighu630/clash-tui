# 仪表盘布局调整：连接列表 + 网速/累计合并（2026-08-12）

## 背景与目标

当前仪表盘为「左 60% 实时网速（双 Sparkline + 速率）+ 右 40%（总流量 + 内存）」。
本次调整（用户已确认）：

1. **取消「总流量」独立框**：累计流量（upTotal/downTotal）并入「网络」框（原「实时网速」改名）的速率行——速率左对齐、累计右对齐同排显示；双柱状图保留。
2. **左侧整列为「连接」框（全高）**：展示最近连接列表，数据源 `GET /connections`。
3. **响应式**：窗口（body）宽度不足时整列隐藏「连接」框，右侧「网络 + 内存」占满全宽。
4. 「内存」框内容不变。

## 最终布局（ASCII）

```
┌─ 状态行 ──────────────────────────────────────────────────┐
├───────────────────────────┬──────────────────────────────┤
│ 连接（全高）               │ 网络                          │
│ example.com TCP ↑1.2M     │ ↑ 37.4 KB/s     累计 6.1 MB   │
│ 1.2.3.4:443 UDP ↓300K     │ [上行柱状图]                  │
│ ...                       │ ↓ 102 KB/s      累计 58.2 MB  │
│                           │ [下行柱状图]                  │
│                           ├──────────────────────────────┤
│                           │ 内存                          │
│                           │ 123 MB inuse [柱状图]          │
└───────────────────────────┴──────────────────────────────┘
```

- 左 60% / 右 40%（沿用现有比例）；body 宽 `< 60` 列时左列隐藏，右列 100%。
- 右列上「网络」/下「内存」各 50% 高度（沿用现有布局约束）。

## 数据源

### /traffic（不变，现有流式）
每帧 `{up, down, upTotal, downTotal}`。速率 = up/down（B/s）；累计 = upTotal/downTotal。
**网络框累计值直接来自此流，无需新请求。**

### GET /connections（新增，轮询快照）
响应 JSON（mihomo API，字段已对照 potoo0/mihomo-tui 参考实现核对）：

```json
{
  "downloadTotal": 0, "uploadTotal": 0,
  "connections": [
    {
      "id": "...",
      "metadata": { "network": "tcp", "type": "HTTP", "sourceIP": "127.0.0.1",
                    "destinationIP": "1.1.1.1", "sourcePort": "55555",
                    "destinationPort": "443", "host": "example.com",
                    "dnsMode": "fake-ip", "processPath": "/usr/bin/curl",
                    "specialProxy": "", "uip": "", "remoteDestination": "1.1.1.1:443",
                    "sniffHost": "" },
      "upload": 100, "download": 200,
      "start": "2026-08-12T10:00:00.000Z",
      "chains": ["DIRECT"], "rule": "DIRECT", "rulePayload": "",
      "dlSpeed": 0, "ulSpeed": 0, "isAlive": true
    }
  ],
  "memory": 0
}
```

- `connections` 可能缺失（无连接时 mihomo 返回 `{"connections":[]}` 或空列表）——按空处理。
- `start` 为 RFC3339(Nano)；解析失败或缺失的连接按"最旧"处理（排末尾）。
- **刷新方式：轮询**（用户确认推荐 A）：每 3 秒 `GET /connections` 全量快照替换。
  理由：无需新增 WS 依赖（tokio-tungstenite）、快照语义无残留连接、仪表盘列表无实时性要求。

## 组件设计

### core/client.rs
- 新增模型（serde 反序列化，camelCase 重命名，字段缺失容忍）：
  - `ConnSnapshot { download_total, upload_total, connections: Vec<ConnInfo>, memory }`
  - `ConnInfo { id, metadata: ConnMeta, upload, download, start: Option<DateTime<Utc>>, chains, rule, rule_payload, dl_speed, ul_speed, is_alive }`（dlSpeed/ulSpeed 可选字段，缺失为 0）
  - `ConnMeta { network, host, sniff_host, remote_destination, destination_ip, destination_port, source_ip, source_port, r#type, process_path }`
- 新增 `Client::get_connections() -> Result<ConnSnapshot, ApiError>`（GET /connections，Bearer 鉴权，走现有 `request_text`）。
- 单元测试：完整 JSON 解析、字段缺失容忍、connections 缺失/空、start 格式解析（RFC3339 + RFC3339Nano）。

### app.rs（AppState + 后台任务 + 事件循环）
- `AppState` 新增 `connections: Vec<ConnInfo>`（轮询快照直接替换，无需 VecDeque）。
- 新增 `UiEvent::ConnectionsFetched(Result<ConnSnapshot, String>)` 或独立 channel——**采用与 memory 一致的独立 mpsc channel 模式**（`spawn_connections_task`：每 3s 轮询，失败 sleep 2s 重连；API 状态联动复用 traffic 任务的 Api 信号即可，不额外通知）。
- 事件循环 `tokio::select!` 增加 `connections_rx.recv()` 分支；`run_loop` 签名增加参数。
- 排序（展示前做，存原始顺序亦可）：**按 start 降序（最新在上），start 缺失排末尾；同 start 按 upload+download 降序**。
- 状态保留上限 200 条（快照替换天然有界，此处仅防御性截断）。
- 单元测试：排序逻辑（新连接在前、缺失 start 排尾、同秒按流量降序）、上限截断。

### ui/dashboard.rs
- **布局重构**：
  - `render()`：`body.width < 60` → 隐藏左列（`render_connections` 不调用，右列 `Constraint::Percentage(100)`）；否则左 60% 连接 / 右 40% 网络+内存。
  - 宽度判定抽成纯函数 `fn connections_visible(width: u16) -> bool`（可单测）。
- **「网络」框**（原 `render_traffic` 改名/改内容）：
  - 标题 ` 网络 `（Cyan 加粗，样式同现有）。
  - 每行：`↑ {format_rate(up)}`（绿加粗，左对齐）+ 右侧 `累计 {format_bytes(up_total)}`（绿，右对齐，`Line::alignment(Right)` 或 span 内补空格）。
  - 下行同理（蓝）。双 Sparkline 保留（3 行高）。
- **「连接」框**（新增 `render_connections`）：
  - 标题 ` 连接 `（Cyan 加粗），边框样式同现有。
  - 行内容：目标 host（`metadata.host` → `sniffHost` → `remoteDestination` → `destinationIP:port` 兜底）+ 类型色标（TCP 绿 / UDP 蓝）+ `↑{upload} ↓{download}`（format_bytes）。
  - 行数 = 框内可用高度（`inner.height`），不足截断；无连接显示 `暂无活动连接`（DarkGray）。
  - 布局：左列内「网络」上 /「连接」下——注意：本方案左列为连接全高，右列为网络+内存，故左列不再分上下（见布局图）。
- 删除原 `render_totals`（总流量框取消）；内存框渲染逻辑移入右列下半。
- 单测：`connections_visible` 阈值（59/60/61 列）。

### 其他
- 无新依赖（reqwest 已有；chrono 已有）。
- 无新按键；帮助文本不变（若提及"实时网速"字样则同步改）。
- README：仪表盘节描述与 ASCII 图更新（`实时网速` → `网络`，增加 `连接` 框说明与响应式行为说明）。

## 错误处理

- `get_connections` 失败（网络/HTTP 状态/JSON 解析）：轮询任务 sleep 2s 重试，保留上一次成功数据（列表不闪烁清空）；不产生新通知（API 状态联动由 traffic 任务负责，避免重复通知刷屏——与现有 memory 任务一致）。
- JSON 单字段缺失容忍为默认值；整个响应解析失败按错误重试。

## 测试

1. client.rs：ConnSnapshot/ConnInfo JSON 解析（全字段、缺字段、connections 缺失、start 无/有/非法）、get_connections 与假服务器联测（新增 `/connections` 路由到现有 `spawn_api_server`）。
2. app.rs：排序逻辑单测（最新在前、缺 start 排尾、同秒按流量降序）、上限 200 截断。
3. dashboard.rs：`connections_visible` 阈值单测（59/60/61）。
4. 保持现有 174 测试全绿（`cargo test`）。
5. 端到端：连真实 mihomo 验证有连接时列表显示、缩窄窗口连接框隐藏。

## 验收标准

- [ ] 「总流量」框消失；「网络」框速率左/累计右同排，双柱状图保留
- [ ] 左列「连接」框显示最近连接（host + 类型色标 + 流量），按 start 降序
- [ ] 窗口窄于 60 列时「连接」框隐藏，右列占满
- [ ] 无新依赖、无新按键；`cargo test` 全绿（174 + 新增）
- [ ] README 仪表盘节同步更新
