//! 站点清单（nginx / IIS）：域名、监听、现有证书、平台托管白名单。
//!
//! 供 Pek.RPanlServer「证书」页盘点与下发匹配使用；**全部为只读操作**（读取配置与
//! 证书文件，不执行任何命令、不修改任何文件）。真正的部署在 [`crate::certdeploy`]，
//! 且只允许白名单内的站点（`CertManagedSites`）。
//!
//! nginx 解析保留每个 `server { }` 块的**字符区间**与 `ssl_certificate(_key)` 指令的
//! 值区间，供部署时精确改写（不重排整份配置、不丢注释）。

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::config::AgentConfig;

/// nginx 站点配置常见目录（与 `weblog` 自动发现同源，另补宝塔/lnmp 变体）。
pub const NGINX_DIRS: &[&str] = &[
    "/www/server/panel/vhost/nginx", // 宝塔
    "/www/server/nginx/conf/vhost",  // 宝塔（老版本）
    "/etc/nginx/sites-enabled",      // Debian/Ubuntu
    "/etc/nginx/conf.d",             // RHEL/CentOS
    "/etc/nginx/vhosts.d",           // openSUSE
    "/usr/local/nginx/conf/vhost",   // lnmp/oneinstack
    "/usr/local/nginx/conf/conf.d",
    "/etc/nginx/nginx.conf",
];

/// 证书视图（脱敏：绝不下发/展示私钥内容）。
#[derive(Debug, Clone, Serialize, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CertView {
    /// Subject CN
    pub subject: String,
    /// 证书声明的域名（SAN 优先）
    pub domains: Vec<String>,
    /// 生效时间（Unix 秒）
    pub not_before: i64,
    /// 到期时间（Unix 秒）
    pub not_after: i64,
    /// 距到期天数（负数 = 已过期）
    pub days_left: i64,
    /// 是否已过期
    pub expired: bool,
    /// SHA-1 指纹（小写十六进制；IIS 绑定时用）
    pub sha1: String,
    /// 是否 CA 证书（误部署会被客户端拒绝）
    pub is_ca: bool,
}

impl CertView {
    fn from_info(info: &dhrust::cert::CertInfo, now: i64) -> Self {
        Self {
            subject: info.subject_cn.clone(),
            domains: info.domains(),
            not_before: info.not_before,
            not_after: info.not_after,
            days_left: info.days_left(now),
            expired: info.expired(now),
            sha1: info.sha1_hex.clone(),
            is_ca: info.is_ca,
        }
    }
}

/// 单个站点（nginx 的一个 `server { }` 块，或一个 IIS 站点）。
#[derive(Debug, Clone, Serialize, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SiteView {
    /// 稳定标识（部署时按它定位；nginx = `nginx:<配置文件>#<序号>`，IIS = `iis:<站点名>`）
    pub key: String,
    /// 类型：`nginx` / `iis`
    pub kind: String,
    /// 展示名
    pub name: String,
    /// 站点域名（nginx = server_name；IIS = https/http 绑定的主机部分）
    pub domains: Vec<String>,
    /// 是否已启用 HTTPS（nginx = listen 带 ssl 或存在 ssl_certificate；IIS = 有 https 绑定）
    pub ssl: bool,
    /// 证书文件路径（nginx）
    pub cert_path: String,
    /// 私钥文件路径（nginx）
    pub key_path: String,
    /// 现有证书元数据（能读取时）
    pub cert: Option<CertView>,
    /// 是否在平台托管白名单（`CertManagedSites`）内
    pub managed: bool,
    /// 命中的白名单规则原文（空 = 未命中）——面板用它解释“为什么已托管”，
    /// 例如 `*.example.com`（通配符命中）或站点 key（精确命中）。
    pub managed_by: String,
    /// 系统/占位站点（无域名或仅回环）：如宝塔 `0.default.conf`、`phpfpm_status.conf`，
    /// 永远无法匹配证书，默认不展示、不上报心跳（避免噪音）。
    pub system: bool,
    /// 配置文件路径（nginx）
    pub file: String,
    /// 备注（无法读取证书的原因等）
    pub note: String,
}

