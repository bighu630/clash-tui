//! 首装检测与提权组件安装（Worker C）。
//!
//! 首次运行时检测系统是否缺少 mihomo 提权应用组件，并在用户确认后安装：
//!
//! - `/usr/local/sbin/mihomo-apply`：内嵌提权脚本（root:root 0755，内容见
//!   `resources/mihomo-apply.sh`，负责 校验 → 原子替换 → 重启 → 健康检查回滚）
//! - `/etc/sudoers.d/99-mihomo`：授权 `%mihomo-admin` 组免密调用提权脚本
//!   （0440，写入后经 `visudo -cf` 校验，失败即报错并提示手动修复）
//! - `mihomo-admin` 系统组，并将当前用户加入该组
//!
//! 所有 sudo 调用均为阻塞式 `std::process::Command`（不带 `-n`，交互式输入密码），
//! 安装器由 app 在恢复终端后调用（见计划 §3 apply 交互流程），避免 TUI raw 模式
//! 与 sudo 密码提示互相干扰。sudoers 写入一律走 `sudo tee`，避免 shell 注入。

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output, Stdio};

/// 提权脚本安装路径（sudoers 授权即针对此路径）
pub const APPLY_SCRIPT: &str = "/usr/local/sbin/mihomo-apply";
/// sudoers 规则文件路径
pub const SUDOERS_FILE: &str = "/etc/sudoers.d/99-mihomo";
/// 提权授权组名
pub const ADMIN_GROUP: &str = "mihomo-admin";

/// 生成 sudoers 规则行：组内成员免密以 root 执行提权脚本（内容与安装路径强绑定）。
pub fn sudoers_line() -> String {
    format!("%{ADMIN_GROUP} ALL=(root) NOPASSWD: {APPLY_SCRIPT}\n")
}

/// 首次安装检测：提权脚本存在且可执行、sudoers 文件存在 → 已安装，返回 false。
pub async fn needs_install() -> bool {
    !(apply_script_ok() && Path::new(SUDOERS_FILE).is_file())
}

/// 提权脚本是否存在且可执行（等价 `test -x`）。
fn apply_script_ok() -> bool {
    Path::new(APPLY_SCRIPT)
        .metadata()
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// 安装错误。所有变体均携带可直接展示给用户的完整信息。
#[derive(Debug, thiserror::Error)]
pub enum InstallError {
    /// mihomo 二进制缺失（步骤 a）
    #[error("未找到 mihomo，请先安装：pacman -S mihomo（其他发行版请使用对应包管理器）")]
    MihomoNotFound,
    /// mihomo systemd 服务单元缺失（步骤 b）
    #[error(
        "未检测到 mihomo.service 单元（systemctl list-unit-files 中不存在）。\n\
         请参照 README「手动安装」创建 /etc/systemd/system/mihomo.service，然后执行：\n\
         sudo systemctl daemon-reload"
    )]
    ServiceUnitMissing,
    /// sudo 命令不可用或无法启动
    #[error("sudo 不可用：{0}")]
    SudoNotAvailable(String),
    /// 命令执行失败（退出码非 0），消息含具体命令与完整输出
    #[error("命令执行失败：{cmd}\n{output}")]
    CommandFailed { cmd: String, output: String },
    /// visudo 校验失败，需要手动修复
    #[error(
        "visudo 校验 /etc/sudoers.d/99-mihomo 失败：\n{output}\n\
         请手动修复后重试：sudo visudo -cf /etc/sudoers.d/99-mihomo"
    )]
    SudoersInvalid { output: String },
    /// 其他错误
    #[error("{0}")]
    Other(String),
}

/// 执行安装，返回日志行数组（供 UI 展示）；失败返回带具体命令输出的错误。
///
/// 步骤：mihomo 二进制 → systemd 单元 → mihomo-admin 组 → 提权脚本 →
/// sudoers 规则 → 当前用户入组 → 打印启用服务的提示（不自动执行，由用户决定）。
pub async fn install() -> Result<Vec<String>, InstallError> {
    let mut logs: Vec<String> = Vec::new();
    logs.push("开始安装 mihomo 提权组件（后续步骤需要 sudo 密码）…".to_string());

    // a. 检查 mihomo 二进制
    if !mihomo_exists() {
        return Err(InstallError::MihomoNotFound);
    }
    logs.push("[1/6] 已检测到 mihomo 二进制".to_string());

    // b. 检查 systemd 服务单元
    if !service_unit_exists() {
        return Err(InstallError::ServiceUnitMissing);
    }
    logs.push("[2/6] 已检测到 mihomo.service 系统单元".to_string());

    // c. 确保 mihomo-admin 系统组存在
    ensure_group(&mut logs)?;

    // d. 安装提权脚本
    install_apply_script(&mut logs)?;

    // e. 写入 sudoers 规则并校验
    install_sudoers(&mut logs)?;

    // f. 将当前用户加入 mihomo-admin 组
    add_user_to_group(&mut logs)?;

    // g. 询问式 enable：不自动执行，仅给出提示（UI 侧可在安装成功后询问用户）
    logs.push("安装完成！启用服务（也可在 TUI 内确认后执行）：".to_string());
    logs.push("  sudo systemctl enable --now mihomo".to_string());
    // 组成员资格在新会话才生效：未生效时醒目提示重新登录，
    // 避免用户困惑"为什么每次应用配置都要 sudo 密码"。
    if session_has_admin_group() {
        logs.push("✓ 当前会话已在 mihomo-admin 组内，sudo -n 免密调用已生效".to_string());
    } else {
        logs.push(String::new());
        logs.push(
            "⚠ 重要：请重新登录终端（或执行 newgrp mihomo-admin）使 mihomo-admin 组权限生效，\
             否则应用配置时仍会要求输入 sudo 密码"
                .to_string(),
        );
    }
    Ok(logs)
}

