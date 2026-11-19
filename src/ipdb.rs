//! 本机 IP 库镜像（`/ipdb/*`）：同一台机器只下一次 IP 库，其它程序按回环地址直接取。
//!
//! 为什么要有它：Pek.ZIP 这类库默认每个进程各自向平台拉组件目录（约 87 KB）与安装包（数 MB 起）。
//! 服务器上既然已经装了 RAgent，让它统一缓存一次、本机进程都从它取，既省带宽也省时间；
//! 目录与签名**按平台原始字节转发**（镜像不重签），客户端仍用平台公钥验签，信任链不变。
//!
//! 目录约定（与平台 `/components/*` 同构，客户端只要把 MirrorUrl 指到 `/ipdb`）：
//! - `{base}/Ipdb/catalog.json`      平台目录原始字节
//! - `{base}/Ipdb/catalog.json.sig`  平台签名原始字节（配了 `IpdbPubKey` 才会落盘/校验）
//! - `{base}/Ipdb/components/<文件名>.zip`
//!
//! 安全边界：**仅回环访问**（对端不是 127.0.0.1/::1 一律当作不存在）；只读自身缓存目录，
//! 文件名白名单（不含路径分隔符与 `..`），不承接任何写操作。

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use dhrust::net::controller::{json_error, json_result, ActionResult};
use dhrust::net::http::{HttpRequest, HttpResponse};
use dhrust::net::router::Ctx;
use dhrust::net::static_files::StaticFiles;
use serde_json::{json, Value as Json};

use crate::config::AgentConfig;
use crate::plugins::{is_allowed_url, HttpFetcher, StoreFetcher};
use crate::util;
use crate::webpanel::WebPanel;

/// 镜像根目录名（`{base}/Ipdb`）。
pub const DIR: &str = "Ipdb";
const CATALOG: &str = "catalog.json";
const SIG: &str = "catalog.json.sig";
/// 只镜像平台「IP 库」分类（与 Pek.ZIP 的 `IP_DATABASE_CATEGORY` 一致）。
const CATEGORY: &str = "ip-database";
const MAX_CATALOG: usize = 2 * 1024 * 1024;
const MAX_PACKAGE: usize = 256 * 1024 * 1024;

pub fn root(base: &Path) -> PathBuf {
    base.join(DIR)
}

pub fn components_dir(base: &Path) -> PathBuf {
    root(base).join("components")
}

/// 下载授权身份目录（install.id / client.key / pubkey.txt / license）。
pub fn auth_dir(base: &Path) -> PathBuf {
    root(base).join("client")
}

fn auth_cfg(base: &Path) -> pek_ip::auth::IpAuthConfig {
    pek_ip::auth::IpAuthConfig {
        enabled: true,
        state_dir: Some(auth_dir(base)),
        ..Default::default()
    }
}

/// 从目录 URL 推导平台源站（`https://host/components/catalog.json` → `https://host`）。
pub fn origin_of(catalog_url: &str) -> Option<String> {
    let url = catalog_url.trim();
    let idx = url.find("/components/")?;
    let origin = &url[..idx];
    if origin.is_empty() {
        None
    } else {
        Some(origin.to_string())
    }
}

/// 本机 target 标识（与平台目录里的 `target` 字段同口径：`win-x64` / `linux-arm64` / `macos-arm64`）。
pub fn target() -> String {
    let os = match std::env::consts::OS {
        "windows" => "win",
        "linux" => "linux",
        "macos" => "macos",
        other => other,
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        other => other,
    };
    format!("{os}-{arch}")
}

/// 对端是否回环（`127.0.0.1:x` / `[::1]:x` / 裸 `::1`）。
fn is_loopback(remote: Option<&str>) -> bool {
    let Some(addr) = remote else {
        return false;
    };
    let host = if let Some(rest) = addr.strip_prefix('[') {
        rest.split(']').next().unwrap_or("")
    } else if addr.matches(':').count() > 1 {
        // 裸 IPv6（无端口），如 `::1`
        addr
    } else {
        addr.split(':').next().unwrap_or(addr)
    };
    host == "127.0.0.1" || host == "::1"
}

/// 缓存文件名白名单：不含路径分隔符、不以点开头、不含 `..`。
fn safe_file_name(name: &str) -> bool {
    !name.is_empty()
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains("..")
        && !name.starts_with('.')
        && name.len() <= 200
}

