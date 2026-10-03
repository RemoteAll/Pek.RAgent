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
    process_stats, system_cpu_rate, tcp_counts, top_processes, DiskIo,
};
pub(crate) use dhrust::sys::process::{
    empty_working_set, is_alive, memory_mb, process_name, raise_priority, set_oom_score_adjust,
    signal_force, signal_graceful, spawn, stop_process, Handle, SpawnRequest,
};

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
