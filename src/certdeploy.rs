//! 证书部署事务（nginx / IIS）：备份 → 写入 → 语法校验 → 生效 → 失败回滚。
//!
//! 安全约束（对齐用户「别把线上站点弄坏」的要求）：
//! 1. **只动白名单站点**（`CertManagedSites`，空 = 一律拒绝写入）；
//! 2. 默认 `dryRun`（只返回计划，不写任何文件）；
//! 3. 改前备份、改后 `nginx -t` 校验；校验或 reload 失败**自动回滚**；
//! 4. 每次只改目标 `server { }` 块内的证书指令（或显式「启用 HTTPS」时插入 443 监听），
//!    不重排配置、不删注释、不碰其它块；
//! 5. 私钥只落在 `<base>/Data/Certs/<id>/`（Unix 权限 600），权限不足时明确报错。
//!
//! IIS：用 `Import-PfxCertificate` 导入到 `LocalMachine\My`，再用 `netsh http` 绑定
//! （已有绑定用 `update sslcert`，无绑定且动作=enable_https 时先 `appcmd` 加 https 绑定）。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::AgentConfig;
use crate::sites::{self, NginxServer};

/// IIS 证书绑定的固定 appid（http.sys 要求 GUID；与站点/证书无关即可）。
#[cfg_attr(not(windows), allow(dead_code))]
const IIS_APPID: &str = "{e2b3a4c5-6d7e-4f80-9a1b-2c3d4e5f6071}";

/// 部署请求（平台 → Agent，JSON）。
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct DeployRequest {
    /// 平台证书编号（落盘目录名；只允许字母数字/下划线/短横线）
    pub cert_id: String,
    /// 证书名称（展示/日志）
    pub cert_name: String,
    /// 证书链 PEM（含站点证书与中间证书）
    pub cert_pem: String,
    /// 私钥 PEM（nginx 必需）
    pub key_pem: String,
    /// PFX（base64；IIS 必需）
    pub pfx_base64: String,
    /// PFX 密码（无密码留空）
    pub pfx_password: String,
    /// 证书域名（平台解析结果；用于匹配与校验）
    pub domains: Vec<String>,
    /// 目标站点 key（`sites` 盘点结果中的 key）
    pub sites: Vec<String>,
    /// 动作：`auto`（有证书指令则替换，否则启用 HTTPS）/ `replace` / `enable_https`
    pub action: String,
    /// 演练：只生成计划，不落盘、不改文件
    pub dry_run: bool,
}

/// 单个站点的执行结果。
#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct DeployItem {
    /// 站点 key
    pub key: String,
    /// 站点名
    pub name: String,
    /// 类型
    pub kind: String,
    /// 实际动作（`replace` / `enable_https` / `skip`）
    pub action: String,
    /// 是否成功（演练模式下表示"计划可行"）
    pub ok: bool,
    /// 变更摘要（文件、指令、绑定）
    pub changes: Vec<String>,
    /// 错误/说明
    pub message: String,
}

/// 部署结果（Agent → 平台）。
#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct DeployResult {
    /// 平台证书编号
    pub cert_id: String,
    /// 是否演练
    pub dry_run: bool,
    /// 是否全部成功（演练 = 计划全部可行）
    pub ok: bool,
    /// 逐站点结果
    pub items: Vec<DeployItem>,
    /// 汇总说明
    pub message: String,
}

/// 最近一次执行结果（面板/诊断）。
pub fn save_last_result(base: &Path, result: &DeployResult) {
    let dir = base.join("Data");
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(text) = serde_json::to_string_pretty(result) {
        let _ = std::fs::write(dir.join("CertLastResult.json"), text);
    }
}

