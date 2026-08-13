# 运行方式抽象（systemd / 直接进程）设计

日期：2026-08-13
状态：已与用户对齐（决策 1/2/3 确认；新增要求：启动的 mihomo 不得是 TUI 子进程、须记录 PID）

## 背景与目标

脱离 systemd 依赖的选项：用户告诉 TUI mihomo 核心二进制路径，TUI 自己启动/重启 mihomo
（需要 root 权限）。默认仍为 systemd 模式（现状保留），运行方式在设置页切换。

用户确认的要求：

1. 运行方式枚举（设置页切换，持久化到 settings.toml）：systemd（现状）/ 直接进程
2. 权限模型：TUI 普通用户不能直接 spawn root 进程；进程启停走扩展 root 包装脚本体系
   （已确认：新增独立脚本 mihomo-proc，sudoers 追加同款无参 NOPASSWD 授权）
3. 路径安全：mihomo 二进制路径不得由 TUI 直接传给 root 脚本执行；路径存 root 侧配置文件
   `/etc/mihomo-tui/mihomo.conf`（root:root 0600），修改路径走一次交互式提权（sudo 密码）；
   root 脚本启动时只读该配置文件
4. 进程管理：PID 文件（防误杀、防多实例）、启动命令与 systemd 模式一致（`mihomo -d /etc/mihomo`）、
   stdout/stderr 重定向、启动失败检测
5. **新增（用户确认）**：启动的 mihomo 进程不得是 TUI/sudo 的子进程——TUI 关闭后服务必须继续
   运行；须有地方记录服务 PID（方便重启）
6. 已知取舍：进程模式无开机自启（设置页提示，不做）

## 安全模型（不变式）

| 不变式 | 说明 |
|---|---|
| sudoers 授权面不扩大 | 新增 `/usr/local/sbin/mihomo-proc` 与 mihomo-apply 同款：无参脚本 + stdin 数据。两条规则是"两个无参脚本"的并集，每个脚本仍是单一职责、可独立审计 |
| 路径不得由 TUI 传入 | 路径唯一事实源 = `/etc/mihomo-tui/mihomo.conf`（root:root 0600）。mihomo-proc 的 start/apply 等操作**只读**该文件，不接收任何路径参数 |
| 路径修改走交互式提权 | 保存路径 = 退出 raw 模式 → 用户输 sudo 密码 → `sudo tee` + chown/chmod（复用 installer 机制）。**不在 NOPASSWD 范围**——否则被攻破的 TUI 可任意设路径+触发启动 = 任意代码执行 |
| 无参数、无拼接 | 脚本不接受命令行参数（sudoers 无参授权）；stdin 首行命令白名单匹配（apply/start/stop/restart/status），非白即拒；路径只出现在引号包裹的变量中，不做 eval |
| 路径字符集约束 | 绝对路径（`/` 开头）+ 字符集 `[A-Za-z0-9_.+/-]`（无空白、无控制字符）。TUI 保存时与脚本读取时双重校验——协议无转义面 |
| 防误杀 | kill 前校验 `/proc/<pid>/cmdline` 首个 NUL 字段 == 配置的二进制路径（精确匹配） |
| 防多实例 | PID 文件存在且进程存活 → start 拒绝（报"已在运行"）；PID 文件残留（进程已死）→ 启动时清理覆盖 |
| 守护化 | `setsid mihomo -d /etc/mihomo </dev/null >>/var/log/mihomo/mihomo.log 2>&1 &`——mihomo 进入**新会话**，脱离 TUI/sudo 的会话与进程组，无控制终端；TUI 退出/终端关闭不产生 SIGHUP，进程被 init 收养继续运行。脚本退出后 PID 写入 `/run/mihomo-tui/mihomo.pid` 供 restart/stop/status 使用 |

环境变量测试钩子（`MIHOMO_TUI_TEST_CONFIG_DIR` / `_RUN_DIR` / `_CONF_FILE` / `_LOG_FILE`）：
sudo 默认 `env_reset` 清空环境，真实调用路径恒为固定路径，测试钩子仅在本机非 sudo 直接执行脚本时生效，不构成注入面。

## 组件与数据流

