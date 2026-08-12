# 设置页（Settings Tab）设计

日期：2026-08-12
状态：已与用户对齐（方案 A + Ctrl+S 仅保存 / Ctrl+A 保存并应用 / 移除仪表盘 s 弹窗 / secret 只读+重新生成）

## 背景

用户需求："再加一个设置tab，把各种config里面可以配置的东西都放在设置里面"。

现状摸底结论：
- `NetworkSettings` 模型（`src/core/models.rs`）已含全部 22 个字段；`merger.rs` 已完整映射
  所有字段 → config.yaml；`settings.toml` 持久化完整（0600）；应用链路完整
  （save_settings → merge → `mihomo -t` 校验 → `mihomo-apply` 提权 → 重启）。
- 仪表盘 `s` 键是唯一表单入口（FormPopup 弹窗，11 字段），另有 11 个字段无任何 UI 入口：
  mode、ipv6、tun.enable（仅仪表盘热切键 m/t/6，无持久表单）、dns.listen、
  dns.enhanced-mode、dns.fake-ip-range、dns.default-nameserver、dns.fallback、
  dns.fake-ip-filter、external-controller、secret。
- 弹窗 Confirm → 解析失败直接关表单弹错误，已填内容丢失。

## 范围（方案 A，已确认）

设置页覆盖 NetworkSettings 现有全部 22 字段，**不新增模型字段、不改 merger**。

## 1. Tab 与导航

- TABS 5 → 6：仪表盘、订阅、规则组、规则、日志、设置（末位）；数字键 1-6。
- `s` 键**全局**跳转设置 tab；**移除**仪表盘 `s` 弹窗（`settings_form`/`apply_form`/
  `split_csv`/`yes_no`/`DashPopup::Form` 相关代码删除），保留 `DashPopup::Msg` 弹窗
  （toggle_double_write 保存失败提示仍用）。
- HELP_LINES 与 page_hints 更新（仪表盘 `s` 说明改为"跳转设置"；新增设置页按键说明）。

## 2. 页面布局（src/ui/settings.rs 新文件）

整页表单 + 垂直滚动，区块标题行（不可选中）+ 字段行：

| 区块 | 字段 | 类型 |
|---|---|---|
| 网络 | mode | 下拉 rule/global/direct |
| 网络 | ipv6 | 是/否 |
| 网络 | allow-lan | 是/否 |
| 端口 | port / socks-port / mixed-port | 数字 |
| 日志 | log-level | 下拉 silent/error/warning/info/debug |
| TUN | enable | 是/否 |
| TUN | stack | 下拉 system/gvisor/mixed |
| TUN | auto-route | 是/否 |
| TUN | mtu | 数字 |
| TUN | dns-hijack | CSV 文本 |
| DNS | enable | 是/否 |
| DNS | listen | 文本 |
| DNS | enhanced-mode | 下拉 fake-ip/redir-host |
| DNS | fake-ip-range | 文本 |
| DNS | nameserver / default-nameserver / fallback / fake-ip-filter | CSV 文本 |
| 其他 | external-controller | 文本 |
| 其他 | secret | 只读；选中按 Enter 重新生成 |

交互：
- ↑/↓ 或 Tab/BackTab 移动选中行（跳过区块标题）；Home/End 跳首/末字段。
- ←/→ 循环切换下拉；Text/Number 字段按 Enter 进入编辑模式（输入/退格/Delete/Home/End/←→），
  Esc 退出编辑模式；Dropdown 字段 Enter 无操作。
- **Ctrl+S** 仅保存；**Ctrl+A** 保存并应用。
- 页面底部状态行：显示「未保存」标记（当前值 ≠ 最近保存快照）、焦点字段提示。
- 校验失败：错误弹窗（MessagePopup），**表单内容保留、焦点不动**，不落盘。

## 3. 数据流与一致性

- 页面字段从 `st.settings` 初始化；切回 tab 时若非 dirty 则重新同步
  （仪表盘热切 m/t/6 写回 settings.toml + st.settings 的值进入设置页可见，双向一致）。
  若 dirty（有未保存编辑）则不覆盖，保留用户编辑。
- Ctrl+S：校验 → `save_settings` → `st.settings = 新值`（同 toggle_double_write 同步模式，
  保证 merger 读到最新持久化值）→ notice「[✓] 已保存」→ 清除 dirty。
- Ctrl+A：校验 → 落盘 + st.settings 同步 → `merge`（复用 dashboard apply_form 模式）→
  `UiCommand::ApplyConfig(yaml)` → 现有 mihomo -t → 提权 → 重启 → ApplyDone 通知链路。
- **无新增 UiCommand / UiEvent**。
- secret 重新生成：`generate_secret()`（`src/core/settings.rs` 已有）→ 写入字段 → 标记 dirty
  → 需 Ctrl+S/Ctrl+A 才落盘。

## 4. 纯函数抽取（可单测）

`src/ui/settings.rs` 内纯函数：
- `field_values(&NetworkSettings) -> Vec<FormField>`：模型 → 表单字段（22 个）。
- `apply_values(&[FormField]) -> Result<NetworkSettings, String>`：表单值 → 模型 + 校验
  （端口 0-65535 且非空；CSV 解析去空项；mode/log-level/enhanced-mode/stack 由下拉保证合法；
  listen/external-controller/fake-ip-range 非空）。
- 快照比较判 dirty（保存时记录 `saved: Vec<String>`，渲染时与当前 values 比较）。

## 5. 测试

- 22 字段往返：field_values → apply_values → 与默认/构造设置全等（复用
  `with_settings_dir` 串行锁辅助，settings 往返测试模式）。
- 校验错误用例：非法端口（0、65536、空）、CSV 空串、空 listen 等返回 Err 且错误信息含字段名。
- secret 重新生成：长度 32 hex、不影响其他字段。
- 现有测试（settings_roundtrip / toggle_fields_roundtrip / merger / dashboard 等）保持全绿；
  cargo build + clippy 0 警告。
- 端到端：改 dns.listen / 端口后 Ctrl+A，mihomo 配置生效（config.yaml 检查 + mihomo -t）。

## 6. 文档

- README：功能总览加设置页说明（区块、按键、保存语义、与仪表盘热切的关系）；按键表更新。
- 帮助弹窗 HELP_LINES 更新。

## 非目标（YAGNI）

- 不新增 mihomo 配置项（sniffing/tcp-concurrent/geo-auto-update 等）。
- 不做"保存前确认切走"弹窗：未保存编辑在切走再回来时保留（dirty 不覆盖），
  重启/退出不拦截。
- 不改 merger、不改模型、不加 UiCommand。
