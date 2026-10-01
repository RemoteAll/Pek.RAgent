//! 流量历史（每日归档）：网站流量与端口流量按自然日落地 `Data/traffic/{YYYY-MM-DD}.json`。
//!
//! 设计要点：
//! - **绝对量快照**：文件内保存“该日截至最后写入时刻的累计值”（而非增量），同一日重复写入
//!   为幂等合并（按站点/端口键覆盖），进程重启或一天内多次写入不会重复计数。
//! - **尽力而为**：无事务；崩溃/断电最多丢失最后一次写入（≤30 秒）的数据。
//! - **保留策略**：`TrafficHistoryDays`（默认 90 天；0 = 不清理），每天最多清理一次。
//! - 线程模型：weblog / portstat 两个后台线程共用本模块，读-改-写以进程内互斥锁保护；
//!   写入走原子替换（临时文件 + rename），读取永远看到完整文件。
//!
//! 文件结构示例：
//! ```json
//! {
//!   "date": "2026-10-01",
//!   "updated": 1759294800,
//!   "web": { "sites": { "example.com": { "hits": 12, "bytes": 3456, "uv": 5,
//!                                        "s2xx": 10, "s3xx": 0, "s4xx": 1, "s5xx": 1 } } },
//!   "ports": { "tcp:80": { "rx": 1024, "tx": 2048 } }
//! }
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use chrono::{Days, Local, NaiveDate};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value as Json};

use crate::util;

/// 保留天数上限（防御性）。
pub const MAX_RETENTION_DAYS: u32 = 3650;
/// 保留天数下限（非 0 时）。
pub const MIN_RETENTION_DAYS: u32 = 7;

/// 读-改-写互斥（weblog 与 portstat 线程共用）。
static LOCK: Mutex<()> = Mutex::new(());
/// 最近一次清理的日期（每天最多清理一次）。
static LAST_PRUNE: Mutex<Option<String>> = Mutex::new(None);

// ————— 数据模型 —————

/// 单日归档文件。
#[derive(Serialize, Deserialize, Default, Debug)]
pub struct DayFile {
    /// 日期（`%Y-%m-%d`）
    pub date: String,
    /// 最后写入时间（epoch 秒）
    #[serde(default)]
    pub updated: i64,
    /// 网站流量
    #[serde(default)]
    pub web: WebPart,
    /// 端口流量（键：`tcp:80` / `udp:53`）
    #[serde(default)]
    pub ports: BTreeMap<String, PortCounters>,
}

/// 网站流量段。
#[derive(Serialize, Deserialize, Default, Debug)]
pub struct WebPart {
    /// 站点名 → 当日汇总
    #[serde(default)]
    pub sites: BTreeMap<String, SiteDay>,
}

/// 单站点单日汇总（绝对量：当日截至最后写入时刻的累计）。
#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq, Eq)]
pub struct SiteDay {
    /// 请求数
    pub hits: u64,
    /// 响应体字节（统计口径与面板一致）
    pub bytes: u64,
    /// 独立 IP（各站点独立计数；跨站不去重）
    pub uv: u64,
    /// 2xx 状态码数
    #[serde(default)]
    pub s2xx: u64,
    /// 3xx 状态码数
    #[serde(default)]
    pub s3xx: u64,
    /// 4xx 状态码数
    #[serde(default)]
    pub s4xx: u64,
    /// 5xx 状态码数
    #[serde(default)]
    pub s5xx: u64,
}

/// 单端口当日收发字节。
#[derive(Serialize, Deserialize, Default, Clone, Copy, Debug, PartialEq, Eq)]
pub struct PortCounters {
    /// 接收字节（到达该端口）
    pub rx: u64,
    /// 发送字节（从该端口发出）
    pub tx: u64,
}

// ————— 路径与命名 —————

/// 历史目录：`{base}/Data/traffic`。
pub fn dir(base: &Path) -> PathBuf {
    base.join("Data").join("traffic")
}

/// 单日文件路径。
pub fn day_path(base: &Path, date: &str) -> PathBuf {
    dir(base).join(format!("{date}.json"))
}

/// 本地日期字符串（`%Y-%m-%d`）。
pub fn today_string() -> String {
    Local::now().format("%Y-%m-%d").to_string()
}

/// 端口键（`tcp:80`）。
pub fn port_key(proto: &str, port: u16) -> String {
    format!("{proto}:{port}")
}

/// 端口键解析（`tcp:80` → (`tcp`, 80)）。
pub fn parse_port_key(key: &str) -> Option<(&str, u16)> {
    let (proto, port) = key.split_once(':')?;
    if proto.is_empty() {
        return None;
    }
    Some((proto, port.parse().ok()?))
}

/// 是否为合法日期名（`%Y-%m-%d`）。
fn is_day_name(s: &str) -> bool {
    s.len() == 10 && NaiveDate::parse_from_str(s, "%Y-%m-%d").is_ok()
}

// ————— 读取与写入 —————