/// 读取最近一次执行结果。
pub fn last_result(base: &Path) -> Option<serde_json::Value> {
    let path = base.join("Data").join("CertLastResult.json");
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// 平台下发的一次性拉取请求（`certFetch`：只含票据 URL 与参数，不含证书材料）。
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct FetchRequest {
    /// 平台证书编号（存储标识）
    pub cert_id: String,
    /// 证书名称
    pub cert_name: String,
    /// 拉取地址（平台签发的一次性票据 URL）
    pub url: String,
    /// 动作：`auto` / `replace` / `enable_https`
    pub action: String,
    /// 演练
    pub dry_run: bool,
    /// 平台指定的站点 key；**空 = 本地按证书域名匹配白名单站点**
    pub sites: Vec<String>,
    /// 平台已知域名（证书解析失败时的回退）
    pub domains: Vec<String>,
}

/// 主动拉取证书并在本地匹配站点后执行（或演练）。
///
/// 流程（对齐用户建议）：收到平台 `certFetch` → 用票据 URL 主动 GET → 本地解析证书域名 →
/// 与本站点清单/白名单匹配 → 只对匹配且已授权的站点生成计划/执行 → 回报结果。
pub fn fetch_and_deploy(cfg: &AgentConfig, base: &Path, req: &FetchRequest) -> DeployResult {
    let mut early = DeployResult {
        cert_id: req.cert_id.clone(),
        dry_run: req.dry_run,
        ok: false,
        items: Vec::new(),
        message: String::new(),
    };
    let material = match fetch_material(&req.url) {
        Ok(v) => v,
        Err(e) => {
            early.message = e;
            save_last_result(base, &early);
            return early;
        }
    };
    let cert_pem = material
        .get("certPem")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let target_sites = if req.sites.is_empty() {
        local_matched_sites(cfg, base, &cert_pem, &req.domains)
    } else {
        req.sites.clone()
    };
    if target_sites.is_empty() {
        early.ok = true;
        early.message = "没有匹配且已加入白名单的站点（无需部署）".to_string();
        save_last_result(base, &early);
        return early;
    }
    let mut domains: Vec<String> = material
        .get("domains")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    if domains.is_empty() {
        domains = req.domains.clone();
    }
    let inner = DeployRequest {
        cert_id: req.cert_id.clone(),
        cert_name: req.cert_name.clone(),
        cert_pem,
        key_pem: material
            .get("keyPem")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        pfx_base64: material
            .get("pfxBase64")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        pfx_password: material
            .get("pfxPassword")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        domains,
        sites: target_sites.clone(),
        action: req.action.clone(),
        dry_run: req.dry_run,
    };
    let mut result = deploy(cfg, base, &inner);
    result.message = format!(
        "{}；本地匹配站点 {} 个（{}）",
        result.message,
        target_sites.len(),
        if req.sites.is_empty() {
            "自动匹配白名单内的域名匹配站点"
        } else {
            "平台指定"
        }
    );
    result
}

/// 从平台票据 URL 拉取证书材料（JSON）。
fn fetch_material(url: &str) -> Result<serde_json::Value, String> {
    let url = url.trim();
    if !allowed_fetch_url(url) {
        return Err(format!(
            "拒绝拉取：仅允许 https 或回环 http 地址（当前：{url}）"
        ));
    }
    let resp = dhrust::net::http_client::blocking_request_offthread(
        "GET",
        url,
        &[],
        None,
        Vec::new(),
        std::time::Duration::from_secs(30),
    )
    .map_err(|e| format!("拉取证书失败：{}", e.0))?;
    if resp.status != 200 {
        let body = String::from_utf8_lossy(&resp.body);
        let detail = body.lines().next().unwrap_or("").trim().to_string();
        return Err(format!("拉取证书失败：HTTP {} {detail}", resp.status));
    }
    serde_json::from_slice(&resp.body).map_err(|e| format!("拉取证书内容无效：{e}"))
}

/// 只允许 https，或回环地址的 http（与自动升级源的校验一致）。
fn allowed_fetch_url(url: &str) -> bool {
    if url.starts_with("https://") {
        return url.len() > "https://".len();
    }
    let Some(rest) = url.strip_prefix("http://") else {
        return false;
    };
    let authority = rest.split('/').next().unwrap_or("");
    let host_port = authority.rsplit('@').next().unwrap_or(authority);
    let host = if host_port.starts_with('[') {
        host_port
            .split(']')
            .next()
            .map(|h| format!("{h}]"))
            .unwrap_or_default()
    } else {
        host_port.split(':').next().unwrap_or("").to_string()
    };
    matches!(host.as_str(), "127.0.0.1" | "localhost" | "[::1]")
}

/// 本地匹配：证书域名覆盖 + 站点在白名单内。
fn local_matched_sites(
    cfg: &AgentConfig,
    base: &Path,
    cert_pem: &str,
    fallback_domains: &[String],
) -> Vec<String> {
    let mut patterns: Vec<String> = Vec::new();
    if let Ok(bundle) = dhrust::cert::split_bundle(cert_pem) {
        for c in &bundle.certs {
            for d in c.domains() {
                if !patterns.iter().any(|p| p == &d) {
                    patterns.push(d);
                }
            }
        }
    }
    if patterns.is_empty() {
        patterns.extend(fallback_domains.iter().cloned());
    }
    if patterns.is_empty() {
        return Vec::new();
    }
    let inv = sites::inventory(cfg, base);
    inv.sites
        .into_iter()
        .filter(|s| s.managed)
        .filter(|s| {
            s.domains
                .iter()
                .any(|d| patterns.iter().any(|p| dhrust::cert::domain_matches(p, d)))
        })
        .map(|s| s.key)
        .collect()
}

/// 执行（或演练）证书部署。
pub fn deploy(cfg: &AgentConfig, base: &Path, req: &DeployRequest) -> DeployResult {
    let mut result = DeployResult {
        cert_id: req.cert_id.clone(),
        dry_run: req.dry_run,
        ok: true,
        items: Vec::new(),
        message: String::new(),
    };
    if !valid_cert_id(&req.cert_id) {
        result.ok = false;
        result.message = "证书编号非法（只允许字母、数字、下划线与短横线）".to_string();
        return result;
    }
    if req.sites.is_empty() {
        result.ok = false;
        result.message = "未指定目标站点".to_string();
        return result;
    }

    // 证书与私钥自检（平台已解析过；节点再校验一次，防止传输损坏）
    let bundle = match dhrust::cert::split_bundle(&req.cert_pem) {
        Ok(b) => b,
        Err(e) => {
            result.ok = false;
            result.message = format!("证书内容无效：{e}");
            return result;
        }
    };
    let cert_info = bundle.certs.first().cloned();

    let inventory = sites::inventory(cfg, base);
    for key in &req.sites {
        let Some(site) = inventory.sites.iter().find(|s| &s.key == key) else {
            result.items.push(DeployItem {
                key: key.clone(),
                ok: false,
                message: "站点不存在（可能配置已变更，请重新盘点）".to_string(),
                ..Default::default()
            });
            result.ok = false;
            continue;
        };
        let mut item = DeployItem {
            key: site.key.clone(),
            name: site.name.clone(),
            kind: site.kind.clone(),
            ..Default::default()
        };
        if !site.managed {
            item.ok = false;
            item.message = format!(
                "站点未加入本机证书托管白名单（CertManagedSites），拒绝写入；站点 key：{}",
                site.key
            );
            result.items.push(item);
            result.ok = false;
            continue;
        }
        // 域名覆盖校验：证书必须覆盖站点某个域名（避免把 A 站点证书装到 B 站点）
        if !site.domains.is_empty() {
            let covered = site
                .domains
                .iter()
                .any(|d| cert_info.as_ref().map(|c| c.covers_host(d)).unwrap_or(false));
            if !covered {
                item.ok = false;
                item.message = format!(
                    "证书域名（{}）不覆盖站点域名（{}）",
                    req.domains.join(", "),
                    site.domains.join(", ")
                );
                result.items.push(item);
                result.ok = false;
                continue;
            }
        }
        match site.kind.as_str() {
            "nginx" => {
                let outcome = deploy_nginx(cfg, base, req, site, &mut item);
                if !outcome {
                    result.ok = false;
                }
            }
            "iis" => {
                let outcome = deploy_iis(base, req, site, &mut item);
                if !outcome {
                    result.ok = false;
                }
            }
            other => {
                item.ok = false;
                item.message = format!("不支持的站点类型：{other}");
                result.ok = false;
            }
        }
        result.items.push(item);
    }

    if result.message.is_empty() {
        let ok = result.items.iter().filter(|i| i.ok).count();
        result.message = format!(
            "{}：{}/{} 个站点{}",
            if req.dry_run { "演练" } else { "执行" },
            ok,
            result.items.len(),
            if result.ok { "成功" } else { "（含失败项）" }
        );
    }
    // 演练也落一份结果：面板「上次执行结果」可直接看到计划与失败原因
    save_last_result(base, &result);
    result
}

fn valid_cert_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

// ———————————————— nginx ————————————————

fn deploy_nginx(
    cfg: &AgentConfig,
    base: &Path,
    req: &DeployRequest,
    site: &sites::SiteView,
    item: &mut DeployItem,
) -> bool {
    if req.key_pem.trim().is_empty() {
        item.ok = false;
        item.message = "该站点需要私钥 PEM（上传证书时未提供私钥）".to_string();
        return false;
    }
    let file = PathBuf::from(&site.file);
    let text = match std::fs::read_to_string(&file) {
        Ok(t) => t,
        Err(e) => {
            item.ok = false;
            item.message = format!("读取站点配置失败：{e}");
            return false;
        }
    };
    // 重新解析定位目标块（key 末段序号）
    let Some(index) = site
        .key
        .rsplit('#')
        .next()
        .and_then(|x| x.parse::<usize>().ok())
    else {
        item.ok = false;
        item.message = "站点标识格式不正确".to_string();
        return false;
    };
    let servers = sites::parse_nginx_servers(&text);
    let Some(srv) = servers.get(index.saturating_sub(1)) else {
        item.ok = false;
        item.message = "站点在配置文件中已不存在（请重新盘点）".to_string();
        return false;
    };

    let (cert_path, key_path) = cert_paths(base, &req.cert_id);
    let cert_path_s = cert_path.to_string_lossy().to_string();
    let key_path_s = key_path.to_string_lossy().to_string();

    let rewritten = match rewrite_nginx(text.as_str(), srv, &cert_path_s, &key_path_s, &req.action)
    {
        Ok((new_text, action)) => {
            item.action = action.clone();
            item.changes.push(format!("配置文件：{}", site.file));
            item.changes.push(format!(
                "{} ssl_certificate → {}",
                if action == "replace" {
                    "替换"
                } else {
                    "新增"
                },
                cert_path_s
            ));
            item.changes
                .push(format!("ssl_certificate_key → {key_path_s}"));
            new_text
        }
        Err(e) => {
            item.ok = false;
            item.message = e;
            return false;
        }
    };

    if req.dry_run {
        item.ok = true;
        item.message = "演练：未写入任何文件".to_string();
        return true;
    }

    // ① 证书落盘
    if let Err(e) = write_cert_files(base, &req.cert_id, &req.cert_pem, &req.key_pem) {
        item.ok = false;
        item.message = e;
        return false;
    }
    // ② 备份配置
    let backup = match backup_file(base, &file) {
        Ok(p) => p,
        Err(e) => {
            item.ok = false;
            item.message = format!("备份站点配置失败：{e}");
            return false;
        }
    };
    // ③ 原子替换配置
    if let Err(e) = dhrust::io::write_all_text_atomic(&file, &rewritten) {
        item.ok = false;
        item.message = format!("写入站点配置失败：{e}");
        return false;
    }
    // ④ nginx -t 校验（失败回滚）
    let test = run_nginx(cfg, "-t");
    if test.0 != 0 {
        let _ = std::fs::copy(&backup, &file);
        item.ok = false;
        item.message = format!(
            "nginx -t 校验失败，已回滚配置：{}",
            first_lines(&test.2, &test.1, 6)
        );
        return false;
    }
    // ⑤ reload 生效（失败回滚并再次 reload 旧配置）
    let reload = run_nginx(cfg, "-s reload");
    if reload.0 != 0 {
        let _ = std::fs::copy(&backup, &file);
        let _ = run_nginx(cfg, "-s reload");
        item.ok = false;
        item.message = format!(
            "nginx reload 失败，已回滚配置：{}",
            first_lines(&reload.2, &reload.1, 6)
        );
        return false;
    }
    item.ok = true;
    item.message = "已写入并 reload 生效（备份：<base>/Data/CertBackups）".to_string();
    true
}

/// 纯函数：改写 nginx 配置文本（只动目标块的证书指令）。
///
/// 返回 `(新文本, 实际动作)`；动作 = `replace`（已有证书指令，替换路径）或
/// `enable_https`（无证书指令，插入 443 监听与证书指令）。
pub fn rewrite_nginx(
    text: &str,
    srv: &NginxServer,
    cert_path: &str,
    key_path: &str,
    action: &str,
) -> Result<(String, String), String> {
    let action = action.trim().to_ascii_lowercase();
    let cert_q = quote_nginx(cert_path);
    let key_q = quote_nginx(key_path);
    let has_cert = srv.cert.is_some();
    let has_key = srv.key.is_some();
    let replace_only = has_cert || has_key;
    if action == "replace" && !replace_only {
        return Err("站点未配置证书指令；请把动作改为「启用 HTTPS」".to_string());
    }
    if replace_only {
        // 深拷贝到可写，按偏移从后往前替换（避免偏移失效）
        let mut out = text.to_string();
        let mut ranges: Vec<(usize, usize, String)> = Vec::new();
        if let Some(c) = &srv.cert {
            ranges.push((c.start, c.end, cert_q.clone()));
        }
        if let Some(k) = &srv.key {
            ranges.push((k.start, k.end, key_q.clone()));
        }
        if !has_key {
            // 有证书、无私钥：在证书指令同一位置后插入私钥指令
            if let Some(c) = &srv.cert {
                ranges.push((
                    c.end,
                    c.end,
                    format!(";\n{}ssl_certificate_key {}", indent_in_block(text, srv, 4), key_q),
                ));
            }
        }
        ranges.sort_by_key(|(s, _, _)| std::cmp::Reverse(*s));
        for (start, end, value) in ranges {
            if out.len() < end || start > end {
                return Err("配置解析偏移失效（文件可能刚被修改）".to_string());
            }
            out.replace_range(start..end, &value);
        }
        let _ = has_key;
        return Ok((out, "replace".to_string()));
    }

    // 无证书指令：启用 HTTPS（插入 listen 443 ssl + 证书指令）
    if action != "enable_https" && action != "auto" {
        return Err(format!("未知动作：{action}"));
    }
    let indent = indent_in_block(text, srv, 4);
    let mut insert = String::new();
    insert.push('\n');
    insert.push_str(&format!("{indent}# >>> Pek 证书托管（自动生成，勿手改） >>>\n"));
    if !srv
        .listens
        .iter()
        .any(|l| l.split_whitespace().any(|p| p == "443"))
    {
        insert.push_str(&format!("{indent}listen 443 ssl;\n"));
    }
    insert.push_str(&format!("{indent}ssl_certificate {cert_q};\n"));
    insert.push_str(&format!("{indent}ssl_certificate_key {key_q};\n"));
    insert.push_str(&format!("{indent}# <<< Pek 证书托管 <<<\n"));

    let at = srv.open_brace + 1;
    if at > text.len() {
        return Err("配置解析偏移失效（文件可能刚被修改）".to_string());
    }
    let mut out = text.to_string();
    out.insert_str(at, &insert);
    Ok((out, "enable_https".to_string()))
}

/// 推断块内指令缩进（取块内第一行非空行的前导空白；没有则用 `fallback` 空格）。
fn indent_in_block(text: &str, srv: &NginxServer, fallback: usize) -> String {
    let inner = &text[srv.open_brace + 1..srv.close_brace.min(text.len())];
    for line in inner.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let indent: String = line.chars().take_while(|c| c.is_whitespace()).collect();
        if !indent.is_empty() {
            return indent;
        }
        break;
    }
    " ".repeat(fallback)
}

