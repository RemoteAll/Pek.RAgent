//! 平台相关：进程启停、存活/内存/名称查询、机器信息、优先級与 OOM 分值。
//!
//! 设计要点：
//! - 启动的子进程默认重定向到空设备，避免服务模式无控制台时输出异常；
//!   应用调试输出（`Debug=true`）追加到 `Log/app-{Name}.log`；
//! - 停止先温和（Unix SIGTERM / Windows taskkill），超时后强制（SIGKILL / taskkill /F）；
//! - 内存读取：Linux `/proc/{pid}/statm`、Windows `GetProcessMemoryInfo`、macOS `ps`。

use std::path::Path;
use std::process::{Child, Command, Stdio};
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

/// 系统 CPU 使用率（0~100；200ms 两次采样差值）。
pub(crate) fn system_cpu_rate() -> Option<f64> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::FILETIME;
        use windows_sys::Win32::System::Threading::GetSystemTimes;

        // 返回 (空闲 100ns, 总忙 100ns)；Windows 的内核时间已包含空闲时间
        fn sample() -> Option<(u64, u64)> {
            fn value(t: FILETIME) -> u64 {
                ((t.dwHighDateTime as u64) << 32) | t.dwLowDateTime as u64
            }

            unsafe {
                let (mut idle, mut kernel, mut user): (FILETIME, FILETIME, FILETIME) =
                    (std::mem::zeroed(), std::mem::zeroed(), std::mem::zeroed());
                if GetSystemTimes(&mut idle, &mut kernel, &mut user) == 0 {
                    return None;
                }
                Some((value(idle), value(kernel) + value(user)))
            }
        }

        let (idle1, total1) = sample()?;
        std::thread::sleep(Duration::from_millis(200));
        let (idle2, total2) = sample()?;

        let total = total2.saturating_sub(total1);
        let idle = idle2.saturating_sub(idle1);
        if total == 0 {
            return None;
        }
        Some(((1.0 - idle as f64 / total as f64) * 100.0).clamp(0.0, 100.0))
    }

    #[cfg(target_os = "linux")]
    {
        // /proc/stat 首行：cpu user nice system idle iowait irq softirq steal ...
        fn sample() -> Option<(u64, u64)> {
            let text = std::fs::read_to_string("/proc/stat").ok()?;
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
            let total: u64 = values.iter().sum();
            Some((idle, total))
        }

        let (idle1, total1) = sample()?;
        std::thread::sleep(Duration::from_millis(200));
        let (idle2, total2) = sample()?;

        let total = total2.saturating_sub(total1);
        let idle = idle2.saturating_sub(idle1);
        if total == 0 {
            return None;
        }
        Some(((1.0 - idle as f64 / total as f64) * 100.0).clamp(0.0, 100.0))
    }

    #[cfg(not(any(windows, target_os = "linux")))]
    {
        None
    }
}

/// 机器唯一标识（Windows 注册表 `MachineGuid`；Linux `/etc/machine-id`）。
pub(crate) fn machine_guid() -> Option<String> {
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
        for line in text.lines().skip(1) {
            let fields: Vec<&str> = line.split_whitespace().collect();
            match fields.get(3).map(|v| v.to_ascii_uppercase()) {
                Some(state) if state == "01" => estab += 1,
                Some(state) if state == "06" => time_wait += 1,
                Some(state) if state == "08" => close_wait += 1,
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
pub(crate) fn hostname() -> String {
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
pub(crate) fn os_description() -> String {
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
pub(crate) fn cpu_model() -> Option<String> {
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
        let parse_kb = |line: &str, key: &str| {
            line.strip_prefix(key).and_then(|rest| {
                rest.split_whitespace()
                    .next()
                    .and_then(|v| v.parse::<u64>().ok())
                    .map(|kb| kb * 1024)
            })
        };
        let mut total = None;
        let mut avail = None;
        for line in text.lines() {
            if total.is_none() {
                total = parse_kb(line, "MemTotal:");
            }
            if avail.is_none() {
                avail = parse_kb(line, "MemAvailable:");
            }
            if total.is_some() && avail.is_some() {
                break;
            }
        }
        total.map(|t| (t, avail.unwrap_or(0)))
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

/// 枚举网络接口（对齐 C# `ShowMachineInfo`：排除回环/虚拟网卡，取 IPv4）。
pub(crate) fn network_interfaces() -> Vec<NetInterface> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{ERROR_BUFFER_OVERFLOW, NO_ERROR};
        use windows_sys::Win32::NetworkManagement::IpHelper::{
            GAA_FLAG_INCLUDE_GATEWAYS, GetAdaptersAddresses, IP_ADAPTER_ADDRESSES_LH,
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
                out.push(NetInterface {
                    name: pwstr(a.FriendlyName),
                    description,
                    up: a.OperStatus == 1, // IfOperStatusUp
                    speed_mbps: a.TransmitLinkSpeed / 1_000_000,
                    mac: format_mac(&a.PhysicalAddress[..mac_len]),
                    ips,
                    gateways,
                    dns,
                });
            }
        }
        out
    }

    #[cfg(target_os = "linux")]
    {
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
            });
        }
        out
    }

    #[cfg(target_os = "macos")]
    {
        Vec::new() // macOS 待实机补充（需 ifconfig/netstat 解析）
    }
}

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
            let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
            let Ok(path) = std::ffi::CString::new(mp) else {
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
                name: mp.to_string(),
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
}
