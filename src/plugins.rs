//! 轻量插件（一期）：本地放置的扩展页面。
//!
//! - 目录约定：`{base}/Plugins/<id>/`，内含 `plugin.json` 清单（字段均可选）：
//!   `{ "name": "显示名", "description": "说明", "version": "1.0.0",
//!      "icon": "🧩", "entry": "index.html", "app": "关联子服务名" }`
//! - 面板「🧩 插件」页：上传 zip 安装 / 列表 / iframe 打开 / 卸载；
//! - 插件页面经 `/plugins/<id>/...` 同源静态服务：复用登录态，可调用面板接口
//!   （受当前用户菜单权限与服务端审计约束）；
//! - 安全边界：插件由管理员本地放置（管理员本可上传文件、注册子服务，**不新增信任面**）；
//!   无在线市场、不代执行安装脚本；zip 条目经 `enclosed_name` 防目录穿越。

use std::fs;
use std::path::{Path, PathBuf};

use dhrust::net::controller::{arg, json_body, json_error, json_result, ActionResult};
use dhrust::net::http::HttpResponse;
use dhrust::net::router::Ctx;
use dhrust::net::static_files::StaticFiles;
use serde_json::{json, Value as Json};

use crate::util;
use crate::webpanel::WebPanel;

/// 插件根目录名（`{base}/Plugins`）。
pub const PLUGIN_DIR: &str = "Plugins";

/// 插件根目录（`{base}/Plugins`）。
pub fn root(base: &Path) -> PathBuf {
    base.join(PLUGIN_DIR)
}

/// 插件 id（目录名）校验：1~64 位 `[A-Za-z0-9._-]`，且非 `.`/`..`。
pub fn validate_id(id: &str) -> Result<(), String> {
    let ok = !id.is_empty()
        && id.len() <= 64
        && id != "."
        && id != ".."
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if ok {
        Ok(())
    } else {
        Err("插件目录名不合法（仅允许字母/数字/._-，≤64 位）".to_string())
    }
}

/// 读取并校验单个插件清单（`plugin.json`）。
fn read_plugin(dir: &Path, id: &str) -> Result<Json, String> {
    validate_id(id)?;
    let text = fs::read_to_string(dir.join("plugin.json"))
        .map_err(|_| "缺少 plugin.json".to_string())?;
    let man: Json =
        serde_json::from_str(&text).map_err(|e| format!("plugin.json 解析失败：{e}"))?;
    if !man.is_object() {
        return Err("plugin.json 需为 JSON 对象".to_string());
    }
    let entry = man
        .get("entry")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("index.html")
        .to_string();
    if entry
        .split('/')
        .any(|s| s.is_empty() || s == "." || s == ".." || s.contains('\\') || s.contains(':'))
    {
        return Err("entry 路径不合法".to_string());
    }
    if !dir.join(&entry).is_file() {
        return Err(format!("入口文件不存在：{entry}"));
    }
    let field = |k: &str| {
        man.get(k)
            .and_then(|v| v.as_str())
            .map(str::trim)
            .unwrap_or("")
            .to_string()
    };
    let name = {
        let n = field("name");
        if n.is_empty() { id.to_string() } else { n }
    };
    let icon = {
        let i = field("icon");
        if i.is_empty() { "🧩".to_string() } else { i }
    };
    Ok(json!({
        "id": id,
        "name": name,
        "description": field("description"),
        "version": field("version"),
        "icon": icon,
        "entry": entry,
        "app": field("app"),
    }))
}

/// 列出全部插件（有效条目 + 无效目录提示）。
pub fn list(base: &Path) -> Json {
    let dir = root(base);
    let mut plugins: Vec<Json> = Vec::new();
    let mut invalid: Vec<Json> = Vec::new();
    if let Ok(rd) = fs::read_dir(&dir) {
        let mut entries: Vec<_> = rd.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let id = entry.file_name().to_string_lossy().to_string();
            if id.starts_with('.') {
                continue; // 安装/临时目录
            }
            match read_plugin(&path, &id) {
                Ok(info) => plugins.push(info),
                Err(e) => invalid.push(json!({ "id": id, "error": e })),
            }
        }
    }
    json!({ "dir": dir.display().to_string(), "plugins": plugins, "invalid": invalid })
}

/// 安装插件（手动上传入口；拒绝重名）。
pub fn install(base: &Path, data: &[u8], name: &str) -> Result<String, String> {
    install_bytes(base, data, name, false)
}

/// 安装插件字节流（`data` = zip；`name` = 文件名，扁平包时用作目录名）。
/// `replace` = true 时同 id 已存在则替换（在线更新用；失败自动回滚旧版本）。
pub fn install_bytes(
    base: &Path,
    data: &[u8],
    name: &str,
    replace: bool,
) -> Result<String, String> {
    if data.is_empty() {
        return Err("上传内容为空".to_string());
    }
    let dir = root(base);
    fs::create_dir_all(&dir).map_err(|e| format!("创建插件目录失败：{e}"))?;
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let zip_path = dir.join(format!(".upload-{}-{}.zip", std::process::id(), nonce));
    let staging = dir.join(format!(".staging-{}-{}", std::process::id(), nonce));
    fs::write(&zip_path, data).map_err(|e| format!("写入临时文件失败：{e}"))?;
    let result = (|| -> Result<String, String> {
        dhrust::zip::extract_zip(&zip_path, &staging)?;
        let (src, id) = locate(&staging, name)?;
        validate_id(&id)?;
        read_plugin(&src, &id)?; // 校验清单与入口
        let dest = dir.join(&id);
        if dest.exists() && !replace {
            return Err(format!("插件已存在：{id}（请先卸载）"));
        }
        // 更新：旧目录先让位，替换失败时回滚
        let old = dir.join(format!(".old-{}-{}", std::process::id(), nonce));
        let had_old = dest.exists();
        if had_old {
            fs::rename(&dest, &old).map_err(|e| format!("替换旧版本失败：{e}"))?;
        }
        match fs::rename(&src, &dest) {
            Ok(()) => {
                if had_old {
                    let _ = fs::remove_dir_all(&old);
                }
                Ok(id)
            }
            Err(e) => {
                if had_old {
                    let _ = fs::rename(&old, &dest);
                }
                Err(format!("安装失败：{e}"))
            }
        }
    })();
    let _ = fs::remove_file(&zip_path);
    let _ = fs::remove_dir_all(&staging);
    result
}

