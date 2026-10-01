//! 代理宿主：前台/服务运行、守护周期、退出清理。

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

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

    // 3) 守护定时器（状态检查/退避重启）
    let guard = manager.clone();
    let timer = Timer::new(1_000, guard_period as i64, move |_| guard.check_all());
    timer.set_async(true);

    // 3.1) 文件变动监视定时器（快速周期，对齐 C# ServiceController.MonitorPeriod=5s）
    let monitor = manager.clone();
    let reload_timer = Timer::new(1_000, 5_000, move |_| monitor.monitor_files());
    reload_timer.set_async(true);

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
    util::log_info("星尘代理已退出");
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
