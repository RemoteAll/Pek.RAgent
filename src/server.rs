//! 本地 HTTP 控制接口（默认 `127.0.0.1:5500`）。
//!
//! **DHDeploy 兼容契约（必须保持）**：
//! ```text
//! GET /RestartService?serviceName=X
//! GET /StartService?serviceName=X
//! GET /StopService?serviceName=X
//! → 200 {"Success":bool,"Message":"...","ServiceName":"X"}   （PascalCase；业务失败也返回 200）
//! ```
//! （来源：DHDeploy.Agent 的 `ApiHttpClient("http://localhost:5500/")` 调用，参数走 URL。）
//!
//! 另提供 `/GetServices`、`/Info`、`/Ping`、`/KillAndStart` 供菜单、看门狗与后续集成使用。

use std::sync::Arc;

use dhrust::net::http::{HttpOutcome, HttpResponse, HttpServer, HttpServerOptions};
use dhrust::net::router::{Ctx, route, Router};
use dhrust::net::static_files::StaticFiles;
use serde::Serialize;

use crate::manager::AppManager;
use crate::util;
use crate::webpanel::{build_api_controller, build_star_controller, WebPanel};

/// 服务操作结果（字段与 C# `ServiceOperationResult` 对齐）。
#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct ServiceOperationResult {
    success: bool,
    message: String,
    service_name: String,
}

/// 启动 HTTP 控制服务。返回线程句柄（进程退出时自然结束）。
pub fn start(manager: Arc<AppManager>, port: u16, local_only: bool) -> std::thread::JoinHandle<()> {
    let addr = if local_only {
        format!("127.0.0.1:{}", port)
    } else {
        format!("0.0.0.0:{}", port)
    };

    std::thread::Builder::new()
        .name("ragent-http".to_string())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    util::log_error(&format!("创建 HTTP 运行时时失败：{}", e));
                    return;
                }
            };

            runtime.block_on(async move {
                let server = match HttpServer::bind(addr.as_str()).await {
                    Ok(s) => s,
                    Err(e) => {
                        util::log_error(&format!("本地控制接口绑定失败 {}：{}", addr, e));
                        return;
                    }
                };

                util::log_format("本地控制接口已启动：http://{}", &[&addr]);

                let router = build_router(manager, port);
                let options = HttpServerOptions {
                    // 每连接独立线程：管理操作可能阻塞（停止/启动应用），避免拖慢其他请求
                    thread_per_connection: true,
                    ..Default::default()
                };

                if let Err(e) = server.serve_with(router.into_handler(), options).await {
                    util::log_error(&format!("本地控制接口退出：{}", e));
                }
            });
        })
        .expect("spawn http thread")
}

/// 构建路由。
fn build_router(manager: Arc<AppManager>, port: u16) -> Router {
    let mut router = Router::new();

    // Web 管理面板：/api/* 与 /star/*（Bearer Token 鉴权，契约对齐 C# 面板）
    let panel = WebPanel::new(manager.clone(), manager.base(), port);
    build_api_controller(panel.clone()).mount(&mut router);
    build_star_controller(panel).mount(&mut router);

    // DHDeploy 契约：应用级启停重启
    let m = manager.clone();
    router.map_get(
        "/RestartService",
        route(move |ctx| {
            let m = m.clone();
            async move { app_operation(m, ctx, OpKind::Restart) }
        }),
    );

    let m = manager.clone();
    router.map_get(
        "/StartService",
        route(move |ctx| {
            let m = m.clone();
            async move { app_operation(m, ctx, OpKind::Start) }
        }),
    );

    let m = manager.clone();
    router.map_get(
        "/StopService",
        route(move |ctx| {
            let m = m.clone();
            async move { app_operation(m, ctx, OpKind::Stop) }
        }),
    );

    // 子服务列表
    let m = manager.clone();
    router.map_get(
        "/GetServices",
        route(move |_ctx| {
            let m = m.clone();
            async move { HttpOutcome::Response(HttpResponse::json(200, services_json(&m))) }
        }),
    );

    // 代理信息
    let m = manager.clone();
    router.map_get(
        "/Info",
        route(move |_ctx| {
            let m = m.clone();
            async move { HttpOutcome::Response(HttpResponse::json(200, info_json(&m, port))) }
        }),
    );

    // 本机信息
    router.map_get(
        "/ShowMachineInfo",
        route(|_ctx| async move {
            HttpOutcome::Response(HttpResponse::text(200, crate::sys::machine_info()))
        }),
    );

    // 心跳/喂狗
    let m = manager.clone();
    router.map_get(
        "/Ping",
        route(move |ctx| {
            let m = m.clone();
            async move { ping(m, ctx) }
        }),
    );

    let m = manager.clone();
    router.map_post(
        "/Ping",
        route(move |ctx| {
            let m = m.clone();
            async move { ping(m, ctx) }
        }),
    );

    // 杀死并启动进程（应用自重启辅助）
    router.map_post(
        "/KillAndStart",
        route(move |ctx| async move { kill_and_start(ctx) }),
    );

    // 静态资源：优先嵌入的面板首页（单文件部署稳定），其次 wwwroot 目录
    let statics = StaticFiles::new("wwwroot").embed(
        "/index.html",
        include_bytes!("../web/index.html"),
        "text/html; charset=utf-8",
    );

    // 404（静态未命中时）
    router.fallback(route(move |ctx| {
        let statics = statics.clone();
        async move {
            if let Some(response) = statics.try_serve(&ctx.req.path) {
                return HttpOutcome::Response(response);
            }
            HttpOutcome::Response(HttpResponse::json(
                404,
                format!(
                    "{{\"Code\":404,\"Message\":\"Not Found: {}\"}}",
                    util::escape_json(&ctx.req.path)
                ),
            ))
        }
    }));

    router
}

