//! 端口流量统计（PortTraffic）：统计服务器各端口（TCP/UDP）的收发流量与连接数。
//!
//! 两种数据源：
//! - **nftables 计数（Linux，主方案）**：在独立表 `inet pek_stats` 中为每个端口添加
//!   `counter` 规则（input 匹配 `dport` / output 匹配 `sport`），内核持续累计字节数
//!   （含已关闭连接），读取绝对值差分出实时速率。只计数、不干预转发（policy accept），
//!   与系统防火墙并存；需要 root（CAP_NET_ADMIN）与 `nft` 命令。表在代理重启后保留
//!   （累计不丢），`PortTraffic` 关闭或卸载时删除；nft 不可用时自动降级为连接视图。
//! - **连接视图（兜底，全平台）**：从 `/proc/net/*`（Linux）或连接表 API（Windows）
//!   统计各端口连接数。Windows 暂无廉价的内核字节计数接口，仅提供连接视图。
//!
//! 端口集合：配置 `PortTrafficPorts`（如 `22,80,443`）优先，留空自动取系统监听端口
//! （上限 64 个）。采样间隔 5 秒；快照供 Web 面板 `/star/portTraffic` 展示。
//!
//! 每日归档（nft 计数模式）：按采样差分累计当日各端口收发（计数器回退时只增不减），
//! 跨天把上一日最终值写入 SQLite（`Data/traffic.db`，模型 `Entity/Model.xml`）；当日累计
//! 持久化在 `Data/port_traffic.json`（30 秒节流），重启同日续算、跨天自动收尾。

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{json, Value as Json};

use crate::config::AgentConfig;
use crate::history;
use crate::manager::AppManager;
use crate::util;

/// nftables 计数表（inet：同时覆盖 IPv4/IPv6；独立表不影响系统防火墙）。
const TABLE: &str = "inet pek_stats";
/// 采样间隔。
const SAMPLE_INTERVAL: Duration = Duration::from_secs(5);
/// 自动监听端口的上限（防规则爆炸）。
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const MAX_AUTO_PORTS: usize = 64;
/// 配置端口的上限（防御性）。
const MAX_CONFIGURED_PORTS: usize = 256;
/// 降级后重试恢复 nft 计数的间隔。
const RETRY_INTERVAL: Duration = Duration::from_secs(300);
/// 采样状态与每日归档的持久化节流间隔。
const PERSIST_INTERVAL: Duration = Duration::from_secs(30);
/// 展示行上限（排序后截断：低端口优先，高位临时端口噪音被截断）。
const MAX_ROWS: usize = 150;

/// 协议。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
enum Proto {
    Tcp,
    Udp,
}

impl Proto {
    fn text(self) -> &'static str {
        match self {
            Proto::Tcp => "tcp",
            Proto::Udp => "udp",
        }
    }
}

/// 方向（相对端口：In = 到达该端口的流量，Out = 从该端口发出的流量）。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
enum Dir {
    In,
    Out,
}

/// 运行模式。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    /// 未启用
    Off,
    /// nftables 字节计数
    Nft,
    /// 连接视图（无字节）
    View,
    /// 平台不支持
    Unsupported,
}

impl Mode {
    fn text(self) -> &'static str {
        match self {
            Mode::Off => "off",
            Mode::Nft => "nft",
            Mode::View => "view",
            Mode::Unsupported => "unsupported",
        }
    }
}

/// 连接视图条目。
#[derive(Default, Clone, Copy)]
struct ViewEntry {
    /// 是否监听中（TCP LISTEN / UDP 绑定）
    listen: bool,
    /// 当前连接数（TCP ESTABLISHED / UDP 绑定数）
    conns: u32,
}

/// 展示行。
#[derive(Clone, Debug)]
struct Row {
    port: u16,
    proto: Proto,
    listen: bool,
    conns: u32,
    /// 累计接收字节（无计数能力时为 None）
    rx_bytes: Option<u64>,
    /// 累计发送字节
    tx_bytes: Option<u64>,
    /// 实时接收速率（字节/秒）
    rx_bps: Option<u64>,
    /// 实时发送速率
    tx_bps: Option<u64>,
}

impl Row {
    fn new(port: u16, proto: Proto) -> Row {
        Row {
            port,
            proto,
            listen: false,
            conns: 0,
            rx_bytes: None,
            tx_bytes: None,
            rx_bps: None,
            tx_bps: None,
        }
    }
}

/// 模块共享状态。
struct Inner {
    mode: Mode,
    message: String,
    rows: Vec<Row>,
    /// 上一轮累计计数（(协议, 端口) → (接收, 发送)），用于速率差分
    last_counters: HashMap<(Proto, u16), (u64, u64)>,
    last_sample: Option<Instant>,
    /// 上次尝试恢复 nft 模式的时间（降级后定期重试）
    last_retry: Option<Instant>,
    /// 当日归档日期（本地 `%Y-%m-%d`；空 = 尚未起算）
    day_date: String,
    /// 当日累计（按采样差分累计；重启从 `Data/port_traffic.json` 恢复）
    day_totals: HashMap<(Proto, u16), (u64, u64)>,
    /// 上次持久化时间（采样状态与历史日文件的 30 秒节流）
    last_persist: Option<Instant>,
}

/// 端口流量统计实例。
struct PortStat {
    base: PathBuf,
    enabled: AtomicBool,
    inner: Mutex<Inner>,
}

/// 全局实例（面板快照读取）。
static INSTANCE: OnceLock<Arc<PortStat>> = OnceLock::new();

// ————— 生命周期 —————