### 运行模式枚举（settings.toml）

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum RunMode { #[default] Systemd, Direct }
```

`NetworkSettings` 新增 `run_mode: RunMode`（`#[serde(default)]`——旧 settings.toml 无此字段时
兼容为 systemd）。UI 显示名：`systemd` / `direct`。

### mihomo-proc 脚本（resources/mihomo-proc.sh，新文件）

`/usr/local/sbin/mihomo-proc`（root:root 0755），stdin 行协议：**首行命令**，apply 时后续为
config.yaml 全文：

| 命令 | 行为 |
|---|---|
| `apply` | 读 stdin 剩余内容到临时文件 → `mihomo -t -f` 校验 → 备份旧配置 → 原子替换（chown root:root、chmod 600、mv）→ 停旧进程（PID 文件）→ 若 systemd 服务 active 则先 `systemctl stop mihomo`（防端口冲突）→ setsid 启动新进程 → 健康检查（进程存活 + cmdline 匹配，最多 5s）→ 失败回滚（恢复备份配置 + 重启） |
| `start` | 读 conf 校验路径 → systemd 服务 active 则先停 → 已在运行则报错 → setsid 启动 → 写 PID 文件 → 健康检查 |
| `stop` | 读 PID 文件 → cmdline 校验 → SIGTERM，5s 未退 SIGKILL → 删除 PID 文件；未运行视为成功 |
| `restart` | stop（忽略未运行）+ start |
| `status` | 输出行协议 `key=value`：`bin=`（conf 中路径，未配置为空）、`pid=`、`running=true/false`（PID 文件 + kill -0 + cmdline 校验）、`config=` |

脚本结构（关键点）：

- 固定路径常量（可被 MIHOMO_TUI_TEST_* 覆盖，见安全模型）
- `read_bin()`：读 conf 单行 `mihomo_bin=<path>` → 去前缀 → 字符集校验（`/` 开头 + 白名单字符）→ 存在且可执行校验；失败输出 ERROR 到 stderr 并退出 1（status 命令例外：输出空 bin 不退出）
- `proc_alive(pid)`：`kill -0` 且 `/proc/$pid/cmdline` 首字段 == bin 路径
- `stop_proc()`：PID 文件缺失 → 未运行；进程存活 → TERM → 轮询 ≤5s → KILL → 删 PID 文件
- `start_proc()`：`setsid "$BIN" -d "$CONFIG_DIR" </dev/null >>"$LOG_FILE" 2>&1 &`；
  `$!` 即 mihomo PID（非交互脚本无 job control，setsid 不 fork，直接 exec——脚本头注释说明
  推理与防御校验）；`echo $! > PID 文件`；sleep 0.5 后校验存活+cmdline，失败则 tail 日志
  最后 5 行到 stderr、删 PID 文件、退出 1
- `ensure_service_stopped()`：`systemctl is-active --quiet mihomo` 为真则 `systemctl stop mihomo`
  （root 能力；仅防端口冲突，TUI 侧操作前已有确认/通知）
- 日志文件 `/var/log/mihomo/mihomo.log`：root:root 0600（与配置同隐私策略），启动前 `mkdir -p` + 确保可写，失败即报错（启动失败检测的一部分）

### mihomo-apply 改动（小）

systemd 模式 apply 前增加**进程实例守卫**：若 `/run/mihomo-tui/mihomo.pid` 存在且进程存活
（cmdline 校验）→ 先停掉再 `systemctl restart`。原因：用户切到 systemd 模式后自动停进程是
异步的，紧接 Ctrl+A 存在竞态窗口；守卫使行为确定（防御纵深，脚本内 ~10 行）。

### 核心层（core/apply.rs）

- `apply_config(yaml, non_interactive, mode)`：签名增加 mode；systemd → 现状（sudo mihomo-apply）；
  direct → `sudo [-n] mihomo-proc`，stdin = `"apply\n" + yaml`。错误分类复用
  `classify_sudo_failure`（SudoNeedsPassword / NotInSudoers / CommandFailed）
- 新增 `ProcOp { Start, Stop, Restart }` + `proc_control(op)`：`sudo [-n] mihomo-proc`，stdin =
  命令行；返回 ApplyOutcome（复用）
- 新增 `proc_status()`：`sudo -n mihomo-proc` stdin=`status` → 解析行协议 → `ProcStatus {
  bin: Option<String>, pid: Option<u32>, running: bool }`（纯解析函数可单测）
- 新增 `RunStatus { service_active: Option<bool>, proc: Option<ProcStatus> }`：模式相关的统一状态
- `validate_config` 不变（用户态 `mihomo -t` 预校验，两种模式共用）

### 安装器（service/installer.rs）

- 常量：`PROC_SCRIPT = "/usr/local/sbin/mihomo-proc"`、`PROC_CONF_DIR = "/etc/mihomo-tui"`、
  `PROC_CONF = "/etc/mihomo-tui/mihomo.conf"`
- `sudoers_line()` → 两条规则（`sudoers_lines()` 返回 `Vec<String>`；保留既有测试语义）
- install() 增加：安装 mihomo-proc（sudo tee + chown root:root + chmod 755，步骤序号更新）；
  sudoers 写两行；安装日志相应更新。幂等（重复安装覆盖）
- 新增 `pub async fn save_mihomo_bin(path) -> Result<Vec<String>, InstallError>`：TUI 侧预校验
  （存在、可执行、`<path> -v` 版本探测输出含 mihomo）→ `sudo mkdir -p /etc/mihomo-tui` →
  `sudo tee` 写 `mihomo_bin=<path>` → `sudo chown root:root` + `sudo chmod 600`。
  **交互式 sudo（需密码）**，由 app 的 InteractiveTask 在恢复终端后调用
- 新增 `pub fn validate_mihomo_bin(path) -> Result<(), String>`（同步、可单测：存在/可执行/
  字符集/`-v` 探测）与 `pub fn is_proc_script_installed() -> bool`

### 设置页（ui/settings.rs）

区块插到最前：`SECTIONS = [("运行方式", 0, 6), ("网络", 6, 3), ("端口", 9, 3), ("日志", 12, 1),
("TUN", 13, 5), ("DNS", 18, 8), ("其他", 26, 2)]`，`FIELD_COUNT = 28`。前 6 个字段：

| idx | label | kind | 交互 |
|---|---|---|---|
| 0 | run-mode | Dropdown([systemd, direct]) | Enter 循环；Ctrl+S 保存持久化；切换 systemd 时若进程实例运行中 → 保存后自动 stop（见 app.rs） |
| 1 | mihomo-bin | ReadOnly | 显示路径（来自 run_status，未设置显示提示）；Enter → FormPopup 输入新路径 → 确认 → `UiCommand::SaveMihomoBin`（交互式提权保存） |
| 2 | mihomo-status | ReadOnly | 状态文本（systemd: 服务运行中/未运行；direct: 运行中 PID n / 未运行 / 未设置路径）；Enter → 刷新 |
| 3 | 启动 | Action | direct 模式显示按钮；Enter → `UiCommand::ProcAction(Start)`（即时执行 + 结果弹窗） |
| 4 | 停止 | Action | 同上 Stop |
| 5 | 重启 | Action | 同上 Restart；systemd 模式下 3-5 显示 `—`（由 systemctl 管理，禁用） |

- `field_values(&NetworkSettings)` 仍为纯函数（返回 28 字段；f[1]/f[2] 占位值，f[3..6] 由
  run_mode 决定启用/禁用）；`sync_from_settings`（持有 st）用 `st.run_status` 覆盖 f[1]/f[2] 显示值
- **dirty 判定只比较 f[0] ∪ f[6..28]**（路径/状态/按钮值随状态刷新变化，不得污染未保存标记）
- `apply_values`：f[0] → run_mode；f[6..28] → 原 22 个 config 字段（索引整体 +6）
- `FieldKind` 增加 `Action` 变体（widgets.rs）：渲染为 `[ 标签 ]` 按钮样式（聚焦反色）；
  不参与 FormPopup（FormPopup 不构造 Action 字段）
- 状态行 hint 更新（区块说明 + 无开机自启提示显示在状态区）

### app.rs

- `AppState` 新增 `run_status: Option<RunStatus>`
- `UiCommand` 新增：`RefreshStatus(RunMode)`、`ProcAction(ProcOp)`、`SaveMihomoBin(String)`
- `UiEvent` 新增：`RunStatusDone(Result<RunStatus, String>)`、`ProcActionDone(Result<ApplyOutcome, String>)`
- `InteractiveTask` 新增：`SaveMihomoBin(String)` → `installer::save_mihomo_bin`（退出 raw 模式、
  交互式 sudo、恢复、结果弹窗——复用现有 run_interactive 机制）
- `switch_page(5)`（进入设置页）→ 发 `RefreshStatus`（模式参照 idx==2 的 RefreshGroups）
- 保存模式切换（settings.rs save() 返回 `UiCommand::ProcAction(Stop)` 当且仅当旧模式 direct、
  新模式 systemd、且 run_status 显示进程实例运行中）→ 停止后 notice「已停止进程模式实例」+
  状态自动刷新
- 事件处理：RunStatusDone / ProcActionDone 更新 AppState + 结果弹窗/notice + 触发一次
  RefreshStatus（apply/操作后状态同步）；ApplyDone 成功后同样触发 RefreshStatus
- apply 分派：spawn 前取 `st.settings.run_mode`，`apply_config(yaml, true, mode)`；
  SudoNeedsPassword 交互重试路径同样携带 mode（InteractiveTask::Apply 捕获 mode）

### 迁移场景（已确认行为）

| 方向 | 行为 |
|---|---|
| systemd → direct | 无立即动作。用户点「启动」（有确认弹窗说明：若 systemd 服务运行中将被停止）→ mihomo-proc start 内 `ensure_service_stopped()`；Ctrl+A apply 同样由脚本处理 |
| direct → systemd | 保存模式切换时若进程实例运行中 → 自动 stop（notice 说明）；mihomo-apply 的进程守卫兜底竞态；systemd 服务未运行由状态行提示（README 说明 `sudo systemctl start mihomo`） |
| 未安装提权组件 | 运行方式区块显示「未安装提权组件，仪表盘按 i 安装」；进程操作报错 NotInSudoers（复用现有修复指引） |
| 旧安装缺 mihomo-proc | `is_proc_script_installed()` 检测，direct 模式操作前提示「缺少 mihomo-proc，请重新安装提权组件」 |

## 错误处理与用户反馈

- 所有 sudo/脚本错误复用现有分类（SudoNeedsPassword → 确认框交互重试；NotInSudoers → 修复指引；
  CommandFailed → stderr 原样弹出）
- 脚本错误消息统一 `ERROR: ...` 前缀透传到结果弹窗
- 启动失败：脚本 tail 日志尾部 5 行 + 退出码 1；apply 失败自动回滚旧配置并提示
- 路径保存失败（校验不过/tee 失败）：弹窗展示具体原因，settings.toml 不受影响

## 测试计划

单元测试（保持现有 291 全绿）：

1. `RunMode` 序列化往返：settings.toml `run_mode = "systemd"/"direct"`；旧文件缺字段默认 systemd
2. apply 分派：构造 stdin 首行命令断言（`apply`/`start`/`stop`/`restart`/`status` + yaml 拼接）；
   环境自适应测试沿用现有模式（bad yaml 在脚本内预校验阶段失败，无副作用）
3. status 行协议解析：`bin=`/`pid=`/`running=` 正常、空值、乱序、未知行忽略
4. 安装器：内嵌 mihomo-proc 脚本断言（setsid/PID 文件/白名单命令/校验逻辑关键行）、
   sudoers 两行、`validate_mihomo_bin`（不存在/不可执行/非法字符/正常）、
   `save_mihomo_bin` 的预校验分支（sudo 调用部分环境自适应跳过）
5. 设置页：SECTIONS 连续性（28）、field_values/apply_values 往返（含 run_mode、索引偏移）、
   dirty 排除规则（状态刷新不污染未保存标记）、Action 字段 Enter 分派、路径 FormPopup 流程
6. 现有测试索引偏移更新（port 3→9 等，逐处检查）

脚本级端到端（无需 root/sudo，环境变量覆盖路径，本机 mihomo 真实二进制）：

- `status`（无 conf → 空 bin；写 conf 后 → 正确输出）
- `start` → mihomo 存活、PID 文件正确、**ps 验证非调用方子进程且独立会话（setsid）**、
  重复 start 报"已在运行"；`stop` → 进程退出、PID 文件删除、误杀保护（伪造 PID 文件指向
  非 mihomo 进程 → 拒绝 kill）；`restart`
- `apply`：修改配置端口 → apply → 新进程用新配置（REST 探测 external-controller 或检查
  /proc cmdline 参数与日志）；坏配置 → 校验失败不替换；启动即崩配置 → 回滚旧配置并重启成功
- mihomo-apply 进程守卫：伪造运行中的进程实例 → 执行 apply → 实例被停 + systemctl restart 正常
- TUI 链路：`sudo -n mihomo-proc status`（NOPASSWD 已授权本机用户）真实调用；交互式 sudo 路径
  （保存路径）需用户密码——实施完成后请用户协助跑一次真实安装/路径保存验证

## 文档

README 更新：功能列表（设置页运行方式区块）、「权限方案」章节（两种模式 + 安全设计：无参脚本
不变式、路径 root 侧 conf + 交互式提权、setsid 守护化、PID 防误杀）、手动安装章节（mihomo-proc
+ sudoers 两行 + conf 文件）、已知取舍（进程模式无开机自启）。

## 已知取舍

- 进程模式无开机自启（用户确认，后续可按需加 systemd user unit 或 rc 脚本）
- 日志无轮转（/var/log/mihomo/mihomo.log，README 说明由 logrotate 可选管理）
- 路径不允许空白/控制字符（字符集白名单换取协议零转义；绝大多数安装路径无空格）
- 脚本间少量逻辑重复（mihomo-apply 与 mihomo-proc 的 PID 校验 ~10 行）——独立脚本原则优先，
  不引入共享文件

## 实施顺序建议

1. prep：`cargo fmt` 清理提交（任务提到待安排的 fmt 提交）
2. 数据模型 + 核心层（RunMode、apply.rs 分派、proc 操作/状态解析）+ 单测
3. 资源脚本 mihomo-proc.sh + mihomo-apply 守卫 + 安装器（sudoers 两行、save_mihomo_bin）+ 单测
4. 设置页（区块、Action 字段、路径弹窗、dirty 规则）+ 单测
5. app.rs（命令/事件/交互任务/状态刷新/模式切换自动 stop）+ 单测
6. 脚本级 E2E（env 覆盖路径）+ 全量回归（cargo test/clippy/fmt）
7. README 更新
8. 用户协助真实 sudo 路径验证；提交推送到 dev
