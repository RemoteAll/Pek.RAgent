//! 平台相关：机器信息文本（`-ShowMachineInfo`）与 DH.RustBase 通用能力的再导出。
//!
//! **2026-10-03 第二批下沉**：进程管理、实时指标采集、机器事实全部移至
//! `dhrust::sys::{process, monitor, machine}`（含单元测试与 Linux 交叉编译校验）。
//! 本文件仅保留 StarAgent 专属的展示层（C# `ShowMachineInfo`/`ToGMK` 文案对齐）
//! 与再导出，调用点（webpanel/sampler/cli/app 等）路径保持不变。

// —— 通用能力再导出（下沉 dhrust；保持本模块既有路径兼容）——

pub(crate) use dhrust::sys::machine::{
    cpu_model, disk_usages, disks, host_uptime_seconds, hostname, machine_guid, memory_info,
    network_interfaces, os_description, set_system_time, user_name,
};
pub(crate) use dhrust::sys::monitor::{
    disk_io, is_process_running, load_average, process_cpu_seconds, process_cpu_split,
    process_stats, system_cpu_rate, tcp_counts, top_processes, DiskIo, ProcItem,
};
pub(crate) use dhrust::sys::process::{
    empty_working_set, is_alive, memory_mb, process_name, raise_priority, set_oom_score_adjust,
    signal_force, signal_graceful, spawn, stop_process, Handle, SpawnRequest,
};

/// 指定进程（含其子进程树）正在监听的 TCP 端口（升序去重）。
///
/// 用于「未配置 `HealthCheck`」时的自动探活：由 RAgent 自己找出子服务的监听口，
/// 再发起探活请求。取不到（无监听 / 权限不足 / 非本平台）返回空。
pub(crate) fn listen_ports_of(pid: u32) -> Vec<u16> {
    let mut ports = listen_ports_of_impl(pid);
    ports.sort_unstable();
    ports.dedup();
    ports
}

#[cfg(target_os = "linux")]
fn listen_ports_of_impl(pid: u32) -> Vec<u16> {
    use std::collections::{HashMap, HashSet};

    // 1) LISTEN socket inode → 端口（/proc/net/tcp{,6} 第 10 列是 inode）
    let mut inode_port: HashMap<u64, u16> = HashMap::new();
    for path in ["/proc/net/tcp", "/proc/net/tcp6"] {
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        for line in text.lines().skip(1) {
            let cols: Vec<&str> = line.split_whitespace().collect();
            if cols.len() < 10 || cols[3] != "0A" {
                continue; // 仅 TCP LISTEN
            }
            let Ok(port) = u16::from_str_radix(
                cols[1].rsplit(':').next().unwrap_or("0"),
                16,
            ) else {
                continue;
            };
            let Ok(inode) = cols[9].parse::<u64>() else {
                continue;
            };
            if port != 0 {
                inode_port.insert(inode, port);
            }
        }
    }
    if inode_port.is_empty() {
        return Vec::new();
    }

    // 2) 进程树（自身 + 后代）：应用常由包装脚本 / 服务宿主再 fork 出真正监听者
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    if let Ok(dir) = std::fs::read_dir("/proc") {
        for entry in dir.flatten() {
            let Ok(p) = entry.file_name().to_string_lossy().parse::<u32>() else {
                continue;
            };
            let Ok(stat) = std::fs::read_to_string(format!("/proc/{p}/stat")) else {
                continue;
            };
            let Some(close) = stat.rfind(')') else { continue };
            let rest: Vec<&str> = stat[close + 1..].split_whitespace().collect();
            if let Some(ppid) = rest.get(1).and_then(|v| v.parse::<u32>().ok()) {
                children.entry(ppid).or_default().push(p);
            }
        }
    }
    let mut pending = vec![pid];
    let mut seen: HashSet<u32> = HashSet::new();
    let mut pids = Vec::new();
    while let Some(p) = pending.pop() {
        if !seen.insert(p) {
            continue;
        }
        pids.push(p);
        if let Some(list) = children.get(&p) {
            pending.extend(list.iter().copied());
        }
    }

    // 3) 这些进程打开的 socket inode → 端口
    let mut ports = Vec::new();
    for p in pids {
        let Ok(dir) = std::fs::read_dir(format!("/proc/{p}/fd")) else {
            continue;
        };
        for entry in dir.flatten() {
            let Ok(target) = std::fs::read_link(entry.path()) else {
                continue;
            };
            let Some(text) = target.to_str() else { continue };
            let Some(inode) = text
                .strip_prefix("socket:[")
                .and_then(|v| v.strip_suffix(']'))
                .and_then(|v| v.parse::<u64>().ok())
            else {
                continue;
            };
            if let Some(&port) = inode_port.get(&inode) {
                ports.push(port);
            }
        }
    }
    ports
}