/// 从 URL 取文件名（去目录/查询串/片段）。
fn file_name(url: &str) -> Option<String> {
    let rest = url.split(['?', '#']).next()?;
    let name = rest.rsplit('/').next()?;
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

/// 静态服务 `/ipdb/catalog.json`、`/ipdb/catalog.json.sig`、`/ipdb/components/<文件名>`。
///
/// 非回环对端、路径穿越、文件不存在一律返回 `None`（交给后续 404）。
pub fn serve(base: &Path, req: &HttpRequest) -> Option<HttpResponse> {
    let plain = req.path.split(['?', '#']).next().unwrap_or("");
    let rest = plain.strip_prefix("/ipdb/")?;
    if !is_loopback(req.remote_addr.as_deref()) {
        return None;
    }
    let (dir, rel) = if rest == CATALOG || rest == SIG {
        (root(base), rest.to_string())
    } else {
        let name = rest.strip_prefix("components/")?;
        if !safe_file_name(name) {
            return None;
        }
        (components_dir(base), name.to_string())
    };
    let mut scoped = req.clone();
    scoped.path = format!("/{rel}");
    StaticFiles::new(dir).try_serve_request(&scoped)
}

/// 一次刷新的结果。
#[derive(Clone, Debug, Default)]
pub struct Report {
    pub catalog_bytes: usize,
    pub downloaded: usize,
    pub skipped: usize,
    pub warnings: Vec<String>,
}

/// 刷新镜像：拉目录（可选验签）→ 挑 `ip-database` 里 `any`/本机 target 的条目 →
/// 本地已有且 SHA-256 一致就跳过，否则下载并校验后落盘。
pub fn refresh(base: &Path, cfg: &AgentConfig, fetch: &dyn StoreFetcher) -> Result<Report, String> {
    let url = cfg.ipdb_source_url.trim();
    if url.is_empty() {
        return Err("未配置 IpdbSourceUrl（镜像源目录地址）".to_string());
    }
    if !is_allowed_url(url) {
        return Err(format!("镜像源地址仅允许 https（127.0.0.1 例外）：{url}"));
    }

    let bytes = fetch
        .get(url, MAX_CATALOG)
        .map_err(|e| format!("拉取组件目录失败：{e}（{url}）"))?;

    // 配了公钥就强制验签（目录被篡改宁可整轮失败，不静默放行）
    let pubkey = cfg.ipdb_pubkey.trim();
    if !pubkey.is_empty() {
        let sig_url = format!("{url}.sig");
        let sig = fetch
            .get(&sig_url, 8192)
            .map_err(|e| format!("拉取目录签名失败：{e}（{sig_url}）"))?;
        dhrust::plugin::verify_base64(pubkey, &bytes, &String::from_utf8_lossy(&sig))
            .map_err(|e| format!("目录签名校验失败：{e}（核对 IpdbPubKey；目录可能被篡改）"))?;
        write_atomic(&root(base).join(SIG), &sig)?;
    }
    write_atomic(&root(base).join(CATALOG), &bytes)?;

    let json: Json = serde_json::from_slice(&bytes)
        .map_err(|e| format!("catalog.json 解析失败：{e}（{url}）"))?;
    let Some(list) = json.get("components").and_then(|v| v.as_array()) else {
        return Err("catalog.json 缺少 components 数组".to_string());
    };

    let mut report = Report {
        catalog_bytes: bytes.len(),
        ..Default::default()
    };
    // 平台对组件包要求下载票据（直连 URL 一律 403）：开 IpdbAutoEnroll 就自动注册 + 取票。
    // 实现直接复用 Pek.IP 的 AuthClient（enroll/ticket 与 Pek.ZIP 同一套协议）。
    let origin = origin_of(url);
    let mut auth: Option<pek_ip::auth::AuthClient> = if cfg.ipdb_auto_enroll {
        match pek_ip::auth::AuthClient::load_or_create(&auth_cfg(base), base) {
            Ok(client) => Some(client),
            Err(e) => {
                report
                    .warnings
                    .push(format!("初始化下载授权身份失败：{e}（目录 {}）", auth_dir(base).display()));
                None
            }
        }
    } else {
        None
    };
    let mut pending_reported = false;
    let host_target = target();
    for item in list {
        if item.get("category").and_then(|v| v.as_str()) != Some(CATEGORY) {
            continue;
        }
        let target_text = item.get("target").and_then(|v| v.as_str()).unwrap_or("");
        if target_text != "any" && target_text != host_target {
            continue;
        }
        let id = item.get("id").and_then(|v| v.as_str()).unwrap_or("");
        if id.ends_with("-v6") && !cfg.ipdb_include_v6 {
            continue;
        }
        let pkg_url = item.get("url").and_then(|v| v.as_str()).unwrap_or("");
        let sha = item.get("sha256").and_then(|v| v.as_str()).unwrap_or("");
        if pkg_url.is_empty() || sha.len() != 64 {
            report
                .warnings
                .push(format!("条目 {id} 缺 url/sha256（{pkg_url}），已跳过"));
            continue;
        }
        let Some(name) = file_name(pkg_url) else {
            report.warnings.push(format!("条目 {id} 的 url 取不到文件名：{pkg_url}"));
            continue;
        };
        if !safe_file_name(&name) {
            report
                .warnings
                .push(format!("条目 {id} 的 url 不是安全文件名：{pkg_url}"));
            continue;
        }
        let dest = components_dir(base).join(&name);
        if let Some(got) = file_sha256(&dest) {
            if got.eq_ignore_ascii_case(sha) {
                report.skipped += 1;
                continue;
            }
        }
        // 取下载地址：有授权身份就用票据 URL，否则直连（新平台直连会 403，下面会给出提示）。
        let mut download_url = pkg_url.to_string();
        if let (Some(client), Some(origin)) = (auth.as_mut(), origin.as_deref()) {
            let version = item.get("version").and_then(|v| v.as_str()).unwrap_or("");
            match client.ticket(origin, id, version) {
                Ok(url) => download_url = url,
                Err(pek_ip::Error::Unsupported(_)) => {
                    // 旧平台无授权接口：保持直连（公开下载）
                }
                Err(err) => {
                    let msg = err.to_string();
                    let pending = msg.contains("批准") || msg.contains("pending");
                    if pending {
                        if !pending_reported {
                            pending_reported = true;
                            report.warnings.push(format!(
                                "已向平台注册下载身份，等待管理员批准（Pek.RPanlServer「分发中心 → IP 授权」；installId={}，状态目录 {}）：{msg}",
                                client.install_id(),
                                auth_dir(base).display()
                            ));
                        }
                    } else {
                        report
                            .warnings
                            .push(format!("申请 {id} 下载票据失败：{msg}（installId={}）", client.install_id()));
                    }
                    continue;
                }
            }
        }
        match fetch.get(&download_url, MAX_PACKAGE) {
            Ok(data) => {
                let got = dhrust::sign::sha256_hex(&data);
                if !got.eq_ignore_ascii_case(sha) {
                    report.warnings.push(format!(
                        "{name} SHA-256 不匹配，已丢弃（期望 {sha}，实际 {got}；{pkg_url}）"
                    ));
                    continue;
                }
                match write_atomic(&dest, &data) {
                    Ok(()) => report.downloaded += 1,
                    Err(e) => report.warnings.push(format!("写入 {name} 失败：{e}")),
                }
            }
            Err(e) => {
                // 403 几乎总是「平台要求下载票据」：目录能公开转发，安装包却需要授权。
                // 这不是重试能解决的，必须在平台侧批准该客户端，否则镜像永远只能转目录。
                let hint = if e.contains("403") {
                    "；平台返回 403：组件包需要下载票据（在 Pek.RPanlServer「分发中心 → IP 授权」批准客户端后才会放行）"
                } else {
                    ""
                };
                report
                    .warnings
                    .push(format!("下载 {name} 失败：{e}{hint}（{pkg_url}）"));
            }
        }
    }
    Ok(report)
}

/// 镜像状态（面板/CLI 展示：有哪些文件、多大、最近刷新时间）。
pub fn status(base: &Path) -> Json {
    let dir = components_dir(base);
    let mut files = Vec::new();
    let mut total: u64 = 0;
    if let Ok(entries) = fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let size = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            total += size;
            files.push(json!({
                "name": path.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default(),
                "sizeKB": size / 1024,
            }));
        }
    }
    files.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    let catalog = fs::metadata(root(base).join(CATALOG))
        .ok()
        .map(|m| m.len())
        .unwrap_or(0);
    let report = LAST_REPORT.lock().unwrap().clone();
    let last_error = LAST_ERROR.lock().unwrap().clone();
    json!({
        "dir": dir.to_string_lossy(),
        "catalogBytes": catalog,
        "files": files,
        "totalKB": total / 1024,
        "lastError": last_error,
        "lastWarnings": report.as_ref().map(|r| r.warnings.clone()).unwrap_or_default(),
        "lastDownloaded": report.as_ref().map(|r| r.downloaded).unwrap_or(0),
        "lastSkipped": report.as_ref().map(|r| r.skipped).unwrap_or(0),
    })
}

