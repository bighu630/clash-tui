//! 配置应用：`mihomo -t` 预校验（临时文件）→ 按运行方式分派：
//! systemd 模式 → `sudo [-n] /usr/local/sbin/mihomo-apply`（stdin 喂入 config.yaml）；
//! direct 模式 → `sudo [-n] /usr/local/sbin/mihomo-proc`（stdin：首行命令 + 数据）。
//! 失败时把 mihomo/sudo 输出原样反馈给用户。

use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::core::models::RunMode;
use crate::core::settings::config_dir;

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn tmp_path(name: &str) -> std::path::PathBuf {
    let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    config_dir().join(format!(".{name}.{}.{n}.tmp", std::process::id()))
}

/// 写临时文件（0600：内容含代理密码/secret，不能按默认 0644 落盘）。
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

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ApplyOutcome {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ApplyError {
    #[error("sudo 不可用")]
    SudoNotAvailable,
    #[error("sudo 需要密码（交互模式）")]
    SudoNeedsPassword,
    #[error(
        "当前用户未被 sudoers 授权调用 mihomo-apply。请在仪表盘页按 i 重新安装提权组件，\
         或检查 /etc/sudoers.d/99-mihomo 与 /etc/sudoers 的 @includedir /etc/sudoers.d 配置"
    )]
    NotInSudoers,
    #[error("未设置 mihomo 路径：请先在设置页 Enter mihomo-bin 设置 mihomo 可执行文件路径")]
    BinNotConfigured,
    #[error(
        "缺少提权脚本 /usr/local/sbin/mihomo-proc（直接进程模式需要）。\n\
         请重新安装提权组件：仪表盘页按 i，或手动安装 resources/mihomo-proc.sh 并更新 sudoers"
    )]
    ProcScriptMissing,
    #[error("mihomo -t 校验失败:\n{stderr}")]
    ValidateFailed { stderr: String },
    #[error(
        "systemctl {action} mihomo 认证失败（polkit 未授权或缺少认证代理）。\n\
         请确认在桌面会话中运行（polkit 代理如 gnome-shell 会弹出密码框）；\n\
         无桌面环境时请手动执行：sudo systemctl {action} mihomo\n原始错误：{stderr}"
    )]
    SystemdAuthFailed { action: String, stderr: String },
    #[error("执行失败:\n{stdout}\n{stderr}")]
    CommandFailed { stdout: String, stderr: String },
    #[error("{0}")]
    Io(String),
}

/// 直接进程模式的操作命令（stdin 首行）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcOp {
    Start,
    Stop,
    Restart,
}

impl ProcOp {
    /// stdin 协议首行。
    pub fn stdin_line(&self) -> &'static str {
        match self {
            ProcOp::Start => "start",
            ProcOp::Stop => "stop",
            ProcOp::Restart => "restart",
        }
    }
}

/// mihomo-proc status 输出解析结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcStatus {
    /// 配置的二进制路径（conf 未设置/读取失败为 None）
    pub bin: Option<String>,
    /// 进程 PID（未运行为 None）
    pub pid: Option<u32>,
    /// 是否运行中
    pub running: bool,
}

/// 模式相关的统一运行状态（设置页显示用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunStatus {
    /// systemd 单元是否存在（list-unit-files；无 systemd 环境为 false）
    pub service_unit: Option<bool>,
    /// systemd 服务是否 active
    pub service_active: Option<bool>,
    /// 直接进程实例状态（未安装提权组件/查询失败为 None）
    pub proc: Option<ProcStatus>,
}

/// 解析 mihomo-proc status 行协议（key=value，未知行忽略）。
pub fn parse_proc_status(output: &str) -> ProcStatus {
    let mut bin = None;
    let mut pid = None;
    let mut running = false;
    for line in output.lines() {
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        match k {
            "bin" => bin = (!v.is_empty()).then(|| v.to_string()),
            "pid" => pid = v.parse().ok(),
            "running" => running = v == "true",
            _ => {}
        }
    }
    ProcStatus { bin, pid, running }
}