/// 定位解压目录中的插件根（根含 `plugin.json`，或唯一子目录含之）。
fn locate(staging: &Path, fallback_name: &str) -> Result<(PathBuf, String), String> {
    if staging.join("plugin.json").is_file() {
        let stem = Path::new(fallback_name.trim())
            .file_stem()
            .map(|s| s.to_string_lossy().trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "plugin".to_string());
        return Ok((staging.to_path_buf(), stem));
    }
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Ok(rd) = fs::read_dir(staging) {
        for entry in rd.flatten() {
            let p = entry.path();
            if p.is_dir() && !entry.file_name().to_string_lossy().starts_with('.') {
                dirs.push(p);
            }
        }
    }
    if dirs.len() == 1 && dirs[0].join("plugin.json").is_file() {
        let id = dirs[0]
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        return Ok((dirs[0].clone(), id));
    }
    Err("压缩包中未找到 plugin.json（可置于包根或唯一子目录）".to_string())
}

/// 卸载插件（删除目录）。
pub fn uninstall(base: &Path, id: &str) -> Result<(), String> {
    validate_id(id)?;
    let dest = root(base).join(id);
    if !dest.is_dir() {
        return Err(format!("插件不存在：{id}"));
    }
    fs::remove_dir_all(&dest).map_err(|e| format!("卸载失败：{e}"))
}

/// 静态服务 `/plugins/<id>/<相对路径>`（目录/空路径命中 `index.html`；
/// 未命中或路径穿越返回 `None`）。
pub fn serve(base: &Path, path: &str) -> Option<HttpResponse> {
    let plain = path.split(['?', '#']).next().unwrap_or("");
    let rest = plain.strip_prefix("/plugins/")?;
    let (id, rel) = match rest.split_once('/') {
        Some((id, rel)) => (id, rel),
        None => (rest, ""),
    };
    validate_id(id).ok()?;
    let dir = root(base).join(id);
    if !dir.is_dir() {
        return None;
    }
    StaticFiles::new(dir).try_serve_file(&format!("/{rel}"))
}

// ————— 在线插件源（catalog.json） —————

/// 在线插件源条目（`catalog.json` 中的一项）。
#[derive(Clone, Debug)]
pub struct StoreEntry {
    pub id: String,
    pub name: String,
    pub description: String,
    /// 开发商/上传者（可选字段；平台侧为归属账号名）
    pub author: String,
    pub version: String,
    pub icon: String,
    pub app: String,
    pub url: String,
    pub sha256: String,
}

/// 下载抽象（生产走 HTTP；单元测试注入映射表）。
pub trait StoreFetcher {
    /// 取回 `url` 全部字节（`max` 上限，超限报错）。
    fn get(&self, url: &str, max: usize) -> Result<Vec<u8>, String>;
}

/// 生产下载器：`dhrust::net::http_client`（支持 https；跟随至多 4 次重定向）。
pub struct HttpFetcher;

/// 在独立线程执行阻塞下载：服务端处理器运行在 tokio 运行时线程上，
/// 直接调用 `blocking_get` 会因“运行时内 block_on”而 panic（实测会杀死 HTTP 服务线程）。
fn fetch_blocking(url: &str) -> Result<dhrust::net::http_client::HttpResponse, String> {
    let u = url.to_string();
    let handle = std::thread::Builder::new()
        .name("plugin-store-fetch".to_string())
        .spawn(move || dhrust::net::http_client::blocking_get(&u, std::time::Duration::from_secs(30)))
        .map_err(|e| format!("创建下载线程失败：{e}"))?;
    match handle.join() {
        Ok(Ok(resp)) => Ok(resp),
        Ok(Err(e)) => Err(format!("下载失败：{}", e.0)),
        Err(_) => Err("下载线程异常退出".to_string()),
    }
}

impl StoreFetcher for HttpFetcher {
    fn get(&self, url: &str, max: usize) -> Result<Vec<u8>, String> {
        let mut current = url.trim().to_string();
        for _ in 0..5 {
            if !is_allowed_url(&current) {
                return Err(format!("仅允许 https 地址（127.0.0.1 例外）：{current}"));
            }
            let resp = fetch_blocking(&current)?;
            if (300..400).contains(&resp.status) {
                // 跟随重定向（只接受绝对 https / 回环 http 地址）
                let loc = resp
                    .header("location")
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| format!("重定向缺少 Location（HTTP {}）", resp.status))?;
                if !is_allowed_url(loc) {
                    return Err("重定向目标不是允许的 https 地址".to_string());
                }
                current = loc.to_string();
                continue;
            }
            if !resp.is_success() {
                return Err(format!("下载失败：HTTP {}", resp.status));
            }
            if resp.body.len() > max {
                return Err(format!(
                    "内容超出大小上限（{} > {} 字节）",
                    resp.body.len(),
                    max
                ));
            }
            return Ok(resp.body);
        }
        Err("重定向次数过多".to_string())
    }
}

