//! 配置文件存取：settings.toml（toml）、subscriptions.toml / overrides.toml（YAML 内容）。
//! 所有写入采用「临时文件 + rename」原子替换。目录可用 MIHOMO_TUI_SETTINGS_DIR 覆盖（测试/样例用）。

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::core::models::{NetworkSettings, Overrides, Subscription};

#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    #[error("{0}")]
    Io(String),
    #[error("{0}")]
    Toml(String),
    #[error("{0}")]
    Yaml(String),
}

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
                .map(|p| {
                    PathBuf::from(p)
                        .join("AppData")
                        .join("Roaming")
                        .join("mihomo-tui")
                })
                .unwrap_or_else(|_| PathBuf::from("mihomo-tui"))
        })
}

/// 纯函数：Windows 配置目录 = %APPDATA%\mihomo-tui（跨平台单测；Linux 亦编译但不用）。
pub fn windows_config_dir(appdata: &str) -> PathBuf {
    PathBuf::from(appdata).join("mihomo-tui")
}

pub fn settings_path() -> PathBuf {
    config_dir().join("settings.toml")
}

pub fn subscriptions_path() -> PathBuf {
    config_dir().join("subscriptions.toml")
}

pub fn overrides_path() -> PathBuf {
    config_dir().join("overrides.toml")
}

/// 读取网络设置；文件缺失 → 默认值并落盘。
pub fn load_settings() -> Result<NetworkSettings, SettingsError> {
    let p = settings_path();
    match fs::read_to_string(&p) {
        Ok(s) => toml::from_str(&s).map_err(|e| SettingsError::Toml(e.to_string())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let def = NetworkSettings::default();
            save_settings(&def)?;
            Ok(def)
        }
        Err(e) => Err(SettingsError::Io(e.to_string())),
    }
}

/// 保存网络设置（toml）。
pub fn save_settings(s: &NetworkSettings) -> Result<(), SettingsError> {
    let body = toml::to_string(s).map_err(|e| SettingsError::Toml(e.to_string()))?;
    atomic_write(&settings_path(), body.as_bytes())
}

