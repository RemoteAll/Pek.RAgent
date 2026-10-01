//! 网站流量统计（WebTraffic）：自动发现 nginx/apache 站点访问日志（`WebLogs` 手动补充），
//! 后台增量解析（inode+offset 防轮转/截断），按站点聚合今日/昨日/累计流量、请求数、UV
//! 与状态码分布，供 Web 面板 `/star/webTraffic` 展示。
//!
//! **零侵入**：只读日志文件，不修改任何 Web 服务器配置。统计口径为日志中的响应体字节
//! （nginx `$body_bytes_sent` / Apache `%b` / Caddy `size`），与宝塔“网站统计”插件同口径；
//! 解析走后台线程按 1 秒增量读取（只读新增部分，长跑开销恒定）。
//!
//! 支持格式：
//! - combined / common（nginx 默认与宝塔、Apache 默认）：`ip - - [time] "req" status bytes ...`
//! - JSON 行（Caddy 默认访问日志）：取 `status` / `size` / `request.remote_ip`
//!
//! 持久化 `Data/web_traffic.json`（30 秒节流）：站点累计与文件读取位置（inode+offset），
//! 重启续读不重复统计。UV 集合不持久化（重启后按“基数 + 新集合”近似）。
//!
//! 每日归档：跨天时把上一日的站点汇总写入 SQLite（`Data/traffic.db`，模型 `Entity/Model.xml`，
//! 见 `history` 模块）；当日数据也在 30 秒节流周期内持续刷新；面板“流量 → 历史数据”按天查看。

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::io::{Read, Seek, SeekFrom};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{json, Value as Json};

use crate::config::AgentConfig;
use crate::history::{self, SiteDay};
use crate::manager::AppManager;
use crate::util;

/// 日志轮询间隔。
const POLL_INTERVAL: Duration = Duration::from_millis(1000);
/// 单次读取上限（字节）：防首次回溯读超大文件时长时间占用（未读完下一轮继续）。
const MAX_CHUNK: usize = 8 * 1024 * 1024;
/// 半行缓冲上限（畸形日志保护）。
const MAX_LINE_BUF: usize = 1024 * 1024;
/// 站点数量上限（保护）。
const MAX_SITES: usize = 256;
/// 今日 UV 集合上限（保护内存）。
const MAX_UV: usize = 1_000_000;
/// 持久化节流间隔。
const PERSIST_INTERVAL: Duration = Duration::from_secs(30);
/// 自动发现重扫间隔（新站点上线后最多 5 分钟可见）。
const REDISCOVER_INTERVAL: Duration = Duration::from_secs(300);
/// 无任何日志源时的重试间隔。
const REDISCOVER_EMPTY_INTERVAL: Duration = Duration::from_secs(30);
/// 速率平滑窗口（1 秒粒度的滑动平均格数）。
const RATE_WINDOW: usize = 5;

// ————— 数据模型 —————

/// 单个日志文件的读取状态。
#[derive(Default)]
struct FileState {
    /// 文件身份（Unix inode / Windows 文件索引）；0 = 不可用
    inode: u64,
    /// 已读取到的字节偏移
    offset: u64,
    /// 跨读取边界的半行缓冲
    pending: Vec<u8>,
    /// 是否已初始化（决定首次读取的起始位置）
    initialized: bool,
    /// 错误日志节流（避免每秒刷屏）
    error_logged: bool,
}

/// 单日统计。
#[derive(Default, Clone)]
struct DayStats {
    bytes: u64,
    requests: u64,
    s2xx: u64,
    s3xx: u64,
    s4xx: u64,
    s5xx: u64,
    /// 今日 UV 集合（本次运行期间新见到的 IP）
    uv: HashSet<IpAddr>,
    /// UV 基数（持久化恢复的历史数量；重启后集合从零，基数为准）
    uv_base: u64,
}

impl DayStats {
    /// UV 展示值（基数 + 本次集合）。
    fn uv_total(&self) -> u64 {
        self.uv_base + self.uv.len() as u64
    }
}

/// 站点统计。
#[derive(Default)]
struct SiteStats {
    /// 当前“今日”日期（`%Y-%m-%d`），用于跨天滚动
    date: String,
    today: DayStats,
    yesterday: DayStats,
    total_bytes: u64,
    total_requests: u64,
    /// 速率窗口（每秒字节增量）与最近速率（字节/秒）
    rate_win: VecDeque<u64>,
    rate_bps: u64,
    /// 速率差分基线
    rate_last: u64,
    /// 速率是否已建立基线（首帧只记基线，避免把历史累计当瞬时速率）
    rate_init: bool,
}

impl SiteStats {
    /// 跨天滚动：日期变化时把今日移为昨日。
    fn roll_day(&mut self, today: &str) {
        if self.date != today {
            self.yesterday = std::mem::take(&mut self.today);
            self.date = today.to_string();
        }
    }

    /// 聚合一行日志。
    fn add(&mut self, line: &LogLine) {
        self.today.bytes += line.bytes;
        self.today.requests += 1;
        self.total_bytes += line.bytes;
        self.total_requests += 1;
        match line.status / 100 {
            2 => self.today.s2xx += 1,
            3 => self.today.s3xx += 1,
            4 => self.today.s4xx += 1,
            5 => self.today.s5xx += 1,
            _ => {}
        }
        if let Some(ip) = line.ip {
            if self.today.uv.len() < MAX_UV {
                self.today.uv.insert(ip);
            }
        }
    }

    /// 速率推进（每 1 秒调用一次；5 秒滑动平均）。
    fn tick_rate(&mut self) {
        if !self.rate_init {
            // 首帧（首次运行可能回溯读入大量历史数据）：只建立基线，不产生速率尖峰
            self.rate_init = true;
            self.rate_last = self.total_bytes;
            self.rate_bps = 0;
            return;
        }
        let delta = self.total_bytes.saturating_sub(self.rate_last);
        self.rate_last = self.total_bytes;
        if self.rate_win.len() >= RATE_WINDOW {
            self.rate_win.pop_front();
        }
        self.rate_win.push_back(delta);
        let sum: u64 = self.rate_win.iter().sum();
        self.rate_bps = sum / self.rate_win.len().max(1) as u64;
    }
}