/// URL 放行规则：https 一律允许；http 仅 127.0.0.1（本地调试）。
pub fn is_allowed_url(url: &str) -> bool {
    let u = url.trim();
    u.starts_with("https://")
        || u.starts_with("http://127.0.0.1:")
        || u.starts_with("http://127.0.0.1/")
}

/// 清单 / 插件包大小上限。
const MAX_CATALOG_SIZE: usize = 1024 * 1024;
const MAX_PACKAGE_SIZE: usize = 32 * 1024 * 1024;

/// 加载并校验在线插件源（可选强制验签 `catalog.json.sig`）。
pub fn load_store(
    cfg: &crate::config::AgentConfig,
    fetch: &dyn StoreFetcher,
) -> Result<Vec<StoreEntry>, String> {
    let url = cfg.plugin_store_url.trim();
    if url.is_empty() {
        return Err("未配置插件源".to_string());
    }
    if !is_allowed_url(url) {
        return Err("插件源地址仅允许 https（127.0.0.1 例外）".to_string());
    }
    let bytes = fetch.get(url, MAX_CATALOG_SIZE)?;
    let pubkey = cfg.plugin_store_pubkey.trim();
    if !pubkey.is_empty() {
        let sig_text = fetch.get(&format!("{url}.sig"), 4096)?;
        verify_catalog_signature(pubkey, &bytes, &sig_text)?;
    }
    let json: Json =
        serde_json::from_slice(&bytes).map_err(|e| format!("catalog.json 解析失败：{e}"))?;
    let Some(list) = json.get("plugins").and_then(|v| v.as_array()) else {
        return Err("catalog.json 缺少 plugins 数组".to_string());
    };
    let mut entries = Vec::new();
    for item in list {
        match parse_entry(item) {
            Ok(entry) => entries.push(entry),
            Err(e) => util::log_format("在线插件源：忽略无效条目（{}）", &[&e]),
        }
    }
    Ok(entries)
}

/// 解析校验单个清单条目。
fn parse_entry(item: &Json) -> Result<StoreEntry, String> {
    let field = |k: &str| {
        item.get(k)
            .and_then(|v| v.as_str())
            .map(str::trim)
            .unwrap_or("")
            .to_string()
    };
    let id = field("id");
    validate_id(&id)?;
    let url = field("url");
    if !is_allowed_url(&url) {
        return Err(format!("{id}：url 仅允许 https（127.0.0.1 例外）"));
    }
    let sha256 = field("sha256").to_ascii_lowercase();
    if sha256.len() != 64 || !sha256.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!("{id}：sha256 缺失或格式错误"));
    }
    let name = {
        let n = field("name");
        if n.is_empty() { id.clone() } else { n }
    };
    let icon = {
        let i = field("icon");
        if i.is_empty() { "🧩".to_string() } else { i }
    };
    Ok(StoreEntry {
        id,
        name,
        description: field("description"),
        author: field("author"),
        version: field("version"),
        icon,
        app: field("app"),
        url,
        sha256,
    })
}

/// 校验目录签名（Ed25519；`pubkey_hex` 接受 32 字节裸公钥或 44 字节 SPKI DER，均为 hex）。
pub fn verify_catalog_signature(
    pubkey_hex: &str,
    data: &[u8],
    sig_text: &[u8],
) -> Result<(), String> {
    use ed25519_dalek::{Signature, Verifier};
    let key = parse_pubkey(pubkey_hex)?;
    let sig_bytes = dhrust::sign::base64_decode(String::from_utf8_lossy(sig_text).trim())
        .ok_or_else(|| "签名文件不是有效的 base64".to_string())?;
    let sig_arr: [u8; 64] = sig_bytes
        .as_slice()
        .try_into()
        .map_err(|_| "签名长度不是 64 字节".to_string())?;
    let sig = Signature::from_bytes(&sig_arr);
    key.verify(data, &sig)
        .map_err(|_| "插件源签名校验失败（目录可能被篡改）".to_string())
}

/// 解析十六进制公钥（32 字节裸公钥，或 44 字节 Ed25519 SPKI DER）。
fn parse_pubkey(pubkey_hex: &str) -> Result<ed25519_dalek::VerifyingKey, String> {
    let clean = pubkey_hex.trim().trim_start_matches("0x").trim();
    let bytes = hex_decode(clean).ok_or_else(|| "公钥不是有效的 hex".to_string())?;
    // Ed25519 SPKI DER 前缀 302a300506032b6570032100（12 字节）+ 32 字节裸公钥
    const SPKI: [u8; 12] = [
        0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
    ];
    let raw: [u8; 32] = if bytes.len() == 32 {
        bytes.as_slice().try_into().unwrap()
    } else if bytes.len() == 44 && bytes.starts_with(&SPKI) {
        bytes[12..].try_into().unwrap()
    } else {
        return Err("公钥长度应为 32 字节（或 44 字节 SPKI DER）".to_string());
    };
    ed25519_dalek::VerifyingKey::from_bytes(&raw).map_err(|e| format!("公钥无效：{e}"))
}

/// hex 解码（奇数长度或非法字符返回 `None`）。
pub fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    let val = |c: u8| -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    };
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() / 2);
    let mut i = 0;
    while i < bytes.len() {
        out.push((val(bytes[i])? << 4) | val(bytes[i + 1])?);
        i += 2;
    }
    Some(out)
}

