# Windows 平台支持设计（mihomo-tui）

**日期**：2026-08-14
**状态**：已与用户对齐（全部按推荐确认）

## 背景与目标

mihomo-tui 目前仅支持 Linux（systemd 默认 + 直接进程模式，direct 经 root 脚本 mihomo-proc 管理）。
目标：Windows 用户通过设置页提供 mihomo 可执行文件路径，TUI 直接启动/停止/重启 mihomo 进程。
Windows 无 systemd/sudoers 体系，**进程模式是唯一运行方式**。

## 平台性评估结论

**依赖**：全部跨平台（ratatui/crossterm/tokio/reqwest-rustls/serde 等），无 unix-only crate。
**需平台分支的模块**：
- `src/service/installer.rs`（664 行）— sudo/sudoers 体系 + PermissionsExt，Windows 整体禁用；`validate_mihomo_bin` 抽出共享并平台化
- `src/core/apply.rs` — sudo/pkexec/mihomo-proc/systemctl + `mode(0o600)`（unix-only API）；Windows 分支为直接写配置 + spawn/kill
- `src/core/settings.rs` — PermissionsExt 目录 0700；Windows 跳过
- `src/app.rs` — 首启安装引导、systemd 守护通知、SystemdAction、SaveMihomoBin 交互任务
- `src/ui/settings.rs` — 运行方式下拉含 systemd、路径校验规则
- `src/core/models.rs` — RunMode 默认值
**可直接复用**：订阅/合并/解析器/API 客户端/exit_ip/country + dashboard/groups/rules/logs/subscriptions/widgets 页面。

## 已确认决策

### Q1 配置目录
Windows：`%APPDATA%\mihomo-tui\`（`dirs` 语义手写：`APPDATA` env → fallback `USERPROFILE\AppData\Roaming`），存放全部文件：
`settings.toml` / `subscriptions.toml` / `overrides.toml` / `config.yaml` / `mihomo.pid` / `mihomo.log`。
mihomo 启动参数：`-d <dir> -f <dir>\config.yaml`。Linux 路径逻辑不变（`$HOME/.config/mihomo-tui`）。
路径构造抽纯函数（`windows_config_dir(appdata: &str)`）便于跨平台单测；`MIHOMO_TUI_SETTINGS_DIR` 覆盖保持优先。

### Q2 配置应用流程（Windows）
1. TUI 原子写 `config.yaml`（临时文件 + rename，沿用 settings.rs 模式）
2. 用**配置的 bin 路径**执行 `mihomo.exe -t -f config.yaml` 校验（Linux 维持 PATH 查找不变）
3. 校验失败 → 报错不重启；成功 → 停旧进程 → spawn 新进程（`-d` + `-f`，stdout/stderr 重定向 mihomo.log，`CREATE_NO_WINDOW` 不弹控制台，TUI 退出后进程继续运行——与 Linux setsid 语义一致）
4. 提供**启动/停止/重启**三按钮（PID 记录 mihomo.pid，kill 前校验进程可执行路径与配置一致，防误杀；残留 PID 文件自动清理）
5. mihomo 路径存 `settings.toml` 新字段 `mihomo_bin`（String，默认空；Linux 忽略该字段，仍走 root conf）

### Q3 TUN 管理员权限
UAC 提权是进程级属性，运行中无法弹窗提升；mihomo 子进程继承父进程 token。双时机提示（纯 API 检测 `OpenProcessToken` + `TokenElevation`，经 windows-sys crate，Windows 条件编译）：
1. **TUN 开关切换为开时**：非管理员 → 确认框「TUN 模式需要管理员权限。当前 TUI 未以管理员身份运行，mihomo 将无法创建 TUN 设备。是否仍要开启？（建议：关闭 TUN，或退出后右键"以管理员身份运行" TUI）」——确认后照常开启
2. **启动时守护**（类比 Linux spawn_startup_guard）：settings 中 TUN 已开启 + 非管理员 → 启动通知条提示一次，不阻塞
3. **wintun.dll 检查**：路径校验时检查 bin 同目录 `wintun.dll`，缺失则在提示中说明（mihomo 官方 release zip 自带）

### Q4 设置页（Windows）
- 运行方式下拉只显示「直接进程」一项（不可切换），持久化 `run_mode = "direct"`；systemd 相关 UI 全部隐藏
- 路径校验 Windows 规则：绝对路径（盘符 `X:\` 或 UNC `\\`）、存在性、`.exe`/可执行探测（无 unix 执行位概念）、`-v` 版本探测保留

### Q5 CI/发布
- `release.yml`：矩阵加 `x86_64-pc-windows-msvc`（windows-latest 原生构建，产物 zip 含 README+LICENSE+sha256）
- `ci.yml`：加 windows job（fmt+clippy+test）
- aarch64-pc-windows-msvc 本次不做

### Q6 平台抽象
不引入 trait 动态分派，用 `cfg(target_os)` 静态分支（本项目规模小，两平台行为差异大，静态分支更直观、无虚函数开销、编译器检查更严格）。新增 `src/service/process.rs`（Windows 进程管理模块）。

## 模块改动清单

| 文件 | 改动 |
|---|---|
| `Cargo.toml` | + windows-sys（target cfg，Win32_Foundation/Security/System.ProcessStatus/System.Threading） |
| `src/core/settings.rs` | config_dir 平台化；PermissionsExt cfg 门控；纯函数 `windows_config_dir` |
| `src/core/models.rs` | `NetworkSettings.mihomo_bin: String`（serde default）；`RunMode::default()` cfg（Windows→Direct） |
| `src/service/installer.rs` | 整体 `#[cfg(not(windows))]`；`validate_mihomo_bin` 移到共享位置并平台化 |
| `src/core/mihomo_bin.rs`（新） | 跨平台 bin 路径校验：Windows 语法校验纯函数 + `-v` 探测 + wintun.dll 检查 |
| `src/service/process.rs`（新） | Windows 进程管理：start/stop/restart/status、PID 文件、防误杀、CREATE_NO_WINDOW、日志重定向 |
| `src/core/apply.rs` | write_secret_file mode cfg 门控；`validate_config` 增加 bin 参数（Linux 传 None 走 PATH）；Windows apply 分派；systemd/proc 控制函数 cfg(not(windows)) |
| `src/app.rs` | 首启安装引导 cfg；spawn_startup_guard 平台化（Windows→TUN 提示）；SaveMihomoBin 平台化（Windows 写 settings.toml）；ProcAction 分派平台化 |
| `src/ui/settings.rs` | 运行方式区块 Windows 只显示 direct；TUN 切换检测；路径弹窗平台化 |
| `.github/workflows/ci.yml` | + windows job |
| `.github/workflows/release.yml` | + windows target |
| `README.md` | Windows 使用指南、平台差异说明 |

## 测试策略
- 纯函数跨平台单测：`windows_config_dir`（喂假 APPDATA 断言路径）、Windows 路径语法校验（盘符/UNC/非法字符，不依赖真实文件系统）
- `#[cfg(windows)]` 测试：真实 APPDATA 构造、bin 校验（CI windows job 跑）
- 现有 Linux 测试保持全绿（SETTINGS_DIR_LOCK / MIHOMO_TUI_SETTINGS_DIR 约定不变）
- Linux `cargo build/test` 全绿 + `cargo check --target x86_64-pc-windows-msvc`（若 target 可装）或依赖 CI windows job 验证

## 不做（YAGNI）
- aarch64-pc-windows-msvc 产物
- Windows 开机自启（服务注册）
- Linux 行为变更（PATH 校验、root conf、systemd 全部维持现状）
- 平台 trait 动态分派抽象
