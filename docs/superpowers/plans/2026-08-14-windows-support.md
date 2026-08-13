# Windows 平台支持实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 为 mihomo-tui 增加 Windows 支持：设置页提供 mihomo 可执行文件路径，TUI 直接启动/停止/重启进程；CI 增加 Windows 构建；发布产出 Windows 二进制；README 增加 Windows 指南。

**Architecture:** 不引入平台 trait 动态分派，全部用 `cfg(target_os)` / `cfg!(windows)` 静态分支（本项目规模小、两平台行为差异大）。新增 `core/mihomo_bin.rs`（共享路径校验）与 `service/process.rs`（Windows 进程管理，模块级 `#[cfg(windows)]` 门控）。Linux 行为 100% 不变。设计定稿见 `docs/superpowers/specs/2026-08-14-windows-support-design.md`（已提交 6e00e62）。

**Tech Stack:** Rust 2021, tokio::process, windows-sys 0.59（仅 Windows target）, GitHub Actions。

**验收环境事实**（worker 必须知道）：
- 本机已装 mihomo：`/usr/bin/mihomo`（apply.rs 部分测试依赖它，见 Task 4 门控）
- 测试串行锁约定：依赖 `MIHOMO_TUI_SETTINGS_DIR` 的测试必须用 `settings::with_settings_dir`（内含 `SETTINGS_DIR_LOCK`），不得直接 set_var
- 现有测试全绿基线：`cargo test`（本机）先跑一遍确认
- dev 分支 HEAD=6e00e62（含本 spec 提交），工作树干净

---

### Task 1: 平台基础（Cargo.toml / models.rs / settings.rs / core/mod.rs）

**Files:**
- Modify: `Cargo.toml`
- Modify: `src/core/models.rs`
- Modify: `src/core/settings.rs`
- Modify: `src/core/mod.rs`

- [ ] **Step 1: Cargo.toml 增加 windows-sys 依赖（仅 Windows target）**

```toml
# 文件末尾追加
[target.'cfg(windows)'.dependencies]
windows-sys = { version = "0.59", features = [
    "Win32_Foundation",
    "Win32_Security",
    "Win32_System_Threading",
] }
```

- [ ] **Step 2: models.rs — `NetworkSettings` 增加 `mihomo_bin` 字段（Windows 专用，Linux 恒空）**

在 `run_mode` 字段（`#[serde(default)] pub run_mode: RunMode,`）之后插入：

```rust
    /// mihomo 可执行文件路径（Windows 专用，存 settings.toml；
    /// Linux 走 root 侧 /etc/mihomo-tui/mihomo.conf，本字段恒为空）
    #[serde(default)]
    pub mihomo_bin: String,
```

- [ ] **Step 3: models.rs — `RunMode` 默认值平台化**

把 derive 上的 `Default` 去掉，`#[default]` 属性去掉，改为手写 impl（`#[default]` 不能平台分支）：

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RunMode {
    /// systemd 服务管理（Linux 默认）
    Systemd,
    /// 直接进程模式：TUI 管理进程（Windows 唯一运行方式）
    Direct,
}

impl Default for RunMode {
    fn default() -> Self {
        #[cfg(windows)]
        {
            RunMode::Direct
        }
        #[cfg(not(windows))]
        {
            RunMode::Systemd
        }
    }
}
```

- [ ] **Step 4: models.rs — `NetworkSettings::default()` 的 `run_mode` 与测试适配**

`default()` 中 `run_mode: RunMode::Systemd,` 改为 `run_mode: RunMode::default(),`。

测试 `run_mode_serde_roundtrip` 末行：

```rust
        assert_eq!(RunMode::default(), RunMode::Systemd);
```
改为：
```rust
        assert_eq!(
            RunMode::default(),
            if cfg!(windows) { RunMode::Direct } else { RunMode::Systemd }
        );
```

新增测试（追加到 models.rs tests mod）：

```rust
    /// Windows 字段：settings.toml 读写往返 + 缺省空串。
    #[test]
    fn mihomo_bin_field_serde() {
        let s = NetworkSettings { secret: "c".repeat(32), ..NetworkSettings::default() };
        assert_eq!(s.mihomo_bin, "");
        let body = toml::to_string(&s).unwrap();
        assert!(body.contains("mihomo_bin"));
        let back: NetworkSettings = toml::from_str(&body).unwrap();
        assert_eq!(back.mihomo_bin, "");
        // 旧文件无该字段也能反序列化（serde default）
        let old: NetworkSettings =
            toml::from_str(&body.replace("\nmihomo_bin = \"\"", "")).unwrap();
        assert_eq!(old.mihomo_bin, "");
    }
```

- [ ] **Step 5: settings.rs — config_dir 平台化**

文件头 `use std::os::unix::fs::PermissionsExt;` 删除（改为各使用点内联 cfg 引入）。

`config_dir()` 替换为：

```rust
/// 配置目录：Linux `$HOME/.config/mihomo-tui`（0700）；Windows `%APPDATA%\mihomo-tui`。
/// MIHOMO_TUI_SETTINGS_DIR 环境变量可覆盖（测试与 merge_sample 使用）。
pub fn config_dir() -> PathBuf {
    let dir = std::env::var("MIHOMO_TUI_SETTINGS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| platform_config_dir());
    let _ = fs::create_dir_all(&dir);
    // 隐私：配置文件含代理密码，目录收紧到仅本人可读（Windows 由用户目录 ACL 保护，跳过）
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o700));
    }
    dir
}

#[cfg(not(windows))]
fn platform_config_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".config").join("mihomo-tui")
}

#[cfg(windows)]
fn platform_config_dir() -> PathBuf {
    // %APPDATA% 缺失（服务场景）→ USERPROFILE\AppData\Roaming → 当前目录兜底
    std::env::var("APPDATA")
        .map(|a| windows_config_dir(&a))
        .unwrap_or_else(|_| {
            std::env::var("USERPROFILE")
                .map(|p| PathBuf::from(p).join("AppData").join("Roaming").join("mihomo-tui"))
                .unwrap_or_else(|_| PathBuf::from("mihomo-tui"))
        })
}

/// 纯函数：Windows 配置目录 = %APPDATA%\mihomo-tui（跨平台单测；Linux 亦编译但不用）。
pub fn windows_config_dir(appdata: &str) -> PathBuf {
    PathBuf::from(appdata).join("mihomo-tui")
}
```

- [ ] **Step 6: settings.rs — atomic_write 的 0600 权限 cfg 门控**

```rust
        let mut f = fs::File::create(&tmp)?;
        // 隐私：配置文件含代理密码，0600（rename 保留临时文件权限）；
        // Windows 由用户目录 ACL 保护，无需收紧
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            f.set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        f.write_all(body)?;
```

- [ ] **Step 7: settings.rs — 目录权限测试 cfg 门控**

找到使用 `.mode() & 0o777` 的测试（约 316 行），把该断言包进 `#[cfg(unix)]`（Windows 无 mode 概念）。形如：

```rust
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let dir_mode = fs::metadata(config_dir()).unwrap().permissions().mode() & 0o777;
            assert_eq!(dir_mode, 0o700, "配置目录应 0700");
        }
```

