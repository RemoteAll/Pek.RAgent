//! 本机 IP 库镜像（`/ipdb/*`）：同一台机器只下一次 IP 库，其它程序按回环地址直接取。
//!
//! 为什么要有它：Pek.ZIP 这类库默认每个进程各自向平台拉组件目录（约 87 KB）与安装包（数 MB 起）。
//! 服务器上既然已经装了 RAgent，让它统一缓存一次、本机进程都从它取，既省带宽也省时间；
//! 目录与签名**按平台原始字节转发**（镜像不重签），客户端仍用平台公钥验签，信任链不变。
//!
//! 目录约定（与平台 `/components/*` 同构，客户端只要把 MirrorUrl 指到 `/ipdb`）：
//! - `{base}/Ipdb/catalog.json`      平台目录原始字节
//! - `{base}/Ipdb/catalog.json.sig`  平台签名原始字节（**平台给就转发**；配了 `IpdbPubKey` 才额外校一遍）
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
/// 最近一次刷新的结果快照（落盘）：服务重启后面板仍能看到「上次什么时候刷的、有没有告警」。
const LAST_REFRESH: &str = "last-refresh.json";
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
    // 目录与签名**手工下发**：`StaticFiles` 只认 MIME 表里的扩展，`.sig` 会被当成未知类型 404
    //（实测踩过：文件在盘上、`/ipdb/catalog.json.sig` 却 404，导致有公钥的客户端全部回退平台）。
    if rest == CATALOG || rest == SIG {
        let bytes = fs::read(root(base).join(rest)).ok()?;
        let ctype = if rest == SIG {
            "text/plain; charset=utf-8"
        } else {
            "application/json; charset=utf-8"
        };
        return Some(HttpResponse::bytes(200, ctype, bytes));
    }
    let name = rest.strip_prefix("components/")?;
    if !safe_file_name(name) {
        return None;
    }
    let mut scoped = req.clone();
    scoped.path = format!("/{name}");
    StaticFiles::new(components_dir(base)).try_serve_request(&scoped)
}

/// 一次刷新的结果。
#[derive(Clone, Debug, Default)]
pub struct Report {
    pub catalog_bytes: usize,
    pub downloaded: usize,
    pub skipped: usize,
    /// 本轮清理掉的旧组件文件数（不再被当前目录引用：旧版本、已关闭的 v6 等）
    pub pruned: usize,
    pub warnings: Vec<String>,
    /// 本轮完成时间（本地时间字符串；仅用于展示/落盘）
    pub checked_at: String,
}

/// 把本轮结果落盘（原子写）；失败只记日志，不影响刷新本身。
fn write_last_refresh(base: &Path, checked_at: &str, result: &Result<Report, String>) {
    let value = match result {
        Ok(r) => json!({
            "checkedAt": checked_at,
            "catalogBytes": r.catalog_bytes,
            "downloaded": r.downloaded,
            "skipped": r.skipped,
            "pruned": r.pruned,
            "warnings": r.warnings,
        }),
        Err(e) => json!({ "checkedAt": checked_at, "error": e }),
    };
    let text = value.to_string();
    if let Err(e) = write_atomic(&root(base).join(LAST_REFRESH), text.as_bytes()) {
        util::log_format("IP 库镜像：写入刷新快照失败：{}", &[&e]);
    }
}

