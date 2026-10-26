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
    process_stats, system_cpu_rate, tcp_counts, top_processes, top_processes_union, DiskIo,
    ProcItem,
};
pub(crate) use dhrust::sys::process::{
    empty_working_set, is_alive, memory_mb, process_name, raise_priority, set_oom_score_adjust,
    signal_force, signal_graceful, spawn, stop_process, working_set_trim_supported, Handle,
    SpawnRequest,
};

// 监听端口查询已下沉 `dhrust::sys::monitor`：进程 Top（`ProcItem.ports`）与
// 「未配置 HealthCheck 时自动探活」共用同一份快照，避免两套实现口径漂移。
// 再导出保持本模块既有调用路径 `sys::listen_ports_of` 不变。
pub(crate) use dhrust::sys::monitor::listen_ports_of;

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
///
/// 格式化复用 `dhrust::web::uptime_text_compact`（星尘 C# 口径，与 Rust 面板一致）。
pub(crate) fn uptime_text() -> Option<String> {
    if !cfg!(any(windows, target_os = "linux")) {
        return None;
    }
    Some(dhrust::web::uptime_text_compact(host_uptime_seconds()))
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