/// `which mihomo`：二进制是否存在。
fn mihomo_exists() -> bool {
    Command::new("which")
        .arg("mihomo")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// `systemctl list-unit-files mihomo.service` 输出中是否含 mihomo.service。
fn service_unit_exists() -> bool {
    Command::new("systemctl")
        .args(["list-unit-files", "mihomo.service"])
        .output()
        .map(|o| {
            o.status.success() && String::from_utf8_lossy(&o.stdout).contains("mihomo.service")
        })
        .unwrap_or(false)
}

/// 执行 sudo 命令（交互式，stdin 继承终端以便输入密码），返回完整输出。
fn run_sudo(cmd: &str, args: &[&str]) -> Result<Output, InstallError> {
    Command::new("sudo")
        .arg(cmd)
        .args(args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| InstallError::SudoNotAvailable(e.to_string()))
}

/// `sudo tee <path>`：以 stdin 喂入内容写文件（避免 shell 重定向/注入）。
fn sudo_tee(path: &str, content: &str) -> Result<Output, InstallError> {
    use std::io::Write;

    let mut child = Command::new("sudo")
        .arg("tee")
        .arg(path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| InstallError::SudoNotAvailable(e.to_string()))?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(content.as_bytes());
        let _ = stdin.flush();
    }
    child
        .wait_with_output()
        .map_err(|e| InstallError::SudoNotAvailable(e.to_string()))
}

/// 检查命令输出：成功 → Ok；失败 → CommandFailed（含具体命令与完整输出）。
fn check_output(out: Output, cmd: &str) -> Result<(), InstallError> {
    if out.status.success() {
        return Ok(());
    }
    let output = format!(
        "退出码: {}\nstdout:\n{}\nstderr:\n{}",
        out.status
            .code()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "信号终止".to_string()),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    Err(InstallError::CommandFailed {
        cmd: cmd.to_string(),
        output,
    })
}

/// c. 确保 `mihomo-admin` 系统组存在（不存在则 `sudo groupadd --system`）。
fn ensure_group(logs: &mut Vec<String>) -> Result<(), InstallError> {
    let exists = Command::new("getent")
        .args(["group", ADMIN_GROUP])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if exists {
        logs.push("[3/6] 系统组 mihomo-admin 已存在".to_string());
        return Ok(());
    }
    let out = run_sudo("groupadd", &["--system", ADMIN_GROUP])?;
    check_output(out, "sudo groupadd --system mihomo-admin")?;
    logs.push("[3/6] 已创建系统组 mihomo-admin".to_string());
    Ok(())
}

/// d. 安装提权脚本 `/usr/local/sbin/mihomo-apply`（root:root 0755）。
fn install_apply_script(logs: &mut Vec<String>) -> Result<(), InstallError> {
    let script = include_str!("../../resources/mihomo-apply.sh");
    let out = sudo_tee(APPLY_SCRIPT, script)?;
    check_output(out, &format!("sudo tee {APPLY_SCRIPT}"))?;

    let out = run_sudo("chown", &["root:root", APPLY_SCRIPT])?;
    check_output(out, &format!("sudo chown root:root {APPLY_SCRIPT}"))?;

    let out = run_sudo("chmod", &["755", APPLY_SCRIPT])?;
    check_output(out, &format!("sudo chmod 755 {APPLY_SCRIPT}"))?;

    logs.push("[4/6] 已安装提权脚本 /usr/local/sbin/mihomo-apply（root:root 0755）".to_string());
    Ok(())
}

