//! 平台相关：进程启停、存活/内存/名称查询、机器信息、优先級与 OOM 分值。
//!
//! 设计要点：
//! - 启动的子进程默认重定向到空设备，避免服务模式无控制台时输出异常；
//!   应用调试输出（`Debug=true`）追加到 `Log/app-{Name}.log`；
//! - 停止先温和（Unix SIGTERM / Windows taskkill），超时后强制（SIGKILL / taskkill /F）；
//! - 内存读取：Linux `/proc/{pid}/statm`、Windows `GetProcessMemoryInfo`、macOS `ps`。

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// 子进程句柄：自己拉起的持有 `Child`；接管来的只记 pid（如代理重启后继续守护）。
pub enum Handle {
    /// 自己拉起的进程
    Owned(Child),
    /// 接管（或外部）进程
    Adopted(u32),
}

impl Handle {
    /// 是否已退出（不阻塞）。
    pub fn has_exited(&mut self) -> bool {
        match self {
            Handle::Owned(c) => matches!(c.try_wait(), Ok(Some(_))),
            Handle::Adopted(p) => !is_alive(*p),
        }
    }
}

/// 进程启动请求。
pub struct SpawnRequest<'a> {
    /// 可执行程序（含 PATH 命令，如 dotnet/java）
    pub program: &'a str,
    /// 参数
    pub args: &'a [String],
    /// 工作目录
    pub cwd: &'a Path,
    /// 环境变量
    pub envs: &'a [(String, String)],
    /// 调试输出文件（追加）。为 None 时输出到空设备
    pub log_file: Option<&'a Path>,
    /// 独立会话/分离进程。用于一次性拉起后父进程立即退出的场景（zip 发布）
    pub detached: bool,
}

/// 拉起进程。
pub fn spawn(req: &SpawnRequest) -> std::io::Result<Child> {
    let mut cmd = Command::new(req.program);
    cmd.args(req.args);
    cmd.current_dir(req.cwd);
    for (k, v) in req.envs {
        cmd.env(k, v);
    }

    match req.log_file {
        Some(file) => {
            if let Some(parent) = file.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let out = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(file)?;
            let err = out.try_clone()?;
            cmd.stdin(Stdio::null()).stdout(Stdio::from(out)).stderr(Stdio::from(err));
        }
        None => {
            cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        }
    }

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;

        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const DETACHED_PROCESS: u32 = 0x0000_0008;

        let mut flags = CREATE_NEW_PROCESS_GROUP;
        if req.detached {
            flags |= DETACHED_PROCESS;
        }
        cmd.creation_flags(flags);
    }

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;

        let detached = req.detached;
        unsafe {
            cmd.pre_exec(move || {
                // 独立会话：避免随宿主进程组收到终端信号，也便于一次性拉起的应用继续运行
                libc::setsid();
                Ok(())
            });
            let _ = detached;
        }
    }

    cmd.spawn()
}

/// 进程是否存活。
#[cfg(windows)]
pub fn is_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    // STILL_ACTIVE：进程仍在运行。
    // 注意：已终止但句柄未关闭的“僵尸”进程 OpenProcess 依然成功，必须检查退出码，
    // 否则会误判为存活（曾导致停止操作等待超时并错误返回失败）。
    const STILL_ACTIVE: u32 = 259;

    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return false;
        }

        let mut code: u32 = 0;
        let ok = GetExitCodeProcess(h, &mut code);
        CloseHandle(h);

        ok != 0 && code == STILL_ACTIVE
    }
}

/// 进程是否存活。
#[cfg(unix)]
pub fn is_alive(pid: u32) -> bool {
    unsafe {
        if libc::kill(pid as libc::pid_t, 0) == 0 {
            return true;
        }
        std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
}

/// 发送温和停止信号（Unix SIGTERM / Windows taskkill）。
pub fn signal_graceful(pid: u32) {
    if pid == 0 {
        return;
    }

    #[cfg(unix)]
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGTERM);
    }

    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// 强制结束进程（Unix SIGKILL / Windows taskkill /F）。
pub fn signal_force(pid: u32) {
    if pid == 0 {
        return;
    }

    #[cfg(unix)]
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGKILL);
    }

    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/F"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// 停止进程：先温和后强制。返回是否已退出。