#[cfg(windows)]
fn listen_ports_of_impl(pid: u32) -> Vec<u16> {
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        GetExtendedTcpTable, MIB_TCPTABLE_OWNER_PID, TCP_TABLE_OWNER_PID_ALL,
    };
    use windows_sys::Win32::Networking::WinSock::AF_INET;

    let mut ports = Vec::new();
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
            return ports;
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
            return ports;
        }
        let table = buf.as_ptr() as *const MIB_TCPTABLE_OWNER_PID;
        let count = (*table).dwNumEntries as usize;
        let rows = (*table).table.as_ptr();
        for i in 0..count {
            let row = rows.add(i);
            if (*row).dwOwningPid != pid || (*row).dwState != 2 {
                continue; // MIB_TCP_STATE_LISTEN
            }
            let port = (((*row).dwLocalPort & 0xffff) as u16).swap_bytes();
            if port != 0 {
                ports.push(port);
            }
        }
    }
    ports
}

#[cfg(not(any(windows, target_os = "linux")))]
fn listen_ports_of_impl(_pid: u32) -> Vec<u16> {
    Vec::new()
}

/// 重启本机（平台「服务器节点」页下发 `reboot` 指令时调用）。
///
/// 延迟 3 秒执行：留出写日志与平台侧记录的时间。以服务身份运行（root / LocalSystem）才有权限，
/// 权限不足时命令会失败并记日志，同时把失败原因回报平台（`rebootResult`）。
pub(crate) fn reboot_machine(agent_id: &str) {
    let agent_id = agent_id.to_string();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(3));
        crate::util::log_info("执行系统重启（平台下发指令）");
        match do_reboot() {
            Ok(()) => crate::util::log_info("已提交系统重启请求"),
            Err(e) => {
                let msg = format!("系统重启失败：{e}");
                crate::util::log_error(&msg);
                crate::panel_ws::send_event(
                    &agent_id,
                    serde_json::json!({ "type": "rebootResult", "ok": false, "reason": msg }),
                );
            }
        }
    });
}

/// 重启前预检：返回 `Err(原因)` 表示当前环境不可能重启整机（避免"点了没反应、原因只在日志里"）。
///
/// 覆盖三类常见失败：
/// 1. **容器内**（Docker/LXC/K8s）：重启容器 ≠ 重启宿主机，指令在容器里执行只会失败；
/// 2. **非 root**（Linux）：`systemctl reboot` / `shutdown` 均需 root（或 polkit 授权）；
/// 3. **缺少重启工具**（极简镜像可能没有 systemctl/shutdown）。
pub(crate) fn reboot_precheck() -> Result<(), String> {
    // 容器环境识别（容器内重启无效）
    if std::path::Path::new("/.dockerenv").exists()
        || std::path::Path::new("/run/.containerenv").exists()
    {
        return Err("检测到容器环境（/.dockerenv）：Agent 跑在容器里，重启容器不会重启宿主机".to_string());
    }
    if let Ok(cg) = std::fs::read_to_string("/proc/1/cgroup")
        && (cg.contains("docker") || cg.contains("lxc") || cg.contains("kubepods"))
    {
        return Err("检测到容器环境（cgroup）：Agent 跑在容器里，无法重启宿主机".to_string());
    }
    #[cfg(target_os = "linux")]
    {
        // 需要 root：读 /proc/self/status 的 "Uid: <real> <effective> ..."
        let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
        if let Some(uid) = status
            .lines()
            .find(|l| l.starts_with("Uid:"))
            .and_then(|l| l.split_whitespace().nth(1))
        {
            if uid != "0" {
                return Err(format!(
                    "当前以非 root 用户（uid={uid}）运行，systemctl reboot/shutdown 需要 root 权限：请把 Agent 装成系统服务（root）后重试"
                ));
            }
        }
        if !has_command("systemctl") && !has_command("shutdown") && !has_reboot_syscall_support() {
            return Err("系统缺少 systemctl/shutdown 且 reboot(2) 不可用，无法执行重启".to_string());
        }
    }
    #[cfg(windows)]
    {
        if !has_command("shutdown") && !std::path::Path::new(&format!(
            "{}\\System32\\shutdown.exe",
            std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".to_string())
        ))
        .is_file()
        {
            return Err("系统缺少 shutdown.exe，无法执行重启".to_string());
        }
    }
    Ok(())
}

/// Linux 下 `reboot(2)` 系统调用是否可用（需 root / CAP_SYS_BOOT）。
#[cfg(target_os = "linux")]
fn has_reboot_syscall_support() -> bool {
    true
}

