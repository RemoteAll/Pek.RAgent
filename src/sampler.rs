//! 后台资源采样器：固定 1 秒粒度采样整机运行指标，面板接口读取“最近快照”。
//!
//! 动机（2026-10-01 性能优化）：
//! - **口径一致**：原实现按面板请求间隔（3 秒）差分，CPU/网络/磁盘速率是 3 秒均值，
//!   与任务管理器（约 1 秒）或宝塔的读数存在窗口差异（瞬时尖峰被平滑）；
//!   统一改为后台 1 秒窗口后，读数与系统工具同粒度。
//! - **多客户端一致**：多个页面/脚本同时轮询时，差分基线不再互相干扰。
//! - **请求零采集**：状态/机器接口直接读快照；首次请求不再触发 200ms 基线采样。
//!
//! 采样线程开销：轻量项（CPU/网络/磁盘差分）每周期执行（Windows 实测 <1ms，Linux 读 /proc 微秒级）；
//! 重项（TCP 表、进程线程/句柄快照）分频执行（约 3~5 秒一次）——Windows 下单次
//! `GetExtendedTcpTable` ≈ 4ms、Toolhelp 全进程快照 ≈ 14ms（实测），无必要每秒执行。
//!
//! 采样间隔由配置 `SampleInterval` 控制（默认 1000ms；0 = 关闭，改由面板请求时现采）。

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::sys;

/// 整机采样快照（`clone` 后供请求线程读取）。
#[derive(Clone)]
pub(crate) struct Snapshot {
    /// 采样时刻（判断快照新鲜度）
    pub sampled_at: Instant,
    /// 整机 CPU 使用率（%，1 秒窗口；与任务管理器/宝塔同粒度）
    pub cpu_rate: Option<f64>,
    /// 代理进程 CPU 占用（占整机 %，1 秒窗口）
    pub agent_cpu_rate: f64,
    /// 网络上行速率（字节/秒）
    pub net_up_bps: u64,
    /// 网络下行速率（字节/秒）
    pub net_down_bps: u64,
    /// 网卡累计接收字节
    pub net_rx_total: u64,
    /// 网卡累计发送字节
    pub net_tx_total: u64,
    /// 磁盘 IOPS
    pub disk_iops: u64,
    /// 磁盘读速率（字节/秒）
    pub disk_read_bps: u64,
    /// 磁盘写速率（字节/秒）
    pub disk_write_bps: u64,
    /// 磁盘平均延迟（毫秒）
    pub disk_latency_ms: f64,
    /// 磁盘累计读字节
    pub disk_read_bytes: u64,
    /// 磁盘累计写字节
    pub disk_write_bytes: u64,
    /// TCP 连接数（ESTABLISHED）
    pub tcp_estab: u32,
    /// TCP 连接数（TIME_WAIT）
    pub tcp_time_wait: u32,
    /// TCP 连接数（CLOSE_WAIT）
    pub tcp_close_wait: u32,
    /// 代理进程线程数
    pub agent_threads: u32,
    /// 代理进程句柄数
    pub agent_handles: u32,
}

/// 最近一次快照。
static SNAPSHOT: Mutex<Option<Snapshot>> = Mutex::new(None);
/// 网络差分基线（时刻, 接收字节, 发送字节）。
static NET_LAST: Mutex<Option<(Instant, u64, u64)>> = Mutex::new(None);
/// 磁盘差分基线（时刻, 累计统计）。
static DISK_LAST: Mutex<Option<(Instant, sys::DiskIo)>> = Mutex::new(None);
/// 代理自身 CPU 采样器（采样周期间差分；`dhrust::sys::monitor` 统一实现，
/// 与 Pek.RPanlServer 概览页共用）。
static AGENT_CPU: dhrust::sys::monitor::ProcessCpuMeter = dhrust::sys::monitor::ProcessCpuMeter::new();

/// 采样间隔（毫秒；配置 `SampleInterval`，启动时写入）。
static INTERVAL_MS: AtomicU64 = AtomicU64::new(1000);

