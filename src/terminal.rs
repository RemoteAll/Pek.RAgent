//! 在线终端（面板胶水层）：鉴权 / 审计 / WebSocket 转发；PTY 引擎在 `dhrust::term`。
//!
//! - `GET /star/termWs?sid=&cols=&rows=[&token=]`：WebSocket 会话（升级后双向流式传输：服务端
//!   发 UTF-8 文本帧=终端输出；客户端发 JSON——`{"t":"i","d":"..."}` 键盘输入、`{"t":"r","c":..,"r":..}`
//!   调整尺寸）
//! - `POST /star/termReset {"sid"}`：重置会话（杀掉 shell；前端自动重连生成新会话）
//!
//! 与宝塔终端一致的真终端体验：单区域流式输出、光标闪烁、命令回显、支持 `vim`/`top`
//! 等交互式程序与全键盘（Ctrl+C 等）。ConPTY/PTY 天然 UTF-8。
//!
//! 安全：命令以本程序（服务账户）权限运行（Linux 通常 root，与 SSH 等效）；连接/断开与
//! **每一条完整命令行**写入操作审计（不记录逐键输入，疑似口令输入不采集）；
//! 危险命令受 `TerminalDangerPolicy` 控制（默认 `confirm`：管理员二次确认、子用户禁止），
//! 子用户还可由 `TerminalAllowedCommands` 限制为固定命令白名单；
//! 配置项 `TerminalEnabled` 可整体关闭；菜单权限 `terminal` 可授予子用户。
//!
//! 输入受控细节：管理员保留完整行编辑（↑ 历史、方向键、Tab）；子用户屏蔽方向键/Tab/
//! 光标类控制键，保证服务端行缓冲与 shell 实际行一致——否则可用历史记录或光标插入
//! 拼出白名单之外的命令。确认令牌一次性、120 秒过期，且只能由同一会话的同一用户消费。
//!
//! 本文件仅保留面板相关职责；会话/进程/回放/订阅等通用能力见 `dhrust::term::Terminal`。

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use dhrust::net::controller::{arg, json_body, json_error, json_result, ActionResult};
use dhrust::net::http::{json_escape, HttpOutcome, HttpResponse};
use dhrust::net::panel_auth::client_ip;
use dhrust::net::router::Ctx;
use dhrust::net::ws::{WsServerConn, WsServerHooks, WsServerMessage};
use dhrust::term::{valid_sid, TermOptions, Terminal};
use serde_json::{json, Value as Json};

use crate::audit;
use crate::config::AgentConfig;
use crate::util;
use crate::webpanel::{WebPanel, PERM_TERMINAL};

/// 全局终端引擎（首次连接时按面板基础目录初始化——起始目录 = 程序目录）。
static ENGINE: OnceLock<Arc<Terminal>> = OnceLock::new();

fn engine(base: &std::path::Path) -> &'static Arc<Terminal> {
    ENGINE.get_or_init(|| {
        Terminal::new(TermOptions {
            cwd: Some(base.to_path_buf()),
            ..Default::default()
        })
    })
}

// ————— WebSocket 入口（server.rs 原始路由；认证在本函数内完成） —————

