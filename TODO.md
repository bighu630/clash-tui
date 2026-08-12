# 日志页 + 通知自动隐藏 — 实施进度

- [x] Task 1: client.rs 数据层（LogLevel/LogEntry/log_stream + 测试）
  - 偏差：修复 LineStream 流结束分支合并多行残留的潜伏 bug（否则新测试无法通过）
- [x] Task 2: app.rs 日志数据管道（不含 tab/页面挂载）
  - 偏差：ui/dashboard.rs test_state 也需补 logs 字段（计划未提及，编译必须）
- [x] Task 3: ui/logs.rs 页面 + 模块注册
  - 偏差：visible_range 公式在 offset≥total 时产生空区间，与计划自身测试期望 (0,20) 矛盾，按测试语义修正
- [x] Task 4: app.rs tab 5 + 页面挂载 + 帮助/提示
  - 偏差：page_hints 的 3/4 分支需保留 _ 兜底分支才能编译（usize match 穷尽性）
  - 偏差：计划 Task 4 测试期望 Warning 与 Task 1 next() 契约（Info→Debug）矛盾，测试改为 Debug（另附 fix 提交）
- [x] Task 5: app.rs 通知整组过期 + 动态底栏
  - 偏差：notice_deadline 签名改 IntoIterator（计划的 &[(…)] 无法接收 &VecDeque）
  - 偏差：rustc 1.88 无 Instant::saturating_add，改 checked_add+兜底
- [x] Task 6: README
- [x] Task 7: 全量验证（220 测试全绿、clippy 0 警告、真实 mihomo 端到端验证通过）
- [ ] Task 7: 全量验证
