//! 节点侧「多连接数据库注册表 + 只读查询」（通过 RAgent 管理数据库）。
//!
//! 背景（2026-11-12）：客户的库常**禁止外部直连**，只能由节点本机连过去查询。
//! 平台与部署客户端只认**连接名**，连接串与口令只留在节点本地：
//!
//! - 清单来源：`Config/Database.toml`（生态统一约定，由 `pek_rcode::database` 解析）；
//!   文件不存在时只有内置的 `traffic`（Agent 自身 SQLite）。
//! - 清单**不含口令/账号**：非 SQLite 连接只暴露 `host`（Server+Port）与库名，便于管理员辨认。
//! - 查询一律走 [`pek_rcode::db_admin::validate_readonly_sql`]（仅单条 `SELECT`）与 500 行上限；
//!   非默认连接用 [`pek_rcode::Dal::open`]（**不带模型、不建表**），
//!   SQLite 连接还要求文件已存在——绝不在客户的机器上凭空造出一个空库。
//! - 写通道（[`exec`]）另走 [`pek_rcode::db_admin::validate_sql`]（分级 + 禁账号/库级/服务级语句），
//!   且 `Config/Database.toml` 根级 `ReadOnlyConnections` 名单里的连接一律拒绝。

use std::path::Path;
use std::sync::{Arc, Mutex};

use std::collections::BTreeMap;

use serde_json::{Value as Json, json};

use crate::util;

/// 内置连接名（Agent 自身库；`Database.toml` 未配置时使用）。
pub const BUILTIN_CONN: &str = "traffic";

/// 已打开的外部连接（键含连接串：配置一改自然换新键，不会用旧连接串继续查）。
static OPENED: Mutex<BTreeMap<String, Arc<pek_rcode::Dal>>> = Mutex::new(BTreeMap::new());

/// 最多缓存多少个外部连接（面板来回切连接时不让驱动连接无限堆积）。
const MAX_CACHED: usize = 8;

/// 本机连接清单（**不含口令/账号**）。
///
/// 返回 `{ origin, default, items[{name, provider, label, note, isDefault, builtin,
/// sqliteFile, sizeBytes, host, database}] }`。
/// 清单只读文件元数据（不打开数据库），可以被平台按需高频拉取。
pub fn list_connections(base: &Path) -> Json {
    let mut items: Vec<Json> = Vec::new();
    let mut origin = "内置（Data/traffic.db）".to_string();
    let mut default_name = BUILTIN_CONN.to_string();
    let mut has_config = false;

    match pek_rcode::database::DatabaseFile::load(base) {
        Ok(Some(file)) => {
            has_config = true;
            origin = "Config/Database.toml".to_string();
            default_name = file.default_name().to_string();
            for name in file.names() {
                let Some(entry) = file.connections.get(name) else {
                    continue;
                };
                let raw = entry.connection_string();
                // 别名（MapTo=）自身没有 Provider：按链尾的真实连接识别方言/文件
                let resolved = file
                    .follow_map_to(name)
                    .map(|(_, c)| c)
                    .unwrap_or_else(|_| raw.to_string());
                items.push(connection_json(
                    base,
                    name,
                    raw,
                    &resolved,
                    entry.note(),
                    file.read_only(name),
                    name == default_name,
                    false,
                ));
            }
            if items.is_empty() {
                // 文件存在但一个连接都没配：不能返回空清单让人以为「节点没库」，
                // 也不能偷偷回退到内置库（默认连接已指向别处）
                origin = "Config/Database.toml（未配置任何连接）".to_string();
            }
        }
        Ok(None) => {}
        Err(e) => {
            // 解析失败必须说清楚：静默回退成内置库会让人以为「客户的库不见了」
            origin = format!("Config/Database.toml 读取失败：{e}");
        }
    }

    if !has_config {
        let file = base.join("Data").join(crate::history::DB_FILE);
        items.push(connection_json(
            base,
            BUILTIN_CONN,
            &format!("Data Source={};Provider=SQLite", file.display()),
            &format!("Data Source={};Provider=SQLite", file.display()),
            "Agent 自身数据库（流量历史/面板用户/操作审计）",
            false,
            true,
            true,
        ));
    }

    json!({
        "origin": origin,
        "default": default_name,
        "items": items,
    })
}

