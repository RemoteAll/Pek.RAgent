//! 系统内存回收（Linux）。
//!
//! **口径说明（对齐宝塔/同类面板的「释放内存」）**
//! - Linux 上从外部能安全回收的只有**页缓存 / 目录项 / 索引节点缓存**：
//!   `sync` 刷脏页后写 `/proc/sys/vm/drop_caches = 3`。这部分内存本来就可被回收，
//!   提前释放只是让「可用内存」立刻变大，不会伤害任何进程。
//! - **进程自己占用的匿名内存外部无法清理**（除非结束该进程）。真正泄漏的进程
//!   只能用面板「进程 Top」定位后重启对应服务——这一点在返回文案里明确告知，
//!   避免用户误以为按钮能解决所有内存问题。
//! - Windows 没有等价接口（`EmptyWorkingSet` 只能针对单个进程，且会拖慢系统），
//!   因此明确返回不支持。

/// 一次系统内存回收的结果。
#[derive(Debug, Clone, Default)]
pub(crate) struct MemReport {
    /// 回收前可用内存（字节）
    pub before_avail: u64,
    /// 回收后可用内存（字节）
    pub after_avail: u64,
    /// 回收前页缓存（字节）
    pub before_cached: u64,
    /// 回收后页缓存（字节）
    pub after_cached: u64,
}

impl MemReport {
    /// 实际释放的字节数（可用内存增量；缓存被回收后可用会上升）。
    pub(crate) fn freed(&self) -> u64 {
        self.after_avail.saturating_sub(self.before_avail)
    }
}

/// 清理系统页缓存（Linux `sync` + `drop_caches`）。
#[cfg(target_os = "linux")]
pub(crate) fn drop_caches() -> Result<MemReport, String> {
    if unsafe { libc::geteuid() } != 0 {
        return Err("需要 root 权限：请以 root 身份（或 root 运行的服务）启动代理".to_string());
    }

    let (before_avail, before_cached) = read_meminfo()?;

    // ① 先把脏页刷盘：直接 drop 会丢未落盘数据，这一步不能省
    unsafe { libc::sync() };

    // ② 3 = 页缓存 + 目录项 + 索引节点（最常用口径，与宝塔一致）
    std::fs::write("/proc/sys/vm/drop_caches", "3")
        .map_err(|e| format!("写入 /proc/sys/vm/drop_caches 失败：{e}（内核可能未开放该接口）"))?;

    let (after_avail, after_cached) = read_meminfo()?;
    crate::util::log_format(
        "系统内存回收：可用 {} → {}（页缓存 {} → {}）",
        &[
            &dhrust::web::format_bytes(before_avail),
            &dhrust::web::format_bytes(after_avail),
            &dhrust::web::format_bytes(before_cached),
            &dhrust::web::format_bytes(after_cached),
        ],
    );

    Ok(MemReport {
        before_avail,
        after_avail,
        before_cached,
        after_cached,
    })
}

/// 非 Linux：明确不支持（不做只清自身却宣称「清了系统」的假实现）。
#[cfg(not(target_os = "linux"))]
pub(crate) fn drop_caches() -> Result<MemReport, String> {
    Err("清理系统缓存仅支持 Linux；Windows 无等价接口，可对单个进程使用「释放内存」".to_string())
}

/// 读 `/proc/meminfo` 的 `MemAvailable` 与 `Cached`（字节）。
#[cfg(target_os = "linux")]
fn read_meminfo() -> Result<(u64, u64), String> {
    let text = std::fs::read_to_string("/proc/meminfo")
        .map_err(|e| format!("读取 /proc/meminfo 失败：{e}"))?;
    let mut avail = 0u64;
    let mut cached = 0u64;
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
            "MemAvailable" => avail = kb * 1024,
            "Cached" => cached = kb * 1024,
            _ => {}
        }
    }
    Ok((avail, cached))
}