/// 站点 → 日志文件。
#[derive(Clone)]
struct Source {
    site: String,
    path: PathBuf,
}

/// 模块共享状态。
struct Inner {
    sources: Vec<Source>,
    stats: HashMap<String, SiteStats>,
    files: HashMap<PathBuf, FileState>,
    last_discover: Option<Instant>,
    last_persist: Option<Instant>,
    dirty: bool,
}

/// 网站流量统计实例。
struct WebTraffic {
    base: PathBuf,
    enabled: AtomicBool,
    inner: Mutex<Inner>,
}

/// 全局实例（面板快照读取）。
static INSTANCE: OnceLock<Arc<WebTraffic>> = OnceLock::new();

// ————— 生命周期 —————

/// 启动后台线程（幂等；由 HTTP 面板启动路径调用）。
///
/// 线程职责：响应配置热重载（WebTraffic/WebLogs 变化即时生效）、1 秒增量读取日志、
/// 聚合统计、30 秒节流持久化；`WebTraffic=false` 时线程空转等待重新启用。
pub(crate) fn start(manager: Arc<AppManager>) {
    static STARTED: std::sync::Once = std::sync::Once::new();
    STARTED.call_once(|| {
        let traffic = Arc::new(WebTraffic {
            base: manager.base().to_path_buf(),
            enabled: AtomicBool::new(false),
            inner: Mutex::new(Inner {
                sources: Vec::new(),
                stats: HashMap::new(),
                files: HashMap::new(),
                last_discover: None,
                last_persist: None,
                dirty: false,
            }),
        });
        {
            let mut inner = traffic.inner.lock().unwrap();
            load_persisted(&traffic.base, &mut inner);
        }
        let _ = INSTANCE.set(traffic.clone());

        let _ = std::thread::Builder::new()
            .name("ragent-webtraffic".to_string())
            .spawn(move || run_loop(traffic, manager));
    });
}

