//! 代理宿主：前台/服务运行、守护周期、退出清理。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use dhrust::threading::Timer;

use crate::config::AgentConfig;
use crate::manager::AppManager;
use crate::service::ServiceManager;
use crate::util;

/// 全局停止标志（信号 / 服务控制 / 回车退出 共用）。
pub static SHUTDOWN: AtomicBool = AtomicBool::new(false);

/// 代理。
pub struct Agent {
    /// 配置
    pub config: AgentConfig,
    /// 应用管理器
    pub manager: Arc<AppManager>,
    /// 平台服务管理（仅 Windows 服务模式读取服务名）
    #[cfg_attr(not(windows), allow(dead_code))]
    pub svc: ServiceManager,
}

impl Agent {
    /// 启动准备：加载配置、创建管理器与服务管理。
    pub fn boot(base: &Path) -> Agent {
        // 清理上次升级遗留的 .old（运行中被占用时删除失败，下次启动再试）
        if let Ok(exe) = std::env::current_exe() {
            let _ = std::fs::remove_file(format!("{}.old", exe.display()));
        }

        let config = AgentConfig::load(base);
        let manager = AppManager::new(base, config.clone());
        let svc = ServiceManager::new(base, &config);
        Agent {
            config,
            manager,
            svc,
        }
    }

    /// 前台运行（`-run`，等价 C# 的模拟运行）：回车或 Ctrl+C 退出。
    pub fn run_foreground(&self) -> i32 {
        util::log_info("星尘代理启动（前台运行模式）");
        self.install_signal_handlers();

        // 回车退出（菜单“模拟运行”的体验）
        std::thread::spawn(|| {
            use std::io::BufRead;
            let stdin = std::io::stdin();
            for line in stdin.lock().lines() {
                match line {
                    Ok(_) => {
                        SHUTDOWN.store(true, Ordering::SeqCst);
                        break;
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(500)),
                }
            }
        });

        run_core(
            self.manager.clone(),
            self.config.local_port,
            self.config.local_only,
            self.config.guard_period,
        );
        0
    }

    /// 服务方式运行（`-s`）。
    pub fn run_service(&self) -> i32 {
        util::log_info("星尘代理启动（系统服务模式）");
        crate::sys::raise_priority();
        self.install_signal_handlers();

        #[cfg(windows)]
        {
            let manager = self.manager.clone();
            let port = self.config.local_port;
            let local_only = self.config.local_only;
            let period = self.config.guard_period;
            let run = move || {
                run_core(manager.clone(), port, local_only, period);
            };
            crate::service::windows::run_as_service(&self.svc.name, run);
            0
        }

        #[cfg(not(windows))]
        {
            run_core(
                self.manager.clone(),
                self.config.local_port,
                self.config.local_only,
                self.config.guard_period,
            );
            0
        }
    }

    /// 主动关闭（供扩展调用；核心退出路径见 `run_core`）。
    #[allow(dead_code)]
    pub fn shutdown(&self, reason: &str) {
        util::log_format("正在退出：{}", &[reason]);
        self.manager.shutdown(reason);
        util::log_info("星尘代理已退出");
    }

    /// 安装信号处理（SIGTERM/SIGINT / 控制台 Ctrl+C）。
    fn install_signal_handlers(&self) {
        #[cfg(unix)]
        unsafe {
            libc::signal(libc::SIGTERM, on_signal as *const () as usize);
            libc::signal(libc::SIGINT, on_signal as *const () as usize);
        }

        #[cfg(windows)]
        unsafe {
            windows_sys::Win32::System::Console::SetConsoleCtrlHandler(Some(on_console_ctrl), 1);
        }
    }
}

/// 核心运行：拉起应用、启动控制接口、守护循环、等待退出。
fn run_core(manager: Arc<AppManager>, port: u16, local_only: bool, guard_period: u64) {
    // 1) 拉起已启用应用
    manager.start_all();

    // 2) 本地控制接口（线程持有；进程退出即结束）
    let _http = crate::server::start(manager.clone(), port, local_only);

    // 2.1) 本地 UDP RPC 服务端（NewLife ApiClient 协议；DHDeploy 重启/拉起链路依赖）
    let _udp = crate::udp_rpc::start(manager.clone(), port);

    // 3) 守护定时器（状态检查/退避重启）
    let guard = manager.clone();
    let timer = Timer::new(1_000, guard_period as i64, move |_| guard.check_all());
    timer.set_async(true);

    // 3.1) 文件变动监视定时器（快速周期，对齐 C# ServiceController.MonitorPeriod=5s）
    let monitor = manager.clone();
    let reload_timer = Timer::new(1_000, 5_000, move |_| monitor.monitor_files());
    reload_timer.set_async(true);

    // 3.2) 自升级监视（上传 `{exe}.new` 后自动原子替换并退出重拉；Linux 禁止直接覆盖运行中 ELF）
    let up_timer = Timer::new(5_000, 10_000, move |_| check_self_upgrade());
    up_timer.set_async(true);

    util::log_format(
        "守护周期 {} 秒；按 Ctrl+C（前台模式可回车）退出",
        &[&(guard_period / 1000).to_string()],
    );

    // 4) 等待停止信号
    while !SHUTDOWN.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(200));
    }

    // 5) 清理
    manager.shutdown("宿主退出");
    drop(timer);
    drop(reload_timer);
    drop(up_timer);
    util::log_info("星尘代理已退出");
}

