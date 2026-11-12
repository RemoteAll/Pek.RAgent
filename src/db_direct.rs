//! 直连数据库：客户端 → 节点，查询/写操作**不经平台中转**。
//!
//! 与直连部署（[`crate::deploy_http`]）同一套安全模型：
//! 1. 平台签发短时效票据（`dhrust::random::hex(24)`），经 WS `dbTicket` 落到**本节点内存**，
//!    同时回给客户端；票据**绑定连接名与允许的语句级别**（`view/write/delete/ddl`）；
//! 2. 客户端带票据请求 `POST /db/apply?ticket=…`（JSON `{conn, sql}`）；
//! 3. 本模块核销票据（10 分钟 TTL、最多 500 次请求）、校验语句级别与连接绑定，
//!    再交给 [`crate::dbconn`]（那里仍有只读校验 / 分级校验 / `ReadOnlyConnections` 保护）。
//!
//! 因此：没有票据的人即使能连上本端口也查不到任何库；票据不能换库、不能越级、过期即废。

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use dhrust::net::http::{HttpOutcome, HttpResponse};
use dhrust::net::router::Ctx;
use serde_json::json;

use crate::manager::AppManager;

/// 票据有效期：拿到就连，10 分钟足够，过期即废。
const TICKET_TTL: Duration = Duration::from_secs(600);
/// 同时挂起的票据上限（防平台异常时刷爆内存）。
const MAX_TICKETS: usize = 64;
/// 单张票据最多请求次数（一次会话够用；防止被当成长期凭据）。
const MAX_REQUESTS: u32 = 500;

/// 直连数据库票据（平台经 WS `dbTicket` 下发）。
#[derive(Clone, Debug)]
pub struct DbTicket {
    /// 绑定的连接名（空 = 节点默认连接）
    pub conn: String,
    /// 允许的语句级别：`view` / `write` / `delete` / `ddl`
    pub ops: Vec<String>,
    /// 平台用户（审计用）
    pub user: String,
    /// 签发时刻（TTL 判定）
    issued: Instant,
    /// 剩余请求次数
    remaining: u32,
}

impl DbTicket {
    /// 构造（`issued` / 次数由本模块维护）。
    pub fn new(conn: impl Into<String>, ops: Vec<String>, user: impl Into<String>) -> Self {
        Self {
            conn: conn.into(),
            ops,
            user: user.into(),
            issued: Instant::now(),
            remaining: MAX_REQUESTS,
        }
    }
}

fn table() -> &'static Mutex<HashMap<String, DbTicket>> {
    static T: OnceLock<Mutex<HashMap<String, DbTicket>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 票据 id 只能是最多 64 位 hex（防注入/超长键）。
fn valid_ticket_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_hexdigit())
}

/// 登记票据；返回 `false` = 票据格式非法 / 表已满。
pub fn issue(id: &str, ticket: DbTicket) -> bool {
    if !valid_ticket_id(id) {
        return false;
    }
    let mut m = table().lock().unwrap_or_else(|e| e.into_inner());
    m.retain(|_, t| t.issued.elapsed() < TICKET_TTL);
    if m.len() >= MAX_TICKETS {
        return false;
    }
    m.insert(id.to_ascii_lowercase(), ticket);
    true
}

/// 核销一次请求：校验票据、连接绑定与语句级别，成功则扣一次配额。
fn take(id: &str, op: &str, conn: &str) -> Result<DbTicket, String> {
    if !valid_ticket_id(id) {
        return Err("数据库票据格式无效".to_string());
    }
    let key = id.to_ascii_lowercase();
    let mut m = table().lock().unwrap_or_else(|e| e.into_inner());
    m.retain(|_, t| t.issued.elapsed() < TICKET_TTL);
    let Some(t) = m.get_mut(&key) else {
        return Err("数据库票据无效或已过期（请重新申请）".to_string());
    };
    if t.issued.elapsed() >= TICKET_TTL {
        m.remove(&key);
        return Err("数据库票据已过期（请重新申请）".to_string());
    }
    if t.remaining == 0 {
        m.remove(&key);
        return Err("数据库票据请求次数已用尽（请重新申请）".to_string());
    }
    // 票据绑定的连接：不允许换库（防「拿 A 库的票据查 B 库」）
    let bound = t.conn.trim();
    let req = conn.trim();
    if bound.is_empty() {
        // 空 = 只授权「默认连接」；若允许任意连接，签给默认库的票据就能读所有库
        if !req.is_empty() {
            return Err(format!(
                "该票据只授权默认连接，不能用于「{req}」"
            ));
        }
    } else if !bound.eq_ignore_ascii_case(req) {
        return Err(format!(
            "该票据只授权连接「{bound}」，不能用于「{req}」"
        ));
    }
    if !t.ops.iter().any(|o| o.eq_ignore_ascii_case(op)) {
        return Err(format!(
            "该票据未授权「{op}」级操作（允许：{}）",
            t.ops.join(" / ")
        ));
    }
    t.remaining -= 1;
    let out = t.clone();
    if t.remaining == 0 {
        m.remove(&key);
    }
    Ok(out)
}

