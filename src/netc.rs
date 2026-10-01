//! TCP 连通检查与 `host:port` 解析（HTTP 调用已改用 [`dhrust::net::http_client`]，含 TLS/重定向）。

use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

/// TCP 连通检查。
pub fn tcp_check(host: &str, port: u16, timeout: Duration) -> Result<(), String> {
    let addr_text = format!("{}:{}", host, port);
    let addr = addr_text
        .to_socket_addrs()
        .map_err(|e| format!("解析地址 {} 失败：{}", addr_text, e))?
        .next()
        .ok_or_else(|| format!("无法解析地址 {}", addr_text))?;

    TcpStream::connect_timeout(&addr, timeout)
        .map(|_| ())
        .map_err(|e| format!("连接 {} 失败：{}", addr_text, e))
}

/// 拆分 `host:port`。
pub fn split_host_port(text: &str, default_port: u16) -> (String, u16) {
    match text.rfind(':') {
        Some(i) => {
            let port = text[i + 1..].parse::<u16>().unwrap_or(default_port);
            (text[..i].to_string(), port)
        }
        None => (text.to_string(), default_port),
    }
}
