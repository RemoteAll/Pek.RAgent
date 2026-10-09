//! 代理自动升级：从 Pek.RPanlServer「代理发行源」检查新版本并下载。
//!
//! 与「上传即升级」手工通道（`Update/` 目录 / Web 上传 / 外部替换检测）互补：
//! 本模块负责"检查 → 下载（优先增量补丁）→ 写入 `{exe}.new`"，
//! 替换 / 影子自检 / 重启助手交给既有升级管线（`agent::check_self_upgrade`）自动完成。
//!
//! - 启用条件：`AutoUpgradeUrl` 非空（默认已指向官方平台）；`PluginStorePubKey` 可选——
//!   配置后强制验签（复用插件源同一把公钥），留空则仅 HTTPS + SHA-256 校验（与插件源语义一致）；
//! - 增量补丁：仅当发行源提供与"本机二进制 SHA-256"精确匹配的补丁时使用（`dhrust::patch`），
//!   任何环节失败自动回退完整包下载；
//! - 定时：按 `AutoUpgradeIntervalMinutes`（默认 60）检查；平台推送（后续 WS 通道）可立即触发。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::config::AgentConfig;
use crate::plugins::{HttpFetcher, StoreFetcher};
use crate::util;

/// catalog 体积上限（1MB）。
const MAX_CATALOG_SIZE: usize = 1024 * 1024;
/// 签名文件上限。
const MAX_SIG_SIZE: usize = 4096;
/// 全量包体积上限（64MB）。
const MAX_PACKAGE_SIZE: usize = 64 * 1024 * 1024;
/// 补丁体积上限（16MB；正常为完整包的 3%~10%）。
const MAX_PATCH_SIZE: usize = 16 * 1024 * 1024;

/// 当前运行平台（与发行目录的 `platform` 字段对齐）。
pub fn current_platform() -> &'static str {
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    {
        "win-x64"
    }
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        "linux-x64"
    }
    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    {
        "linux-arm64"
    }
    #[cfg(all(target_os = "linux", target_arch = "riscv64"))]
    {
        "linux-riscv64"
    }
    #[cfg(all(target_os = "linux", target_arch = "loongarch64"))]
    {
        "linux-loongarch64"
    }
    #[cfg(not(any(
        all(target_os = "windows", target_arch = "x86_64"),
        all(target_os = "linux", target_arch = "x86_64"),
        all(target_os = "linux", target_arch = "aarch64"),
        all(target_os = "linux", target_arch = "riscv64"),
        all(target_os = "linux", target_arch = "loongarch64"),
    )))]
    {
        "unknown"
    }
}

/// 最近一次检查状态（面板展示）。
#[derive(Clone, Debug, Default)]
pub struct UpgradeStatus {
    /// 最近检查时间（本地文本）
    pub checked_at: String,
    /// 结果描述
    pub message: String,
    /// 发行源上最新版本（发现更高版本时非空）
    pub latest: String,
}

/// 单次检查结果。
#[derive(Clone, Debug)]
pub struct CheckOutcome {
    /// 已下载待应用（`{exe}.new` 就绪）
    pub staged: bool,
    /// 结果描述
    pub message: String,
    /// 发行源最新版本（无 = 空）
    pub latest: String,
}

static LAST_CHECK: Mutex<Option<UpgradeStatus>> = Mutex::new(None);
static LAST_ATTEMPT: Mutex<Option<Instant>> = Mutex::new(None);
static CHECKING: AtomicBool = AtomicBool::new(false);

/// 最近一次检查状态快照。
pub fn status() -> Option<UpgradeStatus> {
    LAST_CHECK.lock().unwrap().clone()
}

/// 自动升级是否已启用（配置了发行源地址即启用；公钥可选：有则强制验签，无则仅 HTTPS+SHA-256）。
pub fn enabled(cfg: &AgentConfig) -> bool {
    !cfg.auto_upgrade_url.trim().is_empty()
}

/// 是否到检查时间（`force` 忽略间隔）。
fn due(cfg: &AgentConfig, force: bool) -> bool {
    if !enabled(cfg) {
        return false;
    }
    if force {
        return true;
    }
    let minutes = u64::from(cfg.auto_upgrade_interval_minutes.clamp(5, 1440));
    let interval = Duration::from_secs(minutes * 60);
    match *LAST_ATTEMPT.lock().unwrap() {
        Some(t) if t.elapsed() < interval => false,
        _ => true,
    }
}

