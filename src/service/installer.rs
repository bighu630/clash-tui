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
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use crate::core::settings::config_dir;

/// 提权脚本安装路径（sudoers 授权即针对此路径）
pub const APPLY_SCRIPT: &str = "/usr/local/sbin/mihomo-apply";
/// sudoers 规则文件路径
pub const SUDOERS_FILE: &str = "/etc/sudoers.d/99-mihomo";
/// 提权授权组名
pub const ADMIN_GROUP: &str = "mihomo-admin";
/// 本地安装标记文件名（位于 TUI 配置目录；安装成功后写入，供无特权启动检测使用）。
pub const INSTALL_MARKER: &str = "installed.marker";

/// 安装标记路径：TUI 配置目录下（MIHOMO_TUI_SETTINGS_DIR 可覆盖）。
pub fn installed_marker_path() -> PathBuf {
    config_dir().join(INSTALL_MARKER)
}

/// 生成 sudoers 规则行：组内成员免密以 root 执行提权脚本（内容与安装路径强绑定）。
pub fn sudoers_line() -> String {
    format!("%{ADMIN_GROUP} ALL=(root) NOPASSWD: {APPLY_SCRIPT}\n")
}

/// 首次安装检测：提权脚本存在（系统侧）或本地安装标记存在（本机侧）→ 已安装，返回 false。
///
/// 注意：刻意不 stat /etc/sudoers.d/99-mihomo——该目录为 root:root 0750，
/// 普通用户（TUI 的运行者）stat 必然 EACCES，会导致已安装环境每次启动误报
/// "首次安装"（历史 bug）。启动检测也不触发任何 sudo 命令。
pub async fn needs_install() -> bool {
    !is_installed()
}

/// 已安装判定：提权脚本可执行 或 本地安装标记存在，任一满足即视为已安装。
pub fn is_installed() -> bool {
    is_installed_with(Path::new(APPLY_SCRIPT), &installed_marker_path())
}

/// 判定核心（可测）：注入脚本路径与标记路径。
fn is_installed_with(apply_script: &Path, marker: &Path) -> bool {
    apply_script_ok_at(apply_script) || marker.is_file()
}

/// 提权脚本是否存在且可执行（等价 `test -x`）。
fn apply_script_ok_at(path: &Path) -> bool {
    path.metadata()
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// 写入本地安装标记（内容：版本号；检测仅看存在性）。配置目录由 config_dir() 创建（0700）。
fn write_install_marker() -> Result<(), InstallError> {
    let path = installed_marker_path();
    let content = format!("installed-by mihomo-tui {}\n", env!("CARGO_PKG_VERSION"));
    std::fs::write(&path, content)
        .and_then(|_| std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)))
        .map_err(|e| InstallError::Other(format!("写入安装标记 {} 失败: {e}", path.display())))
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

    // h. 本地安装标记：安装成功后写入，供无特权启动检测识别（不因标记失败而中断安装）。
    match write_install_marker() {
        Ok(()) => logs.push("✓ 已写入本地安装标记（启动检测将识别为已安装）".to_string()),
        Err(e) => logs.push(format!(
            "⚠ 无法写入本地安装标记（系统侧组件已装好，不影响使用）：{e}"
        )),
    }

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
            o.status.success() && groups_contain(&String::from_utf8_lossy(&o.stdout), ADMIN_GROUP)
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

    /// 临时目录辅助：每个测试独立子目录（并行测试互不干扰），测完删除。
    fn test_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "mihomo-tui-installer-test-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn cleanup_dir(dir: &std::path::Path) {
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 创建可执行脚本（0755）。
    fn make_executable_script(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    /// 提权脚本存在且可执行、无本地标记 → 已安装（系统侧组件齐备）。
    #[test]
    fn is_installed_with_apply_script_only() {
        let dir = test_dir("apply-script-only");
        let script = make_executable_script(&dir, "mihomo-apply");
        assert!(is_installed_with(&script, &dir.join("installed.marker")));
        cleanup_dir(&dir);
    }

    /// 回归核心：本地安装标记存在（无需系统侧任何组件）→ 已安装。
    /// 模拟本机场景：脚本在、sudoers 目录对普通用户不可 stat（EACCES），
    /// 检测不得依赖 sudoers 文件可见性。
    #[test]
    fn is_installed_with_marker_only() {
        let dir = test_dir("marker-only");
        let script = dir.join("mihomo-apply");
        let marker = dir.join("installed.marker");
        std::fs::write(&marker, "installed-by mihomo-tui test\n").unwrap();
        assert!(is_installed_with(&script, &marker));
        cleanup_dir(&dir);
    }

    /// 两者皆无 → 未安装。
    #[test]
    fn is_installed_false_when_nothing_present() {
        let dir = test_dir("nothing");
        assert!(!is_installed_with(
            &dir.join("mihomo-apply"),
            &dir.join("installed.marker")
        ));
        cleanup_dir(&dir);
    }

    /// 脚本存在但无执行位（0644）→ 不算已安装。
    #[test]
    fn is_installed_false_when_script_not_executable() {
        let dir = test_dir("not-executable");
        let script = dir.join("mihomo-apply");
        std::fs::write(&script, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(!is_installed_with(&script, &dir.join("installed.marker")));
        cleanup_dir(&dir);
    }

    /// 写标记：内容含版本号、权限 0600，写后 is_installed_with 立即识别。
    /// 与 settings 测试共用 SETTINGS_DIR_LOCK（with_settings_dir），勿自行改 env。
    #[test]
    fn write_install_marker_persists_and_detected() {
        crate::core::settings::with_settings_dir(|| {
            write_install_marker().unwrap();
            let marker = installed_marker_path();
            assert!(marker.is_file(), "标记文件应已写入: {}", marker.display());
            let mode = std::fs::metadata(&marker).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "标记文件权限应为 0600");
            let content = std::fs::read_to_string(&marker).unwrap();
            assert!(
                content.starts_with(&format!(
                    "installed-by mihomo-tui {}",
                    env!("CARGO_PKG_VERSION")
                )),
                "标记内容应含版本号: {content}"
            );
            // 无提权脚本时，仅凭标记即判定已安装
            assert!(is_installed_with(
                Path::new("/nonexistent/mihomo-apply"),
                &marker
            ));
        });
    }

    /// 真实机器回归（环境自适应，无条件跳过逻辑）：本机若已安装提权脚本，
    /// 检测必须识别为已安装；标记路径用固定不存在的路径，不触碰 env 变量。
    #[test]
    fn real_machine_apply_script_detected() {
        if Path::new(APPLY_SCRIPT).exists() {
            assert!(apply_script_ok_at(Path::new(APPLY_SCRIPT)));
            assert!(is_installed_with(
                Path::new(APPLY_SCRIPT),
                Path::new("/nonexistent/installed.marker")
            ));
        }
    }
}