/// 用于“接管”的进程（没有子进程句柄）；自有子进程请用 `Child::try_wait` 回收。
pub fn stop_process(pid: u32, timeout_ms: u64) -> bool {
    if pid == 0 || !is_alive(pid) {
        return true;
    }

    signal_graceful(pid);

    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    while Instant::now() < deadline {
        if !is_alive(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    signal_force(pid);

    let deadline = Instant::now() + Duration::from_millis(2_000);
    while Instant::now() < deadline {
        if !is_alive(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    !is_alive(pid)
}

/// 进程名（含扩展名，如 `app.exe`/`dotnet`）。尽力而为，取不到返回 None。
#[cfg(windows)]
pub fn process_name(pid: u32) -> Option<String> {
    let pid_s = pid.to_string();
    let text = run_capture(
        "tasklist",
        &["/FI", &format!("PID eq {}", pid_s), "/FO", "CSV", "/NH"],
    )?;

    for line in text.lines() {
        let line = line.trim();
        if !line.starts_with('"') {
            continue;
        }
        let name = line.split(',').next()?.trim().trim_matches('"');
        if !name.is_empty() {
            return Some(name.to_string());
        }
    }

    None
}

/// 进程名。
#[cfg(target_os = "linux")]
pub fn process_name(pid: u32) -> Option<String> {
    let text = std::fs::read_to_string(format!("/proc/{}/comm", pid)).ok()?;
    let name = text.trim();
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

/// 进程名。
#[cfg(target_os = "macos")]
pub fn process_name(pid: u32) -> Option<String> {
    let text = run_capture("ps", &["-o", "comm=", "-p", &pid.to_string()])?;
    let name = text.trim();
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

/// 进程私有内存（MB）。取不到返回 None。
#[cfg(windows)]
pub fn memory_mb(pid: u32) -> Option<u64> {
    use std::mem::size_of;
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::ProcessStatus::{
        GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};

    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return None;
        }

        let mut counters: PROCESS_MEMORY_COUNTERS = std::mem::zeroed();
        counters.cb = size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
        let ok = GetProcessMemoryInfo(h, &mut counters, counters.cb);
        CloseHandle(h);

        if ok == 0 {
            None
        } else {
            // PagefileUsage 即进程已提交私有内存（与 C# PrivateMemorySize64 语义一致）
            Some(counters.PagefileUsage as u64 / 1024 / 1024)
        }
    }
}

/// 进程常驻内存（MB）。
#[cfg(target_os = "linux")]
pub fn memory_mb(pid: u32) -> Option<u64> {
    let text = std::fs::read_to_string(format!("/proc/{}/statm", pid)).ok()?;
    let resident_pages: u64 = text.split_whitespace().nth(1)?.parse().ok()?;

    let mut page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if page_size <= 0 {
        page_size = 4096;
    }

    Some(resident_pages * page_size as u64 / 1024 / 1024)
}

/// 进程常驻内存（MB）。
#[cfg(target_os = "macos")]
pub fn memory_mb(pid: u32) -> Option<u64> {
    let text = run_capture("ps", &["-o", "rss=", "-p", &pid.to_string()])?;
    let kb: u64 = text.trim().parse().ok()?;
    Some(kb / 1024)
}

/// 设置 OOM 分值（仅 Linux；尽力而为）。
#[cfg(target_os = "linux")]
pub fn set_oom_score_adjust(pid: u32, value: i32) {
    let _ = std::fs::write(format!("/proc/{}/oom_score_adj", pid), value.to_string());
}

/// 设置 OOM 分值（非 Linux 平台为空操作）。
#[cfg(not(target_os = "linux"))]
pub fn set_oom_score_adjust(_pid: u32, _value: i32) {}

/// 提高当前进程优先级（服务模式下确保代理能有效管控各应用进程）。
pub fn raise_priority() {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Threading::{
            GetCurrentProcess, SetPriorityClass, ABOVE_NORMAL_PRIORITY_CLASS,
        };
        unsafe {
            SetPriorityClass(GetCurrentProcess(), ABOVE_NORMAL_PRIORITY_CLASS);
        }
    }

    #[cfg(unix)]
    unsafe {
        libc::setpriority(libc::PRIO_PROCESS, 0, -5);
    }
}

/// 执行外部命令并捕获标准输出（失败返回 None；Linux 下读取 /proc，无调用方）。
#[cfg_attr(target_os = "linux", allow(dead_code))]
fn run_capture(program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .ok()?;
    Some(String::from_utf8_lossy(&output.stdout).to_string())
}

/// 机器信息文本（用于 `-ShowMachineInfo`）。
///
/// 信息面对齐 C# `ShowMachineInfo`（MachineInfo + 网络接口 + 磁盘列表）；
/// 星尘节点/心跳字段（NodeInfo/PingInfo）待对接星尘服务端后补充。
pub fn machine_info() -> String {
    let mut text = String::new();

    // —— 基本信息 ——
    text.push_str(&format!(
        "系统：{} {}\n",
        os_description(),
        std::env::consts::ARCH
    ));

    let host = hostname();
    let user = user_name();
    if !host.is_empty() && !user.is_empty() {
        text.push_str(&format!("主机：{host}  用户：{user}\n"));
    } else if !host.is_empty() {
        text.push_str(&format!("主机：{host}\n"));
    }

    let cpus = std::thread::available_parallelism()
        .map(|e| e.get())
        .unwrap_or(0);
    match cpu_model() {
        Some(model) => text.push_str(&format!("处理器：{model}（{cpus} 逻辑核心）\n")),
        None => text.push_str(&format!("处理器：{cpus} 核心\n")),
    }

    if let Some((total, avail)) = memory_info() {
        if total > 0 && avail > 0 {
            let used_pct = (total - avail) as f64 * 100.0 / total as f64;
            text.push_str(&format!(
                "内存：{}（可用 {}，已用 {used_pct:.1}%）\n",
                format_gmk(total),
                format_gmk(avail)
            ));
        } else {
            text.push_str(&format!("内存：{}\n", format_gmk(total)));
        }
    }

    if let Some(uptime) = uptime_text() {
        text.push_str(&format!("启动：已运行 {uptime}\n"));
    }

    // —— 程序与目录 ——
    if let Ok(exe) = std::env::current_exe() {
        text.push_str(&format!(
            "程序：{} v{}\n",
            exe.display(),
            env!("CARGO_PKG_VERSION")
        ));
        if let Some(parent) = exe.parent() {
            text.push_str(&format!("基础目录：{}\n", parent.display()));
        }
    }
    text.push_str(&format!("临时目录：{}\n", std::env::temp_dir().display()));

    if let Some(ip) = dhrust::net::my_ip() {
        text.push_str(&format!("本机IP：{ip}\n"));
    }

    // —— 网络接口（对齐 C# `ShowMachineInfo`：排除回环/虚拟网卡） ——
    let nets = network_interfaces();
    if !nets.is_empty() {
        text.push('\n');
        text.push_str(&format!("网络接口（{}）：\n", nets.len()));
        for net in &nets {
            let desc = if net.description.is_empty() {
                net.name.clone()
            } else {
                format!("{}  {}", net.name, net.description)
            };
            text.push_str(&format!(
                "  {}  {}\n",
                desc,
                if net.up { "已连接" } else { "未连接" }
            ));
            if net.speed_mbps > 0 {
                text.push_str(&format!("    速率：{} Mbps\n", net.speed_mbps));
            }
            if !net.mac.is_empty() {
                text.push_str(&format!("    MAC：{}\n", net.mac));
            }
            if !net.ips.is_empty() {
                text.push_str(&format!("    IP：{}\n", net.ips.join(", ")));
            }
            if !net.gateways.is_empty() {
                text.push_str(&format!("    网关：{}\n", net.gateways.join(", ")));
            }
            if !net.dns.is_empty() {
                text.push_str(&format!("    DNS：{}\n", net.dns.join(", ")));
            }
        }
    }

    // —— 磁盘列表（对齐 C#：全量枚举并标注类型） ——
    let disks = disks();
    if !disks.is_empty() {
        text.push('\n');
        text.push_str("磁盘：\n");
        for d in &disks {
            if d.ready {
                text.push_str(&format!("  {}  {}  {}", d.name, d.kind, d.format));
                if !d.label.is_empty() {
                    text.push_str(&format!("  \"{}\"", d.label));
                }
                text.push_str(&format!(
                    "  {}（可用 {}）\n",
                    format_gmk(d.total),
                    format_gmk(d.free)
                ));
            } else {
                text.push_str(&format!("  {}  {}  [未就绪]\n", d.name, d.kind));
            }
        }
    }

    text
}

// ————— Web 面板数据辅助 —————

/// 进程条目（Web 面板 Top 列表）。
pub(crate) struct ProcItem {
    /// 进程名（不含 .exe 后缀）
    pub(crate) name: String,
    /// 进程 ID
    pub(crate) pid: u32,
    /// 内存（MB）
    pub(crate) memory_mb: u64,
    /// 线程数
    pub(crate) threads: u32,
    /// CPU 时间（秒；内核 + 用户，累计值，与 C# `TotalProcessorTime` 语义一致）
    pub(crate) cpu_seconds: f64,
}

/// 进程统计（线程数、句柄数）。取不到返回 None。
pub(crate) fn process_stats(pid: u32) -> Option<(u32, u32)> {
    #[cfg(windows)]
    {
        use std::mem::zeroed;
        use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
            TH32CS_SNAPPROCESS,
        };
        use windows_sys::Win32::System::Threading::{
            GetProcessHandleCount, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };

        unsafe {
            let mut threads = 0u32;
            let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
            if snapshot != INVALID_HANDLE_VALUE && !snapshot.is_null() {
                let mut entry: PROCESSENTRY32W = zeroed();
                entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
                if Process32FirstW(snapshot, &mut entry) != 0 {
                    loop {
                        if entry.th32ProcessID == pid {
                            threads = entry.cntThreads;
                            break;
                        }
                        if Process32NextW(snapshot, &mut entry) == 0 {
                            break;
                        }
                    }
                }
                CloseHandle(snapshot);
            }

            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if h.is_null() {
                return Some((threads, 0));
            }
            let mut handles = 0u32;
            let ok = GetProcessHandleCount(h, &mut handles);
            CloseHandle(h);

            Some((threads, if ok != 0 { handles } else { 0 }))
        }
    }

    #[cfg(target_os = "linux")]
    {
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
        let threads = status
            .lines()
            .find_map(|l| l.strip_prefix("Threads:"))
            .and_then(|v| v.trim().parse::<u32>().ok())
            .unwrap_or(0);
        let handles = std::fs::read_dir(format!("/proc/{pid}/fd"))
            .map(|it| it.count() as u32)
            .unwrap_or(0);
        Some((threads, handles))
    }

    #[cfg(not(any(windows, target_os = "linux")))]
    {
        let _ = pid;
        None
    }
}

/// 上次 CPU 采样 `(空转+等待 ticks, 总 ticks, 上次速率)`——请求间差分。
static CPU_LAST: Mutex<Option<(u64, u64, f64)>> = Mutex::new(None);

/// 由两次采样差值计算使用率（0~100）。
///
/// 口径对齐 psutil/宝塔：`busy = Δ总时 − Δ(idle + iowait)`；字段回退（负增量）按 0 处理
/// （与 top/psutil 一致）；窗口内总时无变化（同 tick 内重复调用）返回 None。
fn cpu_rate_from_delta(prev: (u64, u64), cur: (u64, u64)) -> Option<f64> {
    let dtotal = cur.1.saturating_sub(prev.1);
    if dtotal == 0 {
        return None;
    }
    let didle = cur.0.saturating_sub(prev.0);
    Some(((1.0 - didle as f64 / dtotal as f64) * 100.0).clamp(0.0, 100.0))
}

/// 解析 `/proc/stat` 首行，返回 `(idle + iowait, 总时)`（ticks）。
///
/// guest/guest_nice 已计入 user/nice，总时需扣除（psutil、htop 同口径）。
#[cfg(any(target_os = "linux", test))]
fn parse_proc_stat_first_cpu(text: &str) -> Option<(u64, u64)> {
    let line = text.lines().find(|l| l.starts_with("cpu "))?;
    let values: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .filter_map(|v| v.parse().ok())
        .collect();
    if values.len() < 4 {
        return None;
    }
    let idle = values[3] + values.get(4).copied().unwrap_or(0);
    let guest = values.get(8).copied().unwrap_or(0) + values.get(9).copied().unwrap_or(0);
    let total = values.iter().sum::<u64>().saturating_sub(guest);
    Some((idle, total))
}

/// 系统 CPU 使用率（0~100）。
///
/// 口径对齐 psutil/宝塔：`使用率 = busy / (busy + idle + iowait)`（Linux 扣除 guest 重复计数）；
/// 采样窗口 = 与上次调用之间（面板 3 秒刷新 → 3 秒窗口均值），首次调用以 200ms 双采样建立基线。
pub(crate) fn system_cpu_rate() -> Option<f64> {
    #[cfg(windows)]
    let sample = || -> Option<(u64, u64)> {
        use windows_sys::Win32::Foundation::FILETIME;
        use windows_sys::Win32::System::Threading::GetSystemTimes;

        fn value(t: FILETIME) -> u64 {
            ((t.dwHighDateTime as u64) << 32) | t.dwLowDateTime as u64
        }

        // 返回 (空闲 100ns, 总忙 100ns)；Windows 的内核时间已包含空闲时间
        unsafe {
            let (mut idle, mut kernel, mut user): (FILETIME, FILETIME, FILETIME) =
                (std::mem::zeroed(), std::mem::zeroed(), std::mem::zeroed());
            if GetSystemTimes(&mut idle, &mut kernel, &mut user) == 0 {
                return None;
            }
            Some((value(idle), value(kernel) + value(user)))
        }
    };

    #[cfg(target_os = "linux")]
    let sample = || -> Option<(u64, u64)> {
        let text = std::fs::read_to_string("/proc/stat").ok()?;
        parse_proc_stat_first_cpu(&text)
    };

    #[cfg(not(any(windows, target_os = "linux")))]
    let sample = || -> Option<(u64, u64)> { None };

    let mut slot = CPU_LAST.lock().unwrap();
    if let Some((prev_idle, prev_total, prev_rate)) = *slot {
        let (idle, total) = sample()?;
        let rate = cpu_rate_from_delta((prev_idle, prev_total), (idle, total)).unwrap_or(prev_rate);
        *slot = Some((idle, total, rate));
        return Some(rate);
    }

    // 首次调用：以 200ms 双采样建立基线，后续按请求间隔差分
    let first = sample()?;
    std::thread::sleep(Duration::from_millis(200));
    let second = sample()?;
    let rate = cpu_rate_from_delta(first, second).unwrap_or(0.0);
    *slot = Some((second.0, second.1, rate));
    Some(rate)
}

/// 机器唯一标识（Windows 注册表 `MachineGuid`；Linux `/etc/machine-id`）。
pub(crate) fn machine_guid() -> Option<String> {
    static CACHE: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    CACHE.get_or_init(detect_machine_guid).clone()
}

/// 检测机器唯一标识（进程生命周期内不变，结果缓存）。
fn detect_machine_guid() -> Option<String> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::NO_ERROR;
        use windows_sys::Win32::System::Registry::{
            HKEY, HKEY_LOCAL_MACHINE, KEY_READ, RegCloseKey, RegOpenKeyExW, RegQueryValueExW,
        };

        fn wide(text: &str) -> Vec<u16> {
            text.encode_utf16().chain(std::iter::once(0)).collect()
        }

        let sub = wide("SOFTWARE\\Microsoft\\Cryptography");
        let value = wide("MachineGuid");
        unsafe {
            let mut hkey: HKEY = std::ptr::null_mut();
            if RegOpenKeyExW(HKEY_LOCAL_MACHINE, sub.as_ptr(), 0, KEY_READ, &mut hkey) != NO_ERROR {
                return None;
            }

            let mut size = 0u32;
            let mut kind = 0u32;
            let mut guid = None;
            if RegQueryValueExW(
                hkey,
                value.as_ptr(),
                std::ptr::null(),
                &mut kind,
                std::ptr::null_mut(),
                &mut size,
            ) == NO_ERROR
                && size > 2
            {
                let mut buf = vec![0u8; size as usize];
                if RegQueryValueExW(
                    hkey,
                    value.as_ptr(),
                    std::ptr::null(),
                    &mut kind,
                    buf.as_mut_ptr(),
                    &mut size,
                ) == NO_ERROR
                {
                    let wide_text: &[u16] =
                        std::slice::from_raw_parts(buf.as_ptr() as *const u16, size as usize / 2);
                    let end = wide_text
                        .iter()
                        .position(|&c| c == 0)
                        .unwrap_or(wide_text.len());
                    let text = String::from_utf16_lossy(&wide_text[..end]);
                    if !text.trim().is_empty() {
                        guid = Some(text.trim().to_string());
                    }
                }
            }
            RegCloseKey(hkey);
            guid
        }
    }

    #[cfg(target_os = "linux")]
    {
        let text = std::fs::read_to_string("/etc/machine-id").ok()?;
        let text = text.trim();
        if text.is_empty() {
            None
        } else {
            Some(text.to_string())
        }
    }

    #[cfg(not(any(windows, target_os = "linux")))]
    {
        None
    }
}