- [ ] **Step 8: settings.rs — 新增 windows_config_dir 纯函数测试（跨平台可跑）**

```rust
    /// Windows 配置目录构造（纯函数，Linux 上也能断言字符串行为）。
    #[test]
    fn windows_config_dir_joins_appdata() {
        let p = windows_config_dir(r"C:\Users\alice\AppData\Roaming");
        assert!(p.ends_with("mihomo-tui"));
        assert!(p.to_string_lossy().contains(r"C:\Users\alice\AppData\Roaming"));
    }
```

- [ ] **Step 9: core/mod.rs 注册新模块（Task 2 的 mihomo_bin 先占位）**

`pub mod mihomo_bin;` 按字母序插入（apply 之后、client 之前）。

- [ ] **Step 10: 验证 + 提交**

```bash
cargo build 2>&1 | tail -5
cargo test 2>&1 | tail -15
cargo fmt --all --check && cargo clippy --all-targets -- -D warnings
git add Cargo.toml Cargo.lock src/core/models.rs src/core/settings.rs src/core/mod.rs
git commit -m "feat: 平台基础——windows-sys 依赖、mihomo_bin 字段、RunMode 默认值、config_dir 平台化"
```

期望：全绿（mihomo_bin 字段已加但尚无使用者，cargo 不报未用字段警告）。

---

### Task 2: core/mihomo_bin.rs 共享路径校验 + installer.rs 平台门控

**Files:**
- Create: `src/core/mihomo_bin.rs`
- Modify: `src/core/mod.rs`（Task 1 已注册，若未做则补）
- Modify: `src/service/installer.rs`
- Modify: `src/ui/settings.rs`（仅 import 行）

- [ ] **Step 1: 创建 `src/core/mihomo_bin.rs`（完整内容）**

```rust
//! mihomo 二进制路径校验（跨平台）：
//! Linux —— 字符集白名单 + 可执行位（防脚本注入）；Windows —— 盘符/UNC 绝对路径 + 字符集。
//! 两平台共同的硬性门：`<path> -v` 版本探测输出须含 "mihomo"。

use std::path::Path;
use std::process::Command;

/// 绝对路径判定（平台规则）。
/// Linux：以 `/` 开头；Windows：盘符 `X:\`/`X:/` 或 UNC `\\`。
pub fn is_absolute_path(p: &str) -> bool {
    #[cfg(windows)]
    {
        p.starts_with("\\\\")
            || (p.len() >= 3
                && p.as_bytes()[0].is_ascii_alphabetic()
                && p.as_bytes()[1] == b':'
                && matches!(p.as_bytes()[2], b'\\' | b'/'))
    }
    #[cfg(not(windows))]
    {
        p.starts_with('/')
    }
}

/// Windows 路径语法校验（纯函数，跨平台可测）：绝对路径 + 无控制字符与
/// `" < > | ? *`（Windows 文件名非法字符；`:` 与 `\`/`/` 为盘符/分隔符，允许）。
/// 注意：不检查文件存在性（由调用方在探测前决定）。
pub fn windows_path_syntax_ok(path: &str) -> Result<(), String> {
    if !is_absolute_path(path) {
        return Err("路径必须为绝对路径（如 C:\\mihomo\\mihomo.exe 或 \\\\server\\share\\mihomo.exe）"
            .to_string());
    }
    if !path.chars().all(|c| !c.is_control() && !matches!(c, '"' | '<' | '>' | '|' | '?' | '*')) {
        return Err("路径含不允许的字符（控制字符与 \" < > | ? * 不允许）".to_string());
    }
    Ok(())
}

/// 校验 mihomo 二进制路径：绝对路径 + 字符集 + 存在 +（Linux 可执行位）+ `-v` 版本探测。
pub fn validate_mihomo_bin(path: &str) -> Result<(), String> {
    if path.is_empty() {
        return Err("路径不能为空".to_string());
    }
    #[cfg(windows)]
    {
        windows_path_syntax_ok(path)?;
    }
    #[cfg(not(windows))]
    {
        if !is_absolute_path(path) {
            return Err("路径必须为绝对路径（以 / 开头）".to_string());
        }
        if !path.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '+' | '-' | '/'))
        {
            return Err("路径含不允许的字符（仅允许字母数字与 _ . + - /）".to_string());
        }
    }
    if !Path::new(path).exists() {
        return Err(format!("文件不存在: {path}"));
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = Path::new(path)
            .metadata()
            .map_err(|e| format!("无法读取文件信息: {e}"))?
            .permissions()
            .mode();
        if mode & 0o111 == 0 {
            return Err(format!("文件不可执行: {path}"));
        }
    }
    // 版本探测：`<path> -v` 输出应含 mihomo 字样（两平台通用硬性门）
    let out = Command::new(path)
        .arg("-v")
        .output()
        .map_err(|e| format!("执行 {path} -v 失败: {e}"))?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    if !text.to_lowercase().contains("mihomo") {
        return Err(format!(
            "{path} -v 输出不含 mihomo 字样，可能不是 mihomo 二进制:\n{text}"
        ));
    }
    Ok(())
}

/// Windows：bin 同目录 `wintun.dll` 是否缺失（TUN 模式需要；mihomo 官方 release zip 自带）。
#[cfg(windows)]
pub fn wintun_dll_missing(bin: &str) -> bool {
    let dir = Path::new(bin).parent().unwrap_or_else(|| Path::new("."));
    !dir.join("wintun.dll").exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 跨平台纯函数：Windows 语法规则（Linux CI 上同样执行）。
    #[test]
    fn windows_path_syntax_rules() {
        assert!(windows_path_syntax_ok(r"C:\mihomo\mihomo.exe").is_ok());
        assert!(windows_path_syntax_ok(r"C:/mihomo/mihomo.exe").is_ok());
        assert!(windows_path_syntax_ok(r"\\server\share\mihomo.exe").is_ok());
        // 空格允许（Windows 常见路径；无 shell 注入面）
        assert!(windows_path_syntax_ok(r"C:\Program Files\mihomo\mihomo.exe").is_ok());
        assert!(windows_path_syntax_ok("mihomo.exe").is_err(), "相对路径应拒绝");
        assert!(windows_path_syntax_ok(r"C:mihomo.exe").is_err(), "盘符相对路径应拒绝");
        assert!(windows_path_syntax_ok("C:\\mihomo\\mihomo.exe\"").is_err(), "引号应拒绝");
        assert!(windows_path_syntax_ok("C:\\mihomo\\mi*homo.exe").is_err(), "星号应拒绝");
        assert!(windows_path_syntax_ok("C:\\mihomo\\mihomo.exe\n").is_err(), "控制字符应拒绝");
    }

    /// 绝对路径判定（平台各自的规则）。
    #[test]
    fn is_absolute_path_rules() {
        #[cfg(not(windows))]
        {
            assert!(is_absolute_path("/usr/bin/mihomo"));
            assert!(!is_absolute_path("usr/bin/mihomo"));
        }
        #[cfg(windows)]
        {
            assert!(is_absolute_path(r"C:\mihomo\mihomo.exe"));
            assert!(is_absolute_path(r"\\server\share\mihomo.exe"));
            assert!(!is_absolute_path("C:mihomo.exe"));
            assert!(!is_absolute_path("mihomo.exe"));
        }
    }

    /// 不触碰文件系统的校验失败分支（两平台通用）。
    #[test]
    fn validate_rejects_without_fs() {
        assert!(validate_mihomo_bin("").is_err(), "空路径应拒绝");
        assert!(validate_mihomo_bin("usr/bin/mihomo").is_err(), "相对路径应拒绝");
    }

    /// 依赖真实文件的 Linux 分支（/usr/bin/mihomo 验收环境已装）。
    #[cfg(not(windows))]
    mod linux_fs_cases {
        use super::*;

        #[test]
        fn validate_charset_and_fs_rules() {
            assert!(validate_mihomo_bin("/usr/bin/mihomo; rm -rf /").is_err(), "分号应拒绝");
            assert!(validate_mihomo_bin("/tmp/a b").is_err(), "空白应拒绝");
            assert!(validate_mihomo_bin("/nonexistent/mihomo").is_err());
            // 存在但不可执行（无 x 位）
            assert!(validate_mihomo_bin("/etc/hostname").is_err());
        }

        #[test]
        fn validate_real_mihomo_ok() {
            assert!(validate_mihomo_bin("/usr/bin/mihomo").is_ok());
        }
    }
}
```

注意：`validate_rejects_without_fs` 中 `validate_mihomo_bin("usr/bin/mihomo")` 在 Windows 上走 `windows_path_syntax_ok` → 非绝对 → Err ✓；Linux 走 `/` 检查 → Err ✓。两平台语义一致。

- [ ] **Step 2: installer.rs — 移除旧 validate_mihomo_bin 与相关测试，整体加平台门控**

1. 文件头部（`//!` 文档注释之后、`use` 之前）加：