/// 后台主循环。
fn run_loop(t: Arc<WebTraffic>, manager: Arc<AppManager>) {
    let mut last_cfg: Option<(bool, String)> = None;
    loop {
        if crate::agent::SHUTDOWN.load(Ordering::SeqCst) {
            flush(&t);
            break;
        }

        let cfg = manager.config();
        let fp = (cfg.web_traffic, cfg.web_logs.clone());
        if last_cfg.as_ref() != Some(&fp) {
            last_cfg = Some(fp);
            reconfigure(&t, &cfg);
        }
        if t.enabled.load(Ordering::Relaxed) {
            tick(&t, &cfg);
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// 应用配置变化：更新开关、重建日志源。
fn reconfigure(t: &WebTraffic, cfg: &AgentConfig) {
    t.enabled.store(cfg.web_traffic, Ordering::Relaxed);
    let mut inner = t.inner.lock().unwrap();
    if !cfg.web_traffic {
        // 关闭时清空源（统计数据保留，重新启用后续读；文件偏移状态保留不重复读）
        inner.sources.clear();
        return;
    }
    let manual = parse_manual_logs(&cfg.web_logs);
    let auto = discover_sites();
    apply_sources(&mut inner, merge_sources(manual, auto));
}

/// 替换日志源并确保站点统计条目存在。
fn apply_sources(inner: &mut Inner, sources: Vec<Source>) {
    inner.last_discover = Some(Instant::now());
    for s in &sources {
        inner.stats.entry(s.site.clone()).or_default();
    }
    if sources.len() != inner.sources.len()
        || sources
            .iter()
            .zip(inner.sources.iter())
            .any(|(a, b)| a.site != b.site || a.path != b.path)
    {
        util::log_format(
            "网站流量：发现 {} 个站点日志源",
            &[&sources.len().to_string()],
        );
    }
    inner.sources = sources;
}

/// 单次采样：发现（如有必要）→ 读取增量 → 聚合 → 节流持久化。
fn tick(t: &WebTraffic, cfg: &AgentConfig) {
    let today = today_string();
    let mut inner = t.inner.lock().unwrap();

    // 定期重扫（新站点上线 / 空源更频繁重试）
    let need_rediscover = match inner.last_discover {
        None => true,
        Some(at) => {
            at.elapsed() >= REDISCOVER_INTERVAL
                || (inner.sources.is_empty() && at.elapsed() >= REDISCOVER_EMPTY_INTERVAL)
        }
    };
    if need_rediscover {
        let manual = parse_manual_logs(&cfg.web_logs);
        let auto = discover_sites();
        apply_sources(&mut inner, merge_sources(manual, auto));
    }

    // 跨天收尾与滚动（先归档旧日、再滚动，避免跨天瞬间写错日期）
    close_stale_days(&t.base, &mut inner.stats, &today);
    // 历史保留清理（内部每天最多执行一次，开销可忽略）
    history::maybe_prune(&t.base, cfg.traffic_history_days);

    // 读取增量并聚合成事件（先收集，避免多字段可变借用冲突）
    let sources = inner.sources.clone();
    let mut events: Vec<(String, LogLine)> = Vec::new();
    for src in &sources {
        let st = inner.files.entry(src.path.clone()).or_default();
        match read_increment(&src.path, st) {
            Ok(lines) => {
                st.error_logged = false;
                for line in lines {
                    if let Some(ev) = parse_log_line(&line) {
                        events.push((src.site.clone(), ev));
                    }
                }
            }
            Err(e) => {
                if !st.error_logged {
                    st.error_logged = true;
                    util::log_format(
                        "网站日志读取失败：{}（{}）",
                        &[&src.path.display().to_string(), &e.to_string()],
                    );
                }
            }
        }
    }
    for (site, ev) in &events {
        inner.stats.entry(site.clone()).or_default().add(ev);
    }
    if !events.is_empty() {
        inner.dirty = true;
    }

    // 速率推进（每秒一次）
    for st in inner.stats.values_mut() {
        st.tick_rate();
    }

    // 持久化与每日归档（30 秒节流；有变化才写统计文件；进程启动后首个周期刷新日文件）
    let due = inner
        .last_persist
        .map(|at| at.elapsed() >= PERSIST_INTERVAL)
        .unwrap_or(true);
    if due && (inner.dirty || inner.last_persist.is_none()) {
        if inner.dirty {
            save_persisted(t, &inner);
        }
        write_day_file(&t.base, &today, &inner.stats);
        inner.dirty = false;
        inner.last_persist = Some(Instant::now());
    }
}

/// 退出前落盘（不等节流窗口）；历史日文件一并收尾。
fn flush(t: &WebTraffic) {
    let inner = t.inner.lock().unwrap();
    if inner.dirty {
        save_persisted(t, &inner);
    }
    for date in stat_dates(&inner.stats) {
        write_day_file(&t.base, &date, &inner.stats);
    }
}

// ————— 面板快照 —————

/// 面板数据（`/star/webTraffic`）。
pub(crate) fn snapshot_json() -> Json {
    let Some(t) = INSTANCE.get() else {
        return json!({
            "enabled": false,
            "sites": [],
            "message": "流量统计未启动",
        });
    };

    let inner = t.inner.lock().unwrap();
    let enabled = t.enabled.load(Ordering::Relaxed);

    // 按站点聚合展示（同名多日志合并），保持发现顺序
    let mut order: Vec<String> = Vec::new();
    let mut logs: HashMap<&str, Vec<String>> = HashMap::new();
    for s in &inner.sources {
        if !logs.contains_key(s.site.as_str()) {
            order.push(s.site.clone());
        }
        logs.entry(s.site.as_str())
            .or_default()
            .push(s.path.display().to_string());
    }

    let mut sites: Vec<Json> = Vec::new();
    for name in &order {
        let st = inner.stats.get(name);
        let (
            tb,
            tr,
            uv,
            s2,
            s3,
            s4,
            s5,
            rate,
            yb,
            yreq,
            tot,
            totr,
        ) = match st {
            Some(st) => (
                st.today.bytes,
                st.today.requests,
                st.today.uv_total(),
                st.today.s2xx,
                st.today.s3xx,
                st.today.s4xx,
                st.today.s5xx,
                st.rate_bps,
                st.yesterday.bytes,
                st.yesterday.requests,
                st.total_bytes,
                st.total_requests,
            ),
            None => (0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0),
        };
        sites.push(json!({
            "name": name,
            "logs": logs.get(name.as_str()).cloned().unwrap_or_default(),
            "todayBytes": tb,
            "todayRequests": tr,
            "uv": uv,
            "s2xx": s2,
            "s3xx": s3,
            "s4xx": s4,
            "s5xx": s5,
            "rateBps": rate,
            "yesterdayBytes": yb,
            "yesterdayRequests": yreq,
            "totalBytes": tot,
            "totalRequests": totr,
        }));
    }

    let message = if !enabled {
        "网站流量统计未启用（配置 WebTraffic=true 开启）"
    } else if sites.is_empty() {
        "未发现网站日志（自动扫描 nginx/apache 常见目录；可在配置 WebLogs 手动指定：名称=路径;名称2=路径2）"
    } else {
        ""
    };
    json!({
        "enabled": enabled,
        "sites": sites,
        "message": message,
    })
}

// ————— 日志解析 —————

/// 单行日志解析结果。
#[derive(Debug, PartialEq, Clone)]
struct LogLine {
    /// 客户端 IP（解析失败为 None，不计 UV）
    ip: Option<IpAddr>,
    /// HTTP 状态码
    status: u16,
    /// 响应体字节（统计口径：nginx $body_bytes_sent / Apache %b / Caddy size）
    bytes: u64,
}

/// 解析一行访问日志（combined/common 或 JSON）。
fn parse_log_line(line: &str) -> Option<LogLine> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    if line.starts_with('{') {
        parse_json_line(line)
    } else {
        parse_combined_line(line)
    }
}

/// combined / common：`ip - user [time] "request" status bytes ...`
///
/// nginx 对日志中的引号做转义（`\x22`），因此第一对引号即 `$request`，其后的
/// 两个字段就是状态码与响应体字节。
fn parse_combined_line(line: &str) -> Option<LogLine> {
    let first = line.find('"')?;
    let rest = &line[first + 1..];
    let second = rest.find('"')?;
    let after = rest[second + 1..].trim_start();
    let mut it = after.split_whitespace();
    let status = it.next()?.parse::<u16>().ok()?;
    if !(100..=599).contains(&status) {
        return None;
    }
    // 响应体字节：缺失或 `-`（Apache %b 无响应体）按 0
    let bytes = it
        .next()
        .and_then(|b| b.parse::<u64>().ok())
        .unwrap_or(0);
    let ip = line[..first]
        .split_whitespace()
        .next()
        .and_then(|s| s.parse::<IpAddr>().ok());
    Some(LogLine { ip, status, bytes })
}

/// JSON 行（Caddy 默认访问日志）：取 `status` / `size` / `request.remote_ip`。
fn parse_json_line(line: &str) -> Option<LogLine> {
    let v: Json = serde_json::from_str(line).ok()?;
    let status = v.get("status")?.as_u64()? as u16;
    if !(100..=599).contains(&status) {
        return None;
    }
    let bytes = v.get("size").and_then(|x| x.as_u64()).unwrap_or(0);
    let ip = v
        .get("request")
        .and_then(|r| r.get("remote_ip"))
        .and_then(|x| x.as_str())
        .and_then(|s| s.parse::<IpAddr>().ok());
    Some(LogLine { ip, status, bytes })
}

// ————— 站点发现 —————

/// 手动配置解析：`名称=路径;名称2=路径2`（分号/换行分隔）。
fn parse_manual_logs(text: &str) -> Vec<Source> {
    let mut out = Vec::new();
    for item in text.split([';', '\n', '\r']) {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        let Some((name, path)) = item.split_once('=') else {
            continue;
        };
        let name = name.trim();
        let path = path.trim();
        if name.is_empty() || path.is_empty() {
            continue;
        }
        out.push(Source {
            site: name.to_string(),
            path: PathBuf::from(path),
        });
    }
    out
}

/// 合并手动与自动发现的日志源：手动优先；路径级去重（先到先得）；站点数上限保护。
fn merge_sources(manual: Vec<Source>, auto: Vec<Source>) -> Vec<Source> {
    let mut out: Vec<Source> = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();
    for src in manual.into_iter().chain(auto.into_iter()) {
        if out.len() >= MAX_SITES {
            break;
        }
        // 同名站点的多个文件允许保留（统计时合并）；只限制路径级重复
        if !seen.insert(src.path.clone()) {
            continue;
        }
        out.push(src);
    }
    out
}

/// 自动发现站点日志（nginx/apache 常见目录；仅 Unix）。
fn discover_sites() -> Vec<Source> {
    #[cfg(unix)]
    {
        const NGINX_DIRS: &[&str] = &[
            "/www/server/panel/vhost/nginx", // 宝塔
            "/etc/nginx/sites-enabled",      // Debian/Ubuntu
            "/etc/nginx/conf.d",             // RHEL/CentOS
            "/etc/nginx/vhosts.d",           // openSUSE
            "/usr/local/nginx/conf/vhost",   // lnmp/oneinstack
            "/usr/local/nginx/conf/conf.d",
        ];
        const APACHE_DIRS: &[&str] = &[
            "/etc/apache2/sites-enabled", // Debian/Ubuntu
            "/etc/httpd/conf.d",          // RHEL/CentOS
            "/etc/apache2/vhosts.d",      // openSUSE
            "/usr/local/apache/conf/vhost",
        ];

        let mut out: Vec<Source> = Vec::new();
        for dir in NGINX_DIRS {
            for file in list_conf_files(Path::new(dir)) {
                if let Ok(text) = std::fs::read_to_string(&file) {
                    for (name, path) in parse_nginx_vhosts(&text) {
                        out.push(Source { site: name, path });
                    }
                }
            }
        }
        for dir in APACHE_DIRS {
            for file in list_conf_files(Path::new(dir)) {
                if let Ok(text) = std::fs::read_to_string(&file) {
                    for (name, path) in parse_apache_vhosts(&text) {
                        out.push(Source { site: name, path });
                    }
                }
            }
        }
        out
    }

    #[cfg(not(unix))]
    {
        // Windows 等平台没有固定的 vhost 目录约定（nginx 多为绿色版自定义路径），
        // 仅支持 WebLogs 手动配置
        Vec::new()
    }
}

/// 列出目录下的配置文件（`.conf` 或无扩展名，如 Debian `sites-enabled/default`）。
#[cfg_attr(not(unix), allow(dead_code))]
fn list_conf_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if name.ends_with(".conf") || !name.contains('.') {
            out.push(path);
        }
    }
    out.sort();
    out
}

