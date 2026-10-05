//! 流量历史（每日归档）：网站流量与端口流量按自然日落地 SQLite（Pek.RCode / XCode 体系）。
//!
//! **存储规范**：模型按 XCode 规范维护于 `Entity/Model.xml`（唯一事实来源，编译期内嵌），
//! 运行时经 Pek.RCode（DH.NCode 的 Rust 实现）打开 `Data/traffic.db` 并增量同步表结构；
//! 与 C# 生态共用同一份模型文件（`rcodegen` 可据此生成实体 / 反向工程）。
//!
//! 设计要点：
//! - **绝对量快照**：每行保存“该日截至最后写入时刻的累计值”（非增量），同键重复写入为
//!   幂等覆盖（自然键：日期 + 站点 / 日期 + 协议 + 端口），进程重启或一天内多次写入不会重复计数；
//! - **读取缓存**：面板读取走 Pek.RCode 实体缓存（整表缓存；默认 60 秒过期、任何写入即时失效）；
//! - **保留策略**：`TrafficHistoryDays`（默认 90 天；0 = 不清理），每天最多清理一次；
//! - **旧版迁移**：首次运行自动导入旧版 `Data/traffic/{日期}.json` 并删除已导入文件；
//! - 线程模型：weblog / portstat / webpanel 共用本模块，读写以进程内互斥锁串行。
//!
//! 表结构（详见 `Entity/Model.xml`）：
//! - `Agent_WebTrafficDaily`：日期 + 站点 → 请求数 / 字节 / UV / 状态码分布；
//! - `Agent_PortTrafficDaily`：日期 + 协议 + 端口 → 当日接收 / 发送字节。

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use chrono::{Days, Local, NaiveDate, NaiveDateTime};
use pek_rcode::store::{self, SharedStore};
use pek_rcode::{Dal, DbRow, DbValue, EntityModel, Query, SqlSession, Where};
use serde::Deserialize;
use serde_json::{json, Value as Json};

use crate::util;

/// XCode 模型文件（编译期内嵌；随源码维护并与 C# 生态互通）。
const MODEL_XML: &str = include_str!("../Entity/Model.xml");
/// 网站流量表（实体名，见 `Entity/Model.xml`）。
const TABLE_WEB: &str = "WebTrafficDaily";
/// 端口流量表。
const TABLE_PORTS: &str = "PortTrafficDaily";
/// 数据库文件名（位于 `Data/` 下）。
const DB_FILE: &str = "traffic.db";
/// 单次清理最多删除的行数（防御）。
const MAX_PRUNE_ROWS: usize = 100_000;

/// 保留天数上限（防御性）。
pub const MAX_RETENTION_DAYS: u32 = 3650;
/// 保留天数下限（非 0 时）。
pub const MIN_RETENTION_DAYS: u32 = 7;

// ————— 数据模型 —————

/// 单站点单日汇总（绝对量：当日截至最后写入时刻的累计）。
#[derive(Default, Clone, Debug, PartialEq, Eq)]
pub struct SiteDay {
    /// 请求数
    pub hits: u64,
    /// 响应体字节（统计口径与面板一致）
    pub bytes: u64,
    /// 独立 IP（各站点独立计数；跨站不去重）
    pub uv: u64,
    /// 2xx 状态码数
    pub s2xx: u64,
    /// 3xx 状态码数
    pub s3xx: u64,
    /// 4xx 状态码数
    pub s4xx: u64,
    /// 5xx 状态码数
    pub s5xx: u64,
}

/// 单端口当日累计收发字节。
#[derive(Default, Clone, Copy, Debug, PartialEq, Eq)]
pub struct PortCounters {
    /// 接收字节（到达该端口）
    pub rx: u64,
    /// 发送字节（从该端口发出）
    pub tx: u64,
}

// ————— 数据访问层（Pek.RCode 共享存储：单飞打开 / 串行会话） —————

/// 打开失败的日志节流（同一错误只记一次；成功恢复后重置）。
static OPEN_ERROR: Mutex<Option<String>> = Mutex::new(None);

/// 获取（或首次打开）某数据目录的共享存储。
///
/// 打开（含 `sync_schema` 建表）由 `pek_rcode::store` 在注册表锁内**单飞执行**：
/// 多个采样线程（网站流量 / 端口流量）同时首开同一数据库时只能有一个执行建表，
/// 否则并发 `CREATE TABLE` 会撞 “table already exists”
/// （2026-10-02 服务器实测：两线程首开竞态，后到者建表失败）。
fn storage(base: &Path) -> Result<Arc<SharedStore>, String> {
    match store::get_or_open(base, || open_dal(base)) {
        Ok((opened, created)) => {
            if created && OPEN_ERROR.lock().unwrap().take().is_some() {
                util::log_info("流量历史数据库已恢复可用");
            }
            Ok(opened)
        }
        Err(e) => {
            let mut last = OPEN_ERROR.lock().unwrap();
            if last.as_deref() != Some(e.as_str()) {
                util::log_error(&format!("流量历史数据库打开失败：{e}"));
                *last = Some(e.clone());
            }
            Err(e)
        }
    }
}

/// 在共享数据访问层上执行一次会话操作（写路径统一串行）。
///
/// 供同库其它模块（面板用户 `Agent_PanelUser`、操作审计 `Agent_OperationLog`）复用
/// 同一连接、同一建表单飞逻辑（防并发 `CREATE TABLE` 竞态）与同一读写锁。
pub(crate) fn with_store<F, R>(base: &Path, f: F) -> Result<R, String>
where
    F: FnOnce(&Dal, &mut dyn SqlSession) -> pek_rcode::Result<R>,
{
    storage(base)?.with_session(f)
}

/// 打开数据库（`store::get_or_open` 的单飞闭包）：解析内嵌模型 → SQLite → 增量同步表结构 → 首轮旧版 JSON 迁移。
fn open_dal(base: &Path) -> Result<Dal, String> {
    let model = EntityModel::parse(MODEL_XML).map_err(|e| format!("Model.xml 解析失败：{e}"))?;
    let db_path = base.join("Data").join(DB_FILE);
    if let Some(dir) = db_path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("创建数据目录失败：{e}"))?;
    }
    let conn = format!("Data Source={};Provider=SQLite;ShowSql=false", db_path.display());
    let dal =
        Dal::open_with_model(&conn, model).map_err(|e| format!("打开 SQLite 失败：{e}"))?;
    dal.sync_schema()
        .map_err(|e| format!("同步表结构失败：{e}"))?;
    util::log_format(
        "流量历史数据库已就绪（{}）",
        &[&db_path.display().to_string()],
    );
    import_legacy_json(&dal, base);
    Ok(dal)
}