/// 盘点结果。
#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SiteInventory {
    /// 发现的 Web 服务器类型（`nginx` / `iis`）
    pub web: Vec<String>,
    /// 站点列表
    pub sites: Vec<SiteView>,
}

impl SiteInventory {
    /// 心跳/平台列表用的精简摘要（只含域名与 ssl 状态，控制报文大小）。
    pub fn summary(&self) -> serde_json::Value {
        let sites: Vec<serde_json::Value> = self
            .sites
            .iter()
            .filter(|s| !s.system)
            .map(|s| {
                serde_json::json!({
                    "key": s.key,
                    "kind": s.kind,
                    "name": s.name,
                    "domains": s.domains,
                    "ssl": s.ssl,
                    "managed": s.managed,
                })
            })
            .collect();
        serde_json::json!({ "web": self.web, "sites": sites })
    }
}

/// 系统/占位站点判定：没有域名，或域名全是回环（宝塔 `0.default.conf`、
/// `phpfpm_status.conf`、IIS 无绑定的默认站点等）——永远匹配不到证书，属于噪音。
pub fn is_system_site(domains: &[String]) -> bool {
    if domains.is_empty() {
        return true;
    }
    domains.iter().all(|d| {
        let d = d.trim().to_ascii_lowercase();
        d.is_empty()
            || d == "_"
            || d == "localhost"
            || d == "127.0.0.1"
            || d == "::1"
            || d == "[::1]"
    })
}

/// 盘点本机站点（只读；`base` 用于解析相对证书路径与备份目录）。
pub fn inventory(cfg: &AgentConfig, base: &Path) -> SiteInventory {
    let mut web = Vec::new();
    let mut sites = Vec::new();
    let now = chrono::Utc::now().timestamp();

    let nginx = nginx_sites(cfg, base, now);
    if !nginx.is_empty() || !nginx_config_files(cfg).is_empty() {
        web.push("nginx".to_string());
        sites.extend(nginx);
    }

    let iis = iis_sites(cfg);
    if !iis.is_empty() {
        web.push("iis".to_string());
        sites.extend(iis);
    }

    SiteInventory { web, sites }
}

// ———————————————— 盘点缓存（心跳用） ————————————————

/// 缓存兜底时长：配置一直不变时也不会无限期复用（证书到期天数等随时间变化）。
const CACHE_TTL: Duration = Duration::from_secs(300);

struct Cached {
    sig: String,
    at: Instant,
    inv: SiteInventory,
}

fn cache() -> &'static Mutex<Option<Cached>> {
    static C: OnceLock<Mutex<Option<Cached>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(None))
}

/// 盘点签名：只看文件元数据（长度 + mtime，不读内容）+ 白名单与目录配置。
///
/// 只要站点配置、白名单、nginx 目录任一变化，签名就变 → 心跳立即重新盘点；
/// 完全没变时心跳只做几十次 `metadata()`（亚毫秒级），不再每 60 秒全量读盘解析证书。
fn inventory_signature(cfg: &AgentConfig) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    cfg.cert_managed_sites.hash(&mut h);
    cfg.cert_nginx_dirs.hash(&mut h);
    cfg.cert_nginx_bin.hash(&mut h);
    let stamp = |p: &Path, h: &mut std::collections::hash_map::DefaultHasher| {
        p.to_string_lossy().hash(h);
        if let Ok(m) = std::fs::metadata(p) {
            m.len().hash(h);
            if let Ok(t) = m.modified()
                && let Ok(d) = t.duration_since(std::time::UNIX_EPOCH)
            {
                d.as_secs().hash(h);
                d.subsec_nanos().hash(h);
            }
        }
    };
    for p in nginx_config_paths(cfg) {
        stamp(&p, &mut h);
    }
    #[cfg(windows)]
    for p in iis_config_paths() {
        stamp(&p, &mut h);
    }
    format!("{:x}", h.finish())
}

/// IIS 主配置路径（站点变更会改这两个文件的 mtime）。
#[cfg(windows)]
fn iis_config_paths() -> Vec<PathBuf> {
    let root = std::env::var_os("windir")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
    let dir = root.join("System32").join("inetsrv").join("config");
    vec![
        dir.join("applicationHost.config"),
        dir.join("redirection.config"),
    ]
}