fn quote_nginx(path: &str) -> String {
    if path.contains(char::is_whitespace) || path.contains('"') {
        format!("\"{}\"", path.replace('"', "\\\""))
    } else {
        path.to_string()
    }
}

fn run_nginx(cfg: &AgentConfig, args: &str) -> (i32, String, String) {
    let bin = if cfg.cert_nginx_bin.trim().is_empty() {
        "nginx".to_string()
    } else {
        cfg.cert_nginx_bin.trim().to_string()
    };
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let r = dhrust::sys::process::run_shell(&format!("{bin} {args}"), &cwd, 60);
    (r.exit_code, r.output, r.error)
}

fn first_lines(a: &str, b: &str, max: usize) -> String {
    let text = if a.trim().is_empty() { b } else { a };
    let lines: Vec<&str> = text
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .take(max)
        .collect();
    if lines.is_empty() {
        "（无输出）".to_string()
    } else {
        lines.join(" / ")
    }
}

// ———————————————— IIS ————————————————

#[cfg(windows)]
fn deploy_iis(
    base: &Path,
    req: &DeployRequest,
    site: &sites::SiteView,
    item: &mut DeployItem,
) -> bool {
    if req.pfx_base64.trim().is_empty() {
        item.ok = false;
        item.message = "IIS 部署需要 PFX（证书内容里没有 PFX，请以 PFX 上传或同时提供 PFX）".to_string();
        return false;
    }
    let Some(info) = dhrust::cert::parse_certificate_pem(&req.cert_pem) else {
        item.ok = false;
        item.message = "证书解析失败".to_string();
        return false;
    };
    let thumb = info.sha1_hex.to_ascii_uppercase();
    let hosts: Vec<String> = if site.domains.is_empty() {
        req.domains.clone()
    } else {
        site.domains.clone()
    };
    if hosts.is_empty() {
        item.ok = false;
        item.message = "站点没有可用于绑定的域名（IIS 绑定为 IP/通配，需手工配置）".to_string();
        return false;
    }
    item.action = if site.ssl {
        "replace".to_string()
    } else {
        "enable_https".to_string()
    };
    for h in &hosts {
        item.changes.push(format!("证书绑定 → {h}:443（SHA-1 {thumb}）"));
    }
    if req.dry_run {
        item.ok = true;
        item.message = "演练：未导入证书、未改绑定".to_string();
        return true;
    }

    // ① PFX 落盘
    let pfx = match base.join("Data").join("Certs").join(&req.cert_id).join("site.pfx").into_os_string().into_string() {
        Ok(p) => p,
        Err(_) => {
            item.ok = false;
            item.message = "证书目录路径包含非法字符".to_string();
            return false;
        }
    };
    if let Err(e) = write_pfx(base, &req.cert_id, &req.pfx_base64) {
        item.ok = false;
        item.message = e;
        return false;
    }
    // ② 导入 PFX（LocalMachine\My，私钥不可导出）
    let mut cmd = format!(
        "Import-PfxCertificate -FilePath '{pfx}' -CertStoreLocation 'Cert:\\LocalMachine\\My'"
    );
    if !req.pfx_password.is_empty() {
        cmd.push_str(&format!(
            " -Password (ConvertTo-SecureString -String '{}' -AsPlainText -Force)",
            req.pfx_password.replace('\'', "''")
        ));
    }
    cmd.push_str(" | Out-Null");
    let r = dhrust::sys::process::run_shell(
        &format!("powershell -NoProfile -ExecutionPolicy Bypass -Command \"{cmd}\""),
        base,
        120,
    );
    if r.exit_code != 0 {
        item.ok = false;
        item.message = format!(
            "导入 PFX 失败：{}",
            first_lines(&r.error, &r.output, 6)
        );
        return false;
    }
    // ③ 逐域名绑定（已有 https 绑定 → update；否则 enable_https 时先加绑定）
    let mut done: Vec<String> = Vec::new();
    for h in &hosts {
        let hostport = format!("{h}:443");
        let update = dhrust::sys::process::run_shell(
            &format!(
                "netsh http update sslcert hostnameport={hostport} certhash={thumb} appid={IIS_APPID} certstorename=MY"
            ),
            base,
            60,
        );
        if update.exit_code == 0 {
            done.push(hostport.clone());
            continue;
        }
        if item.action != "enable_https" {
            // replace 但绑定不存在：不擅自新增站点绑定，按失败返回（避免意外改变站点结构）
            item.ok = false;
            item.message = format!(
                "更新证书绑定失败（{hostport}）：{}",
                first_lines(&update.error, &update.output, 4)
            );
            return false;
        }
        // 新增 HTTPS 绑定（*:443:host）
        let add_binding = dhrust::sys::process::run_shell(
            &format!(
                "appcmd set site /site.name:\"{}\" /+bindings.[protocol='https',bindingInformation='*:443:{h}']",
                site.name.replace('"', "\\\"")
            ),
            base,
            60,
        );
        if add_binding.exit_code != 0 {
            item.ok = false;
            item.message = format!(
                "新增 HTTPS 绑定失败（{hostport}）：{}",
                first_lines(&add_binding.error, &add_binding.output, 4)
            );
            return false;
        }
        let add = dhrust::sys::process::run_shell(
            &format!(
                "netsh http add sslcert hostnameport={hostport} certhash={thumb} appid={IIS_APPID} certstorename=MY"
            ),
            base,
            60,
        );
        if add.exit_code != 0 {
            let _ = dhrust::sys::process::run_shell(
                &format!(
                    "appcmd set site /site.name:\"{}\" /-bindings.[protocol='https',bindingInformation='*:443:{h}']",
                    site.name.replace('"', "\\\"")
                ),
                base,
                60,
            );
            item.ok = false;
            item.message = format!(
                "绑定证书失败（{hostport}）：{}",
                first_lines(&add.error, &add.output, 4)
            );
            return false;
        }
        done.push(hostport.clone());
    }
    item.ok = true;
    item.message = format!("已导入证书并绑定：{}", done.join(", "));
    true
}