/// nginx 配置 token 化：按空白与 `;{}` 拆分、去 `#` 注释、引号内保留空白。
#[cfg_attr(not(unix), allow(dead_code))]
fn nginx_tokens(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut in_quote = false;
    let mut in_comment = false;
    for ch in text.chars() {
        if in_comment {
            if ch == '\n' {
                in_comment = false;
            }
            continue;
        }
        if in_quote {
            if ch == '"' {
                in_quote = false;
            } else {
                cur.push(ch);
            }
            continue;
        }
        match ch {
            '#' => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
                in_comment = true;
            }
            '"' => in_quote = true,
            ';' | '{' | '}' => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
                out.push(ch.to_string());
            }
            _ if ch.is_whitespace() => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            _ => cur.push(ch),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// 从 nginx 配置文本提取 `(server_name, access_log)` 列表。
///
/// 只识别 `server { ... }` 块直接子层的 `server_name` 与 `access_log` 指令；
/// `access_log off` 视为无日志；`include` 不递归。
#[cfg_attr(not(unix), allow(dead_code))]
fn parse_nginx_vhosts(text: &str) -> Vec<(String, PathBuf)> {
    let tokens = nginx_tokens(text);
    let mut out: Vec<(String, PathBuf)> = Vec::new();

    #[derive(PartialEq)]
    enum Collect {
        None,
        ServerName,
        AccessLog,
    }

    let mut depth: usize = 0;
    let mut server_at: Option<usize> = None;
    let mut collecting = Collect::None;
    let mut server_name: Option<String> = None;
    let mut access_log: Option<PathBuf> = None;
    let mut prev = String::new();

    for token in &tokens {
        // 收集指令参数
        if collecting != Collect::None {
            if token == ";" {
                collecting = Collect::None;
                continue;
            }
            match collecting {
                Collect::ServerName => {
                    if server_name.is_none() && !token.starts_with('$') && token != "_" {
                        server_name = Some(token.clone());
                    }
                }
                Collect::AccessLog => {
                    if access_log.is_none() {
                        // `off` 与 `/dev/null` 均视为不记录日志（宝塔等默认配置常见，勿作日志源）
                        if token.eq_ignore_ascii_case("off") || token == "/dev/null" {
                            // 继续吃参数，不收集
                        } else if looks_like_abs_path(token) {
                            access_log = Some(PathBuf::from(token));
                        }
                    }
                }
                Collect::None => {}
            }
            continue;
        }

        match token.as_str() {
            "{" => {
                depth += 1;
                if prev == "server" && server_at.is_none() && depth <= 2 {
                    server_at = Some(depth);
                }
            }
            "}" => {
                if Some(depth) == server_at {
                    // server 块结束：提交
                    if let (Some(name), Some(log)) = (server_name.take(), access_log.take()) {
                        out.push((name, log));
                    }
                    server_name = None;
                    access_log = None;
                    server_at = None;
                }
                depth = depth.saturating_sub(1);
            }
            "server_name" if server_at == Some(depth) && depth > 0 => {
                collecting = Collect::ServerName;
            }
            "access_log" if server_at == Some(depth) && depth > 0 => {
                collecting = Collect::AccessLog;
            }
            _ => {}
        }
        prev = token.clone();
    }
    out
}

/// 从 apache 配置文本提取 `(ServerName, CustomLog)` 列表（`<VirtualHost>` 块内）。
#[cfg_attr(not(unix), allow(dead_code))]
fn parse_apache_vhosts(text: &str) -> Vec<(String, PathBuf)> {
    let mut out: Vec<(String, PathBuf)> = Vec::new();
    let mut in_vhost = false;
    let mut server_name: Option<String> = None;
    let mut custom_log: Option<PathBuf> = None;

    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let lower = line.to_ascii_lowercase();
        if lower.starts_with("<virtualhost") {
            in_vhost = true;
            server_name = None;
            custom_log = None;
            continue;
        }
        if lower.starts_with("</virtualhost") {
            if let (Some(name), Some(log)) = (server_name.take(), custom_log.take()) {
                out.push((name, log));
            }
            in_vhost = false;
            continue;
        }
        if !in_vhost {
            continue;
        }
        let mut parts = line.split_whitespace();
        let Some(cmd) = parts.next() else {
            continue;
        };
        if cmd.eq_ignore_ascii_case("ServerName") {
            if server_name.is_none() {
                server_name = parts.next().map(|s| s.to_string());
            }
        } else if cmd.eq_ignore_ascii_case("CustomLog") {
            if custom_log.is_none() {
                if let Some(p) = parts.next() {
                    // 仅接受绝对路径（相对路径基于 ServerRoot，无法可靠解析）
                    if p != "/dev/null" && looks_like_abs_path(p) {
                        custom_log = Some(PathBuf::from(p));
                    }
                }
            }
        }
    }
    out
}

