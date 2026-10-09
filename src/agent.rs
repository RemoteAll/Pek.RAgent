//! 代理宿主：前台/服务运行、守护周期、退出清理。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
            // 创建升级目录（Update/）：运维可直接把新版本文件丢进来，热检测自动升级；
            // 目录命名与 Config/Log 保持一致（首字母大写）
            if let Some(dir) = exe.parent() {
                let _ = std::fs::create_dir_all(dir.join("Update"));
            }
        }

        let config = AgentConfig::load(base);
        let manager = AppManager::new(base, config.clone());
        let svc = crate::service::manager(base, &config);
        Agent {
            config,
            manager,
            svc,
        }
    }

    /// 前台运行（`-run`，等价 C# 的模拟运行）：回车或 Ctrl+C 退出。
    pub fn run_foreground(&self) -> i32 {
        util::log_format(
            "星尘代理启动（前台运行模式），当前版本 {}（构建 {}）",
            &[env!("CARGO_PKG_VERSION"), &dhrust::build_time_text!("PEK_RAGENT_BUILD_UNIX")],
        );
        report_takeover_marker();
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
        util::log_format(
            "星尘代理启动（系统服务模式），当前版本 {}（构建 {}）",
            &[env!("CARGO_PKG_VERSION"), &dhrust::build_time_text!("PEK_RAGENT_BUILD_UNIX")],
        );
        report_takeover_marker();
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
            crate::service::windows::run_as_service(&self.svc.name, || { SHUTDOWN.store(true, Ordering::SeqCst); }, run);
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
    // 1) 拉起已启用应用（配置“加载即补齐”由 AgentConfig::load 内部自动完成）
    manager.start_all();

    // 2) 本地控制接口（线程持有；进程退出即结束）
    //    安全提示：允许远程访问且仍用默认密码时明确提醒（默认密码是公开信息；
    //    面板登录后另有常显横幅，见 status 的 defaultPassword 字段）
    if !local_only {
        let cfg = manager.config();
        if crate::webpanel::uses_default_credentials(&cfg) {
            util::log_info(
                "安全提示：Web 面板已允许远程访问（LocalOnly=false）且仍使用默认密码 admin/admin，请尽快修改密码！",
            );
        }
    }
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

    // 3.2) 自升级监视（Web 上传 / `-update` / update 升级目录 / 外部直接替换
    //      → 热检测 + 影子冒烟 + 原子替换 + 移交新版本；Linux 禁止覆盖写运行中 ELF）
    record_exe_identity_baseline();
    let up_mgr = manager.clone();
    let up_timer = Timer::new(5_000, 10_000, move |_| check_self_upgrade(&up_mgr));
    up_timer.set_async(true);

    // 3.3) 心跳日志（每 5 分钟一条）：周期任务正常时也定期可见，便于运维确认代理存活；
    //      有动作的守护/监视/升级事件本身即时记录，不受影响
    let hb = manager.clone();
    let hb_timer = Timer::new(60_000, 300_000, move |_| heartbeat(&hb));
    hb_timer.set_async(true);

    // 3.4) 自动升级检查（Pek.RPanlServer 发行源；tick 粒度 60s，实际频率由
    //      AutoUpgradeIntervalMinutes 决定；未配置发行源时静默跳过）
    let ua = manager.clone();
    let au_timer = Timer::new(45_000, 60_000, move |_| {
        crate::self_upgrade::trigger(ua.config(), false);
    });
    au_timer.set_async(true);

    // 3.5) 平台实时通道（Pek.RPanlServer「服务器节点」；配置接入令牌后启用：
    //      上报机器数据 + 接收「立即检查升级」指令；线程自管理重连，无需定时器）
    crate::panel_ws::start(manager.clone());

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
    crate::terminal::close_all();
    drop(timer);
    drop(reload_timer);
    drop(up_timer);
    drop(hb_timer);
    drop(au_timer);
    util::log_info("星尘代理已退出");
}