/// 系统运行时长（秒）。
pub(crate) fn host_uptime_seconds() -> u64 {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::SystemInformation::GetTickCount64;
        unsafe { GetTickCount64() / 1000 }
    }

    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string("/proc/uptime")
            .ok()
            .and_then(|t| t.split_whitespace().next()?.parse::<f64>().ok())
            .map(|v| v as u64)
            .unwrap_or(0)
    }

    #[cfg(not(any(windows, target_os = "linux")))]
    {
        0
    }
}

/// 拆分毫秒时间戳为（整秒、纳秒）；负数按欧几里得取整（用于 Unix `timespec`）。
pub(crate) fn split_epoch_ms(epoch_ms: i64) -> (i64, u32) {
    (
        epoch_ms.div_euclid(1000),
        (epoch_ms.rem_euclid(1000) * 1_000_000) as u32,
    )
}

/// 设置系统 UTC 时间（毫秒时间戳；Web 面板“同步时间”按钮，以浏览器时间为准）。
///
/// 只校正时钟、不改时区；需要相应权限：
/// - Unix：root（`clock_settime(CLOCK_REALTIME)`，非 root 返回明确提示）；
/// - Windows：服务账户/管理员（`SetSystemTime` 需要 `SeSystemtimePrivilege`，此处临时启用）。
pub(crate) fn set_system_time(epoch_ms: i64) -> Result<(), String> {
    #[cfg(unix)]
    {
        let (secs, nanos) = split_epoch_ms(epoch_ms);
        // 64 位目标上 `time_t` 恒为 i64（= c_long）：直接赋 i64，
        // 避免引用 musl 目标上已被标记弃用的 `libc::time_t` 别名
        let ts = libc::timespec {
            tv_sec: secs,
            tv_nsec: nanos as libc::c_long,
        };
        let rc = unsafe { libc::clock_settime(libc::CLOCK_REALTIME, &ts) };
        if rc != 0 {
            let error = std::io::Error::last_os_error();
            return Err(match error.raw_os_error() {
                Some(libc::EPERM) => {
                    "权限不足：同步系统时间需要 root（请以 root 运行代理）".to_string()
                }
                Some(libc::EINVAL) => "时间超出内核允许范围".to_string(),
                _ => format!("设置系统时间失败：{error}"),
            });
        }
        Ok(())
    }

    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::HANDLE;
        use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_NOT_ALL_ASSIGNED};
        use windows_sys::Win32::Security::{
            AdjustTokenPrivileges, LookupPrivilegeValueW, SE_PRIVILEGE_ENABLED,
            TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY,
        };
        use windows_sys::Win32::System::SystemInformation::SetSystemTime;
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

        // SetSystemTime 依赖 SE_SYSTEMTIME_NAME 特权（管理员/系统账户持有但默认禁用）
        unsafe {
            let mut token: HANDLE = std::ptr::null_mut();
            if OpenProcessToken(
                GetCurrentProcess(),
                TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
                &mut token,
            ) == 0
            {
                return Err(format!(
                    "打开进程令牌失败：{}",
                    std::io::Error::last_os_error()
                ));
            }

            let privilege: Vec<u16> = "SeSystemtimePrivilege\0".encode_utf16().collect();
            let mut luid = windows_sys::Win32::Foundation::LUID {
                LowPart: 0,
                HighPart: 0,
            };
            let looked_up =
                LookupPrivilegeValueW(std::ptr::null(), privilege.as_ptr(), &mut luid) != 0;

            let mut state = TOKEN_PRIVILEGES {
                PrivilegeCount: 1,
                Privileges: [windows_sys::Win32::Security::LUID_AND_ATTRIBUTES {
                    Luid: luid,
                    Attributes: SE_PRIVILEGE_ENABLED,
                }],
            };
            let adjusted = looked_up
                && AdjustTokenPrivileges(
                    token,
                    0,
                    &mut state,
                    0,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                ) != 0;
            // AdjustTokenPrivileges 即便返回成功，也可能因未持有特权而什么都未启用
            let last = GetLastError();
            CloseHandle(token);

            if !adjusted || last == ERROR_NOT_ALL_ASSIGNED {
                return Err("权限不足：同步系统时间需要管理员/服务账户权限".to_string());
            }
        }

        let Some(system_time) = epoch_ms_to_systemtime(epoch_ms) else {
            return Err("时间戳超出可表示范围".to_string());
        };
        if unsafe { SetSystemTime(&system_time) } == 0 {
            return Err(format!(
                "设置系统时间失败：{}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(())
    }

    #[cfg(not(any(unix, windows)))]
    {
        let _ = epoch_ms;
        Err("当前平台不支持同步系统时间".to_string())
    }
}

/// 毫秒时间戳 → UTC `SYSTEMTIME`（Windows `SetSystemTime` 入参）。
#[cfg(windows)]
fn epoch_ms_to_systemtime(epoch_ms: i64) -> Option<windows_sys::Win32::Foundation::SYSTEMTIME> {
    use chrono::{Datelike, Timelike};
    use windows_sys::Win32::Foundation::SYSTEMTIME;

    let (secs, nanos) = split_epoch_ms(epoch_ms);
    let dt = chrono::DateTime::from_timestamp(secs, nanos)?;
    Some(SYSTEMTIME {
        wYear: dt.year() as u16,
        wMonth: dt.month() as u16,
        wDayOfWeek: dt.weekday().num_days_from_sunday() as u16,
        wDay: dt.day() as u16,
        wHour: dt.hour() as u16,
        wMinute: dt.minute() as u16,
        wSecond: dt.second() as u16,
        wMilliseconds: dt.timestamp_subsec_millis() as u16,
    })
}

/// 系统负载（1/5/15 分钟平均值）。Windows 无此概念，返回 `None`。
#[cfg(target_os = "linux")]
pub(crate) fn load_average() -> Option<(f64, f64, f64)> {
    let text = std::fs::read_to_string("/proc/loadavg").ok()?;
    let mut it = text.split_whitespace();
    let l1 = it.next()?.parse().ok()?;
    let l5 = it.next()?.parse().ok()?;
    let l15 = it.next()?.parse().ok()?;
    Some((l1, l5, l15))
}

/// 系统负载（1/5/15 分钟平均值）。Windows 无此概念，返回 `None`。
#[cfg(not(target_os = "linux"))]
pub(crate) fn load_average() -> Option<(f64, f64, f64)> {
    None
}

/// 是否存在指定进程名的进程（大小写不敏感、忽略 `.exe` 后缀；看门狗用）。
pub(crate) fn is_process_running(name: &str) -> bool {
    let name = name.trim().trim_end_matches(".exe");
    if name.is_empty() {
        return false;
    }

    #[cfg(windows)]
    {
        let mut found = false;
        for_each_process(|pname, _pid, _threads| {
            if pname.eq_ignore_ascii_case(name) {
                found = true;
            }
        });
        found
    }

    #[cfg(target_os = "linux")]
    {
        let Ok(dir) = std::fs::read_dir("/proc") else {
            return false;
        };
        for entry in dir.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let Ok(comm) = std::fs::read_to_string(path.join("comm")) else {
                continue;
            };
            if comm.trim().eq_ignore_ascii_case(name) {
                return true;
            }
        }
        false
    }

    #[cfg(not(any(windows, target_os = "linux")))]
    {
        false
    }
}