// ————— 面板动作（可观测 + 手动刷新） —————

/// `GET /star/ipdbStatus`：镜像开关/源地址/缓存文件清单。
pub fn ipdb_status(panel: &WebPanel, ctx: &Ctx) -> ActionResult {
    if !panel.check_auth(ctx) {
        return json_error(401, "Unauthorized");
    }
    let cfg = panel.config();
    let mut snapshot = status(panel.base());
    if let Some(obj) = snapshot.as_object_mut() {
        obj.insert("enabled".into(), json!(enabled(&cfg)));
        obj.insert("sourceUrl".into(), json!(cfg.ipdb_source_url.trim()));
        obj.insert("refreshMinutes".into(), json!(cfg.ipdb_refresh_minutes));
        obj.insert("signature".into(), json!(!cfg.ipdb_pubkey.trim().is_empty()));
        obj.insert("includeV6".into(), json!(cfg.ipdb_include_v6));
        obj.insert("autoEnroll".into(), json!(cfg.ipdb_auto_enroll));
        // 授权身份（installId）能让运维直接去平台「IP 授权」页找到对应待批准项
        let install_id = pek_ip::auth::AuthClient::load_or_create(&auth_cfg(panel.base()), panel.base())
            .ok()
            .map(|c| c.install_id().to_string())
            .unwrap_or_default();
        obj.insert("installId".into(), json!(install_id));
        obj.insert(
            "mirrorUrl".into(),
            json!(format!("http://127.0.0.1:{}/ipdb", panel.port())),
        );
    }
    json_result(0, "", Some(snapshot))
}