/// 自升级检查（守护周期调用）：发现候选升级文件时自动替换并移交新版本——Unix 由旧进程
/// `execv` 原地接管（进程号不变、无服务空窗），Windows 服务交由 SCM 失败恢复自动拉起
/// （不再启动重启助手），前台/极端场景由重启助手兜底。
///
/// 背景：Linux 内核禁止写入"正在执行的 ELF"（ETXTBSY），无法像 dotnet dll 那样直接覆盖上传；
/// 本实现采用"上传 + 热检测 + 影子冒烟 + 原子替换 + 移交新版本"完成运行时升级——上传后约
/// 10 秒内自动完成；Windows 前台 `-run` 模式退出后需手动重启（日志有提示）。
///
/// 候选来源（按优先级）：
/// 0. 程序文件被"直接替换"（先删后传 / mv / 手工改名替换等"能成功落盘"的替换方式）：
///    检测文件身份变化，校验 + 影子冒烟通过后移交新版本（磁盘上已是新版本，无需再替换）；
/// 1. `{exe}.new`：Web 面板上传 / `-update` 命令 / 手工放置的兼容路径；
/// 2. `{exe 目录}/Update/` 升级目录下的任意文件（取最新修改者）：SFTP 手工上传时把
///    新版本文件丢进该目录即可，文件名随意（启动时自动创建；兼容早期小写 `update`）。
pub(crate) fn check_self_upgrade(manager: &Arc<AppManager>) {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let exe = util::lexical_normalize(&exe);
    let exe_dir = exe
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));

    UPGRADE_CHECKS.fetch_add(1, Ordering::Relaxed);

    // 候选零：程序文件被外部直接替换（文件身份变化）
    check_external_replace(&exe, manager);

    // 候选一：`{exe}.new`（Web 上传 / CLI / 兼容约定）
    let dot_new = PathBuf::from(format!("{}.new", exe.display()));
    if dot_new.is_file() {
        try_auto_upgrade(&dot_new, &exe, &[], manager);
        return;
    }

    // 候选二：升级目录 `Update/`：只采用最新修改的一个文件；其余候选在处置后统一标记
    // `.skipped`——无论目录里放了多少文件，都只发生一轮升级（一次重启）
    let mut candidates = collect_update_files(&exe_dir);
    if !candidates.is_empty() {
        let best = candidates.remove(0);
        try_auto_upgrade(&best, &exe, &candidates, manager);
    }
}

/// exe 文件身份指纹（外部替换检测用）。
///
/// - Unix：设备号 + inode（最强信号，`touch`/属性变更不影响）；
/// - Windows：大小 + 创建时间（运行中的 exe 无法被覆盖写；改名替换会刷新创建时间）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct ExeIdentity {
    len: u64,
    #[cfg(unix)]
    dev: u64,
    #[cfg(unix)]
    ino: u64,
    #[cfg(windows)]
    created: Option<SystemTime>,
}

/// 读取文件身份指纹（失败返回 None）。
fn exe_identity(path: &Path) -> Option<ExeIdentity> {
    let meta = std::fs::metadata(path).ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some(ExeIdentity {
            len: meta.len(),
            dev: meta.dev(),
            ino: meta.ino(),
        })
    }
    #[cfg(windows)]
    {
        Some(ExeIdentity {
            len: meta.len(),
            created: meta.created().ok(),
        })
    }
    #[cfg(not(any(unix, windows)))]
    {
        Some(ExeIdentity { len: meta.len() })
    }
}

/// 进程启动时的 exe 身份基线（启动即记录；`check_external_replace` 懒初始化兜底）。
static EXE_IDENTITY: Mutex<Option<ExeIdentity>> = Mutex::new(None);
/// 已校验失败的外部替换文件指纹（len, mtime 秒），避免周期性重复处理。
static EXTERNAL_REJECTED: Mutex<Option<(u64, u64)>> = Mutex::new(None);
/// 自升级检查累计次数（心跳日志展示，便于确认周期任务存活）。
static UPGRADE_CHECKS: AtomicU64 = AtomicU64::new(0);

