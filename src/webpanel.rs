//! Web 管理面板：登录鉴权、服务状态与控制、配置管理、日志查看、本机信息。
//!
//! **对齐 C# 契约**（StarAgent / DH.NAgent 面板；前端 `index.html` 直接复用）：
//! - `/api/*`：login / logout / status / control / freeMemory / configMetadata /
//!   updateConfig / changePassword / logs / logFiles / health / watchdog / syncTime
//! - `/star/*`：services / startService / stopService / restartService / addService /
//!   removeService / getStarConfig / updateStarConfig / machine / webTraffic / portTraffic /
//!   trafficHistory / getProcessList / dbInfo / dbQuery / dbBackup / dbRestore /
//!   dbListBackups / dbCreateBackup / dbDownloadBackup / dbRestoreBackup / dbDeleteBackup /
//!   fileList / fileRead / fileWrite / fileMkdir / fileNewFile / fileDelete / fileRename /
//!   fileMove / fileCopy / fileUpload / fileDownload / fileCompress / fileExtract /
//!   fileChmod / fileSearch（文件管理）/ logCleanScan / logCleanRun（日志清理）
//! - 统一 JSON 信封 `{code, message?, data?}`；Bearer Token 鉴权（`Authorization` 头）
//!
//! 鉴权级别由 `WebAuthLevel` 控制（对齐 C# `ParseAuthLevel`）：`None` 全部放行 /
//! `LocalOnly`（默认）本机回环地址免鉴权、远程需令牌 / `Full` 一律需令牌。
//!
//! 登录爆破防护按客户端 IP 计数（5 次失败封禁 5 分钟，窗口 15 分钟），与 C# 面板一致；
//! 实现已下沉 `dhrust::net::login_guard`（与 HlkProductTool 面板共用）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, Local};
use dhrust::net::controller::{
    arg, arg_i64, json_body, json_error, json_result, ActionResult, Controller,
};
use dhrust::net::http::HttpResponse;
use dhrust::net::login_guard::LoginGuard;
use dhrust::net::panel_auth::{bearer_token, client_ip, is_loopback, AuthLevel, TokenStore};
use dhrust::net::router::Ctx;
use dhrust::web::{format_bytes, format_speed};
use pek_radmin::panel::{action_name_of, truncate_text};
use serde_json::{json, Value as Json};

use crate::agent;
use crate::audit;
use crate::config::AgentConfig;
use crate::manager::AppManager;
use crate::sampler;
use crate::sys;
use crate::util;

/// 请求主体（已认证身份）：内置管理员或数据库面板用户。
#[derive(Clone, Debug)]
pub(crate) struct Principal {
    /// 登录名（审计记录用；`None` 鉴权级别下远程请求记为 `anon`）
    pub name: String,
    /// 是否内置管理员（全部权限 + 用户管理）
    pub is_admin: bool,
    /// 菜单权限 key 列表（内置管理员为空 = 全部）
    pub perms: Vec<String>,
}

impl Principal {
    /// 是否拥有某菜单权限（管理员恒真）。
    pub fn allowed(&self, perm: &str) -> bool {
        self.is_admin || self.perms.iter().any(|p| p == perm)
    }
}

/// 面板共享状态。
pub struct WebPanel {
    /// 应用管理器
    pub(crate) manager: Arc<AppManager>,
    /// 程序基础目录
    base: PathBuf,
    /// 面板端口
    port: u16,
    /// 进程启动时刻（运行时长）
    started: Instant,
    /// 进程启动墙钟
    started_at: DateTime<Local>,
    /// 令牌表（令牌 → 会话主体；实现下沉 `dhrust::net::panel_auth::TokenStore`，默认 24 小时）
    tokens: TokenStore<Principal>,
    /// 登录限流（默认 15 分钟 5 次 → 封禁 5 分钟；实现下沉 `dhrust::net::login_guard`）
    logins: LoginGuard,
}

impl WebPanel {
    /// 创建面板。
    pub fn new(manager: Arc<AppManager>, base: &Path, port: u16) -> Arc<WebPanel> {
        Arc::new(WebPanel {
            manager,
            base: base.to_path_buf(),
            port,
            started: Instant::now(),
            started_at: Local::now(),
            tokens: TokenStore::new(),
            logins: LoginGuard::new(),
        })
    }

    /// 面板端口。
    pub fn port(&self) -> u16 {
        self.port
    }

    /// 程序基础目录（文件管理等模块使用）。
    pub(crate) fn base(&self) -> &Path {
        &self.base
    }

    /// 当前配置（热重载后为最新值；日志清理等模块使用）。
    pub(crate) fn config(&self) -> AgentConfig {
        self.manager.config()
    }

    /// 更新配置（保存并触发热生效；插件相关接口复用）。
    pub(crate) fn update_config(&self, f: impl FnOnce(&mut AgentConfig)) {
        self.manager.update_config(f);
    }

    /// 进程运行时长。
    pub(crate) fn uptime(&self) -> Duration {
        self.started.elapsed()
    }

    // ————— 令牌与会话 —————

    /// 校验凭据并返回主体（不发放令牌；登录动作与测试辅助共用）。
    ///
    /// 登录源（按顺序）：
    /// 1. 配置文件内置管理员（`WebUserName`/`WebPassword`）——超级权限；
    /// 2. 数据库面板用户（`Agent_PanelUser`）——按菜单权限受限，禁用用户拒绝登录。
    fn authenticate(&self, user: &str, password: &str) -> Option<Principal> {
        if user.is_empty() || password.is_empty() {
            return None;
        }
        let cfg = self.manager.config();

        // 内置管理员
        if !cfg.web_user_name.trim().is_empty()
            && !cfg.web_user_password.is_empty()
            && user.trim().eq_ignore_ascii_case(cfg.web_user_name.trim())
            && password == cfg.web_user_password
        {
            return Some(Principal {
                name: cfg.web_user_name.trim().to_string(),
                is_admin: true,
                perms: Vec::new(),
            });
        }

        // 数据库用户（数据库不可用时仅内置管理员可登录）
        match audit::verify_login(&self.base, user, password) {
            Ok(Some(u)) => Some(Principal {
                name: u.user_name.clone(),
                is_admin: false,
                perms: u.permissions.clone(),
            }),
            Ok(None) => None,
            Err(e) => {
                util::log_error(&format!("登录时读取面板用户失败：{e}"));
                None
            }
        }
    }

    /// 签发令牌并绑定会话（校验用户名密码；失败返回 None）。
    ///
    /// 生产路径登录动作见 [`login`]（先 [`WebPanel::authenticate`] 取主体、再发令牌，
    /// 以便审计记录登录名）；本方法供测试等场景一步获取令牌。
    #[cfg(test)]
    fn issue_token(&self, user: &str, password: &str) -> Option<String> {
        let principal = self.authenticate(user, password)?;
        Some(self.tokens.issue_with(principal))
    }

    /// 校验令牌。
    fn validate_token(&self, token: &str) -> bool {
        self.tokens.validate(token)
    }

    /// 吊销令牌（绑定主体随令牌一并失效）。
    fn revoke_token(&self, token: &str) {
        self.tokens.revoke(token);
    }

    /// 重命名当前令牌绑定的会话主体（内置管理员改名后立即生效，无需重新登录）。
    fn rename_session(&self, ctx: &Ctx, new_name: &str) {
        if let Some(token) = bearer_token(ctx) {
            self.tokens.update(&token, |p| p.name = new_name.to_string());
        }
    }

    // ————— 鉴权辅助 —————

    /// 请求鉴权（`/api/login` 与 `/api/logout` 除外）。
    ///
    /// 级别由 `WebAuthLevel` 决定（对齐 C# `AgentWebPanel` 语义，修改后自动生效）：
    /// - `None`：全部放行（不鉴权；本机回环记内置管理员，远程审计记为 `anon`）；
    /// - `LocalOnly`（默认）：本机回环地址免鉴权（视为内置管理员），其余需有效令牌；
    /// - `Full`：一律校验令牌。
    pub(crate) fn check_auth(&self, ctx: &Ctx) -> bool {
        self.principal(ctx).is_some()
    }

    /// 解析请求主体（已认证身份；`None` = 未认证）。
    ///
    /// Bearer 令牌有效 → 会话绑定的用户与权限；无令牌时按鉴权级别兜底
    /// （详见 [`WebPanel::check_auth`]）。
    pub(crate) fn principal(&self, ctx: &Ctx) -> Option<Principal> {
        if let Some(token) = bearer_token(ctx) {
            if let Some(p) = self.tokens.get(&token) {
                return Some(p);
            }
        }
        let cfg = self.manager.config();
        let mut admin = Principal {
            name: cfg.web_user_name.trim().to_string(),
            is_admin: true,
            perms: Vec::new(),
        };
        if admin.name.is_empty() {
            admin.name = "admin".to_string();
        }
        match self.auth_level() {
            AuthLevel::None => {
                if !is_loopback(&client_ip(ctx)) {
                    admin.name = "anon".to_string();
                }
                Some(admin)
            }
            AuthLevel::LocalOnly => {
                if is_loopback(&client_ip(ctx)) {
                    Some(admin)
                } else {
                    None
                }
            }
            AuthLevel::Full => None,
        }
    }

    /// 解析请求主体（WebSocket 场景：浏览器无法设置请求头，支持查询参数 `token=` 回退）。
    pub(crate) fn principal_with_query_token(&self, ctx: &Ctx) -> Option<Principal> {
        if let Some(p) = self.principal(ctx) {
            return Some(p);
        }
        let token = arg(ctx, "token")?;
        self.tokens.get(token.trim())
    }

    /// 当前鉴权级别（动态读取配置）。
    fn auth_level(&self) -> AuthLevel {
        AuthLevel::parse(&self.manager.config().web_auth_level)
    }
}

// ————— 动作守卫（认证 + 权限 + 审计） —————

/// 权限 key（与前端导航 `data-panel` 一致；可授予列表见 [`crate::audit::ALL_PERMISSIONS`]）。
pub(crate) const PERM_DASHBOARD: &str = "dashboard";
pub(crate) const PERM_SERVICES: &str = "services";
pub(crate) const PERM_TRAFFIC: &str = "traffic";
pub(crate) const PERM_CONTROL: &str = "control";
pub(crate) const PERM_CONFIG: &str = "config";
pub(crate) const PERM_STARCONFIG: &str = "starconfig";
pub(crate) const PERM_LOGS: &str = "logs";
pub(crate) const PERM_WATCHDOG: &str = "watchdog";
pub(crate) const PERM_DATABASE: &str = "database";
pub(crate) const PERM_FILEMAN: &str = "fileman";
pub(crate) const PERM_CLEANUP: &str = "cleanup";
pub(crate) const PERM_PLUGINS: &str = "plugins";
pub(crate) const PERM_AUDIT: &str = "audit";
pub(crate) const PERM_AI: &str = "ai";
pub(crate) const PERM_TERMINAL: &str = "terminal";
/// 用户管理：仅内置管理员（不参与授权列表）。
pub(crate) const PERM_USERS: &str = "users";

/// 包装业务动作为受保护入口：**认证 → 权限 → 执行 → 审计**。
///
/// - `perm` 为空串表示“登录即可”（如修改本人密码）；
/// - 权限不足（403）与全部变更类操作（POST）及敏感读（文件读取/下载、数据库查询/备份）
///   会写入 `Agent_OperationLog` 审计表（参数摘要脱敏，见 [`summarize_request`]）。
pub(crate) fn guarded<F>(
    panel: Arc<WebPanel>,
    perm: &'static str,
    action: F,
) -> impl Fn(&Ctx) -> ActionResult + Send + Sync + 'static
where
    F: Fn(&WebPanel, &Ctx) -> ActionResult + Send + Sync + 'static,
{
    move |ctx| {
        let started = Instant::now();
        let Some(principal) = panel.principal(ctx) else {
            return json_error(401, "Unauthorized");
        };
        let action_name = action_name_of(&ctx.req.path);
        if !perm.is_empty() && !principal.allowed(perm) {
            let result = json_error(403, "没有权限执行该操作");
            record_audit(&panel, ctx, &principal, &action_name, started, true, &result);
            return result;
        }
        let result = action(&panel, ctx);
        if audit_needed(&ctx.req.method, &action_name) {
            record_audit(&panel, ctx, &principal, &action_name, started, false, &result);
        }
        result
    }
}

/// 是否需要审计（写操作与敏感读）。
fn audit_needed(method: &str, action: &str) -> bool {
    if method.eq_ignore_ascii_case("POST")
        || method.eq_ignore_ascii_case("PUT")
        || method.eq_ignore_ascii_case("DELETE")
    {
        return true;
    }
    matches!(
        action,
        "fileRead" | "fileDownload" | "dbQuery" | "dbBackup" | "dbDownloadBackup"
    )
}

/// 写入审计记录（`denied` 表示因权限拒绝而记录）。
fn record_audit(
    panel: &WebPanel,
    ctx: &Ctx,
    principal: &Principal,
    action: &str,
    started: Instant,
    denied: bool,
    result: &ActionResult,
) {
    let (code, message) = result_info(result);
    let mut path = ctx.req.path.clone();
    if !ctx.req.query.is_empty() {
        path = format!("{}?{}", path, truncate_text(&ctx.req.query, 100));
    }
    audit::record(
        &panel.base,
        &audit::AuditEntry {
            category: None,
            user: principal.name.clone(),
            ip: client_ip(ctx),
            action: action.to_string(),
            title: action_title(action),
            method: ctx.req.method.clone(),
            path: truncate_text(&path, 200),
            detail: summarize_request(ctx),
            success: !denied && code == 0,
            code: if denied { 403 } else { code },
            message: truncate_text(&message, 300),
            elapsed_ms: started.elapsed().as_millis() as i64,
        },
    );
}

/// 从动作结果提取（结果码, 消息）。
fn result_info(result: &ActionResult) -> (i32, String) {
    match result {
        ActionResult::Response(r) => {
            let is_json = r.headers.iter().any(|(k, v)| {
                k.eq_ignore_ascii_case("content-type") && v.to_ascii_lowercase().contains("json")
            });
            if is_json {
                if let Ok(v) = serde_json::from_slice::<Json>(&r.body) {
                    let code = v
                        .get("code")
                        .and_then(|c| c.as_i64())
                        .unwrap_or(r.status as i64) as i32;
                    let message = v
                        .get("message")
                        .and_then(|m| m.as_str())
                        .unwrap_or("")
                        .to_string();
                    (code, message)
                } else {
                    (r.status as i32, String::new())
                }
            } else if r.status < 400 {
                (0, "二进制响应".to_string())
            } else {
                (r.status as i32, String::new())
            }
        }
        ActionResult::View { .. } => (0, String::new()),
    }
}

/// 动作中文名（审计展示；未知动作回退为 `操作 {action}`）。
fn action_title(action: &str) -> String {
    pek_radmin::panel::action_title(action, ACTION_TITLES)
}

/// 动作 → 中文名映射（审计展示；`action_name_of` 来自 `pek_radmin::panel`）。
const ACTION_TITLES: &[(&str, &str)] = &[
    ("login", "登录"),
    ("logout", "退出登录"),
    ("control", "服务控制"),
    ("dhdeployPanel", "DHDeploy 面板访问"),
    ("freeMemory", "释放内存"),
    ("updateConfig", "更新配置"),
    ("changePassword", "修改密码"),
    ("upgrade", "程序升级"),
    ("selfUpgradeCheck", "检查升级"),
    ("syncTime", "同步系统时间"),
    ("startService", "启动子服务"),
    ("stopService", "停止子服务"),
    ("restartService", "重启子服务"),
    ("addService", "添加/更新子服务"),
    ("removeService", "删除子服务"),
    ("updateStarConfig", "更新星尘设置"),
    ("dbQuery", "数据库查询"),
    ("dbBackup", "数据库备份下载"),
    ("dbRestore", "数据库还原"),
    ("dbCreateBackup", "创建数据库备份"),
    ("dbDownloadBackup", "下载数据库备份"),
    ("dbRestoreBackup", "从备份还原数据库"),
    ("dbDeleteBackup", "删除数据库备份"),
    ("fileRead", "读取文件"),
    ("fileWrite", "保存文件"),
    ("fileMkdir", "新建文件夹"),
    ("fileNewFile", "新建文件"),
    ("fileDelete", "删除文件"),
    ("fileRename", "重命名"),
    ("fileMove", "移动文件"),
    ("fileCopy", "复制文件"),
    ("fileUpload", "上传文件"),
    ("fileDownload", "下载文件"),
    ("fileCompress", "压缩文件"),
    ("fileExtract", "解压文件"),
    ("fileChmod", "修改文件权限"),
    ("fileSearch", "搜索文件"),
    ("fileSize", "计算目录大小"),
    ("logCleanRun", "日志清理"),
    ("logCleanConfig", "日志清理配置"),
    ("logCleanConfigSave", "保存日志清理配置"),
    ("pluginInstall", "安装插件"),
    ("pluginDelete", "卸载插件"),
    ("pluginStoreInstall", "安装/更新在线插件"),
    ("aiChat", "AI 助手对话"),
    ("termReset", "重置终端会话"),
    ("userSave", "保存面板用户"),
    ("userDelete", "删除面板用户"),
    ("userResetPassword", "重置用户密码"),
    ("auditLogs", "查看操作日志"),
];

