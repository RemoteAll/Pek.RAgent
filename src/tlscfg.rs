//! 面板 HTTPS 证书（`Config/keys/panel.pem` + `panel.key`）。
//!
//! 设计取舍：
//! - **自签叶子证书 `CA:FALSE`**：私钥泄露也只能冒充本机 IP，不能冒充其他站点
//!   （远优于「每台机器各建 CA 再让运维全部信任」）；
//! - SAN 由 dhrust 自动写入 `localhost` / 回环 + **本机全部网卡 IPv4**，
//!   因此浏览器可直接用 `https://内网IP:端口` 打开（只需导入一次证书信任）；
//! - 有效期 10 年、**不做自动续签**（面板显示剩余天数 + 一键重签），
//!   避免运行中重建 TLS 监听带来的故障面；
//! - 私钥 Unix 权限 `0600`，不进配置项、不落日志。

use std::path::{Path, PathBuf};

/// 证书与私钥路径（`{base}/Config/keys/panel.{pem,key}`）。
pub fn cert_paths(base: &Path) -> (PathBuf, PathBuf) {
    let dir = base.join("Config").join("keys");
    (dir.join("panel.pem"), dir.join("panel.key"))
}

/// 确保证书存在（缺失自动生成）并返回 PEM 字节；供启动时 `bind_tls` 使用。
pub fn ensure_pem(base: &Path) -> Result<(Vec<u8>, Vec<u8>), String> {
    let (cert, key) = cert_paths(base);
    match dhrust::net::tls::ensure_self_signed_cert(&cert, &key, &[]) {
        Ok(generated) => {
            if generated {
                // 提示运维“下一步要导入信任”，避免直接打开看到红屏后以为是程序故障
                crate::util::log_format(
                    "已生成面板自签证书：{}（浏览器首次访问 https://… 前请导入该证书信任）",
                    &[&cert.display().to_string()],
                );
            }
        }
        Err(e) => return Err(format!("生成面板证书失败：{e}")),
    }
    dhrust::net::tls::load_pem(&cert, &key).map_err(|e| format!("加载面板证书失败：{e}"))
}

/// 重新签发（删除旧证书后重建）；调用方需提示「重启服务后生效」。
pub fn renew(base: &Path) -> Result<(), String> {
    let (cert, key) = cert_paths(base);
    dhrust::net::tls::reset_cert(&cert, &key).map_err(|e| format!("删除旧证书失败：{e}"))?;
    ensure_pem(base).map(|_| ())
}

/// 证书状态（面板展示用 JSON）。
///
/// 字段：`exists` / `path` / `notAfter` / `daysLeft` / `sans` / `sha256`；
/// 证书缺失或损坏时 `exists=false` 且其余为空，不报错（面板照常可用）。
pub fn status(base: &Path, enabled: bool, port: u16) -> serde_json::Value {
    let (cert_path, _) = cert_paths(base);
    let mut out = serde_json::json!({
        "enabled": enabled,
        "exists": false,
        "path": cert_path.display().to_string(),
        "notAfter": "",
        "daysLeft": 0,
        "sans": Vec::<String>::new(),
        "sha256": "",
        // 建议访问地址（启用以 https，否则 http）
        "panelUrl": format!(
            "{}://127.0.0.1:{}",
            if enabled { "https" } else { "http" },
            port
        ),
        "expiringSoon": false,
    });

    let Ok(text) = std::fs::read_to_string(&cert_path) else {
        return out;
    };
    let Some(info) = dhrust::cert::parse_certificate_pem(&text) else {
        return out;
    };

    let mut sans = info.dns_names.clone();
    sans.extend(info.ip_addresses.iter().cloned());
    let now = chrono::Utc::now().timestamp();
    let days = info.days_left(now);

    out["exists"] = serde_json::json!(true);
    out["notAfter"] = serde_json::json!(dhrust::cert::format_unix_utc(info.not_after));
    out["daysLeft"] = serde_json::json!(days);
    out["sans"] = serde_json::json!(sans);
    out["sha256"] = serde_json::json!(info.sha256_hex.to_uppercase());
    // 到期前 30 天提示重签（与“无自动续签”的口径配套）
    out["expiringSoon"] = serde_json::json!(days <= 30);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_under_config_keys() {
        let base = Path::new("/tmp/agent");
        let (cert, key) = cert_paths(base);
        assert!(cert.ends_with("Config/keys/panel.pem"));
        assert!(key.ends_with("Config/keys/panel.key"));
    }

    #[test]
    fn status_without_cert_is_graceful() {
        let base = std::env::temp_dir().join(format!("ragent-tls-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();

        let v = status(&base, false, 5501);
        assert_eq!(v["exists"], false);
        assert_eq!(v["panelUrl"], "http://127.0.0.1:5501");

        // 生成后：解析出 SAN、剩余天数接近 10 年、指纹非空
        ensure_pem(&base).unwrap();
        let v = status(&base, true, 5501);
        assert_eq!(v["exists"], true);
        assert_eq!(v["panelUrl"], "https://127.0.0.1:5501");
        assert!(v["daysLeft"].as_i64().unwrap() > 3600);
        assert!(!v["sha256"].as_str().unwrap().is_empty());
        let sans = v["sans"].as_array().unwrap();
        assert!(sans.iter().any(|s| s == "127.0.0.1"));

        // 重签：删除后重建仍可用
        renew(&base).unwrap();
        assert_eq!(status(&base, true, 5501)["exists"], true);

        let _ = std::fs::remove_dir_all(&base);
    }
}