```rust
#![cfg(not(windows))]
```

2. 删除 `pub fn validate_mihomo_bin(...)` 整个函数（约 364-407 行）——已移到 core/mihomo_bin.rs。
3. 删除 tests mod 中 `validate_mihomo_bin_cases` 测试（约 613-646 行，含其内部引用的 `validate_mihomo_bin` 与 `probe` 辅助）。
4. 文件内 `use std::os::unix::fs::PermissionsExt;`（15 行）与 380 行的内联 use 保留——模块整体 Linux-only，无需再 cfg。
5. 其余（sudoers_lines/install/needs_install/save_mihomo_bin 等）一律不动。

- [ ] **Step 3: ui/settings.rs — import 改指向共享模块**

```rust
use crate::service::installer::validate_mihomo_bin;
```
改为：
```rust
use crate::core::mihomo_bin::validate_mihomo_bin;
```

- [ ] **Step 4: 验证 + 提交**

```bash
cargo build 2>&1 | tail -3
cargo test 2>&1 | tail -8
cargo fmt --all --check && cargo clippy --all-targets -- -D warnings
git add src/core/mihomo_bin.rs src/core/mod.rs src/service/installer.rs src/ui/settings.rs
git commit -m "feat: mihomo 路径校验平台化（Windows 盘符/UNC 规则）+ installer 整体 Linux 门控"
```

期望：全绿；`cargo test mihomo_bin` 通过新测试。

---

### Task 3: service/process.rs — Windows 进程管理模块

**Files:**
- Create: `src/service/process.rs`
- Modify: `src/service/mod.rs`
- Modify: `src/core/apply.rs`（仅加 `BinNotConfigured` 错误变体）

前置：Task 1（models/settings）+ Task 2（mihomo_bin）已合入。

- [ ] **Step 1: apply.rs — 追加错误变体（放 `NotInSudoers` 附近）**

```rust
    #[error(
        "未设置 mihomo 路径：请先在设置页 Enter mihomo-bin 设置 mihomo 可执行文件路径"
    )]
    BinNotConfigured,
```

- [ ] **Step 2: service/mod.rs 注册（Windows 专属模块）**

```rust
// Worker C: 首装安装器（Linux）
pub mod installer;
// Windows 直接进程管理（无 systemd/sudo 体系，进程模式为唯一运行方式）
#[cfg(windows)]
pub mod process;
```

- [ ] **Step 3: 创建 `src/service/process.rs`（完整内容）**