/// direct 模式 apply 的 stdin 协议体：首行 apply + 空行 + config.yaml。
#[cfg(not(windows))]
pub fn direct_apply_body(yaml: &str) -> String {
    format!("apply\n{yaml}")
}

const VALIDATE_TMP: &str = "validate";
#[cfg(not(windows))]
const APPLY_TMP: &str = "apply";
#[cfg(not(windows))]
const PROC_TMP: &str = "proc";
#[cfg(not(windows))]
const STATUS_TMP: &str = "proc-status";

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

/// 按运行方式分派提权应用：
/// systemd → `sudo [-n] /usr/local/sbin/mihomo-apply`（stdin 喂 yaml）；
/// direct → `sudo [-n] /usr/local/sbin/mihomo-proc`（stdin 首行 apply + yaml）。
/// non_interactive=true 且 sudo 提示密码 → SudoNeedsPassword（交互模式重试）；
/// 非交互且当前用户未被 sudoers 授权 → NotInSudoers（该情形交互重试也必败）。
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

/// 通用提权脚本调用：写临时文件（stdin 内容）→ sudo 执行脚本 → 分类失败原因。
#[cfg(not(windows))]
async fn run_apply_script(
    script: &str,
    body: &str,
    non_interactive: bool,
    tmp_name: &str,
) -> Result<ApplyOutcome, ApplyError> {
    let path = tmp_path(tmp_name);
    write_secret_file(&path, body)
        .await
        .map_err(|e| ApplyError::Io(e.to_string()))?;
    let mut args = Vec::new();
    if non_interactive {
        args.push("-n");
    }
    args.push(script);
    let result = run_capture("sudo", &args, Some(path.to_str().unwrap())).await;
    let _ = tokio::fs::remove_file(&path).await;
    match result {
        Ok((status, stdout, stderr)) => {
            if status.success() {
                Ok(ApplyOutcome {
                    success: true,
                    stdout,
                    stderr,
                })
            } else {
                match classify_sudo_failure(&stderr) {
                    // 非交互模式且 sudo 要求密码 → 提示转交互重试
                    SudoFailureKind::NeedsPassword if non_interactive => {
                        Err(ApplyError::SudoNeedsPassword)
                    }
                    // 用户不在 sudoers：交互重试必败，直接给修复指引
                    SudoFailureKind::NotInSudoers => Err(ApplyError::NotInSudoers),
                    _ => Err(ApplyError::CommandFailed { stdout, stderr }),
                }
            }
        }
        Err(e) => Err(e),
    }
}

/// `sudo -n /usr/local/sbin/mihomo-proc`，stdin 首行 = 命令（start/stop/restart）。
/// 脚本未安装时直接返回 ProcScriptMissing（明确引导重装，避免 sudo 裸报"找不到命令"）。
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

/// `sudo -n /usr/local/sbin/mihomo-proc` status：查询进程实例状态。
/// 脚本未安装时返回 ProcScriptMissing（调用方决定静默或提示）。
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
        let out =
            run_apply_script("/usr/local/sbin/mihomo-proc", "status\n", true, STATUS_TMP).await?;
        Ok(parse_proc_status(&out.stdout))
    }
}

/// systemd 模式服务操作：`pkexec systemctl <action> mihomo`。
/// pkexec 经桌面 polkit 代理弹系统密码框（无需 tty、无需退出 TUI raw 模式；
/// 裸 systemctl 无 tty 时 polkit 拒绝交互认证，不会弹窗）。
/// 无桌面代理/未授权 → SystemdAuthFailed（含手动 sudo 指引）。
#[cfg(not(windows))]
pub async fn systemctl_control(op: ProcOp) -> Result<ApplyOutcome, ApplyError> {
    let action = match op {
        ProcOp::Start => "start",
        ProcOp::Stop => "stop",
        ProcOp::Restart => "restart",
    };
    let result = run_capture("pkexec", &["systemctl", action, "mihomo"], None).await;
    match result {
        Ok((status, stdout, stderr)) => {
            if status.success() {
                Ok(ApplyOutcome {
                    success: true,
                    stdout,
                    stderr,
                })
            } else {
                Err(classify_systemctl_failure(action, &stdout, &stderr))
            }
        }
        Err(e) => Err(e),
    }
}

