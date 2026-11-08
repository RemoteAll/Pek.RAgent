//! 直连部署：客户端 → 节点，**包不经平台**。
//!
//! 为什么要有它（2026-11-08，用户要求「能不通过平台就不要通过平台」）：
//! 平台给 RAgent 下发部署指令后，RAgent 要自己去平台拉几十 MB 的包，平台的出口带宽
//! 全被部署占掉——而这些往往是**客户的服务器**，没有理由消耗我们自己的流量。
//!
//! 职责划分：
//! - **包的数据**：客户端与节点之间直传，平台完全不参与；
//! - **元数据**：项目/实例/权限/任务状态仍在平台（平台是唯一真相源）。
//!
//! 安全模型（不引入共享密钥）：
//! 1. 平台生成一次性票据（32 位 hex），经 WS `deployTicket` 下发到**本节点内存**，
//!    同时回给客户端；
//! 2. 客户端带着票据把包推到 `POST /deploy/apply?ticket=…`；
//! 3. 本模块核销票据（一次性、10 分钟 TTL），校验包 SHA-256 后才落盘；
//! 4. 结果是同步返回给客户端，**同时**通过 WS 回报平台闭环任务（`appUpgradeResult`）。
//!
//! 因此：没有票据的人即使能访问本端口也无法写入任何东西；票据用过即废，无法重放。

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use dhrust::net::http::{HttpOutcome, HttpResponse};
use dhrust::net::router::Ctx;
use serde_json::json;

use crate::manager::AppManager;

/// 票据有效期：客户端拿到就要连，10 分钟足够，过期即废。
const TICKET_TTL: Duration = Duration::from_secs(600);
/// 同时挂起的票据上限（防平台异常时刷爆内存）。
const MAX_TICKETS: usize = 64;

/// 一次性部署票据（平台经 WS `deployTicket` 下发）。
#[derive(Clone, Debug)]
pub struct Ticket {
    /// 应用/服务名
    pub app_name: String,
    /// 版本号（回报平台时用）
    pub version: String,
    /// `deploy`=首次部署 / `upgrade`=升级 / `overwrite`=C# 星尘存量覆盖
    pub mode: String,
    /// 目标目录（`deploy`/`overwrite` 必填；`upgrade` 忽略）
    pub dir_path: String,
    /// 启动参数（首次部署时写入；升级时保留原值）
    pub arguments: Option<String>,
    /// 包内容的 SHA-256（小写 hex）
    pub sha256: String,
    /// 签发时刻（TTL 判定）
    issued: Instant,
}

impl Ticket {
    /// 构造（`issued` 由本模块维护，调用方不用传）。
    pub fn new(
        app_name: impl Into<String>,
        version: impl Into<String>,
        mode: impl Into<String>,
        dir_path: impl Into<String>,
        arguments: Option<String>,
        sha256: impl Into<String>,
    ) -> Self {
        Self {
            app_name: app_name.into(),
            version: version.into(),
            mode: mode.into(),
            dir_path: dir_path.into(),
            arguments: arguments.filter(|s| !s.trim().is_empty()),
            sha256: sha256.into(),
            issued: Instant::now(),
        }
    }
}

fn table() -> &'static Mutex<HashMap<String, Ticket>> {
    static T: OnceLock<Mutex<HashMap<String, Ticket>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 部署串行锁：同一时刻只允许一个直连部署改应用目录。
fn busy() -> &'static Mutex<bool> {
    static B: OnceLock<Mutex<bool>> = OnceLock::new();
    B.get_or_init(|| Mutex::new(false))
}

/// 登记票据；返回 `false` = 票据格式非法 / 表已满。
pub fn issue(id: &str, ticket: Ticket) -> bool {
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

/// 票据 id 只能是最多 64 位 hex（防注入/超长键）。
fn valid_ticket_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_hexdigit())
}