/// 配置中的绝对路径判断（跨平台：`/xxx` 或 `C:/xxx`、`C:\\xxx`）。
///
/// 不使用 `Path::is_absolute` —— 其语义随运行平台变化，而 nginx/apache 配置
/// 解析在 Windows 上测试/运行时应同样接受 Unix 风格路径。
#[cfg_attr(not(unix), allow(dead_code))]
fn looks_like_abs_path(p: &str) -> bool {
    if p.starts_with('/') || p.starts_with("\\") {
        return true;
    }
    let b = p.as_bytes();
    b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'/' || b[2] == b'\\')
}

// ————— 增量读取 —————

/// 文件身份（Unix inode / Windows 文件索引）。
fn file_identity(path: &Path, meta: &std::fs::Metadata) -> u64 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let _ = path;
        meta.ino()
    }

    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
        };
        let _ = meta;
        let Ok(file) = std::fs::File::open(path) else {
            return 0;
        };
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        let ok = unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) };
        if ok == 0 {
            return 0;
        }
        ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64
    }

    #[cfg(not(any(unix, windows)))]
    {
        let _ = (path, meta);
        0
    }
}

/// 文件是否今天更新过（决定首次读取是否回溯读今日内容）。
fn mtime_is_today(meta: &std::fs::Metadata) -> bool {
    let Ok(t) = meta.modified() else {
        return false;
    };
    let dt: chrono::DateTime<chrono::Local> = t.into();
    dt.format("%Y-%m-%d").to_string() == today_string()
}

/// 本地日期字符串（`%Y-%m-%d`）。
fn today_string() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

/// 读取文件增量，返回完整行列表（不完整行留在缓冲，下一轮拼接）。
///
/// 处理三类边界：
/// - 首次读取：文件今天更新过 → 从头读（补今日已有数据）；否则跳到末尾（只统计增量）；
/// - 日志轮转：文件身份变化（inode 不同）→ 新文件从头读；
/// - 截断（copytruncate）：长度小于已读偏移 → 从头读。
fn read_increment(path: &Path, st: &mut FileState) -> std::io::Result<Vec<String>> {
    let meta = std::fs::metadata(path)?;
    let len = meta.len();
    let ident = file_identity(path, &meta);

    if !st.initialized {
        st.inode = ident;
        st.offset = if mtime_is_today(&meta) { 0 } else { len };
        st.initialized = true;
    } else if ident != 0 && st.inode != ident {
        // 日志轮转：新文件从头读
        st.inode = ident;
        st.offset = 0;
        st.pending.clear();
    } else if len < st.offset {
        // 截断：从头读
        st.offset = 0;
        st.pending.clear();
    }

    let mut lines: Vec<String> = Vec::new();
    if len > st.offset {
        let mut file = std::fs::File::open(path)?;
        file.seek(SeekFrom::Start(st.offset))?;
        let want = ((len - st.offset) as usize).min(MAX_CHUNK);
        let mut chunk = vec![0u8; want];
        let n = file.read(&mut chunk)?;
        chunk.truncate(n);
        st.offset += n as u64;

        st.pending.extend_from_slice(&chunk);
        let mut start = 0usize;
        for i in 0..st.pending.len() {
            if st.pending[i] == b'\n' {
                let line = &st.pending[start..i];
                lines.push(String::from_utf8_lossy(line).trim_end_matches('\r').to_string());
                start = i + 1;
            }
        }
        if start > 0 {
            st.pending.drain(..start);
        }
        if st.pending.len() > MAX_LINE_BUF {
            // 畸形保护：超长无换行内容直接丢弃
            st.pending.clear();
        }
    }
    Ok(lines)
}

// ————— 持久化 —————

/// 持久化路径。
fn persist_path(base: &Path) -> PathBuf {
    base.join("Data").join("web_traffic.json")
}

/// 序列化当前状态。
fn persist_json(inner: &Inner) -> Json {
    let mut sites = serde_json::Map::new();
    for (name, st) in &inner.stats {
        sites.insert(
            name.clone(),
            json!({
                "date": st.date,
                "today": day_json(&st.today),
                "yesterday": day_json(&st.yesterday),
                "totalBytes": st.total_bytes,
                "totalRequests": st.total_requests,
            }),
        );
    }
    let mut files = serde_json::Map::new();
    for (path, st) in &inner.files {
        files.insert(
            path.display().to_string(),
            json!({ "inode": st.inode, "offset": st.offset }),
        );
    }
    json!({
        "updated": chrono::Local::now().timestamp(),
        "sites": sites,
        "files": files,
    })
}

/// 单日统计序列化。
fn day_json(d: &DayStats) -> Json {
    json!({
        "bytes": d.bytes,
        "requests": d.requests,
        "s2xx": d.s2xx,
        "s3xx": d.s3xx,
        "s4xx": d.s4xx,
        "s5xx": d.s5xx,
        "uv": d.uv_total(),
    })
}

/// 单日统计反序列化（UV 集合不可恢复，按基数恢复）。
fn day_from_json(v: Option<&Json>) -> DayStats {
    let get = |key: &str| -> u64 {
        v.and_then(|x| x.get(key))
            .and_then(|x| x.as_u64())
            .unwrap_or(0)
    };
    DayStats {
        bytes: get("bytes"),
        requests: get("requests"),
        s2xx: get("s2xx"),
        s3xx: get("s3xx"),
        s4xx: get("s4xx"),
        s5xx: get("s5xx"),
        uv: HashSet::new(),
        uv_base: get("uv"),
    }
}