/// 请求参数摘要（查询串 + JSON/表单体；敏感字段脱敏；截断至 400 字符）。
///
/// 实现已下沉 `pek_radmin::panel::summarize_body`（与 HlkProductTool 面板共用）。
fn summarize_request(ctx: &Ctx) -> String {
    pek_radmin::panel::summarize_body(
        &ctx.req.query,
        ctx.header("content-type").unwrap_or(""),
        ctx.req.body.as_ref(),
    )
}

// ————— 控制器注册 —————

/// 构建 `/api` 控制器（动作经 [`guarded`] 统一做 认证+权限+审计）。
pub fn build_api_controller(panel: Arc<WebPanel>) -> Controller {
    let mut controller = Controller::new("api");

    // 登录/退出：无需令牌（成功与失败均写操作审计）
    let p = panel.clone();
    controller = controller.post("login", move |ctx| login(&p, ctx));
    let p = panel.clone();
    controller = controller.post("logout", move |ctx| logout(&p, ctx));

    // 当前主体（前端登录后取权限渲染菜单；仅认证不校验菜单权限）
    controller = controller.get("me", guarded(panel.clone(), "", me));

    controller = controller.get("status", guarded(panel.clone(), PERM_DASHBOARD, status));
    controller = controller.get("health", guarded(panel.clone(), PERM_DASHBOARD, health));
    controller = controller.get("freeMemory", guarded(panel.clone(), PERM_CONTROL, free_memory));
    controller = controller.get(
        "configMetadata",
        guarded(panel.clone(), PERM_CONFIG, config_metadata),
    );
    controller = controller.post(
        "updateConfig",
        guarded(panel.clone(), PERM_CONFIG, update_config),
    );
    controller = controller.post(
        "changePassword",
        guarded(panel.clone(), "", change_password),
    );
    controller = controller.get("logs", guarded(panel.clone(), PERM_LOGS, logs));
    controller = controller.get("logFiles", guarded(panel.clone(), PERM_LOGS, log_files));
    controller = controller.get(
        "watchdog",
        guarded(panel.clone(), PERM_WATCHDOG, watchdog),
    );
    controller = controller.post("upgrade", guarded(panel.clone(), PERM_CONFIG, upgrade));
    controller = controller.post(
        "syncTime",
        guarded(panel.clone(), PERM_DASHBOARD, sync_time),
    );
    controller = controller.post("control", guarded(panel.clone(), PERM_CONTROL, control));
    controller = controller.post(
        "selfUpgradeCheck",
        guarded(panel.clone(), PERM_CONTROL, self_upgrade_check),
    );
    controller.map(
        "*",
        "dhdeployPanel",
        guarded(panel, PERM_CONTROL, dhdeploy_panel),
    )
}

/// 构建 `/star` 控制器（动作经 [`guarded`] 统一做 认证+权限+审计）。
pub fn build_star_controller(panel: Arc<WebPanel>) -> Controller {
    let mut controller = Controller::new("star");

    controller = controller.get("services", guarded(panel.clone(), PERM_SERVICES, services));
    controller = controller.post(
        "startService",
        guarded(panel.clone(), PERM_SERVICES, |p, ctx| {
            service_op(p, ctx, Op::Start)
        }),
    );
    controller = controller.post(
        "stopService",
        guarded(panel.clone(), PERM_SERVICES, |p, ctx| {
            service_op(p, ctx, Op::Stop)
        }),
    );
    controller = controller.post(
        "restartService",
        guarded(panel.clone(), PERM_SERVICES, |p, ctx| {
            service_op(p, ctx, Op::Restart)
        }),
    );
    controller = controller.post(
        "addService",
        guarded(panel.clone(), PERM_SERVICES, add_service),
    );
    controller = controller.post(
        "removeService",
        guarded(panel.clone(), PERM_SERVICES, remove_service),
    );
    controller = controller.get(
        "getStarConfig",
        guarded(panel.clone(), PERM_STARCONFIG, get_star_config),
    );
    controller = controller.post(
        "updateStarConfig",
        guarded(panel.clone(), PERM_STARCONFIG, update_star_config),
    );
    controller = controller.get("machine", guarded(panel.clone(), PERM_DASHBOARD, machine));
    controller = controller.get(
        "webTraffic",
        guarded(panel.clone(), PERM_TRAFFIC, web_traffic),
    );
    controller = controller.get(
        "portTraffic",
        guarded(panel.clone(), PERM_TRAFFIC, port_traffic),
    );
    controller = controller.get(
        "trafficHistory",
        guarded(panel.clone(), PERM_TRAFFIC, traffic_history),
    );
    controller = controller.get("dbInfo", guarded(panel.clone(), PERM_DATABASE, db_info));
    controller = controller.post("dbQuery", guarded(panel.clone(), PERM_DATABASE, db_query));
    controller = controller.get("dbBackup", guarded(panel.clone(), PERM_DATABASE, db_backup));
    controller = controller.post("dbRestore", guarded(panel.clone(), PERM_DATABASE, db_restore));
    controller = controller.get(
        "dbListBackups",
        guarded(panel.clone(), PERM_DATABASE, db_list_backups),
    );
    controller = controller.post(
        "dbCreateBackup",
        guarded(panel.clone(), PERM_DATABASE, db_create_backup),
    );
    controller = controller.get(
        "dbDownloadBackup",
        guarded(panel.clone(), PERM_DATABASE, db_download_backup),
    );
    controller = controller.post(
        "dbRestoreBackup",
        guarded(panel.clone(), PERM_DATABASE, db_restore_backup),
    );
    controller = controller.post(
        "dbDeleteBackup",
        guarded(panel.clone(), PERM_DATABASE, db_delete_backup),
    );

    // 文件管理（「文件管理」页）
    controller = controller.get(
        "fileList",
        guarded(panel.clone(), PERM_FILEMAN, crate::fileman::file_list),
    );
    controller = controller.get(
        "fileRead",
        guarded(panel.clone(), PERM_FILEMAN, crate::fileman::file_read),
    );
    controller = controller.post(
        "fileWrite",
        guarded(panel.clone(), PERM_FILEMAN, crate::fileman::file_write),
    );
    controller = controller.post(
        "fileMkdir",
        guarded(panel.clone(), PERM_FILEMAN, crate::fileman::file_mkdir),
    );
    controller = controller.post(
        "fileNewFile",
        guarded(panel.clone(), PERM_FILEMAN, crate::fileman::file_new_file),
    );
    controller = controller.post(
        "fileDelete",
        guarded(panel.clone(), PERM_FILEMAN, crate::fileman::file_delete),
    );
    controller = controller.post(
        "fileRename",
        guarded(panel.clone(), PERM_FILEMAN, crate::fileman::file_rename),
    );
    controller = controller.post(
        "fileMove",
        guarded(panel.clone(), PERM_FILEMAN, crate::fileman::file_move),
    );
    controller = controller.post(
        "fileCopy",
        guarded(panel.clone(), PERM_FILEMAN, crate::fileman::file_copy),
    );
    controller = controller.post(
        "fileUpload",
        guarded(panel.clone(), PERM_FILEMAN, crate::fileman::file_upload),
    );
    controller = controller.get(
        "fileDownload",
        guarded(panel.clone(), PERM_FILEMAN, crate::fileman::file_download),
    );
    controller = controller.post(
        "fileCompress",
        guarded(panel.clone(), PERM_FILEMAN, crate::fileman::file_compress),
    );
    controller = controller.post(
        "fileExtract",
        guarded(panel.clone(), PERM_FILEMAN, crate::fileman::file_extract),
    );
    controller = controller.post(
        "fileChmod",
        guarded(panel.clone(), PERM_FILEMAN, crate::fileman::file_chmod),
    );
    controller = controller.get(
        "fileSearch",
        guarded(panel.clone(), PERM_FILEMAN, crate::fileman::file_search),
    );
    controller = controller.post(
        "fileSize",
        guarded(panel.clone(), PERM_FILEMAN, crate::fileman::file_dir_size),
    );

    // 日志清理（供「日志清理」插件使用；接口保留在核心）
    controller = controller.get(
        "logCleanScan",
        guarded(panel.clone(), PERM_CLEANUP, crate::logclean::log_clean_scan),
    );
    controller = controller.post(
        "logCleanRun",
        guarded(panel.clone(), PERM_CLEANUP, crate::logclean::log_clean_run),
    );
    controller = controller.get(
        "logCleanConfig",
        guarded(panel.clone(), PERM_CLEANUP, crate::logclean::log_clean_config),
    );
    controller = controller.post(
        "logCleanConfigSave",
        guarded(panel.clone(), PERM_CLEANUP, crate::logclean::log_clean_config_save),
    );

    // 插件（「插件」页；页面本身经 /plugins/* 静态服务）
    controller = controller.get(
        "pluginList",
        guarded(panel.clone(), PERM_PLUGINS, crate::plugins::plugin_list),
    );
    controller = controller.post(
        "pluginInstall",
        guarded(panel.clone(), PERM_PLUGINS, crate::plugins::plugin_install),
    );
    controller = controller.post(
        "pluginDelete",
        guarded(panel.clone(), PERM_PLUGINS, crate::plugins::plugin_delete),
    );
    controller = controller.get(
        "pluginStore",
        guarded(panel.clone(), PERM_PLUGINS, crate::plugins::plugin_store),
    );
    controller = controller.post(
        "pluginStoreInstall",
        guarded(
            panel.clone(),
            PERM_PLUGINS,
            crate::plugins::plugin_store_install,
        ),
    );

    // 用户管理（仅内置管理员）与操作日志
    controller = controller.get("userList", guarded(panel.clone(), PERM_USERS, user_list));
    controller = controller.post("userSave", guarded(panel.clone(), PERM_USERS, user_save));
    controller = controller.post(
        "userDelete",
        guarded(panel.clone(), PERM_USERS, user_delete),
    );
    controller = controller.get("auditLogs", guarded(panel.clone(), PERM_AUDIT, audit_logs));

    // AI 助手（服务器问题分析；模型接口/Key 可在配置页自定义）
    controller = controller.get(
        "aiStatus",
        guarded(panel.clone(), PERM_AI, crate::ai::ai_status),
    );
    controller = controller.post(
        "aiChat",
        guarded(panel.clone(), PERM_AI, crate::ai::ai_chat),
    );

    // 在线终端（真 PTY + WebSocket，见 crate::terminal；WS 入口注册在 server.rs）
    controller = controller.post(
        "termReset",
        guarded(panel.clone(), PERM_TERMINAL, crate::terminal::term_reset),
    );

    controller.get(
        "getProcessList",
        guarded(panel, PERM_DASHBOARD, get_process_list),
    )
}

// ————— /api 动作 —————

/// 登录：签发令牌（成功/失败均写入操作审计）。
fn login(panel: &WebPanel, ctx: &Ctx) -> ActionResult {
    let ip = client_ip(ctx);

    if panel.logins.is_blocked(&ip) {
        return json_error(429, "Too many failed attempts. Try again later.");
    }

    let user = arg(ctx, "user").unwrap_or_default();
    let password = arg(ctx, "password").unwrap_or_default();

    match panel.authenticate(&user, &password) {
        Some(principal) => {
            let token = panel.tokens.issue_with(principal.clone());
            panel.logins.record_success(&ip);
            util::log_format("Web 面板登录成功：{}（{}）", &[&principal.name, &ip]);
            audit::record(
                &panel.base,
                &audit::AuditEntry {
                    category: None,
                    user: principal.name.clone(),
                    ip: ip.clone(),
                    action: "login".to_string(),
                    title: action_title("login"),
                    method: ctx.req.method.clone(),
                    path: ctx.req.path.clone(),
                    detail: String::new(),
                    success: true,
                    code: 0,
                    message: "登录成功".to_string(),
                    elapsed_ms: 0,
                },
            );
            json_result(0, "", Some(json!({ "token": token })))
        }
        None => {
            panel.logins.record_failure(&ip);
            util::log_format("Web 面板登录失败：{}（{}）", &[&user, &ip]);
            audit::record(
                &panel.base,
                &audit::AuditEntry {
                    category: None,
                    user: truncate_text(user.trim(), 50),
                    ip: ip.clone(),
                    action: "login".to_string(),
                    title: action_title("login"),
                    method: ctx.req.method.clone(),
                    path: ctx.req.path.clone(),
                    detail: String::new(),
                    success: false,
                    code: 401,
                    message: "用户名或密码错误".to_string(),
                    elapsed_ms: 0,
                },
            );
            json_error(401, "Invalid credentials")
        }
    }
}

/// 注销：吊销当前令牌（写入操作审计）。
fn logout(panel: &WebPanel, ctx: &Ctx) -> ActionResult {
    if let Some(token) = bearer_token(ctx) {
        if let Some(principal) = panel.principal(ctx) {
            audit::record(
                &panel.base,
                &audit::AuditEntry {
                    category: None,
                    user: principal.name,
                    ip: client_ip(ctx),
                    action: "logout".to_string(),
                    title: action_title("logout"),
                    method: ctx.req.method.clone(),
                    path: ctx.req.path.clone(),
                    detail: String::new(),
                    success: true,
                    code: 0,
                    message: "已退出登录".to_string(),
                    elapsed_ms: 0,
                },
            );
        }
        panel.revoke_token(&token);
    }
    json_result(0, "ok", None)
}

/// 是否仍在使用默认登录凭据（admin/admin）。启动日志与面板横幅共用判定。
pub(crate) fn uses_default_credentials(cfg: &AgentConfig) -> bool {
    cfg.web_user_name.trim().eq_ignore_ascii_case("admin") && cfg.web_user_password == "admin"
}

