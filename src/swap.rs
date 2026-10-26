//! 虚拟内存（swap）在线配置（Linux）：创建/启用交换文件、关闭并删除、调整 swappiness。
//!
//! **为什么放在代理里**：服务器物理内存不足时，运维往往只剩面板可用；
//! 让代理直接建 swap，比让用户在终端手敲 `fallocate / mkswap / swapon` 更不容易出错，
//! 也避免「先把文件建好再忘了 mkswap」这类半成品状态。
//!
//! 安全边界（全部是硬约束，不提供开关）：
//! - 仅 Linux、仅 root（euid=0）；Windows 的页面文件必须改注册表并重启，面板只读展示；
//! - 路径必须在白名单目录下、只允许 `[A-Za-z0-9/._-]` —— 杜绝命令注入与误写系统目录；
//! - 大小 128 MB ~ 64 GB，且不超过所在分区可用空间的 90%；
//! - 已存在同名文件、或已在 `/proc/swaps` 中 → 拒绝，绝不覆盖用户文件。

use serde_json::{json, Value as Json};

/// 允许创建交换文件的目录（含其子目录）。
#[cfg(target_os = "linux")]
const ALLOWED_DIRS: &[&str] = &["/", "/www", "/data", "/opt", "/var", "/home", "/mnt", "/media"];
/// 交换文件大小范围（MB）。
const MIN_SIZE_MB: u64 = 128;
const MAX_SIZE_MB: u64 = 65_536;
/// `/etc/fstab` 中标识「由本代理管理」的注释行。
#[cfg(target_os = "linux")]
const FSTAB_MARK: &str = "# pek-ragent swap";

/// 面板「虚拟内存」区块数据。
///
/// 合并了平台能力（是否支持在线配置）、权限（是否 root）、当前用量与条目列表，
/// 前端据此决定按钮可用状态与提示文案。
pub(crate) fn status_json() -> Json {
    let info = dhrust::sys::machine::swap_info();
    let entries: Vec<Json> = info
        .entries
        .iter()
        .map(|e| {
            json!({
                "path": e.path,
                "kind": e.kind,
                "sizeMB": e.size_bytes / 1024 / 1024,
                "usedMB": e.used_bytes / 1024 / 1024,
                "sizeText": dhrust::web::format_bytes(e.size_bytes),
                "usedText": dhrust::web::format_bytes(e.used_bytes),
                "priority": e.priority,
            })
        })
        .collect();
    let rate = if info.total_bytes > 0 {
        info.used_bytes as f64 / info.total_bytes as f64
    } else {
        0.0
    };

    let supported = cfg!(target_os = "linux");
    let root = is_root();
    let hint = if !supported {
        "Windows 的页面文件需在「系统属性 → 高级 → 性能 → 虚拟内存」中设置并重启生效，面板仅展示用量"
    } else if !root {
        "是否需要 root 才能创建：当前代理不是以 root 运行，创建/关闭按钮会被拒绝"
    } else {
        ""
    };

    // 推荐值：按物理内存规模估算（服务器无休眠场景），并扣除已启用的交换区
    let phys_mb = crate::sys::memory_info()
        .map(|(total, _)| total / 1024 / 1024)
        .unwrap_or(0);
    let recommended_total = recommended_swap_mb(phys_mb);
    let current_mb = info.total_bytes / 1024 / 1024;
    let recommended_add = {
        let add = recommended_total.saturating_sub(current_mb);
        if add < MIN_SIZE_MB { 0 } else { add }
    };

    json!({
        "supported": supported,
        "configurable": info.configurable,
        "root": root,
        "totalMB": info.total_bytes / 1024 / 1024,
        "usedMB": info.used_bytes / 1024 / 1024,
        "totalText": dhrust::web::format_bytes(info.total_bytes),
        "usedText": dhrust::web::format_bytes(info.used_bytes),
        "usedPercent": format!("{:.1}%", rate * 100.0),
        "swappiness": info.swappiness,
        "entries": entries,
        "defaultPath": "/swapfile",
        "minMB": MIN_SIZE_MB,
        "maxMB": MAX_SIZE_MB,
        "physMB": phys_mb,
        "recommendedTotalMB": recommended_total,
        "recommendedMB": recommended_add,
        "hint": hint,
    })
}

/// 推荐交换区总量（MB）：服务器（无休眠场景）的经验值。
///
/// - 物理内存 ≤ 2 GB：2 × 物理内存（小内存机器最容易 OOM）
/// - 2 GB ~ 8 GB：等于物理内存
/// - > 8 GB：8 GB
///
/// 结果向上对齐到 512 MB，并夹在 `[512, 32768]`；物理内存读不到时给 2048。
fn recommended_swap_mb(phys_mb: u64) -> u64 {
    if phys_mb == 0 {
        return 2048;
    }
    let raw = if phys_mb <= 2048 {
        phys_mb * 2
    } else if phys_mb <= 8192 {
        phys_mb
    } else {
        8192
    };
    (raw.div_ceil(512) * 512).clamp(512, 32_768)
}

