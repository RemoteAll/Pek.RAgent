//! 节点 HTTP 接口的统一 JSON 信封（`{"ok":…,"message":…,"data":…}`）。
//!
//! 直连部署（[`crate::deploy_http`]）与直连数据库（[`crate::db_direct`]）共用同一口径，
//! 客户端侧只认这一层信封（`dhrust::net::http_client::direct_json` 亦按此解析）。
//!
//! 实现已下沉 `dhrust::net::http::HttpResponse::{json_error, json_ok}`；这里只做
//! 「语义层 → HTTP 结果」的薄壳包装，避免两处模块各写一遍。

use dhrust::net::http::{HttpOutcome, HttpResponse};

/// 失败信封：`{"ok":false,"message":"…"}`，状态码透传。
pub fn error(status: u16, message: &str) -> HttpOutcome {
    HttpOutcome::Response(HttpResponse::json_error(status, message))
}

/// 成功信封：`{"ok":true,"message":"…","data":…}`。
pub fn ok(message: &str, data: &serde_json::Value) -> HttpOutcome {
    HttpOutcome::Response(HttpResponse::json_ok(message, data))
}