/// 带缓存的盘点（**心跳专用**）：配置与白名单都没变则复用上次结果。
///
/// 返回 `(盘点结果, 是否重新盘点)`——第二个值用于心跳**只在变化时携带站点摘要**，
/// 避免每 60 秒重复上传几 KB 的站点清单（省带宽；平台侧保留上一次的值）。
/// 面板 `/star/sites`、平台 `siteScan` 仍走实时 `inventory()`。
pub fn inventory_cached(cfg: &AgentConfig, base: &Path) -> (SiteInventory, bool) {
    let sig = inventory_signature(cfg);
    {
        let guard = cache().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(c) = guard.as_ref()
            && c.sig == sig
            && c.at.elapsed() < CACHE_TTL
        {
            return (c.inv.clone(), false);
        }
    }
    let inv = inventory(cfg, base);
    let mut guard = cache().lock().unwrap_or_else(|e| e.into_inner());
    *guard = Some(Cached {
        sig,
        at: Instant::now(),
        inv: inv.clone(),
    });
    (inv, true)
}

/// 清空盘点缓存（白名单/目录等配置变更后调用，保证下一次心跳立即反映）。
pub fn invalidate_cache() {
    let mut guard = cache().lock().unwrap_or_else(|e| e.into_inner());
    *guard = None;
}

/// 心跳用的站点摘要：`(摘要, 是否需要上报)`。
///
/// `None` 表示本轮配置没有变化，心跳不必携带 `sites` 字段（平台保留旧值）。
pub fn summary_cached(cfg: &AgentConfig, base: &Path) -> (serde_json::Value, bool) {
    let (inv, fresh) = inventory_cached(cfg, base);
    (inv.summary(), fresh)
}

// ———————————————— nginx ————————————————

/// 部署用的 nginx `server` 块解析结果（含精确字符区间）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NginxServer {
    /// `server_name` 参数（原样，过滤掉 `_`/`$`/正则）
    pub server_names: Vec<String>,
    /// `listen` 指令（原始参数串）
    pub listens: Vec<String>,
    /// 是否启用 SSL（listen 带 ssl，或存在 ssl_certificate）
    pub ssl: bool,
    /// `ssl_certificate` 指令
    pub cert: Option<NginxDirective>,
    /// `ssl_certificate_key` 指令
    pub key: Option<NginxDirective>,
    /// 块开括号 `{` 的字节偏移
    pub open_brace: usize,
    /// 块闭括号 `}` 的字节偏移
    pub close_brace: usize,
}

/// 一条指令（值 + 值在文件中的字节区间，用于就地替换）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NginxDirective {
    /// 参数值（去引号）
    pub value: String,
    /// 值起始字节
    pub start: usize,
    /// 值结束字节（不含）
    pub end: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TokKind {
    Word,
    Semi,
    Open,
    Close,
}

#[derive(Debug, Clone)]
struct Tok {
    kind: TokKind,
    text: String,
    start: usize,
    end: usize,
}

/// nginx 配置分词：识别 `;` `{` `}` 与带引号的参数；`#` 到行尾为注释。
fn tokenize(text: &str) -> Vec<Tok> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'#' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if b.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        match b {
            b';' => {
                out.push(Tok {
                    kind: TokKind::Semi,
                    text: ";".to_string(),
                    start: i,
                    end: i + 1,
                });
                i += 1;
            }
            b'{' => {
                out.push(Tok {
                    kind: TokKind::Open,
                    text: "{".to_string(),
                    start: i,
                    end: i + 1,
                });
                i += 1;
            }
            b'}' => {
                out.push(Tok {
                    kind: TokKind::Close,
                    text: "}".to_string(),
                    start: i,
                    end: i + 1,
                });
                i += 1;
            }
            b'"' | b'\'' => {
                let quote = b;
                let start = i;
                i += 1;
                let inner_start = i;
                while i < bytes.len() && bytes[i] != quote {
                    i += 1;
                }
                let inner = text[inner_start..i].to_string();
                if i < bytes.len() {
                    i += 1; // 跳过闭合引号
                }
                out.push(Tok {
                    kind: TokKind::Word,
                    text: inner,
                    start,
                    end: i,
                });
            }
            _ => {
                let start = i;
                while i < bytes.len() {
                    let c = bytes[i];
                    if c.is_ascii_whitespace() || c == b';' || c == b'{' || c == b'}' || c == b'#' {
                        break;
                    }
                    i += 1;
                }
                out.push(Tok {
                    kind: TokKind::Word,
                    text: text[start..i].to_string(),
                    start,
                    end: i,
                });
            }
        }
    }
    out
}