/// 站点统计中出现的全部日期（去重）。
fn stat_dates(stats: &HashMap<String, SiteStats>) -> Vec<String> {
    let mut dates: Vec<String> = Vec::new();
    for st in stats.values() {
        if !st.date.is_empty() && !dates.contains(&st.date) {
            dates.push(st.date.clone());
        }
    }
    dates
}

/// 跨天收尾：把仍在旧日期的站点快照写入对应历史日文件，然后滚动到新日期。
fn close_stale_days(base: &Path, stats: &mut HashMap<String, SiteStats>, today: &str) {
    for date in stat_dates(stats) {
        if date != today {
            write_day_file(base, &date, stats);
        }
    }
    for st in stats.values_mut() {
        st.roll_day(today);
    }
}

/// 写入某日期各站点的当日快照（绝对量；同一站点重复写入为幂等覆盖）。
fn write_day_file(base: &Path, date: &str, stats: &HashMap<String, SiteStats>) {
    let mut sites: BTreeMap<String, SiteDay> = BTreeMap::new();
    for (name, st) in stats {
        if st.date != date {
            continue;
        }
        let uv = st.today.uv_total();
        if st.today.requests == 0 && st.today.bytes == 0 && uv == 0 {
            continue;
        }
        sites.insert(
            name.clone(),
            SiteDay {
                hits: st.today.requests,
                bytes: st.today.bytes,
                uv,
                s2xx: st.today.s2xx,
                s3xx: st.today.s3xx,
                s4xx: st.today.s4xx,
                s5xx: st.today.s5xx,
            },
        );
    }
    if !sites.is_empty() {
        history::update_web(base, date, &sites);
    }
}

/// 保存（原子写；失败仅日志）。
fn save_persisted(t: &WebTraffic, inner: &Inner) {
    let path = persist_path(&t.base);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let text = match serde_json::to_string_pretty(&persist_json(inner)) {
        Ok(t) => t,
        Err(e) => {
            util::log_error(&format!("网站流量持久化序列化失败：{e}"));
            return;
        }
    };
    if let Err(e) = dhrust::io::write_all_text_atomic(&path, &text) {
        util::log_error(&format!("网站流量持久化写入失败：{e}"));
    }
}

