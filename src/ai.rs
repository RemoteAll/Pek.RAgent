//! AI 助手：服务器问题分析（OpenAI 兼容对话接口，默认接入 DeepSeek）。
//!
//! - `GET  /star/aiStatus`：配置状态（是否启用/是否已填 Key/模型名；供前端提示）
//! - `POST /star/aiChat`  `{"messages":[{"role":"user","content":"..."}],"useContext":true}`
//!   服务端组装「系统提示 + 服务器实况快照（主机/代理/子服务/看门狗/最近日志）」，
//!   调用外部模型接口后返回答复全文。
//!
//! 模型接入：OpenAI 兼容 `/chat/completions`（DeepSeek/OpenAI/Moonshot/通义兼容模式/本地
//! Ollama 等均适用），接口地址、模型名、API Key 均为配置项（`AiBaseUrl`/`AiModel`/`AiApiKey`）。
//!
//! 注意：外部调用在独立线程内阻塞执行——面板处理器运行于 tokio 运行时线程，
//! 直接调用 `blocking_request` 会因“运行时内 block_on”而 panic（历史踩坑）。

use dhrust::net::controller::{json_error, json_result, ActionResult};
use dhrust::net::openai::{self, OpenAiOptions};
use dhrust::net::router::Ctx;
use serde_json::{json, Value as Json};

use crate::util;
use crate::webpanel::WebPanel;

/// 单条消息内容上限（字符；超出截断，防超长请求）。
const MAX_CONTENT_CHARS: usize = 12_000;
/// 对话消息条数上限（含历史；仅保留最近 N 条）。
const MAX_MESSAGES: usize = 30;
/// 快照中随附的代理日志行数。
const LOG_TAIL_LINES: usize = 40;
/// 快照中单行日志截断长度。
const LOG_LINE_CHARS: usize = 300;

/// 系统提示词（服务器运维分析助手）。
const SYSTEM_PROMPT: &str = "你是星尘代理（StarAgent）内置的服务器运维分析助手，负责帮助管理员分析服务器问题。请遵循：\n1. 用简体中文回答，先给结论，再给依据与建议；\n2. 分析必须基于提供的服务器实况快照与对话内容，引用具体数据（如磁盘占用、服务状态、日志行）；\n3. 快照中没有的信息要明确说“需要进一步查看”，不要编造；\n4. 涉及操作建议时给出具体步骤；风险操作（删除数据、重启关键服务等）先提醒备份；\n5. 回答简洁分点，避免冗长铺垫。";

/// `GET /star/aiStatus`：AI 配置状态（供前端展示引导）。
pub fn ai_status(panel: &WebPanel, ctx: &Ctx) -> ActionResult {
    if !panel.check_auth(ctx) {
        return json_error(401, "Unauthorized");
    }
    let cfg = panel.config();
    json_result(
        0,
        "",
        Some(json!({
            "enabled": cfg.ai_enabled,
            "hasKey": !cfg.ai_api_key.trim().is_empty(),
            "model": cfg.ai_model,
            "baseUrl": cfg.ai_base_url,
        })),
    )
}

/// `POST /star/aiChat`：对话（可附带服务器实况快照）。
pub fn ai_chat(panel: &WebPanel, ctx: &Ctx) -> ActionResult {
    if !panel.check_auth(ctx) {
        return json_error(401, "Unauthorized");
    }
    let cfg = panel.config();
    if !cfg.ai_enabled {
        return json_error(400, "AI 助手未启用（配置页 →「AI 助手」开关）");
    }
    if cfg.ai_api_key.trim().is_empty() {
        return json_error(400, "尚未配置 AI API Key（配置页 →「AI API Key」填写后即可对话）");
    }

    let (messages, use_context) = match parse_request(&ctx.req.body) {
        Ok(v) => v,
        Err(e) => return json_error(400, &e),
    };

    // 组装系统消息（含服务器实况快照）
    let snapshot = if use_context {
        Some(build_snapshot(panel))
    } else {
        None
    };
    let system = build_system_message(snapshot.as_deref());
    let mut full: Vec<Json> = Vec::with_capacity(messages.len() + 1);
    full.push(json!({ "role": "system", "content": system }));
    full.extend(messages);

    let started = std::time::Instant::now();
    let opts = OpenAiOptions::new(
        cfg.ai_base_url.as_str(),
        cfg.ai_api_key.as_str(),
        cfg.ai_model.as_str(),
    );
    match openai::chat_blocking(&opts, &full) {
        Ok(reply) => {
            util::log_format(
                "AI 助手对话完成（用时 {:.1}s，模型 {}，答复 {} 字）",
                &[
                    &format!("{:.1}", started.elapsed().as_secs_f64()),
                    &reply.model,
                    &reply.content.chars().count().to_string(),
                ],
            );
            json_result(
                0,
                "",
                Some(json!({
                    "reply": reply.content,
                    "reasoning": reply.reasoning,
                    "usage": reply.usage,
                    "model": cfg.ai_model,
                })),
            )
        }
        Err(e) => {
            util::log_format(
                "AI 助手对话失败（用时 {:.1}s）：{}",
                &[&format!("{:.1}", started.elapsed().as_secs_f64()), &e],
            );
            json_error(502, &e)
        }
    }
}