/// 服务状态。
fn status(panel: &WebPanel, ctx: &Ctx) -> ActionResult {
    if !panel.check_auth(ctx) {
        return json_error(401, "Unauthorized");
    }

    let cfg = panel.manager.config();
    let pid = std::process::id();
    // 整机/网络/磁盘/TCP/线程句柄等指标来自后台 1 秒采样器：
    // 多客户端读数一致、请求路径零采集；CPU 为 1 秒窗口（与任务管理器/宝塔同粒度）
    let sample = sampler::current();
    let threads = sample.agent_threads;
    let handles = sample.agent_handles;
    let memory_mb = sys::memory_mb(pid).unwrap_or(0);
    let (mem_total, mem_avail) = sys::memory_info().unwrap_or((0, 0));
    let mem_used_mb = mem_total.saturating_sub(mem_avail) / 1024 / 1024;
    let disks = sys::disk_usages();
    // 进程 CPU 时间：供内嵌 health 字段使用（复用本次采集，避免前端第二次请求）
    let (cpu_total, cpu_kernel, cpu_user) = sys::process_cpu_seconds();
    let load = sys::load_average();
    let cpu_rate = sample.cpu_rate;
    let (tcp_estab, tcp_time_wait, tcp_close_wait) =
        (sample.tcp_estab, sample.tcp_time_wait, sample.tcp_close_wait);
    let (net_up, net_down) = (sample.net_up_bps, sample.net_down_bps);
    let (net_rx_total, net_tx_total) = (sample.net_rx_total, sample.net_tx_total);
    let (
        disk_iops_rate,
        disk_read_bps,
        disk_write_bps,
        disk_latency_ms,
        disk_read_bytes,
        disk_write_bytes,
    ) = (
        sample.disk_iops,
        sample.disk_read_bps,
        sample.disk_write_bps,
        sample.disk_latency_ms,
        sample.disk_read_bytes,
        sample.disk_write_bytes,
    );
    let uptime = panel.uptime();
    let cpu_count = std::thread::available_parallelism()
        .map(|v| v.get())
        .unwrap_or(0);
    // 代理自身 CPU 占用（占整机百分比；采样器 1 秒窗口差分）
    let proc_cpu = sample.agent_cpu_rate;
    // 自动升级检查状态快照（面板自升级卡片）
    let self_upgrade = crate::self_upgrade::status();

    let data = json!({
        "serviceName": cfg.service_name,
        "displayName": cfg.display_name,
        "description": cfg.description,
        "running": true,
        "uptime": dhrust::sys::process::format_uptime(uptime),
        "uptimeSeconds": uptime.as_secs(),
        "processId": pid,
        "memoryMB": memory_mb,
        "memoryUsedMB": mem_used_mb,
        "memoryTotalMB": mem_total / 1024 / 1024,
        "threadCount": threads,
        "handleCount": handles,
        "processCpuRate": format!("{proc_cpu:.1}"),
        "processCpuRateValue": (proc_cpu * 10.0).round() / 10.0,
        "startTime": panel.started_at.format("%m-%d %H:%M:%S").to_string(),
        "hostMachine": sys::hostname(),
        "platform": platform_name(),
        "osVersion": sys::os_description(),
        "cpuName": sys::cpu_model().unwrap_or_default(),
        "cpuCount": cpu_count,
        "cpuRate": cpu_rate.map(|v| format!("{v:.1}")).unwrap_or_default(),
        "cpuRateValue": cpu_rate.map(|v| (v * 10.0).round() / 10.0).unwrap_or(0.0),
        "totalMemory": format!("{} GB", mem_total / 1024 / 1024 / 1024),
        "availableMemory": format!("{} GB", mem_avail / 1024 / 1024 / 1024),
        "freeMemory": format!("{} GB", mem_avail / 1024 / 1024 / 1024),
        "board": "",
        "machineGuid": sys::machine_guid().unwrap_or_default(),
        "uplinkSpeed": format_speed(net_up),
        "downlinkSpeed": format_speed(net_down),
        "uplinkBps": net_up,
        "downlinkBps": net_down,
        "netTxBytes": net_tx_total,
        "netRxBytes": net_rx_total,
        "tcpConnections": tcp_estab,
        "tcpTimeWait": tcp_time_wait,
        "tcpCloseWait": tcp_close_wait,
        "diskIops": disk_iops_rate,
        "diskReadBps": disk_read_bps,
        "diskWriteBps": disk_write_bps,
        "diskLatencyMs": disk_latency_ms,
        "diskReadBytes": disk_read_bytes,
        "diskWriteBytes": disk_write_bytes,
        "disks": disks
            .iter()
            .map(|(used, total, name)| json!({ "name": name, "usedMB": used, "totalMB": total }))
            .collect::<Vec<_>>(),
        "load1": load.map(|l| l.0),
        "load5": load.map(|l| l.1),
        "load15": load.map(|l| l.2),
        "hostUptime": dhrust::sys::process::format_uptime(Duration::from_secs(sys::host_uptime_seconds())),
        // 服务器本地时间（含时区偏移；顶部机器概览以 3 秒粒度刷新，用于时间/时区核对）
        "localTime": chrono::Local::now().format("%Y-%m-%d %H:%M:%S %:z").to_string(),
        "port": panel.port(),
        // 默认凭据提示：面板顶部横幅数据（remoteAccess=允许远程访问时风险更高）
        "defaultPassword": uses_default_credentials(&cfg),
        "remoteAccess": !cfg.local_only,
        // 自动升级（Pek.RPanlServer 发行源）：当前版本 / 检查状态（面板「控制」页展示）
        "selfUpgrade": {
            "enabled": crate::self_upgrade::enabled(&cfg),
            "current": env!("CARGO_PKG_VERSION"),
            "url": cfg.auto_upgrade_url.clone(),
            "intervalMinutes": cfg.auto_upgrade_interval_minutes,
            "checkedAt": self_upgrade.as_ref().map(|s| s.checked_at.clone()).unwrap_or_default(),
            "latest": self_upgrade.as_ref().map(|s| s.latest.clone()).unwrap_or_default(),
            "message": self_upgrade.as_ref().map(|s| s.message.clone()).unwrap_or_default(),
        },
        // 平台实时通道（Pek.RPanlServer「服务器节点」）：令牌已配置 / 当前连接状态
        "panelWs": {
            "enabled": crate::panel_ws::enabled(&cfg),
            "connected": crate::panel_ws::connected(),
        },
        // 进程健康指标（原 /api/health 合并至此：面板每 3 秒刷新只需一次请求，
        // 且复用上面已采集的进程统计，省一次全进程快照与进程内存查询）
        "health": json!({
            "memoryMB": memory_mb,
            "memoryLimitMB": 0,
            "threadCount": threads,
            "threadLimit": 0,
            "handleCount": handles,
            "handleLimit": 0,
            "totalProcessorTime": format!("{cpu_total:.1}"),
            "privilegedProcessorTime": format!("{cpu_kernel:.1}"),
            "userProcessorTime": format!("{cpu_user:.1}"),
            "gcTotalMemory": 0,
            "gcCollections": { "gen0": 0, "gen1": 0, "gen2": 0 },
        }),
    });

    json_result(0, "", Some(data))
}

/// 健康指标。
fn health(panel: &WebPanel, ctx: &Ctx) -> ActionResult {
    if !panel.check_auth(ctx) {
        return json_error(401, "Unauthorized");
    }
    let pid = std::process::id();
    let (threads, handles) = sys::process_stats(pid).unwrap_or((0, 0));
    let memory_mb = sys::memory_mb(pid).unwrap_or(0);
    let (total, kernel, user) = sys::process_cpu_seconds();

    let data = json!({
        "memoryMB": memory_mb,
        "memoryLimitMB": 0,
        "threadCount": threads,
        "threadLimit": 0,
        "handleCount": handles,
        "handleLimit": 0,
        "totalProcessorTime": format!("{total:.1}"),
        "privilegedProcessorTime": format!("{kernel:.1}"),
        "userProcessorTime": format!("{user:.1}"),
        "gcTotalMemory": 0,
        "gcCollections": { "gen0": 0, "gen1": 0, "gen2": 0 },
    });

    json_result(0, "", Some(data))
}

/// 网站流量统计（`/star/webTraffic`）：站点日志聚合快照（今日/累计/UV/状态码）。
fn web_traffic(panel: &WebPanel, ctx: &Ctx) -> ActionResult {
    if !panel.check_auth(ctx) {
        return json_error(401, "Unauthorized");
    }
    json_result(0, "", Some(crate::weblog::snapshot_json()))
}

/// 端口流量统计（`/star/portTraffic`）：各端口收发字节/速率/连接数。
fn port_traffic(panel: &WebPanel, ctx: &Ctx) -> ActionResult {
    if !panel.check_auth(ctx) {
        return json_error(401, "Unauthorized");
    }
    json_result(0, "", Some(crate::portstat::snapshot_json()))
}

/// 流量历史（`/star/trafficHistory?days=30`）：每日归档（网站+端口）按日期升序。
fn traffic_history(panel: &WebPanel, ctx: &Ctx) -> ActionResult {
    if !panel.check_auth(ctx) {
        return json_error(401, "Unauthorized");
    }
    let days = arg(ctx, "days")
        .and_then(|s| s.trim().parse::<usize>().ok())
        .unwrap_or(30)
        .clamp(1, crate::history::MAX_RETENTION_DAYS as usize);
    let cfg = panel.manager.config();
    json_result(
        0,
        "",
        Some(crate::history::snapshot_json(
            panel.manager.base(),
            days,
            cfg.traffic_history_days,
        )),
    )
}

// ————— 数据库管理（/star/db*：只读查询 / 备份 / 还原） —————

/// 数据库概况：文件路径/大小/表行数。
fn db_info(panel: &WebPanel, ctx: &Ctx) -> ActionResult {
    if !panel.check_auth(ctx) {
        return json_error(401, "Unauthorized");
    }
    json_result(
        0,
        "",
        Some(crate::history::database_info(panel.manager.base())),
    )
}

/// 只读 SQL 查询（仅 SELECT；安全校验 + 500 行上限）。
fn db_query(panel: &WebPanel, ctx: &Ctx) -> ActionResult {
    if !panel.check_auth(ctx) {
        return json_error(401, "Unauthorized");
    }
    let sql = json_body(ctx)
        .and_then(|v| {
            v.get("sql")
                .and_then(|s| s.as_str())
                .map(|s| s.to_string())
        })
        .unwrap_or_default();
    if let Err(e) = crate::history::validate_readonly_sql(&sql) {
        return json_error(400, &e);
    }
    match crate::history::query_readonly(panel.manager.base(), &sql) {
        Ok(data) => json_result(0, "", Some(data)),
        Err(e) => json_error(500, &e),
    }
}

/// 备份下载：DbTable zip 包（含两张表与模型 XML，与 C# 生态互通）。
fn db_backup(panel: &WebPanel, ctx: &Ctx) -> ActionResult {
    if !panel.check_auth(ctx) {
        return json_error(401, "Unauthorized");
    }
    match crate::history::backup_zip(panel.manager.base()) {
        Ok(bytes) => {
            let name = format!("traffic-backup-{}.zip", Local::now().format("%Y%m%d-%H%M%S"));
            util::log_format(
                "Web 面板数据库备份下载：{}（{} 字节）",
                &[&name, &bytes.len().to_string()],
            );
            ActionResult::Response(
                HttpResponse::bytes(200, "application/zip", bytes).with_header(
                    "Content-Disposition",
                    &format!("attachment; filename=\"{name}\""),
                ),
            )
        }
        Err(e) => json_error(500, &e),
    }
}

/// 上传还原：请求体即备份 zip 包（先校验，后清空现有两张表并导入）。
fn db_restore(panel: &WebPanel, ctx: &Ctx) -> ActionResult {
    if !panel.check_auth(ctx) {
        return json_error(401, "Unauthorized");
    }
    let body = &ctx.req.body;
    const MAX_SIZE: usize = 64 * 1024 * 1024;
    if body.len() < 512 {
        return json_error(400, "备份文件过小或为空（请上传“备份下载”导出的 zip 包）");
    }
    if body.len() > MAX_SIZE {
        return json_error(400, "备份文件过大（上限 64MB）");
    }
    if !body.starts_with(b"PK") {
        return json_error(400, "文件格式校验失败：不是 zip 备份包");
    }
    match crate::history::restore_zip(panel.manager.base(), body.as_ref()) {
        Ok(data) => json_result(0, "还原完成", Some(data)),
        Err(e) => json_error(500, &e),
    }
}

/// 服务器端备份文件列表。
fn db_list_backups(panel: &WebPanel, ctx: &Ctx) -> ActionResult {
    if !panel.check_auth(ctx) {
        return json_error(401, "Unauthorized");
    }
    json_result(
        0,
        "",
        Some(crate::history::list_backups(panel.manager.base())),
    )
}

/// 创建服务器端备份文件（`Data/Backup`）。
fn db_create_backup(panel: &WebPanel, ctx: &Ctx) -> ActionResult {
    if !panel.check_auth(ctx) {
        return json_error(401, "Unauthorized");
    }
    match crate::history::create_backup(panel.manager.base()) {
        Ok(data) => json_result(0, "备份完成", Some(data)),
        Err(e) => json_error(500, &e),
    }
}

/// 下载服务器端备份文件（`?name=`）。
fn db_download_backup(panel: &WebPanel, ctx: &Ctx) -> ActionResult {
    if !panel.check_auth(ctx) {
        return json_error(401, "Unauthorized");
    }
    let name = arg(ctx, "name").unwrap_or_default();
    if let Err(e) = crate::history::validate_backup_name(&name) {
        return json_error(400, &e);
    }
    match crate::history::read_backup(panel.manager.base(), &name) {
        Ok(bytes) => {
            util::log_format(
                "Web 面板数据库备份下载：{}（{} 字节）",
                &[&name, &bytes.len().to_string()],
            );
            ActionResult::Response(
                HttpResponse::bytes(200, "application/zip", bytes).with_header(
                    "Content-Disposition",
                    &format!("attachment; filename=\"{name}\""),
                ),
            )
        }
        Err(e) => json_error(500, &e),
    }
}

/// 从服务器端备份文件还原（JSON `{name}`）。
fn db_restore_backup(panel: &WebPanel, ctx: &Ctx) -> ActionResult {
    if !panel.check_auth(ctx) {
        return json_error(401, "Unauthorized");
    }
    let name = json_body(ctx)
        .and_then(|v| v.get("name").and_then(|s| s.as_str()).map(String::from))
        .unwrap_or_default();
    if let Err(e) = crate::history::validate_backup_name(&name) {
        return json_error(400, &e);
    }
    match crate::history::restore_backup(panel.manager.base(), &name) {
        Ok(data) => json_result(0, "还原完成", Some(data)),
        Err(e) => json_error(500, &e),
    }
}

/// 删除服务器端备份文件（JSON `{name}`）。
fn db_delete_backup(panel: &WebPanel, ctx: &Ctx) -> ActionResult {
    if !panel.check_auth(ctx) {
        return json_error(401, "Unauthorized");
    }
    let name = json_body(ctx)
        .and_then(|v| v.get("name").and_then(|s| s.as_str()).map(String::from))
        .unwrap_or_default();
    if let Err(e) = crate::history::validate_backup_name(&name) {
        return json_error(400, &e);
    }
    match crate::history::delete_backup(panel.manager.base(), &name) {
        Ok(()) => json_result(0, "已删除", None),
        Err(e) => json_error(500, &e),
    }
}

/// 释放内存（尽力回收工作集）。
fn free_memory(panel: &WebPanel, ctx: &Ctx) -> ActionResult {
    if !panel.check_auth(ctx) {
        return json_error(401, "Unauthorized");
    }

    let pid = std::process::id();
    let before = sys::memory_mb(pid).unwrap_or(0);
    util::log_format("Web 面板触发释放内存，释放前 {}MB", &[&before.to_string()]);

    let success = sys::empty_working_set();

    let after = sys::memory_mb(pid).unwrap_or(0);
    let freed = before.saturating_sub(after);
    util::log_format(
        "释放内存完成，释放后 {}MB，释放 {}MB",
        &[&after.to_string(), &freed.to_string()],
    );

    json_result(
        if success { 0 } else { 500 },
        &if success {
            format!("释放内存完成：{before}MB → {after}MB，释放 {freed}MB")
        } else {
            "释放内存失败".to_string()
        },
        Some(json!({ "beforeMB": before, "afterMB": after, "freedMB": freed })),
    )
}

/// 同步系统时间（`{"timeMs": 毫秒时间戳}`；Web 面板“同步时间”按钮）。
///
/// 以**访问面板的浏览器所在机器**时间为准（通常已由 NTP 保持准确），
/// 只校正时钟、不改时区；需要相应权限（Unix root / Windows 管理员或服务账户）。
fn sync_time(panel: &WebPanel, ctx: &Ctx) -> ActionResult {
    if !panel.check_auth(ctx) {
        return json_error(401, "Unauthorized");
    }

    let Some(epoch_ms) = arg_i64(ctx, "timeMs") else {
        return json_error(400, "Missing timeMs");
    };

    // 合理区间护栏（2000-01-01 ~ 2100-01-01 UTC）：防止误传把时钟改坏
    const MIN_MS: i64 = 946_684_800_000;
    const MAX_MS: i64 = 4_102_444_800_000;
    if !(MIN_MS..=MAX_MS).contains(&epoch_ms) {
        return json_error(400, "时间戳超出允许范围（2000~2100 年）");
    }

    let ip = client_ip(ctx);
    match sys::set_system_time(epoch_ms) {
        Ok(()) => {
            let local = Local::now().format("%Y-%m-%d %H:%M:%S %:z").to_string();
            util::log_format("Web 面板同步系统时间成功：{}（{}）", &[&local, &ip]);
            json_result(
                0,
                &format!("系统时间已同步：{local}"),
                Some(json!({ "localTime": local })),
            )
        }
        Err(message) => {
            util::log_format("Web 面板同步系统时间失败：{}（{}）", &[&message, &ip]);
            json_error(500, &message)
        }
    }
}