/// Top 进程列表（按内存或 CPU 时间降序；取前 `count` 个）。
pub(crate) fn top_processes(count: usize, sort_by_cpu: bool) -> Vec<ProcItem> {
    let mut items: Vec<ProcItem> = Vec::new();

    #[cfg(windows)]
    {
        for_each_process(|pname, pid, threads| {
            items.push(ProcItem {
                name: pname.to_string(),
                pid,
                memory_mb: memory_mb(pid).unwrap_or(0),
                threads,
                cpu_seconds: process_cpu_split(pid).map(|(total, _, _)| total).unwrap_or(0.0),
            });
        });
    }

    #[cfg(target_os = "linux")]
    {
        use std::sync::OnceLock;
        static PAGE_SIZE: OnceLock<u64> = OnceLock::new();
        let page = *PAGE_SIZE.get_or_init(|| {
            let mut size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
            if size <= 0 {
                size = 4096;
            }
            size as u64
        });

        if let Ok(dir) = std::fs::read_dir("/proc") {
            for entry in dir.flatten() {
                let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
                    continue;
                };
                // /proc/{pid}/stat：comm 位于括号内，其后 utime/stime/num_threads 为第 14/15/20 字段
                let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
                    continue;
                };
                let Some(open) = stat.find('(') else { continue };
                let Some(close) = stat.rfind(')') else { continue };
                let name = stat[open + 1..close].to_string();
                let rest: Vec<&str> = stat[close + 1..].split_whitespace().collect();
                let utime: u64 = rest.get(11).and_then(|v| v.parse().ok()).unwrap_or(0);
                let stime: u64 = rest.get(12).and_then(|v| v.parse().ok()).unwrap_or(0);
                let threads: u32 = rest.get(17).and_then(|v| v.parse().ok()).unwrap_or(0);

                let memory_mb = std::fs::read_to_string(format!("/proc/{pid}/statm"))
                    .ok()
                    .and_then(|t| t.split_whitespace().nth(1)?.parse::<u64>().ok())
                    .map(|pages| pages * page / 1024 / 1024)
                    .unwrap_or(0);

                items.push(ProcItem {
                    name,
                    pid,
                    memory_mb,
                    threads,
                    cpu_seconds: (utime + stime) as f64 / 100.0,
                });
            }
        }
    }

    #[cfg(not(any(windows, target_os = "linux")))]
    {
        let _ = sort_by_cpu;
    }

    if sort_by_cpu {
        items.sort_by(|a, b| b.cpu_seconds.total_cmp(&a.cpu_seconds));
    } else {
        items.sort_by(|a, b| b.memory_mb.cmp(&a.memory_mb));
    }
    items.truncate(count);
    items
}

/// 系统 TCP 连接计数（已建立 / TIME_WAIT / CLOSE_WAIT）。
pub(crate) fn tcp_counts() -> (u32, u32, u32) {
    #[cfg(windows)]
    {
        use windows_sys::Win32::NetworkManagement::IpHelper::{
            GetExtendedTcpTable, MIB_TCPROW_OWNER_PID, MIB_TCPTABLE_OWNER_PID,
            TCP_TABLE_OWNER_PID_ALL,
        };
        use windows_sys::Win32::Networking::WinSock::AF_INET;

        // MIB_TCP_STATE：5=ESTABLISHED 8=CLOSE_WAIT 11=TIME_WAIT（与 .NET TcpState 数值一致）
        const ESTABLISHED: u32 = 5;
        const CLOSE_WAIT: u32 = 8;
        const TIME_WAIT: u32 = 11;

        unsafe {
            let mut size: u32 = 0;
            GetExtendedTcpTable(
                std::ptr::null_mut(),
                &mut size,
                0,
                AF_INET as u32,
                TCP_TABLE_OWNER_PID_ALL,
                0,
            );
            if size == 0 {
                return (0, 0, 0);
            }

            let mut buf = vec![0u8; size as usize];
            let ret = GetExtendedTcpTable(
                buf.as_mut_ptr() as *mut core::ffi::c_void,
                &mut size,
                0,
                AF_INET as u32,
                TCP_TABLE_OWNER_PID_ALL,
                0,
            );
            if ret != 0 {
                return (0, 0, 0);
            }

            let table = buf.as_ptr() as *const MIB_TCPTABLE_OWNER_PID;
            let count = (*table).dwNumEntries as usize;
            let rows = (*table).table.as_ptr();
            let (mut estab, mut close_wait, mut time_wait) = (0u32, 0u32, 0u32);
            for i in 0..count {
                let row: *const MIB_TCPROW_OWNER_PID = rows.add(i);
                match (*row).dwState {
                    ESTABLISHED => estab += 1,
                    CLOSE_WAIT => close_wait += 1,
                    TIME_WAIT => time_wait += 1,
                    _ => {}
                }
            }
            (estab, time_wait, close_wait)
        }
    }

    #[cfg(target_os = "linux")]
    {
        // /proc/net/tcp：st 列（hex）：01=ESTABLISHED 06=TIME_WAIT 08=CLOSE_WAIT
        let Ok(text) = std::fs::read_to_string("/proc/net/tcp") else {
            return (0, 0, 0);
        };
        let (mut estab, mut time_wait, mut close_wait) = (0u32, 0u32, 0u32);
        // 只取第 4 列（状态码）做字节比较：连接多时避免每行两次堆分配（Vec + 大写 String）
        for line in text.lines().skip(1) {
            let Some(state) = line.split_whitespace().nth(3) else {
                continue;
            };
            match state.as_bytes() {
                b"01" => estab += 1,
                b"06" => time_wait += 1,
                b"08" => close_wait += 1,
                _ => {}
            }
        }
        (estab, time_wait, close_wait)
    }

    #[cfg(not(any(windows, target_os = "linux")))]
    {
        (0, 0, 0)
    }
}

/// 当前进程 CPU 时间（总秒、内核秒、用户秒）。
pub(crate) fn process_cpu_seconds() -> (f64, f64, f64) {
    process_cpu_split(std::process::id()).unwrap_or((0.0, 0.0, 0.0))
}

/// 释放当前进程工作集（Windows `EmptyWorkingSet`；其它平台空操作）。
pub(crate) fn empty_working_set() -> bool {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::ProcessStatus::EmptyWorkingSet;
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_QUERY_INFORMATION, PROCESS_SET_QUOTA,
        };

        unsafe {
            // EmptyWorkingSet 需要 SET_QUOTA 权限（PROCESS_QUERY_LIMITED_INFORMATION 不足）
            let h = OpenProcess(
                PROCESS_SET_QUOTA | PROCESS_QUERY_INFORMATION,
                0,
                std::process::id(),
            );
            if h.is_null() {
                return false;
            }
            let ok = EmptyWorkingSet(h);
            CloseHandle(h);
            ok != 0
        }
    }

    #[cfg(target_os = "linux")]
    {
        // glibc 的 malloc_trim 将空闲堆归还系统（等价于 C# GC + 释放虚拟内存的尽力而为）；
        // musl 无此扩展（交叉编译到 musl 目标时因缺符号失败过），按"空操作成功"处理
        // （面板显示释放 0MB，而非误报失败）。
        #[cfg(target_env = "gnu")]
        {
            unsafe { libc::malloc_trim(0) != 0 }
        }

        #[cfg(not(target_env = "gnu"))]
        {
            true
        }
    }

    #[cfg(not(any(windows, target_os = "linux")))]
    {
        false
    }
}

/// 指定进程 CPU 时间（总秒、内核秒、用户秒）。
#[cfg(windows)]
pub(crate) fn process_cpu_split(pid: u32) -> Option<(f64, f64, f64)> {
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME};
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    fn value(t: FILETIME) -> f64 {
        (((t.dwHighDateTime as u64) << 32) | t.dwLowDateTime as u64) as f64 / 10_000_000.0
    }

    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return None;
        }
        let (mut creation, mut exit, mut kernel, mut user): (FILETIME, FILETIME, FILETIME, FILETIME) =
            (std::mem::zeroed(), std::mem::zeroed(), std::mem::zeroed(), std::mem::zeroed());
        let ok = GetProcessTimes(h, &mut creation, &mut exit, &mut kernel, &mut user);
        CloseHandle(h);
        if ok == 0 {
            return None;
        }
        let kernel = value(kernel);
        let user = value(user);
        Some((kernel + user, kernel, user))
    }
}

/// 指定进程 CPU 时间（总秒、内核秒、用户秒）。
#[cfg(target_os = "linux")]
pub(crate) fn process_cpu_split(pid: u32) -> Option<(f64, f64, f64)> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let close = stat.rfind(')')?;
    let rest: Vec<&str> = stat[close + 1..].split_whitespace().collect();
    let utime: u64 = rest.get(11).and_then(|v| v.parse().ok()).unwrap_or(0);
    let stime: u64 = rest.get(12).and_then(|v| v.parse().ok()).unwrap_or(0);
    let user = utime as f64 / 100.0;
    let kernel = stime as f64 / 100.0;
    Some((user + kernel, kernel, user))
}

