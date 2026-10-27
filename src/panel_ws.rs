//! 平台实时通道（Pek.RAgent → Pek.RPanlServer WebSocket）。
//!
//! 配置「平台接入令牌」（`AutoUpgradeToken`）后启用：
//! - 上行：`register`（节点信息 + 子应用清单）/ `heartbeat`（机器数据，60 秒；子应用清单变化才带）
//!   / `appUpgradeResult`（远程应用升级回执）——平台「服务器节点」页可见；
//! - 下行：`checkUpgrade`（立即检查代理自升级）/ `appUpgrade`（下载并部署**已有**子应用包）。
//!
//! 连接地址从自动升级源（`AutoUpgradeUrl`）推导：`scheme://host[:port]` + `/store/agents/ws`
//! （http → ws、https → wss）；断线由 dhrust `WsClient` 会话层自动重连（指数退避），
//! 配置变更（地址/令牌）在 30 秒内自动换连。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{json, Value as Json};

use dhrust::net::ws::{WsClient, WsClientOptions, WsHooks, WsMessage};

use crate::config::AgentConfig;
use crate::manager::AppManager;
use crate::util;

/// 进程内启动时刻（心跳上报运行时长）。
static STARTED: OnceLock<Instant> = OnceLock::new();
/// 当前连接状态（会话钩子维护；面板状态展示用）。
static CONNECTED: AtomicBool = AtomicBool::new(false);
/// 是否处于「等待平台批准」状态（配对模式，平台已回 `pairPending`）。
static PAIR_PENDING: AtomicBool = AtomicBool::new(false);
/// 当前会话的客户端句柄（平台判定标识重复时用于立即断开触发重连）。
static CURRENT: OnceLock<std::sync::Mutex<Option<WsClient>>> = OnceLock::new();

/// 当前是否已连接平台（令牌未配置/连接中均返回 false）。
pub fn connected() -> bool {
    CONNECTED.load(Ordering::Relaxed)
}

/// 证书部署串行锁（同一时刻只允许一个任务改配置）。
static DEPLOY_BUSY: std::sync::Mutex<bool> = std::sync::Mutex::new(false);

/// 应用升级串行锁（同一时刻只允许一个应用包部署，避免两个任务同时改同一应用目录）。
static APP_UPGRADE_BUSY: std::sync::Mutex<bool> = std::sync::Mutex::new(false);

/// 上次上报的子应用清单签名（心跳仅在变化时携带 `apps`，平台侧 `null` = 保留旧值）。
static APPS_SIGNATURE: OnceLock<std::sync::Mutex<String>> = OnceLock::new();

/// 是否启用（升级源地址非空，且「有令牌」或「开配对」）。
pub fn enabled(cfg: &AgentConfig) -> bool {
    !cfg.auto_upgrade_url.trim().is_empty()
        && (!cfg.auto_upgrade_token.trim().is_empty() || cfg.pairing_enabled)
}

/// 当前是否处于「已接入、等待平台批准」状态（面板展示用）。
pub fn pairing_pending() -> bool {
    PAIR_PENDING.load(Ordering::Relaxed)
}

/// 最近一次连接失败/断开的原因（面板诊断展示用）。
static LAST_ERROR: OnceLock<std::sync::Mutex<String>> = OnceLock::new();

pub fn last_error() -> String {
    LAST_ERROR
        .get_or_init(|| std::sync::Mutex::new(String::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/// 当前实际连接的目标地址（由升级源推导；面板展示"到底连的哪台"）。
pub fn target_url(cfg: &AgentConfig) -> String {
    ws_url(cfg).unwrap_or_default()
}

/// 向平台发送一条上行事件（未连接时静默忽略）。
///
/// 用于把本机执行结果回报平台（如 `rebootResult`），避免"平台点了没反应、原因只在节点日志里"。
pub fn send_event(agent_id: &str, mut v: Json) {
    if let Some(obj) = v.as_object_mut() {
        obj.insert("agentId".to_string(), Json::String(agent_id.to_string()));
    }
    let text = v.to_string();
    if let Some(client) = CURRENT
        .get_or_init(|| std::sync::Mutex::new(None))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        && client.is_connected()
    {
        let _ = client.send_text(text);
    }
}

/// 从升级源地址推导 WS 地址（origin + `/store/agents/ws`；仅 http/https）。
pub fn ws_url(cfg: &AgentConfig) -> Option<String> {
    let raw = cfg.auto_upgrade_url.trim();
    let scheme = if raw.starts_with("https://") {
        "wss://"
    } else if raw.starts_with("http://") {
        "ws://"
    } else {
        return None;
    };
    let rest = raw.split_once("://")?.1;
    let host = rest.split('/').next().unwrap_or("");
    if host.is_empty() {
        return None;
    }
    Some(format!("{scheme}{host}/store/agents/ws"))
}

// 节点标识（平台「服务器节点」身份）存放于配置 `AgentId`：首次运行由
// `AgentConfig::normalize` 生成随机值并随配置持久化（不用 /etc/machine-id——
// 克隆镜像会重复，会导致多台服务器被平台当成同一个节点；对齐 DHDeploy 的独立标识模型）。

/// 启动后台通道（幂等；专用线程 + current_thread 运行时保持会话存活）。
pub fn start(manager: Arc<AppManager>) {
    let _ = STARTED.set(Instant::now());
    let _ = std::thread::Builder::new()
        .name("ragent-panel-ws".to_string())
        .spawn(move || {
            // 关键后台线程：任何 panic 都不能让整个平台通道静默消失。
            // 外层循环重建运行时并重启通道；panic 本身由全局 panic hook 记录线程/位置。
            loop {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        util::log_error(&format!(
                            "平台实时通道运行时创建失败：{e}（10 秒后重试）"
                        ));
                        std::thread::sleep(Duration::from_secs(10));
                        continue;
                    }
                };
                let mgr = manager.clone();
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    rt.block_on(run(mgr));
                }));
                match result {
                    Ok(()) => util::log_error(
                        "平台实时通道任务意外结束（0 退出；5 秒后重启）",
                    ),
                    Err(_) => util::log_error(
                        "平台实时通道任务 panic（5 秒后重建运行时并重启，详见上一条 panic 日志）",
                    ),
                }
                std::thread::sleep(Duration::from_secs(5));
            }
        });
}