/// 启动后台线程（幂等；由 HTTP 面板启动路径调用）。
pub(crate) fn start(manager: Arc<AppManager>) {
    static STARTED: std::sync::Once = std::sync::Once::new();
    STARTED.call_once(|| {
        let stat = Arc::new(PortStat {
            base: manager.base().to_path_buf(),
            enabled: AtomicBool::new(false),
            inner: Mutex::new(Inner {
                mode: Mode::Off,
                message: String::new(),
                rows: Vec::new(),
                last_counters: HashMap::new(),
                last_sample: None,
                last_retry: None,
                day_date: String::new(),
                day_totals: HashMap::new(),
                last_persist: None,
            }),
        });
        {
            let mut inner = stat.inner.lock().unwrap();
            load_state(&stat.base, &mut inner);
        }
        let _ = INSTANCE.set(stat.clone());

        let _ = std::thread::Builder::new()
            .name("ragent-portstat".to_string())
            .spawn(move || run_loop(stat, manager));
    });
}

/// 卸载清理：尽力删除计数表（忽略错误；无表/无权限均静默）。
pub(crate) fn cleanup() {
    if cfg!(target_os = "linux") {
        drop_table();
    }
}

/// 后台主循环。
fn run_loop(t: Arc<PortStat>, manager: Arc<AppManager>) {
    let mut last_fp: Option<(bool, String)> = None;
    loop {
        if crate::agent::SHUTDOWN.load(Ordering::SeqCst) {
            flush(&t);
            break;
        }

        let cfg = manager.config();
        let fp = (cfg.port_traffic, cfg.port_traffic_ports.clone());
        if last_fp.as_ref() != Some(&fp) {
            last_fp = Some(fp);
            reconfigure(&t, &cfg);
        }

        if t.enabled.load(Ordering::Relaxed) {
            sample(&t, &cfg);
            retry_nft_if_degraded(&t);
        }
        std::thread::sleep(SAMPLE_INTERVAL);
    }
}

/// 应用配置变化：开关切换与模式重判。
fn reconfigure(t: &PortStat, cfg: &AgentConfig) {
    let enable = cfg.port_traffic;
    t.enabled.store(enable, Ordering::Relaxed);

    let inner = &mut *t.inner.lock().unwrap();
    inner.rows.clear();
    inner.last_counters.clear();
    inner.last_sample = None;
    inner.last_retry = None;
    inner.message.clear();

    if !enable {
        // 关闭：尽力删除计数表（内核中的累计一并清除）
        if cfg!(target_os = "linux") {
            drop_table();
        }
        inner.mode = Mode::Off;
        return;
    }

    if cfg!(target_os = "linux") {
        match ensure_table() {
            Ok(()) => {
                inner.mode = Mode::Nft;
                util::log_info(
                    "端口流量统计已启用（nftables 计数表 inet pek_stats；关闭开关或卸载时自动清理）",
                );
            }
            Err(e) => {
                inner.mode = Mode::View;
                inner.message = format!("nftables 不可用（{e}）；仅显示连接视图（需要 root 与 nft 命令）");
                util::log_format("端口流量统计降级为连接视图：{}", &[&e]);
            }
        }
    } else if cfg!(windows) {
        inner.mode = Mode::View;
        inner.message =
            "Windows 平台暂不支持端口字节计数（无廉价内核接口），当前显示连接视图".to_string();
    } else {
        inner.mode = Mode::Unsupported;
        inner.message = "当前平台暂不支持端口流量统计".to_string();
    }
}

/// 降级（View）后定期重试恢复 nft 计数（例如 nft 被安装 / 权限变化）。
fn retry_nft_if_degraded(t: &PortStat) {
    if !cfg!(target_os = "linux") {
        return;
    }
    let should = {
        let inner = t.inner.lock().unwrap();
        inner.mode == Mode::View
            && inner
                .last_retry
                .map(|at| at.elapsed() >= RETRY_INTERVAL)
                .unwrap_or(true)
    };
    if !should {
        return;
    }
    let inner = &mut *t.inner.lock().unwrap();
    inner.last_retry = Some(Instant::now());
    if ensure_table().is_ok() {
        inner.mode = Mode::Nft;
        inner.message.clear();
        inner.last_counters.clear();
        inner.last_sample = None;
        util::log_info("端口流量统计恢复 nftables 计数模式");
    }
}

/// 单次采样：连接视图 + （nft 模式时）计数规则同步与读数。
fn sample(t: &PortStat, cfg: &AgentConfig) {
    let views = conn_views();
    let configured = parse_port_list(&cfg.port_traffic_ports);
    let today = history::today_string();

    let inner = &mut *t.inner.lock().unwrap();
    // 历史保留清理（内部每天最多执行一次，开销可忽略）
    history::maybe_prune(&t.base, cfg.traffic_history_days);
    match inner.mode {
        Mode::Nft => {
            let ports = if configured.is_empty() {
                listen_ports()
            } else {
                configured.clone()
            };
            match sync_rules(&ports) {
                Ok(rules) => {
                    let now = Instant::now();
                    let dt = inner
                        .last_sample
                        .map(|s| now.duration_since(s).as_secs_f64())
                        .filter(|d| *d >= 0.5)
                        .unwrap_or(0.0);
                    let (rows, counters) =
                        rows_nft(&rules, &ports, &views, &inner.last_counters, dt);
                    // 每日归档累计（必须在 last_counters 更新前，用上一轮读数做差分）
                    accumulate_daily(&t.base, inner, &counters, &today);
                    inner.rows = rows;
                    inner.last_counters = counters;
                    inner.last_sample = Some(now);
                    inner.message.clear();
                    // 采样状态持久化与每日归档（30 秒节流）
                    let due = inner
                        .last_persist
                        .map(|at| at.elapsed() >= PERSIST_INTERVAL)
                        .unwrap_or(true);
                    if due {
                        save_state(&t.base, inner);
                        write_day_file(&t.base, inner);
                        inner.last_persist = Some(Instant::now());
                    }
                }
                Err(e) => {
                    // 运行中失败（nft 被移除/权限变化等）：降级连接视图
                    inner.mode = Mode::View;
                    inner.message = format!("nftables 计数不可用（{e}），已降级为连接视图");
                    util::log_format("端口流量计数失败，降级为连接视图：{}", &[&e]);
                    inner.rows = rows_view(&configured, &views);
                    inner.last_counters.clear();
                    inner.last_sample = Some(Instant::now());
                    inner.last_retry = Some(Instant::now());
                }
            }
        }
        Mode::View => {
            inner.rows = rows_view(&configured, &views);
        }
        Mode::Off | Mode::Unsupported => {}
    }
}