/// 指定进程 CPU 时间（总秒、内核秒、用户秒）。
#[cfg(not(any(windows, target_os = "linux")))]
pub(crate) fn process_cpu_split(_pid: u32) -> Option<(f64, f64, f64)> {
    None
}

/// 遍历当前所有进程（名称、PID、线程数）。名称已去除 `.exe` 后缀。
#[cfg(windows)]
fn for_each_process(mut f: impl FnMut(&str, u32, u32)) {
    use std::mem::zeroed;
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };

    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE || snapshot.is_null() {
            return;
        }

        let mut entry: PROCESSENTRY32W = zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        if Process32FirstW(snapshot, &mut entry) != 0 {
            loop {
                let end = entry
                    .szExeFile
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(entry.szExeFile.len());
                let raw = String::from_utf16_lossy(&entry.szExeFile[..end]);
                let name = raw.trim_end_matches(".exe");
                if !name.is_empty() {
                    f(name, entry.th32ProcessID, entry.cntThreads);
                }
                if Process32NextW(snapshot, &mut entry) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snapshot);
    }
}

/// 主机名。
///
/// 进程生命周期内视为不变，缓存避免每次面板请求重复读取环境变量 / `/etc/hostname`。
pub(crate) fn hostname() -> String {
    static CACHE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    CACHE.get_or_init(detect_hostname).clone()
}

fn detect_hostname() -> String {
    #[cfg(windows)]
    {
        std::env::var("COMPUTERNAME").unwrap_or_default()
    }

    #[cfg(not(windows))]
    {
        if let Ok(name) = std::env::var("HOSTNAME") {
            if !name.trim().is_empty() {
                return name.trim().to_string();
            }
        }
        std::fs::read_to_string("/etc/hostname")
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    }
}

// ————— 机器信息辅助（-ShowMachineInfo） —————

/// 网络接口信息。
pub(crate) struct NetInterface {
    /// 接口名称（中文系统为“以太网/WLAN”等；Linux 为 eth0 等）
    pub(crate) name: String,
    /// 描述（网卡型号；Linux 通常为空）
    pub(crate) description: String,
    /// 是否已连接
    pub(crate) up: bool,
    /// 链路速率（Mbps；0 表示未知）
    pub(crate) speed_mbps: u64,
    /// MAC 地址（`xx-xx-xx-xx-xx-xx`）
    pub(crate) mac: String,
    /// IPv4 地址列表
    pub(crate) ips: Vec<String>,
    /// 网关列表
    pub(crate) gateways: Vec<String>,
    /// DNS 服务器列表
    pub(crate) dns: Vec<String>,
    /// 累计接收字节数（自系统启动；0 表示未知）
    pub(crate) bytes_received: u64,
    /// 累计发送字节数（自系统启动；0 表示未知）
    pub(crate) bytes_sent: u64,
}

/// 磁盘信息。
pub(crate) struct DiskItem {
    /// 盘符（`C:\`）或挂载点
    pub(crate) name: String,
    /// 类型（固定/可移动/网络/光驱等）
    pub(crate) kind: String,
    /// 文件系统（NTFS/ext4 等）
    pub(crate) format: String,
    /// 卷标
    pub(crate) label: String,
    /// 总字节（未就绪为 0）
    pub(crate) total: u64,
    /// 可用字节
    pub(crate) free: u64,
    /// 是否就绪（光驱无盘等）
    pub(crate) ready: bool,
}

/// 当前用户名。
pub(crate) fn user_name() -> String {
    std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_default()
}

/// 操作系统描述（Windows 形如 `Microsoft Windows NT 10.0.26200.0`，与 .NET `OSDescription` 一致）。
///
/// 进程生命周期内不变，缓存避免每次面板请求重复读 `/etc/os-release` / 调用 RtlGetVersion。
pub(crate) fn os_description() -> String {
    static CACHE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    CACHE.get_or_init(detect_os_description).clone()
}

fn detect_os_description() -> String {
    #[cfg(windows)]
    {
        // 与 dhrust::logs 的实现同源（RtlGetVersion）；待公共化后统一下沉
        use windows_sys::Win32::System::SystemInformation::OSVERSIONINFOW;

        #[link(name = "ntdll")]
        unsafe extern "system" {
            fn RtlGetVersion(version_info: *mut OSVERSIONINFOW) -> i32;
        }

        unsafe {
            let mut info: OSVERSIONINFOW = std::mem::zeroed();
            info.dwOSVersionInfoSize = std::mem::size_of::<OSVERSIONINFOW>() as u32;
            if RtlGetVersion(&mut info) == 0 {
                return format!(
                    "Microsoft Windows NT {}.{}.{}.0",
                    info.dwMajorVersion, info.dwMinorVersion, info.dwBuildNumber
                );
            }
        }
        "Windows".to_string()
    }

    #[cfg(target_os = "linux")]
    {
        if let Ok(text) = std::fs::read_to_string("/etc/os-release") {
            for line in text.lines() {
                if let Some(value) = line.strip_prefix("PRETTY_NAME=") {
                    return value.trim().trim_matches('"').to_string();
                }
            }
        }
        "Linux".to_string()
    }

    #[cfg(target_os = "macos")]
    {
        "macOS".to_string()
    }

    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    {
        std::env::consts::OS.to_string()
    }
}

/// CPU 型号（Windows 读注册表 ProcessorNameString；Linux 读 /proc/cpuinfo；macOS 读 sysctl）。
///
/// 型号在进程生命周期内不变，缓存避免每次面板请求重复读注册表 / 解析 `/proc/cpuinfo`。
pub(crate) fn cpu_model() -> Option<String> {
    static CACHE: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    CACHE.get_or_init(detect_cpu_model).clone()
}

fn detect_cpu_model() -> Option<String> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::NO_ERROR;
        use windows_sys::Win32::System::Registry::{
            HKEY, HKEY_LOCAL_MACHINE, KEY_READ, RegCloseKey, RegOpenKeyExW, RegQueryValueExW,
        };

        fn wide(text: &str) -> Vec<u16> {
            text.encode_utf16().chain(std::iter::once(0)).collect()
        }

        let sub = wide("HARDWARE\\DESCRIPTION\\System\\CentralProcessor\\0");
        let value = wide("ProcessorNameString");
        unsafe {
            let mut hkey: HKEY = std::ptr::null_mut();
            if RegOpenKeyExW(HKEY_LOCAL_MACHINE, sub.as_ptr(), 0, KEY_READ, &mut hkey) != NO_ERROR {
                return None;
            }

            let mut size = 0u32;
            let mut kind = 0u32;
            let mut model = None;
            if RegQueryValueExW(
                hkey,
                value.as_ptr(),
                std::ptr::null(),
                &mut kind,
                std::ptr::null_mut(),
                &mut size,
            ) == NO_ERROR
                && size > 2
            {
                let mut buf = vec![0u8; size as usize];
                if RegQueryValueExW(
                    hkey,
                    value.as_ptr(),
                    std::ptr::null(),
                    &mut kind,
                    buf.as_mut_ptr(),
                    &mut size,
                ) == NO_ERROR
                {
                    let wide_text: &[u16] =
                        std::slice::from_raw_parts(buf.as_ptr() as *const u16, size as usize / 2);
                    let end = wide_text
                        .iter()
                        .position(|&c| c == 0)
                        .unwrap_or(wide_text.len());
                    let text = String::from_utf16_lossy(&wide_text[..end]);
                    if !text.trim().is_empty() {
                        model = Some(text.trim().to_string());
                    }
                }
            }
            RegCloseKey(hkey);
            model
        }
    }

    #[cfg(target_os = "linux")]
    {
        let text = std::fs::read_to_string("/proc/cpuinfo").ok()?;
        for line in text.lines() {
            for key in ["model name", "Hardware", "Model"] {
                if let Some(rest) = line.strip_prefix(key) {
                    if let Some((_, value)) = rest.split_once(':') {
                        let value = value.trim();
                        if !value.is_empty() {
                            return Some(value.to_string());
                        }
                    }
                }
            }
        }
        None
    }

    #[cfg(target_os = "macos")]
    {
        run_capture("sysctl", &["-n", "machdep.cpu.brand_string"])
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }
}

/// 解析 `/proc/meminfo`，返回 `(总内存, MemFree, Buffers, Cached + SReclaimable)`（字节）。
///
/// `cached` 口径对齐 psutil/宝塔与 `free` 命令：`Cached` 与 `SReclaimable` 相加。
#[cfg(any(target_os = "linux", test))]
fn parse_meminfo_bytes(text: &str) -> Option<(u64, u64, u64, u64)> {
    let mut total = None;
    let mut free = None;
    let mut buffers = None;
    let mut cached = None;
    let mut sreclaimable = 0u64;
    for line in text.lines() {
        let Some((key, rest)) = line.split_once(':') else {
            continue;
        };
        let Some(kb) = rest
            .split_whitespace()
            .next()
            .and_then(|v| v.parse::<u64>().ok())
        else {
            continue;
        };
        match key {
            "MemTotal" => total = Some(kb * 1024),
            "MemFree" => free = Some(kb * 1024),
            "Buffers" => buffers = Some(kb * 1024),
            "Cached" => cached = Some(kb * 1024),
            "SReclaimable" => sreclaimable = kb * 1024,
            _ => {}
        }
    }
    let total = total?;
    let free = free?;
    let buffers = buffers.unwrap_or(0);
    let cached = cached.unwrap_or(0) + sreclaimable;
    Some((total, free, buffers, cached))
}