/// 新会话进入终端时的「服务器概况」（相当于登录 MOTD）。
///
/// 只在新会话生成一次（重连回放不重复）；数据从本机采集，**不往 shell 注入命令**，
/// 因此 Linux 的 bash 与 Windows 的 PowerShell/cmd 表现一致，也不污染命令历史。
fn server_banner(manager: &crate::manager::AppManager) -> String {
    const NL: &str = "\r\n";
    const DIM: &str = "\x1b[90m";
    const CYAN: &str = "\x1b[36m";
    const RESET: &str = "\x1b[0m";

    let mut s = String::new();
    s.push_str(NL);
    s.push_str(&format!("{CYAN}━━━ Pek.RAgent 在线终端 ━━━{RESET}{NL}"));
    s.push_str(&format!(
        "{DIM}主机{RESET}   {}{NL}{DIM}系统{RESET}   {} · {}{NL}",
        dhrust::sys::machine::hostname(),
        dhrust::sys::machine::os_description(),
        std::env::consts::ARCH
    ));

    // CPU：不用请求差分口径（首次要 200ms 双采样），只报核数/型号/负载
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(0);
    let mut cpu = format!("{cores} 核");
    if let Some(model) = dhrust::sys::machine::cpu_model()
        && !model.is_empty()
    {
        cpu.push_str(&format!(" · {model}"));
    }
    if let Some((l1, l5, l15)) = dhrust::sys::monitor::load_average() {
        cpu.push_str(&format!(" · 负载 {l1:.2}/{l5:.2}/{l15:.2}"));
    }
    s.push_str(&format!("{DIM}CPU{RESET}    {cpu}{NL}"));

    if let Some((total, avail)) = dhrust::sys::machine::memory_info() {
        let used = total.saturating_sub(avail);
        let pct = used.checked_mul(100).and_then(|v| v.checked_div(total)).unwrap_or(0);
        s.push_str(&format!(
            "{DIM}内存{RESET}   {} / {}（{pct}%）{NL}",
            dhrust::web::format_bytes(used),
            dhrust::web::format_bytes(total)
        ));
    }

    let disks: Vec<String> = dhrust::sys::machine::disks()
        .into_iter()
        .filter(|d| d.total > 0)
        .take(3)
        .map(|d| {
            let used = d.total.saturating_sub(d.free);
            let pct = used.checked_mul(100).and_then(|v| v.checked_div(d.total)).unwrap_or(0);
            format!(
                "{} {} / {}（{pct}%）",
                d.name,
                dhrust::web::format_bytes(used),
                dhrust::web::format_bytes(d.total)
            )
        })
        .collect();
    if !disks.is_empty() {
        s.push_str(&format!("{DIM}磁盘{RESET}   {}{NL}", disks.join("  ·  ")));
    }

    if let Some(iface) = dhrust::sys::machine::network_interfaces()
        .into_iter()
        .find(|i| i.up && !i.ips.is_empty())
    {
        s.push_str(&format!(
            "{DIM}网络{RESET}   {} {}{NL}",
            iface.name,
            iface.ips.join(", ")
        ));
    }

    // Web 服务器与站点数（复用缓存盘点：无变化时零磁盘读取）
    let (inv, _) = crate::sites::inventory_cached(&manager.config(), manager.base());
    let nginx = inv
        .sites
        .iter()
        .filter(|x| x.kind == "nginx" && !x.system)
        .count();
    let iis = inv
        .sites
        .iter()
        .filter(|x| x.kind == "iis" && !x.system)
        .count();
    let mut web: Vec<String> = Vec::new();
    if nginx > 0 || inv.web.iter().any(|w| w == "nginx") {
        web.push(format!("nginx（{nginx} 个站点）"));
    }
    if iis > 0 || inv.web.iter().any(|w| w == "iis") {
        web.push(format!("IIS（{iis} 个站点）"));
    }
    if web.is_empty() {
        web.push(if cfg!(windows) {
            "未检测到 IIS".to_string()
        } else {
            "未检测到 nginx".to_string()
        });
    }
    s.push_str(&format!("{DIM}Web{RESET}    {}{NL}", web.join(" · ")));

    let list = manager.list();
    let running = list.iter().filter(|(_, st)| st.running).count();
    let mins = dhrust::sys::process::uptime().as_secs() / 60;
    let uptime = if mins >= 60 {
        format!("{}h{}m", mins / 60, mins % 60)
    } else {
        format!("{mins}m")
    };
    s.push_str(&format!(
        "{DIM}代理{RESET}   Pek.RAgent v{} · 已运行 {uptime} · 子服务 {running}/{} · {}{NL}",
        env!("CARGO_PKG_VERSION"),
        list.len(),
        if cfg!(windows) { "Windows" } else { "Linux" }
    ));
    s.push_str(&format!(
        "{DIM}提示{RESET}   真 PTY 会话，可直接使用 vim/top 等交互程序；空闲 30 分钟自动回收，刷新页面可恢复最近输出。{NL}{NL}"
    ));
    s
}

/// `GET /star/termBanner`：服务器概况文本（前端在终端清屏后写入，相当于登录 MOTD）。
///
/// 数据全部本机采集、**不往 shell 注入命令**，Linux 的 bash 与 Windows 的 PowerShell/cmd
/// 表现一致，也不污染命令历史。
pub fn term_banner(panel: &WebPanel, _ctx: &Ctx) -> ActionResult {
    json_result(
        0,
        "",
        Some(json!({ "text": server_banner(panel.manager.as_ref()) })),
    )
}