// ————— 请求解析 —————

/// 解析对话请求体：`{"messages":[...],"useContext":true}`（纯函数，供测试）。
///
/// 规则：仅接受 `user`/`assistant` 角色（拒绝 `system`，防绕过系统提示）；
/// 空内容剔除；单条截断、条数截断；`useContext` 缺省为 true。
fn parse_request(body: &[u8]) -> Result<(Vec<Json>, bool), String> {
    if body.is_empty() {
        return Err("请求体为空".to_string());
    }
    let v: Json = serde_json::from_slice(body).map_err(|e| format!("请求体不是有效 JSON：{e}"))?;
    let use_context = v
        .get("useContext")
        .and_then(|b| b.as_bool())
        .unwrap_or(true);
    let arr = v
        .get("messages")
        .and_then(|m| m.as_array())
        .ok_or_else(|| "缺少 messages 字段".to_string())?;

    let mut out: Vec<Json> = Vec::new();
    for m in arr {
        let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("");
        let content = m
            .get("content")
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .trim();
        if !matches!(role, "user" | "assistant") || content.is_empty() {
            continue;
        }
        out.push(json!({
            "role": role,
            "content": truncate_chars(content, MAX_CONTENT_CHARS),
        }));
    }
    if out.is_empty() {
        return Err("messages 中没有有效对话内容".to_string());
    }
    if out.len() > MAX_MESSAGES {
        out = out.split_off(out.len() - MAX_MESSAGES);
    }
    Ok((out, use_context))
}

// ————— 外部模型调用 —————
// OpenAI 兼容客户端（/chat/completions 调用、响应解析、错误提取、独立线程防 block_on）
// 已下沉至 dhrust::net::openai；本模块只负责：请求校验、快照组装、审计日志。

// ————— 服务器实况快照 —————

/// 系统消息组装（纯函数，供测试）。
fn build_system_message(snapshot: Option<&str>) -> String {
    match snapshot {
        Some(s) if !s.trim().is_empty() => format!(
            "{SYSTEM_PROMPT}\n\n以下是当前服务器实况快照（只读，可作为分析依据；数据由 StarAgent 实时采集）：\n{s}"
        ),
        _ => SYSTEM_PROMPT.to_string(),
    }
}