/// 操作类型。
#[derive(Clone, Copy)]
pub(crate) enum OpKind {
    Start,
    Stop,
    Restart,
}

/// 应用级操作统一处理。
fn app_operation(manager: Arc<AppManager>, ctx: Ctx, kind: OpKind) -> HttpOutcome {
    let name = param(&ctx, "serviceName")
        .or_else(|| param(&ctx, "name"))
        .unwrap_or_default();

    if name.is_empty() {
        return result(false, "服务名称不能为空".to_string(), "");
    }

    let (ok, message) = apply_operation(&manager, &name, kind);
    result(ok, message, &name)
}

/// 应用操作核心（HTTP 控制接口与 UDP RPC 服务端共用；消息语义对齐 C# StarService）。
pub(crate) fn apply_operation(manager: &AppManager, name: &str, kind: OpKind) -> (bool, String) {
    match kind {
        OpKind::Start => match manager.start_app(name) {
            Ok(true) => (true, "服务启动成功".to_string()),
            Ok(false) => (true, "服务已在运行".to_string()),
            Err(e) if e.starts_with("服务不存在") => (false, "服务启动失败或服务不存在".to_string()),
            Err(e) => (false, format!("启动服务时发生错误: {}", e)),
        },
        OpKind::Stop => match manager.stop_app(name, "API调用停止") {
            Ok(true) => (true, "服务停止成功".to_string()),
            Ok(false) => (false, "服务停止失败".to_string()),
            Err(e) if e.starts_with("服务不存在") => (false, "服务停止失败或服务不存在".to_string()),
            Err(e) => (false, format!("停止服务时发生错误: {}", e)),
        },
        OpKind::Restart => match manager.restart_app(name, "API调用重启") {
            Ok(true) => (true, "服务重启成功".to_string()),
            Ok(false) => (false, "服务重启失败：启动服务失败".to_string()),
            Err(e) if e.starts_with("服务不存在") => (false, "服务不存在".to_string()),
            Err(e) => (false, format!("重启服务时发生错误: {}", e)),
        },
    }
}

/// 组装操作结果响应。
fn result(success: bool, message: String, name: &str) -> HttpOutcome {
    let value = ServiceOperationResult {
        success,
        message,
        service_name: name.to_string(),
    };
    HttpOutcome::Response(HttpResponse::json(
        200,
        serde_json::to_string(&value).unwrap_or_default(),
    ))
}

/// 参数取值（查询串与表单，多个候选名）。
fn param(ctx: &Ctx, name: &str) -> Option<String> {
    let lower = name.to_ascii_lowercase();
    for (k, v) in ctx.query.iter().chain(ctx.form.iter()) {
        if k.eq_ignore_ascii_case(name) || k.to_ascii_lowercase() == lower {
            if !v.is_empty() {
                return Some(v.clone());
            }
        }
    }
    None
}