#[cfg(not(windows))]
fn deploy_iis(
    _base: &Path,
    _req: &DeployRequest,
    _site: &sites::SiteView,
    item: &mut DeployItem,
) -> bool {
    item.ok = false;
    item.message = "IIS 部署仅支持 Windows 节点".to_string();
    false
}

// ———————————————— 文件工具 ————————————————

fn cert_paths(base: &Path, cert_id: &str) -> (PathBuf, PathBuf) {
    let dir = base.join("Data").join("Certs").join(cert_id);
    (dir.join("fullchain.pem"), dir.join("privkey.pem"))
}

fn write_cert_files(base: &Path, cert_id: &str, cert_pem: &str, key_pem: &str) -> Result<(), String> {
    let dir = base.join("Data").join("Certs").join(cert_id);
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建证书目录失败：{e}"))?;
    let (cert, key) = cert_paths(base, cert_id);
    dhrust::io::write_all_text_atomic(&cert, cert_pem)
        .map_err(|e| format!("写入证书文件失败：{e}"))?;
    if !key_pem.trim().is_empty() {
        dhrust::io::write_all_text_atomic(&key, key_pem)
            .map_err(|e| format!("写入私钥文件失败：{e}"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600));
        }
    }
    Ok(())
}

#[cfg(windows)]
fn write_pfx(base: &Path, cert_id: &str, pfx_base64: &str) -> Result<(), String> {
    let dir = base.join("Data").join("Certs").join(cert_id);
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建证书目录失败：{e}"))?;
    let bytes = dhrust::sign::base64_decode(pfx_base64.trim())
        .ok_or_else(|| "PFX base64 解码失败".to_string())?;
    let tmp = dir.join("site.pfx.tmp");
    std::fs::write(&tmp, &bytes).map_err(|e| format!("写入 PFX 临时文件失败：{e}"))?;
    dhrust::io::safe_replace_file(&tmp, &dir.join("site.pfx"))
        .map_err(|e| format!("写入 PFX 失败：{e}"))?;
    Ok(())
}

