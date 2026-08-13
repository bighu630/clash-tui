// Worker C: 首装安装器（Linux）
pub mod installer;
// Windows 直接进程管理（无 systemd/sudo 体系，进程模式为唯一运行方式）
#[cfg(windows)]
pub mod process;