/// 物理内存（总量、可用；字节）。
pub(crate) fn memory_info() -> Option<(u64, u64)> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
        unsafe {
            let mut status: MEMORYSTATUSEX = std::mem::zeroed();
            status.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
            if GlobalMemoryStatusEx(&mut status) != 0 {
                return Some((status.ullTotalPhys, status.ullAvailPhys));
            }
        }
        None
    }

    #[cfg(target_os = "linux")]
    {
        let text = std::fs::read_to_string("/proc/meminfo").ok()?;
        let (total, free, buffers, cached) = parse_meminfo_bytes(&text)?;
        // 对齐宝塔/psutil 口径：已用 = 总 - MemFree - Buffers - Cached - SReclaimable，
        // 即“可用”按宽松口径计算（缓存视为可回收）
        let mut avail = free + buffers + cached;
        if avail > total {
            // 容器等场景数值失真时退化为纯空闲（psutil 同处理）
            avail = free;
        }
        Some((total, avail))
    }

    #[cfg(target_os = "macos")]
    {
        let text = run_capture("sysctl", &["-n", "hw.memsize"])?;
        let bytes: u64 = text.trim().parse().ok()?;
        Some((bytes, 0))
    }
}

/// 系统运行时长文本（如 `21天13小时`）。
pub(crate) fn uptime_text() -> Option<String> {
    #[cfg(windows)]
    let millis: Option<u64> =
        Some(unsafe { windows_sys::Win32::System::SystemInformation::GetTickCount64() });

    #[cfg(target_os = "linux")]
    let millis: Option<u64> = std::fs::read_to_string("/proc/uptime")
        .ok()
        .and_then(|text| {
            text.split_whitespace()
                .next()
                .and_then(|v| v.parse::<f64>().ok())
        })
        .map(|secs| (secs * 1000.0) as u64);

    #[cfg(target_os = "macos")]
    let millis: Option<u64> = None;

    let millis = millis?;
    let total_minutes = millis / 60_000;
    let days = total_minutes / 1440;
    let hours = (total_minutes % 1440) / 60;
    let minutes = total_minutes % 60;
    if days > 0 {
        Some(format!("{days}天{hours}小时"))
    } else if hours > 0 {
        Some(format!("{hours}小时{minutes}分"))
    } else {
        Some(format!("{minutes}分"))
    }
}

/// 字节数友好格式（对齐 C# `ToGMK`：1024 进制，保留 1 位小数，如 `63.7G`）。
pub(crate) fn format_gmk(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "K", "M", "G", "T"];
    let mut value = bytes as f64;
    let mut idx = 0usize;
    while value >= 1024.0 && idx < UNITS.len() - 1 {
        value /= 1024.0;
        idx += 1;
    }
    if idx == 0 {
        format!("{bytes}B")
    } else {
        format!("{:.1}{}", value, UNITS[idx])
    }
}

/// MAC 地址格式（`xx-xx-xx-xx-xx-xx`）。
#[cfg_attr(not(windows), allow(dead_code))]
fn format_mac(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join("-")
}

/// 排除的虚拟/过滤类网卡关键字（对齐 C# `ShowMachineInfo._Excludes`）。
#[cfg_attr(not(windows), allow(dead_code))]
const VIRTUAL_ADAPTER_EXCLUDES: [&str; 14] = [
    "Loopback",
    "VMware",
    "VBox",
    "Virtual",
    "Teredo",
    "Tunnel",
    "VPN",
    "VNIC",
    "IEEE",
    "Filter",
    "Npcap",
    "QoS",
    "Miniport",
    "Kernel Debug",
];

/// 是否为虚拟/过滤类网卡（按描述匹配，忽略大小写）。
#[cfg_attr(not(windows), allow(dead_code))]
fn is_virtual_adapter(description: &str) -> bool {
    let lower = description.to_ascii_lowercase();
    VIRTUAL_ADAPTER_EXCLUDES
        .iter()
        .any(|e| lower.contains(&e.to_ascii_lowercase()))
}

// `/proc/net/dev` 解析已下沉到 `dhrust::sys::net`（2026-10-01，三项目共用）。

/// 本机网络总流量（接收字节, 发送字节）。用于面板速率差分计算。
/// 实现已下沉到 `dhrust::sys::net::net_totals`（2026-10-01，三项目共用）。
pub(crate) fn net_total_bytes() -> Option<(u64, u64)> {
    dhrust::sys::net::net_totals().map(|(rx, tx, _)| (rx, tx))
}

/// 磁盘 IO 累计统计（读/写完成次数与字节数）。
#[derive(Default, Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct DiskIo {
    /// 读完成次数
    pub(crate) reads: u64,
    /// 写完成次数
    pub(crate) writes: u64,
    /// 读字节数
    pub(crate) read_bytes: u64,
    /// 写字节数
    pub(crate) write_bytes: u64,
    /// IO 累计耗时（毫秒；Linux 为 diskstats 的 ms 字段，Windows 由 100ns 换算）
    pub(crate) ms_total: u64,
}

/// 解析 `/proc/diskstats`，汇总“整盘”（不做分区/映射层重复计数）的读写统计。
///
/// 规则：次设备号 `% 16 == 0` 视为整盘（兼容 sda/sdb/vda 多块盘；分区均为非 0）；
/// 跳过 `dm-`（LVM）与 `md`（软 RAID）映射，其 IO 已体现在底层物理盘。
#[cfg(any(target_os = "linux", test))]
fn parse_diskstats(text: &str) -> DiskIo {
    let mut io = DiskIo::default();
    for line in text.lines() {
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() < 11 {
            continue;
        }
        let name = cols[2];
        if name.starts_with("dm-") || name.starts_with("md") {
            continue;
        }
        let minor = cols[1].parse::<u64>().unwrap_or(1);
        if minor % 16 != 0 {
            continue;
        }
        io.reads += cols[3].parse::<u64>().unwrap_or(0);
        io.read_bytes += cols[5].parse::<u64>().unwrap_or(0) * 512; // 扇区 = 512 字节
        io.writes += cols[7].parse::<u64>().unwrap_or(0);
        io.write_bytes += cols[9].parse::<u64>().unwrap_or(0) * 512;
        io.ms_total += cols[6].parse::<u64>().unwrap_or(0) + cols[10].parse::<u64>().unwrap_or(0);
    }
    io
}

/// 磁盘 IO 累计统计。Web 面板差分计算 IOPS 与读/写速率用。
/// Linux 读 `/proc/diskstats`；Windows 汇总物理磁盘（`IOCTL_DISK_PERFORMANCE`）。
pub(crate) fn disk_io() -> Option<DiskIo> {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string("/proc/diskstats")
            .ok()
            .map(|t| parse_diskstats(&t))
    }

    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::Storage::FileSystem::{
            CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
        };
        use windows_sys::Win32::System::IO::DeviceIoControl;

        /// `DISK_PERFORMANCE`（winioctl.h）头部：仅取读写统计所需字段，`_rest` 保持布局。
        #[repr(C)]
        #[derive(Default, Clone, Copy)]
        struct DiskPerformance {
            bytes_read: i64,
            bytes_written: i64,
            read_time: i64,
            write_time: i64,
            idle_time: i64,
            read_count: u32,
            write_count: u32,
            queue_depth: u32,
            split_count: u32,
            _rest: [u8; 32],
        }

        const IOCTL_DISK_PERFORMANCE: u32 = 0x0007_0020;

        let mut io = DiskIo::default();
        let mut available = false;
        for index in 0..16u32 {
            let path = format!("\\\\.\\PhysicalDrive{index}");
            let path_w: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
            let handle = unsafe {
                CreateFileW(
                    path_w.as_ptr(),
                    0,
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                    std::ptr::null(),
                    OPEN_EXISTING,
                    0,
                    std::ptr::null_mut(),
                )
            };
            if handle == INVALID_HANDLE_VALUE {
                continue;
            }

            let mut perf = DiskPerformance::default();
            let mut returned = 0u32;
            let ok = unsafe {
                DeviceIoControl(
                    handle,
                    IOCTL_DISK_PERFORMANCE,
                    std::ptr::null(),
                    0,
                    &mut perf as *mut DiskPerformance as *mut core::ffi::c_void,
                    std::mem::size_of::<DiskPerformance>() as u32,
                    &mut returned,
                    std::ptr::null_mut(),
                )
            };
            unsafe { CloseHandle(handle) };

            if ok != 0 {
                available = true;
                io.reads += perf.read_count as u64;
                io.writes += perf.write_count as u64;
                io.read_bytes += perf.bytes_read.max(0) as u64;
                io.write_bytes += perf.bytes_written.max(0) as u64;
                // 时间字段单位为 100ns（换算为毫秒）
                io.ms_total += ((perf.read_time.max(0) + perf.write_time.max(0)) as u64) / 10_000;
            }
        }

        if available { Some(io) } else { None }
    }

    #[cfg(not(any(target_os = "linux", windows)))]
    {
        None
    }
}

