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

/// 进程操作互斥锁：start/stop/apply/control 串行执行。
/// 防两次快速 start 并发 → 双实例 + PID 文件被后启动者删除（TOCTOU）。
static OP_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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
    // 原子写：tmp + rename，防崩溃残留半截 PID 文件
    let tmp = config_dir().join(".mihomo.pid.tmp");
    tokio::fs::write(&tmp, format!("{pid}\n"))
        .await
        .map_err(|e| ApplyError::Io(format!("写入 PID 文件失败: {e}")))?;
    tokio::fs::rename(&tmp, pid_path())
        .await
        .map_err(|e| ApplyError::Io(format!("写入 PID 文件失败: {e}")))?;
    Ok(())
}

/// 启动 mihomo（加进程互斥锁）：`-d <dir> -f <dir>\config.yaml`，stdout/stderr → mihomo.log，
/// CREATE_NO_WINDOW 不弹控制台；TUI 退出后进程继续运行（与 Linux setsid 语义一致）。
pub async fn start() -> Result<ApplyOutcome, ApplyError> {
    let _guard = OP_LOCK.lock().await;
    start_unlocked().await
}

/// start 的实际实现（调用方须已持有 OP_LOCK）。
async fn start_unlocked() -> Result<ApplyOutcome, ApplyError> {
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
        // 仅当 PID 文件仍是本实例写入的才清理（防并发下删掉先启动者的 PID 记录）
        if read_pid().await == Some(pid) {
            let _ = tokio::fs::remove_file(pid_path()).await;
        }
        return Err(ApplyError::Io(
            "mihomo 启动后立即退出（请查看 mihomo.log：配置错误、端口被占用或已有其他 mihomo 实例）"
                .to_string(),
        ));
    }
    Ok(ApplyOutcome {
        success: true,
        stdout: format!("mihomo 已启动（PID {pid}，日志 {LOG_FILE}）"),
        stderr: String::new(),
    })
}

/// 停止 mihomo（加进程互斥锁；按 PID 文件定位；kill 前校验镜像路径防误杀；残留 PID 自动清理）。
pub async fn stop() -> Result<ApplyOutcome, ApplyError> {
    let _guard = OP_LOCK.lock().await;
    stop_unlocked().await
}

/// stop 的实际实现（调用方须已持有 OP_LOCK）。
async fn stop_unlocked() -> Result<ApplyOutcome, ApplyError> {
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

/// 应用配置（加进程互斥锁）：用配置 bin 校验 → 备份旧配置 → 原子写 config.yaml → 重启进程。
/// 校验失败不触碰运行中的 mihomo（与 Linux 链路语义一致）；启动失败回滚旧配置并重试一次。
pub async fn apply(yaml: &str) -> Result<ApplyOutcome, ApplyError> {
    let _guard = OP_LOCK.lock().await;
    apply_unlocked(yaml).await
}

/// apply 的实际实现（调用方须已持有 OP_LOCK）。
async fn apply_unlocked(yaml: &str) -> Result<ApplyOutcome, ApplyError> {
    let bin = configured_bin()?;
    crate::core::apply::validate_config(yaml, Some(&bin)).await?;
    let dir = config_dir();
    let tmp = dir.join(".config.yaml.tmp");
    // 备份旧配置（启动失败回滚用，与 Linux mihomo-apply 健康检查回滚对称）
    let bak = dir.join("config.yaml.bak");
    if tokio::fs::try_exists(config_path()).await.unwrap_or(false) {
        let _ = tokio::fs::copy(config_path(), &bak).await;
    }
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
        stop_unlocked().await?;
    }
    // 启动失败 → 回滚旧配置并重试一次
    match start_unlocked().await {
        Ok(o) => Ok(o),
        Err(e) => {
            let _ = tokio::fs::rename(&bak, config_path()).await;
            match start_unlocked().await {
                Ok(_) => Err(ApplyError::Io(format!(
                    "新配置启动失败，已回滚为旧配置: {e}"
                ))),
                Err(e2) => Err(ApplyError::Io(format!(
                    "新配置启动失败（回滚旧配置后仍失败）: {e}；{e2}"
                ))),
            }
        }
    }
}

/// 启/停/重启统一入口（加进程互斥锁；app.rs ProcAction 经 apply::proc_control 分派到这里）。
pub async fn control(op: ProcOp) -> Result<ApplyOutcome, ApplyError> {
    let _guard = OP_LOCK.lock().await;
    match op {
        ProcOp::Start => start_unlocked().await,
        ProcOp::Stop => stop_unlocked().await,
        ProcOp::Restart => {
            stop_unlocked().await?;
            start_unlocked().await
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
        assert!(paths_equal(
            r"C:\mihomo\mihomo.exe",
            r"c:/mihomo/mihomo.exe"
        ));
        assert!(paths_equal(
            r"C:\mihomo\mihomo.exe",
            r"C:\mihomo\Mihomo.EXE"
        ));
        assert!(paths_equal(r"C:\mihomo\", r"c:/mihomo"));
        assert!(!paths_equal(
            r"C:\mihomo\mihomo.exe",
            r"D:\mihomo\mihomo.exe"
        ));
        assert!(!paths_equal(
            r"C:\mihomo\mihomo.exe",
            r"C:\mihomo\other.exe"
        ));
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
