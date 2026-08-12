//! 配置应用：`mihomo -t` 预校验（临时文件）→ `sudo [-n] /usr/local/sbin/mihomo-apply`（stdin 喂入）。
//! 失败时把 mihomo/sudo 输出原样反馈给用户。

use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::core::settings::config_dir;

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn tmp_path(name: &str) -> std::path::PathBuf {
    let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    config_dir().join(format!(".{name}.{}.{n}.tmp", std::process::id()))
}

/// 写临时文件（0600：内容含代理密码/secret，不能按默认 0644 落盘）。
async fn write_secret_file(path: &std::path::Path, body: &str) -> std::io::Result<()> {
    let mut opts = tokio::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true).mode(0o600);
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
    #[error("mihomo -t 校验失败:\n{stderr}")]
    ValidateFailed { stderr: String },
    #[error("执行失败:\n{stdout}\n{stderr}")]
    CommandFailed { stdout: String, stderr: String },
    #[error("{0}")]
    Io(String),
}

const VALIDATE_TMP: &str = "validate";
const APPLY_TMP: &str = "apply";

/// 写临时文件 → `mihomo -t -f` 校验；失败返回 mihomo 原始 stderr。
pub async fn validate_config(yaml: &str) -> Result<(), ApplyError> {
    let path = tmp_path(VALIDATE_TMP);
    write_secret_file(&path, yaml)
        .await
        .map_err(|e| ApplyError::Io(e.to_string()))?;
    let result = run_capture("mihomo", &["-t", "-f", path.to_str().unwrap()], None).await;
    let _ = tokio::fs::remove_file(&path).await;
    match result {
        Ok((status, stdout, stderr)) => {
            if status.success() {
                Ok(())
            } else {
                Err(ApplyError::ValidateFailed {
                    stderr: if stderr.trim().is_empty() { stdout } else { stderr },
                })
            }
        }
        Err(e) => Err(e),
    }
}

/// `sudo [-n] /usr/local/sbin/mihomo-apply`，stdin 喂入 yaml。
/// non_interactive=true 且 sudo 提示密码 → SudoNeedsPassword（交互模式重试）；
/// 非交互且当前用户未被 sudoers 授权 → NotInSudoers（该情形交互重试也必败）。
pub async fn apply_config(yaml: &str, non_interactive: bool) -> Result<ApplyOutcome, ApplyError> {
    let path = tmp_path(APPLY_TMP);
    write_secret_file(&path, yaml)
        .await
        .map_err(|e| ApplyError::Io(e.to_string()))?;
    let mut args = Vec::new();
    if non_interactive {
        args.push("-n");
    }
    args.push("/usr/local/sbin/mihomo-apply");
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

/// /usr/local/sbin/mihomo-apply 是否已安装（存在且可执行）。
pub async fn is_apply_script_installed() -> bool {
    use std::os::unix::fs::PermissionsExt;
    tokio::fs::metadata("/usr/local/sbin/mihomo-apply")
        .await
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// systemctl is-active --quiet mihomo。
pub async fn service_is_active() -> bool {
    Command::new("systemctl")
        .args(["is-active", "--quiet", "mihomo"])
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

/// which mihomo。
pub async fn mihomo_is_installed() -> bool {
    Command::new("which")
        .arg("mihomo")
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

/// 运行命令并捕获输出；stdin 可来自文件。命令不存在 → 按场景映射错误。
async fn run_capture(
    cmd: &str,
    args: &[&str],
    stdin_file: Option<&str>,
) -> Result<(std::process::ExitStatus, String, String), ApplyError> {
    let mut command = Command::new(cmd);
    command.args(args).stdout(Stdio::piped()).stderr(Stdio::piped());
    if let Some(path) = stdin_file {
        // 打开本地文件作为子进程 stdin（快速阻塞操作，可接受）
        let file = std::fs::File::open(path)
            .map_err(|e| ApplyError::Io(e.to_string()))?;
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
    let (out, err, status) = tokio::join!(
        read_all(&mut stdout),
        read_all(&mut stderr),
        child.wait()
    );
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

    #[tokio::test]
    async fn validate_ok_minimal_config() {
        // 本机已装 mihomo（计划验收环境）
        validate_config("port: 7890\n").await.unwrap();
    }

    #[tokio::test]
    async fn validate_bad_yaml_fails_with_stderr() {
        let e = validate_config("port: 7890\nproxies: [\n").await.unwrap_err();
        match e {
            ApplyError::ValidateFailed { stderr } => {
                assert!(!stderr.is_empty(), "mihomo 报错应原样透出");
            }
            other => panic!("期望 ValidateFailed，得到 {other:?}"),
        }
    }

    #[tokio::test]
    async fn apply_non_interactive_password_or_validation_failure() {
        // 环境自适应且无副作用：无效 YAML 在脚本内 mihomo -t 预校验阶段即失败，
        // 不会替换 /etc/mihomo/config.yaml；未装脚本/sudo 需密码/免密/
        // 用户无 sudo 权限（CI/容器）等环境均覆盖。
        let e = apply_config("bad: [\n", true).await.unwrap_err();
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
            if is_apply_script_installed().await {
                let s = stderr.to_lowercase();
                assert!(
                    s.contains("test failed") || s.contains("validation"),
                    "期望脚本校验失败输出，实际 stderr: {stderr}"
                );
            }
        }
    }

    /// 失败分类规则：需密码 / 不在 sudoers / 其他（中英文 locale）。
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
            classify_sudo_failure("alice is not in the sudoers file. This incident will be reported."),
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

    #[tokio::test]
    async fn service_is_active_smoke() {
        let _ = service_is_active().await;
    }
}