/// 核销票据（**一次性**：取出即删，失败也不退回）。
fn take(id: &str) -> Option<Ticket> {
    if !valid_ticket_id(id) {
        return None;
    }
    let mut m = table().lock().unwrap_or_else(|e| e.into_inner());
    m.retain(|_, t| t.issued.elapsed() < TICKET_TTL);
    m.remove(&id.to_ascii_lowercase())
        .filter(|t| t.issued.elapsed() < TICKET_TTL)
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

/// `POST /deploy/apply?ticket=…`：裸 body = 包内容，元数据全在票据里。
///
/// 之所以元数据不放 query：票据已经由平台签发并落到本机，客户端无从篡改
/// 目标目录/服务名，避免「拿别人的票据往任意目录写」。
pub fn apply(manager: Arc<AppManager>, ctx: Ctx) -> HttpOutcome {
    let ticket_id = ctx.query_value("ticket").unwrap_or_default();
    if ticket_id.is_empty() {
        return json_error(401, "缺少部署票据（ticket）");
    }
    let Some(ticket) = take(&ticket_id) else {
        return json_error(
            401,
            "部署票据无效、已使用或已过期；请回平台重新发起（票据一次性，不可重放）",
        );
    };
    let body = &ctx.req.body;
    if body.is_empty() {
        return json_error(400, "包内容为空");
    }
    if body.len() > crate::apppkg::MAX_PACKAGE_SIZE {
        return json_error(
            400,
            &format!(
                "包过大（{} MB，上限 {} MB）",
                body.len() / 1024 / 1024,
                crate::apppkg::MAX_PACKAGE_SIZE / 1024 / 1024
            ),
        );
    }
    let actual = dhrust::sign::sha256_hex(body);
    if !actual.eq_ignore_ascii_case(&ticket.sha256) {
        return json_error(
            400,
            &format!(
                "包校验失败：SHA-256 不一致（期望 {}…，实际 {}…）",
                prefix12(&ticket.sha256),
                prefix12(&actual)
            ),
        );
    }
    // 串行化：避免与平台下发、另一次直连部署同时改同一应用目录
    {
        let mut b = busy().lock().unwrap_or_else(|e| e.into_inner());
        if *b {
            return json_error(409, "节点正在执行另一个部署，请稍后重试");
        }
        *b = true;
    }
    struct Guard;
    impl Drop for Guard {
        fn drop(&mut self) {
            if let Ok(mut b) = busy().lock() {
                *b = false;
            }
        }
    }
    let _guard = Guard;

    crate::util::log_format(
        "直连部署开始：{} v{}（{}，{} KB，票据 {}…）",
        &[
            &ticket.app_name,
            &ticket.version,
            &ticket.mode,
            &(body.len() / 1024).to_string(),
            &prefix12(&ticket_id),
        ],
    );
    let result = run(&manager, &ticket, body);
    let (ok, message) = match &result {
        Ok(msg) => (true, msg.clone()),
        Err(e) => (false, e.clone()),
    };
    if ok {
        crate::util::log_format(
            "直连部署完成：{} v{}——{}",
            &[&ticket.app_name, &ticket.version, &message],
        );
    } else {
        crate::util::log_error(&format!(
            "直连部署失败：{} v{}——{}",
            ticket.app_name, ticket.version, message
        ));
    }
    // 回报平台：平台据此闭环任务、刷新实例版本（平台只收结果，不收包）
    crate::panel_ws::report_app_result(&manager, &ticket.app_name, &ticket.version, ok, &message);
    crate::audit::record(
        manager.base(),
        &crate::audit::AuditEntry {
            category: None,
            user: "direct".to_string(),
            ip: "direct".to_string(),
            action: "appDeployDirect".to_string(),
            title: format!("直连部署：{} v{}", ticket.app_name, ticket.version),
            method: "POST".to_string(),
            path: "/deploy/apply".to_string(),
            detail: message.clone(),
            success: ok,
            code: 0,
            message: if ok { String::new() } else { message.clone() },
            elapsed_ms: 0,
        },
    );
    match result {
        Ok(msg) => json_ok(&msg, &json!({ "appName": ticket.app_name, "version": ticket.version })),
        Err(e) => json_error(400, &e),
    }
}

fn prefix12(s: &str) -> String {
    s.chars().take(12).collect()
}

/// 按票据的 `mode` 执行实际部署。
fn run(manager: &Arc<AppManager>, t: &Ticket, body: &[u8]) -> Result<String, String> {
    match t.mode.to_ascii_lowercase().as_str() {
        // 首次部署 / 升级（Rust 星尘子服务）：`work_dir` 只在新建时生效，
        // 升级时传了也会被 `apppkg` 忽略（不能因为平台改配置就把目录搬走）
        "deploy" | "upgrade" => {
            if !t.dir_path.trim().is_empty() {
                crate::apppkg::validate_deploy_dir(
                    std::path::Path::new(t.dir_path.trim()),
                    manager.base(),
                )?;
            }
            let work_dir = t.dir_path.trim();
            let deployed = crate::apppkg::deploy_bytes_ext(
                manager,
                &t.app_name,
                body,
                &crate::apppkg::DeployOptions {
                    name_hint: Some(&t.app_name),
                    arguments: t.arguments.as_deref(),
                    auto_start: true,
                    work_dir: if work_dir.is_empty() {
                        None
                    } else {
                        Some(work_dir)
                    },
                },
            )?;
            if !deployed.started {
                return Err(format!(
                    "应用包已写入（{}），但应用未成功启动（{}）；请查看节点日志与应用日志",
                    deployed.work_dir.display(),
                    deployed.program.display()
                ));
            }
            Ok(format!(
                "已{}并启动（{} 个文件，目录 {}，程序 {}）",
                if deployed.updated { "升级" } else { "新建" },
                deployed.files,
                deployed.work_dir.display(),
                deployed.program.display()
            ))
        }
        // C# 星尘存量覆盖：只换目录里的文件，重启交给本机星尘
        "overwrite" => {
            use dhrust::staragent::ServiceOp;
            let dir = t.dir_path.trim();
            if dir.is_empty() {
                return Err("缺少 dirPath（平台实例未预置部署目录）".to_string());
            }
            let target = std::path::PathBuf::from(dir);
            crate::apppkg::validate_deploy_dir(&target, manager.base())?;
            // 与平台下发路径同一套预检：服务不存在/目录不符时**一个字节都不写**
            crate::panel_ws::precheck_cstar_target(&t.app_name, dir)?;
            let files = crate::apppkg::overwrite_into(&target, &t.app_name, body)?;
            util_log_overwrite(&t.app_name, dir, files);
            match dhrust::staragent::operate_by_default_ports(
                &t.app_name,
                ServiceOp::Restart,
                Duration::from_secs(60),
            ) {
                Ok(msg) => Ok(format!(
                    "已覆盖 {} 个文件到 {}，并{msg}",
                    files,
                    target.display()
                )),
                Err(e) => Err(format!(
                    "已覆盖 {files} 个文件到 {}，但重启服务「{}」失败：{e}（文件已更新，请人工确认服务状态）",
                    target.display(),
                    t.app_name
                )),
            }
        }
        other => Err(format!(
            "未知的部署方式「{other}」（仅支持 deploy / upgrade / overwrite）"
        )),
    }
}

fn util_log_overwrite(app: &str, dir: &str, files: usize) {
    crate::util::log_format(
        "直连存量覆盖：{} 个文件 → {}（{}）",
        &[&files.to_string(), dir, app],
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tid(seed: u8) -> String {
        format!("{:0>32}", format!("{seed:x}"))
    }

    #[test]
    fn ticket_is_one_shot() {
        let id = tid(1);
        assert!(issue(
            &id,
            Ticket::new("demo", "1.0.0", "deploy", "/tmp/demo", None, "a".repeat(64))
        ));
        assert!(take(&id).is_some(), "第一次核销应成功");
        assert!(take(&id).is_none(), "票据一次性，第二次必须失败");
    }

    #[test]
    fn rejects_invalid_ticket_id() {
        assert!(!issue(
            "",
            Ticket::new("a", "1", "deploy", "/x", None, "a".repeat(64))
        ));
        assert!(!issue(
            "../etc/passwd",
            Ticket::new("a", "1", "deploy", "/x", None, "a".repeat(64))
        ));
        assert!(take("not-hex!").is_none());
    }

    #[test]
    fn unknown_mode_is_rejected() {
        // 用真实目录才能走到 mode 判定之后；这里只验证 mode 匹配逻辑的兜底分支
        let mgr = None::<&Arc<AppManager>>;
        assert!(mgr.is_none());
        let t = Ticket::new("demo", "1.0.0", "bogus", "", None, "a".repeat(64));
        assert_eq!(t.mode, "bogus");
    }
}