/// `GET /star/termWs`：WebSocket 升级（真 PTY 会话；认证支持查询参数 `token=` 回退）。
pub fn term_ws(panel: &WebPanel, ctx: &Ctx) -> HttpOutcome {
    let Some(principal) = panel.principal_with_query_token(ctx) else {
        return HttpOutcome::Response(json_err(401, "Unauthorized"));
    };
    if !principal.allowed(PERM_TERMINAL) {
        return HttpOutcome::Response(json_err(403, "没有权限执行该操作"));
    }
    let cfg = panel.config();
    if !cfg.terminal_enabled {
        return HttpOutcome::Response(json_err(
            400,
            "在线终端未启用（「配置」页 →「在线终端」开关）",
        ));
    }
    if !ctx.req.is_websocket_upgrade() {
        return HttpOutcome::Response(json_err(400, "该地址仅用于 WebSocket 连接"));
    }
    let sid = arg(ctx, "sid").unwrap_or_default().trim().to_string();
    if !valid_sid(&sid) {
        return HttpOutcome::Response(json_err(400, "会话标识无效"));
    }
    let cols = arg(ctx, "cols")
        .and_then(|v| v.trim().parse::<u16>().ok())
        .unwrap_or(120);
    let rows = arg(ctx, "rows")
        .and_then(|v| v.trim().parse::<u16>().ok())
        .unwrap_or(30);

    // 取或建会话（引擎内锁内快进快出；尺寸由引擎归一化）
    let term = engine(panel.base());
    let existed = term.is_alive(&sid);
    if let Err(e) = term.ensure(&sid, &principal.name, cols, rows) {
        return HttpOutcome::Response(json_err(ensure_error_status(&e), &e));
    }
    if !existed {
        util::log_format(
            "在线终端已连接（{}，{}x{}）",
            &[&principal.name, &cols.to_string(), &rows.to_string()],
        );
    }
    // 新会话才生成「服务器概况」（重连不重复采集）——由前端清屏后写入，不与 PTY 输出交错
    let _ = existed;

    // 审计：打开终端会话（有意不记录击键内容）
    audit::record(
        panel.base(),
        &audit::AuditEntry {
            category: None,
            user: principal.name.clone(),
            ip: client_ip(ctx),
            action: "termWs".to_string(),
            title: "打开在线终端".to_string(),
            method: "GET".to_string(),
            path: "/star/termWs".to_string(),
            detail: format!("sid={sid} 尺寸={cols}x{rows}"),
            success: true,
            code: 0,
            message: String::new(),
            elapsed_ms: 0,
        },
    );

    // —— WS 钩子 ——
    let term_open = term.clone();
    let sid_open = sid.clone();
    let on_open = Arc::new(move |conn: WsServerConn| {
        // 重连回放最近输出（分片发送；每帧 ≤ 200KB）
        let replay = term_open.replay(&sid_open);
        if !replay.is_empty() {
            for chunk in replay.chunks(200_000) {
                let _ = conn.send_text(String::from_utf8_lossy(chunk).into_owned());
            }
        }
        // 订阅输出 → 转发线程（std 线程，无 tokio 依赖；重新订阅自动替换旧连接）
        let Ok(rx) = term_open.subscribe(&sid_open) else {
            conn.close();
            return;
        };
        let c = conn.clone();
        let _ = std::thread::Builder::new()
            .name("term-ws-fwd".to_string())
            .spawn(move || {
                while let Ok(data) = rx.recv() {
                    if !c.send_text(String::from_utf8_lossy(&data).into_owned()) {
                        break;
                    }
                }
                // 通道断开（会话重置/退出）或本连接被替换 → 关闭连接
                c.close();
            });
    });

    // —— 输入受控上下文（配置每次取最新，策略热生效；无需重连） ——
    let term_msg = term.clone();
    let sid_msg = sid.clone();
    let base_msg = panel.base().to_path_buf();
    let manager_msg = panel.manager.clone();
    let user_msg = principal.name.clone();
    let admin_msg = principal.is_admin;
    let ip_msg = client_ip(ctx);
    let on_message = Arc::new(move |msg: WsServerMessage| {
        let Ok(v) = serde_json::from_str::<Json>(&msg.text) else {
            return;
        };
        match v.get("t").and_then(|t| t.as_str()).unwrap_or("") {
            "i" => {
                if let Some(d) = v.get("d").and_then(|d| d.as_str()) {
                    let cfg = manager_msg.config();
                    match handle_input(
                        &base_msg, &user_msg, admin_msg, &ip_msg, &sid_msg, &cfg, d,
                    ) {
                        InputFlow::Forward(out) => {
                            if !out.is_empty() {
                                term_msg.write(&sid_msg, out.as_bytes());
                            }
                        }
                        InputFlow::AskConfirm {
                            forward,
                            token,
                            command,
                            reason,
                        } => {
                            if !forward.is_empty() {
                                term_msg.write(&sid_msg, forward.as_bytes());
                            }
                            // 控制帧：以 U+0001 开头，前端据此与终端输出区分
                            let payload = json!({
                                "t": "askConfirm",
                                "token": token,
                                "cmd": command,
                                "reason": reason,
                            });
                            let _ = msg.conn.send_text(format!("\u{1}{payload}"));
                        }
                    }
                }
            }
            "cf" => {
                // 危险命令二次确认：只认本会话、本用户、未过期的令牌
                let token = v.get("token").and_then(|x| x.as_str()).unwrap_or("");
                if let Some((cmd, reason)) = take_confirmed(&sid_msg, &user_msg, token) {
                    term_msg.write(&sid_msg, format!("{cmd}\r").as_bytes());
                    audit::record(
                        &base_msg,
                        &audit::AuditEntry {
                            category: None,
                            user: user_msg.clone(),
                            ip: ip_msg.clone(),
                            action: "termCommand".to_string(),
                            title: "终端命令（二次确认后执行）".to_string(),
                            method: "WS".to_string(),
                            path: "/star/termWs".to_string(),
                            detail: format!(
                                "sid={sid_msg} 危险={reason} 命令={}",
                                dhrust::text::truncate_chars(&cmd, 500)
                            ),
                            success: true,
                            code: 0,
                            message: format!("danger={reason};confirmed"),
                            elapsed_ms: 0,
                        },
                    );
                }
            }
            "r" => {
                let c = v.get("c").and_then(|x| x.as_u64()).unwrap_or(0) as u16;
                let r = v.get("r").and_then(|x| x.as_u64()).unwrap_or(0) as u16;
                term_msg.resize(&sid_msg, c, r);
            }
            _ => {}
        }
    });

    let base_close = panel.base().to_path_buf();
    let user_close = principal.name.clone();
    let ip_close = client_ip(ctx);
    let on_close = Arc::new(move |_conn: WsServerConn, reason: String| {
        let reason = dhrust::text::truncate_chars(&reason, 120);
        util::log_format(
            "在线终端已断开（{}）：{}",
            &[&user_close, &reason],
        );
        audit::record(
            &base_close,
            &audit::AuditEntry {
                category: None,
                user: user_close.clone(),
                ip: ip_close.clone(),
                action: "termClose".to_string(),
                title: "关闭在线终端".to_string(),
                method: "GET".to_string(),
                path: "/star/termWs".to_string(),
                detail: format!("sid={sid} 原因={reason}"),
                success: true,
                code: 0,
                message: String::new(),
                elapsed_ms: 0,
            },
        );
    });

    HttpOutcome::WebSocket(WsServerHooks {
        on_open: Some(on_open),
        on_message: Some(on_message),
        on_binary: None,
        on_close: Some(on_close),
    })
}

