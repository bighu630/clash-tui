# 规则页 Ctrl+A 保存并应用 设计

**Goal:** 为规则页面增加 Ctrl+A 快捷键，实现保存并应用（合并生成 config.yaml -> mihomo -t 校验 -> mihomo-apply 提权写入 -> 重启），复用订阅激活/设置页的合并应用链路。

**Context:** 
- 规则页现有逻辑每次操作即落盘 overrides.toml，无未保存概念；设置页 Ctrl+A 通过 merge->ApplyConfig 触发后续校验与提权；订阅页激活也通过 merge->ApplyConfig。
- 用户要求：规则页正常显示，仅在发生变动后底部提示 Ctrl+A 保存并应用；成功/失败走已有通知路线；不处理全局快捷键冲突。

## 架构

- `src/ui/rules.rs::RulesPage` 增加 `dirty: bool` 标记（内存态，未落盘即后台？），每次新建/编辑/删除/移动成功后置 true；Ctrl+A 触发 `save_and_apply` 置 false 并 emit ApplyConfig。
- `save_and_apply` 复用 `core::merger::merge(MergeContext { settings, overrides, subscription: active })` 与订阅页 `activate_selected` 一致的合并与警告通知逻辑。
- 渲染：底部状态行 `Layout::vertical([Min(1), Length(1)])` ，dirty 时显示 `[未应用] Ctrl+A 保存并应用`，未 dirty 时显示常规 `n 新建 · Enter 编辑 · K/J 移动 · d 删除`（按需）。

## 文件触及

- `src/ui/rules.rs`: 新增 dirty 字段、mark_dirty、save_and_apply、handle_key Ctrl+A、render 底部提示
- `src/app.rs`: `HELP_LINES` 规则段新增 `Ctrl+A  保存并应用` 行；`page_hints(3)` 新增 `("Ctrl+A","应用")`

## 成功标准

- 规则页列表态（无弹窗）按 Ctrl+A：dirty 时触发合并并发送 ApplyConfig，成功/失败给出 notice/弹窗；无 dirty 时无操作或温和提示
- 帮助/底栏提示与改动一致；不影响设置页等其他页面
- `cargo test` 通过
- 提交 push 到 origin/dev