/// 解析 nginx 配置文本中的全部 `server { }` 块（纯函数，可单测）。
pub fn parse_nginx_servers(text: &str) -> Vec<NginxServer> {
    let toks = tokenize(text);
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < toks.len() {
        let is_server = toks[i].kind == TokKind::Word
            && toks[i].text.eq_ignore_ascii_case("server")
            && toks.get(i + 1).map(|t| t.kind) == Some(TokKind::Open);
        if !is_server {
            i += 1;
            continue;
        }
        let open = &toks[i + 1];
        // 找匹配的闭括号（按 token 深度）
        let mut depth = 0i32;
        let mut j = i + 1;
        let mut close: Option<&Tok> = None;
        while j < toks.len() {
            match toks[j].kind {
                TokKind::Open => depth += 1,
                TokKind::Close => {
                    depth -= 1;
                    if depth == 0 {
                        close = Some(&toks[j]);
                        break;
                    }
                }
                _ => {}
            }
            j += 1;
        }
        let Some(close) = close else {
            break; // 配置不完整（括号未闭合），停止解析
        };

        let mut srv = NginxServer {
            open_brace: open.start,
            close_brace: close.start,
            ..Default::default()
        };
        let mut k = i + 2; // open 之后
        let mut d = 1i32;
        while k < j {
            match toks[k].kind {
                TokKind::Open => d += 1,
                TokKind::Close => d -= 1,
                TokKind::Word if d == 1 => {
                    let name = toks[k].text.to_ascii_lowercase();
                    let mut args: Vec<&Tok> = Vec::new();
                    let mut m = k + 1;
                    while m < toks.len()
                        && !matches!(
                            toks[m].kind,
                            TokKind::Semi | TokKind::Open | TokKind::Close
                        )
                    {
                        if toks[m].kind == TokKind::Word {
                            args.push(&toks[m]);
                        }
                        m += 1;
                    }
                    match name.as_str() {
                        "server_name" => {
                            for a in &args {
                                let v = a.text.trim();
                                if !v.is_empty()
                                    && v != "_"
                                    && !v.starts_with('$')
                                    && !v.starts_with('~')
                                {
                                    srv.server_names.push(v.to_string());
                                }
                            }
                        }
                        "listen" => {
                            let joined = args
                                .iter()
                                .map(|a| a.text.as_str())
                                .collect::<Vec<_>>()
                                .join(" ");
                            if args.iter().any(|a| a.text.eq_ignore_ascii_case("ssl")) {
                                srv.ssl = true;
                            }
                            srv.listens.push(joined);
                        }
                        "ssl_certificate" => {
                            if let Some(a) = args.first() {
                                srv.cert = Some(NginxDirective {
                                    value: a.text.clone(),
                                    start: a.start,
                                    end: a.end,
                                });
                                srv.ssl = true;
                            }
                        }
                        "ssl_certificate_key" => {
                            if let Some(a) = args.first() {
                                srv.key = Some(NginxDirective {
                                    value: a.text.clone(),
                                    start: a.start,
                                    end: a.end,
                                });
                                srv.ssl = true;
                            }
                        }
                        _ => {}
                    }
                    k = m; // 停在分隔符（分号）上
                    continue;
                }
                _ => {}
            }
            k += 1;
        }
        out.push(srv);
        i = j + 1;
    }
    out
}