// ————— 持久化与每日归档 —————

/// 采样状态持久化路径。
fn state_path(base: &Path) -> PathBuf {
    base.join("Data").join("port_traffic.json")
}

/// 每日归档累计：按“与上一轮读数差分”累计当日各端口收发。
///
/// 必须在 `last_counters` 更新前调用（差分基准）。计数器回退（计数表被重建/清空）时
/// 差分为 0，当日累计只增不减；跨天时先把上一日最终值写入历史日文件，再开新一日。
fn accumulate_daily(
    base: &Path,
    inner: &mut Inner,
    counters: &HashMap<(Proto, u16), (u64, u64)>,
    today: &str,
) {
    if inner.day_date != today {
        write_day_file_for(base, &inner.day_date, &inner.day_totals);
        inner.day_date = today.to_string();
        inner.day_totals.clear();
    }
    for (key, cur) in counters {
        let prev = inner.last_counters.get(key).copied().unwrap_or(*cur);
        let entry = inner.day_totals.entry(*key).or_insert((0, 0));
        entry.0 = entry.0.saturating_add(cur.0.saturating_sub(prev.0));
        entry.1 = entry.1.saturating_add(cur.1.saturating_sub(prev.1));
    }
}

/// 写入当前日累计到历史库（空值跳过；同键重复写为幂等覆盖）。
fn write_day_file(base: &Path, inner: &Inner) {
    write_day_file_for(base, &inner.day_date, &inner.day_totals);
}

/// 写入指定日期的端口累计到 SQLite 历史库（仅归档有流量的端口）。
///
/// 自动发现模式下会往历史里带入大量未产生流量的监听端口（含高位临时端口），
/// 零值记录既无信息量也干扰面板展示，故只写入当日收发不全为 0 的端口。
fn write_day_file_for(base: &Path, date: &str, totals: &HashMap<(Proto, u16), (u64, u64)>) {
    if date.is_empty() {
        return;
    }
    let mut ports: BTreeMap<String, history::PortCounters> = BTreeMap::new();
    for ((proto, port), (rx, tx)) in totals {
        if *rx == 0 && *tx == 0 {
            continue;
        }
        ports.insert(
            history::port_key(proto.text(), *port),
            history::PortCounters { rx: *rx, tx: *tx },
        );
    }
    if !ports.is_empty() {
        history::update_ports(base, date, &ports);
    }
}

/// 保存采样状态（日期 + 当日累计；失败仅日志）。
fn save_state(base: &Path, inner: &Inner) {
    let path = state_path(base);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let text = match serde_json::to_string_pretty(&json!({
        "date": inner.day_date,
        "updated": chrono::Local::now().timestamp(),
        "totals": counters_json(&inner.day_totals),
    })) {
        Ok(t) => t,
        Err(e) => {
            util::log_error(&format!("端口流量持久化序列化失败：{e}"));
            return;
        }
    };
    if let Err(e) = dhrust::io::write_all_text_atomic(&path, &text) {
        util::log_error(&format!("端口流量持久化写入失败：{e}"));
    }
}

/// 计数器映射 → JSON（键 `tcp:80`，值 `[rx, tx]`）。
fn counters_json(map: &HashMap<(Proto, u16), (u64, u64)>) -> Json {
    let mut obj = serde_json::Map::new();
    for ((proto, port), (rx, tx)) in map {
        obj.insert(history::port_key(proto.text(), *port), json!([rx, tx]));
    }
    Json::Object(obj)
}

/// JSON → 计数器映射（非法键忽略）。
fn counters_from_json(v: Option<&Json>) -> HashMap<(Proto, u16), (u64, u64)> {
    let mut out = HashMap::new();
    let Some(obj) = v.and_then(|x| x.as_object()) else {
        return out;
    };
    for (key, value) in obj {
        let Some((proto, port)) = history::parse_port_key(key) else {
            continue;
        };
        let proto = match proto {
            "tcp" => Proto::Tcp,
            "udp" => Proto::Udp,
            _ => continue,
        };
        let Some(arr) = value.as_array() else {
            continue;
        };
        let rx = arr.first().and_then(|x| x.as_u64()).unwrap_or(0);
        let tx = arr.get(1).and_then(|x| x.as_u64()).unwrap_or(0);
        out.insert((proto, port), (rx, tx));
    }
    out
}

/// 恢复采样状态（不存在/损坏静默从零）。
///
/// - 重启发生在同日：继续累计（首帧差分为 0，仅丢失停机窗口的增量）；
/// - 重启已跨天：先把上一日最终值写入历史日文件，再从首帧重新起算。
fn load_state(base: &Path, inner: &mut Inner) {
    let path = state_path(base);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return;
    };
    let Ok(v) = serde_json::from_str::<Json>(&text) else {
        return;
    };
    let date = v.get("date").and_then(|x| x.as_str()).unwrap_or("");
    if date.is_empty() {
        return;
    }
    let totals = counters_from_json(v.get("totals"));
    if date != history::today_string() {
        // 跨天重启：收尾上一日（日期留空，等待首帧采样重新起算）
        write_day_file_for(base, date, &totals);
        return;
    }
    inner.day_date = date.to_string();
    inner.day_totals = totals;
}

/// 退出前落盘（不等节流窗口）。
fn flush(t: &PortStat) {
    let inner = t.inner.lock().unwrap();
    if inner.day_date.is_empty() {
        return;
    }
    save_state(&t.base, &inner);
    write_day_file(&t.base, &inner);
}

// ————— 面板快照 —————