```rust
//! Windows 直接进程管理：TUI 自行 spawn/kill mihomo 进程。
//! 无 systemd/sudoers 体系，进程模式是唯一运行方式（设计见
//! docs/superpowers/specs/2026-08-14-windows-support-design.md §Q2）。
//! 仅 Windows 编译（service/mod.rs 门控）；Linux 走提权脚本体系。

use std::ffi::c_void;
use std::path::PathBuf;
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::process::{Command, Stdio};

use crate::core::apply::{ApplyError, ApplyOutcome, ProcOp, ProcStatus};
use crate::core::mihomo_bin::validate_mihomo_bin;
use crate::core::settings::{config_dir, load_settings, save_settings};

/// config.yaml 文件名（mihomo 以 `-d <dir> -f <dir>\config.yaml` 启动）。
pub const CONFIG_FILE: &str = "config.yaml";
/// PID 文件（记录 TUI 启动的 mihomo PID；TUI 退出后进程继续，stop/status 靠它定位）。
pub const PID_FILE: &str = "mihomo.pid";
/// mihomo stdout/stderr 重定向日志。
pub const LOG_FILE: &str = "mihomo.log";

/// 创建进程时不弹控制台窗口（TUI 是终端程序，子进程不应抢占窗口）。
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub fn config_path() -> PathBuf {
    config_dir().join(CONFIG_FILE)
}

fn pid_path() -> PathBuf {
    config_dir().join(PID_FILE)
}

fn log_path() -> PathBuf {
    config_dir().join(LOG_FILE)
}

/// 读取 settings.toml 中的 mihomo 路径；未设置 → BinNotConfigured（设置页有引导文案）。
fn configured_bin() -> Result<String, ApplyError> {
    let s = load_settings().map_err(|e| ApplyError::Io(e.to_string()))?;
    if s.mihomo_bin.is_empty() {
        return Err(ApplyError::BinNotConfigured);
    }
    Ok(s.mihomo_bin)
}

/// 路径比较：大小写不敏感、`/` 与 `\` 等价、忽略尾部 `/`（Windows 语义）。
fn paths_equal(a: &str, b: &str) -> bool {
    let norm = |s: &str| s.replace('\\', "/").trim_end_matches('/').to_lowercase();
    norm(a) == norm(b)
}

/// 进程是否存在且可执行镜像路径与配置 bin 一致（防 PID 复用误杀/误判）。
fn process_matches(pid: u32, bin: &str) -> bool {
    unsafe {
        let handle = windows_sys::Win32::System::Threading::OpenProcess(
            windows_sys::Win32::System::Threading::PROCESS_QUERY_LIMITED_INFORMATION,
            0,
            pid,
        );
        if handle.is_null() {
            return false;
        }
        let mut buf = [0u16; 1024];
        let mut size = buf.len() as u32;
        let ok = windows_sys::Win32::System::Threading::QueryFullProcessImageNameW(
            handle,
            0,
            buf.as_mut_ptr(),
            &mut size,
        );
        windows_sys::Win32::Foundation::CloseHandle(handle);
        if ok == 0 {
            return false;
        }
        let img = String::from_utf16_lossy(&buf[..size as usize]);
        paths_equal(&img, bin)
    }
}

fn kill_pid(pid: u32) -> std::io::Result<()> {
    unsafe {
        let handle = windows_sys::Win32::System::Threading::OpenProcess(
            windows_sys::Win32::System::Threading::PROCESS_TERMINATE,
            0,
            pid,
        );
        if handle.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        let ok = windows_sys::Win32::System::Threading::TerminateProcess(handle, 1);
        windows_sys::Win32::Foundation::CloseHandle(handle);
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

/// 当前进程是否 UAC 提权（管理员）。TUN 模式需要管理员权限；
/// 提权是进程级属性，运行中无法提升——只能提示用户以管理员身份重启 TUI。
pub fn is_elevated() -> bool {
    unsafe {
        let mut token = std::ptr::null_mut();
        let ok = windows_sys::Win32::System::Threading::OpenProcessToken(
            windows_sys::Win32::System::Threading::GetCurrentProcess(),
            windows_sys::Win32::Security::TOKEN_QUERY,
            &mut token,
        );
        if ok == 0 {
            return false;
        }
        let mut elev: windows_sys::Win32::Security::TOKEN_ELEVATION = std::mem::zeroed();
        let mut len: u32 = 0;
        let ok = windows_sys::Win32::Security::GetTokenInformation(
            token,
            windows_sys::Win32::Security::TokenElevation,
            &mut elev as *mut _ as *mut c_void,
            std::mem::size_of::<windows_sys::Win32::Security::TOKEN_ELEVATION>() as u32,
            &mut len,
        );
        windows_sys::Win32::Foundation::CloseHandle(token);
        ok != 0 && elev.TokenIsElevated != 0
    }
}

async fn read_pid() -> Option<u32> {
    let s = tokio::fs::read_to_string(pid_path()).await.ok()?;
    s.trim().parse().ok()
}

async fn write_pid(pid: u32) -> Result<(), ApplyError> {
    tokio::fs::write(pid_path(), format!("{pid}\n"))
        .await
        .map_err(|e| ApplyError::Io(format!("写入 PID 文件失败: {e}")))
}

/// 启动 mihomo：`-d <dir> -f <dir>\config.yaml`，stdout/stderr → mihomo.log，
/// CREATE_NO_WINDOW 不弹控制台；TUI 退出后进程继续运行（与 Linux setsid 语义一致）。
pub async fn start() -> Result<ApplyOutcome, ApplyError> {
    let bin = configured_bin()?;
    // 幂等：已在运行（PID 匹配配置 bin）→ 直接返回
    if let Some(pid) = read_pid().await {
        if process_matches(pid, &bin) {
            return Ok(ApplyOutcome {
                success: true,
                stdout: format!("mihomo 已在运行（PID {pid}）"),
                stderr: String::new(),
            });
        }
        // 残留 PID 文件（进程已死）：清理
        let _ = tokio::fs::remove_file(pid_path()).await;
    }
    let dir = config_dir();
    let cfg = config_path();
    let log = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path())
        .await
        .map_err(|e| ApplyError::Io(format!("打开日志文件 {LOG_FILE} 失败: {e}")))?;
    let log_out = log
        .try_clone()
        .map_err(|e| ApplyError::Io(format!("复制日志句柄失败: {e}")))?;
    let mut cmd = Command::new(&bin);
    cmd.args([
        "-d",
        dir.to_str().unwrap_or_default(),
        "-f",
        cfg.to_str().unwrap_or_default(),
    ])
    .stdin(Stdio::null())
    .stdout(Stdio::from(log_out))
    .stderr(Stdio::from(log))
    .creation_flags(CREATE_NO_WINDOW);
    let child = cmd
        .spawn()
        .map_err(|e| ApplyError::Io(format!("启动 mihomo 失败: {e}")))?;
    let pid = child.id().unwrap_or(0);
    write_pid(pid).await?;
    // 短等待：启动即崩（配置错误/端口占用）尽早暴露，避免用户以为成功了
    tokio::time::sleep(Duration::from_millis(500)).await;
    if !process_matches(pid, &bin) {
        let _ = tokio::fs::remove_file(pid_path()).await;
        return Err(ApplyError::Io(
            "mihomo 启动后立即退出（请查看 mihomo.log：配置错误或端口被占用）".to_string(),
        ));
    }
    Ok(ApplyOutcome {
        success: true,
        stdout: format!("mihomo 已启动（PID {pid}，日志 {LOG_FILE}）"),
        stderr: String::new(),
    })
}

/// 停止 mihomo（按 PID 文件定位；kill 前校验镜像路径防误杀；残留 PID 自动清理）。
pub async fn stop() -> Result<ApplyOutcome, ApplyError> {
    let bin = configured_bin()?;
    let Some(pid) = read_pid().await else {
        return Ok(ApplyOutcome {
            success: true,
            stdout: "mihomo 未在运行".to_string(),
            stderr: String::new(),
        });
    };
    if !process_matches(pid, &bin) {
        let _ = tokio::fs::remove_file(pid_path()).await;
        return Ok(ApplyOutcome {
            success: true,
            stdout: "mihomo 未在运行（残留 PID 文件已清理）".to_string(),
            stderr: String::new(),
        });
    }
    kill_pid(pid).map_err(|e| ApplyError::Io(format!("终止进程失败: {e}")))?;
    // 等待退出（最多 3s）
    for _ in 0..30 {
        if !process_matches(pid, &bin) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let _ = tokio::fs::remove_file(pid_path()).await;
    Ok(ApplyOutcome {
        success: true,
        stdout: format!("mihomo 已停止（PID {pid}）"),
        stderr: String::new(),
    })
}

/// 查询进程状态（设置页状态行）。未配置路径 → bin=None（设置页显示"未设置路径"）。
pub async fn status() -> Result<ProcStatus, ApplyError> {
    let s = load_settings().map_err(|e| ApplyError::Io(e.to_string()))?;
    if s.mihomo_bin.is_empty() {
        return Ok(ProcStatus {
            bin: None,
            pid: None,
            running: false,
        });
    }
    let pid = read_pid().await;
    let running = pid
        .map(|p| process_matches(p, &s.mihomo_bin))
        .unwrap_or(false);
    Ok(ProcStatus {
        bin: Some(s.mihomo_bin),
        pid: if running { pid } else { None },
        running,
    })
}

/// 应用配置：用配置 bin 校验 → 原子写 config.yaml → 重启进程。
/// 校验失败不触碰运行中的 mihomo（与 Linux 链路语义一致）。
pub async fn apply(yaml: &str) -> Result<ApplyOutcome, ApplyError> {
    let bin = configured_bin()?;
    crate::core::apply::validate_config(yaml, Some(&bin)).await?;
    let dir = config_dir();
    let tmp = dir.join(".config.yaml.tmp");
    {
        let mut f = tokio::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)
            .await
            .map_err(|e| ApplyError::Io(format!("写入临时配置失败: {e}")))?;
        f.write_all(yaml.as_bytes())
            .await
            .map_err(|e| ApplyError::Io(format!("写入临时配置失败: {e}")))?;
        f.sync_all()
            .await
            .map_err(|e| ApplyError::Io(format!("写入临时配置失败: {e}")))?;
    }
    tokio::fs::rename(&tmp, config_path())
        .await
        .map_err(|e| ApplyError::Io(format!("写入 config.yaml 失败: {e}")))?;
    // 重启：运行中则先停（防双实例/端口抢占），再启动
    let running = read_pid()
        .await
        .map(|p| process_matches(p, &bin))
        .unwrap_or(false);
    if running {
        stop().await?;
    }
    start().await
}

/// 启/停/重启统一入口（app.rs ProcAction 经 apply::proc_control 分派到这里）。
pub async fn control(op: ProcOp) -> Result<ApplyOutcome, ApplyError> {
    match op {
        ProcOp::Start => start().await,
        ProcOp::Stop => stop().await,
        ProcOp::Restart => {
            stop().await?;
            start().await
        }
    }
}

/// 保存 mihomo 路径（Windows 无 root conf：直接写 settings.toml）。
/// 校验含 `-v` 探测；返回操作日志行（含 wintun.dll 缺失提醒）。
pub async fn save_bin(path: &str) -> Result<Vec<String>, String> {
    validate_mihomo_bin(path)?;
    let mut s = load_settings().map_err(|e| e.to_string())?;
    s.mihomo_bin = path.to_string();
    save_settings(&s).map_err(|e| e.to_string())?;
    let mut lines = vec![format!("✓ 已保存 mihomo 路径 {path}（settings.toml）")];
    if crate::core::mihomo_bin::wintun_dll_missing(path) {
        lines.push(
            "注意：未找到 wintun.dll（TUN 模式需要，mihomo 官方 release zip 自带，\
             请将 mihomo.exe 与 wintun.dll 放同一目录）"
                .to_string(),
        );
    }
    Ok(lines)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::settings::with_settings_dir;

    /// 路径比较归一化（纯函数，无进程依赖）。
    #[test]
    fn paths_equal_normalizes() {
        assert!(paths_equal(r"C:\mihomo\mihomo.exe", r"c:/mihomo/mihomo.exe"));
        assert!(paths_equal(r"C:\mihomo\mihomo.exe", r"C:\mihomo\Mihomo.EXE"));
        assert!(paths_equal(r"C:\mihomo\", r"c:/mihomo"));
        assert!(!paths_equal(r"C:\mihomo\mihomo.exe", r"D:\mihomo\mihomo.exe"));
        assert!(!paths_equal(r"C:\mihomo\mihomo.exe", r"C:\mihomo\other.exe"));
    }

    /// PID 文件读写往返（async，包 Runtime）。
    #[test]
    fn pid_file_roundtrip() {
        with_settings_dir(|| {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                assert_eq!(read_pid().await, None);
                write_pid(4242).await.unwrap();
                assert_eq!(read_pid().await, Some(4242));
            });
        });
    }
}
```