/// 收集本机 nginx 站点配置路径（去重、不读内容；缓存签名用它）。
pub fn nginx_config_paths(cfg: &AgentConfig) -> Vec<PathBuf> {
    let mut dirs: Vec<String> = NGINX_DIRS.iter().map(|s| s.to_string()).collect();
    for raw in cfg
        .cert_nginx_dirs
        .split([',', ';', '\n'])
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        dirs.push(raw.to_string());
    }
    let mut files: Vec<PathBuf> = Vec::new();
    for d in dirs {
        let p = PathBuf::from(&d);
        if p.is_file() {
            files.push(p);
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&p) else {
            continue;
        };
        for e in entries.flatten() {
            let path = e.path();
            if path.is_file() {
                let ext = path
                    .extension()
                    .map(|x| x.to_string_lossy().to_ascii_lowercase())
                    .unwrap_or_default();
                if ext == "conf" || ext.is_empty() {
                    files.push(path);
                }
            } else if path.is_dir() {
                // 一层子目录（`conf.d/sub/*.conf` 常见）
                if let Ok(sub) = std::fs::read_dir(&path) {
                    for se in sub.flatten() {
                        let sp = se.path();
                        if sp.is_file()
                            && sp
                                .extension()
                                .map(|x| x.to_string_lossy().eq_ignore_ascii_case("conf"))
                                .unwrap_or(false)
                        {
                            files.push(sp);
                        }
                    }
                }
            }
        }
    }
    files.sort();
    files.dedup();
    files
}

/// 收集本机 nginx 站点配置（去重、限大小；`CertNginxDirs` 追加自定义目录）。
pub fn nginx_config_files(cfg: &AgentConfig) -> Vec<(PathBuf, String)> {
    nginx_config_paths(cfg)
        .into_iter()
        .filter_map(|p| {
            let meta = std::fs::metadata(&p).ok()?;
            if meta.len() > 2 * 1024 * 1024 {
                return None;
            }
            let text = std::fs::read_to_string(&p).ok()?;
            Some((p, text))
        })
        .collect()
}

fn nginx_sites(cfg: &AgentConfig, base: &Path, now: i64) -> Vec<SiteView> {
    let mut out = Vec::new();
    for (file, text) in nginx_config_files(cfg) {
        let file_text = file.to_string_lossy().to_string();
        for (idx, srv) in parse_nginx_servers(&text).into_iter().enumerate() {
            let key = format!("nginx:{file_text}#{}", idx + 1);
            let domains = srv.server_names.clone();
            let name = domains
                .first()
                .cloned()
                .unwrap_or_else(|| file_text.clone());
            let (cert_path, key_path) = (
                srv.cert.as_ref().map(|c| c.value.clone()).unwrap_or_default(),
                srv.key.as_ref().map(|k| k.value.clone()).unwrap_or_default(),
            );
            let (cert, note) = match srv.cert.as_ref() {
                Some(d) => read_cert_view(&d.value, base, now),
                None => (None, String::new()),
            };
            let managed_by = managed_rule(&cfg.cert_managed_sites, &key, &file_text, &domains);
            let managed = managed_by.is_some();
            let system = is_system_site(&domains);
            out.push(SiteView {
                key,
                kind: "nginx".to_string(),
                name,
                domains,
                ssl: srv.ssl,
                cert_path,
                key_path,
                cert,
                managed,
                managed_by: managed_by.unwrap_or_default(),
                system,
                file: file_text.clone(),
                note,
            });
        }
    }
    out
}

fn read_cert_view(path: &str, base: &Path, now: i64) -> (Option<CertView>, String) {
    if path.is_empty() {
        return (None, String::new());
    }
    let p = PathBuf::from(path);
    let p = if p.is_absolute() { p } else { base.join(p) };
    let meta = match std::fs::metadata(&p) {
        Ok(m) => m,
        Err(e) => return (None, format!("证书文件不可读：{e}")),
    };
    if meta.len() > 1024 * 1024 {
        return (None, "证书文件过大（>1MB）".to_string());
    }
    let Ok(text) = std::fs::read_to_string(&p) else {
        return (None, "证书文件读取失败".to_string());
    };
    match dhrust::cert::parse_certificate_pem(&text) {
        Some(info) => (Some(CertView::from_info(&info, now)), String::new()),
        None => (None, "证书文件不是有效 PEM".to_string()),
    }
}