pub(crate) fn network_interfaces() -> Vec<NetInterface> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{ERROR_BUFFER_OVERFLOW, NO_ERROR};
        use windows_sys::Win32::NetworkManagement::IpHelper::{
            FreeMibTable, GAA_FLAG_INCLUDE_GATEWAYS, GetAdaptersAddresses, GetIfTable2,
            IP_ADAPTER_ADDRESSES_LH, MIB_IF_TABLE2,
        };
        use windows_sys::Win32::Networking::WinSock::{AF_INET, AF_UNSPEC, SOCKADDR, SOCKADDR_IN};

        /// 从 `SOCKET_ADDRESS` 提取 IPv4（非 IPv4 返回 None）。
        fn sockaddr_ipv4(addr: *const SOCKADDR) -> Option<String> {
            unsafe {
                if addr.is_null() || (*addr).sa_family != AF_INET {
                    return None;
                }
                let sin = &*(addr as *const SOCKADDR_IN);
                let b = sin.sin_addr.S_un.S_addr.to_ne_bytes();
                Some(format!("{}.{}.{}.{}", b[0], b[1], b[2], b[3]))
            }
        }

        /// 读取 PWSTR（UTF-16）到 String。
        fn pwstr(ptr: *const u16) -> String {
            unsafe {
                if ptr.is_null() {
                    return String::new();
                }
                let mut len = 0usize;
                while *ptr.add(len) != 0 && len < 512 {
                    len += 1;
                }
                String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len))
            }
        }

        // 每网卡累计收发（MIB 接口行按 LUID 匹配；对齐 C# `GetIPv4Statistics()`）
        let mut luid_stats: Vec<(u64, u64, u64)> = Vec::new(); // (LUID, 接收字节, 发送字节)
        unsafe {
            let mut table: *mut MIB_IF_TABLE2 = std::ptr::null_mut();
            if GetIfTable2(&mut table) == 0 && !table.is_null() {
                let t = &*table;
                for i in 0..t.NumEntries as usize {
                    let row = &*t.Table.as_ptr().add(i);
                    // 过滤软件回环（IfType=24）
                    if row.Type == 24 {
                        continue;
                    }
                    // 计数器不可用时为 u64::MAX
                    let rx = if row.InOctets == u64::MAX { 0 } else { row.InOctets };
                    let tx = if row.OutOctets == u64::MAX { 0 } else { row.OutOctets };
                    luid_stats.push((row.InterfaceLuid.Value, rx, tx));
                }
                FreeMibTable(table as *const _);
            }
        }

        let mut out = Vec::new();
        unsafe {
            let mut size: u32 = 16 * 1024;
            let mut buf: Vec<u8> = vec![0; size as usize];
            let mut ret = GetAdaptersAddresses(
                AF_UNSPEC as u32,
                GAA_FLAG_INCLUDE_GATEWAYS,
                std::ptr::null(),
                buf.as_mut_ptr() as *mut IP_ADAPTER_ADDRESSES_LH,
                &mut size,
            );
            if ret == ERROR_BUFFER_OVERFLOW {
                buf = vec![0; size as usize];
                ret = GetAdaptersAddresses(
                    AF_UNSPEC as u32,
                    GAA_FLAG_INCLUDE_GATEWAYS,
                    std::ptr::null(),
                    buf.as_mut_ptr() as *mut IP_ADAPTER_ADDRESSES_LH,
                    &mut size,
                );
            }
            if ret != NO_ERROR {
                return out;
            }

            let mut adapter = buf.as_ptr() as *const IP_ADAPTER_ADDRESSES_LH;
            while !adapter.is_null() {
                let a = &*adapter;
                adapter = a.Next;

                // 过滤：软件回环（IfType=24）/ 隧道（131）与虚拟网卡（对齐 C# 排除表）
                let description = pwstr(a.Description);
                if a.IfType == 24 || a.IfType == 131 || is_virtual_adapter(&description) {
                    continue;
                }

                let mut ips = Vec::new();
                let mut ua = a.FirstUnicastAddress;
                while !ua.is_null() {
                    if let Some(ip) = sockaddr_ipv4((*ua).Address.lpSockaddr) {
                        if !ips.contains(&ip) {
                            ips.push(ip);
                        }
                    }
                    ua = (*ua).Next;
                }

                let mut gateways = Vec::new();
                let mut ga = a.FirstGatewayAddress;
                while !ga.is_null() {
                    if let Some(ip) = sockaddr_ipv4((*ga).Address.lpSockaddr) {
                        gateways.push(ip);
                    }
                    ga = (*ga).Next;
                }

                let mut dns = Vec::new();
                let mut da = a.FirstDnsServerAddress;
                while !da.is_null() {
                    if let Some(ip) = sockaddr_ipv4((*da).Address.lpSockaddr) {
                        dns.push(ip);
                    }
                    da = (*da).Next;
                }

                let mac_len = (a.PhysicalAddressLength as usize).min(a.PhysicalAddress.len());
                let luid = a.Luid.Value;
                let (bytes_received, bytes_sent) = if luid != 0 {
                    luid_stats
                        .iter()
                        .find(|(l, _, _)| *l == luid)
                        .map(|(_, rx, tx)| (*rx, *tx))
                        .unwrap_or((0, 0))
                } else {
                    (0, 0)
                };
                out.push(NetInterface {
                    name: pwstr(a.FriendlyName),
                    description,
                    up: a.OperStatus == 1, // IfOperStatusUp
                    speed_mbps: a.TransmitLinkSpeed / 1_000_000,
                    mac: format_mac(&a.PhysicalAddress[..mac_len]),
                    ips,
                    gateways,
                    dns,
                    bytes_received,
                    bytes_sent,
                });
            }
        }
        out
    }

    #[cfg(target_os = "linux")]
    {
        // 每网卡累计收发：/proc/net/dev 单文件源（任一接口缺失不影响其他接口）
        let dev_entries = std::fs::read_to_string("/proc/net/dev")
            .map(|t| dhrust::sys::net::parse_net_dev_entries(&t))
            .unwrap_or_default();
        let mut out = Vec::new();
        let Ok(dir) = std::fs::read_dir("/sys/class/net") else {
            return out;
        };
        for entry in dir.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name == "lo" {
                continue;
            }
            let base = entry.path();
            let read = |file: &str| {
                std::fs::read_to_string(base.join(file))
                    .map(|s| s.trim().to_string())
                    .ok()
            };
            let mac = read("address").unwrap_or_default();
            if mac.is_empty() || mac == "00:00:00:00:00:00" {
                continue; // 虚拟接口常见全零 MAC
            }
            let (bytes_received, bytes_sent) = dev_entries
                .iter()
                .find(|(n, _, _)| n == &name)
                .map(|(_, rx, tx)| (*rx, *tx))
                .unwrap_or((0, 0));
            out.push(NetInterface {
                name,
                description: String::new(),
                up: read("operstate").as_deref() == Some("up"),
                speed_mbps: read("speed")
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap_or(0),
                mac: mac.to_uppercase().replace(':', "-"),
                ips: Vec::new(),
                gateways: Vec::new(),
                dns: Vec::new(),
                bytes_received,
                bytes_sent,
            });
        }
        out
    }

    #[cfg(target_os = "macos")]
    {
        Vec::new() // macOS 待实机补充（需 ifconfig/netstat 解析）
    }
}

// 挂载过滤（TEMP_FS_TYPES / mount_key / is_temporary_mount）已下沉到 `dhrust::sys::disk`（2026-10-01，三项目共用）。

/// 枚举磁盘（对齐 C# `ShowMachineInfo`：全量枚举并标注类型）。
pub(crate) fn disks() -> Vec<DiskItem> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Storage::FileSystem::{
            GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW,
        };

        fn utf16z(buf: &[u16]) -> String {
            let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
            String::from_utf16_lossy(&buf[..end])
        }

        let mut out = Vec::new();
        let mask = unsafe { GetLogicalDrives() };
        for i in 0..26u32 {
            if mask & (1 << i) == 0 {
                continue;
            }
            let letter = (b'A' + i as u8) as char;
            let root = format!("{letter}:\\");
            let root_w: Vec<u16> = root.encode_utf16().chain(std::iter::once(0)).collect();

            let kind = match unsafe { GetDriveTypeW(root_w.as_ptr()) } {
                2 => "可移动",
                3 => "固定",
                4 => "网络",
                5 => "光驱",
                6 => "内存盘",
                _ => "其它",
            };

            let mut label_buf = [0u16; 128];
            let mut fs_buf = [0u16; 32];
            let ok = unsafe {
                GetVolumeInformationW(
                    root_w.as_ptr(),
                    label_buf.as_mut_ptr(),
                    label_buf.len() as u32,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    fs_buf.as_mut_ptr(),
                    fs_buf.len() as u32,
                )
            } != 0;

            let mut free = 0u64;
            let mut total = 0u64;
            let mut total_free = 0u64;
            let got = unsafe {
                GetDiskFreeSpaceExW(root_w.as_ptr(), &mut free, &mut total, &mut total_free)
            } != 0;

            out.push(DiskItem {
                name: root,
                kind: kind.to_string(),
                format: if ok { utf16z(&fs_buf) } else { String::new() },
                label: if ok { utf16z(&label_buf) } else { String::new() },
                total: if got { total } else { 0 },
                free: if got { free } else { 0 },
                ready: ok,
            });
        }
        out
    }

    #[cfg(target_os = "linux")]
    {
        let mut out = Vec::new();
        let Ok(mounts) = std::fs::read_to_string("/proc/mounts") else {
            return out;
        };
        for line in mounts.lines() {
            let mut cols = line.split_whitespace();
            let (Some(dev), Some(mp), Some(fs)) = (cols.next(), cols.next(), cols.next()) else {
                continue;
            };
            if !dev.starts_with("/dev/") {
                continue;
            }
            // 八进制转义还原（\040 等）；过滤引导分区与系统虚拟文件系统（对齐 DHDeploy `IsTemporaryVolume`）
            let mount = dhrust::sys::disk::unescape_mount(mp);
            if dhrust::sys::disk::is_temporary_mount(&mount, Some(fs)) {
                continue;
            }
            let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
            let Ok(path) = std::ffi::CString::new(mount.as_str()) else {
                continue;
            };
            let ok = unsafe { libc::statvfs(path.as_ptr(), &mut stat) } == 0;
            let (total, free) = if ok {
                let block = stat.f_frsize as u64;
                (stat.f_blocks as u64 * block, stat.f_bavail as u64 * block)
            } else {
                (0, 0)
            };
            out.push(DiskItem {
                name: mount.clone(),
                kind: "固定".to_string(),
                format: fs.to_string(),
                label: String::new(),
                total,
                free,
                ready: ok,
            });
        }
        out
    }

    #[cfg(target_os = "macos")]
    {
        Vec::new() // macOS 待实机补充
    }
}