/// 触发检查（独立线程执行；并发去重；`force` 忽略间隔）。
///
/// 返回是否真正触发（未启用 / 正在检查 / 未到间隔时返回 false）。
pub fn trigger(cfg: AgentConfig, force: bool) -> bool {
    if !due(&cfg, force) {
        return false;
    }
    if CHECKING.swap(true, Ordering::SeqCst) {
        return false;
    }
    *LAST_ATTEMPT.lock().unwrap() = Some(Instant::now());
    std::thread::spawn(move || {
        let result = check_once(&cfg);
        CHECKING.store(false, Ordering::SeqCst);
        let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
        let status = match &result {
            Ok(outcome) => {
                if outcome.staged {
                    util::log_format(
                        "自动升级：{}（当前 {}），已写入待应用文件，将自动完成替换重启",
                        &[&outcome.message, env!("CARGO_PKG_VERSION")],
                    );
                } else {
                    util::log_format("自动升级检查：{}", &[&outcome.message]);
                }
                UpgradeStatus {
                    checked_at: now,
                    message: outcome.message.clone(),
                    latest: outcome.latest.clone(),
                }
            }
            Err(e) => {
                util::log_format("自动升级检查失败：{}", &[e]);
                UpgradeStatus {
                    checked_at: now,
                    message: format!("检查失败：{e}"),
                    latest: String::new(),
                }
            }
        };
        *LAST_CHECK.lock().unwrap() = Some(status);
    });
    true
}

/// 单次检查（阻塞；调用方确保已放入独立线程）。
///
/// 流程：拉取并验签 ± 找本平台条目 → 版本比较 → 优先补丁（来源 SHA-256 须与本机一致）→
/// 回退全量；产物写入 `{exe}.new`（先写临时名再原子改名）。
pub fn check_once(cfg: &AgentConfig) -> Result<CheckOutcome, String> {
    let url = cfg.auto_upgrade_url.trim();
    if url.is_empty() {
        return Err("未配置自动升级源".to_string());
    }
    let pubkey = cfg.plugin_store_pubkey.trim();
    let fetch = HttpFetcher;

    // 1) catalog（配置了公钥则强制验签；留空仅依赖 HTTPS + SHA-256，与插件源同级安全）
    let bytes = fetch.get(url, MAX_CATALOG_SIZE)?;
    if !pubkey.is_empty() {
        let sig = fetch.get(&format!("{url}.sig"), MAX_SIG_SIZE)?;
        dhrust::plugin::verify_base64(pubkey, &bytes, &String::from_utf8_lossy(&sig))
            .map_err(|e| format!("发行源签名校验失败：{e}"))?;
    }
    let json: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|e| format!("catalog.json 解析失败：{e}"))?;
    let platform = current_platform();
    let Some(list) = json.get("releases").and_then(|v| v.as_array()) else {
        return Err("catalog.json 缺少 releases 数组".to_string());
    };
    let Some(entry) = list
        .iter()
        .find(|e| e.get("platform").and_then(|v| v.as_str()) == Some(platform))
    else {
        return Ok(CheckOutcome {
            staged: false,
            message: format!("发行源没有 {platform} 平台的版本"),
            latest: String::new(),
        });
    };
    let version = entry
        .get("version")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let current = env!("CARGO_PKG_VERSION");
    if !dhrust::version::is_newer(&version, current) {
        return Ok(CheckOutcome {
            staged: false,
            message: format!("已是最新版本（{current}）"),
            latest: version,
        });
    }

    // 2) 已有待应用文件：等待升级管线处理，避免重复下载
    let exe = std::env::current_exe()
        .map(|p| util::lexical_normalize(&p))
        .map_err(|e| format!("无法定位当前程序文件：{e}"))?;
    let staged = PathBuf::from(format!("{}.new", exe.display()));
    if staged.is_file() {
        return Ok(CheckOutcome {
            staged: true,
            message: format!("版本 {version} 已下载，等待应用"),
            latest: version,
        });
    }

    let target_sha = entry
        .get("sha256")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let local = std::fs::read(&exe).map_err(|e| format!("读取当前程序失败：{e}"))?;
    let local_sha = dhrust::sign::sha256_hex(&local);

    // 3) 增量补丁优先（来源 SHA-256 必须与本机文件一致）
    let mut patch_note = String::new();
    if let Some(patches) = entry.get("patches").and_then(|v| v.as_array()) {
        for p in patches {
            let from = p.get("fromVersion").and_then(|v| v.as_str()).unwrap_or("");
            let source_sha = p
                .get("sourceSha256")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if from != current || !source_sha.eq_ignore_ascii_case(&local_sha) {
                continue;
            }
            let purl = p.get("url").and_then(|v| v.as_str()).unwrap_or("");
            let psha = p.get("sha256").and_then(|v| v.as_str()).unwrap_or("");
            match try_patch(&fetch, purl, psha, &local, &target_sha, &staged) {
                Ok(size) => {
                    return Ok(CheckOutcome {
                        staged: true,
                        message: format!(
                            "增量升级就绪：{current} → {version}（补丁 {size} 字节，原包 {} 字节）",
                            local.len()
                        ),
                        latest: version,
                    });
                }
                Err(e) => {
                    util::log_format(
                        "自动升级：增量补丁不可用（{}），改用完整包下载",
                        &[&e],
                    );
                    patch_note = format!("（增量补丁不可用：{e}）");
                    break;
                }
            }
        }
    }

    // 4) 完整包
    let full_url = entry.get("url").and_then(|v| v.as_str()).unwrap_or("");
    if full_url.is_empty() {
        return Err("发行源条目缺少下载地址".to_string());
    }
    let data = fetch.get(full_url, MAX_PACKAGE_SIZE)?;
    let sha = dhrust::sign::sha256_hex(&data);
    if !target_sha.is_empty() && !sha.eq_ignore_ascii_case(&target_sha) {
        return Err(format!(
            "完整包校验失败：SHA-256 不匹配（期望 {}…，实际 {}…）",
            &target_sha[..target_sha.len().min(16)],
            &sha[..16]
        ));
    }
    if !is_executable(&data) {
        return Err("完整包格式校验失败：不是可执行程序（ELF/PE）".to_string());
    }
    write_staged(&staged, &data)?;
    Ok(CheckOutcome {
        staged: true,
        message: format!(
            "升级包已下载就绪：{current} → {version}（{} 字节）{patch_note}",
            data.len()
        ),
        latest: version,
    })
}