/// 面板配置元数据（凭据类字段除外：内置管理员用户名/密码在「用户」页管理；数据库用户本人改密走 ChangePassword 接口）。
fn config_metadata(panel: &WebPanel, ctx: &Ctx) -> ActionResult {
    if !panel.check_auth(ctx) {
        return json_error(401, "Unauthorized");
    }

    let cfg = panel.manager.config();
    let items = vec![
        config_item("WebAuthLevel", "鉴权级别", "String", cfg.web_auth_level.clone(), "None不鉴权；LocalOnly本地免鉴权、远程需登录（默认）；Full全部需登录；修改后自动生效（无需重启）"),
        config_item("SampleInterval", "采样间隔(ms)", "Int32", cfg.sample_interval.to_string(), "后台资源采样间隔，默认1000（与任务管理器/宝塔同粒度）；0=关闭后台采样（改为面板请求时现采）。修改需重启服务后生效"),
        config_item("WebTraffic", "网站流量统计", "Boolean", cfg.web_traffic.to_string(), "解析 nginx/apache 访问日志（自动发现站点；个别未发现的站点可在配置文件 WebLogs 项补充），零侵入只读；修改后自动生效"),
        config_item("PortTraffic", "端口流量统计", "Boolean", cfg.port_traffic.to_string(), "默认开启。Linux 创建独立 nftables 计数表统计各端口收发流量（只计数不改转发，关闭/卸载自动清理；需 root）；无 nft 或权限不足、Windows 时降级为连接视图；端口自动取系统监听（个别端口可在配置文件 PortTrafficPorts 项指定）；修改后自动生效"),
        config_item("TrafficHistoryDays", "流量历史保留天数", "Int32", cfg.traffic_history_days.to_string(), "每日归档（SQLite：Data/traffic.db，Pek.RCode 消费方）的保留天数，默认 90（7~3650）；0=永久保留。修改后自动生效"),
        config_item("PluginStoreUrl", "插件源地址（HTTPS）", "String", cfg.plugin_store_url.clone(), "在线插件目录（catalog.json）的地址；仅允许 https（127.0.0.1 例外便于本地调试）；留空=关闭在线插件。修改后自动生效"),
        config_item("PluginStorePubKey", "插件源公钥（Ed25519 hex，可选）", "String", cfg.plugin_store_pubkey.clone(), "填写后强制校验插件源签名（catalog.json.sig），防止插件源被篡改；留空=仅 HTTPS+SHA-256 校验。修改后自动生效"),
        config_item("AutoUpgradeUrl", "自动升级源地址（HTTPS）", "String", cfg.auto_upgrade_url.clone(), "星尘代理发行源（catalog.json）地址；默认已指向官方平台（p.sc8.fun）。仅允许 https（127.0.0.1 例外）；留空=关闭自动升级。配置「插件源公钥」后强制验签（留空=仅 HTTPS+SHA-256 校验）。修改后自动生效"),
        config_item("AutoUpgradeIntervalMinutes", "自动升级检查间隔(分钟)", "Int32", cfg.auto_upgrade_interval_minutes.to_string(), "定期检查发行源新版本的间隔，默认 60（5~1440）。修改后自动生效"),
    config_item("AutoUpgradeToken", "平台接入令牌", "Password", cfg.auto_upgrade_token.clone(), "Pek.RPanlServer「服务器节点」页生成的接入令牌；填写后启用实时通道：向平台上报机器数据（CPU/内存/磁盘/子服务），并接收「立即检查升级」指令。留空 = 不接入。修改后自动生效"),
        config_item("AiEnabled", "AI 助手", "Boolean", cfg.ai_enabled.to_string(), "启用后可在「AI 助手」页对话分析服务器问题（OpenAI 兼容接口，默认接入 DeepSeek）。修改后自动生效"),
        config_item("AiBaseUrl", "AI 接口地址", "String", cfg.ai_base_url.clone(), "OpenAI 兼容 Base 地址（如 https://api.deepseek.com/v1；也可直接填完整 …/chat/completions 地址）。接入其他厂商/本地模型时修改。修改后自动生效"),
        config_item("AiModel", "AI 模型", "String", cfg.ai_model.clone(), "模型名（如 deepseek-chat 对话 / deepseek-reasoner 推理；接入其他服务时填其模型名）。修改后自动生效"),
        config_item("AiApiKey", "AI API Key", "Password", cfg.ai_api_key.clone(), "模型服务商 API Key（Bearer 令牌）。修改后自动生效"),
        config_item("TerminalEnabled", "在线终端", "Boolean", cfg.terminal_enabled.to_string(), "启用后可在「🖥 在线终端」页执行服务器命令（免 SSH 登录；命令以服务账户权限运行，全部执行记录写入审计）。修改后自动生效"),
        config_item("LocalPort", "本地端口", "Int32", cfg.local_port.to_string(), "本地控制端口（TCP 面板与 UDP RPC 共用），默认5501（与 C# 版 StarAgent 5500 错开）；修改需重启服务后生效"),
        config_item("LocalOnly", "仅本机访问", "Boolean", cfg.local_only.to_string(), "为真时只绑定 127.0.0.1（远程无法连接）；默认为假，允许远程访问（面板凭据兜底）。修改需重启服务后生效"),
        config_item("StartWait", "启动等待(ms)", "Int32", cfg.start_wait.to_string(), "该时间内进程退出视为启动失败，默认3000"),
        config_item("MaxFails", "最大失败次数", "Int32", cfg.max_fails.to_string(), "超过后不再尝试启动，默认20"),
        config_item("GuardPeriod", "守护周期(ms)", "Int32", cfg.guard_period.to_string(), "服务守护检查周期，默认30000；修改需重启服务后生效"),
        config_item("Debug", "调试日志", "Boolean", cfg.debug.to_string(), "开启后应用输出重定向到 Log 目录"),
    ];

    json_result(0, "", Some(json!({ "items": items })))
}

/// 更新面板配置（安全白名单，密码走 ChangePassword）。
fn update_config(panel: &WebPanel, ctx: &Ctx) -> ActionResult {
    if !panel.check_auth(ctx) {
        return json_error(401, "Unauthorized");
    }

    let Some(body) = json_body(ctx) else {
        return json_error(400, "Missing config");
    };
    let Some(object) = body.as_object() else {
        return json_error(400, "Missing config");
    };
    if object.is_empty() {
        return json_error(400, "Missing config");
    }

    let mut changed = false;
    panel.manager.update_config(|cfg| {
        for (name, value) in object {
            if apply_config_value(cfg, name, value) {
                changed = true;
            }
        }
    });

    if !changed {
        return json_error(400, "没有可更新的配置项");
    }

    util::log_info("Web 面板更新配置");
    json_result(0, "配置已更新，部分配置需重启服务后生效", None)
}

/// 修改密码：内置管理员改配置凭据；数据库用户改本人（均校验旧密码，立即生效）。
fn change_password(panel: &WebPanel, ctx: &Ctx) -> ActionResult {
    if !panel.check_auth(ctx) {
        return json_error(401, "Unauthorized");
    }
    let Some(principal) = panel.principal(ctx) else {
        return json_error(401, "Unauthorized");
    };

    let old_password = arg(ctx, "oldPassword").unwrap_or_default();
    let new_password = arg(ctx, "newPassword").unwrap_or_default();

    if old_password.is_empty() {
        return json_error(400, "Missing oldPassword");
    }
    if new_password.is_empty() {
        return json_error(400, "Missing newPassword");
    }

    if principal.is_admin {
        if old_password != panel.manager.config().web_user_password {
            return json_error(403, "Old password is incorrect");
        }
        panel
            .manager
            .update_config(|cfg| cfg.web_user_password = new_password);
        util::log_info("Web 面板密码已修改");
        return json_result(0, "密码已修改，下次登录请使用新密码", None);
    }

    match audit::change_user_password(
        panel.base(),
        &principal.name,
        &old_password,
        &new_password,
    ) {
        Ok(()) => {
            util::log_format("面板用户修改密码：{}", &[&principal.name]);
            json_result(0, "密码已修改，下次登录请使用新密码", None)
        }
        Err(e) => {
            if e.contains("旧密码") {
                json_error(403, &e)
            } else {
                json_error(500, &e)
            }
        }
    }
}

/// 读取日志（尾部若干行）。
fn logs(panel: &WebPanel, ctx: &Ctx) -> ActionResult {
    if !panel.check_auth(ctx) {
        return json_error(401, "Unauthorized");
    }

    let mut count = arg_i64(ctx, "count").unwrap_or(200);
    if count <= 0 {
        count = 200;
    }
    let count = count.min(1000) as usize;

    let file = arg(ctx, "file").unwrap_or_default();
    let level = arg(ctx, "level").unwrap_or_default();

    let log_dir = panel.base.join("Log");
    let path = if file.trim().is_empty() {
        // 最新文件：按文件名倒序（与 C# 的 OrderByDescending 一致）
        dhrust::io::latest_file_by_ext(&log_dir, ".log")
    } else {
        // 安全：只取文件名部分，防目录穿越
        match Path::new(file.trim()).file_name() {
            Some(name) => {
                let candidate = log_dir.join(name);
                if candidate.is_file() {
                    Some(candidate)
                } else {
                    None
                }
            }
            None => None,
        }
    };

    let mut lines = match &path {
        Some(p) => dhrust::io::read_tail(p, count),
        None => Vec::new(),
    };
    if !level.trim().is_empty() {
        let level = level.trim().to_string();
        lines.retain(|l| l.to_ascii_uppercase().contains(&level.to_ascii_uppercase()));
    }

    let file_name = if file.trim().is_empty() {
        "latest".to_string()
    } else {
        file.trim().to_string()
    };

    json_result(
        0,
        "",
        Some(json!({ "fileName": file_name, "count": lines.len(), "lines": lines })),
    )
}

/// 日志文件列表。
fn log_files(panel: &WebPanel, ctx: &Ctx) -> ActionResult {
    if !panel.check_auth(ctx) {
        return json_error(401, "Unauthorized");
    }

    let log_dir = panel.base.join("Log");
    let mut files: Vec<(String, u64, String)> = Vec::new();
    if let Ok(dir) = std::fs::read_dir(&log_dir) {
        for entry in dir.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.to_ascii_lowercase().ends_with(".log") {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            let modified = meta
                .modified()
                .ok()
                .map(|t| {
                    let dt: DateTime<Local> = t.into();
                    dt.format("%Y-%m-%d %H:%M:%S").to_string()
                })
                .unwrap_or_default();
            files.push((name, meta.len(), modified));
        }
    }

    // 文件名倒序（最新在前），与 C# 一致
    files.sort_by(|a, b| b.0.cmp(&a.0));

    let list: Vec<Json> = files
        .into_iter()
        .map(|(name, size, modified)| {
            json!({
                "name": name,
                "size": size,
                "sizeDisplay": sys::format_gmk(size),
                "lastModified": modified,
            })
        })
        .collect();

    json_result(0, "", Some(json!({ "files": list })))
}

/// 看门狗监控的服务状态。
fn watchdog(panel: &WebPanel, ctx: &Ctx) -> ActionResult {
    if !panel.check_auth(ctx) {
        return json_error(401, "Unauthorized");
    }

    let cfg = panel.manager.config();
    let services: Vec<Json> = cfg
        .watch_dog
        .split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|name| json!({ "name": name, "running": sys::is_process_running(name) }))
        .collect();

    json_result(0, "", Some(json!({ "services": services })))
}

/// 服务控制（启动/停止/重启代理自身）。
fn control(panel: &WebPanel, ctx: &Ctx) -> ActionResult {
    if !panel.check_auth(ctx) {
        return json_error(401, "Unauthorized");
    }

    let action = arg(ctx, "action").unwrap_or_default().to_ascii_lowercase();
    let cfg = panel.manager.config();

    match action.as_str() {
        "stop" => {
            util::log_info("Web 面板触发服务停止");
            agent::SHUTDOWN.store(true, std::sync::atomic::Ordering::SeqCst);
            json_result(0, "服务正在停止，Web面板仍可用", None)
        }
        "start" | "restart" => {
            util::log_info("Web 面板触发服务重启");
            schedule_service_restart(&cfg);
            json_result(0, "服务正在重启", None)
        }
        _ => json_error(400, &format!("Unknown action: {action}")),
    }
}

// ————— DHDeploy Agent 面板访问控制（星尘控制开关） —————

/// DHDeploy Agent（Rust）面板控制接口（其仅接受回环请求；面板默认仅本机）。
const DHDEPLOY_PANEL_API: &str = "http://127.0.0.1:8282/api/panel/access";

/// 访问模式白名单（与 DHDeploy `panel_access` 一致）。
fn dhdeploy_valid_mode(mode: &str) -> bool {
    matches!(mode, "off" | "local" | "always")
}

/// 服务访问范围白名单（与 DHDeploy `service_access` 一致）。
fn dhdeploy_valid_service(service: &str) -> bool {
    matches!(service, "remote" | "local")
}

/// 面板模式中文描述。
fn dhdeploy_mode_text(mode: &str) -> &'static str {
    match mode {
        "always" => "允许远程访问",
        "off" => "面板已关闭",
        "local" => "仅本机访问",
        _ => "未知（旧版 Agent）",
    }
}

/// 服务访问范围中文描述。
fn dhdeploy_service_text(service: &str) -> &'static str {
    match service {
        "remote" => "允许远程访问",
        "local" => "仅本机（已关闭远程）",
        _ => "未知（旧版 Agent）",
    }
}

/// 本机探测 DHDeploy Agent 面板/服务访问范围（返回 `(panel_mode, service_access)`）。
///
/// 阻塞 HTTP 经库内包装（独立线程 + join）——运行时线程内直接 `block_on` 会 panic（历史踩坑）。
fn dhdeploy_probe() -> Result<(String, String), String> {
    let resp = dhrust::net::http_client::blocking_request_offthread(
        "GET",
        DHDEPLOY_PANEL_API,
        &[],
        None,
        Vec::new(),
        std::time::Duration::from_millis(2000),
    )
    .map_err(|e| format!("未检测到 DHDeploy Agent 面板（{}）", e.0))?;
    if resp.status != 200 {
        return Err(format!("本机 8282 响应异常（HTTP {}）", resp.status));
    }
    let v: Json = serde_json::from_slice(&resp.body)
        .map_err(|_| "控制接口响应非 JSON（可能为 C# 版 Agent 或其他程序）".to_string())?;
    if v.get("code").and_then(|c| c.as_i64()) != Some(0) {
        return Err("控制接口返回异常".to_string());
    }
    let data = v.get("data").cloned().unwrap_or_default();
    let mode = data
        .get("mode")
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if !dhdeploy_valid_mode(&mode) {
        return Err("未识别的面板访问模式".to_string());
    }
    let service = data
        .get("service")
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    // 旧版 Agent 无 service 字段：返回空串（前端显示“未知（旧版 Agent）”）
    if !service.is_empty() && !dhdeploy_valid_service(&service) {
        return Err("未识别的服务访问范围".to_string());
    }
    Ok((mode, service))
}

/// 调用 DHDeploy 控制接口（POST JSON；阻塞 HTTP 经库内包装，运行时内安全）。
fn dhdeploy_post(payload: &Json) -> Result<String, String> {
    let body = payload.to_string().into_bytes();
    let resp = dhrust::net::http_client::blocking_request_offthread(
        "POST",
        DHDEPLOY_PANEL_API,
        &[],
        Some("application/json"),
        body,
        std::time::Duration::from_millis(3000),
    )
    .map_err(|e| format!("调用失败（{}）", e.0))?;
    let v: Json = serde_json::from_slice(&resp.body).unwrap_or_default();
    let code = v
        .get("code")
        .and_then(|c| c.as_i64())
        .unwrap_or(resp.status as i64);
    let message = v
        .get("message")
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .to_string();
    if code != 0 {
        return Err(if message.is_empty() {
            format!("切换失败（HTTP {}）", resp.status)
        } else {
            message
        });
    }
    Ok(message)
}

/// `POST /api/selfUpgradeCheck`：立即执行一次自动升级检查（等同平台下发的「立即检查升级」指令；
/// 检查在后台线程执行，结果见「控制」页自动升级卡片与日志）。
fn self_upgrade_check(panel: &WebPanel, ctx: &Ctx) -> ActionResult {
    if !panel.check_auth(ctx) {
        return json_error(401, "Unauthorized");
    }
    let cfg = panel.manager.config();
    if !crate::self_upgrade::enabled(&cfg) {
        return json_error(400, "未配置自动升级源（AutoUpgradeUrl），无法检查");
    }
    // 注意：先 read 到本地再 move（trigger 接收 AgentConfig 所有权；检查在线程内完成）
    let started = crate::self_upgrade::trigger(cfg, true);
    if started {
        json_result(
            0,
            "已开始检查升级：结果见自动升级卡片（发现新版本将自动完成替换重启）",
            None,
        )
    } else {
        json_error(429, "已有检查正在进行，请稍候")
    }
}

/// `GET /api/dhdeployPanel`：探测本机 DHDeploy Agent（Rust）访问范围；
/// `POST /api/dhdeployPanel`：切换——`{"mode":"off|local|always"}` 面板访问；
/// `{"service":"remote|local"}` 服务访问（热重绑监听，外部可达性）。
///
/// 注：控制器动作表按名称去重（同名后注册覆盖先注册），故 GET/POST 合并为一个动作按方法分支。
fn dhdeploy_panel(panel: &WebPanel, ctx: &Ctx) -> ActionResult {
    if !panel.check_auth(ctx) {
        return json_error(401, "Unauthorized");
    }
    if ctx.req.method.eq_ignore_ascii_case("POST") {
        let mut payload = serde_json::Map::new();
        if let Some(mode) = arg(ctx, "mode") {
            let mode = mode.trim().to_ascii_lowercase();
            if !dhdeploy_valid_mode(&mode) {
                return json_error(
                    400,
                    &format!("无效的面板访问模式：{mode}（可选 off / local / always）"),
                );
            }
            payload.insert("mode".to_string(), json!(mode));
        }
        if let Some(service) = arg(ctx, "service") {
            let service = service.trim().to_ascii_lowercase();
            if !dhdeploy_valid_service(&service) {
                return json_error(
                    400,
                    &format!("无效的服务访问范围：{service}（可选 remote / local）"),
                );
            }
            payload.insert("service".to_string(), json!(service));
        }
        if payload.is_empty() {
            return json_error(
                400,
                "缺少参数（mode = off|local|always；service = remote|local）",
            );
        }
        let message = match dhdeploy_post(&Json::Object(payload)) {
            Ok(message) => message,
            Err(e) => return json_error(500, &e),
        };
        // 切换后重探测（失败则仅回传消息）
        return match dhdeploy_probe() {
            Ok((mode, service)) => json_result(0, &message, Some(dhdeploy_state_json(&mode, &service))),
            Err(_) => json_result(0, &message, Some(json!({ "detected": true }))),
        };
    }
    match dhdeploy_probe() {
        Ok((mode, service)) => json_result(0, "", Some(dhdeploy_state_json(&mode, &service))),
        Err(e) => json_result(0, "", Some(json!({ "detected": false, "reason": e }))),
    }
}

/// 探测结果 JSON（双开关统一结构）。
fn dhdeploy_state_json(mode: &str, service: &str) -> Json {
    json!({
        "detected": true,
        "mode": mode,
        "modeText": dhdeploy_mode_text(mode),
        "service": service,
        "serviceText": dhdeploy_service_text(service),
    })
}

