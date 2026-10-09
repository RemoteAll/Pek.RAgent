//! 平台实时通道（Pek.RAgent → Pek.RPanlServer WebSocket）。
//!
//! 配置「平台接入令牌」（`AutoUpgradeToken`）后启用：
//! - 上行：`register`（节点信息）/ `heartbeat`（机器数据，60 秒）——平台「服务器节点」页可见；
//! - 下行：`checkUpgrade`（平台下发"立即检查升级"→ 触发一次强制自升级检查）。
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
/// 当前会话的客户端句柄（平台判定标识重复时用于立即断开触发重连）。
static CURRENT: OnceLock<std::sync::Mutex<Option<WsClient>>> = OnceLock::new();

/// 当前是否已连接平台（令牌未配置/连接中均返回 false）。
pub fn connected() -> bool {
    CONNECTED.load(Ordering::Relaxed)
}

/// 是否启用（升级源地址与接入令牌都非空）。
pub fn enabled(cfg: &AgentConfig) -> bool {
    !cfg.auto_upgrade_url.trim().is_empty() && !cfg.auto_upgrade_token.trim().is_empty()
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
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    util::log_error(&format!("平台实时通道运行时创建失败：{e}"));
                    return;
                }
            };
            rt.block_on(run(manager));
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
        let full = format!("{url}?token={}", dhrust::web::url_encode(&token));
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
        // 消费首 tick（避免建连即发心跳；配置检查首 tick 保留）
        hb.tick().await;
        loop {
            tokio::select! {
                _ = hb.tick() => {
                    if client.is_connected() {
                        let _ = client.send_text(build_heartbeat(&manager));
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
    });
    let msg_mgr = manager.clone();
    let on_message = Arc::new(move |msg: WsMessage| {
        let Ok(v) = serde_json::from_str::<Json>(&msg.text) else {
            return;
        };
        match v.get("type").and_then(|t| t.as_str()).unwrap_or("") {
            "hello" => util::log_info("平台实时通道已建立（节点已接入）"),
            "idConflict" => {
                util::log_info("平台检测到节点标识与其他服务器重复，重新生成标识");
                regenerate_agent_id(&msg_mgr);
            }
            "checkUpgrade" => {
                util::log_info("平台下发「立即检查升级」指令，开始检查");
                crate::self_upgrade::trigger(msg_mgr.config(), true);
            }
            _ => {}
        }
    });
    let on_disconnected = Arc::new(move |reason: String| {
        CONNECTED.store(false, Ordering::Relaxed);
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

/// `register` 报文（连接建立后发送）。
fn build_register(manager: &AppManager) -> String {
    json!({
        "type": "register",
        "agentId": manager.config().agent_id,
        "name": dhrust::sys::machine::hostname(),
        "version": env!("CARGO_PKG_VERSION"),
        "platform": crate::self_upgrade::current_platform(),
        "os": dhrust::sys::machine::os_description(),
        "hostname": dhrust::sys::machine::hostname(),
    })
    .to_string()
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
    let uptime = STARTED.get().map(|t| t.elapsed().as_secs()).unwrap_or(0);
    let pid = std::process::id();
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
    })
    .to_string()
}
