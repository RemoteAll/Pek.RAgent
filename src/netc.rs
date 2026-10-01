//! 极简 HTTP 客户端与 TCP 连通检查。
//!
//! 仅服务两个内部场景：命令行/菜单调用本机控制接口（localhost:5500）、应用健康检查。
//! 不引入完整 HTTP 客户端栈（无 TLS/无重定向/无 chunked 解码——本机控制接口返回定长响应）。

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

/// GET 文本。仅支持 `http://` 地址。
pub fn http_get_url(url: &str, timeout: Duration) -> Result<String, String> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| format!("仅支持 http:// 地址：{}", url))?;

    let (host_port, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = split_host_port(host_port, 80);
    http_get(&host, port, path, timeout)
}

/// GET 文本（`host:port` + `path`）。
pub fn http_get(host: &str, port: u16, path: &str, timeout: Duration) -> Result<String, String> {
    let addr_text = format!("{}:{}", host, port);
    let addr = addr_text
        .to_socket_addrs()
        .map_err(|e| format!("解析地址 {} 失败：{}", addr_text, e))?
        .next()
        .ok_or_else(|| format!("无法解析地址 {}", addr_text))?;

    let mut sock =
        TcpStream::connect_timeout(&addr, timeout).map_err(|e| format!("连接 {} 失败：{}", addr_text, e))?;
    sock.set_read_timeout(Some(timeout)).map_err(|e| e.to_string())?;
    sock.set_write_timeout(Some(timeout)).map_err(|e| e.to_string())?;

    let request = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nUser-Agent: Pek.RAgent\r\n\r\n",
        path, addr_text
    );
    sock.write_all(request.as_bytes())
        .map_err(|e| format!("发送请求失败：{}", e))?;

    let mut buf = Vec::new();
    sock.read_to_end(&mut buf)
        .map_err(|e| format!("读取响应失败：{}", e))?;
    let text = String::from_utf8_lossy(&buf).into_owned();

    let (head, body) = match text.find("\r\n\r\n") {
        Some(i) => (&text[..i], &text[i + 4..]),
        None => return Err("响应格式错误".to_string()),
    };

    let status_line = head.lines().next().unwrap_or("");
    if !status_line.contains(" 200") {
        return Err(format!("响应状态异常：{}", status_line.trim()));
    }

    Ok(body.to_string())
}

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