/// `POST /star/termReset {"sid"}`：重置会话（杀 shell + 断开当前 WS；前端自动重连出新会话）。
pub fn term_reset(panel: &WebPanel, ctx: &Ctx) -> ActionResult {
    if !panel.check_auth(ctx) {
        return json_error(401, "Unauthorized");
    }
    let sid = json_body(ctx)
        .and_then(|b| {
            b.get("sid")
                .and_then(|v| v.as_str())
                .map(|s| s.trim().to_string())
        })
        .unwrap_or_default();
    if !valid_sid(&sid) {
        return json_error(400, "会话标识无效");
    }
    engine(panel.base()).reset(&sid);
    util::log_format("在线终端会话已重置：{}", &[&sid]);
    json_result(0, "会话已重置", None)
}

// ————— 输入受控：命令审计 + 危险命令策略 + 子用户白名单 —————

/// 二次确认有效期（超时自动作废，需重新输入命令）。
const CONFIRM_TTL: Duration = Duration::from_secs(120);

/// 单会话输入状态：当前行缓冲 + 待确认的危险命令。
#[derive(Default)]
struct GuardState {
    buf: String,
    pending: Option<PendingCommand>,
}

struct PendingCommand {
    token: String,
    command: String,
    reason: String,
    user: String,
    expire: Instant,
}

fn guard_map() -> &'static Mutex<HashMap<String, GuardState>> {
    static MAP: OnceLock<Mutex<HashMap<String, GuardState>>> = OnceLock::new();
    MAP.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 一段输入的处理结果。
enum InputFlow {
    /// 转发给 PTY（原样，或已附加清行/拦截提示）
    Forward(String),
    /// 危险命令：已清行并附加提示，等待前端确认；同时需下发控制帧
    AskConfirm {
        forward: String,
        token: String,
        command: String,
        reason: String,
    },
}