/// 主循环：未配置时周期探测；已配置时建连并在会话内维持心跳（配置变更自动换连）。
async fn run(manager: Arc<AppManager>) {
    loop {
        let cfg = manager.config();
        if !enabled(&cfg) {
            tokio::time::sleep(Duration::from_secs(60)).await;
            continue;
        }
        let Some(url) = ws_url(&cfg) else {
            tokio::time::sleep(Duration::from_secs(60)).await;
            continue;
        };
        let token = cfg.auto_upgrade_token.trim().to_string();
        // 无令牌 → 配对模式（`?pair=1`）：平台会把本节点放进「待批准」队列，
        // 批准后在本连接上下发 `tokenIssue`，写入配置并自动重连。
        let full = if token.is_empty() {
            format!("{url}?pair=1")
        } else {
            format!("{url}?token={}", dhrust::web::url_encode(&token))
        };
        util::log_format("平台实时通道连接中：{}", &[&url]);
        // 会话当前标识（每次连接成功时由 on_connected 刷新）：配置中的标识变更（含平台判定
        // 重复后自动重建）即断开重连，重连时携带新标识重新注册
        let session_id = Arc::new(std::sync::Mutex::new(cfg.agent_id.clone()));
        let client = WsClient::connect(
            full,
            build_hooks(manager.clone(), session_id.clone()),
            WsClientOptions::default(),
        );

        // 会话内循环：60 秒心跳 + 30 秒配置检查（配置变更即断开重连）
        let mut hb = tokio::time::interval(Duration::from_secs(60));
        hb.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut chk = tokio::time::interval(Duration::from_secs(30));
        chk.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // 连接状态看门狗：WsClient 内部会自动重连，但若其监督任务 panic 或卡死，
        // 仅靠会话循环会永远停在“未连接”。超过 3 分钟未恢复则重建客户端。
        let mut link = tokio::time::interval(Duration::from_secs(30));
        link.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut down_since: Option<Instant> = None;
        // 消费首 tick（避免建连即发心跳；配置检查首 tick 保留）
        hb.tick().await;
        link.tick().await;
        loop {
            tokio::select! {
                _ = hb.tick() => {
                    // 配对（待批准）期间不发心跳：平台此时不接收任何上报，省带宽也省日志
                    if client.is_connected() && !PAIR_PENDING.load(Ordering::Relaxed) {
                        let _ = client.send_text(build_heartbeat(&manager));
                    }
                }
                _ = link.tick() => {
                    if client.is_connected() {
                        down_since = None;
                    } else {
                        let since = *down_since.get_or_insert_with(Instant::now);
                        if since.elapsed() >= Duration::from_secs(180) {
                            util::log_error(
                                "平台实时通道 180 秒未恢复，重建客户端（避免监督任务卡死导致永久离线）",
                            );
                            client.close();
                            break;
                        }
                    }
                }
                _ = chk.tick() => {
                    let now = manager.config();
                    let sid = session_id
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .clone();
                    let same = enabled(&now)
                        && ws_url(&now).as_deref() == Some(url.as_str())
                        && now.auto_upgrade_token.trim() == token
                        && now.agent_id == sid;
                    if !same {
                        client.close();
                        util::log_info("平台实时通道配置已变更，重新连接");
                        break;
                    }
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

/// 会话钩子（注册/指令/断开日志）。
fn build_hooks(manager: Arc<AppManager>, session_id: Arc<std::sync::Mutex<String>>) -> WsHooks {
    let reg_mgr = manager.clone();
    let conn_id = session_id.clone();
    let on_connected = Arc::new(move |client: WsClient| {
        CONNECTED.store(true, Ordering::Relaxed);
        PAIR_PENDING.store(false, Ordering::Relaxed);
        // 记录本会话实际使用的标识（重连时若配置已变更——如平台判定重复后重建——取新值）
        {
            let mut sid = conn_id.lock().unwrap_or_else(|e| e.into_inner());
            *sid = reg_mgr.config().agent_id;
        }
        {
            let mut cur = CURRENT
                .get_or_init(|| std::sync::Mutex::new(None))
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            *cur = Some(client.clone());
        }
        let _ = client.send_text(build_register(&reg_mgr));
        // 上线后主动盘点一次站点（平台可能先于心跳建好节点卡片）
        {
            let mgr = reg_mgr.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_secs(2));
                let base = crate::util::base_dir();
                let inv = crate::sites::inventory(&mgr.config(), &base);
                let agent_id = mgr.config().agent_id;
                send_event(
                    &agent_id,
                    json!({
                        "type": "siteInventory",
                        "web": inv.web,
                        "sites": inv.sites,
                        "deploy": crate::certdeploy::last_result(&base),
                    }),
                );
            });
        }
    });
    let msg_mgr = manager.clone();
    let on_message = Arc::new(move |msg: WsMessage| {
        let Ok(v) = serde_json::from_str::<Json>(&msg.text) else {
            return;
        };
        match v.get("type").and_then(|t| t.as_str()).unwrap_or("") {
            "hello" => util::log_info("平台实时通道已建立（节点已接入）"),
            "pairPending" => {
                PAIR_PENDING.store(true, Ordering::Relaxed);
                util::log_info(
                    "已向平台提交接入申请，等待管理员在「服务器节点」页批准（批准后自动下发令牌并上线）",
                );
            }
            "pairDenied" => {
                PAIR_PENDING.store(false, Ordering::Relaxed);
                let reason = v
                    .get("reason")
                    .and_then(|x| x.as_str())
                    .unwrap_or("平台拒绝配对接入");
                util::log_error(&format!("平台拒绝配对接入：{reason}"));
            }
            "idConflict" => {
                util::log_info("平台检测到节点标识与其他服务器重复，重新生成标识");
                regenerate_agent_id(&msg_mgr);
            }
            "storeConfig" => apply_store_config(&msg_mgr, &v),
            "tokenIssue" => apply_node_token(&msg_mgr, &v),
            "reboot" => reboot_once(&msg_mgr),
            // 平台盘点本机站点（只读；用于「证书」页匹配域名与生成计划）
            "siteScan" => {
                let mgr = msg_mgr.clone();
                std::thread::spawn(move || {
                    let base = crate::util::base_dir();
                    let inv = crate::sites::inventory(&mgr.config(), &base);
                    let agent_id = mgr.config().agent_id;
                    send_event(
                        &agent_id,
                        json!({
                            "type": "siteInventory",
                            "web": inv.web,
                            "sites": inv.sites,
                            "deploy": crate::certdeploy::last_result(&base),
                        }),
                    );
                });
            }
            // 平台下发证书任务（`certFetch`：只带票据 URL；节点**主动拉取**并在本地匹配站点）
            "certFetch" => {
                let mgr = msg_mgr.clone();
                let payload = v.clone();
                std::thread::spawn(move || {
                    let agent_id = mgr.config().agent_id;
                    {
                        // 串行化：同一时刻只跑一个部署（避免两个任务改同一份配置）
                        let mut lock = DEPLOY_BUSY.lock().unwrap_or_else(|e| e.into_inner());
                        if *lock {
                            send_event(
                                &agent_id,
                                json!({"type": "certResult", "result": {
                                    "certId": payload.get("certId").cloned().unwrap_or(json!("")),
                                    "dryRun": payload.get("dryRun").cloned().unwrap_or(json!(true)),
                                    "ok": false,
                                    "items": [],
                                    "message": "节点正在执行另一个证书任务，请稍后重试"
                                }}),
                            );
                            return;
                        }
                        *lock = true;
                    }
                    let req: crate::certdeploy::FetchRequest =
                        match serde_json::from_value(payload) {
                            Ok(r) => r,
                            Err(e) => {
                                send_event(
                                    &agent_id,
                                    json!({"type": "certResult", "result": {
                                        "certId": "", "dryRun": true, "ok": false, "items": [],
                                        "message": format!("证书任务参数无效：{e}")
                                    }}),
                                );
                                if let Ok(mut l) = DEPLOY_BUSY.lock() {
                                    *l = false;
                                }
                                return;
                            }
                        };
                    util::log_format(
                        "平台下发证书任务：{}（{}，目标：{}）",
                        &[
                            if req.cert_name.is_empty() {
                                &req.cert_id
                            } else {
                                &req.cert_name
                            },
                            if req.dry_run { "演练" } else { "执行" },
                            if req.sites.is_empty() {
                                "本地匹配白名单站点"
                            } else {
                                "平台指定站点"
                            },
                        ],
                    );
                    let base = crate::util::base_dir();
                    let result = crate::certdeploy::fetch_and_deploy(&mgr.config(), &base, &req);
                    if let Ok(mut l) = DEPLOY_BUSY.lock() {
                        *l = false;
                    }
                    util::log_format(
                        "证书任务完成：{}（{}）",
                        &[&result.message, if result.ok { "全部成功" } else { "含失败项" }],
                    );
                    send_event(&agent_id, json!({"type": "certResult", "result": result}));
                });
            }
            "checkUpgrade" => {
                util::log_info("平台下发「立即检查升级」指令，开始检查");
                crate::self_upgrade::trigger(msg_mgr.config(), true);
            }
            // 平台下发子应用升级（`appUpgrade`）：下载 → SHA-256 校验 → 部署 → 重启 → 回执
            "appUpgrade" => {
                let mgr = msg_mgr.clone();
                let cmd = v.clone();
                std::thread::spawn(move || handle_app_upgrade(&mgr, &cmd));
            }
            // ————— 平台统一管控面板子用户（平台权威；内置管理员不接管） —————
            "panelUserList" => {
                let base = crate::util::base_dir();
                let agent_id = msg_mgr.config().agent_id;
                let reply = match crate::panel_users::list_json(&base) {
                    Ok(data) => json!({ "type": "panelUsers", "ok": true, "data": data }),
                    Err(e) => json!({ "type": "panelUsers", "ok": false, "message": e }),
                };
                send_event(&agent_id, reply);
            }
            "panelUserUpsert" | "panelUserDelete" | "panelUserSync" => {
                let mgr = msg_mgr.clone();
                let cmd = v.clone();
                // DB 写入放到独立线程：不阻塞实时通道（心跳/其它指令照常）
                std::thread::spawn(move || {
                    let base = crate::util::base_dir();
                    let cfg = mgr.config();
                    let agent_id = cfg.agent_id.clone();
                    let action = cmd
                        .get("type")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string();
                    let result = match action.as_str() {
                        "panelUserUpsert" => crate::panel_users::upsert(&base, &cfg, &cmd)
                            .map(|name| json!({ "userName": name })),
                        "panelUserDelete" => crate::panel_users::delete(&base, &cfg, &cmd)
                            .map(|name| json!({ "userName": name })),
                        _ => {
                            let users = cmd
                                .get("users")
                                .and_then(|x| x.as_array())
                                .cloned()
                                .unwrap_or_default();
                            let prune = cmd
                                .get("prune")
                                .and_then(|x| x.as_bool())
                                .unwrap_or(true);
                            crate::panel_users::sync(&base, &cfg, &users, prune).map(
                                |(applied, removed, skipped)| {
                                    json!({ "applied": applied, "removed": removed, "skipped": skipped })
                                },
                            )
                        }
                    };
                    match result {
                        Ok(data) => {
                            send_event(
                                &agent_id,
                                json!({ "type": "panelUserResult", "action": action, "ok": true, "data": data }),
                            );
                            // 立即回传新摘要：平台无需等下一次心跳即可确认已对齐
                            let (digest, count) = crate::panel_users::summary(&base);
                            send_event(
                                &agent_id,
                                json!({ "type": "panelUserSummary", "panelUsersVersion": digest, "panelUsersCount": count }),
                            );
                        }
                        Err(e) => {
                            util::log_error(&format!("平台下发用户操作失败（{action}）：{e}"));
                            send_event(
                                &agent_id,
                                json!({ "type": "panelUserResult", "action": action, "ok": false, "message": e }),
                            );
                        }
                    }
                });
            }
            _ => {}
        }
    });
    let on_disconnected = Arc::new(move |reason: String| {
        CONNECTED.store(false, Ordering::Relaxed);
        PAIR_PENDING.store(false, Ordering::Relaxed);
        // 记下原因：面板「平台实时通道」可直接看到为什么没连上（不用去翻日志）
        *LAST_ERROR
            .get_or_init(|| std::sync::Mutex::new(String::new()))
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = reason.clone();
        util::log_format("平台实时通道已断开：{}（自动重连中）", &[&reason]);
    });
    WsHooks {
        on_message: Some(on_message),
        on_connected: Some(on_connected),
        on_disconnected: Some(on_disconnected),
        on_ping_tick: None,
    }
}

/// 平台判定标识重复时调用：重新生成 `AgentId` 并写盘，然后断开当前会话——
/// 配置检查周期（30 秒）最迟在下一拍以新标识重连注册（60 秒冷却防异常场景下标识抖动）。
fn regenerate_agent_id(manager: &AppManager) {
    static LAST: OnceLock<std::sync::Mutex<Option<Instant>>> = OnceLock::new();
    let mut last = LAST
        .get_or_init(|| std::sync::Mutex::new(None))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(t) = *last
        && t.elapsed() < Duration::from_secs(60)
    {
        util::log_info("节点标识重复提示过快，已忽略（60 秒冷却）");
        return;
    }
    *last = Some(Instant::now());
    drop(last);

    manager.update_config(|cfg| cfg.agent_id = dhrust::random::hex(16));
    util::log_format(
        "已重新生成节点标识：{}（配置已写盘，重连后以新标识上线）",
        &[&manager.config().agent_id],
    );
    // 断开当前会话触发重连：配置检查周期（30 秒）最迟在下一拍以新标识重新注册
    if let Some(client) = CURRENT
        .get_or_init(|| std::sync::Mutex::new(None))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
    {
        client.close();
    }
}

/// 平台下发子应用升级（`appUpgrade`）：
/// 下载（平台相对路径 + 一次性票据）→ SHA-256 校验 → 调用 `apppkg::deploy_bytes` 部署并重启 → 回执。
///
/// 安全边界：只允许升级**本机已存在**的子应用；下载地址只能来自 `AutoUpgradeUrl` 的 origin
/// （平台给的是 `/store/apps/{id}?ticket=...` 相对路径），避免被冒充平台诱导去任意地址下载。
fn handle_app_upgrade(manager: &Arc<AppManager>, cmd: &Json) {
    let agent_id = manager.config().agent_id.clone();
    let app_name = cmd
        .get("appName")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let version = cmd
        .get("version")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let reply = |ok: bool, message: String| {
        send_event(
            &agent_id,
            json!({
                "type": "appUpgradeResult",
                "appName": app_name,
                "version": version,
                "ok": ok,
                "message": message,
            }),
        );
    };

    if app_name.is_empty() || version.is_empty() {
        reply(false, "缺少 appName 或 version".to_string());
        return;
    }
    // 串行化：避免两个升级任务同时改同一应用目录
    {
        let mut busy = APP_UPGRADE_BUSY.lock().unwrap_or_else(|e| e.into_inner());
        if *busy {
            reply(false, "节点正在执行另一个应用升级，请稍后重试".to_string());
            return;
        }
        *busy = true;
    }
    // RAII：即使后续逻辑 panic，也会释放串行锁，不会让节点永久拒绝后续升级
    struct BusyGuard;
    impl Drop for BusyGuard {
        fn drop(&mut self) {
            if let Ok(mut busy) = APP_UPGRADE_BUSY.lock() {
                *busy = false;
            }
        }
    }
    let _guard = BusyGuard;
    let result = run_app_upgrade(manager, cmd, &app_name, &version);
    match &result {
        Ok(msg) => {
            util::log_format(
                "平台应用升级完成：{} v{}——{}",
                &[&app_name, &version, msg],
            );
            reply(true, msg.clone());
            crate::audit::record(
                manager.base(),
                &crate::audit::AuditEntry {
                    category: None,
                    user: "platform".to_string(),
                    ip: "platform".to_string(),
                    action: "appUpgrade".to_string(),
                    title: format!("平台下发应用升级：{app_name} v{version}"),
                    method: "WS".to_string(),
                    path: "/store/agents/ws".to_string(),
                    detail: msg.clone(),
                    success: true,
                    code: 0,
                    message: String::new(),
                    elapsed_ms: 0,
                },
            );
        }
        Err(e) => {
            util::log_error(&format!(
                "平台应用升级失败：{} v{}——{}",
                app_name, version, e
            ));
            reply(false, e.clone());
            crate::audit::record(
                manager.base(),
                &crate::audit::AuditEntry {
                    category: None,
                    user: "platform".to_string(),
                    ip: "platform".to_string(),
                    action: "appUpgrade".to_string(),
                    title: format!("平台下发应用升级：{app_name} v{version}"),
                    method: "WS".to_string(),
                    path: "/store/agents/ws".to_string(),
                    detail: e.clone(),
                    success: false,
                    code: 0,
                    message: e.clone(),
                    elapsed_ms: 0,
                },
            );
        }
    }
}

/// 执行一次应用升级（失败返回可读原因；调用方负责回执与审计）。
fn run_app_upgrade(
    manager: &Arc<AppManager>,
    cmd: &Json,
    app_name: &str,
    version: &str,
) -> Result<String, String> {
    // 只允许升级已有应用：远程新建任意应用会扩大攻击面，也不符合「升级」语义
    // （大小写不敏感匹配，避免平台包名大小写与配置不一致时误判为「不存在」）
    let actual_name = manager
        .config()
        .apps
        .iter()
        .find(|a| a.name.eq_ignore_ascii_case(app_name))
        .map(|a| a.name.clone())
        .ok_or_else(|| format!("节点上没有应用「{app_name}」（远程升级只允许升级已有应用）"))?;
    let path = cmd
        .get("path")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if path.len() > 2048
        || !path.starts_with('/')
        || path.starts_with("//")
        || path.contains("..")
        || path.contains('\\')
        || path.chars().any(|c| c.is_control() || c.is_whitespace())
    {
        return Err("平台下发的下载路径非法（只接受平台相对路径）".to_string());
    }
    let origin = platform_origin(&manager.config())?;
    let url = format!("{origin}{path}");
    let expected_sha = cmd
        .get("sha256")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if expected_sha.len() != 64 || !expected_sha.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err("平台下发的 SHA-256 无效".to_string());
    }
    util::log_format(
        "开始下载应用升级包：{} v{} ← {}",
        &[app_name, version, &redact_url_ticket(&url)],
    );
    let ua = crate::util::user_agent();
    let resp = dhrust::net::http_client::blocking_request_offthread(
        "GET",
        &url,
        &[("User-Agent", ua.as_str())],
        None,
        Vec::new(),
        Duration::from_secs(180),
    )
    .map_err(|e| format!("下载应用包失败（{}）：{e}", redact_url_ticket(&url)))?;
    if resp.status != 200 {
        let body = String::from_utf8_lossy(&resp.body);
        return Err(format!(
            "下载应用包失败：HTTP {}（{}）",
            resp.status,
            body.chars().take(200).collect::<String>()
        ));
    }
    if resp.body.is_empty() {
        return Err("下载应用包失败：响应为空".to_string());
    }
    if resp.body.len() > crate::apppkg::MAX_PACKAGE_SIZE {
        return Err(format!(
            "应用包过大（{} MB，上限 {} MB）",
            resp.body.len() / 1024 / 1024,
            crate::apppkg::MAX_PACKAGE_SIZE / 1024 / 1024
        ));
    }
    let actual = dhrust::sign::sha256_hex(&resp.body);
    if !actual.eq_ignore_ascii_case(&expected_sha) {
        return Err(format!(
            "应用包校验失败：SHA-256 不一致（期望 {}，实际 {}）",
            &expected_sha[..12],
            &actual[..12]
        ));
    }
    let file_name = cmd
        .get("fileName")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .trim()
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("")
        .to_string();
    let file_name = if file_name.is_empty() {
        format!("{app_name}-{version}.zip")
    } else {
        file_name
    };
    let deployed = crate::apppkg::deploy_bytes(
        manager,
        &file_name,
        &resp.body,
        Some(&actual_name),
        None,
        true,
    )?;
    if !deployed.started {
        return Err(format!(
            "应用包已写入，但应用未成功启动（{}）；请查看节点日志与应用日志",
            deployed.program.display()
        ));
    }
    Ok(format!(
        "已部署并启动（{} 个文件，程序 {}）",
        deployed.files,
        deployed.program.display()
    ))
}

/// 日志/回执里隐藏一次性下载票据（`ticket=...`），避免凭据落进日志。
fn redact_url_ticket(url: &str) -> String {
    match url.find("ticket=") {
        Some(pos) => format!("{}ticket=***", &url[..pos]),
        None => url.to_string(),
    }
}

/// 平台 origin（只认 `AutoUpgradeUrl` 的 scheme + authority；https，回环例外允许 http）。
fn platform_origin(cfg: &AgentConfig) -> Result<String, String> {
    let raw = cfg.auto_upgrade_url.trim();
    let Some((scheme, rest)) = raw.split_once("://") else {
        return Err("未配置 AutoUpgradeUrl，无法解析应用包下载地址".to_string());
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("").trim();
    if authority.is_empty() {
        return Err("AutoUpgradeUrl 缺少主机名".to_string());
    }
    let host = dhrust::web::url_host(raw).unwrap_or_default();
    let loopback = host == "localhost"
        || host == "[::1]"
        || host.starts_with("127.")
        || host == "::1";
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "https" && !(scheme == "http" && loopback) {
        return Err(format!(
            "AutoUpgradeUrl 必须是 https（当前 {scheme}://{authority}；仅回环地址允许 http）"
        ));
    }
    if authority.contains('@') {
        return Err("AutoUpgradeUrl 不能包含用户名/口令".to_string());
    }
    Ok(format!("{scheme}://{authority}"))
}

/// 平台下发节点专用令牌（`tokenIssue`）：校验格式 → 写配置 → 立即断开重连（用新令牌）。
///
/// 这样管理员在平台侧签发即可，无需逐台手抄令牌（与分发源配置自动下发同一思路）。
fn apply_node_token(manager: &AppManager, v: &Json) {
    let token = v
        .get("token")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if token.len() != 64 || !token.chars().all(|c| c.is_ascii_hexdigit()) {
        util::log_error("平台下发的节点专用令牌格式无效，已忽略");
        return;
    }
    if manager.config().auto_upgrade_token == token {
        return;
    }
    manager.update_config(|cfg| cfg.auto_upgrade_token = token.clone());
    util::log_info("已接收平台下发的节点专用令牌（配置已更新），即将用新令牌重连");
    if let Some(client) = CURRENT
        .get_or_init(|| std::sync::Mutex::new(None))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
    {
        client.close();
    }
}

/// 执行平台下发的「重启服务器」指令（60 秒冷却：同一条指令重复到达只执行一次）。
///
/// **重启的是整台服务器**（不是 Agent 进程）。执行前先预检（容器环境 / 非 root / 无重启工具），
/// 预检失败立即把原因回报平台——平台会显示在节点行上，不再"点了没反应"。
fn reboot_once(manager: &AppManager) {
    static LAST: OnceLock<std::sync::Mutex<Option<Instant>>> = OnceLock::new();
    let mut last = LAST
        .get_or_init(|| std::sync::Mutex::new(None))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(t) = *last
        && t.elapsed() < Duration::from_secs(60)
    {
        util::log_info("重启指令重复（60 秒冷却内），已忽略");
        return;
    }
    *last = Some(Instant::now());
    drop(last);
    let agent_id = manager.config().agent_id;
    if let Err(e) = crate::sys::reboot_precheck() {
        util::log_error(&format!("拒绝执行系统重启：{e}"));
        send_event(
            &agent_id,
            json!({ "type": "rebootResult", "ok": false, "reason": e }),
        );
        return;
    }
    // 先回报"已受理"：随后整机会断开，平台据此判断指令确实到达了节点
    send_event(
        &agent_id,
        json!({ "type": "rebootResult", "ok": true, "reason": "已受理，主机将在数秒内重启" }),
    );
    util::log_info("平台下发「重启服务器」指令，3 秒后重启本机（整机重启，不是重启 Agent）");
    crate::sys::reboot_machine(&agent_id);
}

/// 平台下发分发源配置（`storeConfig`）：插件源目录地址 + 签名公钥。
///
/// 只覆盖**非空且确有变化**的项，并先按节点自己的规则校验（地址必须 https 或 127.0.0.1，
/// 公钥必须能解析），避免平台推一份本地用不了的配置把节点改坏。写盘后立即生效。
fn apply_store_config(manager: &AppManager, v: &Json) {
    let url = v
        .get("pluginStoreUrl")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let pk = v
        .get("pluginStorePubKey")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let url = if !url.is_empty() && crate::plugins::is_allowed_url(&url) {
        url
    } else {
        String::new()
    };
    let pk = if !pk.is_empty() && dhrust::plugin::parse_pubkey(&pk).is_ok() {
        pk
    } else {
        String::new()
    };
    if url.is_empty() && pk.is_empty() {
        return;
    }
    let cur = manager.config();
    let url_diff = !url.is_empty() && cur.plugin_store_url != url;
    let pk_diff = !pk.is_empty() && cur.plugin_store_pubkey != pk;
    if !url_diff && !pk_diff {
        return;
    }
    manager.update_config(|cfg| {
        if url_diff {
            cfg.plugin_store_url = url.clone();
        }
        if pk_diff {
            cfg.plugin_store_pubkey = pk.clone();
        }
    });
    util::log_format(
        "已同步平台下发的分发源配置（{}）",
        &[if url.is_empty() { "仅签名公钥" } else { &url }],
    );
}

/// `register` 报文（连接建立后发送）。
fn build_register(manager: &AppManager) -> String {
    let services = manager.list();
    with_panel_meta(
        manager,
        json!({
            "type": "register",
            "agentId": manager.config().agent_id,
            "name": dhrust::sys::machine::hostname(),
            "version": env!("CARGO_PKG_VERSION"),
            "platform": crate::self_upgrade::current_platform(),
            "os": dhrust::sys::machine::os_description(),
            "hostname": dhrust::sys::machine::hostname(),
            // 子应用清单：每次接入都全量上报（平台据此远程升级指定应用）
            "apps": apps_payload(&services, true),
        }),
    )
}

/// 子应用清单（平台「服务器节点」展示与远程应用升级选择用）。
///
/// 只上报**无敏感信息**的字段：名称/程序/模式/启用/运行状态；不传启动参数与环境变量
/// （可能含口令/令牌）。`version` 由平台在远程部署后记录，Agent 不额外读盘探测。
fn apps_payload(
    services: &[(crate::config::AppConfig, crate::app::AppStatus)],
    force: bool,
) -> Json {
    // 先用轻量签名判断变化（心跳 60 秒一次；无变化时**不构造 JSON**，省带宽也省分配）
    let mut sig = String::new();
    for (cfg, st) in services {
        sig.push_str(&cfg.name);
        sig.push('|');
        sig.push_str(&cfg.file_name);
        sig.push('|');
        sig.push_str(&cfg.mode_text());
        sig.push('|');
        sig.push_str(if cfg.enable { "1" } else { "0" });
        sig.push(if st.running { '1' } else { '0' });
        sig.push('|');
        sig.push_str(&st.pid.to_string());
        sig.push('|');
        sig.push_str(&st.health_fails.to_string());
        sig.push(';');
    }
    let cache = APPS_SIGNATURE.get_or_init(|| std::sync::Mutex::new(String::new()));
    let mut last = cache.lock().unwrap_or_else(|e| e.into_inner());
    if !force && *last == sig {
        return Json::Null;
    }
    *last = sig;
    drop(last);
    Json::Array(
        services
            .iter()
            .map(|(cfg, st)| {
                json!({
                    "name": cfg.name,
                    "file": cfg.file_name,
                    "mode": cfg.mode_text(),
                    "enable": cfg.enable,
                    "running": st.running,
                    "pid": st.pid,
                    "healthFails": st.health_fails,
                })
            })
            .collect(),
    )
}

/// 平台「服务器节点」展示用的面板元数据：端口 / 是否允许远程访问 / 内置管理员状态。
///
/// **不含密码与哈希**：只上报「是否仍为出厂默认凭据 admin/admin」这一布尔判定
/// （与面板顶部横幅同口径），平台侧据此给节点打「默认密码」红标。
fn panel_meta(manager: &AppManager) -> Json {
    let cfg = manager.config();
    json!({
        "webPort": cfg.local_port,
        "remoteAccess": !cfg.local_only,
        "webUser": cfg.web_user_name.trim(),
        "defaultPassword": crate::webpanel::uses_default_credentials(&cfg),
    })
}

/// 把面板元数据并入报文（键名平铺，与平台 `agent_ws` 解析一致）。
fn with_panel_meta(manager: &AppManager, base: Json) -> String {
    let mut v = base;
    if let (Some(obj), Json::Object(meta)) = (v.as_object_mut(), panel_meta(manager)) {
        for (k, val) in meta {
            obj.insert(k, val);
        }
    }
    // 统一管控面板子用户：`caps` + 用户清单摘要（平台判断是否需要同步，无需拉全量）
    crate::panel_users::merge_summary(&crate::util::base_dir(), &mut v);
    v.to_string()
}

/// `heartbeat` 报文（60 秒周期；机器数据 + 子服务状态）。
fn build_heartbeat(manager: &AppManager) -> String {
    let snap = crate::sampler::current();
    // sys::memory_info 返回字节（与 webpanel 同口径；面板侧换算 MB）
    let (mem_total, mem_avail) = crate::sys::memory_info().unwrap_or((0, 0));
    let mem_used_mb = mem_total.saturating_sub(mem_avail) / 1024 / 1024;
    let mem_total_mb = mem_total / 1024 / 1024;
    let mut disk_total = 0u64;
    let mut disk_used = 0u64;
    for d in crate::sys::disks() {
        if d.ready && d.total > 0 {
            disk_total += d.total;
            disk_used += d.total.saturating_sub(d.free);
        }
    }
    let services = manager.list();
    let running = services.iter().filter(|(_, s)| s.running).count();
    // 子应用清单：仅名称/启用/运行状态/pid 变化时携带（平台侧 null = 保留旧值）
    let apps = apps_payload(&services, false);
    let uptime = STARTED.get().map(|t| t.elapsed().as_secs()).unwrap_or(0);
    let pid = std::process::id();
    let (site_summary, sites_changed) = {
        let base = crate::util::base_dir();
        // 带缓存：配置/白名单未变时只做元数据比对，且**不重复上传站点清单**（只置 sites:null）
        crate::sites::summary_cached(&manager.config(), &base)
    };
    with_panel_meta(
        manager,
        json!({
            "type": "heartbeat",
            "agentId": manager.config().agent_id,
            "version": env!("CARGO_PKG_VERSION"),
            "cpuRate": snap.cpu_rate.unwrap_or(0.0),
            "memoryUsedMB": mem_used_mb,
            "memoryTotalMB": mem_total_mb,
            "diskUsedMB": disk_used / 1_048_576,
            "diskTotalMB": disk_total / 1_048_576,
            "uptimeSecs": uptime,
            "processMemoryMB": crate::sys::memory_mb(pid).unwrap_or(0),
            "servicesRunning": running,
            "servicesTotal": services.len(),
            "apps": apps,
            // 仅在站点/白名单变化时携带清单（平台侧 null = 保留旧值），省带宽
            "sites": if sites_changed { site_summary } else { serde_json::Value::Null },
        }),
    )
}