/// 命令是否可用（在 PATH 中找得到）。
fn has_command(name: &str) -> bool {
    let paths = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&paths).any(|dir| {
        let p = dir.join(name);
        p.is_file()
            || p.with_extension("exe").is_file()
            || dir.join(format!("{name}.exe")).is_file()
    })
}

#[cfg(windows)]
fn do_reboot() -> Result<(), String> {
    // 绝对路径优先（服务环境 PATH 可能被精简）；再退化到按名字查找
    let sysroot = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".to_string());
    let exe = format!("{sysroot}\\System32\\shutdown.exe");
    let bin = if std::path::Path::new(&exe).is_file() {
        exe
    } else {
        "shutdown".to_string()
    };
    let st = std::process::Command::new(&bin)
        .args(["/r", "/t", "0", "/f"])
        .status()
        .map_err(|e| format!("调用 {bin} 失败：{e}"))?;
    if st.success() {
        Ok(())
    } else {
        Err(format!("{bin} /r 返回 {st}（服务账户需要关机权限 SeShutdownPrivilege）"))
    }
}

#[cfg(target_os = "linux")]
fn do_reboot() -> Result<(), String> {
    // ① **首选直接系统调用**（root / CAP_SYS_BOOT 下最可靠）：
    //    不依赖 PATH、不依赖 systemctl 能否连上 systemd 总线（星尘拉起的子进程 PATH 往往很精简，
    //    /sbin、/usr/sbin 可能不在其中，导致 Command::new("systemctl"/"shutdown") 直接找不到）。
    unsafe {
        libc::sync();
    }
    if unsafe { libc::reboot(libc::LINUX_REBOOT_CMD_RESTART) } == 0 {
        return Ok(());
    }
    let syscall_err = std::io::Error::last_os_error();
    let mut errs = vec![format!("reboot(2) 系统调用失败：{syscall_err}")];
    // ② 绝对路径命令兜底（不走 PATH；顺序与系统实际布局一致）
    const CANDIDATES: &[(&str, &[&str])] = &[
        ("/usr/bin/systemctl", &["reboot"]),
        ("/bin/systemctl", &["reboot"]),
        ("/usr/sbin/reboot", &[]),
        ("/sbin/reboot", &[]),
        ("/usr/sbin/shutdown", &["-r", "now"]),
        ("/sbin/shutdown", &["-r", "now"]),
    ];
    for (bin, args) in CANDIDATES {
        if !std::path::Path::new(bin).is_file() {
            continue;
        }
        match std::process::Command::new(bin).args(*args).output() {
            Ok(out) if out.status.success() => return Ok(()),
            Ok(out) => {
                let stderr = String::from_utf8_lossy(&out.stderr);
                errs.push(format!(
                    "{bin} {}：{}{}",
                    args.join(" "),
                    out.status,
                    if stderr.trim().is_empty() {
                        String::new()
                    } else {
                        format!("（{}）", stderr.trim().lines().next().unwrap_or(""))
                    }
                ));
            }
            Err(e) => errs.push(format!("{bin}：{e}")),
        }
    }
    Err(errs.join("；"))
}

#[cfg(target_os = "macos")]
fn do_reboot() -> Result<(), String> {
    let bin = if std::path::Path::new("/sbin/shutdown").is_file() {
        "/sbin/shutdown"
    } else {
        "shutdown"
    };
    let st = std::process::Command::new(bin)
        .args(["-r", "now"])
        .status()
        .map_err(|e| format!("调用 {bin} 失败：{e}"))?;
    if st.success() {
        Ok(())
    } else {
        Err(format!("{bin} 返回 {st}"))
    }
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

/// 系统运行时长文本（如 `21天13小时`）。秒数来自 `dhrust::sys::machine`（Windows/Linux）；
/// 其它平台返回 None。
pub(crate) fn uptime_text() -> Option<String> {
    if !cfg!(any(windows, target_os = "linux")) {
        return None;
    }
    let total_minutes = host_uptime_seconds() / 60;
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

/// 本机网络总流量（接收字节, 发送字节）。用于面板速率差分计算。
/// 实现已下沉到 `dhrust::sys::net::net_totals`（2026-10-01，三项目共用）。
pub(crate) fn net_total_bytes() -> Option<(u64, u64)> {
    dhrust::sys::net::net_totals().map(|(rx, tx, _)| (rx, tx))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_gmk_units() {
        assert_eq!(format_gmk(512), "512B");
        assert_eq!(format_gmk(1024), "1.0K");
        assert_eq!(format_gmk(63 * 1024 * 1024 * 1024), "63.0G");
        assert_eq!(format_gmk(3 * 1024u64 * 1024 * 1024 * 1024), "3.0T");
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