/// `POST /star/ipdbRefresh`：立即刷新一次镜像（忽略间隔）。
pub fn ipdb_refresh(panel: &WebPanel, ctx: &Ctx) -> ActionResult {
    if !panel.check_auth(ctx) {
        return json_error(401, "Unauthorized");
    }
    let cfg = panel.config();
    if !enabled(&cfg) {
        return json_error(400, "IP 库镜像未启用（先打开 IpdbMirrorEnabled）");
    }
    if trigger(cfg, panel.base().to_path_buf(), true) {
        json_result(0, "已开始刷新 IP 库镜像（后台执行，稍后刷新本页看结果）", None)
    } else {
        json_error(409, "已有刷新任务在跑，稍后再试")
    }
}

// ————— 周期刷新（与 self_upgrade 同一套「到期才跑 + 并发去重」口径） —————

static LAST_ATTEMPT: Mutex<Option<Instant>> = Mutex::new(None);
static RUNNING: AtomicBool = AtomicBool::new(false);
/// 最近一次刷新的结果/错误（面板状态页用：一眼看出镜像能不能用）。
static LAST_REPORT: Mutex<Option<Report>> = Mutex::new(None);
static LAST_ERROR: Mutex<Option<String>> = Mutex::new(None);

/// 是否启用镜像（开关打开且配了源地址）。
pub fn enabled(cfg: &AgentConfig) -> bool {
    cfg.ipdb_mirror_enabled && !cfg.ipdb_source_url.trim().is_empty()
}

/// 是否到刷新时间。
fn due(cfg: &AgentConfig) -> bool {
    let minutes = u64::from(cfg.ipdb_refresh_minutes.clamp(5, 1440));
    let interval = Duration::from_secs(minutes * 60);
    !matches!(*LAST_ATTEMPT.lock().unwrap(), Some(t) if t.elapsed() < interval)
}