/// e. 写入 sudoers 规则并校验（`sudo tee` 写入 → 0440 → `visudo -cf`）。
fn install_sudoers(logs: &mut Vec<String>) -> Result<(), InstallError> {
    let out = sudo_tee(SUDOERS_FILE, &sudoers_line())?;
    check_output(out, &format!("sudo tee {SUDOERS_FILE}"))?;

    let out = run_sudo("chmod", &["0440", SUDOERS_FILE])?;
    check_output(out, &format!("sudo chmod 0440 {SUDOERS_FILE}"))?;

    let out = run_sudo("visudo", &["-cf", SUDOERS_FILE])?;
    if !out.status.success() {
        return Err(InstallError::SudoersInvalid {
            output: format!(
                "退出码: {}\nstdout:\n{}\nstderr:\n{}",
                out.status
                    .code()
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "信号终止".to_string()),
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr),
            ),
        });
    }
    logs.push("[5/6] 已写入 /etc/sudoers.d/99-mihomo（0440）并通过 visudo -cf 校验".to_string());
    Ok(())
}

/// f. 将当前用户加入 `mihomo-admin` 组（`sudo usermod -aG`）。
fn add_user_to_group(logs: &mut Vec<String>) -> Result<(), InstallError> {
    let user = current_user();
    if user.is_empty() {
        return Err(InstallError::Other(
            "无法确定当前用户名（$USER 未设置且 id -un 失败）".to_string(),
        ));
    }
    let out = run_sudo("usermod", &["-aG", ADMIN_GROUP, &user])?;
    check_output(out, &format!("sudo usermod -aG {ADMIN_GROUP} {user}"))?;
    logs.push(format!("[6/6] 已将当前用户 {user} 加入 mihomo-admin 组"));
    Ok(())
}

/// 当前会话是否已在 `mihomo-admin` 组内（`id -nG` 解析组名列表）。
/// 组成员资格在重新登录后才在会话中生效，安装后调用以决定是否提示重登。
pub fn session_has_admin_group() -> bool {
    Command::new("id")
        .args(["-nG"])
        .output()
        .map(|o| {
            o.status.success()
                && groups_contain(&String::from_utf8_lossy(&o.stdout), ADMIN_GROUP)
        })
        .unwrap_or(false)
}

/// `id -nG` 输出中是否包含指定组名（按空白切分，精确匹配）。
fn groups_contain(output: &str, name: &str) -> bool {
    output.split_whitespace().any(|g| g == name)
}

/// 当前用户名：优先 `$USER`，回退 `id -un`。
fn current_user() -> String {
    if let Ok(u) = std::env::var("USER") {
        if !u.is_empty() {
            return u;
        }
    }
    Command::new("id")
        .arg("-un")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 内嵌提权脚本必须非空且包含关键逻辑行（校验/重启/回滚）。
    #[test]
    fn apply_script_is_embedded_and_complete() {
        let script = include_str!("../../resources/mihomo-apply.sh");
        assert!(!script.trim().is_empty());
        assert!(script.contains("systemctl restart mihomo"));
        assert!(script.contains("mihomo -t -f"));
        assert!(script.contains("rolling back"));
    }

    /// sudoers 规则行必须与安装路径、授权组强一致。
    #[test]
    fn sudoers_line_matches_paths() {
        let line = sudoers_line();
        assert_eq!(
            line,
            "%mihomo-admin ALL=(root) NOPASSWD: /usr/local/sbin/mihomo-apply\n"
        );
        assert!(line.starts_with(&format!("%{ADMIN_GROUP} ")));
        assert!(line.contains(APPLY_SCRIPT));
        assert!(line.ends_with('\n'));
    }

    /// groups_contain：按空白切分精确匹配组名（子串不算匹配）。
    #[test]
    fn groups_contain_matches_whitespace_separated_names() {
        assert!(groups_contain("root wheel mihomo-admin", "mihomo-admin"));
        assert!(groups_contain("mihomo-admin", "mihomo-admin"));
        assert!(!groups_contain("root wheel users", "mihomo-admin"));
        // 子串不匹配（mihomo-adminx 不是 mihomo-admin）
        assert!(!groups_contain("mihomo-adminx wheel", "mihomo-admin"));
        // 多空格/制表符分隔
        assert!(groups_contain("root   wheel\tmihomo-admin", "mihomo-admin"));
        // 空串
        assert!(!groups_contain("", "mihomo-admin"));
    }

    /// 环境自适应：session_has_admin_group 应与手动解析 `id -nG` 输出一致。
    #[test]
    fn session_has_admin_group_matches_id() {
        let manual = Command::new("id")
            .args(["-nG"])
            .output()
            .ok()
            .map(|o| {
                o.status.success()
                    && groups_contain(&String::from_utf8_lossy(&o.stdout), ADMIN_GROUP)
            })
            .unwrap_or(false);
        assert_eq!(session_has_admin_group(), manual);
    }
}