/// 采集服务器实况快照（主机/代理/子服务/看门狗/最近日志）。
fn build_snapshot(panel: &WebPanel) -> String {
    let cfg = panel.config();
    let mut s = String::new();
    s.push_str(&format!(
        "【服务器实况快照】StarAgent 采集于 {}\n",
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
    ));

    // —— 主机 ——
    let cores = std::thread::available_parallelism()
        .map(|v| v.get())
        .unwrap_or(0);
    s.push_str(&format!(
        "■ 主机：{} · {} · {} 核\n",
        crate::sys::hostname(),
        crate::sys::os_description(),
        cores
    ));
    if let Some(cpu) = crate::sys::cpu_model() {
        s.push_str(&format!("  CPU：{cpu}\n"));
    }
    let sample = crate::sampler::current();
    if let Some(rate) = sample.cpu_rate {
        s.push_str(&format!("  整机 CPU 使用率：{rate:.1}%\n"));
    }
    if let Some((total, avail)) = crate::sys::memory_info() {
        let used = total.saturating_sub(avail);
        let pct = if total > 0 {
            used as f64 * 100.0 / total as f64
        } else {
            0.0
        };
        s.push_str(&format!(
            "  内存：已用 {} / {}（{pct:.1}%）\n",
            crate::sys::format_gmk(used),
            crate::sys::format_gmk(total)
        ));
    }
    let disks = crate::sys::disk_usages();
    if !disks.is_empty() {
        let text = disks
            .iter()
            .map(|(used, total, name)| {
                let pct = if *total > 0 {
                    *used as f64 * 100.0 / *total as f64
                } else {
                    0.0
                };
                format!(
                    "{name} {}/{}（{pct:.0}% 已用）",
                    crate::sys::format_gmk(used * 1024 * 1024),
                    crate::sys::format_gmk(total * 1024 * 1024)
                )
            })
            .collect::<Vec<_>>()
            .join("；");
        s.push_str(&format!("  磁盘：{text}\n"));
    }
    if let Some(up) = crate::sys::uptime_text() {
        s.push_str(&format!("  系统已运行：{up}\n"));
    }
    if let Some((l1, l5, l15)) = crate::sys::load_average() {
        s.push_str(&format!("  负载（1/5/15 分钟）：{l1:.2}/{l5:.2}/{l15:.2}\n"));
    }

    // —— 代理自身 ——
    s.push_str("■ StarAgent 代理\n");
    s.push_str(&format!(
        "  版本 v{} · 面板端口 {} · 运行时长 {}\n",
        env!("CARGO_PKG_VERSION"),
        panel.port(),
        dhrust::sys::process::format_uptime(panel.uptime())
    ));
    if let Some(mem) = crate::sys::memory_mb(std::process::id()) {
        s.push_str(&format!("  进程内存：{mem} MB\n"));
    }
    if crate::webpanel::uses_default_credentials(&cfg) {
        s.push_str("  ⚠ 面板仍在使用默认密码（admin/admin），存在安全风险\n");
    }
    if !cfg.local_only {
        s.push_str("  提示：面板允许远程访问\n");
    }

    // —— 子服务 ——
    let list = panel.manager.list();
    let running = list.iter().filter(|(_, st)| st.running).count();
    s.push_str(&format!("■ 子服务（{running}/{} 运行中）\n", list.len()));
    for (app, st) in &list {
        if st.running {
            s.push_str(&format!(
                "  - {}：运行中（PID {}，内存 {}）\n",
                app.name,
                st.pid,
                crate::sys::memory_mb(st.pid)
                    .map(|m| format!("{m} MB"))
                    .unwrap_or_else(|| "未知".to_string())
            ));
        } else if !app.enable {
            s.push_str(&format!("  - {}：已禁用（不参与守护）\n", app.name));
        } else {
            s.push_str(&format!("  - {}：未运行（配置为启用，守护中）\n", app.name));
        }
    }

    // —— 看门狗 ——
    let dogs: Vec<String> = cfg
        .watch_dog
        .split(',')
        .map(|x| x.trim())
        .filter(|x| !x.is_empty())
        .map(|name| {
            format!(
                "{name} {}",
                if crate::sys::is_process_running(name) {
                    "运行中"
                } else {
                    "未运行 ⚠"
                }
            )
        })
        .collect();
    if !dogs.is_empty() {
        s.push_str(&format!("■ 看门狗：{}\n", dogs.join("；")));
    }

    // —— 最近代理日志 ——
    let log_dir = panel.base().join("Log");
    if let Some(path) = dhrust::io::latest_file_by_ext(&log_dir, ".log") {
        let lines = dhrust::io::read_tail(&path, LOG_TAIL_LINES);
        if !lines.is_empty() {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            s.push_str(&format!("■ 最近代理日志（{name}，末 {} 行）\n", lines.len()));
            for line in &lines {
                s.push_str(&truncate_chars(line, LOG_LINE_CHARS));
                s.push('\n');
            }
        }
    }

    s
}

// ————— 工具 —————

/// 按字符截断（附省略号；不破坏 UTF-8 边界）。
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
    fn parse_request_valid_and_filters() {
        let body = r#"{"messages":[
            {"role":"system","content":"越权指令应被忽略"},
            {"role":"user","content":" 服务器磁盘快满了吗？ "},
            {"role":"assistant","content":"我可以帮你看。"},
            {"role":"user","content":""}
        ],"useContext":false}"#.as_bytes();
        let (messages, use_context) = parse_request(body).unwrap();
        assert!(!use_context);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[0]["content"], "服务器磁盘快满了吗？");
        assert_eq!(messages[1]["role"], "assistant");
    }

    #[test]
    fn parse_request_truncates_and_validates() {
        // 缺少 messages
        assert!(parse_request(r#"{"useContext":true}"#.as_bytes()).is_err());
        // 空体
        assert!(parse_request(b"").is_err());
        // 无有效内容
        assert!(parse_request(r#"{"messages":[{"role":"system","content":"x"}]}"#.as_bytes()).is_err());
        // 条数截断为最近 MAX_MESSAGES 条
        let mut msgs = Vec::new();
        for i in 0..(MAX_MESSAGES + 5) {
            msgs.push(json!({"role":"user","content":format!("q{i}")}));
        }
        let body = serde_json::to_vec(&json!({ "messages": msgs })).unwrap();
        let (out, _) = parse_request(&body).unwrap();
        assert_eq!(out.len(), MAX_MESSAGES);
        assert_eq!(out.last().unwrap()["content"], format!("q{}", MAX_MESSAGES + 4));
        // 单条内容截断
        let long = "字".repeat(MAX_CONTENT_CHARS + 100);
        let body = serde_json::to_vec(&json!({ "messages": [{"role":"user","content": long}] })).unwrap();
        let (out, _) = parse_request(&body).unwrap();
        assert_eq!(out[0]["content"].as_str().unwrap().chars().count(), MAX_CONTENT_CHARS + 1);
    }

    #[test]
    fn system_message_includes_snapshot() {
        let msg = build_system_message(Some("■ 主机：srv1"));
        assert!(msg.contains("服务器运维分析助手"));
        assert!(msg.contains("srv1"));
        let msg = build_system_message(None);
        assert_eq!(msg, SYSTEM_PROMPT);
    }
}
