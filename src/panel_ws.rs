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
/// 是否处于「等待平台批准」状态（配对模式，平台已回 `pairPending`）。
static PAIR_PENDING: AtomicBool = AtomicBool::new(false);
/// 当前会话的客户端句柄（平台判定标识重复时用于立即断开触发重连）。
static CURRENT: OnceLock<std::sync::Mutex<Option<WsClient>>> = OnceLock::new();

/// 当前是否已连接平台（令牌未配置/连接中均返回 false）。
pub fn connected() -> bool {
    CONNECTED.load(Ordering::Relaxed)
}

/// 是否启用（升级源地址非空，且「有令牌」或「开配对」）。
pub fn enabled(cfg: &AgentConfig) -> bool {
    !cfg.auto_upgrade_url.trim().is_empty()
        && (!cfg.auto_upgrade_token.trim().is_empty() || cfg.pairing_enabled)
}

/// 当前是否处于「已接入、等待平台批准」状态（面板展示用）。
pub fn pairing_pending() -> bool {
    PAIR_PENDING.load(Ordering::Relaxed)
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
        // 消费首 tick（避免建连即发心跳；配置检查首 tick 保留）
        hb.tick().await;
        loop {
            tokio::select! {
                _ = hb.tick() => {
                    // 配对（待批准）期间不发心跳：平台此时不接收任何上报，省带宽也省日志
                    if client.is_connected() && !PAIR_PENDING.load(Ordering::Relaxed) {
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
            "checkUpgrade" => {
                util::log_info("平台下发「立即检查升级」指令，开始检查");
                crate::self_upgrade::trigger(msg_mgr.config(), true);
            }
            _ => {}
        }
    });
    let on_disconnected = Arc::new(move |reason: String| {
        CONNECTED.store(false, Ordering::Relaxed);
        PAIR_PENDING.store(false, Ordering::Relaxed);
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
fn reboot_once(_manager: &AppManager) {
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
    util::log_info("平台下发「重启服务器」指令，3 秒后重启本机");
    crate::sys::reboot_machine();
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
        }),
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
    let uptime = STARTED.get().map(|t| t.elapsed().as_secs()).unwrap_or(0);
    let pid = std::process::id();
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
        }),
    )
}
