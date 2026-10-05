//! 面板用户与操作审计（SQLite：与流量历史同库 `Data/traffic.db`，模型见 `Entity/Model.xml`）。
//!
//! - **多用户**：`Agent_PanelUser` 表（用户名 / 密码哈希+盐 / 菜单权限 / 启用 / 备注）；
//!   内置管理员 `admin`（配置文件 `WebUserName`/`WebPassword`，超级权限）不落本表；
//! - **审计**：`Agent_OperationLog` 表记录登录与全部变更类操作（含失败），
//!   参数摘要脱敏（password 等字段替换为 `***`），供面板「操作日志」页分页查看；
//! - 数据访问复用 [`crate::history::with_store`]（同库同连接、建表单飞、写锁串行）。
//!
//! 密码存储：`SHA-256(salt:password)` hex（每用户随机盐，不可逆；对齐“凭据不明文落盘”）。

use std::path::Path;

use chrono::{Local, NaiveDateTime};
use pek_rcode::{Query, Where};
use serde_json::{json, Value as Json};

use crate::history;
use crate::util;

/// 用户表（实体名，见 `Entity/Model.xml`）。
pub(crate) const TABLE_USER: &str = "PanelUser";
/// 操作日志表。
pub(crate) const TABLE_OPLOG: &str = "OperationLog";

/// 全部可选菜单权限（key 与前端导航 `data-panel` 一致；顺序即展示顺序）。
/// `users`（用户管理）不在授予范围：仅内置管理员可访问。
pub(crate) const ALL_PERMISSIONS: &[(&str, &str)] = &[
    ("dashboard", "状态"),
    ("services", "子服务"),
    ("traffic", "流量"),
    ("control", "控制"),
    ("config", "配置"),
    ("starconfig", "星尘设置"),
    ("logs", "日志"),
    ("watchdog", "看门狗"),
    ("database", "数据库"),
    ("fileman", "文件管理"),
    ("cleanup", "日志清理"),
    ("plugins", "插件"),
    ("ai", "AI 助手"),
    ("audit", "操作日志"),
];

/// 规范化权限列表（去重、去空白、过滤未知项，保持 ALL_PERMISSIONS 顺序）。
pub(crate) fn normalize_permissions(list: &[String]) -> Vec<String> {
    ALL_PERMISSIONS
        .iter()
        .map(|(k, _)| k.to_string())
        .filter(|k| list.iter().any(|p| p.trim() == k))
        .collect()
}

// ————— 密码 —————

/// 生成随机盐（32 位十六进制）。
fn new_salt() -> String {
    dhrust::random::hex(16)
}

/// 计算密码哈希：`SHA-256(salt:password)` hex。
fn hash_password(salt: &str, password: &str) -> String {
    dhrust::sign::sha256_hex(format!("{salt}:{password}").as_bytes())
}

// ————— 用户读取 —————

/// 面板用户（不含哈希细节的对外视图；登录校验用内部结构）。
#[derive(Clone, Debug)]
pub(crate) struct PanelUser {
    /// 编号
    pub id: i64,
    /// 用户名
    pub user_name: String,
    /// 密码哈希（hex）
    pub password_hash: String,
    /// 随机盐
    pub salt: String,
    /// 菜单权限 key 列表
    pub permissions: Vec<String>,
    /// 是否启用
    pub enabled: bool,
    /// 备注
    pub remark: String,
}

impl PanelUser {
    /// 用户 JSON 视图（面板用；不含密码字段）。
    pub fn to_json(&self) -> Json {
        json!({
            "id": self.id,
            "userName": self.user_name,
            "permissions": self.permissions,
            "enabled": self.enabled,
            "remark": self.remark,
        })
    }

    /// 校验密码。
    pub fn verify(&self, password: &str) -> bool {
        !self.password_hash.is_empty()
            && self.password_hash == hash_password(&self.salt, password)
    }
}

