//! Linux systemd 服务管理。

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use super::{ServiceManager, ServiceState};
use crate::util;

/// 单元文件路径。
fn unit_path(name: &str) -> PathBuf {
    PathBuf::from(format!("/etc/systemd/system/{}.service", name))
}

/// 执行命令并返回（退出码，标准输出，标准错误）。
fn run(program: &str, args: &[&str]) -> (i32, String, String) {
    match Command::new(program).args(args).output() {
        Ok(out) => (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).to_string(),
            String::from_utf8_lossy(&out.stderr).to_string(),
        ),
        Err(e) => (-1, String::new(), e.to_string()),
    }
}

/// 要求 root 权限。
fn require_root() -> Result<(), String> {
    let euid = unsafe { libc::geteuid() };
    if euid != 0 {
        Err("需要 root 权限（请使用 sudo 运行）".to_string())
    } else {
        Ok(())
    }
}

/// 单元文件内容。
fn unit_text(mgr: &ServiceManager) -> String {
    format!(
        "[Unit]\n\
Description={display}\n\
After=network.target\n\
\n\
[Service]\n\
Type=simple\n\
WorkingDirectory=\"{base}\"\n\
ExecStart=\"{exe}\" -s\n\
Restart=always\n\
RestartSec=5\n\
# 只杀主进程，避免误杀应用进程（对齐 C# StarAgent 的 KillMode=process）\n\
KillMode=process\n\
# 禁止被 OOM 杀死\n\
OOMScoreAdjust=-1000\n\
LimitNOFILE=65535\n\
\n\
[Install]\n\
WantedBy=multi-user.target\n",
        display = mgr.display,
        base = mgr.base.display(),
        exe = mgr.exe.display()
    )
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

    std::fs::write(&path, unit_text(mgr))
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

/// 重新安装。
pub fn reinstall(mgr: &ServiceManager) -> Result<(), String> {
    let _ = uninstall(mgr, true);
    std::thread::sleep(Duration::from_millis(500));
    install(mgr, true)
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