/// 返回命中的白名单规则原文（未命中返回 `None`）。
///
/// 白名单项（逗号/分号/换行分隔）按任一命中为准：
/// - `*`：全部站点（**不建议**，仅应急）；
/// - 站点 key（`nginx:/path#1` / `iis:站点名`）；
/// - nginx 配置文件路径（整份文件）；
/// - 域名或域名通配符（`example.com` / `*.example.com`，与站点任一域名匹配）。
pub fn managed_rule(
    list: &str,
    key: &str,
    file: &str,
    domains: &[String],
) -> Option<String> {
    for raw in list.split([',', ';', '\n']) {
        let pat = raw.trim();
        if pat.is_empty() {
            continue;
        }
        if pat == "*" {
            return Some(pat.to_string());
        }
        if pat.eq_ignore_ascii_case(key) {
            return Some(pat.to_string());
        }
        if !file.is_empty() && path_eq(pat, file) {
            return Some(pat.to_string());
        }
        if domains
            .iter()
            .any(|d| dhrust::cert::domain_matches(pat, d))
        {
            return Some(pat.to_string());
        }
    }
    None
}

fn path_eq(a: &str, b: &str) -> bool {
    let norm = |s: &str| s.replace('\\', "/").trim_end_matches('/').to_string();
    let (a, b) = (norm(a), norm(b));
    if cfg!(windows) {
        a.eq_ignore_ascii_case(&b)
    } else {
        a == b
    }
}

// ———————————————— IIS ————————————————

/// IIS 绑定：(协议, 绑定信息, 主机, 端口)。
pub type IisBinding = (String, String, String, u16);

/// IIS 站点：(站点名, 绑定列表)。
pub type IisSiteBindings = (String, Vec<IisBinding>);

/// 解析 `applicationHost.config` 中的站点与绑定（纯函数，可单测）。
///
/// 返回 `(站点名, [(协议, 绑定信息, 主机, 端口)])`。
pub fn parse_iis_sites(xml: &str) -> Vec<IisSiteBindings> {
    let Ok(value) = dhrust::config::read_xml_to_json(xml) else {
        return Vec::new();
    };
    let root = value.get("configuration").unwrap_or(&value);
    let Some(sites) = root
        .get("system.applicationHost")
        .and_then(|h| h.get("sites"))
        .and_then(|s| s.get("site"))
    else {
        return Vec::new();
    };
    let list: Vec<&serde_json::Value> = match sites {
        serde_json::Value::Array(a) => a.iter().collect(),
        v => vec![v],
    };
    let mut out = Vec::new();
    for site in list {
        let name = site
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if name.is_empty() {
            continue;
        }
        let bindings: Vec<IisBinding> = site
            .get("bindings")
            .and_then(|b| b.get("binding"))
            .map(|b| match b {
                serde_json::Value::Array(a) => a.iter().collect::<Vec<_>>(),
                v => vec![v],
            })
            .unwrap_or_default()
            .into_iter()
            .map(|b| {
                let protocol = b
                    .get("protocol")
                    .and_then(|v| v.as_str())
                    .unwrap_or("http")
                    .to_ascii_lowercase();
                let info = b
                    .get("bindingInformation")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let (host, port) = split_binding(&info);
                (protocol, info, host, port)
            })
            .collect();
        out.push((name, bindings));
    }
    out
}

/// 拆分 IIS `bindingInformation`：`*:443:host` / `ip:port:` / `:443:`。
pub fn split_binding(info: &str) -> (String, u16) {
    let parts: Vec<&str> = info.split(':').collect();
    if parts.len() < 2 {
        return (String::new(), 0);
    }
    let port = parts[1].trim().parse::<u16>().unwrap_or(0);
    let host = parts[2..].join(":").trim().to_string();
    let host = if host == "*" { String::new() } else { host };
    (host, port)
}