/// 释放某数据目录的连接（测试用；Windows 下否则无法删除临时目录）。
#[cfg(test)]
pub(crate) fn drop_storage_for_test(base: &Path) {
    store::drop_for_test(base);
}

// ————— 写入 —————

/// 合并写入某日网站快照（按自然键“日期 + 站点”覆盖；幂等）。
pub fn update_web(base: &Path, date: &str, sites: &BTreeMap<String, SiteDay>) {
    if sites.is_empty() {
        return;
    }
    let Ok(store) = storage(base) else {
        return;
    };
    let Some(day) = parse_day(date) else {
        return;
    };
    let _guard = store.lock();
    let mut session = match store.dal().open_session() {
        Ok(session) => session,
        Err(e) => {
            util::log_error(&format!("流量历史会话创建失败：{e}"));
            return;
        }
    };
    for (site, stats) in sites {
        if let Err(e) = upsert_web_row(store.dal(), session.as_mut(), &day, site, stats) {
            util::log_error(&format!("网站流量落库失败（{date} {site}）：{e}"));
        }
    }
}

/// 合并写入某日端口快照（按自然键“日期 + 协议 + 端口”覆盖；幂等）。
pub fn update_ports(base: &Path, date: &str, ports: &BTreeMap<String, PortCounters>) {
    if ports.is_empty() {
        return;
    }
    let Ok(store) = storage(base) else {
        return;
    };
    let Some(day) = parse_day(date) else {
        return;
    };
    let _guard = store.lock();
    let mut session = match store.dal().open_session() {
        Ok(session) => session,
        Err(e) => {
            util::log_error(&format!("流量历史会话创建失败：{e}"));
            return;
        }
    };
    for (key, counters) in ports {
        let Some((proto, port)) = parse_port_key(key) else {
            continue;
        };
        if let Err(e) = upsert_port_row(store.dal(), session.as_mut(), &day, proto, port, counters) {
            util::log_error(&format!("端口流量落库失败（{date} {key}）：{e}"));
        }
    }
}

/// 插入或更新一行网站流量（自然键：日期 + 站点）。
fn upsert_web_row(
    dal: &Dal,
    session: &mut dyn SqlSession,
    day: &NaiveDateTime,
    site: &str,
    stats: &SiteDay,
) -> pek_rcode::Result<()> {
    let table = dal.table(TABLE_WEB)?;
    let filter = Where::new().eq("StatDate", *day).eq("Site", site);
    let found = table.query(
        session,
        &Query::new().column("Id").filter(filter).take(1),
    )?;

    let values: [(&str, DbValue); 7] = [
        ("Hits", to_i64(stats.hits).into()),
        ("Bytes", to_i64(stats.bytes).into()),
        ("UV", to_i64(stats.uv).into()),
        ("S2xx", to_i64(stats.s2xx).into()),
        ("S3xx", to_i64(stats.s3xx).into()),
        ("S4xx", to_i64(stats.s4xx).into()),
        ("S5xx", to_i64(stats.s5xx).into()),
    ];

    if let Some(row) = found.first() {
        let id = row
            .get_by_name("Id")
            .and_then(|v| v.as_i64())
            .unwrap_or_default();
        table.update_by_pk(session, &values, &[id.into()])?;
    } else {
        let mut fields: Vec<(&str, DbValue)> = Vec::with_capacity(values.len() + 2);
        fields.push(("StatDate", (*day).into()));
        fields.push(("Site", site.into()));
        fields.extend_from_slice(&values);
        table.insert(session, &fields)?;
    }
    Ok(())
}

/// 插入或更新一行端口流量（自然键：日期 + 协议 + 端口）。
fn upsert_port_row(
    dal: &Dal,
    session: &mut dyn SqlSession,
    day: &NaiveDateTime,
    proto: &str,
    port: u16,
    counters: &PortCounters,
) -> pek_rcode::Result<()> {
    let table = dal.table(TABLE_PORTS)?;
    let filter = Where::new()
        .eq("StatDate", *day)
        .eq("Proto", proto)
        .eq("Port", i32::from(port));
    let found = table.query(
        session,
        &Query::new().column("Id").filter(filter).take(1),
    )?;

    let values: [(&str, DbValue); 2] = [
        ("Rx", to_i64(counters.rx).into()),
        ("Tx", to_i64(counters.tx).into()),
    ];

    if let Some(row) = found.first() {
        let id = row
            .get_by_name("Id")
            .and_then(|v| v.as_i64())
            .unwrap_or_default();
        table.update_by_pk(session, &values, &[id.into()])?;
    } else {
        let fields: [(&str, DbValue); 5] = [
            ("StatDate", (*day).into()),
            ("Proto", proto.into()),
            ("Port", i32::from(port).into()),
            ("Rx", to_i64(counters.rx).into()),
            ("Tx", to_i64(counters.tx).into()),
        ];
        table.insert(session, &fields)?;
    }
    Ok(())
}

// ————— 清理 —————

/// 最近一次清理的日期（每天最多清理一次）。
static LAST_PRUNE: Mutex<Option<String>> = Mutex::new(None);

/// 清理过期历史（每天最多执行一次；`retention_days = 0` 表示不清理）。
pub fn maybe_prune(base: &Path, retention_days: u32) {
    let today = today_string();
    {
        let guard = LAST_PRUNE.lock().unwrap();
        if guard.as_deref() == Some(today.as_str()) {
            return;
        }
    }
    let Ok(store) = storage(base) else {
        return;
    };
    let _lock = store.lock();
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
    // 保留 N 天（含今天）：删除严格早于 (今天 - (N-1) 天) 的记录
    let Some(cutoff_date) = today_date.checked_sub_days(Days::new(retention as u64 - 1)) else {
        return;
    };
    let Some(cutoff) = cutoff_date.and_hms_opt(0, 0, 0) else {
        return;
    };

    let mut session = match store.dal().open_session() {
        Ok(session) => session,
        Err(e) => {
            util::log_error(&format!("流量历史清理会话创建失败：{e}"));
            return;
        }
    };
    let mut removed = 0usize;
    for table_name in [TABLE_WEB, TABLE_PORTS] {
        let Ok(table) = store.dal().table(table_name) else {
            continue;
        };
        let filter = Where::new().lt("StatDate", cutoff);
        let rows = match table.query(
            session.as_mut(),
            &Query::new()
                .column("Id")
                .filter(filter)
                .take(MAX_PRUNE_ROWS),
        ) {
            Ok(rows) => rows,
            Err(e) => {
                util::log_error(&format!("流量历史清理查询失败（{table_name}）：{e}"));
                continue;
            }
        };
        for row in &rows {
            let Some(id) = row.get_by_name("Id").and_then(|v| v.as_i64()) else {
                continue;
            };
            if table.delete_by_pk(session.as_mut(), &[id.into()]).is_ok() {
                removed += 1;
            }
        }
    }
    if removed > 0 {
        util::log_format(
            "流量历史清理：移除 {} 条过期记录（保留 {} 天）",
            &[&removed.to_string(), &retention_days.to_string()],
        );
    }
}

