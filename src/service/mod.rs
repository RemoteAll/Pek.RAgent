//! 跨平台服务管理：安装/卸载/启动/停止/重启/状态查询。
//!
//! - **Windows**：SCM。安装用 `sc.exe create`（binPath 指向 `{exe} -s`），运行时由
//!   `windows-service` crate 与 SCM 交互（见 [`windows::run_as_service`]）；
//! - **Linux**：自动探测 init 并分发——**systemd**（主流发行版）、**procd**（OpenWrt，
//!   `/etc/init.d/{name}` + `/etc/rc.common`）、**SysVinit / OpenRC**（`/etc/init.d` +
//!   `update-rc.d` / `chkconfig` / `rc-update`）；对应实现见 systemd / procd / sysv 模块；
//! - **macOS**：launchd `/Library/LaunchDaemons/{name}.plist` + `launchctl`。

use std::path::{Path, PathBuf};

use crate::config::AgentConfig;

#[cfg(windows)]
pub mod windows;

#[cfg(target_os = "linux")]
mod systemd;
#[cfg(target_os = "linux")]
mod procd;
#[cfg(target_os = "linux")]
mod sysv;
#[cfg(target_os = "linux")]
mod linux;

/// Linux init 探测与脚本模板（纯逻辑；测试时也编译，便于在开发机上跑单测）
#[cfg(any(target_os = "linux", test))]
mod inits;

#[cfg(target_os = "macos")]
mod launchd;

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
mod unsupported;

#[cfg(windows)]
use windows as platform;
#[cfg(target_os = "linux")]
use linux as platform;
#[cfg(target_os = "macos")]
use launchd as platform;
#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
use unsupported as platform;

/// 服务状态。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServiceState {
    /// 未安装
    NotInstalled,
    /// 已安装未运行
    Stopped,
    /// 运行中
    Running,
    /// 未知（查询失败等）
    Unknown,
}

impl ServiceState {
    /// 中文文本。
    pub fn text(self) -> &'static str {
        match self {
            ServiceState::NotInstalled => "未安装",
            ServiceState::Stopped => "未启动",
            ServiceState::Running => "运行中",
            ServiceState::Unknown => "未知",
        }
    }
}

/// 平台服务管理器。
pub struct ServiceManager {
    /// 服务名（Windows 服务名 / systemd 单元名 / init 脚本名 / launchd 标签）
    pub name: String,
    /// 显示名
    pub display: String,
    /// 描述（仅 Windows 安装时写入服务描述）
    #[cfg_attr(not(windows), allow(dead_code))]
    pub description: String,
    /// 代理可执行文件
    pub exe: PathBuf,
    /// 基础目录
    pub base: PathBuf,
}

impl ServiceManager {
    /// 按配置创建。
    pub fn new(base: &Path, cfg: &AgentConfig) -> ServiceManager {
        let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("pek-ragent"));
        ServiceManager {
            name: cfg.service_name.clone(),
            display: cfg.display_name.clone(),
            description: cfg.description.clone(),
            exe,
            base: base.to_path_buf(),
        }
    }

    /// 查询状态。
    pub fn query(&self) -> ServiceState {
        platform::query(self)
    }

    /// 服务管理器类型名（`systemd` / `procd` / `SysVinit` / `launchd` / `Windows 服务`），
    /// 用于状态行显示（对齐 C# `-status` 的“状态：systemd ...”风格）。
    pub fn init_name(&self) -> &'static str {
        platform::init_name()
    }

    /// 安装（`start` 为 true 时安装并启动）。
    pub fn install(&self, start: bool) -> Result<(), String> {
        platform::install(self, start)
    }

    /// 查询服务实际注册的程序路径（读取服务注册信息；未安装或读取失败返回 `None`）。
    ///
    /// 与 [`ServiceManager::exe`]（当前进程路径）不同：服务可能安装在其他目录
    /// （如从开发输出目录启动菜单时），用于展示真实安装位置。
    pub fn installed_exe(&self) -> Option<PathBuf> {
        platform::query_installed_exe(self)
    }

    /// 重新安装（先卸载再安装并启动）。
    pub fn reinstall(&self) -> Result<(), String> {
        platform::reinstall(self)
    }

    /// 卸载（`stop` 为 true 时先停止再卸载）。
    pub fn uninstall(&self, stop: bool) -> Result<(), String> {
        platform::uninstall(self, stop)
    }

    /// 启动。
    pub fn start(&self) -> Result<(), String> {
        platform::start(self)
    }

    /// 停止。
    pub fn stop(&self) -> Result<(), String> {
        platform::stop(self)
    }

    /// 重启。
    pub fn restart(&self) -> Result<(), String> {
        platform::restart(self)
    }
}