enum LineVerdict {
    Run { danger: Option<&'static str> },
    Blocked(String),
    Confirm(&'static str),
}

/// 处理一段键盘输入：解析完整命令行 → 审计 / 白名单 / 危险命令策略。
///
/// 设计要点：
/// - **只记命令行，不记逐键**：缓冲到回车才算一条命令，疑似口令输入因不像命令而不记录；
/// - 拦截时不把回车交给 shell：先发 `Ctrl+U` 清掉已回显的行，再输出受控提示；
/// - 确认通过后由前端回发 `cf` 帧，服务端才写入命令并回车（令牌一次性、120 秒过期）。
fn handle_input(
    base: &Path,
    user: &str,
    is_admin: bool,
    ip: &str,
    sid: &str,
    cfg: &AgentConfig,
    data: &str,
) -> InputFlow {
    let (mut buf, mut pending) = {
        let mut map = guard_map().lock().unwrap();
        let state = map.entry(sid.to_string()).or_default();
        if state
            .pending
            .as_ref()
            .map(|p| p.expire <= Instant::now())
            .unwrap_or(false)
        {
            state.pending = None;
        }
        (std::mem::take(&mut state.buf), state.pending.take())
    };

    let mut out = String::new();
    let mut asking: Option<PendingCommand> = None;
    let mut in_escape = false;

    for ch in data.chars() {
        match ch {
            '\x1b' => {
                in_escape = true;
                // 子用户：丢弃方向键/功能键等转义序列——防止用历史记录（↑）绕过白名单校验
                if is_admin {
                    out.push(ch);
                }
            }
            '\r' | '\n' => {
                let line = buf.trim().to_string();
                buf.clear();
                in_escape = false;
                if line.is_empty() {
                    out.push(ch);
                    continue;
                }
                // 新的一行结束，旧的待确认命令作废（防「确认」落到另一条命令上）
                pending = None;
                match decide_line(cfg, is_admin, &line) {
                    LineVerdict::Run { danger } => {
                        out.push(ch);
                        if cfg.terminal_command_audit && is_command_like(&line) {
                            CommandAudit {
                                base,
                                user,
                                ip,
                                sid,
                                cmd: &line,
                            }
                            .record(true, danger, "", "终端命令");
                        }
                    }
                    LineVerdict::Blocked(reason) => {
                        out.push('\x15');
                        out.push_str(&control_notice("\x1b[31m", "已阻止", &reason, &line));
                        out.push_str("\r\n");
                        if cfg.terminal_command_audit {
                            CommandAudit {
                                base,
                                user,
                                ip,
                                sid,
                                cmd: &line,
                            }
                            .record(false, None, &reason, "终端命令（已阻止）");
                        }
                    }
                    LineVerdict::Confirm(reason) => {
                        out.push('\x15');
                        out.push_str(&control_notice("\x1b[33m", "需二次确认", reason, &line));
                        out.push_str("\r\n");
                        let token = dhrust::sign::getrand(24);
                        asking = Some(PendingCommand {
                            token,
                            command: line,
                            reason: reason.to_string(),
                            user: user.to_string(),
                            expire: Instant::now() + CONFIRM_TTL,
                        });
                    }
                }
            }
            '\x7f' | '\x08' => {
                buf.pop();
                out.push(ch);
                in_escape = false;
            }
            '\t' => {
                // 子用户：不转发 Tab（补全会插入缓冲之外的字符，导致白名单校验失真）
                if is_admin {
                    out.push(ch);
                }
                in_escape = false;
            }
            '\x15' | '\x03' => {
                // Ctrl+U 清行 / Ctrl+C 中断：命令作废
                buf.clear();
                out.push(ch);
                in_escape = false;
            }
            c if (c as u32) < 0x20 => {
                // 其余控制键（Ctrl+A/E/K/W 等光标或删除操作）：子用户不转发，
                // 否则 shell 行内容会与本地缓冲不一致，可能绕过白名单/危险命令校验。
                if is_admin {
                    out.push(c);
                }
            }
            c => {
                if in_escape {
                    if is_admin {
                        out.push(c);
                    }
                    if c.is_ascii_alphabetic() || c == '~' {
                        in_escape = false;
                    }
                } else {
                    buf.push(c);
                    out.push(c);
                }
            }
        }
    }

    let ask_payload = asking.as_ref().map(|p| {
        (
            p.token.clone(),
            p.command.clone(),
            p.reason.clone(),
        )
    });
    let next_pending = asking.or(pending);
    {
        let mut map = guard_map().lock().unwrap();
        let state = map.entry(sid.to_string()).or_default();
        state.buf = buf;
        state.pending = next_pending;
    }

    match ask_payload {
        Some((token, command, reason)) => InputFlow::AskConfirm {
            forward: out,
            token,
            command,
            reason,
        },
        None => InputFlow::Forward(out),
    }
}

/// 取出并消费一次性确认令牌（校验会话、用户、有效期）。
fn take_confirmed(sid: &str, user: &str, token: &str) -> Option<(String, String)> {
    if token.is_empty() {
        return None;
    }
    let mut map = guard_map().lock().unwrap();
    let state = map.get_mut(sid)?;
    let valid = state
        .pending
        .as_ref()
        .map(|p| {
            p.expire > Instant::now()
                && p.token == token
                && p.user.eq_ignore_ascii_case(user.trim())
        })
        .unwrap_or(false);
    if !valid {
        if state
            .pending
            .as_ref()
            .map(|p| p.expire <= Instant::now())
            .unwrap_or(false)
        {
            state.pending = None;
        }
        return None;
    }
    let p = state.pending.take()?;
    Some((p.command, p.reason))
}

/// 命令策略判定：危险命令（先判，防止命令外形的注入绕过）→ 白名单（仅子用户）。
fn decide_line(cfg: &AgentConfig, is_admin: bool, line: &str) -> LineVerdict {
    // 危险特征优先：fork 炸弹等不以普通命令开头的内容也要拦
    if let Some(reason) = danger_reason(line) {
        return match cfg.terminal_danger_policy.as_str() {
            "allow" => LineVerdict::Run {
                danger: Some(reason),
            },
            "block" => LineVerdict::Blocked(reason.to_string()),
            // confirm：管理员二次确认，子用户直接禁止（子用户通常不承担系统级操作）
            _ => {
                if is_admin {
                    LineVerdict::Confirm(reason)
                } else {
                    LineVerdict::Blocked(format!("危险命令（{reason}），子用户禁止执行"))
                }
            }
        };
    }
    if !is_command_like(line) {
        return LineVerdict::Run { danger: None };
    }
    let list = cfg.terminal_allowed_commands.trim();
    if !is_admin && !list.is_empty() && !whitelist_allows(line, list) {
        return LineVerdict::Blocked("不在允许的命令白名单内".to_string());
    }
    LineVerdict::Run { danger: None }
}

/// 命令审计上下文（字段较多，收敛为结构体避免参数过长）。
struct CommandAudit<'a> {
    base: &'a Path,
    user: &'a str,
    ip: &'a str,
    sid: &'a str,
    cmd: &'a str,
}

impl CommandAudit<'_> {
    fn record(&self, success: bool, danger: Option<&str>, note: &str, title: &str) {
        let base = self.base;
        let user = self.user;
        let ip = self.ip;
        let sid = self.sid;
        let cmd = self.cmd;
        let mut message = String::new();
        if let Some(d) = danger {
            message.push_str(&format!("danger={d}"));
        }
        if !note.is_empty() {
            if !message.is_empty() {
                message.push(';');
            }
            message.push_str(note);
        }
        audit::record(
            base,
            &audit::AuditEntry {
                category: None,
                user: user.to_string(),
                ip: ip.to_string(),
                action: "termCommand".to_string(),
                title: title.to_string(),
                method: "WS".to_string(),
                path: "/star/termWs".to_string(),
                detail: format!(
                    "sid={sid} 命令={}",
                    dhrust::text::truncate_chars(cmd.trim(), 500)
                ),
                success,
                code: if success { 0 } else { 403 },
                message,
                elapsed_ms: 0,
            },
        );
    }
}