/// 单个连接条目的 JSON（口令/账号一律不出现）。
///
/// `raw` = 配置里的原始连接串（用于 `label`，别名会显示 `MapTo=xxx`）；
/// `resolved` = 链尾真实连接串（用于识别方言 / SQLite 文件 / 主机库名）。
fn connection_json(
    base: &Path,
    name: &str,
    raw: &str,
    resolved: &str,
    note: &str,
    read_only: bool,
    is_default: bool,
    builtin: bool,
) -> Json {
    let parsed = pek_rcode::dal::ConnectionString::parse(resolved);
    let raw_provider = parsed.provider().unwrap_or("").trim().to_string();
    // 方言名归一（vastbase/kingbase/highgo → VastBase/KingBase/HighGo；未知保留原样）
    let provider = pek_rcode::dialect::DatabaseKind::from_provider(&raw_provider)
        .map(|k| k.name().to_string())
        .unwrap_or_else(|_| {
            if raw_provider.is_empty() {
                "未知".to_string()
            } else {
                raw_provider.clone()
            }
        });
    let is_sqlite = provider.eq_ignore_ascii_case("sqlite");
    let sqlite_file = pek_rcode::database::sqlite_file_of(base, resolved);
    let size = sqlite_file
        .as_ref()
        .and_then(|p| std::fs::metadata(p).ok())
        .map(|m| m.len())
        .unwrap_or(0);
    let (host, database) = if is_sqlite {
        (String::new(), String::new())
    } else {
        endpoint_of(resolved)
    };
    json!({
        "name": name,
        "provider": provider,
        // 摘要标签（Provider=... / MapTo=...）；与平台既有面板同一口径
        "label": pek_rcode::database::connection_label(raw),
        "note": note,
        "isDefault": is_default,
        "builtin": builtin,
        // 只读连接（Config/Database.toml 根级 `ReadOnlyConnections` 名单）：写通道会拒绝
        "readOnly": read_only,
        "sqliteFile": sqlite_file
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
        "sizeBytes": size,
        "host": host,
        "database": database,
    })
}

/// 连接串里的（服务器[:端口], 库名）；**不解析账号口令**。
///
/// 键名兼容交由 `pek_rcode::dal::ConnectionString`（`server`/`port`/`database` 访问器），
/// 节点侧只负责「按需拼端口」：SQL Server 的 `Server=tcp:host,1433` 原样保留。
fn endpoint_of(conn: &str) -> (String, String) {
    let parsed = pek_rcode::dal::ConnectionString::parse(conn);
    let server = parsed.server().unwrap_or("").trim();
    let port = parsed.port().unwrap_or("").trim();
    let database = parsed.database().unwrap_or("").trim().to_string();
    let host = if server.is_empty() {
        String::new()
    } else if port.is_empty() || server.contains(':') {
        // 已带端口/协议前缀（`tcp:host,1433`、`host,1433`、IPv6）→ 原样
        server.to_string()
    } else {
        format!("{server}:{port}")
    };
    (host, database)
}

