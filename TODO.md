# TODO: 策略组嵌套（组引用组）全链路支持

## 诊断结论（已完成）
根因：`merger.rs is_valid_member` 有效集合只含订阅节点+内置名，不含组名 →
1. 自定义组引用其他组 → MergeError「成员不存在」
2. 订阅组链式引用 → 成员被误剔除（warning）
3. UI 成员选择器只列订阅节点，无法勾选组
4. 无循环引用检测（mihomo 对循环 fatal）

## 任务
- [x] 读代码定位根因（merger.rs / groups.rs / models.rs / README）
- [ ] Worker A：merger.rs 重构 + 单元测试（TDD）+ models.rs 注释
- [ ] Worker B：groups.rs 成员选择器 + README FAQ 同步
- [ ] cargo build/test 全绿（排除环境性 apply 测试失败）
- [ ] reviewer 审查循环
- [ ] 报告

## 验收
- 自定义组可引用：其他自定义组（含后定义）/订阅组/DIRECT 等内置名
- 订阅组链式引用保留（组名不被误剔除）
- 被引用组不存在（订阅切换后）→ 明确 MergeError
- 直接/间接循环引用 → 写前检测，MergeError 带循环路径
- 新增测试：嵌套输出正确、组名不误剔除、级联剔除、循环检测（直接/间接/混合）