/// 面板数据（`/star/portTraffic`）。
pub(crate) fn snapshot_json() -> Json {
    let Some(t) = INSTANCE.get() else {
        return json!({
            "enabled": false,
            "mode": "off",
            "ports": [],
            "message": "端口流量统计未启动",
        });
    };
    let inner = t.inner.lock().unwrap();
    let ports: Vec<Json> = inner
        .rows
        .iter()
        .map(|r| {
            json!({
                "port": r.port,
                "proto": r.proto.text(),
                "listen": r.listen,
                "conns": r.conns,
                "rxBytes": r.rx_bytes,
                "txBytes": r.tx_bytes,
                "rxBps": r.rx_bps,
                "txBps": r.tx_bps,
            })
        })
        .collect();
    json!({
        "enabled": t.enabled.load(Ordering::Relaxed),
        "mode": inner.mode.text(),
        "message": inner.message,
        "intervalSeconds": SAMPLE_INTERVAL.as_secs(),
        "ports": ports,
    })
}

// ————— 端口集合 —————

/// 解析配置端口列表（`22,80,443`；逗号/空白/分号分隔；排序去重）。
fn parse_port_list(text: &str) -> Vec<u16> {
    let mut set: HashSet<u16> = HashSet::new();
    for item in text.split([',', ' ', '\t', '\n', '\r', ';']) {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        if let Ok(p) = item.parse::<u16>() {
            if p > 0 {
                set.insert(p);
            }
        }
    }
    let mut v: Vec<u16> = set.into_iter().collect();
    v.sort_unstable();
    v.truncate(MAX_CONFIGURED_PORTS);
    v
}

/// 系统监听端口（Linux 读 `/proc/net/*`；其它平台返回空）。
fn listen_ports() -> Vec<u16> {
    #[cfg(target_os = "linux")]
    {
        let mut set: HashSet<u16> = HashSet::new();
        for (path, tcp) in [
            ("/proc/net/tcp", true),
            ("/proc/net/tcp6", true),
            ("/proc/net/udp", false),
            ("/proc/net/udp6", false),
        ] {
            if let Ok(text) = std::fs::read_to_string(path) {
                collect_proc_listen(&text, tcp, &mut set);
            }
        }
        let mut v: Vec<u16> = set.into_iter().collect();
        v.sort_unstable();
        v.truncate(MAX_AUTO_PORTS);
        v
    }
    #[cfg(not(target_os = "linux"))]
    {
        Vec::new()
    }
}

/// 收集监听端口（tcp 仅状态 `0A` LISTEN；udp 全部绑定口）。纯函数，便于测试。
#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
fn collect_proc_listen(text: &str, tcp: bool, set: &mut HashSet<u16>) {
    for line in text.lines().skip(1) {
        let mut cols = line.split_whitespace();
        cols.next(); // sl
        let Some(local) = cols.next() else { continue };
        cols.next(); // rem_address
        let state = cols.next().unwrap_or("");
        if tcp && state != "0A" {
            continue;
        }
        if let Some(port_hex) = local.rsplit(':').next() {
            if let Ok(port) = u16::from_str_radix(port_hex, 16) {
                if port != 0 {
                    set.insert(port);
                }
            }
        }
    }
}

// ————— 连接视图 —————

/// 全平台连接视图：`(协议, 端口) → (监听, 连接数)`。
fn conn_views() -> HashMap<(Proto, u16), ViewEntry> {
    #[cfg(target_os = "linux")]
    {
        let mut map: HashMap<(Proto, u16), ViewEntry> = HashMap::new();
        for (path, proto) in [
            ("/proc/net/tcp", Proto::Tcp),
            ("/proc/net/tcp6", Proto::Tcp),
            ("/proc/net/udp", Proto::Udp),
            ("/proc/net/udp6", Proto::Udp),
        ] {
            if let Ok(text) = std::fs::read_to_string(path) {
                merge_proc_conns(&text, proto, &mut map);
            }
        }
        map
    }

    #[cfg(windows)]
    {
        conn_views_windows()
    }

    #[cfg(not(any(target_os = "linux", windows)))]
    {
        HashMap::new()
    }
}

/// 合并 `/proc/net/tcp|udp` 文本到连接视图（纯函数，便于测试）。
/// tcp：`0A` LISTEN 记监听、`01` ESTABLISHED 计连接；udp：未连接（remote 端口 0）的绑定口记监听并计 socket 数
/// （已 connect 的 UDP socket 属客户端/转发，不作为“监听”统计）。
#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
fn merge_proc_conns(text: &str, proto: Proto, map: &mut HashMap<(Proto, u16), ViewEntry>) {
    for line in text.lines().skip(1) {
        let mut cols = line.split_whitespace();
        cols.next(); // sl
        let Some(local) = cols.next() else { continue };
        let remote = cols.next().unwrap_or("");
        let state = cols.next().unwrap_or("");
        let Some(port_hex) = local.rsplit(':').next() else {
            continue;
        };
        let Ok(port) = u16::from_str_radix(port_hex, 16) else {
            continue;
        };
        if port == 0 {
            continue;
        }
        let e = map.entry((proto, port)).or_default();
        match proto {
            Proto::Tcp => match state.as_bytes() {
                b"0A" => e.listen = true,
                b"01" => e.conns += 1,
                _ => {}
            },
            Proto::Udp => {
                // remote 端口非 0 → 已连接（connected socket），不计入绑定视图
                let remote_port = remote.rsplit(':').next().unwrap_or("0000");
                if remote_port != "0000" {
                    continue;
                }
                e.listen = true;
                e.conns += 1;
            }
        }
    }
}

