//! 配置文件存取：settings.toml（toml）、subscriptions.toml / overrides.toml（YAML 内容）。
//! 所有写入采用「临时文件 + rename」原子替换。目录可用 MIHOMO_TUI_SETTINGS_DIR 覆盖（测试/样例用）。

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
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

/// 配置目录：$HOME/.config/mihomo-tui（不存在则创建，权限 0700——配置含代理密码）。
/// MIHOMO_TUI_SETTINGS_DIR 环境变量可覆盖（测试与 merge_sample 使用）。
pub fn config_dir() -> PathBuf {
    let dir = std::env::var("MIHOMO_TUI_SETTINGS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            PathBuf::from(home).join(".config").join("mihomo-tui")
        });
    let _ = fs::create_dir_all(&dir);
    // 隐私：配置文件含代理密码，目录收紧到仅本人可读
    let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o700));
    dir
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

/// 16 字节 /dev/urandom → 32 hex 字符。
pub fn generate_secret() -> String {
    let mut buf = [0u8; 16];
    let mut f = fs::File::open("/dev/urandom").expect("open /dev/urandom");
    f.read_exact(&mut buf).expect("read /dev/urandom");
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
        // 隐私：配置文件含代理密码，0600（rename 保留临时文件权限）
        f.set_permissions(fs::Permissions::from_mode(0o600))?;
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
    use crate::core::models::{SubscriptionCache, TunSettings, UserGroup, UserRule};

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
            save_subscriptions(&[sub.clone()]).unwrap();
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
            // 配置目录 0700
            let dir_mode = fs::metadata(config_dir()).unwrap().permissions().mode() & 0o777;
            assert_eq!(dir_mode, 0o700, "配置目录应为 0700");
            // 配置文件 0600（原子写后）
            let s = NetworkSettings::default();
            save_settings(&s).unwrap();
            let file_mode = fs::metadata(settings_path()).unwrap().permissions().mode() & 0o777;
            assert_eq!(file_mode, 0o600, "配置文件应为 0600");
        });
    }
}
