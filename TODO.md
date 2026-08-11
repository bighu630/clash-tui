# Todo — 修复代码审查发现的问题

## Major
- [x] M1: merger 兜底注入悬空引用（src/core/merger.rs 步骤 8）
- [x] M2: base64 包裹的分享链接订阅静默 0 节点（src/core/subscription.rs parse_links）
- [x] M3: 激活无缓存订阅静默应用空配置（src/ui/subscriptions.rs activate_selected）
- [x] M4: 激活失败仍持久化 active 标记（src/ui/subscriptions.rs activate_selected）
- [x] M5: 小终端渲染 panic 两处（src/app.rs notices、src/ui/widgets.rs SelectList）
- [x] M6: 首装引导死代码（src/app.rs、src/service/installer.rs）

## Minor
- [x] 1. apply.rs validate/apply 临时文件权限 0600
- [x] 2. settings.rs 配置目录 0700、文件 0600
- [x] 3. client.rs 空 secret 不带头
- [x] 4. merger.rs 步骤 2 自定义组重名去重报 MergeError + 测试
- [x] 5. app.rs on_ui_event traffic/ConfigsRefreshed 竞态去抖
- [x] 6. subscriptions.rs render sig 变化按名称恢复选中
- [x] 7. rules.rs move_rule 先落盘后 swap
- [x] 8. subscription.rs parse_yaml proxies 格式无效错误信息
- [x] 9. app.rs SubscriptionFetched 订阅被删时提示订阅已删除
- [x] 10. mihomo-apply.sh 健康检查轮询
- [x] README FAQ 补一条组链式引用限制

## 收尾
- [x] cargo test 全绿（120 通过）
- [x] cargo clippy --all-targets -- -D warnings 零告警
- [x] 逐个 commit（11 个）