/// Windows 连接视图：`GetExtendedTcpTable`（LISTEN/ESTABLISHED）与 `GetExtendedUdpTable`。
#[cfg(windows)]
fn conn_views_windows() -> HashMap<(Proto, u16), ViewEntry> {
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        GetExtendedTcpTable, GetExtendedUdpTable, MIB_TCPTABLE_OWNER_PID, MIB_UDPTABLE_OWNER_PID,
        TCP_TABLE_OWNER_PID_ALL, UDP_TABLE_OWNER_PID,
    };
    use windows_sys::Win32::Networking::WinSock::AF_INET;

    /// 端口字段：DWORD 低 16 位为网络字节序端口。
    fn port_of(dw_local_port: u32) -> u16 {
        ((dw_local_port & 0xffff) as u16).swap_bytes()
    }

    let mut map: HashMap<(Proto, u16), ViewEntry> = HashMap::new();

    unsafe {
        // TCP
        let mut size: u32 = 0;
        GetExtendedTcpTable(
            std::ptr::null_mut(),
            &mut size,
            0,
            AF_INET as u32,
            TCP_TABLE_OWNER_PID_ALL,
            0,
        );
        if size > 0 {
            let mut buf = vec![0u8; size as usize];
            let ret = GetExtendedTcpTable(
                buf.as_mut_ptr() as *mut core::ffi::c_void,
                &mut size,
                0,
                AF_INET as u32,
                TCP_TABLE_OWNER_PID_ALL,
                0,
            );
            if ret == 0 {
                let table = buf.as_ptr() as *const MIB_TCPTABLE_OWNER_PID;
                let count = (*table).dwNumEntries as usize;
                let rows = (*table).table.as_ptr();
                for i in 0..count {
                    let row = rows.add(i);
                    let port = port_of((*row).dwLocalPort);
                    let e = map.entry((Proto::Tcp, port)).or_default();
                    match (*row).dwState {
                        2 => e.listen = true, // MIB_TCP_STATE_LISTEN
                        5 => e.conns += 1,    // MIB_TCP_STATE_ESTAB
                        _ => {}
                    }
                }
            }
        }

        // UDP
        let mut size: u32 = 0;
        GetExtendedUdpTable(
            std::ptr::null_mut(),
            &mut size,
            0,
            AF_INET as u32,
            UDP_TABLE_OWNER_PID,
            0,
        );
        if size > 0 {
            let mut buf = vec![0u8; size as usize];
            let ret = GetExtendedUdpTable(
                buf.as_mut_ptr() as *mut core::ffi::c_void,
                &mut size,
                0,
                AF_INET as u32,
                UDP_TABLE_OWNER_PID,
                0,
            );
            if ret == 0 {
                let table = buf.as_ptr() as *const MIB_UDPTABLE_OWNER_PID;
                let count = (*table).dwNumEntries as usize;
                let rows = (*table).table.as_ptr();
                for i in 0..count {
                    let row = rows.add(i);
                    let port = port_of((*row).dwLocalPort);
                    if port == 0 {
                        continue;
                    }
                    let e = map.entry((Proto::Udp, port)).or_default();
                    e.listen = true;
                    e.conns += 1;
                }
            }
        }
    }
    map
}

// ————— 行构建（纯函数，便于测试） —————

/// nft 模式行构建：计数规则 + 连接视图 → 展示行与下一轮差分基线。
fn rows_nft(
    rules: &[NftRule],
    ports: &[u16],
    views: &HashMap<(Proto, u16), ViewEntry>,
    last: &HashMap<(Proto, u16), (u64, u64)>,
    dt: f64,
) -> (Vec<Row>, HashMap<(Proto, u16), (u64, u64)>) {
    let mut map: BTreeMap<(Proto, u16), Row> = BTreeMap::new();
    for p in ports {
        for proto in [Proto::Tcp, Proto::Udp] {
            map.insert((proto, *p), Row::new(*p, proto));
        }
    }
    for r in rules {
        let e = map
            .entry((r.proto, r.port))
            .or_insert_with(|| Row::new(r.port, r.proto));
        match r.dir {
            Dir::In => e.rx_bytes = Some(r.bytes),
            Dir::Out => e.tx_bytes = Some(r.bytes),
        }
    }
    for (key, v) in views {
        if let Some(e) = map.get_mut(key) {
            e.listen = v.listen;
            e.conns = v.conns;
        }
    }

    let mut counters: HashMap<(Proto, u16), (u64, u64)> = HashMap::new();
    for (key, row) in map.iter_mut() {
        let rx = row.rx_bytes.unwrap_or(0);
        let tx = row.tx_bytes.unwrap_or(0);
        let (rx_bps, tx_bps) = match last.get(key) {
            Some((lrx, ltx)) if dt > 0.0 => (
                ((rx.saturating_sub(*lrx)) as f64 / dt) as u64,
                ((tx.saturating_sub(*ltx)) as f64 / dt) as u64,
            ),
            _ => (0, 0),
        };
        row.rx_bps = Some(rx_bps);
        row.tx_bps = Some(tx_bps);
        counters.insert(*key, (rx, tx));
    }
    let mut rows: Vec<Row> = map.into_values().collect();
    rows.truncate(MAX_ROWS);
    (rows, counters)
}

/// 连接视图行构建：配置了端口则显示配置集合，否则显示监听端口。
fn rows_view(configured: &[u16], views: &HashMap<(Proto, u16), ViewEntry>) -> Vec<Row> {
    let mut map: BTreeMap<(Proto, u16), Row> = BTreeMap::new();
    if configured.is_empty() {
        for (key, v) in views {
            if v.listen {
                let mut row = Row::new(key.1, key.0);
                row.listen = true;
                row.conns = v.conns;
                map.insert(*key, row);
            }
        }
    } else {
        for p in configured {
            for proto in [Proto::Tcp, Proto::Udp] {
                map.insert((proto, *p), Row::new(*p, proto));
            }
        }
        for (key, v) in views {
            if let Some(e) = map.get_mut(key) {
                e.listen = v.listen;
                e.conns = v.conns;
            }
        }
    }
    let mut rows: Vec<Row> = map.into_values().collect();
    rows.truncate(MAX_ROWS);
    rows
}

// ————— nftables 计数 —————

/// nft 规则快照（由 `nft -j list table` 解析）。
#[derive(Debug, PartialEq, Clone)]
struct NftRule {
    handle: u64,
    proto: Proto,
    port: u16,
    dir: Dir,
    bytes: u64,
}