/// 全部就绪磁盘的用量列表：`(已用 MB, 总量 MB, 名称)`。
/// 过滤未就绪（光驱无盘等）与零容量项；Linux 尽量把根分区 `/` 排在最前。
pub(crate) fn disk_usages() -> Vec<(u64, u64, String)> {
    // Windows 下盘符已按 A-Z 升序，无需排序；`mut` 仅非 Windows 平台使用
    #[cfg_attr(windows, allow(unused_mut))]
    let mut list: Vec<DiskItem> = disks()
        .into_iter()
        .filter(|d| d.ready && d.total > 0)
        .collect();

    #[cfg(not(windows))]
    list.sort_by_key(|d| if d.name == "/" { 0 } else { 1 });

    list.into_iter()
        .map(|d| {
            (
                d.total.saturating_sub(d.free) / 1024 / 1024,
                d.total / 1024 / 1024,
                d.name,
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_process_is_alive() {
        let pid = std::process::id();
        assert!(is_alive(pid));
    }

    #[test]
    fn memory_of_current_process() {
        let pid = std::process::id();
        if let Some(mb) = memory_mb(pid) {
            assert!(mb > 0);
        }
    }

    /// 已退出但句柄未回收的“僵尸”进程不得误判为存活（Windows）。
    #[cfg(windows)]
    #[test]
    fn zombie_process_reports_not_alive() {
        let mut child = std::process::Command::new("cmd")
            .args(["/c", "exit", "0"])
            .spawn()
            .unwrap();
        std::thread::sleep(Duration::from_millis(500));

        let pid = child.id();
        assert!(!is_alive(pid), "僵尸进程被误判为存活");
        let _ = child.wait();
    }

    #[test]
    fn format_gmk_units() {
        assert_eq!(format_gmk(512), "512B");
        assert_eq!(format_gmk(1024), "1.0K");
        assert_eq!(format_gmk(63 * 1024 * 1024 * 1024), "63.0G");
        assert_eq!(format_gmk(3 * 1024u64 * 1024 * 1024 * 1024), "3.0T");
    }

    #[test]
    fn format_mac_shape() {
        assert_eq!(
            format_mac(&[0x8c, 0x32, 0x23, 0x17, 0x8d, 0x54]),
            "8c-32-23-17-8d-54"
        );
        assert_eq!(format_mac(&[]), "");
    }

    #[test]
    fn machine_info_contains_core_lines() {
        let text = machine_info();
        for key in ["系统：", "处理器：", "内存：", "程序："] {
            assert!(text.contains(key), "缺少 {key}:\n{text}");
        }
        assert!(!os_description().is_empty());
    }

    #[test]
    fn load_average_matches_platform() {
        #[cfg(target_os = "linux")]
        assert!(load_average().is_some(), "Linux 应提供负载数据");
        #[cfg(not(target_os = "linux"))]
        assert!(load_average().is_none(), "非 Linux 平台无负载数据");
    }

    #[test]
    fn parse_meminfo_matches_baota_semantics() {
        let sample = "\
MemTotal:       16299544 kB
MemFree:          512000 kB
MemAvailable:    8192000 kB
Buffers:          102400 kB
Cached:          4096000 kB
SReclaimable:     204800 kB
Shmem:            100000 kB
";
        let (total, free, buffers, cached) = parse_meminfo_bytes(sample).unwrap();
        assert_eq!(total, 16299544 * 1024);
        assert_eq!(free, 512000 * 1024);
        assert_eq!(buffers, 102400 * 1024);
        // cached = Cached + SReclaimable（free 命令/psutil 口径）
        assert_eq!(cached, (4096000 + 204800) * 1024);
        // 宝塔 memRealUsed = 总 - MemFree - Buffers - Cached - SReclaimable
        let used = total - free - buffers - cached;
        assert_eq!(used, (16299544 - 512000 - 102400 - 4096000 - 204800) * 1024);
    }

    #[test]
    fn parse_meminfo_without_optional_fields() {
        let sample = "MemTotal:       1000 kB\nMemFree:         100 kB\n";
        let (total, free, buffers, cached) = parse_meminfo_bytes(sample).unwrap();
        assert_eq!((total, free, buffers, cached), (1000 * 1024, 100 * 1024, 0, 0));
        assert!(
            parse_meminfo_bytes("MemFree: 100 kB\n").is_none(),
            "缺 MemTotal 应失败"
        );
    }

    #[test]
    fn parse_proc_stat_first_cpu_matches_psutil() {
        // 10 字段（含 guest/guest_nice）：guest 已计入 user/nice，总时须扣除
        let text = "cpu  100 20 30 400 50 5 5 0 15 5\ncpu0 10 2 3 40 5 0 0 0 1 0\n";
        let (idle, total) = parse_proc_stat_first_cpu(text).unwrap();
        assert_eq!(idle, 400 + 50, "空转=idle+iowait");
        // 全部字段和 630，扣除 guest(15)+guest_nice(5)
        assert_eq!(total, 630 - 20);
        // 仅 4 字段的旧内核
        let legacy = "cpu  1 2 3 4\n";
        assert_eq!(parse_proc_stat_first_cpu(legacy), Some((4, 10)));
    }

    #[test]
    fn cpu_rate_from_delta_matches_psutil_formula() {
        // Δ总 100、Δ(idle+iowait) 20 → 80%
        assert_eq!(cpu_rate_from_delta((10, 100), (30, 200)), Some(80.0));
        // 字段回退（负增量）按 0 处理 → 全忙
        assert_eq!(cpu_rate_from_delta((50, 100), (40, 150)), Some(100.0));
        // 窗口内无 tick 变化：沿用上次速率
        assert_eq!(cpu_rate_from_delta((0, 100), (0, 100)), None);
    }

    #[test]
    fn parse_diskstats_skips_partitions_and_mappers() {
        let sample = "\
   8       0 sda 100 0 1000 250 200 0 2000 500 0 0 0
   8       1 sda1 50 0 500 100 60 0 600 200 0 0 0
   8      16 sdb 7 0 70 5 8 0 80 6 0 0 0
 259       0 nvme0n1 10 0 100 30 20 0 200 40 0 0 0
 253       0 dm-0 90 0 900 300 180 0 1800 600 0 0 0
 252       0 md0 5 0 50 10 5 0 50 20 0 0 0
";
        let io = parse_diskstats(sample);
        // 次数：sda(100+200) + sdb(7+8) + nvme0n1(10+20)；分区/映射层跳过
        assert_eq!(io.reads + io.writes, 345);
        // 字节 = 扇区×512：(1000+70+100)、（2000+80+200）
        assert_eq!(io.read_bytes, 1170 * 512);
        assert_eq!(io.write_bytes, 2280 * 512);
        // IO 耗时毫秒：sda(250+500) + sdb(5+6) + nvme0n1(30+40)
        assert_eq!(io.ms_total, 831);
    }

    #[test]
    fn disk_usages_is_sane() {
        for (used, total, name) in disk_usages() {
            assert!(total > 0, "磁盘总量应为正");
            assert!(used <= total, "磁盘已用不应超过总量");
            assert!(!name.is_empty(), "磁盘名称不应为空");
        }
    }

    #[test]
    fn split_epoch_ms_handles_fractions_and_negatives() {
        assert_eq!(split_epoch_ms(946_730_096_789), (946_730_096, 789_000_000));
        assert_eq!(split_epoch_ms(0), (0, 0));
        assert_eq!(split_epoch_ms(-1), (-1, 999_000_000));
    }

    #[cfg(windows)]
    #[test]
    fn systemtime_conversion_uses_utc() {
        // 946 730 096 789 ms = 2000-01-01 12:34:56.789 UTC（周六）
        let st = epoch_ms_to_systemtime(946_730_096_789).unwrap();
        assert_eq!(
            (
                st.wYear,
                st.wMonth,
                st.wDayOfWeek,
                st.wDay,
                st.wHour,
                st.wMinute,
                st.wSecond,
                st.wMilliseconds
            ),
            (2000, 1, 6, 1, 12, 34, 56, 789)
        );
    }
}