/// 心跳日志（每 5 分钟）：周期任务正常时也定期可见，便于运维确认代理存活与工作状态。
fn heartbeat(manager: &AppManager) {
    let list = manager.list();
    let running = list.iter().filter(|(_, s)| s.running).count();
    util::log_format(
        "运行中：子服务 {}/{} 运行，自升级检查 {} 次（未发现更新），守护/监视/升级检查正常",
        &[
            &running.to_string(),
            &list.len().to_string(),
            &UPGRADE_CHECKS.load(Ordering::Relaxed).to_string(),
        ],
    );
    // 操作日志保留期清理（每天最多一次；机制下沉 pek_radmin::panel）
    crate::audit::maybe_cleanup(manager.base());
}

/// 记录进程启动时的 exe 身份基线（幂等；供外部替换检测对比）。
fn record_exe_identity_baseline() {
    if let Ok(exe) = std::env::current_exe() {
        let exe = util::lexical_normalize(&exe);
        if let Some(id) = exe_identity(&exe) {
            *EXE_IDENTITY.lock().unwrap() = Some(id);
        }
    }
}

/// 外部替换检测（守护周期调用）：程序文件被"直接替换"（先删后传 / mv / 改名替换）时，
/// 校验 + 影子冒烟通过后自动退出重启（磁盘上已是新版本，无需再替换）。
///
/// 说明：Linux 内核禁止"覆盖写"运行中的 ELF（ETXTBSY），但允许删除/改名（unlink/rename）；
/// 部分 FTP/SFTP/同步工具采用"临时文件 + 重命名"实现上传，或用户手动先删再传——
/// 本检测在这些场景下补齐"替换后自动生效重启"的最后一环。
fn check_external_replace(exe: &Path, manager: &Arc<AppManager>) {
    let Some(identity) = exe_identity(exe) else {
        return;
    };
    {
        let mut slot = EXE_IDENTITY.lock().unwrap();
        match *slot {
            None => {
                *slot = Some(identity);
                return;
            }
            Some(baseline) if baseline == identity => return,
            Some(_) => {}
        }
    }

    // 文件身份已变化：可能刚被替换/仍在写入，需静置后校验
    let stamp = std::fs::metadata(exe)
        .map(|meta| (meta.len(), mtime_secs(&meta)))
        .unwrap_or((0, 0));

    // 校验失败的文件只处理一次（指纹去重），避免周期性日志
    if *EXTERNAL_REJECTED.lock().unwrap() == Some(stamp) {
        return;
    }

    if let Err(e) = validate_upgrade_file(exe, 10) {
        // "上传中/为空"：可能仍在写入，下轮再试（静默）
        if !e.contains("上传中") && !e.contains("为空") {
            *EXTERNAL_REJECTED.lock().unwrap() = Some(stamp);
            util::log_error(&format!(
                "检测到程序文件被替换但校验未通过（继续运行当前版本）：{e}"
            ));
        }
        return;
    }
    if let Err(e) = smoke_test(exe, exe) {
        *EXTERNAL_REJECTED.lock().unwrap() = Some(stamp);
        util::log_error(&format!(
            "检测到程序文件被替换但影子自检未通过（继续运行当前版本）：{e}"
        ));
        return;
    }

    // 通过：磁盘上已是可用的新版本，移交新版本生效（Unix 原地接管 / Windows SCM 拉起）
    util::log_info("检测到程序文件被直接替换（外部升级）：影子自检通过，程序文件已是新版本");
    handoff_to_new_version(exe, manager);
}