/// 自升级检查（守护周期调用）：发现 `{exe}.new` 时自动替换并退出，由服务管理器拉起新版本。
///
/// 背景：Linux 内核禁止写入"正在执行的 ELF"（ETXTBSY），无法像 dotnet dll 那样直接覆盖上传；
/// 约定"新版本上传为 `{exe}.new`"实现同等的运行时替换体验——上传完成后约 10 秒内自动替换并
/// 退出重拉（服务模式）；前台 `-run` 模式退出后需手动重启（日志有提示）。
pub(crate) fn check_self_upgrade() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let exe = util::lexical_normalize(&exe);

    let new_path = PathBuf::from(format!("{}.new", exe.display()));
    if !new_path.exists() {
        return;
    }

    match upgrade_from(&new_path, &exe, 10) {
        Ok(()) => {
            util::log_info(
                "检测到新版本（.new）：程序文件已替换，正在退出以便服务管理器拉起新版本……",
            );
            // 异步文件日志同步落盘后再退出（否则最后一条日志可能在队列中丢失）
            dhrust::logs::flush();
            std::process::exit(0);
        }
        Err(e) => {
            // "上传中"属正常中间态（静默）；其余问题记录以便排查
            if !e.contains("上传中") {
                util::log_error(&format!("自升级检查：{e}"));
            }
        }
    }
}

/// 自升级替换核心（纯逻辑，便于单测）：校验新文件并原子替换目标可执行文件。
///
/// - `new_path`：上传的新版本文件（约定 `{exe}.new`）；
/// - `min_age_secs`：新文件最小静置秒数（防止上传中途触发替换；0 不检查）。
pub(crate) fn upgrade_from(new_path: &Path, exe: &Path, min_age_secs: u64) -> Result<(), String> {
    let meta = std::fs::metadata(new_path).map_err(|e| format!("升级文件不可读：{e}"))?;
    if !meta.is_file() || meta.len() == 0 {
        return Err("升级文件为空".to_string());
    }

    // 上传中保护：文件需静置一段时间（大小/mtime 稳定）
    if min_age_secs > 0 {
        if let Ok(modified) = meta.modified() {
            if let Ok(age) = SystemTime::now().duration_since(modified) {
                if age.as_secs() < min_age_secs {
                    return Err(format!("升级文件上传中（静置 {min_age_secs} 秒后自动替换）"));
                }
            }
        }
    }

    // 形态校验：ELF（Linux/macOS）或 PE（Windows），避免把半截/错误文件换上
    let mut head = [0u8; 4];
    {
        use std::io::Read;
        let mut f = std::fs::File::open(new_path).map_err(|e| format!("升级文件不可读：{e}"))?;
        let _ = f.read(&mut head).map_err(|e| format!("升级文件读取失败：{e}"))?;
    }
    let is_elf = head == [0x7F, b'E', b'L', b'F'];
    let is_pe = head[0] == b'M' && head[1] == b'Z';
    if !is_elf && !is_pe {
        return Err("升级文件格式校验失败（非可执行文件，请确认上传完整）".to_string());
    }

    // 可执行位（Unix；SFTP 上传通常为 644）
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(new_path, std::fs::Permissions::from_mode(0o755));
    }

    // 直接 rename 替换：Unix 下运行中的旧文件 inode 保留给当前进程，此调用必成功
    if std::fs::rename(new_path, exe).is_ok() {
        return Ok(());
    }

    // 兜底（Windows：运行中的 exe 无法被覆盖）——"改名让位"：
    // 旧 exe 改名为 .old（允许），再把新文件改名到正式名；失败则回滚
    let old = PathBuf::from(format!("{}.old", exe.display()));
    let _ = std::fs::remove_file(&old);
    std::fs::rename(exe, &old).map_err(|e| format!("旧程序改名失败：{e}"))?;
    match std::fs::rename(new_path, exe) {
        Ok(()) => {
            // 旧文件尽力清理（运行中删除失败则留待后续清理）
            let _ = std::fs::remove_file(&old);
            Ok(())
        }
        Err(e) => {
            let _ = std::fs::rename(&old, exe);
            Err(format!("替换失败：{e}"))
        }
    }
}

#[cfg(unix)]
extern "C" fn on_signal(_sig: i32) {
    SHUTDOWN.store(true, Ordering::SeqCst);
}
#[cfg(windows)]
unsafe extern "system" fn on_console_ctrl(_ctrl_type: u32) -> i32 {
    SHUTDOWN.store(true, Ordering::SeqCst);
    1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upgrade_from_validates_and_replaces() {
        let dir = std::env::temp_dir().join(format!(
            "ragent-upg-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let exe = dir.join("app");
        let new = dir.join("app.new");
        std::fs::write(&exe, b"old").unwrap();

        // 非可执行格式：拒绝
        std::fs::write(&new, b"not-executable-content").unwrap();
        assert!(upgrade_from(&new, &exe, 0).is_err());

        // 空文件：拒绝
        std::fs::write(&new, b"").unwrap();
        assert!(upgrade_from(&new, &exe, 0).is_err());

        // ELF 头：通过并原子替换
        let mut elf = vec![0x7Fu8, b'E', b'L', b'F'];
        elf.extend_from_slice(&[0u8; 64]);
        std::fs::write(&new, &elf).unwrap();
        upgrade_from(&new, &exe, 0).unwrap();
        assert!(!new.exists(), "新文件应已被消费");
        assert_eq!(std::fs::read(&exe).unwrap(), elf);

        // 静置保护：刚写入的文件在 min_age>0 时拒绝（防上传中途触发）
        std::fs::write(&new, &elf).unwrap();
        assert!(upgrade_from(&new, &exe, 60).is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