/// 读取订阅列表；文件缺失 → 空列表。
pub fn load_subscriptions() -> Result<Vec<Subscription>, SettingsError> {
    let p = subscriptions_path();
    match fs::read_to_string(&p) {
        Ok(s) => serde_yaml::from_str(&s).map_err(|e| SettingsError::Yaml(e.to_string())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(SettingsError::Io(e.to_string())),
    }
}

/// 保存订阅列表（文件为 .toml 后缀，内容 YAML——serde_yaml::Value 对 toml 不友好）。
pub fn save_subscriptions(subs: &[Subscription]) -> Result<(), SettingsError> {
    let body = serde_yaml::to_string(subs).map_err(|e| SettingsError::Yaml(e.to_string()))?;
    atomic_write(&subscriptions_path(), body.as_bytes())
}

/// 读取用户覆盖；文件缺失 → 默认。
pub fn load_overrides() -> Result<Overrides, SettingsError> {
    let p = overrides_path();
    match fs::read_to_string(&p) {
        Ok(s) => serde_yaml::from_str(&s).map_err(|e| SettingsError::Yaml(e.to_string())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Overrides::default()),
        Err(e) => Err(SettingsError::Io(e.to_string())),
    }
}

/// 保存用户覆盖（内容 YAML，后缀 .toml）。
pub fn save_overrides(o: &Overrides) -> Result<(), SettingsError> {
    let body = serde_yaml::to_string(o).map_err(|e| SettingsError::Yaml(e.to_string()))?;
    atomic_write(&overrides_path(), body.as_bytes())
}

/// 跨平台随机 16 字节（getrandom）→ 32 hex 小写字符。
pub fn generate_secret() -> String {
    let mut buf = [0u8; 16];
    getrandom::fill(&mut buf).expect("getrandom failed");
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// 临时文件 + rename 原子替换。
fn atomic_write(path: &Path, body: &[u8]) -> Result<(), SettingsError> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(dir).map_err(|e| SettingsError::Io(e.to_string()))?;
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let tmp = dir.join(format!(".{name}.tmp{}", std::process::id()));
    let result = (|| -> std::io::Result<()> {
        let mut f = fs::File::create(&tmp)?;
        // 隐私：配置文件含代理密码，0600（rename 保留临时文件权限）；
        // Windows 由用户目录 ACL 保护，无需收紧
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            f.set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        f.write_all(body)?;
        f.sync_all()?;
        fs::rename(&tmp, path)?;
        Ok(())
    })();
    if let Err(e) = result {
        let _ = fs::remove_file(&tmp);
        return Err(SettingsError::Io(e.to_string()));
    }
    Ok(())
}

/// 设置目录测试串行锁：MIHOMO_TUI_SETTINGS_DIR 是进程级环境变量，
/// 所有依赖它的测试（settings 自身 + dashboard 双写测试）必须共用同一把锁
/// 串行执行，否则并行测试会互相覆盖临时目录造成竞态。
#[cfg(test)]
pub(crate) static SETTINGS_DIR_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 在临时目录下运行（MIHOMO_TUI_SETTINGS_DIR 覆盖），结束后清理。
/// dashboard 等模块的设置持久化测试复用此辅助（与 SETTINGS_DIR_LOCK 配套）。
#[cfg(test)]
pub(crate) fn with_settings_dir<T>(f: impl FnOnce() -> T) -> T {
    let _guard: std::sync::MutexGuard<()> = SETTINGS_DIR_LOCK.lock().unwrap();
    let dir = std::env::temp_dir().join(format!("mihomo-tui-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let old = std::env::var("MIHOMO_TUI_SETTINGS_DIR").ok();
    std::env::set_var("MIHOMO_TUI_SETTINGS_DIR", &dir);
    let r = f();
    match old {
        Some(v) => std::env::set_var("MIHOMO_TUI_SETTINGS_DIR", v),
        None => std::env::remove_var("MIHOMO_TUI_SETTINGS_DIR"),
    }
    let _ = std::fs::remove_dir_all(&dir);
    r
}

/// 兼容别名（settings 自身测试沿用原名）。
#[cfg(test)]
fn with_dir<T>(f: impl FnOnce() -> T) -> T {
    with_settings_dir(f)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::models::{RunMode, SubscriptionCache, TunSettings, UserGroup, UserRule};

    #[test]
    fn settings_roundtrip() {
        with_dir(|| {
            let s = NetworkSettings::default();
            save_settings(&s).unwrap();
            let back = load_settings().unwrap();
            assert_eq!(back.mode, s.mode);
            assert_eq!(back.port, s.port);
            assert_eq!(back.socks_port, s.socks_port);
            assert_eq!(back.mixed_port, s.mixed_port);
            assert_eq!(back.allow_lan, s.allow_lan);
            assert_eq!(back.ipv6, s.ipv6);
            assert_eq!(back.log_level, s.log_level);
            assert_eq!(back.external_controller, s.external_controller);
            assert_eq!(back.secret, s.secret);
            assert_eq!(back.tun.enable, s.tun.enable);
            assert_eq!(back.tun.stack, s.tun.stack);
            assert_eq!(back.tun.auto_route, s.tun.auto_route);
            assert_eq!(back.tun.dns_hijack, s.tun.dns_hijack);
            assert_eq!(back.tun.mtu, s.tun.mtu);
            assert_eq!(back.dns.enable, s.dns.enable);
            assert_eq!(back.dns.listen, s.dns.listen);
            assert_eq!(back.dns.enhanced_mode, s.dns.enhanced_mode);
            assert_eq!(back.dns.fake_ip_range, s.dns.fake_ip_range);
            assert_eq!(back.dns.nameserver, s.dns.nameserver);
            assert_eq!(back.dns.default_nameserver, s.dns.default_nameserver);
            assert_eq!(back.dns.fallback, s.dns.fallback);
            assert_eq!(back.dns.fake_ip_filter, s.dns.fake_ip_filter);
        });
    }

    #[test]
    fn missing_settings_file_returns_default_and_persists() {
        with_dir(|| {
            let s = load_settings().unwrap();
            assert_eq!(s.port, 7890);
            assert_eq!(s.mode, "rule");
            assert!(settings_path().exists());
            // 二次读取与落盘一致
            let back = load_settings().unwrap();
            assert_eq!(back.secret, s.secret);
        });
    }

    #[test]
    fn generate_secret_format() {
        let a = generate_secret();
        let b = generate_secret();
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    #[test]
    fn subscriptions_roundtrip() {
        with_dir(|| {
            let sub = Subscription {
                name: "主订阅".into(),
                url: "https://example.com/sub".into(),
                last_fetch: Some("2026-08-10T12:00:00Z".into()),
                active: true,
                cache: Some(SubscriptionCache {
                    proxies: vec![],
                    proxy_groups: vec![],
                    rules: vec!["GEOIP,CN,DIRECT".into()],
                    fetched_at: "2026-08-10T12:00:00Z".into(),
                }),
            };
            save_subscriptions(std::slice::from_ref(&sub)).unwrap();
            let back = load_subscriptions().unwrap();
            assert_eq!(back, vec![sub]);
        });
    }

    #[test]
    fn missing_subscriptions_is_empty() {
        with_dir(|| {
            assert_eq!(load_subscriptions().unwrap(), Vec::<Subscription>::new());
        });
    }

    #[test]
    fn overrides_roundtrip() {
        with_dir(|| {
            let mut o = Overrides::default();
            o.groups.push(UserGroup {
                name: "测速".into(),
                group_type: "url-test".into(),
                url: String::new(),
                interval: 0,
                tolerance: 0,
                proxies: vec!["n1".into()],
            });
            o.rules.push(UserRule {
                rule_type: "DOMAIN-SUFFIX".into(),
                payload: "example.com".into(),
                target: "测速".into(),
            });
            save_overrides(&o).unwrap();
            let back = load_overrides().unwrap();
            assert_eq!(back, o);
        });
    }

    #[test]
    fn missing_overrides_is_default() {
        with_dir(|| {
            assert_eq!(load_overrides().unwrap(), Overrides::default());
        });
    }

    /// 仪表盘三开关字段（mode/ipv6/tun.enable）落盘往返：save → load 全等。
    /// 这是热切开关双写的持久化契约：任何一次 save 后 load 必须还原全部三个字段。
    #[test]
    fn toggle_fields_roundtrip() {
        with_dir(|| {
            let s = NetworkSettings {
                mode: "global".into(),
                ipv6: true,
                tun: TunSettings {
                    enable: true,
                    ..Default::default()
                },
                run_mode: RunMode::Systemd,
                ..Default::default()
            };
            save_settings(&s).unwrap();
            let back = load_settings().unwrap();
            assert_eq!(back.mode, "global");
            assert!(back.ipv6);
            assert!(back.tun.enable);
        });
    }

    /// settings.toml 被建成目录（异常场景）时 save_settings 必须返回 Err：
    /// 原子写依赖 rename 覆盖目标文件，目标为目录时系统拒绝。
    /// dashboard 的「保存失败」弹窗路径依赖此失败可见性。
    #[test]
    fn save_fails_when_settings_path_is_directory() {
        with_dir(|| {
            std::fs::create_dir_all(settings_path()).unwrap();
            let e = save_settings(&NetworkSettings::default()).unwrap_err();
            assert!(matches!(e, SettingsError::Io(_)), "错误: {e}");
        });
    }

    #[test]
    fn settings_dir_and_files_are_private() {
        with_dir(|| {
            // 配置目录 0700（Windows 无 mode 概念，由用户目录 ACL 保护）
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let dir_mode = fs::metadata(config_dir()).unwrap().permissions().mode() & 0o777;
                assert_eq!(dir_mode, 0o700, "配置目录应 0700");
            }
            // 配置文件 0600（原子写后）
            let s = NetworkSettings::default();
            save_settings(&s).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let file_mode = fs::metadata(settings_path()).unwrap().permissions().mode() & 0o777;
                assert_eq!(file_mode, 0o600, "配置文件应为 0600");
            }
        });
    }

    /// 旧版 settings.toml 无 run_mode 字段 → 加载为 systemd（serde default 兼容）。
    #[test]
    fn legacy_settings_file_defaults_run_mode() {
        with_dir(|| {
            let old = "mode = \"rule\"\nipv6 = false\nallow_lan = false\nport = 7890\nsocks_port = 7891\n\
mixed_port = 7892\nlog_level = \"info\"\nexternal_controller = \"127.0.0.1:9090\"\nsecret = \"1234567890abcdef1234567890abcdef\"\n\
[tun]\nenable = false\nstack = \"mixed\"\nauto_route = true\ndns_hijack = [\"any:53\"]\nmtu = 9000\n\
[dns]\nenable = false\nlisten = \"0.0.0.0:1053\"\nenhanced_mode = \"fake-ip\"\nfake_ip_range = \"198.18.0.1/16\"\n\
nameserver = [\"https://doh.pub/dns-query\"]\ndefault_nameserver = [\"223.5.5.5\"]\nfallback = []\nfake_ip_filter = []\n";
            std::fs::write(settings_path(), old).unwrap();
            let s = load_settings().unwrap();
            assert_eq!(s.run_mode, RunMode::Systemd);
        });
    }

    /// run_mode 持久化往返：save → load 全等。
    #[test]
    fn run_mode_roundtrip() {
        with_dir(|| {
            let s = NetworkSettings {
                run_mode: RunMode::Direct,
                ..NetworkSettings::default()
            };
            save_settings(&s).unwrap();
            let back = load_settings().unwrap();
            assert_eq!(back.run_mode, RunMode::Direct);
            assert!(std::fs::read_to_string(settings_path())
                .unwrap()
                .contains("run_mode = \"direct\""));
        });
    }

    /// 旧 settings.toml 缺失 auto_redirect / auto_detect_interface → auto_redirect 回退为 true，auto_detect_interface 回退为 false
    #[test]
    fn tun_new_fields_legacy_defaults() {
        // 构造缺失两字段的 NetworkSettings TOML（仅 tun 段缺字段）
        let toml_str = "mode = \"rule\"\nipv6 = false\nallow_lan = false\nport = 7890\nsocks_port = 7891\n\
mixed_port = 7892\nlog_level = \"info\"\nexternal_controller = \"127.0.0.1:9090\"\nsecret = \"1234567890abcdef1234567890abcdef\"\nrun_mode = \"systemd\"\nmihomo_bin = \"\"\n\
[tun]\nenable = false\nstack = \"mixed\"\nauto_route = true\ndns_hijack = [\"any:53\"]\nmtu = 9000\n\
[dns]\nenable = true\nlisten = \"0.0.0.0:1053\"\nenhanced_mode = \"fake-ip\"\nfake_ip_range = \"198.18.0.1/16\"\n\
nameserver = [\"https://doh.pub/dns-query\"]\ndefault_nameserver = [\"223.5.5.5\"]\nfallback = [\"tls://dns.alidns.com\"]\nfake_ip_filter = [\"*.lan\"]\n";
        let s: NetworkSettings = toml::from_str(toml_str).unwrap();
        assert!(s.tun.auto_redirect, "旧配置 auto_redirect 应默认为 true");
        assert!(
            !s.tun.auto_detect_interface,
            "旧配置 auto_detect_interface 应默认为 false"
        );
        // 直接对 TunSettings 也验证
        let tun_toml = "enable = false\nstack = \"mixed\"\nauto_route = true\ndns_hijack = [\"any:53\"]\nmtu = 9000\n";
        let t: TunSettings = toml::from_str(tun_toml).unwrap();
        assert!(t.auto_redirect);
        assert!(!t.auto_detect_interface);
    }

    /// roundtrip：设为 true → to_string → from_str → 仍为 true；显式 false 亦往返
    #[test]
    fn tun_new_fields_roundtrip_true() {
        with_dir(|| {
            let s = NetworkSettings {
                tun: TunSettings {
                    auto_redirect: true,
                    auto_detect_interface: true,
                    ..TunSettings::default()
                },
                ..NetworkSettings::default()
            };
            save_settings(&s).unwrap();
            let back = load_settings().unwrap();
            assert!(back.tun.auto_redirect);
            assert!(back.tun.auto_detect_interface);
            // 纯 toml 字符串往返
            let body = toml::to_string(&s).unwrap();
            assert!(body.contains("auto_redirect"));
            assert!(body.contains("auto_detect_interface"));
            let back2: NetworkSettings = toml::from_str(&body).unwrap();
            assert!(back2.tun.auto_redirect);
            assert!(back2.tun.auto_detect_interface);
            // 显式 false 往返
            let s_false = NetworkSettings {
                tun: TunSettings {
                    auto_redirect: false,
                    auto_detect_interface: false,
                    ..TunSettings::default()
                },
                ..NetworkSettings::default()
            };
            let body_f = toml::to_string(&s_false).unwrap();
            let back_f: NetworkSettings = toml::from_str(&body_f).unwrap();
            assert!(!back_f.tun.auto_redirect);
            assert!(!back_f.tun.auto_detect_interface);
        });
    }

    /// Windows 配置目录构造（纯函数，Linux 上也能断言字符串行为）。
    #[test]
    fn windows_config_dir_joins_appdata() {
        let p = windows_config_dir(r"C:\Users\alice\AppData\Roaming");
        assert!(p.ends_with("mihomo-tui"));
        assert!(p
            .to_string_lossy()
            .contains(r"C:\Users\alice\AppData\Roaming"));
    }
}
