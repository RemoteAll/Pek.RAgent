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
//! - 定时：按 `AutoUpgradeIntervalMinutes`（默认 60）检查；平台推送（后续 WS 通道）可立即触发；
//! - **复用已下载包**（2026-10-27）：待应用文件旁写 `{exe}.new.meta`（版本 + SHA-256）。
//!   目标版本未变且 SHA 一致则直接复用不重下；版本变化自动丢弃重下（避免"已下载却报错版本"）；
//! - **失败退避**（2026-10-27）：应用失败记 `{exe}.upgrade-fail.json`，同一目标版本退避期内
//!   不再重复下载完整包（发布源/本机环境有问题时，避免每轮白拉 10MB）。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

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
/// 应用失败后同一目标版本的退避时长（秒）。
///
/// 作用：避免"下载 → 应用失败 → 下轮重新下载"死循环（发布源包损坏、本机环境不满足等
/// 持久问题下，默认 60 分钟一轮 × 10MB 会持续白耗带宽）。退避期满自动重试。
const APPLY_FAIL_BACKOFF_SECS: i64 = 6 * 3600;

/// 待应用文件元数据（记录下载到的是哪个版本，供下轮判断能否复用）。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StagedMeta {
    /// 下载到的目标版本
    pub version: String,
    /// 文件 SHA-256（发行源未提供时为空）
    pub sha256: String,
    /// 下载时间（本地文本）
    pub at: String,
}

/// 一次"应用失败"记录（退避依据）。
#[derive(Clone, Debug, Serialize, Deserialize)]
struct ApplyFailure {
    /// 失败时对应的目标版本
    version: String,
    /// 失败时间（Unix 秒）
    failed_at: i64,
    /// 失败原因（截断）
    error: String,
}

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
    !matches!(*LAST_ATTEMPT.lock().unwrap(), Some(t) if t.elapsed() < interval)
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

    // 2) 目标包指纹（提前解析：待应用文件复用判断、补丁与全量校验都要用）
    let target_sha = entry
        .get("sha256")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();

    let exe = std::env::current_exe()
        .map(|p| util::lexical_normalize(&p))
        .map_err(|e| format!("无法定位当前程序文件：{e}"))?;
    let staged = PathBuf::from(format!("{}.new", exe.display()));

    // 2.1) 已有待应用文件：仅当版本 + SHA 与当前目标一致才复用（不重复下载）
    if staged.is_file() {
        if staged_is_current(&staged, &version, &target_sha) {
            return Ok(CheckOutcome {
                staged: true,
                message: format!("版本 {version} 已下载，等待应用"),
                latest: version,
            });
        }
        // 版本不符（发行源又发新版 / 旧包残留 / 无元数据）→ 丢弃重下，避免"已下载却应用错版本"
        let got = read_staged_meta(&staged)
            .map(|m| m.version)
            .unwrap_or_else(|| "未知版本".to_string());
        util::log_format(
            &format!("自动升级：待应用文件已过期（其中为 {got}，目标 {version}），已丢弃并将重新下载"),
            &[],
        );
        remove_staged(&staged);
    }

    // 2.2) 应用失败退避：同一目标版本刚失败过则暂不重下（避免每轮白拉完整包）
    if let Some(fail) = read_apply_failure(&exe).filter(|f| f.version == version) {
        let elapsed = chrono::Local::now().timestamp().saturating_sub(fail.failed_at);
        if elapsed < APPLY_FAIL_BACKOFF_SECS {
            let left_min = (APPLY_FAIL_BACKOFF_SECS - elapsed).div_euclid(60);
            return Ok(CheckOutcome {
                staged: false,
                message: format!(
                    "版本 {version} 上次应用失败（{}），约 {left_min} 分钟后重试，期间不重复下载",
                    fail.error
                ),
                latest: version,
            });
        }
        util::log_format(
            "自动升级：版本 {} 的应用失败退避已结束（上次原因：{}），重新尝试",
            &[&version, &fail.error],
        );
    }

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
            match try_patch(&fetch, purl, psha, &local, &target_sha, &staged, &version) {
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
    write_staged(&staged, &data, &version, &sha)?;
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
    version: &str,
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
    write_staged(staged, &new_bytes, version, target_sha)?;
    Ok(patch.len())
}

/// 可执行魔数（ELF / PE）。
fn is_executable(data: &[u8]) -> bool {
    data.starts_with(b"\x7FELF") || (data.len() > 2 && &data[..2] == b"MZ")
}

/// 待应用文件旁的元数据路径（`{exe}.new` → `{exe}.new.meta`）。
fn meta_path(staged: &Path) -> PathBuf {
    PathBuf::from(format!("{}.meta", staged.display()))
}

/// 应用失败记录路径（`{exe}.upgrade-fail.json`）。
fn failure_path(exe: &Path) -> PathBuf {
    PathBuf::from(format!("{}.upgrade-fail.json", exe.display()))
}

/// 读取待应用文件元数据（不存在/损坏返回 None）。
pub fn read_staged_meta(staged: &Path) -> Option<StagedMeta> {
    let text = std::fs::read_to_string(meta_path(staged)).ok()?;
    serde_json::from_str(&text).ok()
}

/// 删除待应用文件及其元数据。
fn remove_staged(staged: &Path) {
    let _ = std::fs::remove_file(staged);
    let _ = std::fs::remove_file(meta_path(staged));
}

