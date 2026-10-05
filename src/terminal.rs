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
//! 安全：命令以本程序（服务账户）权限运行（Linux 通常 root，与 SSH 等效）；**连接与断开
//! 写入操作审计**（不记录击键——终端输入可能包含密码等敏感内容，有意不采集）；
//! 配置项 `TerminalEnabled` 可整体关闭；菜单权限 `terminal` 可授予子用户。
//!
//! 本文件仅保留面板相关职责；会话/进程/回放/订阅等通用能力见 `dhrust::term::Terminal`。

use std::sync::{Arc, OnceLock};

use dhrust::net::controller::{arg, json_body, json_error, json_result, ActionResult};
use dhrust::net::http::{json_escape, HttpOutcome, HttpResponse};
use dhrust::net::panel_auth::client_ip;
use dhrust::net::router::Ctx;
use dhrust::net::ws::{WsServerConn, WsServerHooks, WsServerMessage};
use dhrust::term::{valid_sid, TermOptions, Terminal};
use serde_json::Value as Json;

use crate::audit;
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

    let term_msg = term.clone();
    let sid_msg = sid.clone();
    let on_message = Arc::new(move |msg: WsServerMessage| {
        let Ok(v) = serde_json::from_str::<Json>(&msg.text) else {
            return;
        };
        match v.get("t").and_then(|t| t.as_str()).unwrap_or("") {
            "i" => {
                if let Some(d) = v.get("d").and_then(|d| d.as_str()) {
                    term_msg.write(&sid_msg, d.as_bytes());
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
        let reason = truncate_chars(&reason, 120);
        util::log_format(
            "在线终端已断开（{}）：{}",
            &[&user_close, &reason],
        );
        audit::record(
            &base_close,
            &audit::AuditEntry {
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
}