/// 执行 nft 命令（整行拆分后传 argv；nft 会重新拼接解析）。
fn nft_run(line: &str) -> Result<String, String> {
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.is_empty() {
        return Err("空命令".to_string());
    }
    match std::process::Command::new("nft").args(&parts).output() {
        Ok(out) if out.status.success() => Ok(String::from_utf8_lossy(&out.stdout).into_owned()),
        Ok(out) => {
            let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
            if err.is_empty() {
                Err(format!("nft 退出码 {:?}", out.status.code()))
            } else {
                Err(err)
            }
        }
        Err(e) => Err(e.to_string()),
    }
}

/// 忽略“已存在”错误执行 nft 命令。
fn ensure_ignore_exists(cmd: &str) -> Result<(), String> {
    match nft_run(cmd) {
        Ok(_) => Ok(()),
        Err(e) if e.contains("File exists") => Ok(()),
        Err(e) => Err(e),
    }
}

/// 确保计数表与链存在（幂等；已存在视为成功）。
fn ensure_table() -> Result<(), String> {
    ensure_ignore_exists(&format!("add table {TABLE}"))?;
    ensure_ignore_exists(&format!(
        "add chain {TABLE} input {{ type filter hook input priority -350 ; policy accept ; }}"
    ))?;
    ensure_ignore_exists(&format!(
        "add chain {TABLE} output {{ type filter hook output priority -350 ; policy accept ; }}"
    ))?;
    Ok(())
}

/// 删除计数表（关闭开关/卸载时；忽略错误）。
fn drop_table() {
    let _ = nft_run(&format!("delete table {TABLE}"));
}

/// 同步计数规则（增/删）并返回当前规则快照。
fn sync_rules(ports: &[u16]) -> Result<Vec<NftRule>, String> {
    let out = nft_run(&format!("-j list table {TABLE}"))?;
    let v: Json = serde_json::from_str(&out).map_err(|e| format!("解析 nft 输出失败：{e}"))?;
    let rules = parse_nft_rules(&v);

    let mut existing: HashMap<(Proto, u16, Dir), u64> = HashMap::new();
    for r in &rules {
        existing.insert((r.proto, r.port, r.dir), r.handle);
    }

    let mut desired: HashSet<(Proto, u16, Dir)> = HashSet::new();
    for p in ports {
        for proto in [Proto::Tcp, Proto::Udp] {
            for dir in [Dir::In, Dir::Out] {
                desired.insert((proto, *p, dir));
            }
        }
    }

    // 删除不再需要的规则（端口下线等）
    for r in &rules {
        if !desired.contains(&(r.proto, r.port, r.dir)) {
            let chain = chain_of(r.dir);
            let _ = nft_run(&format!("delete rule {TABLE} {chain} handle {}", r.handle));
        }
    }

    // 添加缺失的规则
    for (proto, port, dir) in &desired {
        if !existing.contains_key(&(*proto, *port, *dir)) {
            let chain = chain_of(*dir);
            let field = match dir {
                Dir::In => "dport",
                Dir::Out => "sport",
            };
            let cmd = format!(
                "add rule {TABLE} {chain} {proto} {field} {port} counter comment {comment}",
                proto = proto.text(),
                comment = rule_comment(*proto, *port, *dir),
            );
            nft_run(&cmd)?;
        }
    }
    Ok(rules)
}

/// 方向 → 链名。
fn chain_of(dir: Dir) -> &'static str {
    match dir {
        Dir::In => "input",
        Dir::Out => "output",
    }
}

/// 规则注释（用于回读关联端口/方向）。格式：`pek-{in|out}-{tcp|udp}-{port}`。
fn rule_comment(proto: Proto, port: u16, dir: Dir) -> String {
    let d = match dir {
        Dir::In => "in",
        Dir::Out => "out",
    };
    format!("pek-{}-{}-{}", d, proto.text(), port)
}

/// 解析规则注释（`rule_comment` 的逆操作）。
fn parse_rule_comment(comment: &str) -> Option<(Proto, u16, Dir)> {
    let rest = comment.strip_prefix("pek-")?;
    let mut it = rest.split('-');
    let dir = match it.next()? {
        "in" => Dir::In,
        "out" => Dir::Out,
        _ => return None,
    };
    let proto = match it.next()? {
        "tcp" => Proto::Tcp,
        "udp" => Proto::Udp,
        _ => return None,
    };
    let port = it.next()?.parse::<u16>().ok()?;
    Some((proto, port, dir))
}