/// 创建并启用交换文件（`persist=true` 时写入 `/etc/fstab`，重启后仍生效）。
#[cfg(target_os = "linux")]
pub(crate) fn create(path: &str, size_mb: u64, persist: bool) -> Result<String, String> {
    ensure_root()?;
    let path = normalize_path(path)?;
    validate_size(size_mb)?;
    if swap_enabled(&path) {
        return Err(format!("{path} 已经是启用中的交换区"));
    }
    if std::path::Path::new(&path).exists() {
        return Err(format!("{path} 已存在，请换一个路径或先删除该文件"));
    }
    ensure_disk_space(&path, size_mb)?;

    crate::util::log_format(
        "虚拟内存：创建交换文件 {}（{} MB，持久化={}）",
        &[&path, &size_mb.to_string(), &persist.to_string()],
    );

    // ① 分配文件：fallocate 最快；文件系统不支持时回退 dd
    let mut r = run(&format!("fallocate -l {size_mb}M {path}"), 180);
    if r.exit_code != 0 {
        let _ = run(&format!("rm -f {path}"), 30);
        r = run(
            &format!("dd if=/dev/zero of={path} bs=1M count={size_mb} status=none"),
            600,
        );
    }
    if r.exit_code != 0 {
        let _ = run(&format!("rm -f {path}"), 30);
        return Err(format!("创建交换文件失败：{}", err_text(&r)));
    }

    // ② 权限 / 格式化 / 启用（任一步失败都回收半成品）
    for (cmd, title) in [
        (format!("chmod 600 {path}"), "设置文件权限"),
        (format!("mkswap {path}"), "格式化交换文件"),
        (format!("swapon {path}"), "启用交换区"),
    ] {
        let r = run(&cmd, 180);
        if r.exit_code != 0 {
            let _ = run(&format!("swapoff {path}"), 120);
            let _ = run(&format!("rm -f {path}"), 30);
            return Err(format!("{title}失败：{}", err_text(&r)));
        }
    }

    // ③ 持久化（失败不影响本次已生效）
    let mut note = String::new();
    if persist {
        match append_fstab(&path) {
            Ok(()) => note = "，已写入 /etc/fstab（重启后自动启用）".to_string(),
            Err(e) => {
                note = format!("，但写入 /etc/fstab 失败：{e}（本次已生效，重启后失效）")
            }
        }
    }

    let info = dhrust::sys::machine::swap_info();
    Ok(format!(
        "已启用 {size_mb} MB 虚拟内存，当前总量 {}{note}",
        dhrust::web::format_bytes(info.total_bytes)
    ))
}

/// 关闭并删除交换文件（同时清理 `/etc/fstab` 中由本代理写入的记录）。
#[cfg(target_os = "linux")]
pub(crate) fn remove(path: &str) -> Result<String, String> {
    ensure_root()?;
    let path = normalize_path(path)?;
    if !swap_enabled(&path) {
        return Err(format!("{path} 不是当前启用中的交换区"));
    }

    crate::util::log_format("虚拟内存：关闭交换区 {}", &[&path]);
    // 内存紧张时 swapoff 需要把已换出的页拉回内存，可能很慢
    let r = run(&format!("swapoff {path}"), 600);
    if r.exit_code != 0 {
        return Err(format!(
            "关闭失败：{}（物理内存不足时 swapoff 可能失败，请先释放内存后重试）",
            err_text(&r)
        ));
    }

    if std::path::Path::new(&path).is_file() {
        let r = run(&format!("rm -f {path}"), 30);
        if r.exit_code != 0 {
            return Err(format!(
                "交换区已关闭，但删除文件失败：{}",
                err_text(&r)
            ));
        }
    }

    let note = match remove_fstab(&path) {
        Ok(true) => "，已移除 /etc/fstab 记录",
        Ok(false) => "",
        Err(_) => "，但清理 /etc/fstab 失败（请手工检查）",
    };
    Ok(format!("已关闭并删除 {path}{note}"))
}

