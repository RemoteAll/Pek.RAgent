//! 面板 HTTPS 证书 —— **实现已下沉到 `dhrust::net::panel_cert`**（与 HlktechIoT 等面板共用一份）。
//!
//! 本文件只保留原有调用签名（`ensure_pem` / `renew` / `status` / `cert_paths`），
//! 内部全部转调公共库，**不再重复实现**证书路径约定、状态 JSON 与重签逻辑。
//!
//! 口径（由公共库统一保证，不要在此处另行实现）：
//! - 证书落 `Config/keys/panel.pem` + `panel.key`；
//! - **自签叶子证书 `CA:FALSE`**，私钥泄露也只能冒充本机 IP；
//! - SAN 自动含 `localhost` / 回环 + **本机全部网卡 IPv4**，有效期 ≈10 年；
//! - **不做自动续签**（面板显示剩余天数 + 一键重签）；
//! - 私钥不进配置项、不落日志。

use std::path::Path;

pub use dhrust::net::panel_cert::cert_paths;

/// 确保证书存在（缺失自动生成）并返回 PEM 字节；供启动时 `bind_tls` 使用。
///
/// # 参数
/// - `base`：部署根目录。
///
/// # 返回值
/// `(cert_pem, key_pem)`。
pub fn ensure_pem(base: &Path) -> Result<(Vec<u8>, Vec<u8>), String> {
    let cert = dhrust::net::panel_cert::ensure_pem(base)?;
    if cert.generated {
        // 提示运维「下一步要导入信任」，避免直接打开看到红屏后以为是程序故障
        crate::util::log_format(
            "已生成面板自签证书：{}（浏览器首次访问 https://… 前请导入该证书信任）",
            &[&cert.cert_path.display().to_string()],
        );
    }
    Ok((cert.cert_pem, cert.key_pem))
}

/// 重新签发（删除旧证书后重建）；调用方需提示「重启服务后生效」。
///
/// # 参数
/// - `base`：部署根目录。
///
/// # 返回值
/// 成功/失败。
pub fn renew(base: &Path) -> Result<(), String> {
    dhrust::net::panel_cert::renew(base).map(|_| ())
}

/// 证书状态（面板展示用 JSON）。字段见 [`dhrust::net::panel_cert::status`]。
///
/// # 参数
/// - `base`：部署根目录。
/// - `enabled`：配置里是否启用 TLS（本项目的「实际生效」与配置同值，故一并传入）。
/// - `port`：面板端口。
///
/// # 返回值
/// 状态 JSON。
pub fn status(base: &Path, enabled: bool, port: u16) -> serde_json::Value {
    dhrust::net::panel_cert::status(base, enabled, enabled, &format!("0.0.0.0:{port}"))
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