- [ ] **Step 4: 验证 + 提交**

```bash
cargo build 2>&1 | tail -3
cargo test 2>&1 | tail -6
cargo fmt --all --check && cargo clippy --all-targets -- -D warnings
git add src/service/process.rs src/service/mod.rs src/core/apply.rs
git commit -m "feat: Windows 直接进程管理模块（spawn/kill/PID 防误杀/CREATE_NO_WINDOW/管理员检测）"
```

期望：Linux 全绿（process.rs 在 Linux 不编译）。若本机 rustup 可用，可先验证 Windows 编译面：

```bash
rustup target add x86_64-pc-windows-msvc 2>/dev/null
cargo check --target x86_64-pc-windows-msvc 2>&1 | tail -15
```

期望：0 error（windows-sys API 路径若有出入，按编译错误修正——注意 `TOKEN_QUERY` 在 `Win32::Security`，`QueryFullProcessImageNameW/OpenProcessToken` 在 `Win32::System::Threading`）。

---

### Task 4: core/apply.rs 平台化（Windows 分派 + Linux 门控 + 测试适配）

**Files:**
- Modify: `src/core/apply.rs`

前置：Task 3 已合入（process.rs 存在）。

- [ ] **Step 1: write_secret_file — 0600 mode cfg 门控**

```rust
async fn write_secret_file(path: &std::path::Path, body: &str) -> std::io::Result<()> {
    let mut opts = tokio::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    opts.mode(0o600);
    let mut f = opts.open(path).await?;
    f.write_all(body.as_bytes()).await?;
    f.sync_all().await?;
    Ok(())
}
```

- [ ] **Step 2: validate_config — 增加 bin 参数（Windows 用配置路径；Linux 传 None 走 PATH）**

```rust
/// 写临时文件 → `mihomo -t -f` 校验；失败返回 mihomo 原始 stderr。
/// bin=None 时走 PATH 查找 `mihomo`（Linux 现状）；Some(bin) 用配置的二进制路径（Windows）。
pub async fn validate_config(yaml: &str, bin: Option<&str>) -> Result<(), ApplyError> {
    let path = tmp_path(VALIDATE_TMP);
    write_secret_file(&path, yaml)
        .await
        .map_err(|e| ApplyError::Io(e.to_string()))?;
    let probe = bin.unwrap_or("mihomo");
    let result = run_capture(probe, &["-t", "-f", path.to_str().unwrap()], None).await;
    let _ = tokio::fs::remove_file(&path).await;
    match result {
        Ok((status, stdout, stderr)) => {
            if status.success() {
                Ok(())
            } else {
                Err(ApplyError::ValidateFailed {
                    stderr: if stderr.trim().is_empty() {
                        stdout
                    } else {
                        stderr
                    },
                })
            }
        }
        Err(e) => Err(e),
    }
}
```

- [ ] **Step 3: apply_config — Windows 分支分派到 process::apply**

```rust
pub async fn apply_config(
    yaml: &str,
    non_interactive: bool,
    mode: RunMode,
) -> Result<ApplyOutcome, ApplyError> {
    #[cfg(windows)]
    {
        let _ = (non_interactive, mode);
        crate::service::process::apply(yaml).await
    }
    #[cfg(not(windows))]
    {
        let (script, body) = match mode {
            RunMode::Systemd => ("/usr/local/sbin/mihomo-apply", yaml.to_string()),
            RunMode::Direct => ("/usr/local/sbin/mihomo-proc", direct_apply_body(yaml)),
        };
        run_apply_script(script, &body, non_interactive, APPLY_TMP).await
    }
}
```

- [ ] **Step 4: proc_control / proc_status — Windows 分派**