/// 在指定连接上执行只读查询（`conn` 为空 / 等于默认连接 → Agent 自身库）。
///
/// 安全边界：
/// - 只读：`validate_readonly_sql` 拦掉非 `SELECT` 与多语句；结果 500 行封顶；
/// - 不带模型：外部连接不会触发建表/改表（客户库只读，绝不能因为管理功能改结构）；
/// - SQLite 外部连接**要求文件已存在**，不自动创建空库。
pub fn query(base: &Path, conn: &str, sql: &str) -> Result<Json, String> {
    let name = conn.trim();
    let file = pek_rcode::database::DatabaseFile::load(base)?;
    let default_name = file.as_ref().map(|f| f.default_name().to_string());

    // 未指定 / 默认连接 / 无配置文件时的内置连接 → 走 Agent 自身库（带模型，只读）
    let is_builtin = name.is_empty()
        || (file.is_none() && name.eq_ignore_ascii_case(BUILTIN_CONN));
    let is_default = default_name
        .as_deref()
        .is_some_and(|d| d.eq_ignore_ascii_case(name));
    if is_builtin || is_default {
        let mut v = crate::history::query_readonly(base, sql)?;
        if let Some(obj) = v.as_object_mut() {
            obj.insert("conn".to_string(), json!(default_name.clone().unwrap_or_else(|| BUILTIN_CONN.to_string())));
            obj.insert("provider".to_string(), json!(default_provider(base)));
        }
        return Ok(v);
    }

    let Some(file) = file else {
        return Err(format!(
            "连接「{name}」不存在：本机没有 Config/Database.toml，只有内置连接 {BUILTIN_CONN}"
        ));
    };
    // 连接名大小写不敏感（与 DatabaseFile::lookup 同口径）
    let real_name = file
        .names()
        .into_iter()
        .find(|n| n.eq_ignore_ascii_case(name))
        .map(str::to_string)
        .ok_or_else(|| {
            format!(
                "连接「{name}」不存在（可用：{}）",
                file.names().join(", ")
            )
        })?;
    let conn_str = file.resolve_connection(base, &real_name)?;

    if let Some(path) = pek_rcode::database::sqlite_file_of(base, &conn_str)
        && !path.is_file()
    {
        return Err(format!(
            "连接「{real_name}」的 SQLite 数据文件不存在：{}（管理查询不会自动创建空库，请先核对路径）",
            path.display()
        ));
    }

    let dal = open_cached(base, &real_name, &conn_str)?;
    let mut session = dal
        .open_session()
        .map_err(|e| format!("连接「{real_name}」打开会话失败：{e}"))?;
    let mut v = pek_rcode::db_admin::query_readonly_on(session.as_mut(), sql)
        .map_err(|e| format!("连接「{real_name}」查询失败：{e}"))?;
    if let Some(obj) = v.as_object_mut() {
        obj.insert("conn".to_string(), json!(real_name));
        obj.insert("provider".to_string(), json!(dal.kind().name()));
    }
    util::log_format(
        "管理查询完成：连接={}，行数={}",
        &[
            &real_name,
            &v.get("rowCount")
                .and_then(|x| x.as_u64())
                .unwrap_or(0)
                .to_string(),
        ],
    );
    Ok(v)
}