/// 在线安装/更新：从插件源按 id 下载（SHA-256 必须匹配），校验通过后安装（同 id 已存在 = 替换更新）。
pub fn install_online(
    base: &Path,
    cfg: &crate::config::AgentConfig,
    fetch: &dyn StoreFetcher,
    id: &str,
) -> Result<String, String> {
    validate_id(id)?;
    let entries = load_store(cfg, fetch)?;
    let entry = entries
        .iter()
        .find(|e| e.id == id)
        .ok_or_else(|| format!("插件源中不存在：{id}"))?;
    let data = fetch.get(&entry.url, MAX_PACKAGE_SIZE)?;
    let got = dhrust::sign::sha256_hex(&data);
    if !got.eq_ignore_ascii_case(&entry.sha256) {
        return Err(format!(
            "SHA-256 校验失败，已拒绝安装（期望 {}，实际 {}）",
            entry.sha256, got
        ));
    }
    install_bytes(base, &data, &format!("{id}.zip"), true)
}

// ————— 发布工具（发布端：校验包 / 入库 / 更新目录 / 签名） —————

/// 插件包清单概要（发布用）。
#[derive(Clone, Debug)]
pub struct PackageManifest {
    pub id: String,
    pub name: String,
    pub description: String,
    pub version: String,
    pub icon: String,
    pub app: String,
}

/// 校验插件包（zip 字节）并读取其清单（`fallback_name` 为扁平包的目录名回退）。
pub fn inspect_package(data: &[u8], fallback_name: &str) -> Result<PackageManifest, String> {
    if data.is_empty() {
        return Err("插件包为空".to_string());
    }
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("pek-plugin-inspect-{}-{}", std::process::id(), nonce));
    let zip_path = dir.with_extension("inspect.zip");
    fs::create_dir_all(&dir).map_err(|e| format!("创建临时目录失败：{e}"))?;
    fs::write(&zip_path, data).map_err(|e| format!("写入临时文件失败：{e}"))?;
    let result = (|| -> Result<PackageManifest, String> {
        dhrust::zip::extract_zip(&zip_path, &dir)?;
        let (src, id) = locate(&dir, fallback_name)?;
        let info = read_plugin(&src, &id)?;
        let s = |k: &str| {
            info.get(k)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string()
        };
        Ok(PackageManifest {
            id,
            name: s("name"),
            description: s("description"),
            version: s("version"),
            icon: s("icon"),
            app: s("app"),
        })
    })();
    let _ = fs::remove_file(&zip_path);
    let _ = fs::remove_dir_all(&dir);
    result
}

/// 发布信息（发布端输出）。
#[derive(Clone, Debug)]
pub struct PublishedInfo {
    pub id: String,
    pub version: String,
    pub sha256: String,
    pub url: String,
    pub file_name: String,
}

/// 对插件源目录（catalog.json）签名，产出 `catalog.json.sig`（base64，Ed25519）。
pub fn sign_catalog_file(catalog: &Path, key_file: &Path) -> Result<(), String> {
    use ed25519_dalek::{Signer, SigningKey};
    let key_hex = fs::read_to_string(key_file)
        .map_err(|e| format!("读取私钥失败（{}）：{e}", key_file.display()))?;
    let bytes = hex_decode(key_hex.trim()).ok_or_else(|| "私钥不是有效的 hex 文本".to_string())?;
    let seed: [u8; 32] = bytes
        .as_slice()
        .try_into()
        .map_err(|_| "私钥长度应为 32 字节".to_string())?;
    let data = fs::read(catalog).map_err(|e| format!("读取 {} 失败：{e}", catalog.display()))?;
    let key = SigningKey::from_bytes(&seed);
    let sig = key.sign(&data);
    let sig_path = PathBuf::from(format!("{}.sig", catalog.display()));
    fs::write(&sig_path, dhrust::sign::base64_encode(&sig.to_bytes()))
        .map_err(|e| format!("写入签名失败：{e}"))?;
    Ok(())
}

