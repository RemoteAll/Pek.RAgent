//! 不支持的平台占位实现。

use super::{ServiceManager, ServiceState};

fn unsupported() -> Result<(), String> {
    Err("当前平台暂不支持服务化安装（仅支持 Windows / Linux / macOS）".to_string())
}

/// 查询状态。
pub fn query(_mgr: &ServiceManager) -> ServiceState {
    ServiceState::NotInstalled
}

/// 安装。
pub fn install(_mgr: &ServiceManager, _start: bool) -> Result<(), String> {
    unsupported()
}

/// 重新安装。
pub fn reinstall(_mgr: &ServiceManager) -> Result<(), String> {
    unsupported()
}

/// 卸载。
pub fn uninstall(_mgr: &ServiceManager, _stop: bool) -> Result<(), String> {
    unsupported()
}

/// 启动。
pub fn start(_mgr: &ServiceManager) -> Result<(), String> {
    unsupported()
}

/// 停止。
pub fn stop(_mgr: &ServiceManager) -> Result<(), String> {
    unsupported()
}

/// 重启。
pub fn restart(_mgr: &ServiceManager) -> Result<(), String> {
    unsupported()
}