/// 读回落盘的刷新快照（服务重启后沿用，能让面板显示上次结果）。
fn read_last_refresh(base: &Path) -> Option<Report> {
    let text = fs::read_to_string(root(base).join(LAST_REFRESH)).ok()?;
    let v: Json = serde_json::from_str(&text).ok()?;
    Some(Report {
        catalog_bytes: v.get("catalogBytes").and_then(|x| x.as_u64()).unwrap_or(0) as usize,
        downloaded: v.get("downloaded").and_then(|x| x.as_u64()).unwrap_or(0) as usize,
        skipped: v.get("skipped").and_then(|x| x.as_u64()).unwrap_or(0) as usize,
        pruned: v.get("pruned").and_then(|x| x.as_u64()).unwrap_or(0) as usize,
        warnings: v
            .get("warnings")
            .and_then(|x| x.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|i| i.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
        checked_at: v
            .get("checkedAt")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
    })
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

    let mut report = Report {
        catalog_bytes: bytes.len(),
        ..Default::default()
    };
    // 当前目录引用到的文件名（只有 ip-database 里我们要的那几只；v6 关掉时就不含 v6）
    let mut keep: std::collections::HashSet<String> = std::collections::HashSet::new();

    // 目录签名：**平台给转发什么就存什么**（镜像不重签），配了 IpdbPubKey 才强制验签。
    //
    // 关键：即使没配公钥也要把 `.sig` 一起存下来——客户端（Pek.ZIP）会用自己 TOFU 学到的
    // 平台公钥去校它；镜像不存就等于「镜像目录不可验证」，客户端只能回退平台，白折腾。
    let pubkey = cfg.ipdb_pubkey.trim();
    let sig_url = format!("{url}.sig");
    match fetch.get(&sig_url, 8192) {
        Ok(sig) => {
            if !pubkey.is_empty() {
                dhrust::plugin::verify_base64(pubkey, &bytes, &String::from_utf8_lossy(&sig))
                    .map_err(|e| format!("目录签名校验失败：{e}（核对 IpdbPubKey；目录可能被篡改）"))?;
            }
            write_atomic(&root(base).join(SIG), &sig)?;
        }
        Err(e) if !pubkey.is_empty() => {
            // 配了公钥却没有签名：宁可整轮失败（不静默放行）
            return Err(format!("拉取目录签名失败：{e}（{sig_url}）"));
        }
        Err(e) => {
            report.warnings.push(format!(
                "平台未提供目录签名，镜像将没有 .sig（配了平台公钥的客户端会回退平台）：{e}（{sig_url}）"
            ));
        }
    }
    write_atomic(&root(base).join(CATALOG), &bytes)?;

    let json: Json = serde_json::from_slice(&bytes)
        .map_err(|e| format!("catalog.json 解析失败：{e}（{url}）"))?;
    let Some(list) = json.get("components").and_then(|v| v.as_array()) else {
        return Err("catalog.json 缺少 components 数组".to_string());
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
        // 记下本轮“当前目录引用的文件名”：刷新末尾只保留它们（否则每月一发版，旧包会无限堆盘）
        keep.insert(name.clone());
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

    // 清理：只保留当前目录引用的文件（旧版本 / 已关闭的 v6 自动释放空间）
    //
    // 两道安全阀：
    // 1) `keep` 为空（本轮一个条目都没识别到）→ 绝不清理，避免目录解析异常把缓存清空；
    // 2) **「当前引用的文件没全部就位」→ 跳过清理**。实测过平台有每日下载配额（20 次/天）：
    //    配额用尽时新版本下不来，若此时还清掉旧版本，镜像就会连这个数据集一起丢掉。
    let missing = keep
        .iter()
        .filter(|n| !components_dir(base).join(n.as_str()).is_file())
        .count();
    if keep.is_empty() {
        // 什么都不删（正常不会发生：keep 至少含一个可下载条目）
    } else if missing > 0 {
        report.warnings.push(format!(
            "本轮有 {missing} 个组件未就位（下载失败/配额用尽），已跳过旧版本清理（旧包保留，客户端可继续用）"
        ));
    } else if let Ok(entries) = fs::read_dir(&components_dir(base)) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let name = path
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            if keep.contains(&name) {
                continue;
            }
            match fs::remove_file(&path) {
                Ok(()) => {
                    report.pruned += 1;
                    util::log_format("IP 库镜像：清理旧组件 {}（不再被当前目录引用）", &[&name]);
                }
                Err(e) => report
                    .warnings
                    .push(format!("清理旧组件 {name} 失败：{e}（{}）", path.display())),
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
    let report = LAST_REPORT.lock().unwrap().clone().or_else(|| read_last_refresh(base));
    let last_error = LAST_ERROR.lock().unwrap().clone();
    let last_checked = LAST_CHECKED_AT
        .lock()
        .unwrap()
        .clone()
        .or_else(|| report.as_ref().map(|r| r.checked_at.clone()));
    json!({
        "dir": dir.to_string_lossy(),
        "catalogBytes": catalog,
        "files": files,
        "totalKB": total / 1024,
        "lastError": last_error,
        "lastCheckedAt": last_checked,
        "lastWarnings": report.as_ref().map(|r| r.warnings.clone()).unwrap_or_default(),
        "lastDownloaded": report.as_ref().map(|r| r.downloaded).unwrap_or(0),
        "lastSkipped": report.as_ref().map(|r| r.skipped).unwrap_or(0),
        "lastPruned": report.as_ref().map(|r| r.pruned).unwrap_or(0),
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
        // 只读已有的 installId：状态接口不能因为一次 GET 就创建身份文件（未启用时不该在磁盘上留痕迹）
        obj.insert("installId".into(), json!(existing_install_id(panel.base())));
        // 待批准：刷新告警里带「批准」字样（平台对所有新客户端都是 pending，需人工批准）
        let pending = LAST_REPORT
            .lock()
            .unwrap()
            .as_ref()
            .map(|r| r.warnings.iter().any(|w| w.contains("批准")))
            .unwrap_or(false);
        obj.insert("pendingApproval".into(), json!(pending));
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
/// 最近一次刷新完成的时间（本地 `YYYY-MM-DD HH:MM:SS`；面板直接展示，免得运维去翻日志）。
static LAST_CHECKED_AT: Mutex<Option<String>> = Mutex::new(None);

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
        let checked_at = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
        *LAST_CHECKED_AT.lock().unwrap() = Some(checked_at.clone());
        write_last_refresh(&base, &checked_at, &result);
        match result {
            Ok(r) => {
                *LAST_ERROR.lock().unwrap() = None;
                *LAST_REPORT.lock().unwrap() = Some(r.clone());
                let (catalog, down, skip, warn, pruned) = (
                    r.catalog_bytes.to_string(),
                    r.downloaded.to_string(),
                    r.skipped.to_string(),
                    r.warnings.len().to_string(),
                    r.pruned.to_string(),
                );
                util::log_format(
                    "IP 库镜像刷新完成：目录 {} 字节，新下载 {}，复用 {}，清理 {}，告警 {}",
                    &[&catalog, &down, &skip, &pruned, &warn],
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

/// 已有的 installId（**只读**；不存在返回空串，不创建身份）。
fn existing_install_id(base: &Path) -> String {
    fs::read_to_string(auth_dir(base).join("install.id"))
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
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

        // 旧版本文件（不再被当前目录引用）应被清理；当前引用的必须保留
        let stale = components_dir(&base).join("v4-2020.01.zip");
        fs::write(&stale, b"old").unwrap();
        let report4 = refresh(&base, &cfg, &fake).expect("refresh4");
        assert_eq!(report4.pruned, 1, "旧版本应被清理");
        assert!(!stale.exists(), "旧版本文件应被删除");
        assert!(base.join("Ipdb/components/v4.zip").is_file(), "当前版本必须保留");
        assert!(base.join("Ipdb/components/v6.zip").is_file());

        // 关掉 v6 → 旧 v6 包不再被引用，下一轮应被清掉（释放 37 MiB）
        cfg.ipdb_include_v6 = false;
        let report5 = refresh(&base, &cfg, &fake).expect("refresh5");
        assert_eq!(report5.pruned, 1);
        assert!(!base.join("Ipdb/components/v6.zip").exists());
        assert!(base.join("Ipdb/components/v4.zip").is_file());

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
            ("https://p.example/components/catalog.json.sig", b"c2ln"),
            ("https://p.example/components/v4.zip", b"tampered".as_slice()),
        ]);
        let mut cfg = AgentConfig::default();
        cfg.ipdb_source_url = BASE_URL.to_string();

        let report = refresh(&base, &cfg, &fake).expect("refresh");
        assert_eq!(report.downloaded, 0);
        assert!(
            report.warnings.iter().any(|w| w.contains("SHA-256 不匹配")),
            "{:?}",
            report.warnings
        );
        // 不落盘：坏包绝不能出现在缓存里
        assert!(!base.join("Ipdb/components/v4.zip").exists());
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn prune_is_skipped_when_a_referenced_package_is_missing() {
        // 实测过：平台有每日下载配额（20 次/天）。配额用尽时新版本下不来，
        // 如果此时还把旧版本清掉，镜像就连这个数据集都没了——所以必须「引用未全部就位就不清」。
        let base = tmp_base("prune-guard");
        let pkg = b"zip".to_vec();
        let sha = dhrust::sign::sha256_hex(&pkg);
        let catalog: Vec<u8> = format!(
            r#"{{"components":[
                {{"id":"ip2region-v4","category":"ip-database","target":"any","url":"https://p.example/components/v4.zip","sha256":"{sha}"}},
                {{"id":"geolite2-city","category":"ip-database","target":"any","url":"https://p.example/components/city.zip","sha256":"{sha}"}}
            ]}}"#
        )
        .into_bytes();
        // city.zip 故意不给 → 模拟“下载失败/配额用尽”
        let fake = Fake::new(&[
            (BASE_URL, &catalog),
            ("https://p.example/components/catalog.json.sig", b"c2ln"),
            ("https://p.example/components/v4.zip", &pkg),
        ]);
        let mut cfg = AgentConfig::default();
        cfg.ipdb_source_url = BASE_URL.to_string();

        fs::create_dir_all(components_dir(&base)).unwrap();
        let stale = components_dir(&base).join("v4-2020.01.zip");
        fs::write(&stale, b"old").unwrap();

        let report = refresh(&base, &cfg, &fake).expect("refresh");
        assert_eq!(report.pruned, 0, "引用未就位时不得清理旧包");
        assert!(stale.exists(), "旧包必须保留");
        assert!(base.join("Ipdb/components/v4.zip").is_file());
        assert!(
            report.warnings.iter().any(|w| w.contains("跳过旧版本清理")),
            "{:?}",
            report.warnings
        );
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
    fn refresh_forwards_catalog_signature_even_without_pubkey() {
        // 镜像不重签：平台给什么签名就存什么，客户端（Pek.ZIP）用自己 TOFU 的公钥去校。
        // 没配 IpdbPubKey 时**也必须存**，否则客户端只能判定「镜像目录不可验证」而回退平台。
        let base = tmp_base("sig-forward");
        let catalog = br#"{"components":[]}"#.to_vec();
        let fake = Fake::new(&[(BASE_URL, &catalog), ("https://p.example/components/catalog.json.sig", b"c2ln")]);
        let mut cfg = AgentConfig::default();
        cfg.ipdb_source_url = BASE_URL.to_string();
        let report = refresh(&base, &cfg, &fake).expect("refresh");
        assert_eq!(report.warnings.len(), 0);
        assert!(base.join("Ipdb/catalog.json").is_file());
        assert!(base.join("Ipdb/catalog.json.sig").is_file(), "平台有签名时必须转发保存");
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn refresh_warns_but_keeps_going_when_signature_missing() {
        // 平台没给签名 + 没配公钥 → 只告警（客户端会自行回退平台），目录仍可用
        let base = tmp_base("sig-missing");
        let catalog = br#"{"components":[]}"#.to_vec();
        let fake = Fake::new(&[(BASE_URL, &catalog)]);
        let mut cfg = AgentConfig::default();
        cfg.ipdb_source_url = BASE_URL.to_string();
        let report = refresh(&base, &cfg, &fake).expect("refresh");
        assert_eq!(report.warnings.len(), 1);
        assert!(report.warnings[0].contains(".sig"), "{}", report.warnings[0]);
        assert!(base.join("Ipdb/catalog.json").is_file());
        assert!(!base.join("Ipdb/catalog.json.sig").exists());
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn serve_is_loopback_only_and_rejects_traversal() {
        let base = tmp_base("serve");
        fs::create_dir_all(components_dir(&base)).unwrap();
        fs::write(root(&base).join(CATALOG), b"{}").unwrap();
        fs::write(root(&base).join(SIG), b"c2ln").unwrap();
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
        // `.sig` 必须能下发（StaticFiles 不认这个扩展，所以是手工下发的；漏了会让有公钥的客户端全部回退平台）
        req.path = "/ipdb/catalog.json.sig".into();
        assert!(serve(&base, &req).is_some(), "/ipdb/catalog.json.sig 必须 200");
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