// ————— 面板快照 —————

/// 面板快照：最近 `days` 天（按日期升序），附带保留天数与每日合计。
///
/// 读取走 Pek.RCode 实体缓存（整表缓存；写入自动失效、默认 60 秒过期）。
pub fn snapshot_json(base: &Path, days: usize, retention_days: u32) -> Json {
    let empty = || {
        json!({
            "retentionDays": retention_days,
            "days": Vec::<Json>::new(),
        })
    };
    let Ok(store) = storage(base) else {
        return empty();
    };
    let _guard = store.lock();
    let mut session = match store.dal().open_session() {
        Ok(session) => session,
        Err(e) => {
            util::log_error(&format!("流量历史读取会话创建失败：{e}"));
            return empty();
        }
    };

    let web_rows: Arc<Vec<DbRow>> = match store
        .dal()
        .entity_cache(TABLE_WEB)
        .and_then(|c| c.entities(store.dal(), session.as_mut()))
    {
        Ok(rows) => rows,
        Err(e) => {
            util::log_error(&format!("网站流量历史读取失败：{e}"));
            Arc::new(Vec::new())
        }
    };
    let port_rows: Arc<Vec<DbRow>> = match store
        .dal()
        .entity_cache(TABLE_PORTS)
        .and_then(|c| c.entities(store.dal(), session.as_mut()))
    {
        Ok(rows) => rows,
        Err(e) => {
            util::log_error(&format!("端口流量历史读取失败：{e}"));
            Arc::new(Vec::new())
        }
    };

    // 按日期分组
    let mut web: BTreeMap<String, BTreeMap<String, SiteDay>> = BTreeMap::new();
    for row in web_rows.iter() {
        let Some(date) = row
            .get_by_name("StatDate")
            .and_then(|v| v.as_datetime())
            .map(|dt| format_day(&dt))
        else {
            continue;
        };
        let site = row
            .get_by_name("Site")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let get = |name: &str| -> u64 {
            row.get_by_name(name)
                .and_then(|v| v.as_i64())
                .unwrap_or(0)
                .max(0) as u64
        };
        web.entry(date).or_default().insert(
            site,
            SiteDay {
                hits: get("Hits"),
                bytes: get("Bytes"),
                uv: get("UV"),
                s2xx: get("S2xx"),
                s3xx: get("S3xx"),
                s4xx: get("S4xx"),
                s5xx: get("S5xx"),
            },
        );
    }
    let mut ports: BTreeMap<String, BTreeMap<String, PortCounters>> = BTreeMap::new();
    for row in port_rows.iter() {
        let Some(date) = row
            .get_by_name("StatDate")
            .and_then(|v| v.as_datetime())
            .map(|dt| format_day(&dt))
        else {
            continue;
        };
        let proto = row
            .get_by_name("Proto")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let port = row
            .get_by_name("Port")
            .and_then(|v| v.as_i64())
            .unwrap_or(0)
            .clamp(0, u16::MAX as i64) as u16;
        let get = |name: &str| -> u64 {
            row.get_by_name(name)
                .and_then(|v| v.as_i64())
                .unwrap_or(0)
                .max(0) as u64
        };
        ports.entry(date).or_default().insert(
            port_key(&proto, port),
            PortCounters {
                rx: get("Rx"),
                tx: get("Tx"),
            },
        );
    }

    // 合并日期（升序），取最近 N 天
    let mut all_dates: BTreeSet<String> = BTreeSet::new();
    all_dates.extend(web.keys().cloned());
    all_dates.extend(ports.keys().cloned());
    let take = days.clamp(1, MAX_RETENTION_DAYS as usize);
    let start = all_dates.len().saturating_sub(take);

    let mut out: Vec<Json> = Vec::new();
    for date in all_dates.iter().skip(start) {
        let sites_map = web.get(date);
        let hits: u64 = sites_map
            .map(|m| m.values().map(|s| s.hits).sum())
            .unwrap_or(0);
        let bytes: u64 = sites_map
            .map(|m| m.values().map(|s| s.bytes).sum())
            .unwrap_or(0);
        let uv: u64 = sites_map
            .map(|m| m.values().map(|s| s.uv).sum())
            .unwrap_or(0);
        let sites_json: Json = sites_map
            .map(|m| {
                m.iter()
                    .map(|(name, s)| {
                        (
                            name.clone(),
                            json!({
                                "hits": s.hits,
                                "bytes": s.bytes,
                                "uv": s.uv,
                                "s2xx": s.s2xx,
                                "s3xx": s.s3xx,
                                "s4xx": s.s4xx,
                                "s5xx": s.s5xx,
                            }),
                        )
                    })
                    .collect::<serde_json::Map<String, Json>>()
                    .into()
            })
            .unwrap_or(Json::Object(serde_json::Map::new()));
        let ports_json: Json = ports
            .get(date)
            .map(|m| {
                m.iter()
                    .map(|(key, c)| (key.clone(), json!({ "rx": c.rx, "tx": c.tx })))
                    .collect::<serde_json::Map<String, Json>>()
                    .into()
            })
            .unwrap_or(Json::Object(serde_json::Map::new()));
        out.push(json!({
            "date": date,
            "web": {
                "hits": hits,
                "bytes": bytes,
                "uv": uv,
                "sites": sites_json,
            },
            "ports": ports_json,
        }));
    }

    json!({
        "retentionDays": retention_days,
        "days": out,
    })
}

// ————— 单日读取（测试辅助） —————

/// 某日全部站点快照（仅测试引用；生产路径读快照或实时状态）。
#[cfg(test)]
pub(crate) fn day_web_sites(base: &Path, date: &str) -> BTreeMap<String, SiteDay> {
    let mut out = BTreeMap::new();
    let Ok(store) = storage(base) else {
        return out;
    };
    let Some(day) = parse_day(date) else {
        return out;
    };
    let _guard = store.lock();
    let Ok(mut session) = store.dal().open_session() else {
        return out;
    };
    let Ok(table) = store.dal().table(TABLE_WEB) else {
        return out;
    };
    let Ok(rows) = table.query(
        session.as_mut(),
        &Query::new().filter(Where::new().eq("StatDate", day)),
    ) else {
        return out;
    };
    for row in &rows {
        let Some(site) = row.get_by_name("Site").and_then(|v| v.as_str()) else {
            continue;
        };
        let get = |name: &str| -> u64 {
            row.get_by_name(name)
                .and_then(|v| v.as_i64())
                .unwrap_or(0)
                .max(0) as u64
        };
        out.insert(
            site.to_string(),
            SiteDay {
                hits: get("Hits"),
                bytes: get("Bytes"),
                uv: get("UV"),
                s2xx: get("S2xx"),
                s3xx: get("S3xx"),
                s4xx: get("S4xx"),
                s5xx: get("S5xx"),
            },
        );
    }
    out
}