/// 行 → 用户。
fn row_to_user(row: &pek_rcode::DbRow) -> Option<PanelUser> {
    let user_name = row.get_by_name("UserName")?.as_str()?.to_string();
    let text = |name: &str| {
        row.get_by_name(name)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    };
    Some(PanelUser {
        id: row
            .get_by_name("Id")
            .and_then(|v| v.as_i64())
            .unwrap_or_default(),
        user_name,
        password_hash: text("PasswordHash"),
        salt: text("Salt"),
        permissions: text("Permissions")
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
        enabled: row
            .get_by_name("Enabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        remark: text("Remark"),
    })
}

/// 按用户名查找（大小写不敏感：与内置 admin 登录判定一致）。
pub(crate) fn find_user(base: &Path, user_name: &str) -> Result<Option<PanelUser>, String> {
    let name = user_name.trim().to_string();
    if name.is_empty() {
        return Ok(None);
    }
    let all = list_raw(base)?;
    Ok(all.into_iter().find(|u| u.user_name.eq_ignore_ascii_case(&name)))
}

/// 列出全部用户（内部结构）。
fn list_raw(base: &Path) -> Result<Vec<PanelUser>, String> {
    history::with_store(base, |dal, session| {
        let table = dal.table(TABLE_USER)?;
        let rows = table.query(
            session,
            &Query::new().order_by("Id", false),
        )?;
        Ok(rows.rows.iter().filter_map(row_to_user).collect())
    })
}

/// 列出全部用户（JSON 视图，含权限中文名）。
pub(crate) fn list_users_json(base: &Path) -> Result<Json, String> {
    let users = list_raw(base)?;
    let items: Vec<Json> = users
        .iter()
        .map(|u| {
            let mut view = u.to_json();
            let names: Vec<String> = u
                .permissions
                .iter()
                .map(|k| {
                    ALL_PERMISSIONS
                        .iter()
                        .find(|(key, _)| key == k)
                        .map(|(_, name)| name.to_string())
                        .unwrap_or_else(|| k.clone())
                })
                .collect();
            if let Some(obj) = view.as_object_mut() {
                obj.insert("permissionNames".to_string(), json!(names));
            }
            view
        })
        .collect();
    Ok(json!({
        "users": items,
        "permissions": ALL_PERMISSIONS
            .iter()
            .map(|(k, n)| json!({ "key": k, "name": n }))
            .collect::<Vec<_>>(),
    }))
}

/// 登录校验：返回启用且密码正确的用户。
pub(crate) fn verify_login(
    base: &Path,
    user_name: &str,
    password: &str,
) -> Result<Option<PanelUser>, String> {
    let Some(user) = find_user(base, user_name)? else {
        return Ok(None);
    };
    if !user.enabled {
        return Ok(None);
    }
    if user.verify(password) {
        Ok(Some(user))
    } else {
        Ok(None)
    }
}

// ————— 用户写入 —————

/// 保存用户（不存在则创建，要求 `password` 非空；存在则更新，`password` 为空表示不改）。
pub(crate) fn save_user(
    base: &Path,
    user_name: &str,
    password: Option<&str>,
    permissions: &[String],
    enabled: bool,
    remark: &str,
) -> Result<(), String> {
    let name = user_name.trim();
    if name.is_empty() || name.len() > 50 {
        return Err("用户名不能为空且不超过 50 字符".to_string());
    }
    if name.contains(',') || name.contains(char::is_whitespace) {
        return Err("用户名不能包含逗号或空白字符".to_string());
    }
    let perms = normalize_permissions(permissions);
    let password = password.unwrap_or("").trim().to_string();

    let existing = find_user(base, name)?;
    match existing {
        Some(user) => {
            history::with_store(base, |dal, session| {
                let table = dal.table(TABLE_USER)?;
                let mut fields: Vec<(&str, pek_rcode::DbValue)> = vec![
                    ("Permissions", perms.join(",").into()),
                    ("Enabled", enabled.into()),
                    ("Remark", remark.trim().into()),
                ];
                if !password.is_empty() {
                    let salt = new_salt();
                    let hash = hash_password(&salt, &password);
                    fields.push(("Salt", salt.into()));
                    fields.push(("PasswordHash", hash.into()));
                }
                table.update_by_pk(session, &fields, &[user.id.into()])?;
                Ok(())
            })
        }
        None => {
            if password.is_empty() {
                return Err("新用户必须设置密码".to_string());
            }
            let salt = new_salt();
            let hash = hash_password(&salt, &password);
            history::with_store(base, |dal, session| {
                let table = dal.table(TABLE_USER)?;
                table.insert(
                    session,
                    &[
                        ("UserName", name.into()),
                        ("PasswordHash", hash.as_str().into()),
                        ("Salt", salt.as_str().into()),
                        ("Permissions", perms.join(",").as_str().into()),
                        ("Enabled", enabled.into()),
                        ("Remark", remark.trim().into()),
                    ],
                )?;
                Ok(())
            })
        }
    }
}

/// 修改用户密码（校验旧密码；供用户自助改密）。
pub(crate) fn change_user_password(
    base: &Path,
    user_name: &str,
    old_password: &str,
    new_password: &str,
) -> Result<(), String> {
    if new_password.is_empty() {
        return Err("新密码不能为空".to_string());
    }
    let Some(user) = find_user(base, user_name)? else {
        return Err("用户不存在".to_string());
    };
    if !user.verify(old_password) {
        return Err("旧密码不正确".to_string());
    }
    let salt = new_salt();
    let hash = hash_password(&salt, new_password);
    history::with_store(base, |dal, session| {
        let table = dal.table(TABLE_USER)?;
        table.update_by_pk(
            session,
            &[("Salt", salt.as_str().into()), ("PasswordHash", hash.as_str().into())],
            &[user.id.into()],
        )?;
        Ok(())
    })
}

/// 删除用户。
pub(crate) fn delete_user(base: &Path, user_name: &str) -> Result<(), String> {
    let Some(user) = find_user(base, user_name)? else {
        return Err("用户不存在".to_string());
    };
    history::with_store(base, |dal, session| {
        let table = dal.table(TABLE_USER)?;
        table.delete_by_pk(session, &[user.id.into()])?;
        Ok(())
    })
}

// ————— 操作审计 —————

/// 审计记录（写入用）。
pub(crate) struct AuditEntry {
    /// 操作者
    pub user: String,
    /// 来源 IP
    pub ip: String,
    /// 动作（接口名）
    pub action: String,
    /// 动作中文名
    pub title: String,
    /// HTTP 方法
    pub method: String,
    /// 请求路径
    pub path: String,
    /// 参数摘要（已脱敏）
    pub detail: String,
    /// 是否成功
    pub success: bool,
    /// 结果码
    pub code: i32,
    /// 结果消息
    pub message: String,
    /// 耗时毫秒
    pub elapsed_ms: i64,
}

/// 写入一条审计记录（失败只记程序日志，不影响业务）。
pub(crate) fn record(base: &Path, entry: &AuditEntry) {
    let time: NaiveDateTime = Local::now().naive_local();
    let result = history::with_store(base, |dal, session| {
        let table = dal.table(TABLE_OPLOG)?;
        table.insert(
            session,
            &[
                ("LogTime", time.into()),
                ("UserName", entry.user.as_str().into()),
                ("Ip", entry.ip.as_str().into()),
                ("Action", entry.action.as_str().into()),
                ("Title", entry.title.as_str().into()),
                ("Method", entry.method.as_str().into()),
                ("Path", entry.path.as_str().into()),
                ("Detail", entry.detail.as_str().into()),
                ("Success", entry.success.into()),
                ("Code", entry.code.into()),
                ("Message", entry.message.as_str().into()),
                ("ElapsedMs", entry.elapsed_ms.into()),
            ],
        )?;
        Ok(())
    });
    if let Err(e) = result {
        util::log_error(&format!("操作日志写入失败：{e}"));
    }
}

/// 分页查询操作日志（按时间倒序）。
///
/// - `user`：按操作者精确过滤（空 = 全部）；
/// - `keyword`：对 动作/中文名/路径/消息/详情 做模糊匹配（不区分大小写）；
/// - `success`：`Some(true/false)` 过滤成功/失败。
///
/// 实现：SQL 侧先按 `user`/`success` 过滤（受 `MAX_SCAN` 上限保护），关键词命中与分页
/// 在内存完成——面板操作日志量级（万级以内）足够；超出上限时以 `truncated` 标记提示。
pub(crate) fn query_logs(
    base: &Path,
    page: usize,
    size: usize,
    user: &str,
    keyword: &str,
    success: Option<bool>,
) -> Result<Json, String> {
    /// 单次扫描上限（防御；面板操作频率低，万级内足够）。
    const MAX_SCAN: usize = 20_000;

    let page = page.max(1);
    let size = size.clamp(1, 200);
    let mut filter: Option<Where> = None;
    if !user.trim().is_empty() {
        let mut w = Where::new().eq("UserName", user.trim());
        if let Some(ok) = success {
            w = w.eq("Success", ok);
        }
        filter = Some(w);
    } else if let Some(ok) = success {
        filter = Some(Where::new().eq("Success", ok));
    }
    let keyword = keyword.trim().to_lowercase();

    history::with_store(base, |dal, session| {
        let table = dal.table(TABLE_OPLOG)?;
        let mut query = Query::new()
            .order_by("LogTime", true)
            .order_by("Id", true)
            .take(MAX_SCAN + 1);
        if let Some(f) = filter {
            query = query.filter(f);
        }
        let rows = table.query(session, &query)?;
        let truncated = rows.len() > MAX_SCAN;

        let mut items: Vec<Json> = Vec::new();
        for row in rows.rows.iter().take(MAX_SCAN) {
            let text = |name: &str| {
                row.get_by_name(name)
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string()
            };
            let time = row
                .get_by_name("LogTime")
                .and_then(|v| v.as_datetime())
                .map(|t| t.format("%Y-%m-%d %H:%M:%S").to_string())
                .unwrap_or_default();
            let action = text("Action");
            let title = text("Title");
            let path = text("Path");
            let message = text("Message");
            let detail = text("Detail");
            let user_name = text("UserName");
            if !keyword.is_empty() {
                let haystack =
                    format!("{action} {title} {path} {message} {user_name} {detail}").to_lowercase();
                if !haystack.contains(&keyword) {
                    continue;
                }
            }
            items.push(json!({
                "id": row.get_by_name("Id").and_then(|v| v.as_i64()).unwrap_or_default(),
                "time": time,
                "userName": user_name,
                "ip": text("Ip"),
                "action": action,
                "title": title,
                "method": text("Method"),
                "path": path,
                "detail": detail,
                "success": row.get_by_name("Success").and_then(|v| v.as_bool()).unwrap_or(false),
                "code": row.get_by_name("Code").and_then(|v| v.as_i64()).unwrap_or_default(),
                "message": message,
                "elapsedMs": row.get_by_name("ElapsedMs").and_then(|v| v.as_i64()).unwrap_or_default(),
            }));
        }

        let total = items.len();
        let start = page.saturating_sub(1) * size;
        let page_items: Vec<Json> = items.into_iter().skip(start).take(size).collect();
        Ok(json!({
            "total": total,
            "page": page,
            "size": size,
            "truncated": truncated,
            "items": page_items,
        }))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_base(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "ragent-audit-{}-{}-{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    fn cleanup(base: &Path) {
        history::drop_storage_for_test(base);
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn password_hash_is_salted_and_stable() {
        let salt = new_salt();
        let hash = hash_password(&salt, "secret");
        assert_eq!(hash.len(), 64);
        assert_eq!(hash, hash_password(&salt, "secret"));
        assert_ne!(hash, hash_password(&salt, "other"));
        assert_ne!(hash, hash_password("other-salt", "secret"));
        assert_ne!(hash, new_salt());
    }

    #[test]
    fn user_crud_and_login_flow() {
        let base = temp_base("users");
        // 创建（权限自动归一化排序：dashboard 在 fileman 前）
        save_user(
            &base,
            "alice",
            Some("pw123"),
            &["fileman".into(), "dashboard".into()],
            true,
            "测试用户",
        )
        .unwrap();
        let u = find_user(&base, "ALICE").unwrap().unwrap();
        assert!(u.enabled);
        assert_eq!(u.permissions, vec!["dashboard".to_string(), "fileman".to_string()]);
        assert!(u.verify("pw123"));

        // 登录
        assert!(verify_login(&base, "alice", "pw123").unwrap().is_some());
        assert!(verify_login(&base, "alice", "bad").unwrap().is_none());

        // 更新（不改密码）：权限/启用/备注更新
        save_user(&base, "alice", None, &["logs".into()], false, "停用").unwrap();
        let u = find_user(&base, "alice").unwrap().unwrap();
        assert!(!u.enabled);
        assert!(u.verify("pw123"), "未提供密码时密码不变");
        assert_eq!(u.permissions, vec!["logs".to_string()]);
        assert!(verify_login(&base, "alice", "pw123").unwrap().is_none(), "禁用后不能登录");

        // 重命名不存在 → 新增需密码
        assert!(save_user(&base, "bob", None, &[], true, "").is_err());

        // 改密码
        change_user_password(&base, "alice", "pw123", "pw456").unwrap();
        assert!(change_user_password(&base, "alice", "wrong", "x").is_err());
        save_user(&base, "alice", None, &["logs".into()], true, "").unwrap();
        assert!(verify_login(&base, "alice", "pw456").unwrap().is_some());

        // 删除
        delete_user(&base, "alice").unwrap();
        assert!(find_user(&base, "alice").unwrap().is_none());
        assert!(delete_user(&base, "alice").is_err());

        cleanup(&base);
    }

    #[test]
    fn audit_records_and_queries_with_filters() {
        let base = temp_base("oplog");
        let entry = |i: usize, user: &str, ok: bool| AuditEntry {
            user: user.to_string(),
            ip: "127.0.0.1".to_string(),
            action: format!("fileDelete"),
            title: "删除文件".to_string(),
            method: "POST".to_string(),
            path: format!("/star/fileDelete"),
            detail: format!("paths=item-{i}"),
            success: ok,
            code: if ok { 0 } else { 400 },
            message: if ok { "已删除".to_string() } else { "失败".to_string() },
            elapsed_ms: 5,
        };
        for i in 0..5 {
            record(&base, &entry(i, "alice", true));
        }
        record(&base, &entry(9, "bob", false));

        // 全部：6 条
        let all = query_logs(&base, 1, 4, "", "", None).unwrap();
        assert_eq!(all["total"], 6);
        assert_eq!(all["items"].as_array().unwrap().len(), 4);
        let page2 = query_logs(&base, 2, 4, "", "", None).unwrap();
        assert_eq!(page2["items"].as_array().unwrap().len(), 2);

        // 用户过滤
        let alice = query_logs(&base, 1, 50, "alice", "", None).unwrap();
        assert_eq!(alice["total"], 5);

        // 成功/失败过滤
        let failed = query_logs(&base, 1, 50, "", "", Some(false)).unwrap();
        assert_eq!(failed["total"], 1);
        assert_eq!(failed["items"][0]["userName"], "bob");

        // 关键词（命中 detail）
        let kw = query_logs(&base, 1, 50, "", "item-3", None).unwrap();
        assert_eq!(kw["total"], 1);

        // 关键词（命中中文标题）
        let kw2 = query_logs(&base, 1, 50, "", "删除文件", None).unwrap();
        assert_eq!(kw2["total"], 6);

        cleanup(&base);
    }

    #[test]
    fn permissions_normalized_filters_unknown_keys() {
        let list: Vec<String> = vec![
            "fileman".into(),
            " unknown ".into(),
            "dashboard".into(),
            "fileman".into(),
        ];
        assert_eq!(
            normalize_permissions(&list),
            vec!["dashboard".to_string(), "fileman".to_string()]
        );
        assert!(normalize_permissions(&["nope".to_string()]).is_empty());
    }
}