/// 加载历史（不存在/损坏时静默从零开始）。
fn load_persisted(base: &Path, inner: &mut Inner) {
    let path = persist_path(base);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return;
    };
    let Ok(v) = serde_json::from_str::<Json>(&text) else {
        return;
    };

    if let Some(sites) = v.get("sites").and_then(|x| x.as_object()) {
        for (name, s) in sites {
            let mut st = SiteStats::default();
            st.date = s
                .get("date")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            st.today = day_from_json(s.get("today"));
            st.yesterday = day_from_json(s.get("yesterday"));
            st.total_bytes = s.get("totalBytes").and_then(|x| x.as_u64()).unwrap_or(0);
            st.total_requests = s
                .get("totalRequests")
                .and_then(|x| x.as_u64())
                .unwrap_or(0);
            st.rate_last = st.total_bytes;
            st.rate_init = true;
            inner.stats.insert(name.clone(), st);
        }
    }

    if let Some(files) = v.get("files").and_then(|x| x.as_object()) {
        for (p, f) in files {
            let mut st = FileState::default();
            st.inode = f.get("inode").and_then(|x| x.as_u64()).unwrap_or(0);
            st.offset = f.get("offset").and_then(|x| x.as_u64()).unwrap_or(0);
            st.initialized = true;
            inner.files.insert(PathBuf::from(p), st);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ————— 解析 —————

    #[test]
    fn parses_nginx_combined_lines() {
        let line = r#"192.168.1.10 - - [01/Oct/2026:10:00:00 +0800] "GET /index.html HTTP/1.1" 200 5123 "https://a.com/" "Mozilla/5.0""#;
        let ev = parse_log_line(line).unwrap();
        assert_eq!(ev.status, 200);
        assert_eq!(ev.bytes, 5123);
        assert_eq!(ev.ip.unwrap().to_string(), "192.168.1.10");

        // 宝塔/标准 nginx 带 XFF 的格式也在第一对引号后取数
        let line2 = r#"1.2.3.4 - - [01/Oct/2026:10:00:00 +0800] "POST /api HTTP/1.1" 404 0 "-" "curl" "8.8.8.8""#;
        let ev2 = parse_log_line(line2).unwrap();
        assert_eq!(ev2.status, 404);
        assert_eq!(ev2.bytes, 0);
    }

    #[test]
    fn parses_apache_and_broken_lines() {
        // Apache common：响应体 `-`
        let line = r#"203.0.113.5 - frank [01/Oct/2026:10:00:00 +0800] "HEAD / HTTP/1.0" 304 -"#;
        let ev = parse_log_line(line).unwrap();
        assert_eq!(ev.status, 304);
        assert_eq!(ev.bytes, 0);

        // 坏行：不 panic 且拒绝
        assert!(parse_log_line("").is_none());
        assert!(parse_log_line("garbage line without quotes").is_none());
        assert!(parse_log_line(r#"1.2.3.4 - - [t] "GET / HTTP/1.1" xyz 100"#).is_none());
    }

    #[test]
    fn parses_caddy_json_lines() {
        let line = r#"{"level":"info","ts":1759294800.1,"logger":"http.log.access","msg":"handled request","request":{"remote_ip":"172.16.0.9","host":"a.com","method":"GET","uri":"/"},"bytes_read":0,"size":2048,"status":200}"#;
        let ev = parse_log_line(line).unwrap();
        assert_eq!(ev.status, 200);
        assert_eq!(ev.bytes, 2048);
        assert_eq!(ev.ip.unwrap().to_string(), "172.16.0.9");

        // 非访问日志的 JSON 行（无 status）被忽略
        assert!(parse_log_line(r#"{"level":"info","msg":"starting"}"#).is_none());
    }

    // ————— nginx 配置 —————

    #[test]
    fn extracts_nginx_server_blocks() {
        let text = r#"
http {
    # 全局默认日志
    access_log /var/log/nginx/access.log;
    server {
        listen 80;
        server_name example.com www.example.com;
        access_log /www/wwwlogs/example.com.log main;
        location / {
            root /www/wwwroot/example.com;
        }
    }
    server {
        listen 443 ssl;
        server_name api.example.com;
        access_log off;
    }
    server {
        listen 8080;
        server_name dev.local;
    }
    include /etc/nginx/conf.d/*.conf;
}
"#;
        let got = parse_nginx_vhosts(text);
        assert_eq!(got.len(), 1, "只应取到带 server_name 与 access_log 的块：{got:?}");
        assert_eq!(got[0].0, "example.com");
        assert_eq!(got[0].1, PathBuf::from("/www/wwwlogs/example.com.log"));
    }

    #[test]
    fn extracts_baota_style_vhost() {
        // 宝塔面板生成的站点配置（真实结构：server 块末尾 access_log + location 内 /dev/null）
        let text = r#"
server
{
    listen 80;
    server_name example.com www.example.com;
    index index.php index.html;
    root /www/wwwroot/example.com;

    #SSL-START SSL相关配置，请勿删除
    #error_page 404/404.html;
    #SSL-END

    include enable-php-74.conf;

    location ~ .*\.(gif|jpg|jpeg|png|bmp|swf)$
    {
        expires      30d;
        error_log /dev/null;
        access_log /dev/null;
    }

    location ~ .*\.(js|css)?$
    {
        expires      12h;
        error_log /dev/null;
        access_log /dev/null;
    }

    access_log  /www/wwwlogs/example.com.log;
    error_log  /www/wwwlogs/example.com.error.log;
}
"#;
        let got = parse_nginx_vhosts(text);
        assert_eq!(
            got.len(),
            1,
            "应只提取 server 直接子层的真实日志：{got:?}"
        );
        assert_eq!(got[0].0, "example.com");
        assert_eq!(got[0].1, PathBuf::from("/www/wwwlogs/example.com.log"));

        // 80→443 跳转块无 access_log：跳过；/dev/null 不得被收集
        let text = r#"
server
{
    listen 80;
    server_name shop.example.com;
    return 301 https://$host$request_uri;
}
server
{
    listen 443 ssl;
    server_name shop.example.com;
    access_log /dev/null;
    access_log /www/wwwlogs/shop.example.com.log;
}
"#;
        let got = parse_nginx_vhosts(text);
        assert_eq!(got.len(), 1, "跳转块应跳过：{got:?}");
        assert_eq!(got[0].1, PathBuf::from("/www/wwwlogs/shop.example.com.log"));
    }

    #[test]
    fn nginx_tokens_skips_comments_and_quotes() {
        let text = "# comment { server\nserver_name a.com; # tail\naccess_log \"/path with space/x.log\";";
        let tokens = nginx_tokens(text);
        assert!(tokens.contains(&"server_name".to_string()));
        assert!(tokens.contains(&"/path with space/x.log".to_string()));
        assert!(!tokens.iter().any(|t| t.contains("comment")));
    }

    // ————— apache 配置 —————

    #[test]
    fn extracts_apache_vhosts() {
        let text = r#"
<VirtualHost *:80>
    ServerName shop.example.com
    DocumentRoot /var/www/shop
    CustomLog /var/log/apache2/shop-access.log combined
</VirtualHost>
<VirtualHost *:80>
    ServerName ignored.example.com
</VirtualHost>
"#;
        let got = parse_apache_vhosts(text);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, "shop.example.com");
        assert_eq!(got[0].1, PathBuf::from("/var/log/apache2/shop-access.log"));
    }

    // ————— 手动配置与合并 —————

    #[test]
    fn parses_manual_logs_config() {
        let got = parse_manual_logs("我的站=/var/log/nginx/a.log; b = /srv/b.log ;bad;c=");
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].site, "我的站");
        assert_eq!(got[0].path, PathBuf::from("/var/log/nginx/a.log"));
        assert_eq!(got[1].site, "b");
    }

    #[test]
    fn merges_manual_first_and_dedups_paths() {
        let manual = vec![Source {
            site: "m1".into(),
            path: PathBuf::from("/x/1.log"),
        }];
        let auto = vec![
            Source {
                site: "a1".into(),
                path: PathBuf::from("/x/1.log"), // 与手动重复：去重
            },
            Source {
                site: "a2".into(),
                path: PathBuf::from("/x/2.log"),
            },
        ];
        let merged = merge_sources(manual, auto);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].site, "m1");
        assert_eq!(merged[1].site, "a2");
    }

    // ————— 增量读取 —————

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ragent-weblog-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn append(path: &Path, text: &str) {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        f.write_all(text.as_bytes()).unwrap();
    }

    #[test]
    fn reads_increment_and_handles_rotation() {
        let dir = temp_dir("tail");
        let log = dir.join("access.log");
        let line1 = "1.1.1.1 - - [t] \"GET /1 HTTP/1.1\" 200 100\n";
        let line2 = "1.1.1.2 - - [t] \"GET /2 HTTP/1.1\" 200 200\n";
        append(&log, line1);
        append(&log, line2);
        // 半行（无换行符）：本轮不产出，下轮拼接
        append(&log, "1.1.1.3 - - [t] \"GET /3 HTTP/1.1\" 20");

        let mut st = FileState::default();
        // 首次读取：mtime 是今天 → 从头读
        let lines = read_increment(&log, &mut st).unwrap();
        assert_eq!(lines.len(), 2, "半行不应产出：{lines:?}");

        // 无新增：无行
        let lines = read_increment(&log, &mut st).unwrap();
        assert!(lines.is_empty());

        // 补全半行：产出完整行
        append(&log, "0 300\n");
        let lines = read_increment(&log, &mut st).unwrap();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("GET /3"));

        // 模拟 logrotate：改名旧文件 + 新文件
        std::fs::rename(&log, dir.join("access.log.1")).unwrap();
        append(&log, "2.2.2.2 - - [t] \"GET /new HTTP/1.1\" 500 50\n");
        let lines = read_increment(&log, &mut st).unwrap();
        assert_eq!(lines.len(), 1, "轮转后应读取新文件：{lines:?}");
        assert!(lines[0].contains("GET /new"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn truncation_resets_offset() {
        let dir = temp_dir("trunc");
        let log = dir.join("a.log");
        append(&log, "1.1.1.1 - - [t] \"GET /1 HTTP/1.1\" 200 100\n");
        let mut st = FileState::default();
        assert_eq!(read_increment(&log, &mut st).unwrap().len(), 1);

        // 截断重写（copytruncate）
        std::fs::write(&log, "1.1.1.9 - - [t] \"GET /2 HTTP/1.1\" 200 1\n").unwrap();
        let lines = read_increment(&log, &mut st).unwrap();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("GET /2"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ————— 聚合与滚动 —————

    #[test]
    fn aggregates_and_rolls_day() {
        let mut st = SiteStats::default();
        st.roll_day("2026-10-01");
        st.add(&LogLine {
            ip: Some("1.1.1.1".parse().unwrap()),
            status: 200,
            bytes: 100,
        });
        st.add(&LogLine {
            ip: Some("1.1.1.1".parse().unwrap()),
            status: 404,
            bytes: 10,
        });
        st.add(&LogLine {
            ip: Some("2.2.2.2".parse().unwrap()),
            status: 500,
            bytes: 5,
        });
        assert_eq!(st.today.bytes, 115);
        assert_eq!(st.today.requests, 3);
        assert_eq!(st.today.uv_total(), 2);
        assert_eq!(st.today.s2xx, 1);
        assert_eq!(st.today.s4xx, 1);
        assert_eq!(st.today.s5xx, 1);
        assert_eq!(st.total_bytes, 115);

        // 跨天：今日滚为昨日，今日清零
        st.roll_day("2026-10-02");
        assert_eq!(st.today.bytes, 0);
        assert_eq!(st.yesterday.bytes, 115);
        assert_eq!(st.yesterday.uv_total(), 2);
        assert_eq!(st.total_bytes, 115);

        // 速率：首帧只建基线（历史累计不产生速率尖峰），随后按每秒差分推进
        st.tick_rate();
        assert_eq!(st.rate_bps, 0, "首帧不应产生速率");
        st.add(&LogLine {
            ip: None,
            status: 200,
            bytes: 300,
        });
        st.tick_rate();
        assert_eq!(st.rate_bps, 300);
        st.tick_rate();
        assert_eq!(st.rate_bps, 150);
    }

    // ————— 持久化 —————

    #[test]
    fn persists_and_loads_state() {
        let dir = temp_dir("persist");
        let t = WebTraffic {
            base: dir.clone(),
            enabled: AtomicBool::new(true),
            inner: Mutex::new(Inner {
                sources: Vec::new(),
                stats: HashMap::new(),
                files: HashMap::new(),
                last_discover: None,
                last_persist: None,
                dirty: true,
            }),
        };

        {
            let mut inner = t.inner.lock().unwrap();
            let mut st = SiteStats::default();
            st.roll_day("2026-10-01");
            st.add(&LogLine {
                ip: Some("9.9.9.9".parse().unwrap()),
                status: 200,
                bytes: 777,
            });
            st.today.uv_base = 5; // 模拟历史 UV 基数
            inner.stats.insert("s1".to_string(), st);
            let mut fst = FileState::default();
            fst.inode = 42;
            fst.offset = 1024;
            fst.initialized = true;
            inner.files.insert(PathBuf::from("/x/a.log"), fst);
            save_persisted(&t, &inner);
        }

        let mut inner = Inner {
            sources: Vec::new(),
            stats: HashMap::new(),
            files: HashMap::new(),
            last_discover: None,
            last_persist: None,
            dirty: false,
        };
        load_persisted(&dir, &mut inner);

        let st = inner.stats.get("s1").unwrap();
        assert_eq!(st.today.bytes, 777);
        assert_eq!(st.today.uv_total(), 6, "UV = 基数 5 + 新集合 1");
        assert_eq!(st.total_bytes, 777);
        assert_eq!(st.rate_last, 777, "速率基线应重置为累计值");
        let f = inner.files.get(Path::new("/x/a.log")).unwrap();
        assert_eq!(f.inode, 42);
        assert_eq!(f.offset, 1024);
        assert!(f.initialized);

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ————— 每日归档 —————

    #[test]
    fn closes_stale_days_into_history_files() {
        let dir = temp_dir("histclose");
        let mut stats: HashMap<String, SiteStats> = HashMap::new();

        // a.com：仍是“昨天”（9-30）的数据，跨天时应收尾归档
        let mut a = SiteStats::default();
        a.roll_day("2026-09-30");
        a.add(&LogLine {
            ip: Some("1.1.1.1".parse().unwrap()),
            status: 200,
            bytes: 500,
        });
        a.today.uv_base = 3; // 模拟历史 UV 基数
        stats.insert("a.com".to_string(), a);

        // b.com：已是“今天”（10-01）的数据
        let mut b = SiteStats::default();
        b.roll_day("2026-10-01");
        b.add(&LogLine {
            ip: None,
            status: 500,
            bytes: 100,
        });
        stats.insert("b.com".to_string(), b);

        close_stale_days(&dir, &mut stats, "2026-10-01");

        // 旧日归档：只含 9-30 的 a.com；UV = 基数 3 + 集合 1
        let sites = crate::history::day_web_sites(&dir, "2026-09-30");
        assert_eq!(sites.len(), 1, "{sites:?}");
        let a_day = &sites["a.com"];
        assert_eq!(a_day.hits, 1);
        assert_eq!(a_day.bytes, 500);
        assert_eq!(a_day.uv, 4);

        // 滚动完成：a 的今日清零、昨日保留；b 不受影响
        let a = &stats["a.com"];
        assert_eq!(a.date, "2026-10-01");
        assert_eq!(a.today.requests, 0);
        assert_eq!(a.yesterday.requests, 1);
        assert_eq!(stats["b.com"].today.requests, 1);

        // 今日未跨天收尾（b 的数据由周期写入负责）：10-01 尚无记录
        assert!(crate::history::day_web_sites(&dir, "2026-10-01").is_empty());

        crate::history::drop_storage_for_test(&dir);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