/// 触发一次刷新（独立线程；并发去重；`force` 忽略间隔）。返回是否真正触发。
pub fn trigger(cfg: AgentConfig, base: PathBuf, force: bool) -> bool {
    if !enabled(&cfg) {
        return false;
    }
    if !force && !due(&cfg) {
        return false;
    }
    if RUNNING.swap(true, Ordering::SeqCst) {
        return false;
    }
    *LAST_ATTEMPT.lock().unwrap() = Some(Instant::now());
    std::thread::spawn(move || {
        let result = refresh(&base, &cfg, &HttpFetcher);
        RUNNING.store(false, Ordering::SeqCst);
        match result {
            Ok(r) => {
                *LAST_ERROR.lock().unwrap() = None;
                *LAST_REPORT.lock().unwrap() = Some(r.clone());
                let (catalog, down, skip, warn) = (
                    r.catalog_bytes.to_string(),
                    r.downloaded.to_string(),
                    r.skipped.to_string(),
                    r.warnings.len().to_string(),
                );
                util::log_format(
                    "IP 库镜像刷新完成：目录 {} 字节，新下载 {}，复用 {}，告警 {}",
                    &[&catalog, &down, &skip, &warn],
                );
                for w in r.warnings.iter().take(10) {
                    util::log_format("IP 库镜像告警：{}", &[w]);
                }
            }
            Err(e) => {
                *LAST_ERROR.lock().unwrap() = Some(e.clone());
                util::log_format(
                    "IP 库镜像刷新失败：{}（源 {}，目录 {}）",
                    &[&e, cfg.ipdb_source_url.trim(), &base.to_string_lossy()],
                );
            }
        }
    });
    true
}

// ————— 工具 —————

fn file_sha256(path: &Path) -> Option<String> {
    let data = fs::read(path).ok()?;
    Some(dhrust::sign::sha256_hex(&data))
}

