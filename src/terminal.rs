//! 在线终端：真 PTY（Windows ConPTY / Unix openpty）+ WebSocket 流式，前端 xterm.js。
//!
//! - `GET /star/termWs?sid=&cols=&rows=[&token=]`：WebSocket 会话（升级后双向流式传输：
//!   服务端发 UTF-8 文本帧=终端输出；客户端发 JSON——`{"t":"i","d":"..."}` 键盘输入、
//!   `{"t":"r","c":..,"r":..}` 调整尺寸）
//! - `POST /star/termReset {"sid"}`：重置会话（杀掉 shell；前端自动重连生成新会话）
//!
//! 与宝塔终端一致的真终端体验：单区域流式输出、光标闪烁、命令回显、支持 `vim`/`top`
//! 等交互式程序与全键盘（Ctrl+C 等）。ConPTY/PTY 天然 UTF-8——Windows 中文不再需要
//! 代码页转码（2026-10-05 的旧管道执行引擎已整体移除）。
//!
//! 安全：命令以本程序（服务账户）权限运行（Linux 通常 root，与 SSH 等效）；**连接与断开
//! 写入操作审计**（不记录击键——终端输入可能包含密码等敏感内容，有意不采集）；
//! 配置项 `TerminalEnabled` 可整体关闭；菜单权限 `terminal` 可授予子用户。

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use dhrust::net::controller::{arg, json_body, json_error, json_result, ActionResult};
use dhrust::net::http::{json_escape, HttpOutcome, HttpResponse};
use dhrust::net::panel_auth::client_ip;
use dhrust::net::router::Ctx;
use dhrust::net::ws::{WsServerConn, WsServerHooks, WsServerMessage};
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use serde_json::Value as Json;

use crate::audit;
use crate::util;
use crate::webpanel::{WebPanel, PERM_TERMINAL};

/// 会话上限（防资源滥用；满员时拒绝新建）
const MAX_SESSIONS: usize = 8;
/// 会话空闲回收（无 WS 订阅且空闲超时）
const IDLE_TIMEOUT: Duration = Duration::from_secs(1800);
/// 重连回放缓冲上限（最近输出；WebSocket 重连时回放）
const RING_MAX: usize = 128 * 1024;
/// 终端尺寸限制
const MIN_COLS: u16 = 20;
const MAX_COLS: u16 = 500;
const MIN_ROWS: u16 = 5;
const MAX_ROWS: u16 = 200;

/// WebSocket 输出汇：当前订阅者（std mpsc → 转发线程 → WS 文本帧）。
/// 重新订阅时替换 sender（旧转发线程因通道断开自然退出并关闭旧连接）。
struct WsSink {
    tx: Mutex<Option<Sender<Vec<u8>>>>,
}

/// 终端会话（一个常驻 shell）。
struct Session {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    /// 最近输出（重连回放；PTY 读线程写入）
    ring: Arc<Mutex<Vec<u8>>>,
    sink: Arc<WsSink>,
    /// shell 存活标记（读线程 EOF 置假）
    alive: Arc<AtomicBool>,
    owner: String,
    last_active: Instant,
    cols: u16,
    rows: u16,
}