/// 启动后台采样线程（幂等；由 HTTP 面板启动路径传入配置的采样间隔）。
///
/// `interval_ms`：0 = 关闭后台采样（面板请求走 [`current`] 的就地采样兜底，与改造前一致）；
/// 其它值由配置归一化限定在 200ms~60s。
pub(crate) fn start(interval_ms: u64) {
    INTERVAL_MS.store(interval_ms, Ordering::Relaxed);
    if interval_ms == 0 {
        return;
    }

    static STARTED: std::sync::Once = std::sync::Once::new();
    STARTED.call_once(|| {
        let interval = Duration::from_millis(interval_ms);
        // 重项分频：TCP ≈ 每 3 秒、线程/句柄 ≈ 每 5 秒（与面板展示时效匹配；见 tick 注释）
        let tcp_every = (3_000u64 / interval_ms).max(1);
        let stats_every = (5_000u64 / interval_ms).max(1);
        let _ = std::thread::Builder::new()
            .name("ragent-sampler".to_string())
            .spawn(move || {
                let mut n: u64 = 0;
                let mut prev: Option<Snapshot> = None;
                loop {
                    let snapshot =
                        tick(n % tcp_every == 0, n % stats_every == 0, prev.as_ref());
                    *SNAPSHOT.lock().unwrap() = Some(snapshot.clone());
                    prev = Some(snapshot);
                    n = n.wrapping_add(1);
                    std::thread::sleep(interval);
                }
            });
    });
}

/// 当前快照：采样线程未启动/停滞（超过采样间隔的 3 倍，至少 3 秒）时就地采样一次兜底
/// （面板首次请求通常早于第一个采样周期；配置 `SampleInterval=0` 时全部走此路径）。
pub(crate) fn current() -> Snapshot {
    let threshold = Duration::from_millis(
        (INTERVAL_MS.load(Ordering::Relaxed).saturating_mul(3)).max(3_000),
    );
    let fresh = SNAPSHOT
        .lock()
        .unwrap()
        .clone()
        .filter(|s| s.sampled_at.elapsed() < threshold);
    fresh.unwrap_or_else(|| tick(true, true, None))
}