/// 原子写（同目录 `.tmp-<pid>` + rename），避免读到写一半的文件。
fn write_atomic(path: &Path, data: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("创建目录失败：{e}（{}）", parent.display()))?;
    }
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    fs::write(&tmp, data).map_err(|e| format!("写入临时文件失败：{e}（{}）", tmp.display()))?;
    fs::rename(&tmp, path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        format!("替换目标文件失败：{e}（{}）", path.display())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake {
        files: std::collections::HashMap<String, Vec<u8>>,
        hits: std::cell::RefCell<Vec<String>>,
    }

    impl Fake {
        fn new(items: &[(&str, &[u8])]) -> Self {
            let mut files = std::collections::HashMap::new();
            for (k, v) in items {
                files.insert((*k).to_string(), v.to_vec());
            }
            Self {
                files,
                hits: std::cell::RefCell::new(Vec::new()),
            }
        }
    }

    impl StoreFetcher for Fake {
        fn get(&self, url: &str, max: usize) -> Result<Vec<u8>, String> {
            self.hits.borrow_mut().push(url.to_string());
            let data = self.files.get(url).ok_or_else(|| format!("404：{url}"))?;
            if data.len() > max {
                return Err("超出上限".to_string());
            }
            Ok(data.clone())
        }
    }

    const BASE_URL: &str = "https://p.example/components/catalog.json";

    fn tmp_base(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ragent-ipdb-{}-{}", name, std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn refresh_only_takes_ip_database_and_verifies_sha256() {
        let base = tmp_base("ok");
        let pkg = b"zip-bytes".to_vec();
        let sha = dhrust::sign::sha256_hex(&pkg);
        let catalog = format!(
            r#"{{"components":[
                {{"id":"ip2region-v4","category":"ip-database","target":"any","url":"https://p.example/components/v4.zip","sha256":"{sha}"}},
                {{"id":"ip2region-v6","category":"ip-database","target":"any","url":"https://p.example/components/v6.zip","sha256":"{sha}"}},
                {{"id":"dbserver-mysql","category":"driver","target":"any","url":"https://p.example/components/mysql.zip","sha256":"{sha}"}},
                {{"id":"other-arch","category":"ip-database","target":"linux-sparc","url":"https://p.example/components/sparc.zip","sha256":"{sha}"}}
            ]}}"#
        );
        let fake = Fake::new(&[
            (BASE_URL, catalog.as_bytes()),
            ("https://p.example/components/v4.zip", &pkg),
            ("https://p.example/components/v6.zip", &pkg),
            ("https://p.example/components/mysql.zip", &pkg),
            ("https://p.example/components/sparc.zip", &pkg),
        ]);
        let mut cfg = AgentConfig::default();
        cfg.ipdb_source_url = BASE_URL.to_string();

        let report = refresh(&base, &cfg, &fake).expect("refresh");
        assert_eq!(report.catalog_bytes, catalog.len());
        assert_eq!(report.downloaded, 1, "只下 ip2region-v4（v6 默认排除）");
        assert!(base.join("Ipdb/components/v4.zip").is_file());
        assert!(!base.join("Ipdb/components/v6.zip").exists());
        assert!(!base.join("Ipdb/components/mysql.zip").exists());
        assert!(!base.join("Ipdb/components/sparc.zip").exists());
        assert!(base.join("Ipdb/catalog.json").is_file());

        // 第二次：SHA-256 一致 → 不再下载（省带宽）
        let report2 = refresh(&base, &cfg, &fake).expect("refresh2");
        assert_eq!(report2.downloaded, 0);
        assert_eq!(report2.skipped, 1);

        // v6 开关打开 → 才会下 v6
        cfg.ipdb_include_v6 = true;
        let report3 = refresh(&base, &cfg, &fake).expect("refresh3");
        assert_eq!(report3.downloaded, 1);
        assert!(base.join("Ipdb/components/v6.zip").is_file());

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn refresh_rejects_tampered_package() {
        let base = tmp_base("bad-sha");
        let catalog: Vec<u8> = format!(
            r#"{{"components":[{{"id":"ip2region-v4","category":"ip-database","target":"any","url":"https://p.example/components/v4.zip","sha256":"{}"}}]}}"#,
            "a".repeat(64)
        )
        .into_bytes();
        let fake = Fake::new(&[
            (BASE_URL, &catalog),
            ("https://p.example/components/v4.zip", b"tampered".as_slice()),
        ]);
        let mut cfg = AgentConfig::default();
        cfg.ipdb_source_url = BASE_URL.to_string();

        let report = refresh(&base, &cfg, &fake).expect("refresh");
        assert_eq!(report.downloaded, 0);
        assert_eq!(report.warnings.len(), 1);
        assert!(report.warnings[0].contains("SHA-256 不匹配"));
        // 不落盘：坏包绝不能出现在缓存里
        assert!(!base.join("Ipdb/components/v4.zip").exists());
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn refresh_rejects_non_https_source() {
        let base = tmp_base("bad-url");
        let fake = Fake::new(&[]);
        let mut cfg = AgentConfig::default();
        cfg.ipdb_source_url = "http://evil.example/catalog.json".to_string();
        let err = refresh(&base, &cfg, &fake).unwrap_err();
        assert!(err.contains("仅允许 https"), "{err}");
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn serve_is_loopback_only_and_rejects_traversal() {
        let base = tmp_base("serve");
        fs::create_dir_all(components_dir(&base)).unwrap();
        fs::write(root(&base).join(CATALOG), b"{}").unwrap();
        fs::write(components_dir(&base).join("v4.zip"), b"zip").unwrap();

        let mut req = HttpRequest {
            method: "GET".into(),
            path: "/ipdb/catalog.json".into(),
            query: String::new(),
            headers: Vec::new(),
            body: Default::default(),
            remote_addr: Some("127.0.0.1:5555".into()),
        };
        assert!(serve(&base, &req).is_some());
        req.path = "/ipdb/components/v4.zip".into();
        assert!(serve(&base, &req).is_some());
        req.path = "/ipdb/components/../Ipdb/catalog.json".into();
        assert!(serve(&base, &req).is_none(), "路径穿越必须被拒");
        req.path = "/ipdb/components/missing.zip".into();
        assert!(serve(&base, &req).is_none());

        // 非回环对端：一律当作不存在
        req.path = "/ipdb/catalog.json".into();
        req.remote_addr = Some("10.0.0.8:5555".into());
        assert!(serve(&base, &req).is_none());

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn loopback_and_file_name_rules() {
        assert!(is_loopback(Some("127.0.0.1:80")));
        assert!(is_loopback(Some("[::1]:80")));
        assert!(is_loopback(Some("::1")));
        assert!(!is_loopback(Some("10.0.0.1:80")));
        assert!(!is_loopback(None));

        assert_eq!(
            file_name("https://p.sc8.fun/components/ip2region-v4-2026.10-any.zip?v=1").as_deref(),
            Some("ip2region-v4-2026.10-any.zip")
        );
        assert!(safe_file_name("a-b_c.zip"));
        assert!(!safe_file_name("../x"));
        assert!(!safe_file_name("a/b"));
        assert!(!safe_file_name(".hidden"));
    }

    #[test]
    fn origin_is_derived_from_catalog_url() {
        assert_eq!(
            origin_of("https://p.sc8.fun/components/catalog.json").as_deref(),
            Some("https://p.sc8.fun")
        );
        assert_eq!(
            origin_of("http://127.0.0.1:8899/ipdb/components/catalog.json").as_deref(),
            Some("http://127.0.0.1:8899/ipdb")
        );
        // 不是组件目录形态 → 取不到源站（拒绝猜测）
        assert!(origin_of("https://p.sc8.fun/catalog.json").is_none());
    }
}