/// 尝试补丁路径：下载 → 校验 → 以本机二进制为字典还原 → 校验目标 → 写待应用文件。
fn try_patch(
    fetch: &HttpFetcher,
    url: &str,
    expect_sha: &str,
    local: &[u8],
    target_sha: &str,
    staged: &std::path::Path,
) -> Result<usize, String> {
    if url.is_empty() {
        return Err("补丁地址为空".to_string());
    }
    let patch = fetch.get(url, MAX_PATCH_SIZE)?;
    let sha = dhrust::sign::sha256_hex(&patch);
    if !expect_sha.is_empty() && !sha.eq_ignore_ascii_case(expect_sha) {
        return Err("补丁 SHA-256 不匹配".to_string());
    }
    let new_bytes = dhrust::patch::apply_zstd(local, &patch, dhrust::patch::MAX_OUTPUT_64MB)?;
    let new_sha = dhrust::sign::sha256_hex(&new_bytes);
    if !target_sha.is_empty() && !new_sha.eq_ignore_ascii_case(target_sha) {
        return Err("还原目标校验失败（SHA-256 不匹配）".to_string());
    }
    if !is_executable(&new_bytes) {
        return Err("还原结果不是可执行程序（ELF/PE）".to_string());
    }
    write_staged(staged, &new_bytes)?;
    Ok(patch.len())
}

/// 可执行魔数（ELF / PE）。
fn is_executable(data: &[u8]) -> bool {
    data.starts_with(b"\x7FELF") || (data.len() > 2 && &data[..2] == b"MZ")
}

/// 写入待应用文件（先写临时名再原子改名，防止半成品被升级管线捡起）。
fn write_staged(staged: &std::path::Path, data: &[u8]) -> Result<(), String> {
    let tmp = PathBuf::from(format!("{}.download-tmp", staged.display()));
    std::fs::write(&tmp, data).map_err(|e| format!("写入升级文件失败：{e}"))?;
    std::fs::rename(&tmp, staged).map_err(|e| format!("提交升级文件失败：{e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_mapping_is_known() {
        // 测试运行平台应能映射（本机为 win-x64 / linux-x64 等已知组合）
        assert_ne!(current_platform(), "unknown");
    }

    #[test]
    fn due_respects_interval_and_force() {
        let mut cfg = AgentConfig::default();
        // 清空默认地址 → 永不检查
        cfg.auto_upgrade_url = String::new();
        assert!(!due(&cfg, true));
        cfg.auto_upgrade_url = "https://x.example/catalog.json".to_string();
        // 首次：到点
        assert!(due(&cfg, false));
        *LAST_ATTEMPT.lock().unwrap() = Some(Instant::now());
        // 刚检查过 → 不到点；force 可忽略
        assert!(!due(&cfg, false));
        assert!(due(&cfg, true));
        *LAST_ATTEMPT.lock().unwrap() = None;
    }

    #[test]
    fn check_requires_configuration() {
        let mut cfg = AgentConfig::default();
        cfg.auto_upgrade_url = String::new();
        let err = check_once(&cfg).unwrap_err();
        assert!(err.contains("未配置"), "实际：{err}");
        // 默认配置（地址已预置官方平台）即视为启用
        assert!(enabled(&AgentConfig::default()));
    }
}