/// 文件修改时间（UNIX 秒；不可用时返回 0）。
fn mtime_secs(meta: &std::fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 执行一次自动升级：成功则替换文件并退出（服务管理器拉起新版）；
/// 静置中保留待下轮；失败改名 `*.failed` 留证并停止重试（避免周期性日志刷屏）。
///
/// `skip_list`：同一批次未被采用的其他候选（升级目录里的其余文件）——处置后统一
/// 标记 `.skipped`，保证无论目录里有多少文件都只发生一轮升级（一次重启）。
fn try_auto_upgrade(new_path: &Path, exe: &Path, skip_list: &[PathBuf], manager: &Arc<AppManager>) {
    match apply_upgrade(new_path, exe, 10) {
        Ok(()) => {
            util::log_info("检测到新版本：影子自检通过，程序文件已替换");
            // 其余候选跳过（避免重启后逐个升级造成多轮重启）
            skip_candidates(skip_list);
            // 移交新版本（Unix 原地接管 / Windows 服务由 SCM 拉起 / 其余助手兜底）
            handoff_to_new_version(exe, manager);
        }
        Err(e) => {
            // 正常中间态（静默，下个周期重试）：
            // - "上传中"：文件尚未稳定；
            // - 替换类瞬态错误（Windows 文件占用/共享冲突等）：下个周期通常自愈。
            if e.contains("上传中") || is_transient_upgrade_error(&e) {
                return;
            }
            // 失败留证并停止重试：候选文件改名为 `*.failed`
            let failed = PathBuf::from(format!("{}.failed", new_path.display()));
            let _ = std::fs::remove_file(&failed);
            if std::fs::rename(new_path, &failed).is_ok() {
                util::log_error(&format!(
                    "自升级失败（当前程序不受影响）：{e}；文件已保留为 {} 供排查",
                    failed.display()
                ));
            } else {
                util::log_error(&format!("自升级失败（当前程序不受影响）：{e}"));
            }
            // 其余候选同样跳过：避免失败后逐个重试造成多轮“尝试→重启”
            skip_candidates(skip_list);
        }
    }
}

/// 把未被采用的候选文件标记为 `*.skipped`（保留现场、不再参与自动升级）。
fn skip_candidates(paths: &[PathBuf]) {
    for p in paths {
        let skipped = PathBuf::from(format!("{}.skipped", p.display()));
        let _ = std::fs::remove_file(&skipped);
        let _ = std::fs::rename(p, &skipped);
    }
}

/// 升级收尾（各升级路径共用）：把运行权交给新版本——启动次数最少、停顿最短、日志完整。
///
/// - Unix：`execv` 原地接管（同一进程号直接变为新版本，无服务空窗，不依赖服务管理器）；
/// - Windows 服务（已安装且指向本程序）：交由 SCM 失败恢复自动拉起（升级前静默刷新
///   恢复策略；非零退出码确保被判定为"失败"并触发恢复动作），不再启动重启助手；
/// - 其余场景（Windows 前台 / Unix 极端失败）：重启助手兜底（助手负责等待或显式拉起）。
///
/// 本函数不返回：接管成功时进程映像被替换，其余路径以退出结束。
pub(crate) fn handoff_to_new_version(exe: &Path, manager: &Arc<AppManager>) -> ! {
    // 退出前快照运行状态：子进程随宿主退出继续存活，新实例据此“接管”而非重复拉起
    // （安全网：与 monitor_files 的即时持久化双保险）
    manager.persist_state();

    #[cfg(unix)]
    {
        let err = exec_takeover(exe);
        util::log_error(&format!("原地接管失败，回退重启助手流程：{err}"));
    }

    #[cfg(windows)]
    {
        if let Some(name) = own_service_name(manager, exe) {
            // 刷新失败恢复策略（幂等；防被外部清空导致"退出后无人拉起"）
            reassert_failure_actions(&name);
            util::log_format(
                "程序文件已替换：由服务管理器失败恢复自动拉起新版本（{}，无需重启助手）……",
                &[&name],
            );
            dhrust::logs::flush();
            // 非零退出码：确保服务管理器把本次退出识别为"失败"并触发恢复动作
            std::process::exit(UPGRADE_RESTART_EXIT_CODE);
        }
    }

    // 兜底：重启助手（等待服务管理器拉起，超时显式启动）+ 退出
    schedule_service_restart(exe);
    dhrust::logs::flush();
    std::process::exit(0);
}

/// `execv` 原地接管：用已替换到位的程序文件重建当前进程映像（成功不返回；失败返回原因）。
///
/// 同进程号、无服务空窗：不依赖服务管理器与重启助手；日志连续完整（新版本在同一
/// 日志文件中续写启动记录）。仅 Unix 具备该能力（Windows 无 exec 语义）。
#[cfg(unix)]
fn exec_takeover(exe: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;

    let path = match std::ffi::CString::new(exe.as_os_str().as_bytes()) {
        Ok(p) => p,
        Err(_) => return "程序路径含 NUL 字符".to_string(),
    };
    let argv = match takeover_argv(exe, std::env::args_os()) {
        Ok(a) => a,
        Err(e) => return e,
    };
    let mut argv_ptrs: Vec<*const libc::c_char> = argv.iter().map(|a| a.as_ptr()).collect();
    argv_ptrs.push(std::ptr::null());

    // 交接标记：新版本启动时记录"由旧版原地接管"（启动早期移除，避免被子进程继承）。
    // edition 2024 下环境变量写操作为 unsafe；此处位于升级尾段、即将 exec，竞态无实际影响
    unsafe {
        std::env::set_var("PEK_RAGENT_TAKEOVER_FROM", env!("CARGO_PKG_VERSION"));
    }

    util::log_info("程序文件已替换：正在原地接管新版本（execv 原地重启，进程号不变、服务不中断）……");
    dhrust::logs::flush();

    // 成功：当前进程映像被新版本替换，execv 不返回；返回即失败（errno 见 last_os_error）
    let rc = unsafe { libc::execv(path.as_ptr(), argv_ptrs.as_ptr()) };
    format!("execv 失败（错误码 {rc}）：{}", std::io::Error::last_os_error())
}

/// 构建 execv 的 argv（argv[0] 保留现有命令行；空命令行回退程序路径）。
#[cfg(unix)]
fn takeover_argv<I>(exe: &Path, args: I) -> Result<Vec<std::ffi::CString>, String>
where
    I: IntoIterator<Item = std::ffi::OsString>,
{
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let mut argv: Vec<CString> = Vec::new();
    for arg in args {
        argv.push(CString::new(arg.as_bytes()).map_err(|_| "命令行参数含 NUL 字符".to_string())?);
    }
    if argv.is_empty() {
        argv.push(
            CString::new(exe.as_os_str().as_bytes())
                .map_err(|_| "程序路径含 NUL 字符".to_string())?,
        );
    }
    Ok(argv)
}

/// 升级移交的退出码（非零：确保服务管理器把本次退出识别为"失败"并触发恢复动作；值本身无其他含义）。
#[cfg(windows)]
const UPGRADE_RESTART_EXIT_CODE: i32 = 101;

/// 当前部署的 Windows 服务名（服务已安装且注册程序指向本程序时返回——即"可交由 SCM 恢复拉起"）。
#[cfg(windows)]
fn own_service_name(manager: &Arc<AppManager>, exe: &Path) -> Option<String> {
    let svc = crate::service::manager(manager.base(), &manager.config());
    if svc.query() == crate::service::ServiceState::NotInstalled {
        return None;
    }
    match svc.installed_exe() {
        Some(path) if paths_same(&path, exe) => Some(svc.name.clone()),
        _ => None,
    }
}

/// 路径是否指向同一程序（Windows 大小写不敏感）。
#[cfg(windows)]
fn paths_same(a: &Path, b: &Path) -> bool {
    a.to_string_lossy().eq_ignore_ascii_case(&b.to_string_lossy())
}

/// 升级前刷新 Windows 服务失败恢复策略（与安装时一致；尽力而为、静默）。
#[cfg(windows)]
fn reassert_failure_actions(name: &str) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let _ = std::process::Command::new("sc.exe")
        .args([
            "failure",
            name,
            "reset=",
            "86400",
            "actions=",
            "restart/5000/restart/10000/restart/30000",
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
}

/// 升级标记检查（原地接管由旧版本设置环境变量传递）：启动早期记录并移除，避免被子进程继承。
fn report_takeover_marker() {
    if let Some(from) = std::env::var_os("PEK_RAGENT_TAKEOVER_FROM") {
        // edition 2024 下环境变量写操作为 unsafe；此处为启动最早期，无并发读者
        unsafe {
            std::env::remove_var("PEK_RAGENT_TAKEOVER_FROM");
        }
        let from = from.to_string_lossy().to_string();
        util::log_format("已由 v{} 原地接管升级启动（进程号不变，服务无中断）", &[&from]);
    }
}

/// 安排服务重启助手：从新程序文件启动 `-ensure-running -upgrade` 子进程，
/// 由新版本进程在旧进程退出后确保服务运行。
///
/// 作为**兜底路径**使用（Unix 原地接管与 Windows 服务失败恢复均不经过本助手）：
/// 服务管理器的失败恢复可能延迟较久或次数有限（systemd 受 StartLimit 约束）；
/// 前台模式由助手自行判断跳过。
pub(crate) fn schedule_service_restart(exe: &Path) {
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("-ensure-running").arg("-upgrade");
    if let Some(dir) = exe.parent() {
        cmd.current_dir(dir);
        // 显式指定真实基础目录（防止从影子位置运行时目录错位）
        cmd.env("PEK_RAGENT_BASE", dir);
    }
    // 子进程输出无人消费，统一丢弃避免管道阻塞
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW：避免服务会话中出现控制台窗口
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    match cmd.spawn() {
        Ok(_) => util::log_info(
            "已安排重启助手（-ensure-running -upgrade），服务将在旧进程退出后自动拉起",
        ),
        Err(e) => util::log_error(&format!(
            "安排重启助手失败：{e}（若服务未自动拉起，请手动执行 -start）"
        )),
    }
}

/// 是否为替换阶段的瞬态错误（文件占用/共享冲突等；下个周期重试通常自愈）。
///
/// 这类错误不应触发 `*.failed` 留证（否则会把一次可用升级误判为失败）；
/// 典型场景：Windows 下替换期间仍有进程短暂持有文件（os error 32）。
fn is_transient_upgrade_error(e: &str) -> bool {
    e.starts_with("替换失败") || e.starts_with("旧程序改名失败") || e.contains("os error 32")
}

/// 收集升级目录中的全部候选文件（按修改时间降序：最新在前）。
///
/// 同时覆盖 `Update/`（首选）与早期小写 `update/`；排除 `*.failed` / `*.skipped`
/// 留证文件。供"一次重启"策略使用：只升级最新的一个，其余标记跳过。
pub(crate) fn collect_update_files(exe_dir: &Path) -> Vec<PathBuf> {
    let mut all: Vec<(SystemTime, PathBuf)> = Vec::new();
    let mut seen_dirs: Vec<PathBuf> = Vec::new();
    for dir in [exe_dir.join("Update"), exe_dir.join("update")] {
        // 目录去重：Windows 文件系统大小写不敏感，`Update` 与 `update` 可能是同一目录
        // （canonicalize 会统一大小写与实际路径；失败时退回原路径比较）
        let key = std::fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone());
        if seen_dirs.contains(&key) {
            continue;
        }
        seen_dirs.push(key);
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if !meta.is_file() {
                continue;
            }
            // 排除留证文件（`*.failed`）与已跳过文件（`*.skipped`）
            if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                if ext.eq_ignore_ascii_case("failed") || ext.eq_ignore_ascii_case("skipped") {
                    continue;
                }
            }
            let Ok(modified) = meta.modified() else {
                continue;
            };
            all.push((modified, path));
        }
    }
    all.sort_by(|a, b| b.0.cmp(&a.0));
    all.into_iter().map(|(_, p)| p).collect()
}