/// 已下载文件是否就是当前目标（版本一致 + SHA 一致）。
///
/// 无元数据（旧版本下载的遗留文件、手工上传）一律视为过期——无法判断版本就不能复用。
fn staged_is_current(staged: &Path, version: &str, target_sha: &str) -> bool {
    match read_staged_meta(staged) {
        Some(meta) => {
            meta.version == version
                && (target_sha.is_empty() || meta.sha256.eq_ignore_ascii_case(target_sha))
        }
        None => false,
    }
}

/// 读取应用失败记录。
fn read_apply_failure(exe: &Path) -> Option<ApplyFailure> {
    let text = std::fs::read_to_string(failure_path(exe)).ok()?;
    serde_json::from_str(&text).ok()
}

/// 记录一次应用失败（由升级管线在失败分支调用），用于下轮退避。
///
/// 无元数据的候选（手工上传/外部替换）不做退避：那类场景不经过下载，退避无意义。
pub fn record_apply_failure(exe: &Path, error: &str) {
    let staged = PathBuf::from(format!("{}.new", exe.display()));
    let Some(meta) = read_staged_meta(&staged) else {
        return;
    };
    let fail = ApplyFailure {
        version: meta.version,
        failed_at: chrono::Local::now().timestamp(),
        error: error.chars().take(200).collect(),
    };
    if let Ok(text) = serde_json::to_string_pretty(&fail) {
        let _ = std::fs::write(failure_path(exe), text);
    }
    // 元数据同步清理（候选已改名 .failed，meta 不再有意义）
    let _ = std::fs::remove_file(meta_path(&staged));
}

/// 升级成功后清理失败记录。
pub fn clear_apply_failure(exe: &Path) {
    let _ = std::fs::remove_file(failure_path(exe));
}

/// 写入待应用文件（先写临时名再原子改名，防止半成品被升级管线捻起），
/// 随后写入版本元数据（供下轮判断能否复用）。
fn write_staged(staged: &std::path::Path, data: &[u8], version: &str, sha: &str) -> Result<(), String> {
    let tmp = PathBuf::from(format!("{}.download-tmp", staged.display()));
    std::fs::write(&tmp, data).map_err(|e| format!("写入升级文件失败：{e}"))?;
    std::fs::rename(&tmp, staged).map_err(|e| format!("提交升级文件失败：{e}"))?;
    let meta = StagedMeta {
        version: version.to_string(),
        sha256: sha.to_string(),
        at: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
    };
    if let Ok(text) = serde_json::to_string_pretty(&meta) {
        let _ = std::fs::write(meta_path(staged), text);
    }
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
        let mut cfg = AgentConfig {
            auto_upgrade_url: String::new(),
            ..AgentConfig::default()
        };
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
        let cfg = AgentConfig {
            auto_upgrade_url: String::new(),
            ..AgentConfig::default()
        };
        let err = check_once(&cfg).unwrap_err();
        assert!(err.contains("未配置"), "实际：{err}");
        // 默认配置（地址已预置官方平台）即视为启用
        assert!(enabled(&AgentConfig::default()));
    }

    #[test]
    fn staged_meta_roundtrip_and_match() {
        let dir = std::env::temp_dir().join(format!("ragent-staged-{}", dhrust::random::hex(4)));
        std::fs::create_dir_all(&dir).unwrap();
        let staged = dir.join("app.new");

        // 写入文件时同步写元数据
        write_staged(&staged, b"bin", "0.2.0", "abc123").unwrap();
        let meta = read_staged_meta(&staged).expect("应能读回元数据");
        assert_eq!(meta.version, "0.2.0");
        assert_eq!(meta.sha256, "abc123");

        // 版本 + SHA 匹配 → 可复用（不重复下载）
        assert!(staged_is_current(&staged, "0.2.0", "abc123"));
        assert!(staged_is_current(&staged, "0.2.0", "ABC123"), "SHA 比较应大小写不敏感");
        // 发行源又发新版 → 不可复用（必须重下，不能报"新版已下载"）
        assert!(!staged_is_current(&staged, "0.3.0", "abc123"));
        // 同版本重打包（SHA 变化）→ 不可复用
        assert!(!staged_is_current(&staged, "0.2.0", "def456"));

        // 无元数据（旧版遗留 / 手工上传）→ 不可复用
        let manual = dir.join("manual.new");
        std::fs::write(&manual, b"bin").unwrap();
        assert!(!staged_is_current(&manual, "0.2.0", ""));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn apply_failure_recorded_only_with_meta() {
        let dir = std::env::temp_dir().join(format!("ragent-fail-{}", dhrust::random::hex(4)));
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("pek-ragent");
        std::fs::write(&exe, b"bin").unwrap();
        let staged = PathBuf::from(format!("{}.new", exe.display()));

        // 无元数据（手工上传/外部替换）：不记录退避，保持既有重试行为
        record_apply_failure(&exe, "手工上传失败");
        assert!(read_apply_failure(&exe).is_none());

        // 有元数据（自动下载的）：记录目标版本与原因，并清理元数据
        write_staged(&staged, b"bin", "0.2.0", "abc").unwrap();
        record_apply_failure(&exe, "影子自检未通过");
        let fail = read_apply_failure(&exe).expect("应记录应用失败");
        assert_eq!(fail.version, "0.2.0");
        assert!(fail.error.contains("影子自检"), "实际：{}", fail.error);
        assert!(read_staged_meta(&staged).is_none(), "记录失败后应清理元数据");

        // 升级成功后清除记录
        clear_apply_failure(&exe);
        assert!(read_apply_failure(&exe).is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