/// 采样一次：差分各项速率并汇总快照。
///
/// `with_tcp` / `with_stats` 控制“重项”是否本次执行——Windows 下单次
/// `GetExtendedTcpTable` ≈ 4ms、Toolhelp 全进程快照 ≈ 14ms（实测），对面板展示
/// 所需的时效无必要每秒执行；为 false 时沿用 `prev` 中对应的旧值。
fn tick(with_tcp: bool, with_stats: bool, prev: Option<&Snapshot>) -> Snapshot {
    // 整机 CPU：`system_cpu_rate` 自带差分基线（调用间隔 = 采样周期 → 1 秒窗口）
    let cpu_rate = sys::system_cpu_rate();

    // 代理自身 CPU 占用（首帧为“采样起点以来平均”，之后按采样周期窗口差分；
    // 差分内核在 `dhrust::sys::monitor::ProcessCpuMeter`，与 Pek.RPanlServer 共用）
    let agent_cpu_rate = AGENT_CPU.sample_since(Instant::now());

    // 网络速率与累计字节
    let (mut net_up_bps, mut net_down_bps) = (0u64, 0u64);
    let (mut net_rx_total, mut net_tx_total) = (0u64, 0u64);
    if let Some((rx, tx)) = sys::net_total_bytes() {
        net_rx_total = rx;
        net_tx_total = tx;
        let now = Instant::now();
        let mut slot = NET_LAST.lock().unwrap();
        if let Some((t, lrx, ltx)) = *slot {
            let dt = now.duration_since(t).as_secs_f64();
            if dt >= 0.2 {
                net_up_bps = ((tx.saturating_sub(ltx)) as f64 / dt) as u64;
                net_down_bps = ((rx.saturating_sub(lrx)) as f64 / dt) as u64;
            }
        }
        *slot = Some((now, rx, tx));
    }

    // 磁盘 IOPS / 读写速率 / 平均延迟
    let (mut disk_iops, mut disk_read_bps, mut disk_write_bps) = (0u64, 0u64, 0u64);
    let mut disk_latency_ms = 0.0f64;
    let (mut disk_read_bytes, mut disk_write_bytes) = (0u64, 0u64);
    if let Some(io) = sys::disk_io() {
        disk_read_bytes = io.read_bytes;
        disk_write_bytes = io.write_bytes;
        let now = Instant::now();
        let mut slot = DISK_LAST.lock().unwrap();
        if let Some((t, last)) = *slot {
            let dt = now.duration_since(t).as_secs_f64();
            if dt >= 0.2 {
                let ops = (io.reads + io.writes).saturating_sub(last.reads + last.writes);
                disk_latency_ms = if ops > 0 {
                    (io.ms_total.saturating_sub(last.ms_total) as f64 / ops as f64 * 10.0).round()
                        / 10.0
                } else {
                    0.0
                };
                disk_iops = (ops as f64 / dt) as u64;
                disk_read_bps = (io.read_bytes.saturating_sub(last.read_bytes) as f64 / dt) as u64;
                disk_write_bps =
                    (io.write_bytes.saturating_sub(last.write_bytes) as f64 / dt) as u64;
            }
        }
        *slot = Some((now, io));
    }

    // TCP 连接计数与代理进程线程/句柄（重项：按采样器分频执行，未采样周期沿用旧值）
    let (tcp_estab, tcp_time_wait, tcp_close_wait) = if with_tcp {
        sys::tcp_counts()
    } else {
        prev.map(|p| (p.tcp_estab, p.tcp_time_wait, p.tcp_close_wait))
            .unwrap_or((0, 0, 0))
    };
    let (agent_threads, agent_handles) = if with_stats {
        sys::process_stats(std::process::id()).unwrap_or((0, 0))
    } else {
        prev.map(|p| (p.agent_threads, p.agent_handles))
            .unwrap_or((0, 0))
    };

    Snapshot {
        sampled_at: Instant::now(),
        cpu_rate,
        agent_cpu_rate,
        net_up_bps,
        net_down_bps,
        net_rx_total,
        net_tx_total,
        disk_iops,
        disk_read_bps,
        disk_write_bps,
        disk_latency_ms,
        disk_read_bytes,
        disk_write_bytes,
        tcp_estab,
        tcp_time_wait,
        tcp_close_wait,
        agent_threads,
        agent_handles,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_samples_with_cpu_rate() {
        // 采样线程未启动时就地采样；CPU 速率需两次采样建立差分窗口（最长等 2 秒）
        let _ = current();
        let mut rate = None;
        for _ in 0..20 {
            std::thread::sleep(Duration::from_millis(100));
            if let Some(value) = current().cpu_rate {
                rate = Some(value);
                break;
            }
        }
        let rate = rate.expect("2 秒内应取得 CPU 使用率");
        assert!((0.0..=100.0).contains(&rate), "CPU 使用率应在 0~100：{rate}");
    }

    /// 手动基准：`cargo test bench_sampler_tick -- --ignored --nocapture`
    /// （每项取最快一次；用于评估采样开销与分频优化效果）
    #[test]
    #[ignore = "手动基准"]
    fn bench_sampler_tick() {
        fn timeit(name: &str, mut f: impl FnMut()) {
            for _ in 0..2 {
                f(); // 预热（含首次基线采样）
            }
            let mut best = Duration::MAX;
            for _ in 0..8 {
                let t = Instant::now();
                f();
                best = best.min(t.elapsed());
            }
            println!("{name}: {best:?}");
        }

        timeit("system_cpu_rate", || {
            let _ = sys::system_cpu_rate();
        });
        timeit("net_total_bytes", || {
            let _ = sys::net_total_bytes();
        });
        timeit("disk_io", || {
            let _ = sys::disk_io();
        });
        timeit("tcp_counts", || {
            let _ = sys::tcp_counts();
        });
        timeit("process_stats", || {
            let _ = sys::process_stats(std::process::id());
        });
        timeit("light tick", || {
            let _ = tick(false, false, None);
        });
        timeit("full tick", || {
            let _ = tick(true, true, None);
        });
    }
}