/// 统一升级管线：预检 → 影子冒烟（独立进程执行新版自检）→ 原子替换。
///
/// 影子冒烟参考 C# StarAgent 的升级机制（新版先以独立进程启动验证，失败回滚）：
/// 只有新版能正常启动（`-selftest` 通过）才允许替换，失败时当前程序保持不变。
pub(crate) fn apply_upgrade(new_path: &Path, exe: &Path, min_age_secs: u64) -> Result<(), String> {
    validate_upgrade_file(new_path, min_age_secs)?;
    smoke_test(new_path, exe)?;
    upgrade_from(new_path, exe, min_age_secs)
}

/// 升级文件预检：非空、静置时长、可执行形态（ELF/PE）、Unix 可执行位。
fn validate_upgrade_file(new_path: &Path, min_age_secs: u64) -> Result<(), String> {
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

    Ok(())
}

/// 影子冒烟：以独立进程从影子位置执行新版自检（`-selftest`），验证其可以正常启动。
///
/// 注意：通过 `PEK_RAGENT_BASE` 显式指定真实基础目录——影子文件可能位于 `Update/` 等
/// 子目录，否则自检进程会把影子位置当作程序目录，在其下生成 Log/Config 污染目录。
fn smoke_test(new_path: &Path, official_exe: &Path) -> Result<(), String> {
    let mut cmd = std::process::Command::new(new_path);
    cmd.arg("-selftest");
    if let Some(dir) = official_exe.parent() {
        cmd.env("PEK_RAGENT_BASE", dir);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW：避免服务会话中出现控制台窗口
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    match cmd.output() {
        Ok(out) if out.status.success() => Ok(()),
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
            let detail = if !stderr.is_empty() { stderr } else { stdout };
            let code = out.status.code();
            if detail.is_empty() {
                Err(format!("影子自检未通过（退出码 {code:?}）"))
            } else {
                Err(format!("影子自检未通过（退出码 {code:?}）：{detail}"))
            }
        }
        Err(e) => Err(format!("影子自检无法启动：{e}")),
    }
}