fn json_error(status: u16, message: &str) -> HttpOutcome {
    HttpOutcome::Response(HttpResponse::json(
        status,
        json!({ "ok": false, "message": message }).to_string(),
    ))
}

fn json_ok(message: &str, data: &serde_json::Value) -> HttpOutcome {
    HttpOutcome::Response(HttpResponse::json(
        200,
        json!({ "ok": true, "message": message, "data": data }).to_string(),
    ))
}

/// `POST /db/apply?ticket=…`：JSON `{conn?, sql}`；查询与写共用（按语句分级自动分流）。
pub fn apply(manager: Arc<AppManager>, ctx: Ctx) -> HttpOutcome {
    let ticket_id = ctx.query_value("ticket").unwrap_or_default();
    if ticket_id.is_empty() {
        return json_error(401, "缺少数据库票据（ticket）");
    }
    let body: serde_json::Value = match serde_json::from_slice(ctx.req.body.as_ref()) {
        Ok(v) => v,
        Err(e) => return json_error(400, &format!("请求体不是有效 JSON：{e}")),
    };
    let sql = body
        .get("sql")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let conn = body
        .get("conn")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    // 语句分级：与平台侧同一口径（服务端仍会再校验一次）
    let op = match pek_rcode::db_admin::validate_sql(&sql) {
        Ok(op) => op,
        Err(e) => return json_error(400, &e),
    };
    let ticket = match take(&ticket_id, op.key(), &conn) {
        Ok(t) => t,
        Err(e) => return json_error(401, &e),
    };
    let base = manager.base();
    let result = if op.is_read() {
        crate::dbconn::query(base, &conn, &sql)
    } else {
        crate::dbconn::exec(base, &conn, &sql)
    };
    let ip = crate::webpanel::effective_client_ip(manager.config().trust_forwarded_for, &ctx).0;
    match result {
        Ok(data) => {
            crate::util::log_format(
                "直连数据库操作：用户={}，连接={}，类型={}，语句长度={}",
                &[
                    if ticket.user.trim().is_empty() { "(未知)" } else { &ticket.user },
                    if conn.is_empty() { "(默认)" } else { &conn },
                    op.label(),
                    &sql.chars().count().to_string(),
                ],
            );
            if !op.is_read() {
                crate::dbconn::record_audit(
                    base,
                    &ticket.user,
                    &ip,
                    "dbDirect",
                    "/db/apply",
                    &conn,
                    op.label(),
                    &sql,
                    true,
                    "",
                );
            }
            json_ok("已执行", &data)
        }
        Err(e) => {
            if !op.is_read() {
                crate::dbconn::record_audit(
                    base,
                    &ticket.user,
                    &ip,
                    "dbDirect",
                    "/db/apply",
                    &conn,
                    op.label(),
                    &sql,
                    false,
                    &e,
                );
            }
            json_error(400, &e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ticket_binds_conn_and_ops_and_expires_by_quota() {
        let id = "0123456789abcdef";
        assert!(issue(
            id,
            DbTicket::new("main", vec!["view".into()], "op1")
        ));
        // 级别越权
        let err = take(id, "write", "main").unwrap_err();
        assert!(err.contains("未授权"), "{err}");
        // 换库（具名票据不能换到别的连接）
        let err = take(id, "view", "other").unwrap_err();
        assert!(err.contains("只授权连接"), "{err}");
        // 正常核销
        assert!(take(id, "view", "MAIN").is_ok());
        // 未知票据
        assert!(take("ffffffffffffffff", "view", "main").is_err());
        // 默认连接票据：只允许默认连接（空 conn），不能指定具体连接
        let id2 = "0011223344556677";
        assert!(issue(id2, DbTicket::new("", vec!["view".into()], "op3")));
        assert!(take(id2, "view", "").is_ok());
        let err = take(id2, "view", "main").unwrap_err();
        assert!(err.contains("只授权默认连接"), "{err}");
    }

    #[test]
    fn quota_exhausts_and_removes_ticket() {
        let id = "abcdef0123456789";
        let mut t = DbTicket::new("", vec!["view".into()], "op2");
        t.remaining = 2;
        assert!(issue(id, t));
        assert!(take(id, "view", "").is_ok());
        assert!(take(id, "view", "").is_ok());
        // 用尽即删
        assert!(take(id, "view", "").is_err());
    }
}