// ————— /star 动作 —————

/// 子服务列表。
fn services(panel: &WebPanel, ctx: &Ctx) -> ActionResult {
    if !panel.check_auth(ctx) {
        return json_error(401, "Unauthorized");
    }

    let list = panel.manager.list();
    let mut running = 0usize;
    let mut services = Vec::with_capacity(list.len());
    for (cfg, st) in &list {
        if st.running {
            running += 1;
        }
        // 运行中应用：CPU（占整机 %，按面板轮询间隔差分）与内存占用（面板“资源”列）
        let (cpu_rate, memory_mb) = if st.running && st.pid > 0 {
            (app_cpu_rate(st.pid), sys::memory_mb(st.pid))
        } else {
            (None, None)
        };
        services.push(json!({
            "Name": cfg.name,
            "FileName": cfg.file_name,
            "Arguments": cfg.arguments,
            "WorkingDirectory": cfg.working_directory,
            "Enable": cfg.enable,
            "Mode": pascal(&cfg.mode_text()),
            "MaxMemory": cfg.max_memory,
            "HealthCheck": cfg.health_check,
            "AllowMultiple": cfg.allow_multiple,
            "AutoStop": cfg.auto_stop,
            "ReloadOnChange": cfg.reload_on_change,
            "Environments": cfg.environments,
            "OomScoreAdjust": cfg.oom_score_adjust,
            "priority": "Normal",
            "UserName": cfg.user_name,
            // 资源占用（Rust 扩展字段；C# 面板反序列化忽略未知字段）
            "CpuRate": cpu_rate.map(|v| (v * 10.0).round() / 10.0),
            "MemoryMB": memory_mb,
            "Running": st.running,
            "ProcessId": st.pid,
            "ProcessName": st.process_name,
            "StartTime": st.start_time,
        }));
    }

    json_result(
        0,
        "",
        Some(json!({ "services": services, "total": list.len(), "running": running })),
    )
}

/// 子应用 CPU 占用（占整机 %，按面板轮询间隔差分；首次采样无基线返回 None，下一轮即出数）。
fn app_cpu_rate(pid: u32) -> Option<f64> {
    let total = sys::process_cpu_split(pid)?.0;
    let cores = std::thread::available_parallelism()
        .map(|v| v.get())
        .unwrap_or(1);
    let now = Instant::now();
    static APP_CPU_LAST: Mutex<Option<HashMap<u32, (Instant, f64)>>> = Mutex::new(None);
    let mut slot = APP_CPU_LAST.lock().expect("app cpu sample");
    let map = slot.get_or_insert_with(HashMap::new);
    // 清理长时间未刷新的基线（应用已退出/不再上报）
    map.retain(|_, (t, _)| now.duration_since(*t) < Duration::from_secs(600));
    let pct = map.get(&pid).map(|(t0, c0)| {
        dhrust::sys::monitor::process_cpu_percent(
            (total - c0).max(0.0),
            now.duration_since(*t0).as_secs_f64(),
            cores,
        )
    });
    map.insert(pid, (now, total));
    pct
}

/// 子服务操作类型。
#[derive(Clone, Copy)]
enum Op {
    /// 启动
    Start,
    /// 停止
    Stop,
    /// 重启
    Restart,
}

/// 子服务启停重启。
fn service_op(panel: &WebPanel, ctx: &Ctx, op: Op) -> ActionResult {
    if !panel.check_auth(ctx) {
        return json_error(401, "Unauthorized");
    }

    let name = arg(ctx, "serviceName").unwrap_or_default();
    if name.trim().is_empty() {
        return json_error(400, "服务名称不能为空");
    }

    let (code, message) = match op {
        Op::Start => match panel.manager.start_app(&name) {
            Ok(true) => (0, "服务启动成功".to_string()),
            Ok(false) => (0, "服务已在运行".to_string()),
            Err(e) if e.starts_with("服务不存在") => (1, "服务启动失败或服务不存在".to_string()),
            Err(e) => (1, format!("启动服务时发生错误: {e}")),
        },
        Op::Stop => match panel.manager.stop_app(&name, "Web面板调用停止") {
            Ok(true) => (0, "服务停止成功".to_string()),
            Ok(false) => (1, "服务停止失败".to_string()),
            Err(e) if e.starts_with("服务不存在") => (1, "服务停止失败或服务不存在".to_string()),
            Err(e) => (1, format!("停止服务时发生错误: {e}")),
        },
        Op::Restart => match panel.manager.restart_app(&name, "Web面板调用重启") {
            Ok(true) => (0, "服务重启成功".to_string()),
            Ok(false) => (1, "服务重启失败：启动服务失败".to_string()),
            Err(e) if e.starts_with("服务不存在") => (1, "服务不存在".to_string()),
            Err(e) => (1, format!("重启服务时发生错误: {e}")),
        },
    };

    json_result(code, &message, Some(json!({ "serviceName": name })))
}

/// 新增或更新子服务。
fn add_service(panel: &WebPanel, ctx: &Ctx) -> ActionResult {
    if !panel.check_auth(ctx) {
        return json_error(401, "Unauthorized");
    }

    let Some(body) = json_body(ctx) else {
        return json_error(400, "服务名称不能为空");
    };

    let name = field(&body, "name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if name.is_empty() {
        return json_error(400, "服务名称不能为空");
    }

    // 已有同名配置时在其基础上合并（保留表单未覆盖的字段，如 Debug/Overwrite）
    let existing = panel.manager.config().find_app(&name).cloned();
    let mut app = existing.unwrap_or_default();

    app.name = name.clone();
    if let Some(v) = field(&body, "fileName").and_then(|v| v.as_str()) {
        app.file_name = v.trim().to_string();
    }
    app.arguments = field(&body, "arguments")
        .and_then(|v| v.as_str())
        .map(|v| v.to_string());
    app.working_directory = field(&body, "workingDirectory")
        .and_then(|v| v.as_str())
        .map(|v| v.to_string());
    if let Some(v) = field(&body, "mode").and_then(|v| v.as_str()) {
        app.mode = v.trim().to_ascii_lowercase();
    }
    if let Some(v) = field(&body, "maxMemory").and_then(|v| v.as_u64()) {
        app.max_memory = v as u32;
    }
    app.health_check = field(&body, "healthCheck")
        .and_then(|v| v.as_str())
        .map(|v| v.to_string());
    app.environments = field(&body, "environments")
        .and_then(|v| v.as_str())
        .map(|v| v.to_string());
    if let Some(v) = field(&body, "enable").and_then(|v| v.as_bool()) {
        app.enable = v;
    }
    if let Some(v) = field(&body, "autoStop").and_then(|v| v.as_bool()) {
        app.auto_stop = v;
    }
    if let Some(v) = field(&body, "reloadOnChange").and_then(|v| v.as_bool()) {
        app.reload_on_change = v;
    }
    if let Some(v) = field(&body, "allowMultiple").and_then(|v| v.as_bool()) {
        app.allow_multiple = v;
    }

    panel.manager.upsert_app(app);

    json_result(
        0,
        &format!("服务 [{name}] 已添加/更新"),
        Some(json!({ "serviceName": name })),
    )
}

/// 删除子服务。
fn remove_service(panel: &WebPanel, ctx: &Ctx) -> ActionResult {
    if !panel.check_auth(ctx) {
        return json_error(401, "Unauthorized");
    }

    let name = arg(ctx, "serviceName").unwrap_or_default();
    if name.trim().is_empty() {
        return json_error(400, "服务名称不能为空");
    }

    panel.manager.remove_app(&name);

    json_result(
        0,
        &format!("服务 [{name}] 已删除"),
        Some(json!({ "serviceName": name })),
    )
}

/// 星尘配置（分组展示）。
fn get_star_config(panel: &WebPanel, ctx: &Ctx) -> ActionResult {
    if !panel.check_auth(ctx) {
        return json_error(401, "Unauthorized");
    }

    let cfg = panel.manager.config();
    let groups = json!([
        {
            "group": "星尘连接",
            "items": [
                {
                    "name": "Server", "displayName": "服务端地址", "value": cfg.server,
                    "type": "String",
                    "description": "星尘服务端地址（当前版本尚未对接注册中心，保留字段）"
                }
            ]
        },
        {
            "group": "StarAgent 本地配置",
            "items": [
                {
                    "name": "LocalPort", "displayName": "本地端口(UDP)", "value": cfg.local_port,
                    "type": "Int32",
                    "description": "本地API通信端口（UDP），默认5501（与 C# 版 StarAgent 5500 错开）。与 Web 面板端口共用，UDP 用于本地 RPC，TCP 用于 Web 面板"
                },
                {
                    "name": "Project", "displayName": "项目名", "value": cfg.project,
                    "type": "String", "description": "新节点默认所要加入的项目"
                },
                {
                    "name": "StartupHook", "displayName": "启动挂钩", "value": cfg.startup_hook,
                    "type": "Boolean", "description": "拉起目标进程时对dotNet应用注入星尘监控钩子"
                },
                {
                    "name": "Delay", "displayName": "延迟时间(ms)", "value": cfg.delay,
                    "type": "Int32", "description": "重启进程或服务的延迟时间，默认3000ms"
                }
            ]
        }
    ]);

    json_result(0, "", Some(json!({ "groups": groups })))
}

/// 上传升级：请求体即新版本程序文件（原始二进制），校验后影子冒烟并原子替换，随后自动重启。
///
/// 体验对齐 1Panel：面板选文件上传即可，服务端完成"暂存 → 校验 → 影子自检 → 替换 → 退出重拉"，
/// 无需用户手动改名或停服。
fn upgrade(panel: &WebPanel, ctx: &Ctx) -> ActionResult {
    if !panel.check_auth(ctx) {
        return json_error(401, "Unauthorized");
    }

    let body = &ctx.req.body;
    // 上传上限（与 HTTP 服务器默认 body 上限一致）
    const MAX_SIZE: usize = 64 * 1024 * 1024;
    if body.len() < 1024 {
        return json_error(400, "升级文件过小或为空，请上传完整的可执行文件");
    }
    if body.len() > MAX_SIZE {
        return json_error(400, "升级文件过大（上限 64MB）");
    }
    let is_elf = body.starts_with(b"\x7FELF");
    let is_pe = body.starts_with(b"MZ");
    if !is_elf && !is_pe {
        return json_error(400, "文件格式校验失败：不是可执行程序（ELF/PE）");
    }

    let Ok(current) = std::env::current_exe() else {
        return json_error(500, "无法定位当前程序文件");
    };
    let exe = util::lexical_normalize(&current);
    let staged = PathBuf::from(format!("{}.new", exe.display()));
    if let Err(e) = std::fs::write(&staged, body.as_ref()) {
        return json_error(500, &format!("写入升级文件失败：{e}"));
    }

    match agent::apply_upgrade(&staged, &exe, 0) {
        Ok(()) => {
            util::log_info(
                "Web 面板上传升级：影子自检通过，程序文件已替换，即将退出等待服务管理器拉起新版本……",
            );
            // 不依赖服务管理器的失败恢复策略：由新版本进程显式确保服务运行
            agent::schedule_service_restart(&exe);
            // 异步文件日志同步落盘后再退出（否则最后一条日志可能在队列中丢失）
            dhrust::logs::flush();
            // 延迟退出：确保本次 HTTP 响应先送达浏览器，再由服务管理器拉起新版本
            std::thread::spawn(|| {
                std::thread::sleep(Duration::from_millis(1000));
                std::process::exit(0);
            });
            json_result(
                0,
                "升级完成：影子自检通过，程序文件已替换，服务即将自动重启",
                None,
            )
        }
        Err(e) => {
            // 失败清理暂存文件（错误原因已告知，避免残留文件触发周期性重试）
            let _ = std::fs::remove_file(&staged);
            json_error(500, &format!("升级失败（当前程序保持不变）：{e}"))
        }
    }
}

/// 更新星尘配置（安全白名单）。
fn update_star_config(panel: &WebPanel, ctx: &Ctx) -> ActionResult {
    if !panel.check_auth(ctx) {
        return json_error(401, "Unauthorized");
    }

    let Some(body) = json_body(ctx) else {
        return json_error(400, "没有需要更新的配置项");
    };
    let Some(object) = body.as_object() else {
        return json_error(400, "没有需要更新的配置项");
    };
    if object.is_empty() {
        return json_error(400, "没有需要更新的配置项");
    }

    let mut changed = false;
    panel.manager.update_config(|cfg| {
        for (name, value) in object {
            let ok = match name.to_ascii_lowercase().as_str() {
                "server" => set_string(value, |s| cfg.server = s),
                "project" => set_string(value, |s| cfg.project = s),
                "startuphook" => set_bool(value, |b| cfg.startup_hook = b),
                "localport" => set_port(value, |p| cfg.local_port = p),
                "delay" => set_u64(value, |v| cfg.delay = v),
                _ => false,
            };
            if ok {
                changed = true;
            }
        }
    });

    if !changed {
        return json_error(400, "没有需要更新的配置项");
    }

    util::log_info("Web 面板更新星尘配置");
    json_result(0, "配置已保存到: StarAgent.config", None)
}

/// 本机详细信息。
fn machine(panel: &WebPanel, ctx: &Ctx) -> ActionResult {
    if !panel.check_auth(ctx) {
        return json_error(401, "Unauthorized");
    }

    let _ = panel;
    let cpu_count = std::thread::available_parallelism()
        .map(|v| v.get())
        .unwrap_or(0);
    let cpu_rate = sampler::current().cpu_rate;
    let (mem_total, mem_avail) = sys::memory_info().unwrap_or((0, 0));
    let mem_used = mem_total.saturating_sub(mem_avail);
    let mem_rate = if mem_total > 0 {
        mem_used as f64 / mem_total as f64
    } else {
        0.0
    };

    let drives: Vec<Json> = sys::disks()
        .into_iter()
        .filter(|d| d.ready)
        .map(|d| {
            let used = d.total.saturating_sub(d.free);
            let rate = if d.total > 0 {
                used as f64 / d.total as f64
            } else {
                0.0
            };
            json!({
                "name": d.name,
                "label": d.label,
                "format": d.format,
                "totalSize": sys::format_gmk(d.total),
                "usedSize": sys::format_gmk(used),
                "freeSize": sys::format_gmk(d.free),
                "usedPercent": format!("{:.1}%", rate * 100.0),
                "totalMB": d.total / 1024 / 1024,
                "usedMB": used / 1024 / 1024,
                "freeMB": d.free / 1024 / 1024,
            })
        })
        .collect();

    let nics: Vec<Json> = sys::network_interfaces()
        .into_iter()
        .map(|n| {
            json!({
                "name": n.name,
                "description": n.description,
                "ip": n.ips.first().cloned().unwrap_or_default(),
                "mac": n.mac,
                "type": "",
                "speed": if n.speed_mbps > 0 { format!("{} Mbps", n.speed_mbps) } else { String::new() },
                "operationalStatus": if n.up { "Up" } else { "Down" },
                "bytesReceived": format_bytes(n.bytes_received),
                "bytesSent": format_bytes(n.bytes_sent),
            })
        })
        .collect();

    let processes: Vec<Json> = sys::top_processes(15, false)
        .into_iter()
        .map(|p| {
            json!({
                "name": p.name,
                "pid": p.pid,
                "memoryMB": p.memory_mb.to_string(),
                "cpuTime": format!("{:.1}s", p.cpu_seconds),
                "threadCount": p.threads,
                "handleCount": 0,
                "startTime": "",
            })
        })
        .collect();

    let os = json!({
        "os": format!("{} {}", sys::os_description(), std::env::consts::ARCH),
        "platform": platform_name(),
        "hostName": sys::hostname(),
        "userName": sys::user_name(),
        "processorCount": cpu_count,
        "tickCount": sys::host_uptime_seconds(),
        "hostUptime": dhrust::sys::process::format_uptime(Duration::from_secs(sys::host_uptime_seconds())),
        "runtime": format!("Pek.RAgent v{}（Rust）", env!("CARGO_PKG_VERSION")),
        "processArch": arch_name(),
        "systemArch": arch_name(),
    });

    let cpu = json!({
        "cpuName": sys::cpu_model().unwrap_or_default(),
        "cpuCount": cpu_count,
        "cpuRate": cpu_rate.map(|v| (v * 10.0).round() / 100.0).unwrap_or(0.0),
        "cpuRatePercent": cpu_rate.map(|v| format!("{v:.1}%")).unwrap_or_default(),
    });

    let memory = json!({
        "totalMemory": sys::format_gmk(mem_total),
        "usedMemory": sys::format_gmk(mem_used),
        "availableMemory": sys::format_gmk(mem_avail),
        "memoryRate": (mem_rate * 10.0).round() / 10.0,
        "memoryRatePercent": format!("{:.1}%", mem_rate * 100.0),
        "totalMB": mem_total / 1024 / 1024,
        "usedMB": mem_used / 1024 / 1024,
        "availableMB": mem_avail / 1024 / 1024,
    });

    json_result(
        0,
        "",
        Some(json!({
            "os": os,
            "cpu": cpu,
            "memory": memory,
            "drives": drives,
            "nics": nics,
            "processes": processes,
        })),
    )
}