/// 子服务列表 JSON（字段与 C# `ServicesInfo` 对齐）。
fn services_json(manager: &AppManager) -> String {
    let list = manager.list();
    let mut services = Vec::new();
    let mut running = Vec::new();

    for (cfg, status) in &list {
        services.push(serde_json::json!({
            "Name": cfg.name,
            "FileName": cfg.file_name,
            "Arguments": cfg.arguments.clone().unwrap_or_default(),
            "WorkingDirectory": cfg.working_directory.clone().unwrap_or_default(),
            "UserName": cfg.user_name.clone().unwrap_or_default(),
            "Enable": cfg.enable,
            "Mode": cfg.mode_text(),
            "AllowMultiple": cfg.allow_multiple,
            "Environments": cfg.environments.clone().unwrap_or_default(),
            "AutoStop": cfg.auto_stop,
            "ReloadOnChange": cfg.reload_on_change,
            "MaxMemory": cfg.max_memory,
            "OomScoreAdjust": cfg.oom_score_adjust,
            "HealthCheck": cfg.health_check.clone().unwrap_or_default(),
            "Running": status.running,
            "ProcessId": status.pid,
            "ProcessName": status.process_name,
            "StartTime": status.start_time,
        }));

        if status.running {
            running.push(serde_json::json!({
                "Name": cfg.name,
                "ProcessId": status.pid,
                "ProcessName": status.process_name,
                "CreateTime": status.start_time,
            }));
        }
    }

    serde_json::json!({
        "Services": services,
        "RunningServices": running,
    })
    .to_string()
}

/// 代理信息 JSON。
fn info_json(manager: &AppManager, port: u16) -> String {
    let cfg = manager.config();
    let list = manager.list();
    let running = list.iter().filter(|(_, s)| s.running).count();

    serde_json::json!({
        "Name": "Pek.RAgent",
        "Version": env!("CARGO_PKG_VERSION"),
        "OS": std::env::consts::OS,
        "Arch": std::env::consts::ARCH,
        "IP": dhrust::net::my_ip().map(|e| e.to_string()).unwrap_or_default(),
        "Server": cfg.server,
        "LocalPort": port,
        "AppCount": list.len(),
        "RunningCount": running,
    })
    .to_string()
}

/// 心跳：喂狗 + 返回服务器时间（毫秒）。
fn ping(manager: Arc<AppManager>, ctx: Ctx) -> HttpOutcome {
    let pid = param(&ctx, "processId")
        .and_then(|v| v.trim().parse::<u32>().ok())
        .unwrap_or(0);
    let timeout = param(&ctx, "watchdogTimeout")
        .and_then(|v| v.trim().parse::<u32>().ok())
        .unwrap_or(0);

    if pid > 0 && timeout > 0 {
        manager.feed_dog(pid, timeout);
    }

    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    HttpOutcome::Response(HttpResponse::json(
        200,
        format!("{{\"ServerTime\":{}}}", millis),
    ))
}

/// 杀死进程并重新启动（应用自重启辅助，`LocalStarClient.KillAndRestartMySelf` 的等价能力）。
#[derive(serde::Deserialize, Default)]
#[serde(rename_all = "PascalCase", default)]
struct KillAndStartRequest {
    #[serde(alias = "processId")]
    process_id: u32,
    #[serde(alias = "delay")]
    delay: u32,
    #[serde(alias = "fileName")]
    file_name: String,
    #[serde(alias = "arguments")]
    arguments: String,
    #[serde(alias = "workingDirectory")]
    working_directory: String,
}

fn kill_and_start(ctx: Ctx) -> HttpOutcome {
    let body = String::from_utf8_lossy(&ctx.req.body).to_string();
    let req: KillAndStartRequest = serde_json::from_str(&body).unwrap_or_default();

    let pid = req.process_id;
    let name = if pid > 0 {
        crate::sys::process_name(pid).unwrap_or_default()
    } else {
        String::new()
    };

    std::thread::spawn(move || {
        if req.delay > 0 {
            std::thread::sleep(std::time::Duration::from_secs(req.delay as u64));
        }

        if req.process_id > 0 {
            util::log_format(
                "KillAndStart：停止进程 PID={}",
                &[&req.process_id.to_string()],
            );
            crate::sys::stop_process(req.process_id, 5_000);
        }

        if !req.file_name.is_empty() {
            let cwd_text = if req.working_directory.trim().is_empty() {
                ".".to_string()
            } else {
                req.working_directory.clone()
            };
            let cwd = std::path::PathBuf::from(&cwd_text);
            let args = dhrust::io::split_args(&req.arguments);
            let envs = vec![("BasePath".to_string(), cwd_text.clone())];

            let spawn_req = crate::sys::SpawnRequest {
                program: &req.file_name,
                args: &args,
                cwd: &cwd,
                envs: &envs,
                log_file: None,
                detached: true,
            };
            match crate::sys::spawn(&spawn_req) {
                Ok(child) => util::log_format(
                    "KillAndStart：启动 {} PID={}",
                    &[&req.file_name, &child.id().to_string()],
                ),
                Err(e) => util::log_error(&format!("KillAndStart：启动失败 {}", e)),
            }
        }
    });

    HttpOutcome::Response(HttpResponse::json(
        200,
        format!(
            "{{\"Name\":\"{}\",\"ProcessId\":{}}}",
            util::escape_json(&name),
            pid
        ),
    ))
}