#[cfg(not(windows))]
#[allow(dead_code)]
fn write_pfx(_base: &Path, _cert_id: &str, _pfx_base64: &str) -> Result<(), String> {
    Err("IIS 部署仅支持 Windows 节点".to_string())
}

fn backup_file(base: &Path, file: &Path) -> Result<PathBuf, String> {
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
    let dir = base.join("Data").join("CertBackups").join(stamp);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let name = file
        .file_name()
        .map(|x| x.to_string_lossy().to_string())
        .unwrap_or_else(|| "site.conf".to_string());
    let dest = dir.join(format!("{name}.bak"));
    std::fs::copy(file, &dest).map_err(|e| e.to_string())?;
    Ok(dest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrite_replaces_existing_certificate_paths() {
        let text = "server {\n    listen 443 ssl;\n    server_name a.com;\n    ssl_certificate /old/c.pem;\n    ssl_certificate_key /old/k.pem;\n}\n";
        let srv = &sites::parse_nginx_servers(text)[0];
        let (out, action) =
            rewrite_nginx(text, srv, "/new/fullchain.pem", "/new/privkey.pem", "auto").unwrap();
        assert_eq!(action, "replace");
        assert!(out.contains("ssl_certificate /new/fullchain.pem;"));
        assert!(out.contains("ssl_certificate_key /new/privkey.pem;"));
        assert!(!out.contains("/old/"));
        assert!(out.contains("listen 443 ssl;"), "不应重复插入 listen");
        assert_eq!(out.matches("ssl_certificate ").count(), 1);
    }

    #[test]
    fn rewrite_enables_https_for_plain_site() {
        let text = "server {\n    listen 80;\n    server_name a.com;\n    root /www/a;\n}\n";
        let srv = &sites::parse_nginx_servers(text)[0];
        let (out, action) = rewrite_nginx(
            text,
            srv,
            "/new/fullchain.pem",
            "/new/privkey.pem",
            "enable_https",
        )
        .unwrap();
        assert_eq!(action, "enable_https");
        assert!(out.contains("listen 443 ssl;"));
        assert!(out.contains("ssl_certificate /new/fullchain.pem;"));
        assert!(out.contains("ssl_certificate_key /new/privkey.pem;"));
        assert!(out.contains("Pek 证书托管"), "应有可识别的托管标记");
        assert!(out.contains("root /www/a;"), "原站配置不能丢");
        // 插入位置在 server 块内
        let first_close = out.find('}').unwrap();
        assert!(out.find("ssl_certificate").unwrap() < first_close);
    }

    #[test]
    fn rewrite_refuses_replace_on_plain_site() {
        let text = "server {\n    listen 80;\n    server_name a.com;\n}\n";
        let srv = &sites::parse_nginx_servers(text)[0];
        let err = rewrite_nginx(text, srv, "/c.pem", "/k.pem", "replace").unwrap_err();
        assert!(err.contains("启用 HTTPS"), "{err}");
    }

    #[test]
    fn rewrite_does_not_touch_other_blocks() {
        let text = "server {\n    listen 443 ssl;\n    server_name a.com;\n    ssl_certificate /a.pem;\n    ssl_certificate_key /a.key;\n}\nserver {\n    listen 443 ssl;\n    server_name b.com;\n    ssl_certificate /b.pem;\n    ssl_certificate_key /b.key;\n}\n";
        let list = sites::parse_nginx_servers(text);
        let (out, _) = rewrite_nginx(text, &list[1], "/new/c.pem", "/new/k.pem", "auto").unwrap();
        assert!(out.contains("ssl_certificate /a.pem;"), "第一个块不能变");
        assert!(out.contains("ssl_certificate /new/c.pem;"));
        assert!(out.contains("ssl_certificate_key /new/k.pem;"));
    }

    #[test]
    fn quotes_paths_with_spaces() {
        let text = "server {\n    listen 80;\n    server_name a.com;\n}\n";
        let srv = &sites::parse_nginx_servers(text)[0];
        let (out, _) = rewrite_nginx(text, srv, "/data/my certs/c.pem", "/data/k.pem", "auto")
            .unwrap();
        assert!(out.contains("ssl_certificate \"/data/my certs/c.pem\";"));
    }

    #[test]
    fn cert_id_validation() {
        assert!(valid_cert_id("abc-123_X"));
        assert!(!valid_cert_id(""));
        assert!(!valid_cert_id("../etc"));
        assert!(!valid_cert_id("a/b"));
        assert!(!valid_cert_id(&"a".repeat(65)));
    }
}
