//! Web 管理面板：登录鉴权、服务状态与控制、配置管理、日志查看、本机信息。
//!
//! **对齐 C# 契约**（StarAgent / DH.NAgent 面板；前端 `index.html` 直接复用）：
//! - `/api/*`：login / logout / status / control / freeMemory / configMetadata /
//!   updateConfig / changePassword / logs / logFiles / health / watchdog
//! - `/star/*`：services / startService / stopService / restartService / addService /
//!   removeService / getStarConfig / updateStarConfig / machine / getProcessList
//! - 统一 JSON 信封 `{code, message?, data?}`；Bearer Token 鉴权（`Authorization` 头）
//!
//! 登录爆破防护按客户端 IP 计数（5 次失败封禁 5 分钟，窗口 15 分钟），与 C# 面板一致。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, Local};
use dhrust::net::controller::{
    arg, arg_i64, json_body, json_error, json_result, ActionResult, Controller,
};
use dhrust::net::router::Ctx;
use serde_json::{json, Value as Json};

use crate::agent;
use crate::config::AgentConfig;
use crate::manager::AppManager;
use crate::sys;
use crate::util;

/// 令牌有效期（小时）。
const TOKEN_HOURS: i64 = 24;
/// 最大允许失败次数。
const MAX_ATTEMPTS: u32 = 5;
/// 失败统计窗口。
const ATTEMPT_WINDOW_MINUTES: i64 = 15;
/// 封禁时长。
const BLOCK_MINUTES: i64 = 5;

/// 登录尝试记录。
struct Attempt {
    /// 窗口内失败次数
    count: u32,
    /// 首次失败时间
    first: DateTime<Local>,
    /// 封禁截止时间
    blocked_until: Option<DateTime<Local>>,
}