/// 调整 `vm.swappiness`（0~100；同时写入 `/etc/sysctl.d/99-pek-swap.conf` 持久化）。
#[cfg(target_os = "linux")]
pub(crate) fn set_swappiness(value: u32) -> Result<String, String> {
    ensure_root()?;
    if value > 100 {
        return Err("swappiness 取值 0~100".to_string());
    }
    std::fs::write("/proc/sys/vm/swappiness", value.to_string())
        .map_err(|e| format!("写入 /proc/sys/vm/swappiness 失败：{e}"))?;
    let conf = "/etc/sysctl.d/99-pek-swap.conf";
    let body = format!("# managed by Pek.RAgent\nvm.swappiness = {value}\n");
    let note = if std::fs::write(conf, body).is_ok() {
        "（已写入 /etc/sysctl.d/99-pek-swap.conf 持久化）"
    } else {
        "（本次生效，未能持久化）"
    };
    crate::util::log_format("虚拟内存：swappiness = {}", &[&value.to_string()]);
    Ok(format!("swappiness 已设为 {value}{note}"))
}

// ————— 非 Linux：明确不支持（不做半吊子实现） —————

#[cfg(not(target_os = "linux"))]
pub(crate) fn create(_path: &str, _size_mb: u64, _persist: bool) -> Result<String, String> {
    Err("虚拟内存在线配置目前仅支持 Linux（Windows 页面文件需改注册表并重启）".to_string())
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn remove(_path: &str) -> Result<String, String> {
    Err("虚拟内存在线配置目前仅支持 Linux".to_string())
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn set_swappiness(_value: u32) -> Result<String, String> {
    Err("虚拟内存在线配置目前仅支持 Linux".to_string())
}

// ————— 平台与权限 —————

#[cfg(target_os = "linux")]
fn is_root() -> bool {
    unsafe { libc::geteuid() == 0 }
}

#[cfg(not(target_os = "linux"))]
fn is_root() -> bool {
    false
}

#[cfg(target_os = "linux")]
fn ensure_root() -> Result<(), String> {
    if is_root() {
        Ok(())
    } else {
        Err("需要 root 权限：请以 root 身份（或 root 运行的服务）启动代理".to_string())
    }
}

// ————— Linux 实现细节 —————

/// 单条命令执行（超时上限见 `dhrust::sys::process::run_shell`）。
#[cfg(target_os = "linux")]
fn run(cmd: &str, timeout_seconds: u64) -> dhrust::sys::process::ShellResult {
    dhrust::sys::process::run_shell(cmd, std::path::Path::new("/"), timeout_seconds)
}

#[cfg(target_os = "linux")]
fn err_text(r: &dhrust::sys::process::ShellResult) -> String {
    let text = if r.error.trim().is_empty() {
        r.output.trim()
    } else {
        r.error.trim()
    };
    if text.is_empty() {
        format!("退出码 {}", r.exit_code)
    } else {
        text.lines().take(3).collect::<Vec<_>>().join("; ")
    }
}

/// 路径校验：绝对路径 + 字符白名单 + 父目录白名单 + 禁 `..`。
#[cfg(target_os = "linux")]
fn normalize_path(raw: &str) -> Result<String, String> {
    let path = raw.trim();
    if path.is_empty() {
        return Err("请填写交换文件路径（如 /swapfile）".to_string());
    }
    if !path.starts_with('/') {
        return Err("路径必须是绝对路径（如 /swapfile）".to_string());
    }
    if path.ends_with('/') {
        return Err("请填写文件路径而不是目录".to_string());
    }
    if !path
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'.' | b'_' | b'-'))
    {
        return Err("路径只能包含字母、数字、斜杠、点、下划线、连字符".to_string());
    }
    if path.split('/').any(|seg| seg == "..") {
        return Err("路径不能包含 ..".to_string());
    }
    let parent = std::path::Path::new(path)
        .parent()
        .and_then(|p| p.to_str())
        .filter(|p| !p.is_empty())
        .unwrap_or("/");
    let allowed = ALLOWED_DIRS.iter().any(|dir| {
        if *dir == "/" {
            parent == "/"
        } else {
            parent == *dir || parent.starts_with(&format!("{dir}/"))
        }
    });
    if !allowed {
        return Err(format!(
            "不允许在 {parent} 下创建交换文件（可用：{}）",
            ALLOWED_DIRS.join(" ")
        ));
    }
    Ok(path.to_string())
}

#[cfg(target_os = "linux")]
fn validate_size(size_mb: u64) -> Result<(), String> {
    if !(MIN_SIZE_MB..=MAX_SIZE_MB).contains(&size_mb) {
        return Err(format!(
            "大小需在 {MIN_SIZE_MB} MB ~ {MAX_SIZE_MB} MB 之间"
        ));
    }
    Ok(())
}

/// 该路径是否已在 `/proc/swaps` 中启用。
#[cfg(target_os = "linux")]
fn swap_enabled(path: &str) -> bool {
    std::fs::read_to_string("/proc/swaps")
        .map(|text| {
            text.lines()
                .skip(1)
                .filter_map(|line| line.split_whitespace().next())
                .any(|p| p == path)
        })
        .unwrap_or(false)
}