/// 在指定连接上执行**写语句**（P3；安全闸门见 [`pek_rcode::db_admin::validate_write_sql`]）。
///
/// 额外约束：
/// - `Config/Database.toml` 根级 `ReadOnlyConnections` 名单里的连接一律拒绝（生产核心库只许看不到写）；
/// - 外部连接不带模型、不建表；影响行数原样回传，平台/客户端据此提示；
/// - 默认连接（Agent 自身库）允许写，但仅供管理员修正数据，写前请确认备份。
pub fn exec(base: &Path, conn: &str, sql: &str) -> Result<Json, String> {
    pek_rcode::db_admin::validate_write_sql(sql)?;
    let name = conn.trim();
    let file = pek_rcode::database::DatabaseFile::load(base)?;
    let default_name = file.as_ref().map(|f| f.default_name().to_string());
    let is_builtin = name.is_empty() || (file.is_none() && name.eq_ignore_ascii_case(BUILTIN_CONN));
    let is_default = default_name
        .as_deref()
        .is_some_and(|d| d.eq_ignore_ascii_case(name));
    // 默认连接也可能在 Database.toml 里被标为只读（生产核心库只许看不到写）
    let default_read_only = match (&file, &default_name) {
        (Some(f), Some(d)) => f.read_only(d),
        _ => false,
    };

    if is_builtin || is_default {
        if default_read_only {
            return Err(format!(
                "连接「{}」被标记为只读（Config/Database.toml 的 ReadOnlyConnections），拒绝写操作",
                default_name.clone().unwrap_or_else(|| BUILTIN_CONN.to_string())
            ));
        }
        let store = crate::history::storage(base)?;
        let mut session = store
            .dal()
            .open_session()
            .map_err(|e| format!("打开默认连接会话失败：{e}"))?;
        let mut v = pek_rcode::db_admin::exec_write_on(session.as_mut(), sql)
            .map_err(|e| format!("默认连接写操作失败：{e}"))?;
        if let Some(obj) = v.as_object_mut() {
            obj.insert(
                "conn".to_string(),
                json!(default_name.unwrap_or_else(|| BUILTIN_CONN.to_string())),
            );
        }
        util::log_format(
            "管理写操作完成：连接=(默认)，影响行数={}",
            &[
                &v.get("affected")
                    .and_then(|x| x.as_u64())
                    .unwrap_or(0)
                    .to_string(),
            ],
        );
        return Ok(v);
    }

    let Some(file) = file else {
        return Err(format!(
            "连接「{name}」不存在：本机没有 Config/Database.toml，只有内置连接 {BUILTIN_CONN}"
        ));
    };
    let real_name = file
        .names()
        .into_iter()
        .find(|n| n.eq_ignore_ascii_case(name))
        .map(str::to_string)
        .ok_or_else(|| {
            format!(
                "连接「{name}」不存在（可用：{}）",
                file.names().join(", ")
            )
        })?;
    if file.read_only(&real_name) {
        return Err(format!(
            "连接「{real_name}」被标记为只读（Config/Database.toml 的 ReadOnlyConnections 名单），拒绝写操作；如需写入请运维人员先修改该配置"
        ));
    }
    let conn_str = file.resolve_connection(base, &real_name)?;
    if let Some(path) = pek_rcode::database::sqlite_file_of(base, &conn_str)
        && !path.is_file()
    {
        return Err(format!(
            "连接「{real_name}」的 SQLite 数据文件不存在：{}（写通道不会自动创建空库，请先核对路径）",
            path.display()
        ));
    }
    let dal = open_cached(base, &real_name, &conn_str)?;
    let mut session = dal
        .open_session()
        .map_err(|e| format!("连接「{real_name}」打开会话失败：{e}"))?;
    let mut v = pek_rcode::db_admin::exec_write_on(session.as_mut(), sql)
        .map_err(|e| format!("连接「{real_name}」写操作失败：{e}"))?;
    if let Some(obj) = v.as_object_mut() {
        obj.insert("conn".to_string(), json!(real_name));
        obj.insert("provider".to_string(), json!(dal.kind().name()));
    }
    util::log_format(
        "管理写操作完成：连接={}，影响行数={}",
        &[
            &real_name,
            &v.get("affected")
                .and_then(|x| x.as_u64())
                .unwrap_or(0)
                .to_string(),
        ],
    );
    Ok(v)
}

/// SQL 摘要（审计/日志用）：实现已下沉 `pek_rcode::db_admin::redact_sql`。
pub use pek_rcode::db_admin::redact_sql;

/// 写操作审计（平台中转与直连共用）。
///
/// 直连（`/db/apply`）与平台 WS 下发都不经过面板 `guarded`，必须在这里补记，
/// 否则「谁改了库」没有台账。查询不记（读操作量大且无破坏性，面板侧仍按既有规则记 `dbQuery`）。
#[allow(clippy::too_many_arguments)]
pub fn record_audit(
    base: &Path,
    user: &str,
    ip: &str,
    action: &str,
    path: &str,
    conn: &str,
    op_label: &str,
    sql: &str,
    ok: bool,
    message: &str,
) {
    crate::audit::record(
        base,
        &crate::audit::AuditEntry {
            category: None,
            user: if user.trim().is_empty() { "unknown".to_string() } else { user.trim().to_string() },
            ip: if ip.trim().is_empty() { "unknown".to_string() } else { ip.trim().to_string() },
            action: action.to_string(),
            title: format!(
                "{}：{}",
                op_label,
                if conn.trim().is_empty() { "默认连接" } else { conn.trim() }
            ),
            method: "POST".to_string(),
            path: path.to_string(),
            detail: redact_sql(sql, 200),
            success: ok,
            code: if ok { 0 } else { 400 },
            message: message.to_string(),
            elapsed_ms: 0,
        },
    );
}

/// 默认连接的 Provider 名（自身库查询回执里带上，便于界面显示）。
fn default_provider(base: &Path) -> String {
    crate::history::database_info(base)
        .get("provider")
        .and_then(|v| v.as_str())
        .unwrap_or("SQLite")
        .to_string()
}