/// 面板共享状态。
pub struct WebPanel {
    /// 应用管理器
    manager: Arc<AppManager>,
    /// 程序基础目录
    base: PathBuf,
    /// 面板端口
    port: u16,
    /// 进程启动时刻（运行时长）
    started: Instant,
    /// 进程启动墙钟
    started_at: DateTime<Local>,
    /// 令牌表（token → 到期时间）
    tokens: Mutex<HashMap<String, DateTime<Local>>>,
    /// 登录限流（IP → 尝试记录）
    attempts: Mutex<HashMap<String, Attempt>>,
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
            tokens: Mutex::new(HashMap::new()),
            attempts: Mutex::new(HashMap::new()),
        })
    }

    /// 面板端口。
    pub fn port(&self) -> u16 {
        self.port
    }

    /// 进程运行时长。
    fn uptime(&self) -> Duration {
        self.started.elapsed()
    }

    // ————— 令牌 —————

    /// 签发令牌（校验用户名密码；失败返回 None）。
    fn issue_token(&self, user: &str, password: &str) -> Option<String> {
        if user.is_empty() || password.is_empty() {
            return None;
        }

        let cfg = self.manager.config();
        if cfg.web_user_name.trim().is_empty() || cfg.web_user_password.is_empty() {
            return None;
        }
        if !user.trim().eq_ignore_ascii_case(cfg.web_user_name.trim()) {
            return None;
        }
        if password != cfg.web_user_password {
            return None;
        }

        let token = dhrust::random::token();
        let now = Local::now();
        let mut tokens = self.tokens.lock().unwrap();
        // 清理过期令牌（与 C# 一致）
        tokens.retain(|_, expire| *expire > now);
        tokens.insert(token.clone(), now + chrono::Duration::hours(TOKEN_HOURS));
        Some(token)
    }

    /// 校验令牌。
    fn validate_token(&self, token: &str) -> bool {
        if token.is_empty() {
            return false;
        }

        let now = Local::now();
        let mut tokens = self.tokens.lock().unwrap();
        match tokens.get(token) {
            Some(expire) if *expire > now => true,
            Some(_) => {
                tokens.remove(token);
                false
            }
            None => false,
        }
    }

    /// 吊销令牌。
    fn revoke_token(&self, token: &str) {
        if !token.is_empty() {
            self.tokens.lock().unwrap().remove(token);
        }
    }

    // ————— 登录限流 —————

    /// 指定 IP 是否处于封禁中。
    fn is_blocked(&self, ip: &str) -> bool {
        if ip.is_empty() {
            return false;
        }

        let now = Local::now();
        let mut attempts = self.attempts.lock().unwrap();
        match attempts.get(ip) {
            Some(info) => match info.blocked_until {
                Some(until) if now < until => true,
                Some(_) => {
                    attempts.remove(ip);
                    false
                }
                None => false,
            },
            None => false,
        }
    }

    /// 记录一次失败（达到阈值时封禁）。
    fn record_failure(&self, ip: &str) {
        if ip.is_empty() {
            return;
        }

        let now = Local::now();
        let mut attempts = self.attempts.lock().unwrap();
        let info = attempts.entry(ip.to_string()).or_insert(Attempt {
            count: 0,
            first: now,
            blocked_until: None,
        });

        // 窗口过期，重置计数
        if now - info.first > chrono::Duration::minutes(ATTEMPT_WINDOW_MINUTES) {
            info.count = 0;
            info.first = now;
            info.blocked_until = None;
        }

        info.count += 1;
        if info.count >= MAX_ATTEMPTS {
            info.blocked_until = Some(now + chrono::Duration::minutes(BLOCK_MINUTES));
        }
    }

    /// 记录成功登录，清除该 IP 记录。
    fn record_success(&self, ip: &str) {
        if !ip.is_empty() {
            self.attempts.lock().unwrap().remove(ip);
        }
    }

    // ————— 鉴权辅助 —————

    /// 从请求头解析 Bearer 令牌。
    fn bearer_token(ctx: &Ctx) -> Option<String> {
        let auth = ctx.req.header("Authorization")?;
        let prefix = "Bearer ";
        if auth.len() <= prefix.len() || !auth[..prefix.len()].eq_ignore_ascii_case(prefix) {
            return None;
        }
        let token = auth[prefix.len()..].trim();
        if token.is_empty() {
            None
        } else {
            Some(token.to_string())
        }
    }

    /// 请求鉴权（`/api/login` 与 `/api/logout` 除外）。
    fn check_auth(&self, ctx: &Ctx) -> bool {
        match Self::bearer_token(ctx) {
            Some(token) => self.validate_token(&token),
            None => false,
        }
    }

    /// 客户端 IP（无端口）。
    fn client_ip(ctx: &Ctx) -> String {
        match ctx.req.remote_addr.as_deref() {
            Some(addr) => match addr.rsplit_once(':') {
                Some((host, _)) => host
                    .trim_start_matches('[')
                    .trim_end_matches(']')
                    .to_string(),
                None => addr.to_string(),
            },
            None => "unknown".to_string(),
        }
    }
}

// ————— 控制器注册 —————

/// 构建 `/api` 控制器。
pub fn build_api_controller(panel: Arc<WebPanel>) -> Controller {
    let mut controller = Controller::new("api");

    let p = panel.clone();
    controller = controller.post("login", move |ctx| login(&p, ctx));
    let p = panel.clone();
    controller = controller.post("logout", move |ctx| logout(&p, ctx));
    let p = panel.clone();
    controller = controller.get("status", move |ctx| status(&p, ctx));
    let p = panel.clone();
    controller = controller.get("health", move |ctx| health(&p, ctx));
    let p = panel.clone();
    controller = controller.get("freeMemory", move |ctx| free_memory(&p, ctx));
    let p = panel.clone();
    controller = controller.get("configMetadata", move |ctx| config_metadata(&p, ctx));
    let p = panel.clone();
    controller = controller.post("updateConfig", move |ctx| update_config(&p, ctx));
    let p = panel.clone();
    controller = controller.post("changePassword", move |ctx| change_password(&p, ctx));
    let p = panel.clone();
    controller = controller.get("logs", move |ctx| logs(&p, ctx));
    let p = panel.clone();
    controller = controller.get("logFiles", move |ctx| log_files(&p, ctx));
    let p = panel.clone();
    controller = controller.get("watchdog", move |ctx| watchdog(&p, ctx));
    let p = panel.clone();
    controller = controller.post("upgrade", move |ctx| upgrade(&p, ctx));
    controller.post("control", move |ctx| control(&panel, ctx))
}