/// 分类 systemctl 失败：polkit 认证类错误 → SystemdAuthFailed（引导）；其余 → CommandFailed。
#[cfg(not(windows))]
fn classify_systemctl_failure(action: &str, stdout: &str, stderr: &str) -> ApplyError {
    let combined = format!("{stdout}\n{stderr}").to_lowercase();
    if combined.contains("interactive authentication required")
        || combined.contains("not authorized")
        || combined.contains("access denied")
        || combined.contains("policykit")
        || combined.contains("polkit")
        || combined.contains("authentication agent")
        || combined.contains("controlling terminal")
    {
        ApplyError::SystemdAuthFailed {
            action: action.to_string(),
            stderr: stderr.to_string(),
        }
    } else {
        ApplyError::CommandFailed {
            stdout: stdout.to_string(),
            stderr: stderr.to_string(),
        }
    }
}

/// /usr/local/sbin/mihomo-proc 是否已安装（存在且可执行）。
/// 与 is_apply_script_installed 共用 script_installed_at 辅助。
#[cfg(not(windows))]
pub async fn is_proc_script_installed() -> bool {
    script_installed_at("/usr/local/sbin/mihomo-proc").await
}

#[cfg(not(windows))]
async fn script_installed_at(path: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    tokio::fs::metadata(path)
        .await
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// /usr/local/sbin/mihomo-apply 是否已安装（存在且可执行）。
#[cfg(not(windows))]
pub async fn is_apply_script_installed() -> bool {
    script_installed_at("/usr/local/sbin/mihomo-apply").await
}

/// 同步版（installer 测试用）：/usr/local/sbin/mihomo-proc 是否已安装。
#[cfg(not(windows))]
pub fn is_proc_script_installed_sync() -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata("/usr/local/sbin/mihomo-proc")
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// systemctl is-active --quiet mihomo。
#[cfg(not(windows))]
pub async fn service_is_active() -> bool {
    Command::new("systemctl")
        .args(["is-active", "--quiet", "mihomo"])
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

/// systemctl list-unit-files mihomo.service 是否含 mihomo.service（单元是否存在）。
#[cfg(not(windows))]
pub async fn service_unit_exists() -> bool {
    Command::new("systemctl")
        .args(["list-unit-files", "mihomo.service"])
        .output()
        .await
        .map(|o| {
            o.status.success() && String::from_utf8_lossy(&o.stdout).contains("mihomo.service")
        })
        .unwrap_or(false)
}

/// which mihomo 取路径（设置页路径输入预填用；找不到返回 None）。
pub fn find_mihomo_in_path() -> Option<String> {
    let out = std::process::Command::new("which")
        .arg("mihomo")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// which mihomo（存在性判断）。
pub async fn mihomo_is_installed() -> bool {
    find_mihomo_in_path().is_some()
}

/// 运行命令并捕获输出；stdin 可来自文件。命令不存在 → 按场景映射错误。
async fn run_capture(
    cmd: &str,
    args: &[&str],
    stdin_file: Option<&str>,
) -> Result<(std::process::ExitStatus, String, String), ApplyError> {
    let mut command = Command::new(cmd);
    command
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(path) = stdin_file {
        // 打开本地文件作为子进程 stdin（快速阻塞操作，可接受）
        let file = std::fs::File::open(path).map_err(|e| ApplyError::Io(e.to_string()))?;
        command.stdin(Stdio::from(file));
    } else {
        command.stdin(Stdio::null());
    }
    let mut child = match command.spawn() {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if cmd == "sudo" {
                return Err(ApplyError::SudoNotAvailable);
            }
            return Err(ApplyError::Io(format!("{cmd} 未安装: {e}")));
        }
        Err(e) => return Err(ApplyError::Io(e.to_string())),
    };
    let mut stdout = child.stdout.take().expect("stdout 管道已请求");
    let mut stderr = child.stderr.take().expect("stderr 管道已请求");
    let (out, err, status) =
        tokio::join!(read_all(&mut stdout), read_all(&mut stderr), child.wait());
    let status = status.map_err(|e| ApplyError::Io(e.to_string()))?;
    Ok((status, out, err))
}

async fn read_all<R: tokio::io::AsyncRead + Unpin>(r: &mut R) -> String {
    let mut buf = Vec::new();
    if tokio::io::copy(r, &mut buf).await.is_ok() {
        String::from_utf8_lossy(&buf).into_owned()
    } else {
        String::new()
    }
}

/// sudo 失败原因分类（兼容中英文 locale）。
#[cfg(not(windows))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SudoFailureKind {
    /// sudo 要求输入密码（非交互模式会失败）
    NeedsPassword,
    /// 当前用户不在 sudoers 授权列表（交互重试也必败）
    NotInSudoers,
    /// 其他失败（命令不存在、脚本校验失败等）
    Other,
}

/// 解析 sudo stderr 判定失败原因。
/// 设计意图：只匹配 sudo 自身的诊断消息特征，避免把 mihomo -t 校验输出误判为
/// "需要 sudo 密码"——脚本内校验失败时 mihomo 会输出如
/// `proxy 0: '' has unset fields: cipher, password`，若裸词 "password" 也作判定依据
/// 就会命中，导致对必然失败的交互重试再弹一次确认框。因此裸词 "password" 不再作为
/// 判定依据；"passwd" 与 "password" 是不同子串（互不包含），保留 "passwd" 判定无碍。
#[cfg(not(windows))]
fn classify_sudo_failure(stderr: &str) -> SudoFailureKind {
    let s = stderr.to_lowercase();
    if s.contains("a password is required") || s.contains("需要密码") || s.contains("passwd") {
        SudoFailureKind::NeedsPassword
    } else if s.contains("sudoers") {
        SudoFailureKind::NotInSudoers
    } else {
        SudoFailureKind::Other
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(not(windows))]
    use crate::core::models::RunMode;
    #[cfg(not(windows))]
    use crate::core::settings::with_settings_dir;

    /// 依赖 config_dir（MIHOMO_TUI_SETTINGS_DIR）的三个用例与 settings/dashboard
    /// 测试共用 SETTINGS_DIR_LOCK（with_settings_dir）串行执行，消除 env 并行读写
    /// 竞态。with_settings_dir 为同步闭包，内部以独立 Runtime block_on 驱动异步体。
    /// 依赖真实 mihomo 环境，windows-latest CI 无 → 仅 Linux；
    /// ubuntu-latest 等无 mihomo 的环境自适应跳过。
    #[cfg(not(windows))]
    fn mihomo_available() -> bool {
        std::process::Command::new("mihomo")
            .arg("-v")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    #[cfg(not(windows))]
    #[test]
    fn validate_ok_minimal_config() {
        // 环境自适应：ubuntu-latest 等无 mihomo 的 CI 直接跳过
        if !mihomo_available() {
            eprintln!("跳过：本机无 mihomo");
            return;
        }
        with_settings_dir(|| {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(validate_config("port: 7890\n", None)).unwrap();
        });
    }

    #[cfg(not(windows))]
    #[test]
    fn validate_bad_yaml_fails_with_stderr() {
        // 环境自适应：无 mihomo 时无法产生校验 stderr，跳过
        if !mihomo_available() {
            eprintln!("跳过：本机无 mihomo");
            return;
        }
        with_settings_dir(|| {
            let rt = tokio::runtime::Runtime::new().unwrap();
            let e = rt
                .block_on(validate_config("port: 7890\nproxies: [\n", None))
                .unwrap_err();
            match e {
                ApplyError::ValidateFailed { stderr } => {
                    assert!(!stderr.is_empty(), "mihomo 报错应原样透出");
                }
                other => panic!("期望 ValidateFailed，得到 {other:?}"),
            }
        });
    }

    #[cfg(not(windows))]
    #[test]
    fn apply_non_interactive_password_or_validation_failure() {
        // 环境自适应且无副作用：无效 YAML 在脚本内 mihomo -t 预校验阶段即失败，
        // 不会替换 /etc/mihomo/config.yaml；未装脚本/sudo 需密码/免密/
        // 用户无 sudo 权限（CI/容器）等环境均覆盖。
        with_settings_dir(|| {
            let rt = tokio::runtime::Runtime::new().unwrap();
            let e = rt
                .block_on(apply_config("bad: [\n", true, RunMode::Systemd))
                .unwrap_err();
            match &e {
                ApplyError::SudoNeedsPassword
                | ApplyError::SudoNotAvailable
                | ApplyError::NotInSudoers
                | ApplyError::CommandFailed { .. } => {}
                other => panic!("意外错误: {other:?}"),
            }
            // 脚本已安装时，CommandFailed 必为脚本内 mihomo -t 校验失败（stderr 透出），
            // 证明失败发生在替换配置之前，无配置替换副作用。
            if let ApplyError::CommandFailed { stderr, .. } = &e {
                if rt.block_on(is_apply_script_installed()) {
                    let s = stderr.to_lowercase();
                    assert!(
                        s.contains("test failed") || s.contains("validation"),
                        "期望脚本校验失败输出，实际 stderr: {stderr}"
                    );
                }
            }
        });
    }

    /// 失败分类规则：需密码 / 不在 sudoers / 其他（中英文 locale）。
    #[cfg(not(windows))]
    #[test]
    fn classify_sudo_failure_rules() {
        use SudoFailureKind::*;
        assert!(matches!(
            classify_sudo_failure("sudo: a password is required"),
            NeedsPassword
        ));
        assert!(matches!(
            classify_sudo_failure("sudo: 需要密码"),
            NeedsPassword
        ));
        assert!(matches!(
            classify_sudo_failure(
                "alice is not in the sudoers file. This incident will be reported."
            ),
            NotInSudoers
        ));
        assert!(matches!(
            classify_sudo_failure("用户不在 sudoers 文件"),
            NotInSudoers
        ));
        assert!(matches!(
            classify_sudo_failure("sudo: mihomo-apply: command not found"),
            Other
        ));
        assert!(matches!(classify_sudo_failure("test failed"), Other));
        // 回归：mihomo -t 校验失败输出可含 "password"（unset fields 提示），
        // 不得误判为"需要 sudo 密码"（否则脚本校验失败会触发必然失败的交互重试）。
        assert!(matches!(
            classify_sudo_failure("proxy 0: '' has unset fields: cipher, password"),
            Other
        ));
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn script_installed_flag_matches_fs() {
        let installed = std::path::Path::new("/usr/local/sbin/mihomo-apply")
            .metadata()
            .map(|m| {
                use std::os::unix::fs::PermissionsExt;
                m.is_file() && m.permissions().mode() & 0o111 != 0
            })
            .unwrap_or(false);
        assert_eq!(is_apply_script_installed().await, installed);
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn mihomo_installed_flag_matches_fs() {
        let which = tokio::process::Command::new("which")
            .arg("mihomo")
            .output()
            .await
            .unwrap()
            .status
            .success();
        assert_eq!(mihomo_is_installed().await, which);
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn service_is_active_smoke() {
        let _ = service_is_active().await;
    }

    /// 直接进程模式 apply 的 stdin 协议：首行 apply + 空行后接 yaml。
    #[cfg(not(windows))]
    #[test]
    fn direct_apply_stdin_protocol() {
        let body = direct_apply_body("port: 7890\n");
        assert!(body.starts_with("apply\nport: 7890\n"), "body: {body:?}");
    }

    /// status 行协议解析：正常/空值/乱序/未知行。
    #[test]
    fn parse_proc_status_lines() {
        let s = parse_proc_status(
            "bin=/usr/bin/mihomo\npid=1234\nrunning=true\nconfig=/etc/mihomo/config.yaml\n",
        );
        assert_eq!(s.bin.as_deref(), Some("/usr/bin/mihomo"));
        assert_eq!(s.pid, Some(1234));
        assert!(s.running);
        // 空值
        let s = parse_proc_status("bin=\npid=\nrunning=false\n");
        assert_eq!(s.bin, None);
        assert_eq!(s.pid, None);
        assert!(!s.running);
        // 乱序 + 未知行 + 非数字 pid
        let s = parse_proc_status("running=true\nunknown=xyz\npid=abc\nbin=/opt/mihomo\n");
        assert_eq!(s.bin.as_deref(), Some("/opt/mihomo"));
        assert_eq!(s.pid, None);
        assert!(s.running);
        // 空输出
        let s = parse_proc_status("");
        assert_eq!(s.bin, None);
        assert!(!s.running);
    }

    /// ProcOp 的命令行映射。
    #[test]
    fn proc_op_stdin_lines() {
        assert_eq!(ProcOp::Start.stdin_line(), "start");
        assert_eq!(ProcOp::Stop.stdin_line(), "stop");
        assert_eq!(ProcOp::Restart.stdin_line(), "restart");
    }

    /// 环境自适应：apply_config direct 模式，坏 yaml 必须在脚本内预校验阶段失败
    /// （未装脚本/sudo 无权限/需密码等环境均覆盖，无副作用）。
    /// Windows 上 apply_config 走 process::apply（BinNotConfigured），不适用。
    #[cfg(not(windows))]
    #[test]
    fn apply_direct_bad_yaml_fails_early() {
        with_settings_dir(|| {
            let rt = tokio::runtime::Runtime::new().unwrap();
            let e = rt
                .block_on(apply_config("bad: [\n", true, RunMode::Direct))
                .unwrap_err();
            match &e {
                ApplyError::SudoNeedsPassword
                | ApplyError::SudoNotAvailable
                | ApplyError::NotInSudoers
                | ApplyError::CommandFailed { .. } => {}
                other => panic!("意外错误: {other:?}"),
            }
        });
    }

    /// 环境自适应：systemd 模式行为不变（与既有 apply_non_interactive 测试同构）。
    /// Windows 上 apply_config 走 process::apply（BinNotConfigured），不适用。
    #[cfg(not(windows))]
    #[test]
    fn apply_systemd_bad_yaml_fails_early() {
        with_settings_dir(|| {
            let rt = tokio::runtime::Runtime::new().unwrap();
            let e = rt
                .block_on(apply_config("bad: [\n", true, RunMode::Systemd))
                .unwrap_err();
            match &e {
                ApplyError::SudoNeedsPassword
                | ApplyError::SudoNotAvailable
                | ApplyError::NotInSudoers
                | ApplyError::CommandFailed { .. } => {}
                other => panic!("意外错误: {other:?}"),
            }
        });
    }

    /// 环境自适应：find_mihomo_in_path 与 which mihomo 一致。
    /// Windows 无 which；幂等分支由 mihomo_installed_flag_matches_fs 之外的其他用例覆盖。
    #[cfg(not(windows))]
    #[tokio::test]
    async fn find_mihomo_in_path_matches_which() {
        let which = tokio::process::Command::new("which")
            .arg("mihomo")
            .output()
            .await
            .unwrap();
        let expected = if which.status.success() {
            Some(String::from_utf8_lossy(&which.stdout).trim().to_string())
        } else {
            None
        };
        assert_eq!(find_mihomo_in_path(), expected);
        if let Some(p) = &expected {
            let m = tokio::fs::metadata(p).await.unwrap();
            use std::os::unix::fs::PermissionsExt;
            assert!(
                m.is_file() && m.permissions().mode() & 0o111 != 0,
                "应返回可执行文件: {p}"
            );
        }
    }

    /// 环境自适应：service_unit_exists 与 systemctl list-unit-files 一致。
    #[cfg(not(windows))]
    #[tokio::test]
    async fn service_unit_exists_matches_systemctl() {
        let out = tokio::process::Command::new("systemctl")
            .args(["list-unit-files", "mihomo.service"])
            .output()
            .await
            .unwrap();
        let expected =
            out.status.success() && String::from_utf8_lossy(&out.stdout).contains("mihomo.service");
        assert_eq!(service_unit_exists().await, expected);
    }

    /// 环境自适应：proc_status 失败分类正确（未装脚本 → 命令不存在或 NotInSudoers）。
    #[tokio::test]
    async fn proc_status_env_adaptive() {
        match proc_status().await {
            Ok(s) => {
                // 已装 mihomo-proc 且免密可用：字段结构合法
                assert!(s.bin.is_none() || s.bin.as_deref().unwrap().starts_with('/'));
                assert!(!s.running);
            }
            Err(
                ApplyError::ProcScriptMissing
                | ApplyError::SudoNotAvailable
                | ApplyError::NotInSudoers
                | ApplyError::CommandFailed { .. }
                | ApplyError::SudoNeedsPassword,
            ) => {}
            Err(other) => panic!("意外错误: {other:?}"),
        }
    }

    /// systemctl 失败分类：polkit 认证类 → SystemdAuthFailed；其余 → CommandFailed。
    #[cfg(not(windows))]
    #[test]
    fn classify_systemctl_failure_rules() {
        let e = classify_systemctl_failure(
            "start",
            "",
            "Failed to start mihomo.service: Interactive authentication required.",
        );
        assert!(matches!(e, ApplyError::SystemdAuthFailed { .. }), "{e}");
        let ApplyError::SystemdAuthFailed { action, stderr } = &e else {
            panic!()
        };
        assert_eq!(action, "start");
        assert!(stderr.contains("Interactive authentication required"));
        assert!(matches!(
            classify_systemctl_failure("stop", "", "Failed to stop mihomo.service: Access denied."),
            ApplyError::SystemdAuthFailed { .. }
        ));
        // pkexec 无桌面代理时的典型报错
        assert!(matches!(
            classify_systemctl_failure(
                "start",
                "",
                "Error creating textual authentication agent: Error opening current controlling terminal"
            ),
            ApplyError::SystemdAuthFailed { .. }
        ));
        assert!(matches!(
            classify_systemctl_failure("restart", "", "no authentication agent found"),
            ApplyError::SystemdAuthFailed { .. }
        ));
        assert!(matches!(
            classify_systemctl_failure("stop", "", "polkit: Not authorized"),
            ApplyError::SystemdAuthFailed { .. }
        ));
        // 非认证类失败：单元不存在等 → CommandFailed
        assert!(matches!(
            classify_systemctl_failure("start", "", "Unit mihomo.service not found."),
            ApplyError::CommandFailed { .. }
        ));
    }

    /// systemctl_control 的真实执行不在单测覆盖（pkexec 会触发桌面 polkit 密码弹窗，
    /// 干扰日常测试）；由 classify_systemctl_failure_rules 覆盖失败分类，
    /// 实际执行链路经手动/端到端验证（设置页按钮）。
    /// 环境自适应：mihomo-proc 未安装时 proc_control/proc_status 直接返回
    /// ProcScriptMissing（明确引导重装，而非 sudo 裸报"找不到命令"）。
    /// Linux 专属（is_proc_script_installed 已门控）。
    #[cfg(not(windows))]
    #[tokio::test]
    async fn proc_ops_report_missing_script_with_guidance() {
        if !is_proc_script_installed().await {
            let e = proc_control(ProcOp::Start).await.unwrap_err();
            assert!(
                matches!(e, ApplyError::ProcScriptMissing),
                "未安装时应返回 ProcScriptMissing: {e:?}"
            );
            let e = proc_status().await.unwrap_err();
            assert!(matches!(e, ApplyError::ProcScriptMissing));
            // 错误文案含重装引导
            assert!(e.to_string().contains("mihomo-proc"));
            assert!(e.to_string().contains("按 i"));
        } else {
            // 已安装环境：不额外断言（正常链路由其他测试覆盖）
            let _ = proc_control(ProcOp::Stop).await;
        }
    }
}
