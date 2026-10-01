//! Linux systemd 服务管理。

use std::path::PathBuf;
use std::time::{Duration, Instant};

use super::linux::{require_root, run};
use super::{ServiceManager, ServiceState};
use crate::util;

/// 单元文件路径。
fn unit_path(name: &str) -> PathBuf {
    PathBuf::from(format!("/etc/systemd/system/{}.service", name))
}

/// 查询状态。
pub fn query(mgr: &ServiceManager) -> ServiceState {
    if !unit_path(&mgr.name).exists() {
        return ServiceState::NotInstalled;
    }

    let (_, stdout, _) = run("systemctl", &["is-active", &mgr.name]);
    match stdout.trim() {
        "active" => ServiceState::Running,
        "inactive" | "failed" => ServiceState::Stopped,
        "activating" | "deactivating" => ServiceState::Unknown,
        _ => ServiceState::Unknown,
    }
}

/// 等待状态。
/// 查询服务实际注册的程序路径（读单元文件解析 `ExecStart=`；失败返回 `None`）。
pub fn query_installed_exe(mgr: &ServiceManager) -> Option<PathBuf> {
    let text = std::fs::read_to_string(unit_path(&mgr.name)).ok()?;
    super::inits::parse_exec_start(&text)
}

fn wait_state(mgr: &ServiceManager, expected: ServiceState, timeout_ms: u64) -> bool {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    while Instant::now() < deadline {
        if query(mgr) == expected {
            return true;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    query(mgr) == expected
}

/// 安装（`start` 为 true 时安装并启动）。
pub fn install(mgr: &ServiceManager, start: bool) -> Result<(), String> {
    require_root()?;

    let path = unit_path(&mgr.name);
    if path.exists() {
        return Err(format!(
            "服务已存在：{}（可先 -uninstall，或使用 -reinstall）",
            mgr.name
        ));
    }

    std::fs::write(&path, super::inits::systemd_unit_text(mgr))
        .map_err(|e| format!("写入单元文件失败 {}：{}", path.display(), e))?;

    let (code, _, err) = run("systemctl", &["daemon-reload"]);
    if code != 0 {
        return Err(format!("systemctl daemon-reload 失败：{}", err.trim()));
    }

    let (code, _, err) = run("systemctl", &["enable", &mgr.name]);
    if code != 0 {
        return Err(format!("systemctl enable 失败：{}", err.trim()));
    }

    util::log_format("systemd 服务已安装：{}", &[&mgr.name]);

    if start {
        start_service(mgr)?;
    }

    Ok(())
}

/// 卸载（`stop` 为 true 时先停止）。
pub fn uninstall(mgr: &ServiceManager, stop: bool) -> Result<(), String> {
    require_root()?;

    let path = unit_path(&mgr.name);
    if !path.exists() {
        return Ok(());
    }

    if stop {
        let _ = run("systemctl", &["stop", &mgr.name]);
    }
    let _ = run("systemctl", &["disable", &mgr.name]);

    std::fs::remove_file(&path).map_err(|e| format!("删除单元文件失败 {}：{}", path.display(), e))?;

    let (code, _, err) = run("systemctl", &["daemon-reload"]);
    if code != 0 {
        return Err(format!("systemctl daemon-reload 失败：{}", err.trim()));
    }

    util::log_format("systemd 服务已卸载：{}", &[&mgr.name]);
    Ok(())
}

/// 启动服务。
pub fn start(mgr: &ServiceManager) -> Result<(), String> {
    start_service(mgr)
}

fn start_service(mgr: &ServiceManager) -> Result<(), String> {
    if query(mgr) == ServiceState::NotInstalled {
        return Err(format!("服务未安装：{}", mgr.name));
    }
    if query(mgr) == ServiceState::Running {
        return Ok(());
    }

    let (code, _, err) = run("systemctl", &["start", &mgr.name]);
    if code != 0 {
        return Err(format!("systemctl start 失败：{}", err.trim()));
    }

    if wait_state(mgr, ServiceState::Running, 30_000) {
        Ok(())
    } else {
        Err("启动服务超时".to_string())
    }
}

/// 停止服务。
pub fn stop(mgr: &ServiceManager) -> Result<(), String> {
    stop_service(mgr)
}

fn stop_service(mgr: &ServiceManager) -> Result<(), String> {
    if query(mgr) != ServiceState::Running {
        return Ok(());
    }

    let (code, _, err) = run("systemctl", &["stop", &mgr.name]);
    if code != 0 {
        return Err(format!("systemctl stop 失败：{}", err.trim()));
    }

    if wait_state(mgr, ServiceState::Stopped, 30_000) {
        Ok(())
    } else {
        Err("停止服务超时".to_string())
    }
}

/// 重启服务。
pub fn restart(mgr: &ServiceManager) -> Result<(), String> {
    if query(mgr) == ServiceState::NotInstalled {
        return Err(format!("服务未安装：{}", mgr.name));
    }

    let (code, _, err) = run("systemctl", &["restart", &mgr.name]);
    if code != 0 {
        return Err(format!("systemctl restart 失败：{}", err.trim()));
    }

    if wait_state(mgr, ServiceState::Running, 30_000) {
        Ok(())
    } else {
        Err("重启服务超时".to_string())
    }
}