/// 构建 `/star` 控制器。
pub fn build_star_controller(panel: Arc<WebPanel>) -> Controller {
    let mut controller = Controller::new("star");

    let p = panel.clone();
    controller = controller.get("services", move |ctx| services(&p, ctx));
    let p = panel.clone();
    controller = controller.post("startService", move |ctx| service_op(&p, ctx, Op::Start));
    let p = panel.clone();
    controller = controller.post("stopService", move |ctx| service_op(&p, ctx, Op::Stop));
    let p = panel.clone();
    controller = controller.post("restartService", move |ctx| service_op(&p, ctx, Op::Restart));
    let p = panel.clone();
    controller = controller.post("addService", move |ctx| add_service(&p, ctx));
    let p = panel.clone();
    controller = controller.post("removeService", move |ctx| remove_service(&p, ctx));
    let p = panel.clone();
    controller = controller.get("getStarConfig", move |ctx| get_star_config(&p, ctx));
    let p = panel.clone();
    controller = controller.post("updateStarConfig", move |ctx| update_star_config(&p, ctx));
    let p = panel.clone();
    controller = controller.get("machine", move |ctx| machine(&p, ctx));
    controller.get("getProcessList", move |ctx| get_process_list(&panel, ctx))
}

// ————— /api 动作 —————

/// 登录：签发令牌。
fn login(panel: &WebPanel, ctx: &Ctx) -> ActionResult {
    let ip = WebPanel::client_ip(ctx);

    if panel.is_blocked(&ip) {
        return json_error(429, "Too many failed attempts. Try again later.");
    }

    let user = arg(ctx, "user").unwrap_or_default();
    let password = arg(ctx, "password").unwrap_or_default();

    match panel.issue_token(&user, &password) {
        Some(token) => {
            panel.record_success(&ip);
            util::log_format("Web 面板登录成功：{}（{}）", &[&user, &ip]);
            json_result(0, "", Some(json!({ "token": token })))
        }
        None => {
            panel.record_failure(&ip);
            util::log_format("Web 面板登录失败：{}（{}）", &[&user, &ip]);
            json_error(401, "Invalid credentials")
        }
    }
}

/// 注销：吊销当前令牌。
fn logout(panel: &WebPanel, ctx: &Ctx) -> ActionResult {
    if let Some(token) = WebPanel::bearer_token(ctx) {
        panel.revoke_token(&token);
    }
    json_result(0, "ok", None)
}