/// 发布插件包到插件源目录：
/// 校验包 → 复制为 `{id}-{version}.zip` → 更新 `catalog.json`（url+sha256）→ 重新签名。
/// `base_url` 为空时读取 `store.json` 中保存的地址；首次发布必须提供。
pub fn publish_to_store(
    store: &Path,
    zip_file: &Path,
    base_url: &str,
    key_file: &Path,
) -> Result<PublishedInfo, String> {
    fs::create_dir_all(store).map_err(|e| format!("创建插件源目录失败：{e}"))?;
    let cfg_path = store.join("store.json");
    let mut store_cfg: Json = fs::read_to_string(&cfg_path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(|| json!({ "name": "插件源" }));
    let base = if base_url.trim().is_empty() {
        store_cfg
            .get("baseUrl")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string()
    } else {
        base_url.trim().trim_end_matches('/').to_string()
    };
    if base.is_empty() {
        return Err("缺少 --base-url（首次发布必须提供插件源的基础 HTTPS 地址）".to_string());
    }
    if !is_allowed_url(&format!("{base}/")) {
        return Err("base-url 仅允许 https（127.0.0.1 例外）".to_string());
    }
    if !base_url.trim().is_empty() {
        store_cfg["baseUrl"] = json!(base);
        fs::write(&cfg_path, serde_json::to_string_pretty(&store_cfg).unwrap())
            .map_err(|e| format!("写入 store.json 失败：{e}"))?;
    }
    // 校验包并读取清单
    let data = fs::read(zip_file).map_err(|e| format!("读取插件包失败：{e}"))?;
    let stem = zip_file
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let manifest = inspect_package(&data, &stem)?;
    let file_name = if manifest.version.is_empty() {
        format!("{}.zip", manifest.id)
    } else {
        format!("{}-{}.zip", manifest.id, manifest.version)
    };
    let dest = store.join(&file_name);
    fs::write(&dest, &data).map_err(|e| format!("写入插件包失败：{e}"))?;
    let sha256 = dhrust::sign::sha256_hex(&data);
    let url = format!("{base}/{file_name}");
    // 更新目录（按 id upsert；记录旧版本包名用于清理）
    let catalog_path = store.join("catalog.json");
    let mut catalog: Json = fs::read_to_string(&catalog_path)
        .ok()
        .and_then(|t| serde_json::from_str::<Json>(&t).ok())
        .filter(|v| v.is_object())
        .unwrap_or_else(|| {
            json!({
                "name": store_cfg.get("name").and_then(|v| v.as_str()).unwrap_or("插件源"),
                "plugins": []
            })
        });
    if catalog.get("plugins").and_then(|v| v.as_array()).is_none() {
        catalog["plugins"] = json!([]);
    }
    let old_file = catalog
        .get("plugins")
        .and_then(|v| v.as_array())
        .and_then(|arr| {
            arr.iter()
                .find(|p| p.get("id").and_then(|v| v.as_str()) == Some(manifest.id.as_str()))
        })
        .and_then(|p| p.get("url"))
        .and_then(|v| v.as_str())
        .and_then(|u| u.rsplit('/').next())
        .map(|s| s.to_string())
        .filter(|s| s.ends_with(".zip") && s != &file_name);
    let entry = json!({
        "id": manifest.id,
        "name": manifest.name,
        "description": manifest.description,
        "version": manifest.version,
        "icon": manifest.icon,
        "app": manifest.app,
        "url": url,
        "sha256": sha256,
    });
    if let Some(arr) = catalog.get_mut("plugins").and_then(|v| v.as_array_mut()) {
        arr.retain(|p| p.get("id").and_then(|v| v.as_str()) != Some(manifest.id.as_str()));
        arr.push(entry);
        arr.sort_by(|a, b| {
            a.get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .cmp(b.get("id").and_then(|v| v.as_str()).unwrap_or(""))
        });
    }
    catalog["updated"] = json!(chrono::Local::now().format("%Y-%m-%d").to_string());
    fs::write(&catalog_path, serde_json::to_string_pretty(&catalog).unwrap())
        .map_err(|e| format!("写入 catalog.json 失败：{e}"))?;
    if let Some(old) = old_file {
        let _ = fs::remove_file(store.join(old));
    }
    // 重新签名
    sign_catalog_file(&catalog_path, key_file)?;
    Ok(PublishedInfo {
        id: manifest.id,
        version: manifest.version,
        sha256,
        url,
        file_name,
    })
}

/// 从插件源目录移除插件（删除条目与包文件并重新签名）。
pub fn remove_from_store(store: &Path, id: &str, key_file: &Path) -> Result<(), String> {
    validate_id(id)?;
    let catalog_path = store.join("catalog.json");
    let text =
        fs::read_to_string(&catalog_path).map_err(|e| format!("读取 catalog.json 失败：{e}"))?;
    let mut catalog: Json =
        serde_json::from_str(&text).map_err(|e| format!("catalog.json 解析失败：{e}"))?;
    let mut removed_file: Option<String> = None;
    let Some(arr) = catalog.get_mut("plugins").and_then(|v| v.as_array_mut()) else {
        return Err("catalog.json 缺少 plugins 数组".to_string());
    };
    arr.retain(|p| {
        if p.get("id").and_then(|v| v.as_str()) == Some(id) {
            removed_file = p
                .get("url")
                .and_then(|v| v.as_str())
                .and_then(|u| u.rsplit('/').next())
                .map(|s| s.to_string());
            false
        } else {
            true
        }
    });
    if removed_file.is_none() {
        return Err(format!("插件源中不存在：{id}"));
    }
    catalog["updated"] = json!(chrono::Local::now().format("%Y-%m-%d").to_string());
    fs::write(&catalog_path, serde_json::to_string_pretty(&catalog).unwrap())
        .map_err(|e| format!("写入 catalog.json 失败：{e}"))?;
    if let Some(f) = removed_file {
        if f.ends_with(".zip") {
            let _ = fs::remove_file(store.join(f));
        }
    }
    sign_catalog_file(&catalog_path, key_file)
}

// ————— 面板动作 —————

/// `GET /star/pluginList`：已安装插件列表。
pub fn plugin_list(panel: &WebPanel, ctx: &Ctx) -> ActionResult {
    if !panel.check_auth(ctx) {
        return json_error(401, "Unauthorized");
    }
    json_result(0, "", Some(list(panel.base())))
}

/// `POST /star/pluginInstall?name=<文件名>`：上传安装（请求体为 zip 字节）。
pub fn plugin_install(panel: &WebPanel, ctx: &Ctx) -> ActionResult {
    if !panel.check_auth(ctx) {
        return json_error(401, "Unauthorized");
    }
    let name = arg(ctx, "name").unwrap_or_default();
    match install(panel.base(), &ctx.req.body, &name) {
        Ok(id) => {
            util::log_format("插件安装完成：{}", &[&id]);
            json_result(0, &format!("插件已安装：{id}"), Some(json!({ "id": id })))
        }
        Err(e) => json_error(400, &e),
    }
}

/// `POST /star/pluginDelete {"id":"..."}`：卸载插件。
pub fn plugin_delete(panel: &WebPanel, ctx: &Ctx) -> ActionResult {
    if !panel.check_auth(ctx) {
        return json_error(401, "Unauthorized");
    }
    let id = json_body(ctx)
        .and_then(|b| {
            b.get("id")
                .and_then(|v| v.as_str())
                .map(|s| s.trim().to_string())
        })
        .unwrap_or_default();
    if id.is_empty() {
        return json_error(400, "缺少插件 id");
    }
    match uninstall(panel.base(), &id) {
        Ok(()) => {
            util::log_format("插件已卸载：{}", &[&id]);
            json_result(0, "插件已卸载", None)
        }
        Err(e) => json_error(400, &e),
    }
}

/// `GET /star/pluginStore`：在线插件源列表（含已安装/可更新状态；未配置时 configured=false）。
pub fn plugin_store(panel: &WebPanel, ctx: &Ctx) -> ActionResult {
    if !panel.check_auth(ctx) {
        return json_error(401, "Unauthorized");
    }
    let cfg = panel.config();
    let url = cfg.plugin_store_url.trim().to_string();
    let signed = !cfg.plugin_store_pubkey.trim().is_empty();
    let installed = installed_index(panel.base());
    if url.is_empty() {
        return json_result(
            0,
            "",
            Some(json!({ "configured": false, "signature": signed, "installed": installed })),
        );
    }
    match load_store(&cfg, &HttpFetcher) {
        Ok(entries) => {
            let list: Vec<Json> = entries
                .iter()
                .map(|e| {
                    let cur = installed.get(&e.id);
                    json!({
                        "id": e.id,
                        "name": e.name,
                        "description": e.description,
                        "author": e.author,
                        "version": e.version,
                        "icon": e.icon,
                        "app": e.app,
                        "url": e.url,
                        "installed": cur.is_some(),
                        "installedVersion": cur.cloned().unwrap_or_default(),
                        "upToDate": cur.map(|v| v == &e.version).unwrap_or(false),
                    })
                })
                .collect();
            json_result(
                0,
                "",
                Some(json!({
                    "configured": true,
                    "url": url,
                    "signature": signed,
                    "plugins": list,
                    "installed": installed,
                })),
            )
        }
        Err(e) => json_result(
            0,
            "",
            Some(json!({
                "configured": true,
                "url": url,
                "signature": signed,
                "error": e,
                "installed": installed,
            })),
        ),
    }
}

/// `POST /star/pluginStoreInstall {"id"}`：从在线插件源安装/更新（HTTPS + SHA-256 校验；可选强制验签）。
pub fn plugin_store_install(panel: &WebPanel, ctx: &Ctx) -> ActionResult {
    if !panel.check_auth(ctx) {
        return json_error(401, "Unauthorized");
    }
    let id = json_body(ctx)
        .and_then(|b| {
            b.get("id")
                .and_then(|v| v.as_str())
                .map(|s| s.trim().to_string())
        })
        .unwrap_or_default();
    if id.is_empty() {
        return json_error(400, "缺少插件 id");
    }
    let cfg = panel.config();
    match install_online(panel.base(), &cfg, &HttpFetcher, &id) {
        Ok(done) => {
            util::log_format("在线插件安装完成：{}", &[&done]);
            json_result(
                0,
                &format!("插件已安装/更新：{done}"),
                Some(json!({ "id": done })),
            )
        }
        Err(e) => json_error(400, &e),
    }
}

/// 已安装插件索引（id → 版本）。
fn installed_index(base: &Path) -> std::collections::BTreeMap<String, String> {
    let mut map = std::collections::BTreeMap::new();
    if let Some(arr) = list(base)["plugins"].as_array() {
        for p in arr {
            let id = p["id"].as_str().unwrap_or("").to_string();
            let version = p["version"].as_str().unwrap_or("").to_string();
            if !id.is_empty() {
                map.insert(id, version);
            }
        }
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_base(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ragent-plugins-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn build_zip(files: &[(&str, &str)]) -> Vec<u8> {
        let mut zip = dhrust::zip::ZipWriter::new();
        for (name, data) in files {
            zip.add_file(name, data.as_bytes());
        }
        zip.finish()
    }

    #[test]
    fn validate_id_rules() {
        assert!(validate_id("demo").is_ok());
        assert!(validate_id("my.plugin_1").is_ok());
        for bad in ["", ".", "..", "a/b", "a\\b", "中 文", "a:b"] {
            assert!(validate_id(bad).is_err(), "{bad} 应被拒绝");
        }
        assert!(validate_id(&"a".repeat(65)).is_err());
    }

    #[test]
    fn list_skips_dirs_without_manifest() {
        let base = temp_base("list");
        let dir = root(&base);
        fs::create_dir_all(dir.join("good")).unwrap();
        fs::write(
            dir.join("good/plugin.json"),
            r#"{"name":"好插件","entry":"index.html"}"#,
        )
        .unwrap();
        fs::write(dir.join("good/index.html"), "<html>ok</html>").unwrap();
        fs::create_dir_all(dir.join("nomanifest")).unwrap();
        let j = list(&base);
        let plugins = j["plugins"].as_array().unwrap();
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0]["id"], "good");
        assert_eq!(plugins[0]["name"], "好插件");
        let invalid = j["invalid"].as_array().unwrap();
        assert_eq!(invalid.len(), 1);
        assert_eq!(invalid[0]["id"], "nomanifest");
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn install_flat_folder_and_duplicate_and_uninstall() {
        let base = temp_base("install");
        // 扁平包：目录名取上传文件名
        let flat = build_zip(&[
            ("plugin.json", r#"{"name":"扁平"}"#),
            ("index.html", "<html>flat</html>"),
        ]);
        let id = install(&base, &flat, "myplug.zip").unwrap();
        assert_eq!(id, "myplug");
        assert!(root(&base).join("myplug/index.html").is_file());
        // 重复安装被拒
        assert!(install(&base, &flat, "myplug.zip").is_err());
        // 目录包：唯一顶层子目录
        let folder = build_zip(&[
            ("demo/plugin.json", r#"{"name":"目录包","entry":"main.html"}"#),
            ("demo/main.html", "<html>demo</html>"),
            ("demo/js/app.js", "console.log(1)"),
        ]);
        assert_eq!(install(&base, &folder, "whatever.zip").unwrap(), "demo");
        assert!(root(&base).join("demo/js/app.js").is_file());
        // 无清单 → 拒绝
        let bad = build_zip(&[("readme.txt", "hi")]);
        assert!(install(&base, &bad, "bad.zip").is_err());
        // 入口缺失 → 拒绝
        let noentry = build_zip(&[("plugin.json", r#"{"entry":"missing.html"}"#)]);
        assert!(install(&base, &noentry, "noentry.zip").is_err());
        // 卸载
        uninstall(&base, "myplug").unwrap();
        assert!(!root(&base).join("myplug").exists());
        assert!(uninstall(&base, "myplug").is_err());
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn serve_hits_files_and_blocks_traversal() {
        let base = temp_base("serve");
        let dir = root(&base).join("p1");
        fs::create_dir_all(dir.join("assets")).unwrap();
        fs::write(dir.join("index.html"), "<html>p1</html>").unwrap();
        fs::write(dir.join("assets/app.js"), "console.log(1)").unwrap();
        assert!(serve(&base, "/plugins/p1/").is_some());
        assert!(serve(&base, "/plugins/p1/index.html").is_some());
        assert!(serve(&base, "/plugins/p1/assets/app.js").is_some());
        assert!(serve(&base, "/plugins/p1/nope.txt").is_none());
        assert!(serve(&base, "/plugins/missing/").is_none());
        assert!(serve(&base, "/plugins/../secret").is_none());
        assert!(serve(&base, "/plugins/p1/../plugin.json").is_none());
        assert!(serve(&base, "/other/p1/").is_none());
        let _ = fs::remove_dir_all(&base);
    }

    // ————— 在线插件源 —————

    struct MapFetcher {
        map: std::collections::HashMap<String, Vec<u8>>,
    }

    impl MapFetcher {
        fn new(items: Vec<(&str, Vec<u8>)>) -> Self {
            Self {
                map: items.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
            }
        }
    }

    impl StoreFetcher for MapFetcher {
        fn get(&self, url: &str, _max: usize) -> Result<Vec<u8>, String> {
            self.map
                .get(url)
                .cloned()
                .ok_or_else(|| format!("(mock) 未找到：{url}"))
        }
    }

    fn store_cfg(url: &str, pubkey: &str) -> crate::config::AgentConfig {
        let mut cfg = crate::config::AgentConfig::default();
        cfg.plugin_store_url = url.to_string();
        cfg.plugin_store_pubkey = pubkey.to_string();
        cfg
    }

    fn hex_of(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    fn test_key() -> ed25519_dalek::SigningKey {
        ed25519_dalek::SigningKey::from_bytes(&[42u8; 32])
    }

    fn signed_catalog(entries_json: &str) -> (String, String) {
        use ed25519_dalek::Signer;
        let catalog = format!(r#"{{"name":"测试源","plugins":{entries_json}}}"#);
        let sig = test_key().sign(catalog.as_bytes());
        (catalog, dhrust::sign::base64_encode(&sig.to_bytes()))
    }

    #[test]
    fn hex_and_pubkey_parsing() {
        assert_eq!(hex_decode("00ff10").unwrap(), vec![0x00, 0xff, 0x10]);
        assert!(hex_decode("0").is_none());
        assert!(hex_decode("zz").is_none());
        let pub_hex = hex_of(test_key().verifying_key().as_bytes());
        assert!(parse_pubkey(&pub_hex).is_ok());
        // SPKI DER 形式（12 字节前缀 + 32 字节裸公钥）
        let mut spki = vec![
            0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
        ];
        spki.extend_from_slice(test_key().verifying_key().as_bytes());
        assert!(parse_pubkey(&hex_of(&spki)).is_ok());
        assert!(parse_pubkey("abcd").is_err());
    }

    #[test]
    fn catalog_signature_roundtrip_and_tamper() {
        let (catalog, sig) = signed_catalog("[]");
        let pub_hex = hex_of(test_key().verifying_key().as_bytes());
        assert!(verify_catalog_signature(&pub_hex, catalog.as_bytes(), sig.as_bytes()).is_ok());
        assert!(verify_catalog_signature(&pub_hex, b"tampered", sig.as_bytes()).is_err());
        assert!(verify_catalog_signature(&pub_hex, catalog.as_bytes(), b"not-base64!!!").is_err());
    }

    #[test]
    fn store_load_and_online_install_with_hash_pinning() {
        let zip = build_zip(&[
            ("demo/plugin.json", r#"{"name":"在线演示","version":"1.0"}"#),
            ("demo/index.html", "<html>online</html>"),
        ]);
        let sha = dhrust::sign::sha256_hex(&zip);
        let entries = format!(
            r#"[{{"id":"demo","name":"在线演示","version":"1.0","url":"https://example.com/demo.zip","sha256":"{sha}"}},{{"id":"bad","url":"http://evil.com/x.zip","sha256":"{sha}"}}]"#
        );
        let (catalog, sig) = signed_catalog(&entries);
        let fetch = MapFetcher::new(vec![
            ("https://example.com/catalog.json", catalog.clone().into_bytes()),
            ("https://example.com/catalog.json.sig", sig.clone().into_bytes()),
            ("https://example.com/demo.zip", zip.clone()),
        ]);
        // 未配置插件源 → 报错
        assert!(load_store(&store_cfg("", ""), &fetch).is_err());
        // 验签开启：正常通过；无效条目（http url）被忽略
        let cfg = store_cfg(
            "https://example.com/catalog.json",
            &hex_of(test_key().verifying_key().as_bytes()),
        );
        let list = load_store(&cfg, &fetch).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, "demo");
        // 验签开启但签名缺失 → 拒绝
        let fetch_no_sig = MapFetcher::new(vec![(
            "https://example.com/catalog.json",
            catalog.clone().into_bytes(),
        )]);
        assert!(load_store(&cfg, &fetch_no_sig).is_err());
        // 目录被他人重签 → 拒绝
        let (catalog2, sig2) = {
            use ed25519_dalek::Signer;
            let other = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
            let c = catalog.clone();
            let s = dhrust::sign::base64_encode(&other.sign(c.as_bytes()).to_bytes());
            (c, s)
        };
        let fetch_other = MapFetcher::new(vec![
            ("https://example.com/catalog.json", catalog2.into_bytes()),
            ("https://example.com/catalog.json.sig", sig2.into_bytes()),
        ]);
        assert!(load_store(&cfg, &fetch_other).is_err());
        // 在线安装 → 成功；更新替换生效
        let base = temp_base("store");
        assert_eq!(install_online(&base, &cfg, &fetch, "demo").unwrap(), "demo");
        assert!(root(&base).join("demo/index.html").is_file());
        let zip2 = build_zip(&[
            ("demo/plugin.json", r#"{"name":"在线演示","version":"2.0"}"#),
            ("demo/index.html", "<html>online v2</html>"),
        ]);
        let sha2 = dhrust::sign::sha256_hex(&zip2);
        let entries2 = format!(
            r#"[{{"id":"demo","name":"在线演示","version":"2.0","url":"https://example.com/demo.zip","sha256":"{sha2}"}}]"#
        );
        let (catalog_v2, sig_v2) = signed_catalog(&entries2);
        let fetch_v2 = MapFetcher::new(vec![
            ("https://example.com/catalog.json", catalog_v2.into_bytes()),
            ("https://example.com/catalog.json.sig", sig_v2.into_bytes()),
            ("https://example.com/demo.zip", zip2),
        ]);
        assert_eq!(
            install_online(&base, &cfg, &fetch_v2, "demo").unwrap(),
            "demo"
        );
        let v2 = fs::read_to_string(root(&base).join("demo/index.html")).unwrap();
        assert!(v2.contains("online v2"), "{v2}");
        // 下载内容被篡改（与 sha256 不符）→ 拒绝
        let fetch_bad = MapFetcher::new(vec![
            ("https://example.com/catalog.json", catalog.into_bytes()),
            ("https://example.com/catalog.json.sig", sig.into_bytes()),
            ("https://example.com/demo.zip", b"tampered bytes".to_vec()),
        ]);
        let err = install_online(&base, &cfg, &fetch_bad, "demo").unwrap_err();
        assert!(err.contains("SHA-256"), "{err}");
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn inspect_package_validates_and_reads_manifest() {
        let flat = build_zip(&[
            ("plugin.json", r#"{"name":"扁平","version":"9"}"#),
            ("index.html", "x"),
        ]);
        let m = inspect_package(&flat, "flatdemo.zip").unwrap();
        assert_eq!(m.id, "flatdemo");
        assert_eq!(m.name, "扁平");
        assert_eq!(m.version, "9");
        let folder = build_zip(&[
            ("d1/plugin.json", r#"{"name":"目录"}"#),
            ("d1/index.html", "x"),
        ]);
        assert_eq!(inspect_package(&folder, "whatever.zip").unwrap().id, "d1");
        let bad = build_zip(&[("readme.txt", "x")]);
        assert!(inspect_package(&bad, "bad.zip").is_err());
    }

    #[test]
    fn publish_flow_updates_catalog_and_signature() {
        let base = temp_base("publish");
        let store = base.join("store");
        let key_file = base.join("k.key");
        fs::write(&key_file, hex_of(&[42u8; 32])).unwrap();
        let zip_file = base.join("demo.zip");
        fs::write(
            &zip_file,
            build_zip(&[
                ("demo/plugin.json", r#"{"name":"发布测试","version":"1.0.0","description":"d"}"#),
                ("demo/index.html", "v1"),
            ]),
        )
        .unwrap();
        let info = publish_to_store(&store, &zip_file, "https://example.com/plugins/", &key_file)
            .unwrap();
        assert_eq!(info.id, "demo");
        assert_eq!(info.file_name, "demo-1.0.0.zip");
        assert!(info.url.ends_with("/demo-1.0.0.zip"));
        let catalog = fs::read_to_string(store.join("catalog.json")).unwrap();
        let j: Json = serde_json::from_str(&catalog).unwrap();
        assert_eq!(j["plugins"][0]["sha256"], info.sha256);
        assert_eq!(j["plugins"][0]["url"], info.url);
        let sig = fs::read(store.join("catalog.json.sig")).unwrap();
        let pubhex = hex_of(test_key().verifying_key().as_bytes());
        assert!(verify_catalog_signature(&pubhex, catalog.as_bytes(), &sig).is_ok());
        // 发布 v2：条目更新、旧版本包清理、base-url 记忆
        let zip2 = base.join("demo2.zip");
        fs::write(
            &zip2,
            build_zip(&[
                ("demo/plugin.json", r#"{"name":"发布测试","version":"2.0.0"}"#),
                ("demo/index.html", "v2"),
            ]),
        )
        .unwrap();
        let info2 = publish_to_store(&store, &zip2, "", &key_file).unwrap();
        assert_eq!(info2.version, "2.0.0");
        assert!(store.join("demo-2.0.0.zip").is_file());
        assert!(!store.join("demo-1.0.0.zip").exists(), "旧版本包应被清理");
        // 下线
        remove_from_store(&store, "demo", &key_file).unwrap();
        let j: Json =
            serde_json::from_str(&fs::read_to_string(store.join("catalog.json")).unwrap()).unwrap();
        assert_eq!(j["plugins"].as_array().unwrap().len(), 0);
        assert!(!store.join("demo-2.0.0.zip").exists());
        assert!(remove_from_store(&store, "missing", &key_file).is_err());
        let _ = fs::remove_dir_all(&base);
    }
}