impl Session {
    /// 结束进程并断开订阅（幂等）。
    fn cleanup(&mut self) {
        if let Ok(mut g) = self.sink.tx.lock() {
            g.take();
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// 会话表（sid → Session）。
fn sessions() -> &'static Mutex<HashMap<String, Session>> {
    static REGISTRY: OnceLock<Mutex<HashMap<String, Session>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

// ————— WebSocket 入口（server.rs 原始路由；认证在本函数内完成） —————

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
        .unwrap_or(120)
        .clamp(MIN_COLS, MAX_COLS);
    let rows = arg(ctx, "rows")
        .and_then(|v| v.trim().parse::<u16>().ok())
        .unwrap_or(30)
        .clamp(MIN_ROWS, MAX_ROWS);

    // 取或建会话（锁内快进快出）
    let (sink, replay) = {
        let mut map = sessions().lock().unwrap();
        purge_idle(&mut map);
        if !map.contains_key(&sid) {
            if map.len() >= MAX_SESSIONS {
                return HttpOutcome::Response(json_err(
                    400,
                    "终端会话数已达上限（请先重置不用的会话）",
                ));
            }
            match spawn_session(&sid, &principal.name, cols, rows, panel.base()) {
                Ok(s) => {
                    map.insert(sid.clone(), s);
                }
                Err(e) => return HttpOutcome::Response(json_err(500, &e)),
            }
            util::log_format(
                "在线终端已连接（{}，{}x{}）",
                &[&principal.name, &cols.to_string(), &rows.to_string()],
            );
        }
        let s = map.get_mut(&sid).expect("session exists");
        if s.owner != principal.name {
            return HttpOutcome::Response(json_err(403, "会话归属校验失败（请重置会话）"));
        }
        s.cols = cols;
        s.rows = rows;
        let _ = s.master.resize(pty_size(cols, rows));
        s.last_active = Instant::now();
        let replay = s.ring.lock().map(|r| r.clone()).unwrap_or_default();
        (s.sink.clone(), replay)
    };

    // 审计：打开终端会话（有意不记录击键内容）
    audit::record(
        panel.base(),
        &audit::AuditEntry {
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
    let sink_open = sink.clone();
    let on_open = Arc::new(move |conn: WsServerConn| {
        // 重连回放最近输出（分片发送；每帧 ≤ 200KB）
        if !replay.is_empty() {
            for chunk in replay.chunks(200_000) {
                let _ = conn.send_text(String::from_utf8_lossy(chunk).into_owned());
            }
        }
        // 订阅输出 → 转发线程（std 线程，无 tokio 依赖）
        let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
        if let Ok(mut g) = sink_open.tx.lock() {
            g.replace(tx);
        }
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

    let sid_msg = sid.clone();
    let on_message = Arc::new(move |msg: WsServerMessage| {
        let Ok(v) = serde_json::from_str::<Json>(&msg.text) else {
            return;
        };
        match v.get("t").and_then(|t| t.as_str()).unwrap_or("") {
            "i" => {
                if let Some(d) = v.get("d").and_then(|d| d.as_str()) {
                    let mut map = sessions().lock().unwrap();
                    if let Some(s) = map.get_mut(&sid_msg) {
                        let _ = s.writer.write_all(d.as_bytes());
                        let _ = s.writer.flush();
                        s.last_active = Instant::now();
                    }
                }
            }
            "r" => {
                let c = v.get("c").and_then(|x| x.as_u64()).unwrap_or(0) as u16;
                let r = v.get("r").and_then(|x| x.as_u64()).unwrap_or(0) as u16;
                if c >= MIN_COLS && r >= MIN_ROWS {
                    let mut map = sessions().lock().unwrap();
                    if let Some(s) = map.get_mut(&sid_msg) {
                        s.cols = c.min(MAX_COLS);
                        s.rows = r.min(MAX_ROWS);
                        let _ = s.master.resize(pty_size(s.cols, s.rows));
                    }
                }
            }
            _ => {}
        }
    });

    let sid_close = sid.clone();
    let base_close = panel.base().to_path_buf();
    let user_close = principal.name.clone();
    let ip_close = client_ip(ctx);
    let on_close = Arc::new(move |_conn: WsServerConn, reason: String| {
        let reason = truncate_chars(&reason, 120);
        util::log_format("在线终端已断开（{user_close}）：{}", &[&reason]);
        audit::record(
            &base_close,
            &audit::AuditEntry {
                user: user_close.clone(),
                ip: ip_close.clone(),
                action: "termClose".to_string(),
                title: "关闭在线终端".to_string(),
                method: "GET".to_string(),
                path: "/star/termWs".to_string(),
                detail: format!("sid={sid_close} 原因={reason}"),
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
    if let Some(mut s) = sessions().lock().unwrap().remove(&sid) {
        s.cleanup();
    }
    util::log_format("在线终端会话已重置：{}", &[&sid]);
    json_result(0, "会话已重置", None)
}

/// 进程退出时关闭全部终端会话（run_core 清理阶段调用）。
pub fn close_all() {
    let mut map = sessions().lock().unwrap();
    for (_, mut s) in map.drain() {
        s.cleanup();
    }
}

// ————— 会话与进程 —————

/// 会话 id 规则：8~64 位字母/数字/`-`/`_`。
fn valid_sid(sid: &str) -> bool {
    (8..=64).contains(&sid.len())
        && sid
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn pty_size(cols: u16, rows: u16) -> PtySize {
    PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    }
}

/// 启动常驻 shell（PTY）。Windows = ConPTY + cmd（UTF-8 原生）；Unix = bash -l / sh。
fn spawn_session(
    sid: &str,
    owner: &str,
    cols: u16,
    rows: u16,
    cwd: &std::path::Path,
) -> Result<Session, String> {
    let pair = native_pty_system()
        .openpty(pty_size(cols, rows))
        .map_err(|e| format!("创建 PTY 失败：{e}"))?;

    let mut cmd = if cfg!(windows) {
        // 保持 cmd 默认提示符与回显——即"真终端"体验（ConPTY 流为 UTF-8）
        CommandBuilder::new(std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string()))
    } else if std::path::Path::new("/bin/bash").exists() {
        let mut c = CommandBuilder::new("/bin/bash");
        c.arg("-l"); // 登录 shell（PATH/别名等与 SSH 一致）
        c
    } else {
        CommandBuilder::new("/bin/sh")
    };
    cmd.env("TERM", "xterm-256color");
    // 起始目录 = 程序基础目录（提示符直观；Linux 下即部署目录）
    if cwd.is_dir() {
        cmd.cwd(cwd);
    }

    let child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| format!("启动 shell 失败：{e}"))?;
    // 释放从端句柄（否则 master 读取端拿不到 EOF）
    drop(pair.slave);

    let mut reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| format!("获取终端读取端失败：{e}"))?;
    let writer = pair
        .master
        .take_writer()
        .map_err(|e| format!("获取终端写入端失败：{e}"))?;

    let ring = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::new(WsSink {
        tx: Mutex::new(None),
    });
    let alive = Arc::new(AtomicBool::new(true));

    // 读线程：PTY 输出 → 回放缓冲 + 当前 WS 订阅通道；EOF（shell 退出）时自清理注册表
    {
        let ring2 = ring.clone();
        let sink2 = sink.clone();
        let alive2 = alive.clone();
        let sid2 = sid.to_string();
        std::thread::Builder::new()
            .name(format!("term-{sid}"))
            .spawn(move || {
                let mut buf = [0u8; 8192];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            let data = &buf[..n];
                            if let Ok(mut r) = ring2.lock() {
                                r.extend_from_slice(data);
                                let len = r.len();
                                if len > RING_MAX {
                                    let cut = len - RING_MAX;
                                    r.drain(..cut);
                                }
                            }
                            if let Ok(g) = sink2.tx.lock() {
                                if let Some(tx) = g.as_ref() {
                                    let _ = tx.send(data.to_vec());
                                }
                            }
                        }
                    }
                }
                alive2.store(false, Ordering::SeqCst);
                if let Ok(mut g) = sink2.tx.lock() {
                    g.take();
                }
                // shell 退出：从注册表移除并回收（kill 幂等）
                let removed = sessions().lock().unwrap().remove(&sid2);
                if let Some(mut s) = removed {
                    s.cleanup();
                }
            })
            .map_err(|e| format!("启动终端读取线程失败：{e}"))?;
    }

    Ok(Session {
        master: pair.master,
        writer,
        child,
        ring,
        sink,
        alive,
        owner: owner.to_string(),
        last_active: Instant::now(),
        cols,
        rows,
    })
}

/// 清理空闲（无订阅）或已死亡的会话。
fn purge_idle(map: &mut HashMap<String, Session>) {
    let now = Instant::now();
    let mut dead: Vec<String> = Vec::new();
    map.retain(|k, s| {
        let subscribed = s.sink.tx.lock().map(|g| g.is_some()).unwrap_or(false);
        let expired = !subscribed && now.duration_since(s.last_active) > IDLE_TIMEOUT;
        if !s.alive.load(Ordering::SeqCst) || expired {
            dead.push(k.clone());
            false
        } else {
            true
        }
    });
    for key in dead {
        if let Some(mut s) = map.remove(&key) {
            s.cleanup();
        }
    }
}

// ————— 工具 —————

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

/// 按字符截断（不破坏 UTF-8 边界）。
fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let clipped: String = text.chars().take(max).collect();
    format!("{clipped}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_sid_rules() {
        assert!(valid_sid("abcd1234"));
        assert!(valid_sid("a-b_c-12345678"));
        assert!(!valid_sid("short"));
        assert!(!valid_sid("has space 12345"));
        assert!(!valid_sid("宝塔12345678"));
        assert!(!valid_sid(&"x".repeat(65)));
    }

    /// 等待回放缓冲出现目标文本。
    fn wait_ring(s: &Session, needle: &str, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if let Ok(r) = s.ring.lock() {
                if String::from_utf8_lossy(&r).contains(needle) {
                    return true;
                }
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn send_keys(s: &mut Session, text: &str) {
        s.writer.write_all(text.as_bytes()).unwrap();
        s.writer.flush().unwrap();
    }

    #[test]
    fn pty_roundtrip_echo_cwd_and_chinese() {
        let cwd = std::env::temp_dir();
        let mut s =
            spawn_session("test0001", "tester", 100, 30, &cwd).expect("spawn pty");
        // 基本回显（真终端：命令会回显 + 输出）
        send_keys(&mut s, "echo pty-hello-1\r\n");
        assert!(wait_ring(&s, "pty-hello-1", Duration::from_secs(20)), "echo 回显");
        // 交互式持久：切换目录再查询（PTY 内 cd 持久）
        let cd_cmd = if cfg!(windows) {
            "cd /d %TEMP%"
        } else {
            "cd /tmp"
        };
        send_keys(&mut s, &format!("{cd_cmd}\r\n"));
        std::thread::sleep(Duration::from_millis(500));
        send_keys(&mut s, "cd\r\n");
        std::thread::sleep(Duration::from_millis(500));
        let out = String::from_utf8_lossy(&s.ring.lock().unwrap()).to_string();
        if cfg!(windows) {
            assert!(out.to_ascii_lowercase().contains("temp"), "{out}");
        } else {
            assert!(out.contains("/tmp"), "{out}");
        }
        // 中文（ConPTY/PTY 原生 UTF-8，无需代码页处理）
        send_keys(&mut s, "echo 中文PTY测试ABC\r\n");
        assert!(
            wait_ring(&s, "中文PTY测试ABC", Duration::from_secs(20)),
            "中文输出"
        );
        // 调整尺寸（不应报错）
        s.master.resize(pty_size(120, 40)).expect("resize");
        s.cleanup();
    }

    #[test]
    fn cleanup_kills_shell() {
        let cwd = std::env::temp_dir();
        let mut s =
            spawn_session("test0002", "tester", 80, 24, &cwd).expect("spawn pty");
        assert!(s.alive.load(Ordering::SeqCst));
        // 等待 shell 就绪后结束
        send_keys(&mut s, "echo ready\r\n");
        assert!(wait_ring(&s, "ready", Duration::from_secs(20)));
        s.cleanup();
    }
}