/// Top 进程列表。
fn get_process_list(panel: &WebPanel, ctx: &Ctx) -> ActionResult {
    if !panel.check_auth(ctx) {
        return json_error(401, "Unauthorized");
    }

    let sort = arg(ctx, "sort").unwrap_or_else(|| "memory".to_string());
    let count = arg_i64(ctx, "count").unwrap_or(10).clamp(1, 200) as usize;
    let by_cpu = sort.eq_ignore_ascii_case("cpu");

    let processes: Vec<Json> = sys::top_processes(count, by_cpu)
        .into_iter()
        .map(|p| {
            json!({
                "name": p.name,
                "pid": p.pid,
                "memoryMB": p.memory_mb.to_string(),
                "cpuSeconds": format!("{:.1}", p.cpu_seconds),
                "threadCount": p.threads,
                "handleCount": 0,
                "startTime": "",
            })
        })
        .collect();

    let total = processes.len();
    json_result(0, "", Some(json!({ "processes": processes, "total": total })))
}

// ————— 主体信息 / 用户管理 / 操作日志 —————

/// 当前登录主体（前端据此渲染菜单与权限；登录后即取）。
fn me(panel: &WebPanel, ctx: &Ctx) -> ActionResult {
    let Some(principal) = panel.principal(ctx) else {
        return json_error(401, "Unauthorized");
    };
    let perms: Vec<&str> = if principal.is_admin {
        audit::ALL_PERMISSIONS.iter().map(|(k, _)| *k).collect()
    } else {
        principal.perms.iter().map(|s| s.as_str()).collect()
    };
    json_result(
        0,
        "",
        Some(json!({
            "user": principal.name,
            "isAdmin": principal.is_admin,
            "perms": perms,
            "permissions": audit::ALL_PERMISSIONS
                .iter()
                .map(|(k, n)| json!({ "key": k, "name": n }))
                .collect::<Vec<_>>(),
        })),
    )
}

/// 面板用户列表（含内置管理员虚拟条目与可选权限清单；仅内置管理员）。
fn user_list(panel: &WebPanel, ctx: &Ctx) -> ActionResult {
    if !panel.check_auth(ctx) {
        return json_error(401, "Unauthorized");
    }
    match audit::list_users_json(panel.base()) {
        Ok(mut data) => {
            // 内置管理员（配置文件凭据）以虚拟条目展示在首位：全部权限、不可删除、可改密码
            let cfg = panel.manager.config();
            let keys: Vec<&str> = audit::ALL_PERMISSIONS.iter().map(|(k, _)| *k).collect();
            let names: Vec<&str> = audit::ALL_PERMISSIONS.iter().map(|(_, n)| *n).collect();
            let admin = json!({
                "id": 0,
                "userName": cfg.web_user_name.trim(),
                "permissions": keys,
                "permissionNames": names,
                "enabled": true,
                "remark": "配置文件凭据",
                "isBuiltin": true,
            });
            if let Some(list) = data.get_mut("users").and_then(|v| v.as_array_mut()) {
                list.insert(0, admin);
            }
            json_result(0, "", Some(data))
        }
        Err(e) => json_error(500, &e),
    }
}

/// 保存面板用户（新增须密码；编辑密码留空 = 不变；仅内置管理员）。
fn user_save(panel: &WebPanel, ctx: &Ctx) -> ActionResult {
    if !panel.check_auth(ctx) {
        return json_error(401, "Unauthorized");
    }
    let Some(body) = json_body(ctx) else {
        return json_error(400, "缺少请求体");
    };
    let name = body
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let password = body
        .get("password")
        .and_then(|v| v.as_str())
        .map(String::from);
    let enabled = body
        .get("enabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let remark = body
        .get("remark")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let permissions: Vec<String> = body
        .get("permissions")
        .and_then(|v| v.as_array())
        .map(|list| {
            list.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    let cfg = panel.manager.config();
    if name.eq_ignore_ascii_case(cfg.web_user_name.trim()) {
        // 内置管理员：可改用户名与/或密码（写入配置文件凭据；当前操作者即管理员本人）
        let new_name = body
            .get("newName")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .unwrap_or_else(|| name.clone());
        let pw = password
            .as_deref()
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(String::from);
        let renamed = !new_name.eq_ignore_ascii_case(cfg.web_user_name.trim());
        if !renamed && pw.is_none() {
            return json_error(400, "未修改任何内容");
        }
        if renamed {
            match audit::find_user(panel.base(), &new_name) {
                Ok(Some(_)) => return json_error(400, "用户名已存在"),
                Ok(None) => {}
                Err(e) => return json_error(500, &e),
            }
        }
        let saved_name = new_name.clone();
        panel.manager.update_config(|cfg| {
            if renamed {
                cfg.web_user_name = saved_name.clone();
            }
            if let Some(p) = &pw {
                cfg.web_user_password = p.clone();
            }
        });
        if renamed {
            panel.rename_session(ctx, &new_name);
        }
        util::log_info("Web 面板内置管理员凭据已更新（用户管理）");
        return json_result(0, "内置管理员已更新", None);
    }
    match audit::save_user(
        panel.base(),
        &name,
        password.as_deref(),
        &permissions,
        enabled,
        &remark,
    ) {
        Ok(()) => {
            util::log_format("面板用户已保存：{}", &[&name]);
            json_result(0, "已保存", None)
        }
        Err(e) => json_error(400, &e),
    }
}

/// 删除面板用户（仅内置管理员）。
fn user_delete(panel: &WebPanel, ctx: &Ctx) -> ActionResult {
    if !panel.check_auth(ctx) {
        return json_error(401, "Unauthorized");
    }
    let name = arg(ctx, "name").unwrap_or_default();
    if name.trim().is_empty() {
        return json_error(400, "缺少用户名");
    }
    let cfg = panel.manager.config();
    if name.trim().eq_ignore_ascii_case(cfg.web_user_name.trim()) {
        return json_error(400, "内置管理员不能删除");
    }
    match audit::delete_user(panel.base(), &name) {
        Ok(()) => {
            util::log_format("面板用户已删除：{}", &[&name]);
            json_result(0, "已删除", None)
        }
        Err(e) => json_error(400, &e),
    }
}

/// 操作日志分页查询（审计页；`page/size/user/q/success` 筛选）。
fn audit_logs(panel: &WebPanel, ctx: &Ctx) -> ActionResult {
    if !panel.check_auth(ctx) {
        return json_error(401, "Unauthorized");
    }
    let page: usize = arg(ctx, "page")
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(1);
    let size: usize = arg(ctx, "size")
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(50);
    let user = arg(ctx, "user").unwrap_or_default();
    let q = arg(ctx, "q").unwrap_or_default();
    let success = match arg(ctx, "success").unwrap_or_default().trim() {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    };
    match audit::query_logs(panel.base(), page, size, &user, &q, success) {
        Ok(data) => json_result(0, "", Some(data)),
        Err(e) => json_error(500, &e),
    }
}

// ————— 辅助 —————

/// 平台名（与 C# `Runtime.Windows/Linux/OSX` 输出一致）。
fn platform_name() -> &'static str {
    if cfg!(windows) {
        "Windows"
    } else if cfg!(target_os = "macos") {
        "macOS"
    } else if cfg!(target_os = "linux") {
        "Linux"
    } else {
        "Other"
    }
}

/// 架构名（对齐 .NET `Architecture` 枚举文本）。
fn arch_name() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "X64",
        "x86" => "X86",
        "aarch64" => "Arm64",
        "arm" => "Arm",
        other => other,
    }
}

/// 首字母大写（`shadow` → `Shadow`，对齐 C# 枚举文本）。
fn pascal(text: &str) -> String {
    match text.chars().next() {
        Some(first) => {
            let mut out = String::with_capacity(text.len());
            out.extend(first.to_uppercase());
            out.push_str(&text[first.len_utf8()..]);
            out
        }
        None => String::new(),
    }
}

/// 构建配置元数据项。
fn config_item(name: &str, display: &str, kind: &str, value: String, description: &str) -> Json {
    let value = match kind {
        "Int32" => value.parse::<i64>().map(Json::from).unwrap_or(Json::Null),
        "Boolean" => Json::from(value.eq_ignore_ascii_case("true")),
        _ => Json::from(value),
    };
    json!({
        "name": name,
        "displayName": display,
        "type": kind,
        "value": value,
        "description": description,
    })
}

/// 大小写不敏感地取 JSON 对象字段。
fn field<'a>(object: &'a Json, name: &str) -> Option<&'a Json> {
    object
        .as_object()?
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value)
}

/// JSON 值转字符串（兼容数字/布尔文本值）。
fn value_string(value: &Json) -> Option<String> {
    match value {
        Json::String(text) => Some(text.clone()),
        Json::Number(number) => Some(number.to_string()),
        Json::Bool(flag) => Some(flag.to_string()),
        _ => None,
    }
}

/// JSON 值转 u64（兼容字符串数字）。
fn value_u64(value: &Json) -> Option<u64> {
    match value {
        Json::Number(number) => number.as_u64(),
        Json::String(text) => text.trim().parse().ok(),
        _ => None,
    }
}