/// 某日全部端口快照（仅测试引用；生产路径读快照或实时状态）。
#[cfg(test)]
pub(crate) fn day_ports(base: &Path, date: &str) -> BTreeMap<String, PortCounters> {
    let mut out = BTreeMap::new();
    let Ok(store) = storage(base) else {
        return out;
    };
    let Some(day) = parse_day(date) else {
        return out;
    };
    let _guard = store.lock();
    let Ok(mut session) = store.dal().open_session() else {
        return out;
    };
    let Ok(table) = store.dal().table(TABLE_PORTS) else {
        return out;
    };
    let Ok(rows) = table.query(
        session.as_mut(),
        &Query::new().filter(Where::new().eq("StatDate", day)),
    ) else {
        return out;
    };
    for row in &rows {
        let Some(proto) = row.get_by_name("Proto").and_then(|v| v.as_str()) else {
            continue;
        };
        let port = row
            .get_by_name("Port")
            .and_then(|v| v.as_i64())
            .unwrap_or(0)
            .clamp(0, u16::MAX as i64) as u16;
        let get = |name: &str| -> u64 {
            row.get_by_name(name)
                .and_then(|v| v.as_i64())
                .unwrap_or(0)
                .max(0) as u64
        };
        out.insert(
            port_key(proto, port),
            PortCounters {
                rx: get("Rx"),
                tx: get("Tx"),
            },
        );
    }
    out
}

// ————— 数据库管理（面板“数据库”页：只读查询 / 备份 / 还原） —————

/// 物理表名（`Entity/Model.xml` 的 `TableName`；清空与信息统计用）。
const DB_TABLE_WEB: &str = "Agent_WebTrafficDaily";
/// 物理表名（端口表）。
const DB_TABLE_PORTS: &str = "Agent_PortTrafficDaily";
/// 物理表名（面板用户表）。
const DB_TABLE_USERS: &str = "Agent_PanelUser";
/// 物理表名（操作审计表）。
const DB_TABLE_OPLOG: &str = "Agent_OperationLog";

/// 备份范围内的全部业务表（实体名）——新增表时同步扩展。
fn backup_tables() -> [&'static str; 4] {
    [
        TABLE_WEB,
        TABLE_PORTS,
        crate::audit::TABLE_USER,
        crate::audit::TABLE_OPLOG,
    ]
}

/// 实体名 → 物理表名。
fn physical_table(entity: &str) -> &'static str {
    match entity {
        TABLE_WEB => DB_TABLE_WEB,
        TABLE_PORTS => DB_TABLE_PORTS,
        crate::audit::TABLE_USER => DB_TABLE_USERS,
        crate::audit::TABLE_OPLOG => DB_TABLE_OPLOG,
        _ => "",
    }
}

/// 只读查询校验（安全闸门）：仅放行单条 `SELECT` 文本。
///
/// 规则（宁可误杀不可放过）：
/// - 非空、长度 ≤ 4000 字符；
/// - 不允许包含 `;`（阻断多语句注入；字符串字面量内的分号一并拒绝）；
/// - 忽略前导空白后必须以 `SELECT` 关键字开头且后跟空白（挡住 `WITH`/`PRAGMA`/`SELECTX` 等路径）。
pub fn validate_readonly_sql(sql: &str) -> Result<(), String> {
    let s = sql.trim();
    if s.is_empty() {
        return Err("SQL 不能为空".into());
    }
    if s.len() > 4000 {
        return Err("SQL 过长（上限 4000 字符）".into());
    }
    if s.contains(';') {
        return Err("仅支持单条查询：SQL 中不允许出现分号（字符串内的分号也会被拒绝）".into());
    }
    // 必须以 SELECT 关键字开头，且其后紧跟空白或字符串结束（拒绝 "SELECTX" 之类）。
    let is_select = match s.get(..6) {
        Some(head) if head.eq_ignore_ascii_case("SELECT") => s
            .as_bytes()
            .get(6)
            .is_none_or(|b| b.is_ascii_whitespace()),
        _ => false,
    };
    if !is_select {
        return Err("仅允许 SELECT 只读查询（当前版本不开放增删改与建表操作）".into());
    }
    Ok(())
}

/// 数据库概况（面板“数据库”页）：文件路径、大小与各业务表行数。
pub fn database_info(base: &Path) -> Json {
    let db_path = base.join("Data").join(DB_FILE);
    let size = std::fs::metadata(&db_path).map(|m| m.len()).unwrap_or(0);
    let path_text = db_path.display().to_string();
    let mut tables: Vec<Json> = Vec::new();
    let mut error: Option<String> = None;
    match storage(base) {
        Ok(store) => {
            for entity in backup_tables() {
                let rows = store.dal().open_session().ok().and_then(|mut session| {
                    store
                        .dal()
                        .table(entity)
                        .ok()
                        .and_then(|t| t.count(session.as_mut(), None).ok())
                });
                tables.push(json!({ "name": physical_table(entity), "rows": rows }));
            }
        }
        Err(e) => error = Some(e),
    }
    json!({
        "path": path_text,
        "sizeBytes": size,
        "provider": "SQLite",
        "tables": tables,
        "error": error,
    })
}

/// `DbValue` → JSON（面板展示；BLOB 仅显示尺寸避免大对象）。
fn db_value_json(value: &DbValue) -> Json {
    match value {
        DbValue::Null => Json::Null,
        DbValue::Bool(b) => json!(b),
        DbValue::Int(i) => json!(i),
        DbValue::Float(f) => json!(f),
        DbValue::Decimal(d) => json!(d.to_string()),
        DbValue::Text(s) => json!(s),
        DbValue::Blob(b) => json!(format!("<BLOB {} B>", b.len())),
        DbValue::DateTime(dt) => json!(dt.format("%Y-%m-%d %H:%M:%S").to_string()),
    }
}

/// 只读执行 SQL（面板）：安全前置 [`validate_readonly_sql`]；结果最多返回 500 行（截断标记）。
pub fn query_readonly(base: &Path, sql: &str) -> Result<Json, String> {
    validate_readonly_sql(sql)?;
    let store = storage(base)?;
    let started = std::time::Instant::now();
    let mut session = store.dal().open_session().map_err(|e| e.to_string())?;
    let set = session.query(sql, &[]).map_err(|e| e.to_string())?;
    let elapsed_ms = started.elapsed().as_millis() as u64;

    const MAX_ROWS: usize = 500;
    let truncated = set.rows.len() > MAX_ROWS;
    let rows: Vec<Json> = set
        .rows
        .iter()
        .take(MAX_ROWS)
        .map(|row| {
            Json::Array(
                (0..set.columns.len())
                    .map(|i| row.get(i).map(db_value_json).unwrap_or(Json::Null))
                    .collect(),
            )
        })
        .collect();
    Ok(json!({
        "columns": set.columns.as_ref(),
        "rows": rows,
        "rowCount": rows.len(),
        "truncated": truncated,
        "elapsedMs": elapsed_ms,
    }))
}