/// 服务状态。
fn status(panel: &WebPanel, ctx: &Ctx) -> ActionResult {
    if !panel.check_auth(ctx) {
        return json_error(401, "Unauthorized");
    }

    let cfg = panel.manager.config();
    let pid = std::process::id();
    let (threads, handles) = sys::process_stats(pid).unwrap_or((0, 0));
    let memory_mb = sys::memory_mb(pid).unwrap_or(0);
    let (mem_total, mem_avail) = sys::memory_info().unwrap_or((0, 0));
    let cpu_rate = sys::system_cpu_rate();
    let (tcp_estab, tcp_time_wait, tcp_close_wait) = sys::tcp_counts();
    let uptime = panel.uptime();
    let cpu_count = std::thread::available_parallelism()
        .map(|v| v.get())
        .unwrap_or(0);

    let data = json!({
        "serviceName": cfg.service_name,
        "displayName": cfg.display_name,
        "description": cfg.description,
        "running": true,
        "uptime": format_uptime(uptime),
        "uptimeSeconds": uptime.as_secs(),
        "processId": pid,
        "memoryMB": memory_mb,
        "memoryTotalMB": mem_total / 1024 / 1024,
        "threadCount": threads,
        "handleCount": handles,
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
        "uplinkSpeed": "",
        "downlinkSpeed": "",
        "tcpConnections": tcp_estab,
        "tcpTimeWait": tcp_time_wait,
        "tcpCloseWait": tcp_close_wait,
        "diskIops": 0,
        "hostUptime": format_uptime(Duration::from_secs(sys::host_uptime_seconds())),
        "port": panel.port(),
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

/// 面板配置元数据（排除密码字段，走 ChangePassword 接口）。
fn config_metadata(panel: &WebPanel, ctx: &Ctx) -> ActionResult {
    if !panel.check_auth(ctx) {
        return json_error(401, "Unauthorized");
    }

    let cfg = panel.manager.config();
    let items = vec![
        config_item("WebUserName", "面板用户名", "String", cfg.web_user_name.clone(), "Web 管理面板的登录用户名"),
        config_item("WebAuthLevel", "鉴权级别", "String", cfg.web_auth_level.clone(), "None不鉴权，LocalOnly本地免鉴权，Full全部鉴权（预留）"),
        config_item("LocalPort", "本地端口", "Int32", cfg.local_port.to_string(), "本地控制端口（TCP 面板与 UDP RPC 共用），默认5500；修改需重启服务后生效"),
        config_item("LocalOnly", "仅本机访问", "Boolean", cfg.local_only.to_string(), "为真时只绑定 127.0.0.1；修改需重启服务后生效"),
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

/// 修改密码（校验旧密码，立即生效）。
fn change_password(panel: &WebPanel, ctx: &Ctx) -> ActionResult {
    if !panel.check_auth(ctx) {
        return json_error(401, "Unauthorized");
    }

    let old_password = arg(ctx, "oldPassword").unwrap_or_default();
    let new_password = arg(ctx, "newPassword").unwrap_or_default();

    if old_password.is_empty() {
        return json_error(400, "Missing oldPassword");
    }
    if new_password.is_empty() {
        return json_error(400, "Missing newPassword");
    }

    if old_password != panel.manager.config().web_user_password {
        return json_error(403, "Old password is incorrect");
    }

    panel
        .manager
        .update_config(|cfg| cfg.web_user_password = new_password);
    util::log_info("Web 面板密码已修改");

    json_result(0, "密码已修改，下次登录请使用新密码", None)
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
                    "description": "本地API通信端口（UDP），默认5500。与 Web 面板端口共用，UDP 用于本地 RPC，TCP 用于 Web 面板"
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
    let cpu_rate = sys::system_cpu_rate();
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
                "bytesReceived": "",
                "bytesSent": "",
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
        "hostUptime": format_uptime(Duration::from_secs(sys::host_uptime_seconds())),
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

// ————— 辅助 —————

/// 运行时长格式化（`d.hh:mm:ss`，与 C# 面板一致）。
fn format_uptime(duration: Duration) -> String {
    let secs = duration.as_secs();
    format!(
        "{}.{:02}:{:02}:{:02}",
        secs / 86400,
        (secs % 86400) / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

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

    /// 构造测试上下文。
    fn context(method: &str, path: &str, body: &str, token: Option<&str>) -> Ctx {
        use dhrust::net::router::Ctx;
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
            remote_addr: Some("127.0.0.1:50000".to_string()),
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
        let panel = WebPanel::new(manager, &dir, 5500);
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
        assert_eq!(status["data"]["port"], 5500);
        assert_eq!(status["data"]["running"], true);

        // 注销后失效
        let _ = logout(&panel, &context("POST", "/api/logout", "", Some(&token)));
        assert!(!panel.validate_token(&token));

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

        for _ in 0..MAX_ATTEMPTS {
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
        panel.record_success("10.0.0.1");
        assert!(!panel.is_blocked("10.0.0.1"));

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
    fn client_ip_strips_port() {
        let ctx = context("GET", "/", "", None);
        assert_eq!(WebPanel::client_ip(&ctx), "127.0.0.1");
    }
}