/// 终端内的受控提示（不带控制帧；前端弹窗另行下发）。
fn control_notice(color: &str, kind: &str, reason: &str, cmd: &str) -> String {
    format!(
        "{color}[受控] {kind}：{reason}\x1b[0m 命令：{}\x1b[0m",
        dhrust::text::truncate_chars(cmd.trim(), 200)
    )
}

/// 常见 shell / 系统命令（用于「像不像一条命令」判断——口令、交互输入不会被误记）。
const SHELL_COMMANDS: &[&str] = &[
    "ls", "ll", "cd", "pwd", "cat", "echo", "printf", "cp", "mv", "rm", "mkdir", "rmdir",
    "touch", "chmod", "chown", "chgrp", "find", "grep", "egrep", "sed", "awk", "sort", "uniq",
    "wc", "tar", "zip", "unzip", "gzip", "xz", "systemctl", "service", "journalctl", "docker",
    "podman", "docker-compose", "nginx", "mysql", "mysqldump", "psql", "redis-cli", "mongo",
    "df", "du", "free", "top", "htop", "ps", "kill", "pkill", "killall", "netstat", "ss",
    "ip", "ifconfig", "route", "iptables", "nft", "ufw", "firewall-cmd", "apt", "apt-get",
    "yum", "dnf", "apk", "pacman", "npm", "npx", "node", "python", "python3", "pip", "pip3",
    "cargo", "rustc", "go", "java", "dotnet", "git", "curl", "wget", "ssh", "scp", "rsync",
    "sftp", "ftp", "vi", "vim", "nano", "emacs", "less", "more", "head", "tail", "crontab",
    "useradd", "usermod", "userdel", "groupadd", "passwd", "chpasswd", "sudo", "su", "export",
    "source", "sh", "bash", "zsh", "fish", "powershell", "pwsh", "cmd", "reg", "sc", "taskkill",
    "tasklist", "ipconfig", "netsh", "wmic", "dir", "type", "del", "copy", "move", "ren", "md",
    "rd", "cls", "whoami", "hostname", "uname", "date", "uptime", "reboot", "shutdown", "halt",
    "poweroff", "ln", "stat", "file", "which", "where", "env", "printenv", "history", "alias",
    "unalias", "set", "unset", "mount", "umount", "fdisk", "parted", "lsblk", "blkid",
    "smartctl", "mdadm", "mkfs", "mkswap", "dd", "sync", "sysctl", "modprobe", "insmod",
    "rmmod", "dmesg", "strace", "lsof", "tcpdump", "nmap", "openssl", "certbot", "make",
    "cmake", "gcc", "g++", "clang", "ld", "strip", "ldd", "strings", "objdump", "gdb", "sed",
    "install", "install-app.sh", "install.sh", "pec-ragent", "pek-ragent",
];

/// PowerShell 常用动词（`Get-Process` 之类按「动词-名词」识别）。
const POWERSHELL_VERBS: &[&str] = &[
    "get", "set", "new", "remove", "add", "clear", "copy", "move", "rename", "start", "stop",
    "restart", "invoke", "test", "import", "export", "enable", "disable", "update", "install",
    "uninstall", "register", "unregister", "out", "write", "read", "select", "where",
    "foreach", "format", "convert", "show", "open", "close", "restart", "wait", "push", "pop",
];

/// 判断一行是否像命令（首 token 命中命令表/带路径/PS 动词-名词）。
fn is_command_like(line: &str) -> bool {
    let mut tokens = line.split_whitespace();
    let Some(mut token) = tokens.next() else {
        return false;
    };
    // 跳过前置环境变量赋值：FOO=bar systemctl restart x
    while token.contains('=') && !token.starts_with('-') {
        match tokens.next() {
            Some(t) => token = t,
            None => return false,
        }
    }
    if token.is_empty() || token.starts_with('-') || token.starts_with(':') {
        return false;
    }
    if token.contains('/') || token.contains('\\') {
        return true;
    }
    let lower = token.trim_end_matches(':').to_ascii_lowercase();
    if SHELL_COMMANDS.contains(&lower.as_str()) {
        return true;
    }
    if let Some((verb, rest)) = lower.split_once('-')
        && !rest.is_empty()
        && POWERSHELL_VERBS.contains(&verb)
    {
        return true;
    }
    false
}