/// 备份全部业务表（含面板用户与操作审计）为 DbTable zip 包（`backup_schema=true` 带模型 XML；与 C# 生态互通）。
pub fn backup_zip(base: &Path) -> Result<Vec<u8>, String> {
    let store = storage(base)?;
    let _guard = store.lock();
    let tmp = base.join("Data").join(format!(
        ".traffic-backup-{}.zip",
        Local::now().format("%Y%m%d%H%M%S%3f")
    ));
    let tables = backup_tables();
    let expected = tables.len();
    let result = store
        .dal()
        .backup_all(&tables, &tmp, true)
        .map_err(|e| format!("备份失败：{e}"))
        .and_then(|count| {
            if count < expected {
                Err(format!("备份失败：仅成功 {count}/{expected} 张表"))
            } else {
                std::fs::read(&tmp).map_err(|e| format!("读取备份文件失败：{e}"))
            }
        });
    let _ = std::fs::remove_file(&tmp);
    result
}

/// 从 DbTable zip 包还原业务表：先全量解码校验（不合格不动现有数据），再清空导入。
///
/// **自适应旧备份**：仅处理包内实际存在的表（旧版仅含两张流量表的备份不会清空
/// 面板用户与审计表）；至少需包含一张已知表。返回各表恢复行数；完成后失效实体缓存。
pub fn restore_zip(base: &Path, bytes: &[u8]) -> Result<Json, String> {
    let store = storage(base)?;

    // 1) 结构预校验：包内存在的已知表其 DbTable 流必须完整可解码（不合格不动现有数据）
    let mut present: Vec<&'static str> = Vec::new();
    {
        use std::io::Read;
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes))
            .map_err(|e| format!("不是有效的备份包（zip）：{e}"))?;
        let names: Vec<String> = (0..zip.len())
            .filter_map(|i| zip.by_index(i).ok().map(|f| f.name().to_string()))
            .collect();
        for entity in backup_tables() {
            let entry_name = format!("{entity}.table");
            if !names.iter().any(|n| n == &entry_name) {
                continue;
            }
            let mut entry = zip
                .by_name(&entry_name)
                .map_err(|_| format!("备份包缺少数据项 {entry_name}"))?;
            let mut data = Vec::new();
            entry
                .read_to_end(&mut data)
                .map_err(|e| format!("读取 {entry_name} 失败：{e}"))?;
            pek_rcode::dbtable::decode_rowset(&data)
                .map_err(|e| format!("{entry_name} 解码校验失败：{e}"))?;
            present.push(entity);
        }
        if present.is_empty() {
            return Err("备份包不包含任何已知数据表（WebTrafficDaily/PortTrafficDaily/PanelUser/OperationLog）".into());
        }
    }

    // 2) 落临时文件
    let tmp = base.join("Data").join(".traffic-restore.zip");
    std::fs::write(&tmp, bytes).map_err(|e| format!("写入临时文件失败：{e}"))?;

    // 3) 清空 + 导入（与写路径共用串行锁；完成后失效实体缓存）
    let expected = present.len();
    let outcome = (|| -> Result<(Vec<String>, Json), String> {
        let _guard = store.lock();
        let mut session = store.dal().open_session().map_err(|e| e.to_string())?;
        for entity in &present {
            let physical = physical_table(entity);
            session
                .execute(&format!("DELETE FROM \"{physical}\""), &[])
                .map_err(|e| format!("清空 {physical} 失败：{e}"))?;
        }
        drop(session);
        let done = store
            .dal()
            .restore_all(&tmp, Some(&present), false)
            .map_err(|e| format!("导入失败：{e}"))?;
        if done.len() < expected {
            return Err(format!(
                "导入不完整：成功 {}/{expected} 张表（请重新还原）",
                done.len()
            ));
        }
        let mut rows = serde_json::Map::new();
        for entity in &present {
            let mut session = store.dal().open_session().map_err(|e| e.to_string())?;
            let n = store
                .dal()
                .table(entity)
                .map_err(|e| e.to_string())?
                .count(session.as_mut(), None)
                .map_err(|e| e.to_string())?;
            rows.insert(physical_table(entity).to_string(), json!(n));
        }
        for entity in &present {
            store.dal().invalidate_cache(entity);
        }
        Ok((done, Json::Object(rows)))
    })();

    let _ = std::fs::remove_file(&tmp);
    let (done, rows) = outcome?;
    util::log_info(&format!(
        "Web 面板数据库还原完成：表 {done:?}，行数 {rows}"
    ));
    Ok(json!({ "tables": done, "rows": rows }))
}

/// 服务器端备份目录：`{base}/Data/Backup`。
fn backup_dir(base: &Path) -> PathBuf {
    base.join("Data").join("Backup")
}

/// 备份文件名校验（安全闸门）：仅允许简单文件名（防路径穿越；`[A-Za-z0-9._-]` 且以 `.zip` 结尾）。
pub fn validate_backup_name(name: &str) -> Result<(), String> {
    let ok = !name.is_empty()
        && name.len() <= 128
        && name.ends_with(".zip")
        && !name.contains("..")
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.');
    if ok {
        Ok(())
    } else {
        Err("备份文件名无效".into())
    }
}

/// 创建服务器端备份文件（`Data/Backup/traffic-{时间}.zip`），返回文件信息。
pub fn create_backup(base: &Path) -> Result<Json, String> {
    let bytes = backup_zip(base)?;
    let dir = backup_dir(base);
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建备份目录失败：{e}"))?;
    let stamp = Local::now().format("%Y%m%d-%H%M%S").to_string();
    let mut name = format!("traffic-{stamp}.zip");
    let mut path = dir.join(&name);
    let mut n = 1;
    while path.exists() {
        n += 1;
        name = format!("traffic-{stamp}-{n}.zip");
        path = dir.join(&name);
    }
    std::fs::write(&path, &bytes).map_err(|e| format!("写入备份文件失败：{e}"))?;
    util::log_info(&format!(
        "Web 面板数据库备份已创建：{name}（{} 字节）",
        bytes.len()
    ));
    Ok(json!({ "name": name, "sizeBytes": bytes.len() }))
}