/// 自升级替换核心（校验 + 原子替换，不含影子冒烟；纯逻辑便于单测）。
///
/// - `new_path`：新版本文件（Web 上传 / CLI / 升级目录候选）；
/// - `min_age_secs`：新文件最小静置秒数（防止上传中途触发替换；0 不检查）。
pub(crate) fn upgrade_from(new_path: &Path, exe: &Path, min_age_secs: u64) -> Result<(), String> {
    validate_upgrade_file(new_path, min_age_secs)?;

    // 替换（统一原语 `dhrust::io::replace_file`）：Unix 下运行中的旧文件 inode 保留给
    // 当前进程、原地改名必成功；Windows 运行中被占用时自动"改名让位"到 `.old` 再就位，
    // 失败自动回滚
    let old = PathBuf::from(format!("{}.old", exe.display()));
    dhrust::io::replace_file(new_path, exe, &old).map_err(|e| format!("替换失败：{e}"))?;

    // 旧文件尽力清理（运行中删除失败则留待后续清理）
    let _ = std::fs::remove_file(&old);
    Ok(())
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

    #[test]
    fn apply_upgrade_runs_shadow_smoke_before_replace() {
        // 影子冒烟不通过（假 ELF 无法正常启动）：目标文件必须保持不变
        let dir = std::env::temp_dir().join(format!(
            "ragent-smoke-{}-{}",
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
        std::fs::write(&exe, b"old-content").unwrap();

        let mut fake = vec![0x7Fu8, b'E', b'L', b'F'];
        fake.extend_from_slice(&[b'x'; 1024]);
        std::fs::write(&new, &fake).unwrap();

        assert!(
            apply_upgrade(&new, &exe, 0).is_err(),
            "冒烟应拦截无法运行的文件"
        );
        assert_eq!(
            std::fs::read(&exe).unwrap(),
            b"old-content",
            "冒烟失败时不得替换目标文件"
        );
        assert!(new.exists(), "候选文件由调用方决定去留（本管线不消费）");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn latest_update_file_picks_newest_and_skips_failed() {
        let dir = std::env::temp_dir().join(format!(
            "ragent-update-dir-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);

        // 目录不存在 / 空目录：无候选
        assert!(collect_update_files(&dir).is_empty());
        let update = dir.join("Update");
        std::fs::create_dir_all(&update).unwrap();
        assert!(collect_update_files(&dir).is_empty());

        // 取最新修改的文件
        let old = update.join("old.bin");
        std::fs::write(&old, b"old").unwrap();
        set_mtime(&old, -120);
        let fresh = update.join("fresh.bin");
        std::fs::write(&fresh, b"fresh").unwrap();
        assert_eq!(collect_update_files(&dir)[0], fresh);

        // `*.failed` 留证文件不参与
        let failed = update.join("zz.failed");
        std::fs::write(&failed, b"bad").unwrap();
        assert_eq!(collect_update_files(&dir)[0], fresh);

        // `*.skipped` 跳过文件不参与；collect 返回全部候选（最新在前）
        let skipped = update.join("yy.skipped");
        std::fs::write(&skipped, b"skipped").unwrap();
        let all = collect_update_files(&dir);
        assert_eq!(all.len(), 2, "应仅剩 fresh + old 两个候选");
        assert_eq!(all[0], fresh, "最新在前");
        assert_eq!(all[1], old);

        // skip_candidates：未被采用的候选改名 *.skipped，之后不再参与
        skip_candidates(&collect_update_files(&dir));
        assert!(collect_update_files(&dir).is_empty(), "跳过标记后应无候选");
        assert!(update.join("fresh.bin.skipped").exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 将文件修改时间相对当前偏移 `secs` 秒（负数为回拨）。
    fn set_mtime(path: &Path, secs: i64) {
        let now = SystemTime::now();
        let t = if secs >= 0 {
            now + Duration::from_secs(secs as u64)
        } else {
            now - Duration::from_secs((-secs) as u64)
        };
        let f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        f.set_times(std::fs::FileTimes::new().set_modified(t)).unwrap();
    }

    #[test]
    fn transient_upgrade_errors_are_recognized() {
        // 替换类瞬态错误：下个周期重试（不落 `*.failed`）
        assert!(is_transient_upgrade_error(
            "替换失败：另一个程序正在使用此文件，进程无法访问。 (os error 32)"
        ));
        assert!(is_transient_upgrade_error("旧程序改名失败：拒绝访问。 (os error 5)"));
        // 永久错误：格式/冒烟问题（留证停止重试）
        assert!(!is_transient_upgrade_error(
            "升级文件格式校验失败（非可执行文件，请确认上传完整）"
        ));
        assert!(!is_transient_upgrade_error("影子自检未通过（退出码 1）"));
    }

    #[test]
    fn exe_identity_detects_replacement() {
        // 同一文件身份稳定；"先删后传/改名替换"（inode/创建时间变化）后身份应变化
        let dir = std::env::temp_dir().join(format!(
            "ragent-identity-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let exe = dir.join("app");
        std::fs::write(&exe, b"v1").unwrap();
        let first = exe_identity(&exe).unwrap();
        assert_eq!(first, exe_identity(&exe).unwrap(), "同一文件身份应稳定");

        std::fs::remove_file(&exe).unwrap();
        std::fs::write(&exe, b"v2-longer").unwrap();
        assert_ne!(first, exe_identity(&exe).unwrap(), "替换后身份应变化");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn takeover_argv_preserves_command_line() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let exe = PathBuf::from("/opt/agent/pek-ragent");
        // 正常：argv[0] 与参数原样保留（原地接管后新进程续用同一命令行）
        let args: Vec<OsString> = vec!["/opt/agent/pek-ragent".into(), "-s".into()];
        let argv = takeover_argv(&exe, args.into_iter()).unwrap();
        let values: Vec<String> = argv.iter().map(|c| c.to_string_lossy().into_owned()).collect();
        assert_eq!(values, vec!["/opt/agent/pek-ragent", "-s"]);

        // 空命令行：回退为程序路径（保证 argv[0] 存在）
        let argv = takeover_argv(&exe, std::iter::empty::<OsString>()).unwrap();
        assert_eq!(argv.len(), 1);
        assert_eq!(argv[0].to_str().unwrap(), "/opt/agent/pek-ragent");

        // NUL 字符：拒绝（execv 会截断参数）
        let bad = OsString::from_vec(b"a\0b".to_vec());
        assert!(takeover_argv(&exe, vec![bad].into_iter()).is_err());
    }
}