/// 取（或首次打开）某外部连接的 `Dal`。
///
/// 键含连接串：`Config/Database.toml` 改过连接串后自然换新键，旧 Dal 不再被命中。
fn open_cached(base: &Path, name: &str, conn: &str) -> Result<Arc<pek_rcode::Dal>, String> {
    let key = format!("{}\u{1}{}\u{1}{}", base.display(), name, conn);
    {
        let guard = OPENED.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(dal) = guard.get(&key) {
            return Ok(dal.clone());
        }
    }
    let dal = pek_rcode::Dal::open(conn).map_err(|e| format!("连接「{name}」打开失败：{e}"))?;
    // 探活一次：驱动是惰性连接的，不先探活会变成「清单能列、每次查询都报错」，
    // 让人误以为是权限问题
    dal.open_session()
        .map_err(|e| format!("连接「{name}」不可用：{e}"))?;
    let dal = Arc::new(dal);
    let mut guard = OPENED.lock().unwrap_or_else(|e| e.into_inner());
    if guard.len() >= MAX_CACHED
        && let Some(first) = guard.keys().next().cloned()
    {
        guard.remove(&first);
    }
    guard.insert(key, dal.clone());
    Ok(dal)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pek-ragent-dbconn-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join("Config")).unwrap();
        std::fs::create_dir_all(dir.join("Data")).unwrap();
        dir
    }

    /// 建一个带数据的 SQLite 文件（外部连接用例）。
    fn make_sqlite(path: &Path) {
        let conn = format!("Data Source={};Provider=SQLite", path.display());
        let dal = pek_rcode::Dal::open(&conn).unwrap();
        let mut session = dal.open_session().unwrap();
        session
            .execute("CREATE TABLE t_demo (Id INTEGER PRIMARY KEY, Name TEXT)", &[])
            .unwrap();
        session
            .execute("INSERT INTO t_demo (Id, Name) VALUES (1, 'alpha')", &[])
            .unwrap();
    }

    #[test]
    fn lists_builtin_when_no_config() {
        let dir = temp_dir("builtin");
        let v = list_connections(&dir);
        assert_eq!(v["default"].as_str(), Some(BUILTIN_CONN));
        let items = v["items"].as_array().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["name"].as_str(), Some(BUILTIN_CONN));
        assert_eq!(items[0]["builtin"].as_bool(), Some(true));
        assert_eq!(items[0]["provider"].as_str(), Some("SQLite"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lists_connections_without_credentials() {
        let dir = temp_dir("redact");
        std::fs::write(
            dir.join("Config").join("Database.toml"),
            r#"Default = "main"
[Connections.main]
ConnectionString = 'Data Source=Data/main.db;Provider=SQLite'
Note = "本机主库"
[Connections.byd]
ConnectionString = 'Server=10.1.2.3;Port=3306;Database=byd;Uid=root;Pwd=SECRET-PWD;Provider=MySql'
Note = "BYD 主库（仅本机可连）"
[Connections.vb]
ConnectionString = 'Server=10.2.3.4;Port=5432;Database=mes;Uid=sa;Pwd=SECRET-VB;provider=vastbase;Search Path=tenant1, public'
Note = "VastBase 主库"
"#,
        )
        .unwrap();
        let v = list_connections(&dir);
        assert_eq!(v["default"].as_str(), Some("main"));
        let text = serde_json::to_string(&v).unwrap();
        // 口令/账号绝不出现在清单里
        assert!(!text.contains("SECRET-PWD"), "清单泄露了口令：{text}");
        assert!(!text.contains("SECRET-VB"), "清单泄露了口令：{text}");
        assert!(!text.contains("Uid=root"), "清单泄露了账号：{text}");
        let items = v["items"].as_array().unwrap();
        assert_eq!(items.len(), 3);
        let byd = items.iter().find(|i| i["name"] == "byd").unwrap();
        assert_eq!(byd["provider"].as_str(), Some("MySql"));
        assert_eq!(byd["host"].as_str(), Some("10.1.2.3:3306"));
        assert_eq!(byd["database"].as_str(), Some("byd"));
        assert_eq!(byd["isDefault"].as_bool(), Some(false));
        // VastBase（海量）：provider 归一成方言名，主机+库名可见
        let vb = items.iter().find(|i| i["name"] == "vb").unwrap();
        assert_eq!(vb["provider"].as_str(), Some("VastBase"));
        assert_eq!(vb["host"].as_str(), Some("10.2.3.4:5432"));
        assert_eq!(vb["database"].as_str(), Some("mes"));
        let main = items.iter().find(|i| i["name"] == "main").unwrap();
        assert_eq!(main["isDefault"].as_bool(), Some(true));
        assert_eq!(main["host"].as_str(), Some(""), "SQLite 不应出现 host");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn queries_extra_connection_readonly() {
        let dir = temp_dir("query");
        let extra = dir.join("Data").join("extra.db");
        make_sqlite(&extra);
        std::fs::write(
            dir.join("Config").join("Database.toml"),
            r#"Default = "main"
[Connections.main]
ConnectionString = 'Data Source=Data/main.db;Provider=SQLite'
[Connections.extra]
ConnectionString = 'Data Source=Data/extra.db;Provider=SQLite'
"#,
        )
        .unwrap();
        // 外部连接：只读查询可用
        let v = query(&dir, "extra", "SELECT Name FROM t_demo").unwrap();
        assert_eq!(v["columns"][0].as_str(), Some("Name"));
        assert_eq!(v["rows"][0][0].as_str(), Some("alpha"));
        assert_eq!(v["conn"].as_str(), Some("extra"));
        // 写操作被安全闸门拦掉（并给出明确原因）
        let err = query(&dir, "extra", "DELETE FROM t_demo").unwrap_err();
        assert!(err.contains("SELECT"), "应提示仅允许 SELECT：{err}");
        // 不存在的连接要说清楚
        let err = query(&dir, "nope", "SELECT 1").unwrap_err();
        assert!(err.contains("不存在"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn execs_write_and_honors_readonly() {
        let dir = temp_dir("exec");
        let extra = dir.join("Data").join("extra.db");
        make_sqlite(&extra);
        std::fs::write(
            dir.join("Config").join("Database.toml"),
            r#"Default = "main"
ReadOnlyConnections = ["locked"]
[Connections.main]
ConnectionString = 'Data Source=Data/main.db;Provider=SQLite'
[Connections.extra]
ConnectionString = 'Data Source=Data/extra.db;Provider=SQLite'
[Connections.locked]
ConnectionString = 'Data Source=Data/extra.db;Provider=SQLite'
"#,
        )
        .unwrap();
        // 可写连接：写入生效（影响 1 行），随后能查到
        let v = exec(&dir, "extra", "INSERT INTO t_demo (Id, Name) VALUES (2, 'beta')").unwrap();
        assert_eq!(v["affected"].as_u64(), Some(1));
        let q = query(&dir, "extra", "SELECT Name FROM t_demo WHERE Id=2").unwrap();
        assert_eq!(q["rows"][0][0].as_str(), Some("beta"));
        // 只读连接：明确拒绝，且不执行
        let err = exec(&dir, "locked", "DELETE FROM t_demo").unwrap_err();
        assert!(err.contains("只读"), "{err}");
        // 表级 DDL 在节点侧放行（权限分级在平台按 op 判定）：返回级别标记
        let v = exec(&dir, "extra", "DROP TABLE t_demo").unwrap();
        assert_eq!(v["op"].as_str(), Some("ddl"));
        assert_eq!(v["opLabel"].as_str(), Some("结构变更"));
        // 账号/权限类语句永远拒绝（不属库表操作）
        let err = exec(&dir, "extra", "GRANT ALL ON t_demo TO x").unwrap_err();
        assert!(err.contains("GRANT"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn refuses_missing_sqlite_file() {
        let dir = temp_dir("missing");
        std::fs::write(
            dir.join("Config").join("Database.toml"),
            r#"Default = "main"
[Connections.main]
ConnectionString = 'Data Source=Data/main.db;Provider=SQLite'
[Connections.ghost]
ConnectionString = 'Data Source=Data/ghost.db;Provider=SQLite'
"#,
        )
        .unwrap();
        let err = query(&dir, "ghost", "SELECT 1").unwrap_err();
        assert!(err.contains("不存在"), "{err}");
        // 不允许因为一次查询就在客户目录里造出空库
        assert!(!dir.join("Data").join("ghost.db").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