/// 服务器端备份文件列表（按创建时间倒序）。
pub fn list_backups(base: &Path) -> Json {
    let dir = backup_dir(base);
    let mut items: Vec<Json> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') || validate_backup_name(&name).is_err() {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            if !meta.is_file() {
                continue;
            }
            let created = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            items.push(json!({ "name": name, "sizeBytes": meta.len(), "created": created }));
        }
    }
    items.sort_by_key(|item| std::cmp::Reverse(item["created"].as_u64().unwrap_or(0)));
    json!({ "dir": dir.display().to_string(), "items": items })
}

/// 读取服务器端备份文件（下载）。
pub fn read_backup(base: &Path, name: &str) -> Result<Vec<u8>, String> {
    validate_backup_name(name)?;
    std::fs::read(backup_dir(base).join(name)).map_err(|e| format!("读取备份文件失败：{e}"))
}

/// 从服务器端备份文件还原（覆盖式；预校验不合格不动数据）。
pub fn restore_backup(base: &Path, name: &str) -> Result<Json, String> {
    let bytes = read_backup(base, name)?;
    let result = restore_zip(base, &bytes)?;
    util::log_info(&format!("Web 面板已从备份文件还原：{name}"));
    Ok(result)
}

/// 删除服务器端备份文件。
pub fn delete_backup(base: &Path, name: &str) -> Result<(), String> {
    validate_backup_name(name)?;
    std::fs::remove_file(backup_dir(base).join(name))
        .map_err(|e| format!("删除备份文件失败：{e}"))?;
    util::log_info(&format!("Web 面板已删除备份文件：{name}"));
    Ok(())
}

// ————— 路径与工具 —————