/// 分区可用空间检查（最多占用可用空间的 90%）。
#[cfg(target_os = "linux")]
fn ensure_disk_space(path: &str, size_mb: u64) -> Result<(), String> {
    let dir = std::path::Path::new(path)
        .parent()
        .unwrap_or(std::path::Path::new("/"));
    let out = dhrust::sys::process::run("df", &["-P", "-B1", &dir.display().to_string()]);
    if out.0 != 0 {
        return Ok(()); // 拿不到就用系统命令自身报错兜底
    }
    let Some(line) = out.1.lines().nth(1) else {
        return Ok(());
    };
    let Some(avail) = line
        .split_whitespace()
        .nth(3)
        .and_then(|v| v.parse::<u64>().ok())
    else {
        return Ok(());
    };
    let need = size_mb * 1024 * 1024;
    if need > avail.saturating_mul(9) / 10 {
        return Err(format!(
            "空间不足：{} 可用 {}，本次需要 {}（最多使用可用空间的 90%）",
            dir.display(),
            dhrust::web::format_bytes(avail),
            dhrust::web::format_bytes(need)
        ));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn fstab_line(path: &str) -> String {
    format!("{path} none swap sw 0 0")
}

/// 追加 `/etc/fstab` 记录（先备份为 `.pek-bak`；已有同名记录则跳过）。
#[cfg(target_os = "linux")]
fn append_fstab(path: &str) -> Result<(), String> {
    let conf = "/etc/fstab";
    let old = std::fs::read_to_string(conf).map_err(|e| format!("读取 {conf} 失败：{e}"))?;
    let target = fstab_line(path);
    if old.lines().any(|l| l.trim() == target) {
        return Ok(());
    }
    let _ = std::fs::copy(conf, "/etc/fstab.pek-bak");
    let sep = if old.is_empty() || old.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    let body = format!("{old}{sep}{FSTAB_MARK}\n{target}\n");
    std::fs::write(conf, body).map_err(|e| format!("写入 {conf} 失败：{e}"))
}

/// 移除本代理写入的 `/etc/fstab` 记录（返回是否有改动）。
#[cfg(target_os = "linux")]
fn remove_fstab(path: &str) -> Result<bool, String> {
    let conf = "/etc/fstab";
    let Ok(text) = std::fs::read_to_string(conf) else {
        return Ok(false);
    };
    let target = fstab_line(path);
    let lines: Vec<&str> = text.lines().collect();
    let mut out: Vec<&str> = Vec::with_capacity(lines.len());
    let mut changed = false;
    let mut i = 0usize;
    while i < lines.len() {
        let line = lines[i];
        // 标记注释 + 紧随其后的目标行：成对删除
        if line.trim() == FSTAB_MARK && lines.get(i + 1).map(|l| l.trim()) == Some(target.as_str())
        {
            changed = true;
            i += 2;
            continue;
        }
        if line.trim() == target {
            changed = true;
            i += 1;
            continue;
        }
        out.push(line);
        i += 1;
    }
    if !changed {
        return Ok(false);
    }
    let _ = std::fs::copy(conf, "/etc/fstab.pek-bak");
    let mut body = out.join("\n");
    body.push('\n');
    std::fs::write(conf, body).map_err(|e| format!("写入 {conf} 失败：{e}"))?;
    Ok(true)
}

#[cfg(test)]
mod recommend_tests {
    use super::*;

    #[test]
    fn recommends_swap_by_physical_memory() {
        // 小内存机器：2 倍物理内存
        assert_eq!(recommended_swap_mb(1024), 2048);
        assert_eq!(recommended_swap_mb(2048), 4096);
        // 中等内存：等于物理内存
        assert_eq!(recommended_swap_mb(4096), 4096);
        assert_eq!(recommended_swap_mb(8192), 8192);
        // 大内存：固定 8 GB
        assert_eq!(recommended_swap_mb(16384), 8192);
        assert_eq!(recommended_swap_mb(65536), 8192);
        // 读不到物理内存时的兜底
        assert_eq!(recommended_swap_mb(0), 2048);
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn rejects_relative_and_traversal_paths() {
        assert!(normalize_path("swapfile").is_err());
        assert!(normalize_path("/www/../etc/swapfile").is_err());
        assert!(normalize_path("/etc/swapfile").is_err());
        assert!(normalize_path("/swapfile; rm -rf /").is_err());
        assert!(normalize_path("/swapfile").is_ok());
        assert!(normalize_path("/www/swap/swapfile").is_ok());
    }

    #[test]
    fn validates_size_range() {
        assert!(validate_size(64).is_err());
        assert!(validate_size(1024).is_ok());
        assert!(validate_size(MAX_SIZE_MB + 1).is_err());
    }
}
