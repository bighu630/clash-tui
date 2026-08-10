//! 配置应用：`mihomo -t` 预校验（临时文件）→ `sudo [-n] /usr/local/sbin/mihomo-apply`（stdin 喂入）。
//! 失败时把 mihomo/sudo 输出原样反馈给用户。

use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::process::Command;

use crate::core::settings::config_dir;

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn tmp_path(name: &str) -> std::path::PathBuf {
    let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    config_dir().join(format!(".{name}.{}.{n}.tmp", std::process::id()))
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
    tokio::fs::write(&path, yaml)
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
/// non_interactive=true 且 sudo 提示密码 → SudoNeedsPassword。
pub async fn apply_config(yaml: &str, non_interactive: bool) -> Result<ApplyOutcome, ApplyError> {
    let path = tmp_path(APPLY_TMP);
    tokio::fs::write(&path, yaml)
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
            } else if non_interactive && needs_password(&stderr) {
                Err(ApplyError::SudoNeedsPassword)
            } else {
                Err(ApplyError::CommandFailed { stdout, stderr })
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

/// sudo 提示需要密码（兼容中英文 locale）。
fn needs_password(stderr: &str) -> bool {
    let s = stderr.to_lowercase();
    s.contains("password") || s.contains("passwd") || s.contains("密码")
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
    async fn apply_non_interactive_password_or_failure() {
        // 环境自适应：mihomo-apply 未安装时 sudo 非交互应报密码/命令失败
        let e = apply_config("port: 7890\n", true).await.unwrap_err();
        match e {
            ApplyError::SudoNeedsPassword
            | ApplyError::SudoNotAvailable
            | ApplyError::CommandFailed { .. } => {}
            other => panic!("意外错误: {other:?}"),
        }
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
