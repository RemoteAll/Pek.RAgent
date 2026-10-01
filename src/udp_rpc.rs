//! 本地 UDP RPC 服务端（NewLife ApiClient 二进制协议；与 C# StarAgent 的 UDP 5500 同契约）。
//!
//! 背景：新版 DHDeploy 走 `udp://127.0.0.1:5500`（`ApiClient.Invoke`），C# StarAgent 的 UDP RPC
//! 由 `StarService` 提供；本模块补齐该服务端，协议实现复用 [`dhrust::net::api_rpc`]
//! （客户端/服务端同源，杜绝双端分叉）。
//!
//! 动作面（对齐 C# `StarService`）：
//! - `StartService` / `StopService` / `RestartService`：完整实现（复用 HTTP 控制接口的核心逻辑，
//!   消息语义与 `ServiceOperationResult` 形态对齐 C#）；
//! - `Ping` / `Info` / `GetServices` / `SetServer`：简化实现（C# 对应返回为复杂对象，当前无消费者；
//!   `SetServer` 写入配置 `Server` 字段）——待 StarServer 对接时补齐数据形态。
//!
//! 仅限本机访问（对齐 C# `CheckLocal`；`dhrust::net::api_rpc::serve_udp` 的 `local_only`）。

use std::sync::Arc;

use dhrust::net::api_rpc::{self, ApiReply};

use crate::manager::AppManager;
use crate::server::OpKind;

/// 启动 UDP RPC 服务端线程（`127.0.0.1:{port}`；与 Web 面板端口共用，TCP/UDP 分属不同协议）。
pub fn start(manager: Arc<AppManager>, port: u16) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let addr = format!("127.0.0.1:{port}");
        if let Err(e) = api_rpc::serve_udp(&addr, true, move |action, args| {
            handle(&manager, action, args)
        }) {
            crate::util::log_error(&format!("UDP RPC 服务启动失败（{addr}）：{e}"));
        }
    })
}

/// 动作分发（对齐 C# `StarService`；未知动作回业务失败）。
fn handle(manager: &Arc<AppManager>, action: &str, args_json: Option<&str>) -> ApiReply {
    let name = arg_str(args_json, "serviceName");

    match action {
        "Ping" => reply(true, "Pong", ""),
        "Info" => reply(true, "Pek.RAgent", ""),
        "GetServices" => {
            let names: Vec<String> = manager
                .config()
                .apps
                .iter()
                .map(|a| a.name.clone())
                .collect();
            reply(true, &names.join(","), "")
        }
        "SetServer" => {
            let server = arg_str(args_json, "server");
            manager.update_config(|cfg| cfg.server = server.clone());
            reply(true, "OK", "")
        }
        "StartService" | "StopService" | "RestartService" => {
            if name.is_empty() {
                return reply(false, "服务名称不能为空", "");
            }
            let kind = match action {
                "StartService" => OpKind::Start,
                "StopService" => OpKind::Stop,
                _ => OpKind::Restart,
            };
            let (ok, message) = crate::server::apply_operation(manager, &name, kind);
            reply(ok, &message, &name)
        }
        _ => reply(false, &format!("不支持的动作：{action}"), &name),
    }
}

/// 从 args JSON 取字符串字段（容错；解析失败视为缺省）。
fn arg_str(args_json: Option<&str>, key: &str) -> String {
    args_json
        .and_then(|a| serde_json::from_str::<serde_json::Value>(a).ok())
        .and_then(|v| v.get(key).and_then(|x| x.as_str()).map(|s| s.to_string()))
        .unwrap_or_default()
}

/// 组装业务回复。
fn reply(success: bool, message: &str, service_name: &str) -> ApiReply {
    ApiReply {
        success,
        message: message.to_string(),
        service_name: service_name.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_manager() -> (Arc<AppManager>, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "ragent-udp-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let manager = AppManager::new(&dir, crate::config::AgentConfig::default());
        (manager, dir)
    }

    #[test]
    fn ping_and_service_operations() {
        let (manager, dir) = test_manager();

        let r = handle(&manager, "Ping", None);
        assert!(r.success);
        assert_eq!(r.message, "Pong");

        // 服务不存在（对齐 C# 文案）
        let r = handle(&manager, "RestartService", Some(r#"{"serviceName":"nope"}"#));
        assert!(!r.success);
        assert_eq!(r.message, "服务不存在");

        // 空服务名（对齐 C# 文案）
        let r = handle(&manager, "StartService", Some(r#"{"serviceName":""}"#));
        assert!(!r.success);
        assert_eq!(r.message, "服务名称不能为空");

        // 未知动作
        let r = handle(&manager, "NoSuch", None);
        assert!(!r.success);
        assert!(r.message.contains("不支持的动作"));

        // SetServer 写入配置
        let r = handle(
            &manager,
            "SetServer",
            Some(r#"{"server":"udp://10.0.0.1:5500"}"#),
        );
        assert!(r.success);
        assert_eq!(manager.config().server, "udp://10.0.0.1:5500");

        // GetServices 返回应用名清单
        let r = handle(&manager, "GetServices", None);
        assert!(r.success);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