```rust
pub async fn proc_control(op: ProcOp) -> Result<ApplyOutcome, ApplyError> {
    #[cfg(windows)]
    {
        return crate::service::process::control(op).await;
    }
    #[cfg(not(windows))]
    {
        if !is_proc_script_installed().await {
            return Err(ApplyError::ProcScriptMissing);
        }
        run_apply_script(
            "/usr/local/sbin/mihomo-proc",
            &format!("{}\n", op.stdin_line()),
            true,
            PROC_TMP,
        )
        .await
    }
}

pub async fn proc_status() -> Result<ProcStatus, ApplyError> {
    #[cfg(windows)]
    {
        return crate::service::process::status().await;
    }
    #[cfg(not(windows))]
    {
        if !is_proc_script_installed().await {
            return Err(ApplyError::ProcScriptMissing);
        }
        let out = run_apply_script("/usr/local/sbin/mihomo-proc", "status\n", true, STATUS_TMP)
            .await?;
        Ok(parse_proc_status(&out.stdout))
    }
}
```

- [ ] **Step 5: Linux 专属项整体 cfg(not(windows))**

给以下每一项加 `#[cfg(not(windows))]`（函数整体）：
- `direct_apply_body`
- `run_apply_script`
- `SudoFailureKind` 枚举 + `classify_sudo_failure`
- `classify_systemctl_failure`
- `script_installed_at` + `is_proc_script_installed` + `is_apply_script_installed` + `is_proc_script_installed_sync`
- `systemctl_control` + `service_is_active` + `service_unit_exists`

`find_mihomo_in_path` / `mihomo_is_installed` **不加**门控（Windows 上 `which` 不存在自然返回 None，pub 项无 dead-code 警告）。`parse_proc_status` 不加门控（纯函数，测试跨平台）。

检查 `#[cfg(not(windows))]` 后无孤儿引用：`run_capture`、`write_secret_file`、`tmp_path`、`validate_config` 为共享项（保持无条件）。

- [ ] **Step 6: 测试适配**

1. `validate_ok_minimal_config`、`validate_bad_yaml_fails_with_stderr`、`apply_non_interactive_password_or_validation_failure` 三个测试：加 `#[cfg(not(windows))]`（依赖真实 mihomo/sudo 环境，windows-latest CI 没有），并把两处 `validate_config(...)` 调用加第二个参数 `None`。
2. `classify_sudo_failure_rules` 测试：加 `#[cfg(not(windows))]`（被测函数已门控）。
3. 检查 tests mod 中是否还有其他引用门控项（如 `systemctl_control`/`is_proc_script_installed_sync`）的测试，同样加 `#[cfg(not(windows))]`。

- [ ] **Step 7: 验证 + 提交**

```bash
cargo build 2>&1 | tail -3
cargo test 2>&1 | tail -8
cargo fmt --all --check && cargo clippy --all-targets -- -D warnings
cargo check --target x86_64-pc-windows-msvc 2>&1 | tail -15   # target 已装则跑
git add src/core/apply.rs
git commit -m "feat: apply 链路平台化——Windows 直接写配置+校验+重启，Linux 门控不变"
```

期望：Linux 全绿；Windows check 0 error（若本机无 target 则跳过，CI 兜底）。

---

### Task 5: app.rs 平台化

**Files:**
- Modify: `src/app.rs`

前置：Task 3+4 已合入。

- [ ] **Step 1: import 拆分**

```rust
use crate::core::apply::{
    apply_config, proc_control, proc_status, validate_config, ApplyOutcome, ProcOp, RunStatus,
};
#[cfg(not(windows))]
use crate::core::apply::{service_is_active, service_unit_exists, systemctl_control};
```

- [ ] **Step 2: ApplyConfig 命令 — 传 bin 参数**

```rust
            UiCommand::ApplyConfig(yaml) => {
                let sudo_tx = self.sudo_tx.clone();
                let mode = self.state.settings.run_mode;
                let bin = self.state.settings.mihomo_bin.clone();
                let bin_opt = (!bin.is_empty()).then_some(bin);
                tokio::spawn(async move {
                    // 先 mihomo -t 校验，再非交互 sudo
                    match validate_config(&yaml, bin_opt.as_deref()).await {
```

- [ ] **Step 3: RefreshStatus 命令 — 平台分支**

```rust
            UiCommand::RefreshStatus => {
                let ui_tx = self.ui_tx.clone();
                tokio::spawn(async move {
                    #[cfg(not(windows))]
                    let (unit, active) = (service_unit_exists().await, service_is_active().await);
                    // 进程实例查询失败（未装脚本/未授权/超时）静默置 None，设置页显示"查询失败"
                    let proc = tokio::time::timeout(PROC_STATUS_TIMEOUT, proc_status())
                        .await
                        .ok()
                        .and_then(|r| r.ok());
                    #[cfg(not(windows))]
                    let _ = ui_tx.send(UiEvent::RunStatusDone(Ok(RunStatus {
                        service_unit: Some(unit),
                        service_active: Some(active),
                        proc,
                    })));
                    #[cfg(windows)]
                    let _ = ui_tx.send(UiEvent::RunStatusDone(Ok(RunStatus {
                        service_unit: None,
                        service_active: None,
                        proc,
                    })));
                });
            }
```

- [ ] **Step 4: SystemdAction / InstallSetup 命令 — 空实现 arm**

```rust
            #[cfg(not(windows))]
            UiCommand::SystemdAction(op) => { /* 原实现整体保留 */ }

            #[cfg(windows)]
            UiCommand::SystemdAction(_) => {}
```
同理 InstallSetup：
```rust
            #[cfg(not(windows))]
            UiCommand::InstallSetup => { /* 原实现整体保留 */ }

            #[cfg(windows)]
            UiCommand::InstallSetup => {}
```

- [ ] **Step 5: SaveMihomoBin 命令 — 确认文案平台化**

```rust
            UiCommand::SaveMihomoBin(path) => {
                let confirm_text = {
                    #[cfg(windows)]
                    {
                        format!("将 mihomo 路径保存为 {path}（settings.toml，无需提权）。是否继续？")
                    }
                    #[cfg(not(windows))]
                    {
                        format!("需要 root 权限写入 {path} 到系统配置。是否继续？")
                    }
                };
                self.pending_confirm = Some((
                    ConfirmPopup::new("保存 mihomo 路径".into(), confirm_text),
                    InteractiveTask::SaveMihomoBin(path),
                ));
            }
```

- [ ] **Step 6: run_interactive — Install 门控 + SaveMihomoBin 平台实现**

```rust
            #[cfg(not(windows))]
            InteractiveTask::Install => crate::service::installer::install()
                .await
                .map(|lines| ApplyOutcome {
                    success: true,
                    stdout: lines.join("\n"),
                    stderr: String::new(),
                })
                .map_err(|e| e.to_string()),
            InteractiveTask::SaveMihomoBin(path) => {
                #[cfg(windows)]
                {
                    crate::service::process::save_bin(&path)
                        .await
                        .map(|lines| ApplyOutcome {
                            success: true,
                            stdout: lines.join("\n"),
                            stderr: String::new(),
                        })
                        .map_err(|e| e.to_string())
                }
                #[cfg(not(windows))]
                {
                    crate::service::installer::save_mihomo_bin(&path)
                        .await
                        .map(|lines| ApplyOutcome {
                            success: true,
                            stdout: lines.join("\n"),
                            stderr: String::new(),
                        })
                        .map_err(|e| e.to_string())
                }
            }
```