/// 危险命令识别（大小写不敏感、空白归一；返回人类可读原因）。
fn danger_reason(line: &str) -> Option<&'static str> {
    let compact = line
        .to_ascii_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");

    // 根目录/核心目录的递归删除（按路径边界判断，避免把 `rm -rf /tmp/x` 当成删根）
    for prefix in ["rm -rf /", "rm -fr /", "rm -r -f /", "chmod -r 777 /"] {
        if contains_root_target(&compact, prefix) {
            return Some("递归删除系统目录/改系统权限");
        }
    }

    const PATTERNS: &[(&str, &str)] = &[
        ("--no-preserve-root", "删除根目录"),
        ("mkfs", "格式化文件系统"),
        ("mkswap", "重建交换分区"),
        ("of=/dev/sd", "裸写磁盘"),
        ("of=/dev/nvme", "裸写磁盘"),
        ("of=/dev/vd", "裸写磁盘"),
        ("of=/dev/mmcblk", "裸写磁盘"),
        ("> /dev/sd", "裸写磁盘"),
        ("dd if=/dev/zero", "裸写磁盘"),
        ("shutdown", "关机/重启"),
        ("reboot", "关机/重启"),
        ("poweroff", "关机/重启"),
        ("halt", "关机/重启"),
        ("init 0", "关机/重启"),
        ("init 6", "关机/重启"),
        ("systemctl stop", "停止系统服务"),
        ("systemctl disable", "禁用系统服务"),
        ("systemctl mask", "屏蔽系统服务"),
        ("kill -9 1", "杀 init/系统进程"),
        ("killall5", "杀全部进程"),
        ("userdel", "删除系统用户"),
        ("chpasswd", "批量改密码"),
        ("visudo", "修改 sudo 权限"),
        ("crontab -r", "清空计划任务"),
        ("iptables -f", "清空防火墙规则"),
        ("iptables -x", "清空防火墙规则"),
        ("nft flush ruleset", "清空防火墙规则"),
        ("ufw disable", "关闭防火墙"),
        ("find / -delete", "批量删除文件"),
        ("find / -exec rm", "批量删除文件"),
        ("truncate -s 0 /", "清空系统文件"),
        ("cat /dev/zero >", "写爆磁盘"),
        (":(){", "fork 炸弹"),
        ("mv /*", "移动系统目录"),
        ("format c:", "格式化系统盘"),
        ("diskpart", "磁盘分区操作"),
        ("bcdedit", "修改启动项"),
        ("reg delete hklm", "删除注册表"),
        ("remove-item -recurse c:\\", "递归删除系统盘"),
        ("rd /s /q c:\\", "递归删除系统盘"),
        ("del /f /s /q c:\\", "递归删除系统盘"),
    ];
    for (pattern, reason) in PATTERNS {
        if compact.contains(pattern) {
            return Some(reason);
        }
    }
    None
}

/// `rm -rf /` 这类前缀是否有「真正的根/核心目录」紧随其后（`/tmp`、`/opt/app` 不算）。
fn contains_root_target(text: &str, prefix: &str) -> bool {
    let mut from = 0;
    while let Some(idx) = text[from..].find(prefix) {
        let start = from + idx;
        let before_ok = start == 0
            || matches!(
                text.as_bytes()[start - 1] as char,
                ' ' | ';' | '&' | '|' | '(' | '\n'
            );
        let rest = &text[start + prefix.len()..];
        let target: String = rest.chars().take_while(|c| !c.is_whitespace()).collect();
        // 取目标路径首段：`/etc`、`/etc/nginx`、`/*` 都算核心目录，`/tmp`、`/opt/app` 不算
        let first = target.split('/').next().unwrap_or("");
        let core = target.is_empty()
            || target == "*"
            || matches!(
                first,
                "etc" | "usr" | "boot" | "bin" | "sbin" | "lib" | "lib64" | "var" | "root"
                    | "home" | "dev" | "proc" | "sys"
            );
        if before_ok && core {
            return true;
        }
        from = start + prefix.len();
    }
    false
}

/// 子用户命令白名单（前缀匹配 + 边界；`;`/换行分隔，大小写不敏感）。
fn whitelist_allows(line: &str, list: &str) -> bool {
    let cmd = line.trim_start().to_ascii_lowercase();
    list.split([';', '\n', '\r'])
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .any(|p| {
            let p = p.to_ascii_lowercase();
            if !cmd.starts_with(&p) {
                return false;
            }
            match cmd.as_bytes().get(p.len()) {
                None => true,
                Some(b) => matches!(*b as char, ' ' | '\t' | ';' | '&' | '|'),
            }
        })
}

/// 进程退出时关闭全部终端会话（run_core 清理阶段调用）。
pub fn close_all() {
    if let Some(t) = ENGINE.get() {
        t.close_all();
    }
}

// ————— 工具 —————

/// 引擎 `ensure` 错误 → HTTP 状态码（消息为稳定前缀契约，见 `dhrust::term`）。
fn ensure_error_status(msg: &str) -> u16 {
    if msg.contains("归属") {
        403
    } else if msg.contains("上限") || msg.contains("无效") {
        400
    } else {
        500
    }
}