/// 读取单日文件（不存在返回默认；损坏则改名 `.corrupt` 保留现场后从零开始）。
pub(crate) fn read_day(base: &Path, date: &str) -> DayFile {
    let path = day_path(base, date);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return DayFile::default();
    };
    match serde_json::from_str::<DayFile>(&text) {
        Ok(day) => day,
        Err(e) => {
            util::log_error(&format!(
                "流量历史文件损坏（{}）：{e}；改名保留后重新开始",
                path.display()
            ));
            let corrupt = path.with_extension("json.corrupt");
            let _ = std::fs::remove_file(&corrupt);
            let _ = std::fs::rename(&path, &corrupt);
            DayFile::default()
        }
    }
}

/// 写入单日文件（原子替换；失败仅日志）。
fn write_day(base: &Path, day: &DayFile) {
    let path = day_path(base, &day.date);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let text = match serde_json::to_string_pretty(day) {
        Ok(t) => t,
        Err(e) => {
            util::log_error(&format!("流量历史序列化失败：{e}"));
            return;
        }
    };
    if let Err(e) = dhrust::io::write_all_text_atomic(&path, &text) {
        util::log_error(&format!("流量历史写入失败（{}）：{e}", path.display()));
    }
}

/// 合并写入某日网站快照（按站点名覆盖；该日其它站点与端口数据保留）。
///
/// 可重复调用：同一站点同一日的多次写入为“最新绝对量”覆盖，幂等。
pub fn update_web(base: &Path, date: &str, sites: &BTreeMap<String, SiteDay>) {
    if sites.is_empty() || !is_day_name(date) {
        return;
    }
    let _guard = LOCK.lock().unwrap();
    let mut day = read_day(base, date);
    day.date = date.to_string();
    day.updated = Local::now().timestamp();
    for (name, stats) in sites {
        day.web.sites.insert(name.clone(), stats.clone());
    }
    write_day(base, &day);
}

/// 合并写入某日端口快照（按端口键覆盖；该日其它端口与网站数据保留）。
pub fn update_ports(base: &Path, date: &str, ports: &BTreeMap<String, PortCounters>) {
    if ports.is_empty() || !is_day_name(date) {
        return;
    }
    let _guard = LOCK.lock().unwrap();
    let mut day = read_day(base, date);
    day.date = date.to_string();
    day.updated = Local::now().timestamp();
    for (key, counters) in ports {
        day.ports.insert(key.clone(), *counters);
    }
    write_day(base, &day);
}

/// 清理过期历史（每天最多执行一次；`retention_days = 0` 表示不清理）。
pub fn maybe_prune(base: &Path, retention_days: u32) {
    let today = today_string();
    {
        let guard = LAST_PRUNE.lock().unwrap();
        if guard.as_deref() == Some(today.as_str()) {
            return;
        }
    }
    let _lock = LOCK.lock().unwrap();
    {
        let mut guard = LAST_PRUNE.lock().unwrap();
        if guard.as_deref() == Some(today.as_str()) {
            return;
        }
        *guard = Some(today.clone());
    }
    if retention_days == 0 {
        return;
    }
    let retention = retention_days.clamp(MIN_RETENTION_DAYS, MAX_RETENTION_DAYS);
    let Ok(today_date) = NaiveDate::parse_from_str(&today, "%Y-%m-%d") else {
        return;
    };
    // 保留 N 天（含今天）：删除严格早于 (今天 - (N-1) 天) 的文件
    let Some(cutoff) = today_date.checked_sub_days(Days::new(retention as u64 - 1)) else {
        return;
    };
    let cutoff_text = cutoff.format("%Y-%m-%d").to_string();
    let Ok(entries) = std::fs::read_dir(dir(base)) else {
        return;
    };
    let mut removed = 0usize;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let Some(stem) = name.strip_suffix(".json") else {
            continue;
        };
        if !is_day_name(stem) {
            continue;
        }
        if stem < cutoff_text.as_str() && std::fs::remove_file(entry.path()).is_ok() {
            removed += 1;
        }
    }
    if removed > 0 {
        util::log_format(
            "流量历史清理：移除 {} 个过期日文件（保留 {} 天）",
            &[&removed.to_string(), &retention_days.to_string()],
        );
    }
}