注意：`run_interactive` 的 `InteractiveTask::Apply` arm 不动（`apply_config` 内部已平台分派）。

- [ ] **Step 7: spawn_startup_guard — 双 cfg 变体**

```rust
    /// 启动引导：Windows —— TUN 已开启但非管理员 → 通知一次（UAC 无法中途提升）。
    #[cfg(windows)]
    fn spawn_startup_guard(&self) {
        if !self.state.settings.tun.enable {
            return;
        }
        let ui_tx = self.ui_tx.clone();
        tokio::spawn(async move {
            if !crate::service::process::is_elevated() {
                let _ = ui_tx.send(UiEvent::StartupNotice(
                    "TUN 模式需要管理员权限：当前 TUI 未以管理员身份运行，mihomo 将无法创建 \
                     TUN 设备。请关闭 TUN，或退出后右键「以管理员身份运行」本程序"
                        .to_string(),
                ));
            }
        });
    }

    /// 启动引导：Linux —— systemd 模式且服务不可用 → 通知用户。
    #[cfg(not(windows))]
    fn spawn_startup_guard(&self) {
        /* 原实现整体保留（含 `if self.state.settings.run_mode != RunMode::Systemd { return; }`） */
    }
```

- [ ] **Step 8: 首启安装引导 — cfg 门控**

`app::run` 中的 `if crate::service::installer::needs_install().await { ... }` 整块包进：

```rust
    #[cfg(not(windows))]
    {
        if crate::service::installer::needs_install().await {
            /* 原 ConfirmPopup 块整体保留 */
        }
    }
```

- [ ] **Step 9: 验证 + 提交**

```bash
cargo build 2>&1 | tail -3
cargo test 2>&1 | tail -8
cargo fmt --all --check && cargo clippy --all-targets -- -D warnings
cargo check --target x86_64-pc-windows-msvc 2>&1 | tail -15   # target 已装则跑
git add src/app.rs
git commit -m "feat: app 主循环平台化——Windows 跳过安装引导/systemd，路径保存走 settings.toml，TUN 启动通知"
```

---

### Task 6: ui/settings.rs 平台化（含 TUN 警告）

**Files:**
- Modify: `src/ui/settings.rs`

前置：Task 2（import 已改）+ Task 3（is_elevated）+ Task 4 已合入。

- [ ] **Step 1: field_values — 运行方式下拉 Windows 只显示 direct**

```rust
    let run_mode_options: Vec<String> = if cfg!(windows) {
        vec!["direct".into()]
    } else {
        vec!["systemd".into(), "direct".into()]
    };
```
`f[0]` 的构造改为 `kind: FieldKind::Dropdown(run_mode_options)`。

- [ ] **Step 2: apply_values — 解析选项平台化**

```rust
        run_mode: {
            let options: &[&str] = if cfg!(windows) {
                &["direct"]
            } else {
                &["systemd", "direct"]
            };
            match cfg_parse_dropdown(&f[0], options)?.as_str() {
                "systemd" => RunMode::Systemd,
                _ => RunMode::Direct,
            }
        },
```

- [ ] **Step 3: 保存流程 — 保留 mihomo_bin + TUN 管理员警告**

`save()` 中 `Ok(s) => {` 分支、`save_settings` 之前插入：

```rust
            Ok(mut s) => {
                // mihomo_bin 不经过表单（Windows 路径由 mihomo-bin 弹窗维护），保存时保留现值
                s.mihomo_bin = st.settings.mihomo_bin.clone();
                // Windows：TUN 从关→开 且 TUI 非管理员 → 警告弹窗（UAC 无法中途提升；
                // 提示后仍尊重用户选择继续保存）
                #[cfg(windows)]
                if s.tun.enable && !st.settings.tun.enable && !crate::service::process::is_elevated()
                {
                    self.popup = Some(MessagePopup::new(
                        "TUN 需要管理员权限".into(),
                        vec![
                            "TUN 模式需要管理员权限：当前 TUI 未以管理员身份运行，".into(),
                            "mihomo 将无法创建 TUN 设备。".into(),
                            "请关闭 TUN，或退出后右键「以管理员身份运行」本程序。".into(),
                        ],
                    ));
                }
                let old_mode = st.settings.run_mode;
                if let Err(e) = save_settings(&s) {
```

注意：原代码是 `Ok(s) => {`（不可变绑定），需改为 `Ok(mut s) => {`。

- [ ] **Step 4: 路径输入弹窗预填 — 平台化绝对路径判定**

```rust
                    1 => {
                        let current = self.fields[1].value.clone();
                        let prefill = if crate::core::mihomo_bin::is_absolute_path(&current) {
                            current
                        } else {
                            find_mihomo_in_path().unwrap_or_default()
                        };
```

- [ ] **Step 5: 测试适配**

1. `fixed_settings()` 中 `run_mode: RunMode::Systemd,` 改为：
```rust
            run_mode: if cfg!(windows) { RunMode::Direct } else { RunMode::Systemd },
```
2. `field_values_apply_values_roundtrip` 中 `assert_eq!(back.run_mode, RunMode::Systemd);` 改为：
```rust
        assert_eq!(
            back.run_mode,
            if cfg!(windows) { RunMode::Direct } else { RunMode::Systemd }
        );
```
3. 检查 tests mod 其余用例（`default_settings_roundtrip` 等）——`NetworkSettings::default()` 已平台化，无需改；`run_mode = "systemd"` 字符串断言若存在需同样平台化。

- [ ] **Step 6: 验证 + 提交**

```bash
cargo build 2>&1 | tail -3
cargo test 2>&1 | tail -8
cargo fmt --all --check && cargo clippy --all-targets -- -D warnings
cargo check --target x86_64-pc-windows-msvc 2>&1 | tail -15
git add src/ui/settings.rs
git commit -m "feat: 设置页平台化——Windows 仅 direct 模式、TUN 管理员警告、路径预填平台规则"
```

---

### Task 7: CI 增加 Windows job + 发布 Windows 产物

**Files:**
- Modify: `.github/workflows/ci.yml`
- Modify: `.github/workflows/release.yml`

与其他任务无文件冲突，可与 Task 3/4/5 并行。

- [ ] **Step 1: ci.yml — 追加 windows job（保持现有 check job 不动）**

```yaml
  check-windows:
    name: fmt + clippy + test (windows)
    runs-on: windows-latest
    steps:
      - uses: actions/checkout@v6

      - uses: dtolnay/rust-toolchain@stable
        with:
          components: rustfmt, clippy

      - uses: Swatinem/rust-cache@v2

      - name: Check formatting
        run: cargo fmt --all --check

      - name: Clippy
        run: cargo clippy --all-targets -- -D warnings

      - name: Test
        run: cargo test
```

- [ ] **Step 2: release.yml — 追加独立 windows job（Linux 矩阵 job 不动）**