/// 面板约定信封（WS 入口的拒绝响应）。
fn json_err(status: u16, message: &str) -> HttpResponse {
    HttpResponse::json(
        status,
        format!(
            "{{\"code\":{status},\"message\":\"{}\"}}",
            json_escape(message)
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ensure_error_status_mapping() {
        assert_eq!(ensure_error_status("会话归属校验失败（请重置会话）"), 403);
        assert_eq!(
            ensure_error_status("终端会话数已达上限（请先重置不用的会话）"),
            400
        );
        assert_eq!(ensure_error_status("创建 PTY 失败：xxx"), 500);
        assert_eq!(ensure_error_status("启动 shell 失败：xxx"), 500);
    }

    #[test]
    fn sid_validation_delegates_to_engine() {
        // 引擎规则在 dhrust 有独立测试；此处确认转发正确
        assert!(valid_sid("abcd1234"));
        assert!(!valid_sid("short"));
    }

    #[test]
    fn danger_patterns_cover_common_outages() {
        for cmd in [
            "rm -rf /",
            "sudo rm -rf /*",
            "rm -rf /etc/nginx",
            "chmod -R 777 /",
            "mkfs.ext4 /dev/sda1",
            "dd if=/dev/zero of=/dev/sda",
            "shutdown -h now",
            "systemctl stop nginx",
            "kill -9 1",
            "iptables -F",
            ":(){ :|:& };:",
        ] {
            assert!(danger_reason(cmd).is_some(), "应识别为危险命令：{cmd}");
        }
        // 正常命令不误伤（尤其是 /tmp、/opt/app 下的递归删除）
        for cmd in [
            "ls -l /",
            "rm -rf /tmp/build",
            "rm -rf ./target",
            "systemctl status nginx",
            "df -h",
        ] {
            assert!(danger_reason(cmd).is_none(), "不应识别为危险命令：{cmd}");
        }
    }

    #[test]
    fn command_like_skips_secret_like_input() {
        assert!(is_command_like("sudo systemctl restart nginx"));
        assert!(is_command_like("./install.sh"));
        assert!(is_command_like("Get-Process"));
        assert!(is_command_like("FOO=bar docker ps"));
        assert!(!is_command_like("mysecret123"));
        assert!(!is_command_like(":wq"));
        assert!(!is_command_like(""));
    }

    #[test]
    fn whitelist_prefix_has_boundary() {
        assert!(whitelist_allows("docker ps -a", "docker ps;systemctl restart"));
        assert!(whitelist_allows("systemctl restart nginx", "docker ps\nsystemctl restart"));
        assert!(!whitelist_allows("docker push img", "docker ps"));
        assert!(!whitelist_allows("rmdir /tmp/x", "rm"));
        assert!(!whitelist_allows("ls", "rm"));
    }

    #[test]
    fn input_flow_blocks_or_confirms_dangerous_commands() {
        let base = std::env::temp_dir().join(format!(
            "pek-ragent-term-{}",
            dhrust::sign::getrand(8)
        ));
        std::fs::create_dir_all(&base).unwrap();
        let cfg = AgentConfig::default();

        // 普通命令：原样转发，回车交给 shell
        match handle_input(&base, "admin", true, "127.0.0.1", "sidnormal", &cfg, "ls -l\r") {
            InputFlow::Forward(out) => assert!(out.ends_with('\r'), "{out:?}"),
            _ => panic!("普通命令不应要求确认"),
        }

        // 管理员危险命令：清行 + 待确认；确认令牌一次性、且只能由同一用户消费
        match handle_input(
            &base,
            "admin",
            true,
            "127.0.0.1",
            "sidadmin",
            &cfg,
            "rm -rf /etc/nginx\r",
        ) {
            InputFlow::AskConfirm {
                token,
                forward,
                command,
                ..
            } => {
                assert!(forward.contains('\x15'), "应清掉已回显的行：{forward:?}");
                assert!(!forward.ends_with('\r'), "不能把回车交给 shell");
                assert_eq!(command, "rm -rf /etc/nginx");
                assert!(take_confirmed("sidadmin", "other", &token).is_none());
                assert_eq!(
                    take_confirmed("sidadmin", "admin", &token).map(|(c, _)| c),
                    Some("rm -rf /etc/nginx".to_string())
                );
                assert!(
                    take_confirmed("sidadmin", "admin", &token).is_none(),
                    "令牌必须一次性"
                );
            }
            _ => panic!("管理员应进入二次确认"),
        }

        // 子用户危险命令：直接阻止（无待确认令牌）
        let flow = handle_input(&base, "alice", false, "127.0.0.1", "sidsub", &cfg, "reboot\r");
        match flow {
            InputFlow::Forward(out) => {
                assert!(out.contains('\x15'), "{out:?}");
                assert!(!out.ends_with('\r'), "被阻止的命令不能执行：{out:?}");
            }
            _ => panic!("子用户危险命令应直接阻止"),
        }

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn policy_branches() {        let cfg = AgentConfig::default();
        assert!(matches!(
            decide_line(&cfg, true, "rm -rf /etc"),
            LineVerdict::Confirm(_)
        ));
        assert!(matches!(
            decide_line(&cfg, false, "rm -rf /etc"),
            LineVerdict::Blocked(_)
        ));
        // allow：只审计不拦截
        let allow = AgentConfig {
            terminal_danger_policy: "allow".to_string(),
            ..AgentConfig::default()
        };
        assert!(matches!(
            decide_line(&allow, false, "reboot"),
            LineVerdict::Run { danger: Some(_) }
        ));
        // block：连管理员也拦
        let block = AgentConfig {
            terminal_danger_policy: "block".to_string(),
            ..AgentConfig::default()
        };
        assert!(matches!(
            decide_line(&block, true, "reboot"),
            LineVerdict::Blocked(_)
        ));
        // 子用户白名单：未命中直接阻止（即使命令本身无害）
        let wl = AgentConfig {
            terminal_allowed_commands: "docker ps".to_string(),
            ..AgentConfig::default()
        };
        assert!(matches!(
            decide_line(&wl, false, "ls -l"),
            LineVerdict::Blocked(_)
        ));
        assert!(matches!(
            decide_line(&wl, false, "docker ps"),
            LineVerdict::Run { danger: None }
        ));
        // 内置管理员不受白名单限制
        assert!(matches!(
            decide_line(&wl, true, "ls -l"),
            LineVerdict::Run { danger: None }
        ));
    }
}