/// 面板快照：最近 `days` 天（按日期升序），附带保留天数与每日合计。
pub fn snapshot_json(base: &Path, days: usize, retention_days: u32) -> Json {
    let mut files: Vec<(String, PathBuf)> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir(base)) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let Some(stem) = name.strip_suffix(".json") else {
                continue;
            };
            if is_day_name(stem) {
                files.push((stem.to_string(), entry.path()));
            }
        }
    }
    files.sort();

    let take = days.clamp(1, MAX_RETENTION_DAYS as usize);
    let start = files.len().saturating_sub(take);
    let mut out: Vec<Json> = Vec::new();
    for (date, path) in &files[start..] {
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        let Ok(day) = serde_json::from_str::<DayFile>(&text) else {
            continue;
        };
        let hits: u64 = day.web.sites.values().map(|s| s.hits).sum();
        let bytes: u64 = day.web.sites.values().map(|s| s.bytes).sum();
        let uv: u64 = day.web.sites.values().map(|s| s.uv).sum();
        out.push(json!({
            "date": date,
            "web": {
                "hits": hits,
                "bytes": bytes,
                "uv": uv,
                "sites": day.web.sites,
            },
            "ports": day.ports,
        }));
    }
    json!({
        "retentionDays": retention_days,
        "days": out,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ragent-history-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn site(hits: u64, bytes: u64, uv: u64) -> SiteDay {
        SiteDay {
            hits,
            bytes,
            uv,
            ..Default::default()
        }
    }

    #[test]
    fn merges_web_snapshots_across_writes() {
        let dir = temp_dir("web");
        let mut first = BTreeMap::new();
        first.insert("a.com".to_string(), site(1, 100, 1));
        update_web(&dir, "2026-10-01", &first);

        let mut second = BTreeMap::new();
        let mut a2 = site(2, 200, 2);
        a2.s4xx = 1;
        second.insert("a.com".to_string(), a2);
        second.insert("b.com".to_string(), site(5, 500, 3));
        update_web(&dir, "2026-10-01", &second);

        let day = read_day(&dir, "2026-10-01");
        assert_eq!(day.date, "2026-10-01");
        assert_eq!(day.web.sites.len(), 2);
        assert_eq!(day.web.sites["a.com"].hits, 2, "同键应覆盖为最新快照");
        assert_eq!(day.web.sites["a.com"].s4xx, 1);
        assert_eq!(day.web.sites["b.com"].bytes, 500);
        assert!(day.ports.is_empty());

        // 非法日期不落盘
        update_web(&dir, "not-a-day", &second);
        assert!(!day_path(&dir, "not-a-day").exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn merges_port_snapshots_and_keeps_others() {
        let dir = temp_dir("port");
        let mut first = BTreeMap::new();
        first.insert(port_key("tcp", 80), PortCounters { rx: 1, tx: 2 });
        update_ports(&dir, "2026-10-01", &first);

        let mut second = BTreeMap::new();
        second.insert(port_key("tcp", 80), PortCounters { rx: 10, tx: 20 });
        second.insert(port_key("udp", 53), PortCounters { rx: 3, tx: 4 });
        update_ports(&dir, "2026-10-01", &second);

        let day = read_day(&dir, "2026-10-01");
        assert_eq!(day.ports["tcp:80"], PortCounters { rx: 10, tx: 20 });
        assert_eq!(day.ports["udp:53"], PortCounters { rx: 3, tx: 4 });

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn snapshot_limits_range_and_sorts() {
        let dir = temp_dir("snap");
        for day in ["2026-09-28", "2026-09-29", "2026-09-30", "2026-10-01"] {
            let mut sites = BTreeMap::new();
            sites.insert("a.com".to_string(), site(1, 10, 1));
            update_web(&dir, day, &sites);
        }
        // 非日期文件与损坏文件应被忽略
        let tdir = dir.join("Data").join("traffic");
        std::fs::write(tdir.join("notes.json"), "{}").unwrap();
        std::fs::write(tdir.join("2026-09-27.json"), "{broken").unwrap();

        let v = snapshot_json(&dir, 3, 90);
        assert_eq!(v["retentionDays"], 90);
        let days = v["days"].as_array().unwrap();
        assert_eq!(days.len(), 3);
        assert_eq!(days[0]["date"], "2026-09-29");
        assert_eq!(days[2]["date"], "2026-10-01");
        assert_eq!(days[2]["web"]["hits"], 1);
        assert_eq!(days[2]["web"]["bytes"], 10);
        assert_eq!(days[2]["web"]["sites"]["a.com"]["uv"], 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn prunes_expired_days_only() {
        let dir = temp_dir("prune");
        let today = today_string();
        let today_date = NaiveDate::parse_from_str(&today, "%Y-%m-%d").unwrap();
        let old = (today_date - Days::new(100)).format("%Y-%m-%d").to_string();
        let recent = (today_date - Days::new(5)).format("%Y-%m-%d").to_string();
        for day in [old.as_str(), recent.as_str(), today.as_str()] {
            let mut sites = BTreeMap::new();
            sites.insert("a.com".to_string(), site(1, 10, 1));
            update_web(&dir, day, &sites);
        }

        // 重置清理守卫，确保本测试真实执行清理
        *LAST_PRUNE.lock().unwrap() = None;
        maybe_prune(&dir, 90);
        assert!(!day_path(&dir, &old).exists(), "超过 90 天的日文件应被清理");
        assert!(day_path(&dir, &recent).exists());
        assert!(day_path(&dir, &today).exists());

        // 0 = 不清理（旧文件已在上一步删除，此调用只验证不报错）
        *LAST_PRUNE.lock().unwrap() = None;
        maybe_prune(&dir, 0);
        assert!(day_path(&dir, &recent).exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn port_keys_roundtrip() {
        assert_eq!(port_key("tcp", 80), "tcp:80");
        assert_eq!(parse_port_key("tcp:80"), Some(("tcp", 80)));
        assert_eq!(parse_port_key("udp:65535"), Some(("udp", 65535)));
        assert!(parse_port_key("tcp").is_none());
        assert!(parse_port_key(":80").is_none());
        assert!(parse_port_key("tcp:abc").is_none());
    }
}