/// 从 `nft -j list table` 输出解析计数规则（纯函数，便于测试）。
fn parse_nft_rules(v: &Json) -> Vec<NftRule> {
    let mut out = Vec::new();
    let Some(items) = v.get("nftables").and_then(|x| x.as_array()) else {
        return out;
    };
    for item in items {
        let Some(rule) = item.get("rule") else {
            continue;
        };
        let Some(comment) = rule.get("comment").and_then(|x| x.as_str()) else {
            continue;
        };
        let Some((proto, port, dir)) = parse_rule_comment(comment) else {
            continue;
        };
        let handle = rule.get("handle").and_then(|x| x.as_u64()).unwrap_or(0);
        let mut bytes = 0u64;
        if let Some(exprs) = rule.get("expr").and_then(|x| x.as_array()) {
            for e in exprs {
                if let Some(c) = e.get("counter") {
                    bytes = c.get("bytes").and_then(|x| x.as_u64()).unwrap_or(0);
                }
            }
        }
        out.push(NftRule {
            handle,
            proto,
            port,
            dir,
            bytes,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_port_list() {
        assert_eq!(parse_port_list("80,443, 22;8080"), vec![22, 80, 443, 8080]);
        assert_eq!(parse_port_list(""), Vec::<u16>::new());
        // 非法项忽略；0 忽略
        assert_eq!(parse_port_list("abc,0,-1,65536"), Vec::<u16>::new());
        assert_eq!(parse_port_list("80,80,80"), vec![80]);
        // 超限截断（防御）
        let many = (1..=300).map(|i| i.to_string()).collect::<Vec<_>>().join(",");
        assert_eq!(parse_port_list(&many).len(), MAX_CONFIGURED_PORTS);
    }

    #[test]
    fn parses_proc_listen_ports() {
        // 真实 /proc/net/tcp 样本（字段：sl local_address rem_address st ...）
        let tcp = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n\
                   0: 00000000:0016 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 12345 1 0000000000000000 100 0 0 10 0\n\
                   1: 0100007F:D4C2 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 12346 1 0000000000000000 100 0 0 10 0\n\
                   2: 0100007F:1F90 0100007F:D4C2 01 00000000:00000000 00:00000000 00000000     0        0 12347 1 0000000000000000 20 4 30 10 -1\n";
        let udp = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n\
                   0: 00000000:0035 00000000:0000 07 00000000:00000000 00:00000000 00000000     0        0 9999 2 0000000000000000 0\n";
        let mut set = HashSet::new();
        collect_proc_listen(tcp, true, &mut set);
        collect_proc_listen(udp, false, &mut set);
        let mut got: Vec<u16> = set.into_iter().collect();
        got.sort_unstable();
        // 22(0x16) 与 54466(0xD4C2) 是 LISTEN；8080(0x1F90) 是 ESTABLISHED 不计；
        // UDP 53(0x35) 计入
        assert_eq!(got, vec![22, 53, 54466]);
    }

    #[test]
    fn merges_proc_connections() {
        let tcp = "  sl  local_address rem_address   st\n\
                   0: 00000000:0016 00000000:0000 0A\n\
                   1: 00000000:0016 0100007F:8000 01\n\
                   2: 00000000:0016 0100007F:8001 01\n\
                   3: 00000000:1F90 0100007F:8002 01\n\
                   4: 00000000:0016 0100007F:8003 06\n";
        // 第三行 remote 端口非 0（已 connect 的客户端 socket）应被过滤
        let udp = "  sl  local_address rem_address   st\n\
                   0: 00000000:0035 00000000:0000 07\n\
                   1: 00000000:0035 00000000:0000 07\n\
                   2: 00000000:0035 0100007F:0C35 07\n";
        let mut map: HashMap<(Proto, u16), ViewEntry> = HashMap::new();
        merge_proc_conns(tcp, Proto::Tcp, &mut map);
        merge_proc_conns(udp, Proto::Udp, &mut map);

        let t22 = map.get(&(Proto::Tcp, 22)).unwrap();
        assert!(t22.listen);
        assert_eq!(t22.conns, 2, "TIME_WAIT 不计入连接数");
        let t8080 = map.get(&(Proto::Tcp, 8080)).unwrap();
        assert!(!t8080.listen);
        assert_eq!(t8080.conns, 1);
        let u53 = map.get(&(Proto::Udp, 53)).unwrap();
        assert!(u53.listen);
        assert_eq!(u53.conns, 2, "已连接的 UDP socket 不计入绑定视图");
    }

    #[test]
    fn rule_comment_roundtrip() {
        for proto in [Proto::Tcp, Proto::Udp] {
            for dir in [Dir::In, Dir::Out] {
                let c = rule_comment(proto, 8443, dir);
                assert_eq!(parse_rule_comment(&c), Some((proto, 8443, dir)));
            }
        }
        assert_eq!(rule_comment(Proto::Tcp, 80, Dir::In), "pek-in-tcp-80");
        assert!(parse_rule_comment("other-comment").is_none());
        assert!(parse_rule_comment("pek-in-icmp-1").is_none());
    }

    #[test]
    fn parses_nft_json_output() {
        // `nft -j list table inet pek_stats` 的真实结构（截取关键字段）
        let sample = r#"{"nftables":[
            {"metainfo":{"version":"1.0.9","release_name":"future","json_schema_version":1}},
            {"table":{"family":"inet","name":"pek_stats","handle":1}},
            {"chain":{"family":"inet","table":"pek_stats","name":"input","handle":1,"type":"filter","hook":"input","prio":-350,"policy":"accept"}},
            {"rule":{"family":"inet","table":"pek_stats","chain":"input","handle":2,
                "comment":"pek-in-tcp-80",
                "expr":[{"match":{"op":"==","left":{"payload":{"protocol":"tcp","field":"dport"}},"right":80}},
                        {"counter":{"packets":17,"bytes":4096}}]}},
            {"rule":{"family":"inet","table":"pek_stats","chain":"output","handle":3,
                "comment":"pek-out-tcp-80",
                "expr":[{"match":{"op":"==","left":{"payload":{"protocol":"tcp","field":"sport"}},"right":80}},
                        {"counter":{"packets":18,"bytes":8192}}]}},
            {"rule":{"family":"inet","table":"pek_stats","chain":"input","handle":4,
                "comment":"foreign-rule","expr":[{"counter":{"packets":1,"bytes":1}}]}}
        ]}"#;
        let v: Json = serde_json::from_str(sample).unwrap();
        let rules = parse_nft_rules(&v);
        assert_eq!(rules.len(), 2, "外部/无注释规则应被忽略：{rules:?}");
        assert_eq!(
            rules[0],
            NftRule {
                handle: 2,
                proto: Proto::Tcp,
                port: 80,
                dir: Dir::In,
                bytes: 4096
            }
        );
        assert_eq!(rules[1].bytes, 8192);
        assert_eq!(rules[1].dir, Dir::Out);
    }

    #[test]
    fn builds_nft_rows_with_rates() {
        let rules = vec![
            NftRule {
                handle: 2,
                proto: Proto::Tcp,
                port: 80,
                dir: Dir::In,
                bytes: 10_000,
            },
            NftRule {
                handle: 3,
                proto: Proto::Tcp,
                port: 80,
                dir: Dir::Out,
                bytes: 20_000,
            },
        ];
        let mut views = HashMap::new();
        views.insert(
            (Proto::Tcp, 80),
            ViewEntry {
                listen: true,
                conns: 7,
            },
        );
        let mut last = HashMap::new();
        last.insert((Proto::Tcp, 80), (5_000u64, 10_000u64));

        let (rows, counters) = rows_nft(&rules, &[80], &views, &last, 5.0);
        assert_eq!(rows.len(), 2, "应有 tcp/udp 两行：{rows:?}");
        let tcp = rows.iter().find(|r| r.proto == Proto::Tcp).unwrap();
        assert_eq!(tcp.rx_bytes, Some(10_000));
        assert_eq!(tcp.tx_bytes, Some(20_000));
        // (10000-5000)/5s = 1000 B/s；(20000-10000)/5 = 2000 B/s
        assert_eq!(tcp.rx_bps, Some(1000));
        assert_eq!(tcp.tx_bps, Some(2000));
        assert!(tcp.listen);
        assert_eq!(tcp.conns, 7);
        assert_eq!(counters.get(&(Proto::Tcp, 80)), Some(&(10_000, 20_000)));

        // 无基线（首次采样）：速率 0、不产生尖峰
        let (rows2, _) = rows_nft(&rules, &[80], &views, &HashMap::new(), 5.0);
        let tcp2 = rows2.iter().find(|r| r.proto == Proto::Tcp).unwrap();
        assert_eq!(tcp2.rx_bps, Some(0));
    }

    #[test]
    fn builds_view_rows_filters_and_configures() {
        let mut views: HashMap<(Proto, u16), ViewEntry> = HashMap::new();
        views.insert(
            (Proto::Tcp, 22),
            ViewEntry {
                listen: true,
                conns: 2,
            },
        );
        views.insert(
            (Proto::Tcp, 51000),
            ViewEntry {
                listen: false,
                conns: 9,
            },
        );
        views.insert(
            (Proto::Udp, 53),
            ViewEntry {
                listen: true,
                conns: 1,
            },
        );

        // 未配置：仅显示监听端口（出站随机端口被过滤）
        let rows = rows_view(&[], &views);
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.listen));
        assert!(rows.iter().any(|r| r.port == 22));
        assert!(rows.iter().any(|r| r.port == 53 && r.proto == Proto::Udp));

        // 配置模式：只显示配置端口（含未监听的，供对照）
        let rows = rows_view(&[22, 443], &views);
        let ports: Vec<(Proto, u16)> = rows.iter().map(|r| (r.proto, r.port)).collect();
        assert_eq!(
            ports,
            vec![
                (Proto::Tcp, 22),
                (Proto::Tcp, 443),
                (Proto::Udp, 22),
                (Proto::Udp, 443)
            ]
        );
        let t22 = rows
            .iter()
            .find(|r| r.proto == Proto::Tcp && r.port == 22)
            .unwrap();
        assert!(t22.listen);
        assert_eq!(t22.conns, 2);
    }

    // ————— 每日归档 —————

    fn temp_base(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ragent-portstat-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn empty_inner() -> Inner {
        Inner {
            mode: Mode::Nft,
            message: String::new(),
            rows: Vec::new(),
            last_counters: HashMap::new(),
            last_sample: None,
            last_retry: None,
            day_date: String::new(),
            day_totals: HashMap::new(),
            last_persist: None,
        }
    }

    #[test]
    fn daily_accumulates_deltas_and_closes_on_rollover() {
        let base = temp_base("daily");
        let mut inner = empty_inner();
        let key = (Proto::Tcp, 80);

        let mut counters = HashMap::new();
        counters.insert(key, (100u64, 200u64));
        // 零流量端口（从未有收发）：不应进入历史归档
        counters.insert((Proto::Udp, 53), (0u64, 0u64));
        // 首帧：无差分基线，只建立基准不计入
        accumulate_daily(&base, &mut inner, &counters, "2026-10-01");
        assert_eq!(inner.day_totals.get(&key), Some(&(0, 0)));
        inner.last_counters = counters.clone();

        // 次帧：+50/+100
        counters.insert(key, (150, 300));
        accumulate_daily(&base, &mut inner, &counters, "2026-10-01");
        assert_eq!(inner.day_totals.get(&key), Some(&(50, 100)));
        inner.last_counters = counters.clone();

        // 计数表重建（读数回退）：差分为 0，累计不回归
        counters.insert(key, (10, 10));
        accumulate_daily(&base, &mut inner, &counters, "2026-10-01");
        assert_eq!(inner.day_totals.get(&key), Some(&(50, 100)));
        inner.last_counters = counters.clone();

        // 跨天：旧日写入历史（tcp:80 收 50 发 100），新日连续累计
        counters.insert(key, (60, 80));
        accumulate_daily(&base, &mut inner, &counters, "2026-10-02");
        assert_eq!(inner.day_date, "2026-10-02");
        assert_eq!(inner.day_totals.get(&key), Some(&(50, 70)));
        let ports = crate::history::day_ports(&base, "2026-10-01");
        assert_eq!(
            ports.get("tcp:80"),
            Some(&crate::history::PortCounters { rx: 50, tx: 100 })
        );
        assert!(
            ports.get("udp:53").is_none(),
            "零流量端口不应归档：{ports:?}"
        );

        crate::history::drop_storage_for_test(&base);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn state_roundtrip_and_stale_day_close() {
        let base = temp_base("state");

        // 同日恢复：累计继续（不写历史）
        let mut same = empty_inner();
        same.day_date = crate::history::today_string();
        same.day_totals.insert((Proto::Udp, 53), (7, 9));
        save_state(&base, &same);
        let mut back = empty_inner();
        load_state(&base, &mut back);
        assert_eq!(back.day_date, crate::history::today_string());
        assert_eq!(back.day_totals.get(&(Proto::Udp, 53)), Some(&(7, 9)));

        // 跨天恢复：收尾写历史，日期置空等待首帧重起
        let mut stale = empty_inner();
        stale.day_date = "1999-01-01".to_string();
        stale.day_totals.insert((Proto::Tcp, 22), (1, 2));
        save_state(&base, &stale);
        let mut back2 = empty_inner();
        load_state(&base, &mut back2);
        assert!(back2.day_date.is_empty());
        let ports = crate::history::day_ports(&base, "1999-01-01");
        assert_eq!(
            ports.get("tcp:22"),
            Some(&crate::history::PortCounters { rx: 1, tx: 2 })
        );

        crate::history::drop_storage_for_test(&base);
        let _ = std::fs::remove_dir_all(&base);
    }
}
