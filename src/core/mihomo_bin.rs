//! mihomo 二进制路径校验（跨平台）：
//! Linux —— 字符集白名单 + 可执行位（防脚本注入）；Windows —— 盘符/UNC 绝对路径 + 字符集。
//! 两平台共同的硬性门：`<path> -v` 版本探测输出须含 "mihomo"。

use std::path::Path;
use std::process::Command;

/// Windows 绝对路径规则（纯函数）：盘符 `X:\`/`X:/` 或 UNC `\\`。
/// 与 `is_absolute_path` 的 Windows 分支共用，保证 `windows_path_syntax_ok` 跨平台可测。
fn windows_absolute(p: &str) -> bool {
    p.starts_with("\\\\")
        || (p.len() >= 3
            && p.as_bytes()[0].is_ascii_alphabetic()
            && p.as_bytes()[1] == b':'
            && matches!(p.as_bytes()[2], b'\\' | b'/'))
}

/// 绝对路径判定（平台规则）。
/// Linux：以 `/` 开头；Windows：盘符 `X:\`/`X:/` 或 UNC `\\`。
pub fn is_absolute_path(p: &str) -> bool {
    #[cfg(windows)]
    {
        windows_absolute(p)
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
    if !windows_absolute(path) {
        return Err(
            "路径必须为绝对路径（如 C:\\mihomo\\mihomo.exe 或 \\\\server\\share\\mihomo.exe）"
                .to_string(),
        );
    }
    if !path
        .chars()
        .all(|c| !c.is_control() && !matches!(c, '"' | '<' | '>' | '|' | '?' | '*'))
    {
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
        if !path
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '+' | '-' | '/'))
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
        assert!(
            windows_path_syntax_ok("mihomo.exe").is_err(),
            "相对路径应拒绝"
        );
        assert!(
            windows_path_syntax_ok(r"C:mihomo.exe").is_err(),
            "盘符相对路径应拒绝"
        );
        assert!(
            windows_path_syntax_ok("C:\\mihomo\\mihomo.exe\"").is_err(),
            "引号应拒绝"
        );
        assert!(
            windows_path_syntax_ok("C:\\mihomo\\mi*homo.exe").is_err(),
            "星号应拒绝"
        );
        assert!(
            windows_path_syntax_ok("C:\\mihomo\\mihomo.exe\n").is_err(),
            "控制字符应拒绝"
        );
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
        assert!(
            validate_mihomo_bin("usr/bin/mihomo").is_err(),
            "相对路径应拒绝"
        );
    }

    /// 依赖真实文件的 Linux 分支（/usr/bin/mihomo 验收环境已装）。
    #[cfg(not(windows))]
    mod linux_fs_cases {
        use super::*;

        #[test]
        fn validate_charset_and_fs_rules() {
            assert!(
                validate_mihomo_bin("/usr/bin/mihomo; rm -rf /").is_err(),
                "分号应拒绝"
            );
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