fn iis_sites(cfg: &AgentConfig) -> Vec<SiteView> {
    if !cfg!(windows) || !dhrust::sys::iis::available() {
        return Vec::new();
    }
    let Some((config_path, xml)) = dhrust::sys::iis::read_host_config_with_path() else {
        return Vec::new();
    };
    let file_text = config_path.to_string_lossy().to_string();
    let mut out = Vec::new();
    for (name, bindings) in parse_iis_sites(&xml) {
        let mut domains: Vec<String> = Vec::new();
        let mut ssl = false;
        for (protocol, info, host, port) in &bindings {
            let _ = (info, port);
            if protocol == "https" {
                ssl = true;
            }
            if !host.is_empty() && !domains.iter().any(|d| d.eq_ignore_ascii_case(host)) {
                domains.push(host.clone());
            }
        }
        let key = format!("iis:{name}");
        let managed_by = managed_rule(&cfg.cert_managed_sites, &key, &file_text, &domains);
        let managed = managed_by.is_some();
        let system = is_system_site(&domains);
        out.push(SiteView {
            key,
            kind: "iis".to_string(),
            name,
            domains,
            ssl,
            cert_path: String::new(),
            key_path: String::new(),
            cert: None,
            managed,
            managed_by: managed_by.unwrap_or_default(),
            system,
            file: file_text.clone(),
            note: if ssl {
                String::new()
            } else {
                "站点暂无 HTTPS 绑定（部署需显式选择「启用 HTTPS」）".to_string()
            },
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
# 宝塔风格 vhost
server {
    listen 80;
    server_name example.com www.example.com;
    access_log /www/wwwlogs/example.com.log;
    location / {
        proxy_pass http://127.0.0.1:8080;
    }
}

server {
    listen 443 ssl http2;
    server_name api.example.com;
    ssl_certificate /etc/letsencrypt/live/example.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/example.com/privkey.pem;
    location ~ ^/api {
        server_name inner.example.com;
    }
}

server {
    listen 80;
    server_name _;
}
"#;

    #[test]
    fn parses_server_blocks_with_positions() {
        let list = parse_nginx_servers(SAMPLE);
        assert_eq!(list.len(), 3, "应有 3 个 server 块：{list:?}");
        assert_eq!(list[0].server_names, vec!["example.com", "www.example.com"]);
        assert!(!list[0].ssl);
        assert!(list[0].cert.is_none());
        assert!(list[1].ssl);
        assert_eq!(list[1].server_names, vec!["api.example.com"]);
        let cert = list[1].cert.as_ref().unwrap();
        assert_eq!(cert.value, "/etc/letsencrypt/live/example.com/fullchain.pem");
        assert_eq!(
            &SAMPLE[cert.start..cert.end],
            "/etc/letsencrypt/live/example.com/fullchain.pem"
        );
        assert!(list[1].key.is_some());
        // 嵌套 location 内的 server_name 不应污染顶层
        assert!(!list[1].server_names.contains(&"inner.example.com".to_string()));
        // `_` 默认站点过滤
        assert!(list[2].server_names.is_empty());
        // 区间有效：open < close
        assert!(list[0].open_brace < list[0].close_brace);
    }

    #[test]
    fn handles_quoted_paths_and_comments() {
        let text = r#"
server {
    # ssl_certificate /old/cert.pem;
    listen 443 ssl;
    server_name a.com;
    ssl_certificate "/path with space/fullchain.pem";
    ssl_certificate_key '/path with space/key.pem';
}
"#;
        let list = parse_nginx_servers(text);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].cert.as_ref().unwrap().value, "/path with space/fullchain.pem");
        assert_eq!(list[0].key.as_ref().unwrap().value, "/path with space/key.pem");
        assert_eq!(&text[list[0].cert.as_ref().unwrap().start..list[0].cert.as_ref().unwrap().end], "\"/path with space/fullchain.pem\"");
    }

    #[test]
    fn wildcard_and_multi_domains_kept() {
        let text = r#"
server {
    listen 443 ssl;
    server_name *.example.com example.com ~^regex\\.example\\.com$ $host;
}
"#;
        let list = parse_nginx_servers(text);
        assert_eq!(
            list[0].server_names,
            vec!["*.example.com", "example.com"],
            "正则与变量应被过滤"
        );
    }

    #[test]
    fn whitelist_matching_rules() {
        fn managed(list: &str, key: &str, file: &str, domains: &[String]) -> bool {
            managed_rule(list, key, file, domains).is_some()
        }
        let domains = vec!["a.example.com".to_string()];
        assert!(!managed("", "k", "/f", &domains));
        assert!(managed("k", "k", "/f", &domains));
        assert!(managed("/f", "k", "/f", &domains));
        assert!(managed("*.example.com", "k", "/f", &domains));
        assert!(managed("a.example.com", "k", "/f", &domains));
        assert!(!managed("b.example.com", "k", "/f", &domains));
        assert!(managed("*", "k", "/f", &domains));
        assert!(managed(
            "x; /f ; y",
            "k",
            "/f",
            &domains
        ));
        // 命中的规则原文要能回传到面板（解释“为什么已托管”）
        assert_eq!(
            managed_rule("x; *.example.com", "k", "/f", &domains).as_deref(),
            Some("*.example.com")
        );
        assert_eq!(managed_rule("", "k", "/f", &domains), None);
    }

    /// 心跳盘点缓存：配置/白名单没变就复用；任一处变化必须立即重新盘点。
    #[test]
    fn inventory_cache_invalidates_on_change() {
        let dir = std::env::temp_dir().join(format!("ragent-sites-cache-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let base = dir.clone();
        let mut cfg = AgentConfig {
            cert_nginx_dirs: dir.to_string_lossy().to_string(),
            ..AgentConfig::default()
        };

        std::fs::write(
            dir.join("a.conf"),
            "server {\n    listen 80;\n    server_name a.example.com;\n}\n",
        )
        .unwrap();
        let (inv1, fresh1) = inventory_cached(&cfg, &base);
        assert!(fresh1, "首次必定重新盘点");
        assert_eq!(inv1.sites.len(), 1);
        assert!(!inv1.sites[0].managed);
        // 没有任何变化 → 命中缓存（结果一致，且标记为「无需上报」）
        let (inv_same, fresh_same) = inventory_cached(&cfg, &base);
        assert_eq!(inv_same.sites.len(), 1);
        assert!(!fresh_same, "无变化时心跳不应重复上报站点清单");

        // 白名单变化 → 立即反映（不必等 TTL）
        cfg.cert_managed_sites = "*.example.com".to_string();
        let (inv2, fresh2) = inventory_cached(&cfg, &base);
        assert!(fresh2);
        assert!(inv2.sites[0].managed, "{:?}", inv2.sites[0]);

        // 新增站点文件 → 立即反映
        std::fs::write(
            dir.join("b.conf"),
            "server {\n    listen 80;\n    server_name b.example.com;\n}\n",
        )
        .unwrap();
        let (inv3, fresh3) = inventory_cached(&cfg, &base);
        assert!(fresh3);
        assert_eq!(inv3.sites.len(), 2);

        invalidate_cache();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parses_iis_bindings() {
        let xml = r#"<?xml version="1.0"?>
<configuration>
  <system.applicationHost>
    <sites>
      <site name="Default Web Site" id="1">
        <bindings>
          <binding protocol="http" bindingInformation="*:80:" />
          <binding protocol="https" bindingInformation="*:443:www.example.com" sslFlags="1" />
        </bindings>
      </site>
      <site name="shop" id="2">
        <bindings>
          <binding protocol="https" bindingInformation="*:8443:shop.example.com" />
        </bindings>
      </site>
    </sites>
  </system.applicationHost>
</configuration>"#;
        let sites = parse_iis_sites(xml);
        assert_eq!(sites.len(), 2);
        assert_eq!(sites[0].0, "Default Web Site");
        assert_eq!(sites[0].1.len(), 2);
        assert_eq!(sites[0].1[1].0, "https");
        assert_eq!(sites[0].1[1].2, "www.example.com");
        assert_eq!(sites[0].1[1].3, 443);
        assert_eq!(sites[1].1[0].3, 8443);
    }

    #[test]
    fn binding_split_edge_cases() {
        assert_eq!(split_binding("*:443:host.example.com"), ("host.example.com".into(), 443));
        assert_eq!(split_binding(":8080:"), (String::new(), 8080));
        assert_eq!(split_binding("*:443:*"), (String::new(), 443));
        assert_eq!(split_binding("bad"), (String::new(), 0));
    }
}