```yaml
  # 3) Windows 产物：windows-latest 原生构建（MSVC），zip 打包上传
  upload-assets-windows:
    name: Build & Upload (x86_64-pc-windows-msvc)
    needs: create-release
    runs-on: windows-latest
    permissions:
      contents: write
    steps:
      - uses: actions/checkout@v6

      - uses: dtolnay/rust-toolchain@stable
        with:
          targets: x86_64-pc-windows-msvc

      - uses: Swatinem/rust-cache@v2

      # 产物命名：mihomo-tui-v0.1.0-x86_64-pc-windows-msvc.zip
      # include 同 Linux（README + LICENSE）；checksum 生成 .sha256
      - uses: taiki-e/upload-rust-binary-action@v1
        with:
          bin: mihomo-tui
          target: x86_64-pc-windows-msvc
          archive: $bin-$tag-$target
          include: README.md,LICENSE
          checksum: sha256
          token: ${{ secrets.GITHUB_TOKEN }}
```

- [ ] **Step 3: 验证 + 提交**

```bash
# YAML 语法检查（无 yq 则目检缩进）
python3 -c "import yaml,sys; [yaml.safe_load(open(f)) for f in ['.github/workflows/ci.yml','.github/workflows/release.yml']]; print('yaml ok')"
git add .github/workflows/ci.yml .github/workflows/release.yml
git commit -m "ci: Windows job（fmt+clippy+test）+ release 产出 x86_64-pc-windows-msvc zip"
```

---

### Task 8: README 增加 Windows 支持文档

**Files:**
- Modify: `README.md`

与其他任务无文件冲突，可与 Task 3/4/5 并行。

- [ ] **Step 1: 功能总览的平台描述**

开头 `# mihomo-tui` 简介段落后，增加平台支持说明（放在「功能总览」之前）：

```markdown
## 平台支持

| 平台 | 运行方式 | 说明 |
|---|---|---|
| Linux | systemd（默认）/ 直接进程 | 直接进程经提权脚本 `mihomo-proc` 管理；配置目录 `~/.config/mihomo-tui/` |
| Windows | 直接进程（唯一） | TUI 直接启动/停止/重启 `mihomo.exe`；配置目录 `%APPDATA%\mihomo-tui\` |

Windows 没有 systemd/sudo 体系：在设置页「运行方式」区块设置 mihomo 可执行文件路径后，
TUI 直接以子进程方式管理 mihomo（详见「Windows 使用指南」）。
```

- [ ] **Step 2: 新增「Windows 使用指南」章节（放在「使用指南」之后、「前提」之前）**

```markdown
## Windows 使用指南

### 1. 安装

1. 下载 mihomo Windows 版（[GitHub Releases](https://github.com/MetaCubeX/mihomo/releases)，
   选 `mihomo-windows-amd64-v*.zip`），解压到任意目录（如 `C:\mihomo\`）。
   **如需 TUN 模式，请保留同目录的 `wintun.dll`**（官方 zip 自带）。
2. 下载本项目的 Windows release（`mihomo-tui-v*-x86_64-pc-windows-msvc.zip`）并解压。
3. 在终端中运行 `mihomo-tui.exe`（推荐 Windows Terminal）。

### 2. 设置 mihomo 路径

- 设置页（`s` 键）→「运行方式」区块 → `mihomo-bin` 行按 `Enter`
- 输入 mihomo 可执行文件**绝对路径**（如 `C:\mihomo\mihomo.exe`）并确认
- TUI 会校验路径并执行 `mihomo.exe -v` 探测；路径保存在 `%APPDATA%\mihomo-tui\settings.toml`

### 3. 使用

- 订阅页按 `a` 添加订阅 → 按 `Enter` 激活：自动完成「合并 → `mihomo -t` 校验 →
  写入 config.yaml → 重启 mihomo」
- 设置页「运行方式」区块的**启动/停止/重启**按钮直接管理 mihomo 进程
- 所有配置存放在 `%APPDATA%\mihomo-tui\`：`config.yaml`、`mihomo.pid`、`mihomo.log`、
  `settings.toml`、`subscriptions.toml`、`overrides.toml`
- mihomo 以子进程方式运行（`CREATE_NO_WINDOW`，不弹控制台窗口）；**TUI 退出后 mihomo
  继续运行**，再次启动 TUI 可看到状态并停止/重启

### 4. TUN 模式（管理员权限）

Windows 上开启 TUN 需要**管理员权限**（UAC 提权是进程级属性，运行中无法提升）：

- 以管理员身份运行 TUI（右键 `mihomo-tui.exe` →「以管理员身份运行」），且
  `mihomo.exe` 同目录存在 `wintun.dll`
- 若 TUI 未以管理员运行：设置页开启 TUN 时会弹警告，启动时也会提示一次；
  此时 mihomo 无法创建 TUN 设备（其他功能不受影响）

### 5. 与 Linux 的差异

- 无 systemd 服务、无开机自启（重启系统后需手动启动 mihomo）
- 无提权安装器（`i` 键安装流程仅在 Linux 可用）
- mihomo 日志写入 `%APPDATA%\mihomo-tui\mihomo.log`（持续追加，无轮转）
```

- [ ] **Step 3: 「前提」章节开头区分平台**

```markdown
## 前提

**Linux**：Arch Linux（或任何能装 mihomo 的 Linux 发行版；Arch 上 `sudo pacman -S mihomo`）
**Windows**：见「Windows 使用指南」（mihomo.exe + wintun.dll，无 systemd/sudo 要求）

（Linux 部分原文保留：mihomo 已安装并作为 **systemd 服务**存在……）
```

- [ ] **Step 4: 验证 + 提交**

```bash
git add README.md
git commit -m "docs: Windows 使用指南与平台支持说明"
```

---

### Task 9: 集成验证（负责人执行）

- [ ] **Step 1: Linux 全量验证**

```bash
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --release
```

期望：全绿。跑 `cargo test mihomo_bin settings:: tests::windows_config_dir` 确认新测试执行。

- [ ] **Step 2: Windows 编译面验证**

```bash
rustup target add x86_64-pc-windows-msvc 2>/dev/null || true
cargo check --target x86_64-pc-windows-msvc 2>&1 | tail -20
```

期望：0 error。若本机无法装 target，说明依赖 CI windows job 兜底（推送后观察 Actions 结果）。

- [ ] **Step 3: 交互冒烟（Linux 行为无回归）**

```bash
cargo run --example merge_sample > /tmp/winplan-config.yaml && head -5 /tmp/winplan-config.yaml
```

期望：正常输出合并配置（settings 无损坏）。

- [ ] **Step 4: 推送 dev 分支**（确认无其他会话未推送提交后）

```bash
git status --short
git push origin dev
```

---

## 任务依赖与并行

```
Task 1 → Task 2 → Task 3 → Task 4 → Task 5 → Task 6 → Task 9（验证）
              └─ Task 7（CI）、Task 8（README）可与 Task 3-6 并行（无文件冲突）
```

顺序执行 1→2→3→4→5→6（同文件依赖：apply.rs 被 Task 3/4 修改、app.rs/ui 依赖其 API），
Task 7、8 独立可并行。Task 9 由负责人执行后进入 reviewer 审查循环。