/// JSON 值转布尔（兼容字符串 `true/false`）。
fn value_bool(value: &Json) -> Option<bool> {
    match value {
        Json::Bool(flag) => Some(*flag),
        Json::String(text) => match text.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" => Some(true),
            "false" | "0" | "no" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

/// 应用字符串配置项。
fn set_string(value: &Json, f: impl FnOnce(String)) -> bool {
    match value_string(value) {
        Some(text) => {
            f(text);
            true
        }
        None => false,
    }
}

/// 应用布尔配置项。
fn set_bool(value: &Json, f: impl FnOnce(bool)) -> bool {
    match value_bool(value) {
        Some(flag) => {
            f(flag);
            true
        }
        None => false,
    }
}

/// 应用 u64 配置项。
fn set_u64(value: &Json, f: impl FnOnce(u64)) -> bool {
    match value_u64(value) {
        Some(v) => {
            f(v);
            true
        }
        None => false,
    }
}

/// 应用端口配置项（1~65535）。
fn set_port(value: &Json, f: impl FnOnce(u16)) -> bool {
    match value_u64(value) {
        Some(v) if (1..=65535).contains(&v) => {
            f(v as u16);
            true
        }
        _ => false,
    }
}

/// 应用面板配置值（`/api/updateConfig` 白名单）。
fn apply_config_value(cfg: &mut AgentConfig, name: &str, value: &Json) -> bool {
    match name.to_ascii_lowercase().as_str() {
        "webusername" => set_string(value, |s| cfg.web_user_name = s),
        "webauthlevel" => set_string(value, |s| cfg.web_auth_level = s),
        "localport" => set_port(value, |p| cfg.local_port = p),
        "localonly" => set_bool(value, |b| cfg.local_only = b),
        "startwait" => set_u64(value, |v| cfg.start_wait = v),
        "maxfails" => match value_u64(value) {
            Some(v) if v <= i32::MAX as u64 => {
                cfg.max_fails = v as i32;
                true
            }
            _ => false,
        },
        "guardperiod" => set_u64(value, |v| cfg.guard_period = v),
        "sampleinterval" => set_u64(value, |v| cfg.sample_interval = v),
        "webtraffic" => set_bool(value, |b| cfg.web_traffic = b),
        "weblogs" => set_string(value, |s| cfg.web_logs = s),
        "porttraffic" => set_bool(value, |b| cfg.port_traffic = b),
        "porttrafficports" => set_string(value, |s| cfg.port_traffic_ports = s),
        "traffichistorydays" => match value_u64(value) {
            Some(v) if v <= crate::history::MAX_RETENTION_DAYS as u64 => {
                cfg.traffic_history_days = v as u32;
                true
            }
            _ => false,
        },
        "logcleanuppaths" => set_string(value, |s| cfg.log_cleanup_paths = s),
        "pluginstoreurl" => set_string(value, |s| cfg.plugin_store_url = s),
        "pluginstorepubkey" => set_string(value, |s| cfg.plugin_store_pubkey = s),
        "autoupgradeurl" => set_string(value, |s| cfg.auto_upgrade_url = s),
        "autoupgradetoken" => set_string(value, |s| cfg.auto_upgrade_token = s),
        "autoupgradeintervalminutes" => match value_u64(value) {
            Some(v) if (5..=1440).contains(&v) => {
                cfg.auto_upgrade_interval_minutes = v as u32;
                true
            }
            _ => false,
        },
        "aienabled" => set_bool(value, |b| cfg.ai_enabled = b),
        "aibaseurl" => set_string(value, |s| cfg.ai_base_url = s),
        "aimodel" => set_string(value, |s| cfg.ai_model = s),
        "aiapikey" => set_string(value, |s| cfg.ai_api_key = s),
        "terminalenabled" => set_bool(value, |b| cfg.terminal_enabled = b),
        "debug" => set_bool(value, |b| cfg.debug = b),
        _ => false,
    }
}

/// 计划服务重启：分离进程延迟 2 秒后停止并启动系统服务。
///
/// 面板响应先返回；Windows 用 `sc stop/start`，Linux 用 `systemctl restart`。
/// 服务未安装（前台运行时）命令会失败但无影响（后台分离，静默）。
fn schedule_service_restart(cfg: &AgentConfig) {
    let name = cfg.service_name.clone();

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const DETACHED_PROCESS: u32 = 0x0000_0008;

        let script = format!(
            "ping -n 2 127.0.0.1 >nul & sc stop \"{name}\" & ping -n 3 127.0.0.1 >nul & sc start \"{name}\""
        );
        match std::process::Command::new("cmd")
            .args(["/c", &script])
            .creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS)
            .spawn()
        {
            Ok(_) => util::log_format("已安排服务重启（{}）", &[&name]),
            Err(e) => util::log_error(&format!("安排服务重启失败：{}", e)),
        }
    }

    #[cfg(not(windows))]
    {
        let script = format!("sleep 2; systemctl restart '{name}'");
        match std::process::Command::new("sh").args(["-c", &script]).spawn() {
            Ok(_) => util::log_format("已安排服务重启（{}）", &[&name]),
            Err(e) => util::log_error(&format!("安排服务重启失败：{}", e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dhrust::net::http::HttpRequest;

    #[test]
    fn dhdeploy_mode_helpers() {
        assert!(dhdeploy_valid_mode("local"));
        assert!(dhdeploy_valid_mode("always"));
        assert!(dhdeploy_valid_mode("off"));
        assert!(!dhdeploy_valid_mode("everyone"));
        assert!(!dhdeploy_valid_mode(""));
        assert_eq!(dhdeploy_mode_text("local"), "仅本机访问");
        assert_eq!(dhdeploy_mode_text("always"), "允许远程访问");
        assert_eq!(dhdeploy_mode_text("off"), "面板已关闭");
        assert!(dhdeploy_valid_service("remote"));
        assert!(dhdeploy_valid_service("local"));
        assert!(!dhdeploy_valid_service("off"));
        assert!(!dhdeploy_valid_service("everyone"));
        assert_eq!(dhdeploy_service_text("remote"), "允许远程访问");
        assert_eq!(dhdeploy_service_text("local"), "仅本机（已关闭远程）");
        assert_eq!(dhdeploy_service_text(""), "未知（旧版 Agent）");
    }

    /// 构造来自远程地址的测试上下文（默认配置下需令牌的场景）。
    fn context(method: &str, path: &str, body: &str, token: Option<&str>) -> Ctx {
        context_from("192.168.1.100:50000", method, path, body, token)
    }

    /// 构造指定来源地址的测试上下文。
    fn context_from(remote: &str, method: &str, path: &str, body: &str, token: Option<&str>) -> Ctx {
        let headers = match token {
            Some(token) => vec![("Authorization".to_string(), format!("Bearer {token}"))],
            None => Vec::new(),
        };
        Ctx::build(HttpRequest {
            method: method.to_string(),
            path: path.to_string(),
            query: String::new(),
            headers,
            body: body.to_string().into(),
            remote_addr: Some(remote.to_string()),
        })
    }

    /// 构造二进制请求体的测试上下文（数据库还原上传用）。
    fn context_bytes(method: &str, path: &str, body: Vec<u8>, token: Option<&str>) -> Ctx {
        let headers = match token {
            Some(token) => vec![("Authorization".to_string(), format!("Bearer {token}"))],
            None => Vec::new(),
        };
        Ctx::build(HttpRequest {
            method: method.to_string(),
            path: path.to_string(),
            query: String::new(),
            headers,
            body: body.into(),
            remote_addr: Some("192.168.1.100:50000".to_string()),
        })
    }

    /// 构建独立临时目录的面板。
    fn panel_with_default_password() -> (Arc<WebPanel>, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "ragent-panel-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        let cfg = AgentConfig::default();
        let manager = AppManager::new(&dir, cfg);
        let panel = WebPanel::new(manager, &dir, 5501);
        (panel, dir)
    }

    /// 取出信封 JSON。
    fn body_json(action: ActionResult) -> Json {
        match action {
            ActionResult::Response(response) => {
                serde_json::from_slice(&response.body).expect("响应应为 JSON")
            }
            _ => panic!("应为响应"),
        }
    }

    #[test]
    fn login_issues_token_and_rejects_wrong_password() {
        let (panel, dir) = panel_with_default_password();

        // 错误密码
        let bad = login(
            &panel,
            &context("POST", "/api/login", r#"{"user":"admin","password":"x"}"#, None),
        );
        let bad = body_json(bad);
        assert_eq!(bad["code"], 401);
        assert_eq!(bad["message"], "Invalid credentials");

        // 正确密码
        let ok = login(
            &panel,
            &context(
                "POST",
                "/api/login",
                r#"{"user":"admin","password":"admin"}"#,
                None,
            ),
        );
        let ok = body_json(ok);
        assert_eq!(ok["code"], 0);
        let token = ok["data"]["token"].as_str().unwrap().to_string();
        assert!(!token.is_empty());

        // 令牌生效
        assert!(panel.validate_token(&token));
        let status = status(&panel, &context("GET", "/api/status", "", Some(&token)));
        let status = body_json(status);
        assert_eq!(status["code"], 0);
        assert_eq!(status["data"]["port"], 5501);
        assert_eq!(status["data"]["running"], true);

        // 注销后失效
        let _ = logout(&panel, &context("POST", "/api/logout", "", Some(&token)));
        assert!(!panel.validate_token(&token));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn traffic_endpoints_require_auth() {
        let (panel, dir) = panel_with_default_password();

        // 未鉴权：拒绝
        let r = web_traffic(&panel, &context("GET", "/star/webTraffic", "", None));
        assert_eq!(body_json(r)["code"], 401);
        let r = port_traffic(&panel, &context("GET", "/star/portTraffic", "", None));
        assert_eq!(body_json(r)["code"], 401);
        let r = traffic_history(&panel, &context("GET", "/star/trafficHistory", "", None));
        assert_eq!(body_json(r)["code"], 401);

        // 鉴权后：返回快照信封（模块未启动时为占位数据，但结构完整）
        let ok = login(
            &panel,
            &context("POST", "/api/login", r#"{"user":"admin","password":"admin"}"#, None),
        );
        let token = body_json(ok)["data"]["token"].as_str().unwrap().to_string();

        let j = body_json(web_traffic(
            &panel,
            &context("GET", "/star/webTraffic", "", Some(&token)),
        ));
        assert_eq!(j["code"], 0);
        assert!(j["data"].get("enabled").is_some());
        assert!(j["data"].get("sites").is_some());

        let j = body_json(port_traffic(
            &panel,
            &context("GET", "/star/portTraffic", "", Some(&token)),
        ));
        assert_eq!(j["code"], 0);
        assert!(j["data"].get("ports").is_some());
        assert!(j["data"].get("mode").is_some());

        let j = body_json(traffic_history(
            &panel,
            &context("GET", "/star/trafficHistory", "", Some(&token)),
        ));
        assert_eq!(j["code"], 0);
        assert!(j["data"].get("days").is_some());
        assert!(j["data"].get("retentionDays").is_some());

        // 释放 SQLite 连接后再删除临时目录（Windows 下打开的文件句柄会阻止删除）
        crate::history::drop_storage_for_test(&dir);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn upgrade_rejects_invalid_payloads_and_keeps_program() {
        let (panel, dir) = panel_with_default_password();

        let ok = login(
            &panel,
            &context("POST", "/api/login", r#"{"user":"admin","password":"admin"}"#, None),
        );
        let ok = body_json(ok);
        assert_eq!(ok["code"], 0);
        let token = ok["data"]["token"].as_str().unwrap().to_string();

        // 未鉴权：拒绝
        let r = upgrade(&panel, &context("POST", "/api/upgrade", "x", None));
        assert_eq!(body_json(r)["code"], 401);

        // 过小：拒绝
        let r = upgrade(&panel, &context("POST", "/api/upgrade", "tiny", Some(&token)));
        assert_eq!(body_json(r)["code"], 400);

        // 非可执行格式：拒绝
        let big = "x".repeat(4096);
        let r = upgrade(&panel, &context("POST", "/api/upgrade", &big, Some(&token)));
        let j = body_json(r);
        assert_eq!(j["code"], 400);
        assert!(j["message"].as_str().unwrap().contains("格式"));

        // 形态合法但无法运行（假 ELF）：影子自检拦截，不得替换当前程序
        let mut fake = vec![0x7Fu8, b'E', b'L', b'F'];
        fake.extend_from_slice(&[b'x'; 4096]);
        let fake = String::from_utf8_lossy(&fake).to_string();
        let r = upgrade(&panel, &context("POST", "/api/upgrade", &fake, Some(&token)));
        let j = body_json(r);
        assert_eq!(j["code"], 500);
        assert!(j["message"].as_str().unwrap().contains("升级失败"));

        // 升级失败不得残留暂存文件
        let staged = PathBuf::from(format!(
            "{}.new",
            util::lexical_normalize(&std::env::current_exe().unwrap()).display()
        ));
        assert!(!staged.exists(), "失败后应清理暂存文件");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rate_limiter_blocks_after_failures() {
        let (panel, dir) = panel_with_default_password();

        // 默认策略 5 次失败封禁（dhrust::net::login_guard）
        for _ in 0..5 {
            let result = login(
                &panel,
                &context("POST", "/api/login", r#"{"user":"admin","password":"bad"}"#, None),
            );
            assert_eq!(body_json(result)["code"], 401);
        }

        // 已达上限：即使密码正确也被拒绝
        let blocked = login(
            &panel,
            &context(
                "POST",
                "/api/login",
                r#"{"user":"admin","password":"admin"}"#,
                None,
            ),
        );
        assert_eq!(body_json(blocked)["code"], 429);

        // 成功记录会清除限流（模拟另一 IP）
        panel.logins.record_success("10.0.0.1");
        assert!(!panel.logins.is_blocked("10.0.0.1"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unauthorized_endpoints_require_token() {
        let (panel, dir) = panel_with_default_password();

        let result = status(&panel, &context("GET", "/api/status", "", None));
        assert_eq!(body_json(result)["code"], 401);

        let result = services(&panel, &context("GET", "/star/services", "", None));
        assert_eq!(body_json(result)["code"], 401);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sync_time_validates_payload_and_auth() {
        let (panel, dir) = panel_with_default_password();
        let token = panel.issue_token("admin", "admin").unwrap();

        // 未登录：拒绝
        let result = sync_time(
            &panel,
            &context("POST", "/api/syncTime", r#"{"timeMs":946684800000}"#, None),
        );
        assert_eq!(body_json(result)["code"], 401);

        // 缺少 timeMs：400
        let result = sync_time(&panel, &context("POST", "/api/syncTime", "{}", Some(&token)));
        assert_eq!(body_json(result)["code"], 400);

        // 越界时间戳（1970 年）：400（校验先行，不会真正改时钟）
        let result = sync_time(
            &panel,
            &context("POST", "/api/syncTime", r#"{"timeMs":12345}"#, Some(&token)),
        );
        assert_eq!(body_json(result)["code"], 400);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn services_shape_matches_contract() {
        let (panel, dir) = panel_with_default_password();
        let token = panel.issue_token("admin", "admin").unwrap();

        let result = services(&panel, &context("GET", "/star/services", "", Some(&token)));
        let json = body_json(result);
        assert_eq!(json["code"], 0);
        let list = json["data"]["services"].as_array().unwrap();
        assert!(!list.is_empty());
        let first = &list[0];
        for key in [
            "Name",
            "FileName",
            "Enable",
            "Mode",
            "Running",
            "ProcessId",
            "ProcessName",
            "StartTime",
            "priority",
            "CpuRate",
            "MemoryMB",
        ] {
            assert!(first.get(key).is_some(), "缺少字段 {key}：{first}");
        }
        assert_eq!(json["data"]["total"], list.len());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn add_and_remove_service_persist_to_config() {
        let (panel, dir) = panel_with_default_password();
        let token = panel.issue_token("admin", "admin").unwrap();

        // 添加
        let body = r#"{"name":"demo","fileName":"demo.zip","mode":"Shadow","maxMemory":128,"enable":true}"#;
        let result = add_service(&panel, &context("POST", "/star/addService", body, Some(&token)));
        assert_eq!(body_json(result)["code"], 0);

        let cfg = panel.manager.config();
        let app = cfg.find_app("demo").expect("配置中应有 demo");
        assert_eq!(app.file_name, "demo.zip");
        assert_eq!(app.max_memory, 128);
        assert_eq!(app.mode, "shadow");
        assert!(app.enable);

        // 磁盘已落盘
        let path = crate::config::config_path(&dir);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"demo\""));

        // 删除
        let result = remove_service(
            &panel,
            &context("POST", "/star/removeService", r#"{"serviceName":"demo"}"#, Some(&token)),
        );
        assert_eq!(body_json(result)["code"], 0);
        assert!(panel.manager.config().find_app("demo").is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn update_config_and_change_password() {
        let (panel, dir) = panel_with_default_password();
        let token = panel.issue_token("admin", "admin").unwrap();

        // 更新配置（含非法值忽略）
        let body = r#"{"DelayFake":1,"MaxFails":30,"Debug":true}"#;
        let result = update_config(&panel, &context("POST", "/api/updateConfig", body, Some(&token)));
        assert_eq!(body_json(result)["code"], 0);
        let cfg = panel.manager.config();
        assert_eq!(cfg.max_fails, 30);
        assert!(cfg.debug);

        // 旧密码错误
        let result = change_password(
            &panel,
            &context(
                "POST",
                "/api/changePassword",
                r#"{"oldPassword":"bad","newPassword":"newpass"}"#,
                Some(&token),
            ),
        );
        assert_eq!(body_json(result)["code"], 403);

        // 修改成功并即时生效
        let result = change_password(
            &panel,
            &context(
                "POST",
                "/api/changePassword",
                r#"{"oldPassword":"admin","newPassword":"newpass"}"#,
                Some(&token),
            ),
        );
        assert_eq!(body_json(result)["code"], 0);
        assert!(panel.issue_token("admin", "newpass").is_some());
        assert!(panel.issue_token("admin", "admin").is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn star_config_roundtrip() {
        let (panel, dir) = panel_with_default_password();
        let token = panel.issue_token("admin", "admin").unwrap();

        let result = get_star_config(&panel, &context("GET", "/star/getStarConfig", "", Some(&token)));
        let json = body_json(result);
        assert_eq!(json["code"], 0);
        let groups = json["data"]["groups"].as_array().unwrap();
        assert_eq!(groups.len(), 2);
        assert!(groups[1]["items"].as_array().unwrap().len() >= 4);

        let body = r#"{"Delay":5000,"StartupHook":true,"UnknownField":1}"#;
        let result = update_star_config(
            &panel,
            &context("POST", "/star/updateStarConfig", body, Some(&token)),
        );
        assert_eq!(body_json(result)["code"], 0);
        let cfg = panel.manager.config();
        assert_eq!(cfg.delay, 5000);
        assert!(cfg.startup_hook);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn logs_and_watchdog_endpoints() {
        let (panel, dir) = panel_with_default_password();
        let token = panel.issue_token("admin", "admin").unwrap();

        // 写入一个日志文件
        let log_dir = dir.join("Log");
        std::fs::create_dir_all(&log_dir).unwrap();
        std::fs::write(log_dir.join("2025_01_02.log"), "line1\nline2\nline3\n").unwrap();

        let result = log_files(&panel, &context("GET", "/api/logFiles", "", Some(&token)));
        let json = body_json(result);
        assert_eq!(json["code"], 0);
        assert_eq!(json["data"]["files"][0]["name"], "2025_01_02.log");

        let result = logs(&panel, &context("GET", "/api/logs", "", Some(&token)));
        let json = body_json(result);
        assert_eq!(json["code"], 0);
        assert_eq!(json["data"]["count"], 3);
        assert_eq!(json["data"]["lines"][2], "line3");

        let result = watchdog(&panel, &context("GET", "/api/watchdog", "", Some(&token)));
        let json = body_json(result);
        assert_eq!(json["code"], 0);
        assert_eq!(json["data"]["services"].as_array().unwrap().len(), 0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn machine_and_health_endpoints() {
        let (panel, dir) = panel_with_default_password();
        let token = panel.issue_token("admin", "admin").unwrap();

        let result = machine(&panel, &context("GET", "/star/machine", "", Some(&token)));
        let json = body_json(result);
        assert_eq!(json["code"], 0);
        assert!(json["data"]["os"]["hostName"].is_string());
        assert!(json["data"]["memory"]["totalMB"].is_number());
        assert!(json["data"]["processes"].is_array());

        let result = health(&panel, &context("GET", "/api/health", "", Some(&token)));
        let json = body_json(result);
        assert_eq!(json["code"], 0);
        assert!(json["data"]["memoryMB"].is_number());
        assert!(json["data"]["gcCollections"]["gen0"].is_number());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn status_reports_host_wide_resources() {
        let (panel, dir) = panel_with_default_password();
        let token = panel.issue_token("admin", "admin").unwrap();

        let json = body_json(status(&panel, &context("GET", "/api/status", "", Some(&token))));
        assert_eq!(json["code"], 0);
        let d = &json["data"];

        // 整机内存：已用/总量为合理正数
        let used = d["memoryUsedMB"].as_u64().unwrap_or(0);
        let total = d["memoryTotalMB"].as_u64().unwrap_or(0);
        assert!(total > 0, "应返回整机总内存");
        assert!(used > 0 && used <= total, "整机已用内存应介于 0 与总量之间：{used}/{total}");

        // 服务器本地时间（含时区；供顶部机器概览展示）
        let lt = d["localTime"].as_str().unwrap_or_default();
        assert!(
            lt.contains('-') && lt.contains(':'),
            "应返回服务器本地时间：{lt}"
        );

        // 磁盘列表：数组，每项总量为正、已用不超过总量、名称非空
        let disks = d["disks"].as_array().expect("应返回磁盘数组");
        for disk in disks {
            let used = disk["usedMB"].as_u64().unwrap_or(0);
            let total = disk["totalMB"].as_u64().unwrap_or(0);
            assert!(total > 0, "磁盘总量应为正：{disk}");
            assert!(used <= total, "磁盘已用不应超过总量：{disk}");
            assert!(!disk["name"].as_str().unwrap_or_default().is_empty());
        }

        // 平台负载：Linux 有值，Windows 为 null
        #[cfg(target_os = "linux")]
        assert!(d["load1"].as_f64().is_some(), "Linux 应返回负载");
        #[cfg(not(target_os = "linux"))]
        assert!(d["load1"].is_null(), "非 Linux 平台负载应为 null");

        // 趋势图数据：数值速率与累计量齐备
        assert!(d["uplinkBps"].is_number() && d["downlinkBps"].is_number());
        assert!(d["netTxBytes"].as_u64().unwrap_or(0) > 0, "应返回累计发送字节");
        assert!(d["netRxBytes"].as_u64().unwrap_or(0) > 0, "应返回累计接收字节");
        assert!(d["diskReadBytes"].as_u64().is_some() && d["diskWriteBytes"].as_u64().is_some());
        assert!(d["diskLatencyMs"].is_number(), "应返回 IO 延迟数值");

        // health 已合并进 status（面板每 3 秒刷新只需一次请求）
        assert!(d["health"]["memoryMB"].is_number(), "status 应内嵌 health 指标");
        assert!(d["health"]["threadCount"].is_number());
        assert!(d["health"]["gcCollections"]["gen0"].is_number());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn status_flags_default_password_for_banner() {
        let (panel, dir) = panel_with_default_password();
        let token = panel.issue_token("admin", "admin").unwrap();

        // 默认配置 admin/admin 且 LocalOnly=false：应提示修改默认密码
        let json = body_json(status(&panel, &context("GET", "/api/status", "", Some(&token))));
        let d = &json["data"];
        assert_eq!(d["defaultPassword"], true, "默认凭据应触发横幅提示");
        assert_eq!(d["remoteAccess"], true, "默认配置允许远程访问");
        assert!(uses_default_credentials(&AgentConfig::default()));

        // 修改密码后提示消失（已签发令牌仍有效）
        panel
            .manager
            .update_config(|cfg| cfg.web_user_password = "secret".to_string());
        let json = body_json(status(&panel, &context("GET", "/api/status", "", Some(&token))));
        assert_eq!(json["data"]["defaultPassword"], false, "改密后不应再提示");
        assert!(!uses_default_credentials(&panel.manager.config()));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn client_ip_strips_port() {
        let ctx = context_from("127.0.0.1:50000", "GET", "/", "", None);
        assert_eq!(client_ip(&ctx), "127.0.0.1");
    }

    /// 数据库管理端点：只读闸门、查询、备份与还原全链路。
    #[test]
    fn db_admin_endpoints_query_backup_restore() {
        let (panel, dir) = panel_with_default_password();
        let token = panel.issue_token("admin", "admin").unwrap();

        // 只读闸门：写语句拒绝
        let r = db_query(
            &panel,
            &context(
                "POST",
                "/star/dbQuery",
                r#"{"sql":"DELETE FROM x"}"#,
                Some(&token),
            ),
        );
        assert_eq!(body_json(r)["code"], 400);

        // 查询（含列名与值）
        let r = db_query(
            &panel,
            &context(
                "POST",
                "/star/dbQuery",
                r#"{"sql":"SELECT 1 AS a"}"#,
                Some(&token),
            ),
        );
        let j = body_json(r);
        assert_eq!(j["code"], 0);
        assert_eq!(j["data"]["columns"][0], "a");
        assert_eq!(j["data"]["rows"][0][0], 1);

        // 概况（流量 2 张 + 面板用户 + 操作审计）
        let j = body_json(db_info(
            &panel,
            &context("GET", "/star/dbInfo", "", Some(&token)),
        ));
        assert_eq!(j["code"], 0);
        assert_eq!(j["data"]["provider"], "SQLite");
        assert_eq!(j["data"]["tables"].as_array().map(|a| a.len()), Some(4));

        // 备份下载
        let r = db_backup(&panel, &context("GET", "/star/dbBackup", "", Some(&token)));
        let bytes = match r {
            ActionResult::Response(resp) => resp.body,
            _ => panic!("应为二进制响应"),
        };
        assert!(bytes.starts_with(b"PK"));

        // 还原（上传备份包；空库往返）
        let r = db_restore(
            &panel,
            &context_bytes("POST", "/star/dbRestore", bytes.to_vec(), Some(&token)),
        );
        let j = body_json(r);
        assert_eq!(j["code"], 0, "还原应成功：{j}");
        assert_eq!(j["data"]["rows"]["Agent_WebTrafficDaily"], 0);

        // 非法包（非 zip 内容）被拒
        let r = db_restore(
            &panel,
            &context_bytes("POST", "/star/dbRestore", vec![0u8; 600], Some(&token)),
        );
        assert_eq!(body_json(r)["code"], 400);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 服务器端备份档案端点：创建 / 列表 / 下载 / 还原 / 删除。
    #[test]
    fn db_backup_files_endpoints() {
        let (panel, dir) = panel_with_default_password();
        let token = panel.issue_token("admin", "admin").unwrap();

        // 创建
        let j = body_json(db_create_backup(
            &panel,
            &context("POST", "/star/dbCreateBackup", "{}", Some(&token)),
        ));
        assert_eq!(j["code"], 0, "创建备份应成功：{j}");
        let name = j["data"]["name"].as_str().unwrap().to_string();
        assert!(name.ends_with(".zip"), "备份名：{name}");

        // 列表
        let j = body_json(db_list_backups(
            &panel,
            &context("GET", "/star/dbListBackups", "", Some(&token)),
        ));
        assert_eq!(j["code"], 0);
        let items = j["data"]["items"].as_array().unwrap();
        assert!(items
            .iter()
            .any(|it| it["name"].as_str() == Some(name.as_str())));

        // 下载（query 参数）
        let headers = vec![("Authorization".to_string(), format!("Bearer {token}"))];
        let ctx = Ctx::build(HttpRequest {
            method: "GET".to_string(),
            path: "/star/dbDownloadBackup".to_string(),
            query: format!("name={name}"),
            headers: headers.clone(),
            body: Vec::new().into(),
            remote_addr: Some("192.168.1.100:50000".to_string()),
        });
        match db_download_backup(&panel, &ctx) {
            ActionResult::Response(resp) => assert!(resp.body.starts_with(b"PK")),
            _ => panic!("应为二进制响应"),
        }

        // 非法名称被拒
        let ctx = Ctx::build(HttpRequest {
            method: "GET".to_string(),
            path: "/star/dbDownloadBackup".to_string(),
            query: "name=evil.txt".to_string(),
            headers,
            body: Vec::new().into(),
            remote_addr: Some("192.168.1.100:50000".to_string()),
        });
        assert_eq!(body_json(db_download_backup(&panel, &ctx))["code"], 400);

        // 从档案还原
        let j = body_json(db_restore_backup(
            &panel,
            &context(
                "POST",
                "/star/dbRestoreBackup",
                &format!(r#"{{"name":"{name}"}}"#),
                Some(&token),
            ),
        ));
        assert_eq!(j["code"], 0, "还原应成功：{j}");
        assert_eq!(j["data"]["rows"]["Agent_WebTrafficDaily"], 0);

        // 删除
        let j = body_json(db_delete_backup(
            &panel,
            &context(
                "POST",
                "/star/dbDeleteBackup",
                &format!(r#"{{"name":"{name}"}}"#),
                Some(&token),
            ),
        ));
        assert_eq!(j["code"], 0);
        let j = body_json(db_list_backups(
            &panel,
            &context("GET", "/star/dbListBackups", "", Some(&token)),
        ));
        assert!(j["data"]["items"].as_array().unwrap().is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn auth_level_local_only_allows_loopback_and_requires_token_remotely() {
        let (panel, dir) = panel_with_default_password();

        // 默认 LocalOnly：本机回环地址（含 127.0.0.0/8 与 IPv6 形式）无令牌放行
        for ip in [
            "127.0.0.1:50000",
            "127.0.0.53:50000",
            "[::1]:50000",
            "[::ffff:127.0.0.1]:50000",
        ] {
            let result = status(&panel, &context_from(ip, "GET", "/api/status", "", None));
            assert_eq!(body_json(result)["code"], 0, "本机地址应免鉴权：{ip}");
        }

        // 远程无令牌：拒绝
        let result = status(
            &panel,
            &context_from("192.168.1.100:50000", "GET", "/api/status", "", None),
        );
        assert_eq!(body_json(result)["code"], 401);

        // 远程持有效令牌：放行
        let token = panel.issue_token("admin", "admin").unwrap();
        let result = status(
            &panel,
            &context_from("192.168.1.100:50000", "GET", "/api/status", "", Some(&token)),
        );
        assert_eq!(body_json(result)["code"], 0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn auth_level_full_requires_token_even_on_loopback() {
        let (panel, dir) = panel_with_default_password();
        let token = panel.issue_token("admin", "admin").unwrap();

        let result = update_config(
            &panel,
            &context(
                "POST",
                "/api/updateConfig",
                r#"{"WebAuthLevel":"Full"}"#,
                Some(&token),
            ),
        );
        assert_eq!(body_json(result)["code"], 0);

        // 本机无令牌：拒绝；持令牌：放行
        let result = status(
            &panel,
            &context_from("127.0.0.1:50000", "GET", "/api/status", "", None),
        );
        assert_eq!(body_json(result)["code"], 401);
        let result = status(
            &panel,
            &context_from("127.0.0.1:50000", "GET", "/api/status", "", Some(&token)),
        );
        assert_eq!(body_json(result)["code"], 0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn auth_level_none_allows_remote_without_token() {
        let (panel, dir) = panel_with_default_password();
        let token = panel.issue_token("admin", "admin").unwrap();

        let result = update_config(
            &panel,
            &context(
                "POST",
                "/api/updateConfig",
                r#"{"WebAuthLevel":"none"}"#,
                Some(&token),
            ),
        );
        assert_eq!(body_json(result)["code"], 0);

        let result = status(
            &panel,
            &context_from("192.168.1.100:50000", "GET", "/api/status", "", None),
        );
        assert_eq!(body_json(result)["code"], 0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn auth_level_parse_matches_csharp_fallback() {
        assert_eq!(AuthLevel::parse("none"), AuthLevel::None);
        assert_eq!(AuthLevel::parse("None"), AuthLevel::None);
        assert_eq!(AuthLevel::parse(" FULL "), AuthLevel::Full);
        assert_eq!(AuthLevel::parse("localonly"), AuthLevel::LocalOnly);
        assert_eq!(AuthLevel::parse(""), AuthLevel::LocalOnly);
        assert_eq!(AuthLevel::parse("unknown"), AuthLevel::LocalOnly);
    }

    #[test]
    fn builtin_admin_listed_and_password_managed_via_user_save() {
        let (panel, dir) = panel_with_default_password();
        let token = panel.issue_token("admin", "admin").unwrap();

        // 用户列表：内置管理员虚拟条目在首位（全部权限、不可删除）
        let j = body_json(user_list(
            &panel,
            &context("GET", "/star/userList", "", Some(&token)),
        ));
        assert_eq!(j["code"], 0);
        let users = j["data"]["users"].as_array().unwrap();
        assert!(!users.is_empty());
        assert_eq!(users[0]["userName"], "admin");
        assert_eq!(users[0]["isBuiltin"], true);
        assert_eq!(users[0]["permissionNames"].as_array().unwrap().len(), 15);

        // 删除内置管理员被拒
        let j = body_json(user_delete(
            &panel,
            &context(
                "POST",
                "/star/userDelete",
                r#"{"name":"admin"}"#,
                Some(&token),
            ),
        ));
        assert_eq!(j["code"], 400);

        // 通过 userSave 修改内置管理员密码（写入配置文件凭据）
        let j = body_json(user_save(
            &panel,
            &context(
                "POST",
                "/star/userSave",
                r#"{"name":"admin","password":"newpw"}"#,
                Some(&token),
            ),
        ));
        assert_eq!(j["code"], 0, "{j}");
        assert!(panel.issue_token("admin", "newpw").is_some());
        assert!(panel.issue_token("admin", "admin").is_none());

        // 无密码且未改名 → 400
        let j = body_json(user_save(
            &panel,
            &context("POST", "/star/userSave", r#"{"name":"admin"}"#, Some(&token)),
        ));
        assert_eq!(j["code"], 400);

        // 改名冲突（与数据库用户重名）→ 400
        audit::save_user(&dir, "taken", Some("tk123"), &[], true, "").unwrap();
        let j = body_json(user_save(
            &panel,
            &context(
                "POST",
                "/star/userSave",
                r#"{"name":"admin","newName":"taken"}"#,
                Some(&token),
            ),
        ));
        assert_eq!(j["code"], 400);

        // 改名（不动密码）：admin → boss 立即生效；当前会话主体同步更名
        let j = body_json(user_save(
            &panel,
            &context(
                "POST",
                "/star/userSave",
                r#"{"name":"admin","newName":"boss"}"#,
                Some(&token),
            ),
        ));
        assert_eq!(j["code"], 0, "{j}");
        assert!(panel.issue_token("boss", "newpw").is_some());
        assert!(panel.issue_token("admin", "newpw").is_none());
        let j = body_json(user_list(
            &panel,
            &context("GET", "/star/userList", "", Some(&token)),
        ));
        assert_eq!(j["data"]["users"][0]["userName"], "boss");
        let m = body_json(me(&panel, &context("GET", "/api/me", "", Some(&token))));
        assert_eq!(m["data"]["user"], "boss");
        // 改名后删除仍被拒（虚拟条目）
        let j = body_json(user_delete(
            &panel,
            &context(
                "POST",
                "/star/userDelete",
                r#"{"name":"boss"}"#,
                Some(&token),
            ),
        ));
        assert_eq!(j["code"], 400);

        crate::history::drop_storage_for_test(&dir);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn format_bytes_matches_csharp_starapi() {
        // 对齐 C# StarApi.FormatBytes：B/KB/MB/GB，1024 进制
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1536), "1.5 KB");
        assert_eq!(format_bytes(5 * 1024 * 1024), "5.0 MB");
        assert_eq!(format_bytes(3 * 1024 * 1024 * 1024), "3.00 GB");
    }

    #[test]
    fn login_writes_audit_and_me_reports_permissions() {
        let (panel, dir) = panel_with_default_password();
        audit::save_user(
            &dir,
            "viewer",
            Some("vw123"),
            &["traffic".into(), "dashboard".into()],
            true,
            "只读用户",
        )
        .unwrap();

        // 表用户登录成功
        let r = body_json(login(
            &panel,
            &context(
                "POST",
                "/api/login",
                r#"{"user":"viewer","password":"vw123"}"#,
                None,
            ),
        ));
        assert_eq!(r["code"], 0);
        let token = r["data"]["token"].as_str().unwrap().to_string();

        // /api/me 返回其权限（顺序按权限清单归一化）
        let m = body_json(me(&panel, &context("GET", "/api/me", "", Some(&token))));
        assert_eq!(m["code"], 0);
        assert_eq!(m["data"]["user"], "viewer");
        assert_eq!(m["data"]["isAdmin"], false);
        assert_eq!(m["data"]["perms"], json!(["dashboard", "traffic"]));

        // 成功登录已写审计
        let logs = audit::query_logs(&dir, 1, 50, "viewer", "", None).unwrap();
        assert_eq!(logs["total"], 1);
        assert_eq!(logs["items"][0]["action"], "login");
        assert_eq!(logs["items"][0]["success"], true);

        // 失败登录同样写审计
        let _ = login(
            &panel,
            &context(
                "POST",
                "/api/login",
                r#"{"user":"viewer","password":"bad"}"#,
                None,
            ),
        );
        let logs = audit::query_logs(&dir, 1, 50, "viewer", "", None).unwrap();
        assert_eq!(logs["total"], 2);

        crate::history::drop_storage_for_test(&dir);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn guarded_enforces_permissions_and_audits_changes() {
        let (panel, dir) = panel_with_default_password();
        audit::save_user(&dir, "op1", Some("pw1"), &["fileman".into()], true, "").unwrap();
        let token = panel.issue_token("op1", "pw1").unwrap();

        // 有权限的动作：放行且写入审计
        let file_act = guarded(panel.clone(), PERM_FILEMAN, |_, _| json_result(0, "ok", None));
        let r = body_json(file_act(&context(
            "POST",
            "/star/fileDelete",
            r#"{"paths":["x"]}"#,
            Some(&token),
        )));
        assert_eq!(r["code"], 0);
        let logs = audit::query_logs(&dir, 1, 50, "op1", "", None).unwrap();
        assert_eq!(logs["total"], 1);
        assert_eq!(logs["items"][0]["action"], "fileDelete");
        assert_eq!(logs["items"][0]["success"], true);

        // 无权限的动作：403 且记为失败
        let db_act = guarded(panel.clone(), PERM_DATABASE, |_, _| json_result(0, "ok", None));
        let r = body_json(db_act(&context("POST", "/star/dbQuery", "", Some(&token))));
        assert_eq!(r["code"], 403);
        let denied = audit::query_logs(&dir, 1, 50, "op1", "dbQuery", Some(false)).unwrap();
        assert_eq!(denied["total"], 1);

        // 内置管理员全放行
        let admin_token = panel.issue_token("admin", "admin").unwrap();
        let r = body_json(db_act(&context("POST", "/star/dbQuery", "", Some(&admin_token))));
        assert_eq!(r["code"], 0);

        // 远程未认证：401（不写审计）
        let before = audit::query_logs(&dir, 1, 50, "", "", None).unwrap()["total"]
            .as_u64()
            .unwrap();
        let r = body_json(db_act(&context("POST", "/star/dbQuery", "", None)));
        assert_eq!(r["code"], 401);
        let after = audit::query_logs(&dir, 1, 50, "", "", None).unwrap()["total"]
            .as_u64()
            .unwrap();
        assert_eq!(before, after, "未认证请求不写审计");

        crate::history::drop_storage_for_test(&dir);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn summarize_request_redacts_passwords_and_tokens() {
        // JSON 体：password 字段脱敏
        let ctx = Ctx::build(HttpRequest {
            method: "POST".to_string(),
            path: "/api/changePassword".to_string(),
            query: String::new(),
            headers: vec![("Content-Type".to_string(), "application/json".to_string())],
            body: br#"{"oldPassword":"secretA","newPassword":"secretB"}"#
                .to_vec()
                .into(),
            remote_addr: Some("127.0.0.1:1".to_string()),
        });
        let text = summarize_request(&ctx);
        assert!(text.contains("\"oldPassword\":\"***\""), "{text}");
        assert!(text.contains("\"newPassword\":\"***\""), "{text}");
        assert!(!text.contains("secretA"), "{text}");

        // 查询串：token 参数脱敏
        let ctx = Ctx::build(HttpRequest {
            method: "GET".to_string(),
            path: "/star/dbDownloadBackup".to_string(),
            query: "name=a.zip&token=abcdef".to_string(),
            headers: Vec::new(),
            body: Vec::new().into(),
            remote_addr: Some("127.0.0.1:1".to_string()),
        });
        let text = summarize_request(&ctx);
        assert!(text.contains("token=***"), "{text}");
        assert!(!text.contains("abcdef"), "{text}");

        // AI 密钥：ApiKey 字段脱敏（不落审计明文）
        let ctx = Ctx::build(HttpRequest {
            method: "POST".to_string(),
            path: "/api/updateConfig".to_string(),
            query: String::new(),
            headers: vec![("Content-Type".to_string(), "application/json".to_string())],
            body: br#"{"AiApiKey":"sk-verysecret","AiModel":"deepseek-chat"}"#
                .to_vec()
                .into(),
            remote_addr: Some("127.0.0.1:1".to_string()),
        });
        let text = summarize_request(&ctx);
        assert!(text.contains("\"AiApiKey\":\"***\""), "{text}");
        assert!(!text.contains("sk-verysecret"), "{text}");
    }
}