/// 旧版 JSON 归档目录：`{base}/Data/traffic`（仅用于首轮迁移）。
fn dir(base: &Path) -> PathBuf {
    base.join("Data").join("traffic")
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

/// `%Y-%m-%d` → 当日零点。
fn parse_day(date: &str) -> Option<NaiveDateTime> {
    NaiveDate::parse_from_str(date, "%Y-%m-%d")
        .ok()
        .and_then(|d| d.and_hms_opt(0, 0, 0))
}

/// 当日零点 → `%Y-%m-%d`。
fn format_day(dt: &NaiveDateTime) -> String {
    dt.date().format("%Y-%m-%d").to_string()
}

/// 是否为合法日期名（`%Y-%m-%d`）。
fn is_day_name(s: &str) -> bool {
    s.len() == 10 && NaiveDate::parse_from_str(s, "%Y-%m-%d").is_ok()
}

/// u64 → i64（SQLite 整数；溢出按上限截断）。
fn to_i64(value: u64) -> i64 {
    value.min(i64::MAX as u64) as i64
}

// ————— 旧版 JSON 迁移 —————

/// 旧版 JSON 日文件（第一代存储格式，仅迁移用）。
#[derive(Deserialize, Default)]
struct LegacyDay {
    #[serde(default)]
    date: String,
    #[serde(default)]
    web: LegacyWeb,
    #[serde(default)]
    ports: BTreeMap<String, LegacyCounters>,
}

/// 旧版网站段。
#[derive(Deserialize, Default)]
struct LegacyWeb {
    #[serde(default)]
    sites: BTreeMap<String, LegacySite>,
}

/// 旧版单站点汇总。
#[derive(Deserialize, Default)]
struct LegacySite {
    #[serde(default)]
    hits: u64,
    #[serde(default)]
    bytes: u64,
    #[serde(default)]
    uv: u64,
    #[serde(default)]
    s2xx: u64,
    #[serde(default)]
    s3xx: u64,
    #[serde(default)]
    s4xx: u64,
    #[serde(default)]
    s5xx: u64,
}

/// 旧版端口计数。
#[derive(Deserialize, Default)]
struct LegacyCounters {
    #[serde(default)]
    rx: u64,
    #[serde(default)]
    tx: u64,
}

/// 首轮迁移：库为空时导入 `Data/traffic/*.json`，成功导入的文件删除（迁移后以库为唯一来源）。
fn import_legacy_json(dal: &Dal, base: &Path) {
    // 仅在库为空（首次迁移）时执行
    let empty = (|| -> Result<bool, String> {
        let mut session = dal.open_session().map_err(|e| e.to_string())?;
        let mut count_all = |name: &str| -> Result<i64, String> {
            dal.table(name)
                .map_err(|e| e.to_string())?
                .count(session.as_mut(), None)
                .map_err(|e| e.to_string())
        };
        Ok(count_all(TABLE_WEB)? == 0 && count_all(TABLE_PORTS)? == 0)
    })()
    .unwrap_or(false);
    if !empty {
        return;
    }

    let Ok(entries) = std::fs::read_dir(dir(base)) else {
        return;
    };
    let mut files: Vec<PathBuf> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let Some(stem) = name.strip_suffix(".json") else {
            continue;
        };
        if is_day_name(stem) {
            files.push(entry.path());
        }
    }
    files.sort();

    let mut imported = 0usize;
    for path in files {
        let stem = path
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(day_file) = serde_json::from_str::<LegacyDay>(&text) else {
            util::log_error(&format!(
                "流量历史迁移：解析失败，保留原文件 {}",
                path.display()
            ));
            continue;
        };
        let date = if day_file.date.is_empty() {
            stem
        } else {
            day_file.date.clone()
        };
        let Some(day) = parse_day(&date) else {
            continue;
        };

        let mut ok = true;
        match dal.open_session() {
            Ok(mut session) => {
                for (site, s) in &day_file.web.sites {
                    let stats = SiteDay {
                        hits: s.hits,
                        bytes: s.bytes,
                        uv: s.uv,
                        s2xx: s.s2xx,
                        s3xx: s.s3xx,
                        s4xx: s.s4xx,
                        s5xx: s.s5xx,
                    };
                    if let Err(e) =
                        upsert_web_row(dal, session.as_mut(), &day, site, &stats)
                    {
                        util::log_error(&format!("流量历史迁移失败（{date} {site}）：{e}"));
                        ok = false;
                    }
                }
                for (key, c) in &day_file.ports {
                    let Some((proto, port)) = parse_port_key(key) else {
                        continue;
                    };
                    let counters = PortCounters { rx: c.rx, tx: c.tx };
                    if let Err(e) = upsert_port_row(
                        dal,
                        session.as_mut(),
                        &day,
                        proto,
                        port,
                        &counters,
                    ) {
                        util::log_error(&format!("流量历史迁移失败（{date} {key}）：{e}"));
                        ok = false;
                    }
                }
            }
            Err(e) => {
                util::log_error(&format!("流量历史迁移会话创建失败：{e}"));
                ok = false;
            }
        }
        if ok && std::fs::remove_file(&path).is_ok() {
            imported += 1;
        }
    }
    if imported > 0 {
        util::log_format(
            "流量历史迁移：导入 {} 个旧版日文件 → {}",
            &[&imported.to_string(), DB_FILE],
        );
    }
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

    fn cleanup(base: &Path) {
        drop_storage_for_test(base);
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn upserts_and_merges_web_snapshots() {
        let base = temp_dir("web");
        let mut first = BTreeMap::new();
        first.insert("a.com".to_string(), site(1, 100, 1));
        update_web(&base, "2026-10-01", &first);

        let mut second = BTreeMap::new();
        let mut a2 = site(2, 200, 2);
        a2.s4xx = 1;
        second.insert("a.com".to_string(), a2);
        second.insert("b.com".to_string(), site(5, 500, 3));
        update_web(&base, "2026-10-01", &second);

        let sites = day_web_sites(&base, "2026-10-01");
        assert_eq!(sites.len(), 2);
        assert_eq!(sites["a.com"].hits, 2, "同键应覆盖为最新快照");
        assert_eq!(sites["a.com"].s4xx, 1);
        assert_eq!(sites["b.com"].bytes, 500);

        // 幂等：重复写入不叠加
        update_web(&base, "2026-10-01", &second);
        let sites = day_web_sites(&base, "2026-10-01");
        assert_eq!(sites["a.com"].hits, 2);

        // 其它日期互不影响
        update_web(&base, "2026-10-02", &first);
        assert_eq!(day_web_sites(&base, "2026-10-01")["a.com"].hits, 2);
        assert_eq!(day_web_sites(&base, "2026-10-02")["a.com"].hits, 1);

        // 非法日期不落库
        update_web(&base, "not-a-day", &first);
        assert!(day_web_sites(&base, "not-a-day").is_empty());

        cleanup(&base);
    }

    #[test]
    fn upserts_port_snapshots_and_keeps_others() {
        let base = temp_dir("port");
        let mut first = BTreeMap::new();
        first.insert(port_key("tcp", 80), PortCounters { rx: 1, tx: 2 });
        update_ports(&base, "2026-10-01", &first);

        let mut second = BTreeMap::new();
        second.insert(port_key("tcp", 80), PortCounters { rx: 10, tx: 20 });
        second.insert(port_key("udp", 53), PortCounters { rx: 3, tx: 4 });
        update_ports(&base, "2026-10-01", &second);

        let ports = day_ports(&base, "2026-10-01");
        assert_eq!(ports["tcp:80"], PortCounters { rx: 10, tx: 20 });
        assert_eq!(ports["udp:53"], PortCounters { rx: 3, tx: 4 });

        cleanup(&base);
    }

    #[test]
    fn snapshot_limits_range_and_sorts() {
        let base = temp_dir("snap");
        for day in ["2026-09-28", "2026-09-29", "2026-09-30", "2026-10-01"] {
            let mut sites = BTreeMap::new();
            sites.insert("a.com".to_string(), site(1, 10, 1));
            update_web(&base, day, &sites);
        }

        let v = snapshot_json(&base, 3, 90);
        assert_eq!(v["retentionDays"], 90);
        let days = v["days"].as_array().unwrap();
        assert_eq!(days.len(), 3);
        assert_eq!(days[0]["date"], "2026-09-29");
        assert_eq!(days[2]["date"], "2026-10-01");
        assert_eq!(days[2]["web"]["hits"], 1);
        assert_eq!(days[2]["web"]["bytes"], 10);
        assert_eq!(days[2]["web"]["sites"]["a.com"]["uv"], 1);

        cleanup(&base);
    }

    #[test]
    fn prunes_expired_rows_only() {
        let base = temp_dir("prune");
        let today = today_string();
        let today_date = NaiveDate::parse_from_str(&today, "%Y-%m-%d").unwrap();
        let old = (today_date - Days::new(100)).format("%Y-%m-%d").to_string();
        let recent = (today_date - Days::new(5)).format("%Y-%m-%d").to_string();
        for day in [old.as_str(), recent.as_str(), today.as_str()] {
            let mut sites = BTreeMap::new();
            sites.insert("a.com".to_string(), site(1, 10, 1));
            update_web(&base, day, &sites);
        }

        // 重置清理守卫，确保本测试真实执行清理
        *LAST_PRUNE.lock().unwrap() = None;
        maybe_prune(&base, 90);
        assert!(
            day_web_sites(&base, &old).is_empty(),
            "超过 90 天的记录应被清理"
        );
        assert_eq!(day_web_sites(&base, &recent)["a.com"].hits, 1);
        assert_eq!(day_web_sites(&base, &today)["a.com"].hits, 1);

        // 0 = 不清理（新写入的远古日期保留）
        let ancient = (today_date - Days::new(200)).format("%Y-%m-%d").to_string();
        let mut sites = BTreeMap::new();
        sites.insert("a.com".to_string(), site(2, 20, 1));
        update_web(&base, &ancient, &sites);
        *LAST_PRUNE.lock().unwrap() = None;
        maybe_prune(&base, 0);
        assert_eq!(day_web_sites(&base, &ancient)["a.com"].hits, 2);

        cleanup(&base);
    }

    #[test]
    fn imports_legacy_json_files_once() {
        let base = temp_dir("import");
        let legacy_dir = dir(&base);
        std::fs::create_dir_all(&legacy_dir).unwrap();
        let text = r#"{
            "date": "2026-09-30",
            "web": { "sites": { "a.com": { "hits": 3, "bytes": 300, "uv": 2, "s2xx": 3 } } },
            "ports": { "tcp:80": { "rx": 10, "tx": 20 } }
        }"#;
        std::fs::write(legacy_dir.join("2026-09-30.json"), text).unwrap();
        std::fs::write(legacy_dir.join("notes.json"), "{}").unwrap();

        // 首次访问触发建库 + 导入
        let sites = day_web_sites(&base, "2026-09-30");
        assert_eq!(sites["a.com"].hits, 3);
        assert_eq!(sites["a.com"].uv, 2);
        let ports = day_ports(&base, "2026-09-30");
        assert_eq!(ports["tcp:80"], PortCounters { rx: 10, tx: 20 });
        assert!(!legacy_dir.join("2026-09-30.json").exists(), "导入后应删除旧文件");
        assert!(legacy_dir.join("notes.json").exists(), "非日文件不动");

        // 重新打开：库非空，不重复导入
        drop_storage_for_test(&base);
        let sites = day_web_sites(&base, "2026-09-30");
        assert_eq!(sites["a.com"].hits, 3, "重复打开不应双倍计数");

        cleanup(&base);
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

    #[test]
    fn readonly_sql_guard_rejects_writes() {
        assert!(validate_readonly_sql("SELECT 1").is_ok());
        assert!(validate_readonly_sql("  select * from t ").is_ok());
        for bad in [
            "DELETE FROM t",
            "UPDATE t SET a=1",
            "INSERT INTO t VALUES(1)",
            "DROP TABLE t",
            "ALTER TABLE t ADD COLUMN c",
            "PRAGMA journal_mode=WAL",
            "WITH x AS (SELECT 1) SELECT * FROM x",
            "SELECT 1; DROP TABLE t",
            "SELECT ';'",
            "SELECTX",
            "",
            "  ",
        ] {
            assert!(validate_readonly_sql(bad).is_err(), "应拒绝：{bad}");
        }
        assert!(validate_readonly_sql(&format!("SELECT {}", "1".repeat(5000))).is_err());
    }

    #[test]
    fn backup_restore_roundtrip() {
        let base = temp_dir("bakrestore");

        let mut ports = BTreeMap::new();
        ports.insert("tcp:80".to_string(), PortCounters { rx: 100, tx: 200 });
        update_ports(&base, "2026-10-01", &ports);
        let mut sites = BTreeMap::new();
        sites.insert("a.com".to_string(), site(7, 700, 3));
        update_web(&base, "2026-10-01", &sites);

        // 备份
        let zip_bytes = backup_zip(&base).expect("备份应成功");
        assert!(zip_bytes.starts_with(b"PK"), "应为 zip 包");

        // 备份后再修改数据（模拟后续变更）
        let mut ports2 = BTreeMap::new();
        ports2.insert("tcp:80".to_string(), PortCounters { rx: 1, tx: 1 });
        update_ports(&base, "2026-10-01", &ports2);
        assert_eq!(day_ports(&base, "2026-10-01")["tcp:80"], PortCounters { rx: 1, tx: 1 });

        // 还原 → 回到备份时点
        let result = restore_zip(&base, &zip_bytes).expect("还原应成功");
        assert_eq!(result["rows"][DB_TABLE_PORTS], 1);
        assert_eq!(result["rows"][DB_TABLE_WEB], 1);
        assert_eq!(
            day_ports(&base, "2026-10-01")["tcp:80"],
            PortCounters { rx: 100, tx: 200 }
        );
        assert_eq!(day_web_sites(&base, "2026-10-01")["a.com"].hits, 7);

        // 非法包被拒且现有数据不动
        let before = day_ports(&base, "2026-10-01");
        assert!(restore_zip(&base, b"not a zip").is_err());
        assert_eq!(day_ports(&base, "2026-10-01"), before);

        cleanup(&base);
    }

    #[test]
    fn backup_files_lifecycle() {
        let base = temp_dir("bakfiles");
        let mut ports = BTreeMap::new();
        ports.insert("tcp:80".to_string(), PortCounters { rx: 10, tx: 20 });
        update_ports(&base, "2026-10-02", &ports);

        // 创建（服务器端档案）
        let info = create_backup(&base).expect("创建备份应成功");
        let name = info["name"].as_str().unwrap().to_string();
        assert!(name.starts_with("traffic-") && name.ends_with(".zip"));
        assert!(info["sizeBytes"].as_u64().unwrap() > 0);

        // 列表
        let list = list_backups(&base);
        let items = list["items"].as_array().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["name"].as_str(), Some(name.as_str()));
        assert!(list["dir"].as_str().unwrap().ends_with("Backup"));

        // 名安全闸门（防路径穿越）
        let too_long = "x".repeat(200);
        for bad in [
            "../x.zip",
            "a/b.zip",
            "a\\b.zip",
            "x.txt",
            "",
            "..zip",
            "a..zip",
            too_long.as_str(),
        ] {
            assert!(validate_backup_name(bad).is_err(), "应拒绝：{bad}");
        }
        assert!(validate_backup_name("traffic-20261002-110524.zip").is_ok());

        // 读取（下载）
        let bytes = read_backup(&base, &name).unwrap();
        assert!(bytes.starts_with(b"PK"));

        // 从档案还原
        let result = restore_backup(&base, &name).expect("还原应成功");
        assert_eq!(result["rows"][DB_TABLE_PORTS], 1);

        // 删除
        delete_backup(&base, &name).unwrap();
        assert!(list_backups(&base)["items"].as_array().unwrap().is_empty());

        cleanup(&base);
    }

    /// 旧版备份包（仅两张流量表）还原时不得清空面板用户与审计表（自适应恢复）。
    #[test]
    fn restore_old_backup_keeps_user_tables() {
        use crate::audit;
        let base = temp_dir("bakold");

        // 建一个面板用户 + 一条审计记录
        audit::save_user(&base, "keepme", Some("pw"), &["dashboard".into()], true, "").unwrap();
        audit::record(
            &base,
            &audit::AuditEntry {
                user: "tester".to_string(),
                ip: "127.0.0.1".to_string(),
                action: "smoke".to_string(),
                title: "冒烟".to_string(),
                method: "POST".to_string(),
                path: "/x".to_string(),
                detail: String::new(),
                success: true,
                code: 0,
                message: String::new(),
                elapsed_ms: 1,
            },
        );

        // 构造“旧版备份包”：仅两张流量表（模拟升级前产出的备份）
        let store = storage(&base).unwrap();
        let tmp = base.join("Data").join(".old-format.zip");
        {
            let _guard = store.lock();
            store
                .dal()
                .backup_all(&[TABLE_WEB, TABLE_PORTS], &tmp, true)
                .unwrap();
        }
        let bytes = std::fs::read(&tmp).unwrap();
        std::fs::remove_file(&tmp).unwrap();

        // 还原旧包：只恢复包内两张表
        let result = restore_zip(&base, &bytes).expect("旧包还原应成功");
        assert!(result["rows"]["Agent_WebTrafficDaily"].is_number());
        assert!(
            result["rows"]["Agent_PanelUser"].is_null(),
            "旧包不含用户表，不应出现在恢复结果中：{result}"
        );

        // 面板用户未被清空，仍可登录校验通过
        let u = audit::find_user(&base, "keepme").unwrap().expect("用户应保留");
        assert!(u.verify("pw"));
        // 审计记录未被清空
        let logs = audit::query_logs(&base, 1, 10, "", "", None).unwrap();
        assert!(logs["total"].as_u64().unwrap() >= 1);

        cleanup(&base);
    }

    /// 并发首开单飞：多线程同时首次打开同一库，只允许一个执行建表，其余复用连接。
    #[test]
    fn concurrent_first_open_is_single_flight() {
        let base = temp_dir("openrace");
        let mut handles = Vec::new();
        for i in 0..8 {
            let dir = base.clone();
            handles.push(std::thread::spawn(move || {
                let opened = storage(&dir);
                assert!(opened.is_ok(), "线程 {i} 打开失败：{:?}", opened.err());
            }));
        }
        for handle in handles {
            handle.join().unwrap();
        }
        cleanup(&base);
    }
}
