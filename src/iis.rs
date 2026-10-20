//! IIS 工作进程（w3wp.exe）信息解析（Windows）。
//!
//! 面板进程表中的 `w3wp.exe` 是 IIS 宿主，exe 路径固定为
//! `C:\Windows\System32\inetsrv\w3wp.exe`，真正的站点在 IIS 配置中。
//! 本模块负责：
//!
//! 1. 读取 w3wp 命令行（`NtQueryInformationProcess`，免 PEB/免子进程）取 `-ap` 应用池；
//! 2. 解析 `%windir%\System32\inetsrv\config\applicationHost.config`，
//!    把应用池映射到站点/应用的物理路径（60 秒缓存）；
//! 3. 命令行取不到时，回退用进程用户（如 `IIS APPPOOL\Pool` / `Pool`）匹配应用池。
//!
//! 非 Windows / 非 IIS 环境返回 `None`，不影响原有进程展示。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::Value;

/// IIS 应用（站点 + 应用路径 + 物理路径）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IisSite {
    pub site: String,
    pub path: String,
    pub physical_path: String,
}

/// 一个 w3wp 工作进程的解析结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IisWorker {
    pub app_pool: String,
    pub sites: Vec<IisSite>,
}

/// 查询指定进程的 IIS 工作进程信息。
///
/// - 非 `w3wp` → `None`；
/// - 命令行/用户都取不到应用池 → `None`；
/// - IIS 配置缺失时仍返回应用池（`sites` 为空）。
pub fn lookup(pid: u32, process_name: &str, user: &str) -> Option<IisWorker> {
    if !is_w3wp(process_name) {
        return None;
    }
    let apps = apps_by_pool_cached();
    let pool = command_line_of(pid)
        .and_then(|cmd| parse_app_pool(&cmd))
        .or_else(|| pool_from_user(user, &apps))?;
    let sites = apps
        .get(&pool.to_ascii_lowercase())
        .cloned()
        .unwrap_or_default();
    Some(IisWorker { app_pool: pool, sites })
}

/// 面板展示用：`(应用/站点短名, 物理路径)`。
///
/// - 单站点/单应用：站点名（子应用为 `站点+路径`）与物理路径；
/// - 多站点共用应用池：`首个站点 等 N 个应用`，物理路径以 `；` 连接。
pub fn display(worker: &IisWorker) -> (String, String) {
    let mut labels: Vec<String> = Vec::new();
    let mut paths: Vec<String> = Vec::new();
    for site in &worker.sites {
        let label = if site.path.is_empty() || site.path == "/" {
            site.site.clone()
        } else {
            format!("{}{}", site.site, site.path)
        };
        if !label.is_empty() && !labels.contains(&label) {
            labels.push(label);
        }
        if !site.physical_path.is_empty() && !paths.contains(&site.physical_path) {
            paths.push(site.physical_path.clone());
        }
    }
    let app = match labels.len() {
        0 => String::new(),
        1 => labels[0].clone(),
        _ => format!("{} 等 {} 个应用", labels[0], labels.len()),
    };
    (app, paths.join("；"))
}

fn is_w3wp(name: &str) -> bool {
    name.trim()
        .trim_end_matches(".exe")
        .eq_ignore_ascii_case("w3wp")
}

/// 从 w3wp 命令行解析 `-ap "应用池"`（也兼容不带引号）。
fn parse_app_pool(command_line: &str) -> Option<String> {
    let lower = command_line.to_ascii_lowercase();
    let mut from = 0;
    while let Some(pos) = lower[from..].find("-ap") {
        let abs = from + pos;
        let before_ok = cmd_char_before(command_line, abs).is_none_or(|c| c.is_whitespace());
        let rest = &command_line[abs + 3..];
        let after_ok = rest
            .chars()
            .next()
            .is_none_or(|c| c.is_whitespace() || c == '"');
        if before_ok && after_ok {
            let rest = rest.trim_start();
            let value = match rest.strip_prefix('"') {
                Some(stripped) => stripped.split('"').next().unwrap_or(""),
                None => rest.split_whitespace().next().unwrap_or(""),
            };
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
        from = abs + 3;
    }
    None
}

fn cmd_char_before(text: &str, byte_index: usize) -> Option<char> {
    text.get(..byte_index)
        .and_then(|s| s.chars().next_back())
}

/// 用进程用户匹配应用池：`IIS APPPOOL\Pool` → `Pool`。
fn pool_from_user(user: &str, apps: &HashMap<String, Vec<IisSite>>) -> Option<String> {
    let candidate = user.rsplit(['\\', '/']).next().unwrap_or(user).trim();
    if candidate.is_empty() {
        return None;
    }
    apps.get(&candidate.to_ascii_lowercase())
        .map(|_| candidate.to_string())
}

/// 应用池 → 应用列表缓存（60 秒；IIS 配置修改后最多延迟一分钟）。
static CACHE: Mutex<Option<(Instant, HashMap<String, Vec<IisSite>>)>> = Mutex::new(None);
const CACHE_TTL: Duration = Duration::from_secs(60);

fn apps_by_pool_cached() -> HashMap<String, Vec<IisSite>> {
    let mut slot = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((at, map)) = slot.as_ref() {
        if at.elapsed() < CACHE_TTL {
            return map.clone();
        }
    }
    let map = read_apps_config()
        .map(|text| apps_by_pool(&text))
        .unwrap_or_default();
    *slot = Some((Instant::now(), map.clone()));
    map
}

#[cfg(windows)]
fn read_apps_config() -> Option<String> {
    read_config_with_path().map(|(_, text)| text)
}

/// 读取实际生效的 applicationHost.config（含 IIS 共享配置转发），返回路径与内容。
#[cfg(windows)]
fn read_config_with_path() -> Option<(PathBuf, String)> {
    let root = std::env::var_os("windir")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(r"C:\Windows"));
    let dir = root.join("System32").join("inetsrv").join("config");
    let local = dir.join("applicationHost.config");
    let text = std::fs::read_to_string(&local).ok()?;
    if text.contains("system.applicationHost") {
        return Some((local, text));
    }
    // IIS 共享配置：本地 applicationHost.config 只是转发文件，
    // 真正的站点配置由 redirection.config 指向（常见于多机统一管理）。
    let redirect = std::fs::read_to_string(dir.join("redirection.config")).ok()?;
    let value = dhrust::config::read_xml_to_json(&redirect).ok()?;
    let shared = shared_config_path(&value)?;
    let path = PathBuf::from(expand_env_vars(&shared));
    let text = std::fs::read_to_string(&path).ok()?;
    Some((path, text))
}

/// 从 `redirection.config` 提取共享配置路径（`<redirection><configFile path="..."/>`）。
#[cfg_attr(not(windows), allow(dead_code))]
fn shared_config_path(value: &Value) -> Option<String> {
    let root = value.get("configuration").unwrap_or(value);
    let file = root
        .get("redirection")
        .and_then(|r| r.get("configFile"))
        .or_else(|| root.get("configFile"))?;
    for item in as_values(file) {
        let path = str_attr(item, "path");
        if !path.is_empty() {
            return Some(path);
        }
        if let Some(text) = item.as_str() {
            if !text.trim().is_empty() {
                return Some(text.trim().to_string());
            }
        }
        if let Some(text) = item.get("#text").and_then(|v| v.as_str()) {
            if !text.trim().is_empty() {
                return Some(text.trim().to_string());
            }
        }
    }
    None
}

#[cfg(not(windows))]
fn read_apps_config() -> Option<String> {
    None
}

/// IIS 站点日志配置（`<siteDefaults>` 继承 + 环境变量展开）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IisLogSite {
    pub name: String,
    pub id: String,
    /// 日志根目录（未展开 `<目录>\W3SVC<站点Id>` 子目录）
    pub dir: PathBuf,
    /// 日志格式：`W3C`（默认）/ `IIS` / `NCSA`
    pub format: String,
    /// 站点日志是否启用（`<logFile enabled="false">` 为关闭）
    pub enabled: bool,
    /// 有效日志字段（`logExtFileFlags`，逗号分隔；空 = IIS 默认字段，不含 BytesSent）
    pub flags: String,
}

/// 自动发现结果：可统计的站点给出日志文件，其余给出站点名与原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredLog {
    pub name: String,
    pub file: Option<PathBuf>,
    pub note: String,
    pub id: String,
    pub enabled: bool,
    /// IIS 运行状态（`Started` / `Stopped` / 空 = 未知）
    pub state: String,
    /// 有效日志字段是否包含 `BytesSent`（sc-bytes）
    pub sc_bytes: bool,
    pub format: String,
}

/// 自动发现 IIS 站点日志（Windows）。
///
/// 返回**配置中的全部站点**：有日志文件的给出文件路径；日志关闭、格式不支持、
/// 目录/文件尚未产生时给出 `file=None` 与原因——面板据此仍能列出全部站点，
/// 而不是只剩“有日志文件的 Default Web Site”。
#[cfg(windows)]
pub fn discover_log_sites() -> Vec<DiscoveredLog> {
    let default_root = default_log_root();
    let mut sites = read_apps_config()
        .map(|text| log_sites_from_xml(&text, &default_root))
        .unwrap_or_default();
    if sites.is_empty() {
        // 配置读不到时回退扫描默认/旧版 IIS 日志目录（W3SVC*）
        let legacy_root = std::env::var_os("windir")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Windows"))
            .join("System32")
            .join("LogFiles");
        for root in [default_root, legacy_root] {
            let Ok(entries) = std::fs::read_dir(&root) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let name = entry.file_name().to_string_lossy().to_string();
                if path.is_dir() && name.to_ascii_lowercase().starts_with("w3svc") {
                    sites.push(IisLogSite {
                        name,
                        id: String::new(),
                        dir: path,
                        format: "W3C".to_string(),
                        enabled: true,
                        flags: String::new(),
                    });
                }
            }
        }
    }
    let states = site_states();
    sites
        .into_iter()
        .map(|site| describe_log_site_with_state(site, &states))
        .collect()
}

#[cfg(windows)]
fn describe_log_site_with_state(
    site: IisLogSite,
    states: &HashMap<String, String>,
) -> DiscoveredLog {
    let mut note = String::new();
    let mut file = None;
    // IIS 在配置目录下再建 `W3SVC<站点Id>` 子目录；部分环境把站点目录直接指向子目录，
    // 两种布局都尝试（先子目录、后根目录），避免自定义日志目录被漏掉。
    let mut candidates: Vec<PathBuf> = Vec::new();
    if !site.id.trim().is_empty() {
        candidates.push(site.dir.join(format!("W3SVC{}", site.id.trim())));
    }
    candidates.push(site.dir.clone());
    if !site.enabled {
        note = "IIS 日志记录已关闭（需在 IIS 管理器中启用）".to_string();
    } else if !supported_format(&site.format) {
        note = format!("日志格式为 {}（需改为 W3C 才能按字段统计）", site.format);
    } else if let Some(path) = candidates.iter().find_map(|dir| latest_log_file(dir)) {
        file = Some(path);
    } else if candidates.iter().any(|dir| dir.is_dir()) {
        note = format!("暂无日志文件（{}）", site.dir.display());
    } else {
        note = format!("日志目录不存在（{}）", site.dir.display());
    }
    DiscoveredLog {
        name: site.name.clone(),
        file,
        note,
        id: site.id,
        enabled: site.enabled,
        state: states.get(&site.name).cloned().unwrap_or_default(),
        sc_bytes: has_flag(&site.flags, "BytesSent"),
        format: site.format,
    }
}

/// W3C/NCSA 可直接解析；IIS 原生逗号格式暂不支持。
#[cfg_attr(not(windows), allow(dead_code))]
fn supported_format(format: &str) -> bool {
    let f = format.trim();
    f.is_empty() || f.eq_ignore_ascii_case("W3C") || f.eq_ignore_ascii_case("NCSA")
}

/// 日志字段列表中是否包含指定标志（IIS 用 `BytesSent` 表示 sc-bytes）。
#[cfg_attr(not(windows), allow(dead_code))]
fn has_flag(flags: &str, name: &str) -> bool {
    flags
        .split(',')
        .any(|f| f.trim().eq_ignore_ascii_case(name))
}

/// IIS 默认 W3C 字段（不含 BytesSent；用于未显式配置字段时补齐）。
const DEFAULT_LOG_FLAGS: &str = "Date, Time, ClientIP, UserName, ServerIP, Method, UriStem, UriQuery, TimeTaken, HttpStatus, Win32Status, ServerPort, UserAgent, HttpSubStatus, Referer";

/// 追加 `BytesSent`（sc-bytes）；字段为空时先补 IIS 默认字段，避免只剩字节列。
#[cfg_attr(not(windows), allow(dead_code))]
fn flags_with_bytes(flags: &str) -> String {
    if has_flag(flags, "BytesSent") {
        return flags.trim().to_string();
    }
    if flags.trim().is_empty() {
        format!("{DEFAULT_LOG_FLAGS}, BytesSent")
    } else {
        format!("{}, BytesSent", flags.trim())
    }
}

/// 站点运行状态缓存（15 秒；面板 3 秒轮询不重复拉起 appcmd）。
#[cfg(windows)]
static STATE_CACHE: Mutex<Option<(Instant, HashMap<String, String>)>> = Mutex::new(None);
#[cfg(windows)]
const STATE_TTL: Duration = Duration::from_secs(15);

/// 查询 IIS 站点运行状态（`Started` / `Stopped`；未知/非 Windows 返回空）。
#[cfg(windows)]
pub fn site_states() -> HashMap<String, String> {
    {
        let slot = STATE_CACHE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((at, map)) = slot.as_ref() {
            if at.elapsed() < STATE_TTL {
                return map.clone();
            }
        }
    }
    let map = query_site_states();
    let mut slot = STATE_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    *slot = Some((Instant::now(), map.clone()));
    map
}

#[cfg(not(windows))]
#[allow(dead_code)]
pub fn site_states() -> HashMap<String, String> {
    HashMap::new()
}

#[cfg(windows)]
fn query_site_states() -> HashMap<String, String> {
    let Some(appcmd) = appcmd_path() else {
        return HashMap::new();
    };
    let args = vec!["list".to_string(), "sites".to_string(), "/xml".to_string()];
    let Ok(out) = run_hidden(&appcmd, &args) else {
        return HashMap::new();
    };
    let map = parse_site_states_xml(&out);
    if !map.is_empty() {
        return map;
    }
    parse_site_states_text(&out)
}

/// 解析 `appcmd list sites /xml`（`<SITE SITE.NAME="x" state="Started"/>`）。
#[cfg_attr(not(windows), allow(dead_code))]
fn parse_site_states_xml(xml: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let Ok(value) = dhrust::config::read_xml_to_json(xml) else {
        return out;
    };
    let root = value.get("appcmd").unwrap_or(&value);
    let Some(list) = root.get("SITE").or_else(|| root.get("site")) else {
        return out;
    };
    for item in as_values(list) {
        let mut name = str_attr(item, "SITE.NAME");
        if name.is_empty() {
            name = str_attr(item, "site.name");
        }
        let state = str_attr(item, "state");
        if !name.is_empty() && !state.is_empty() {
            out.insert(name, state);
        }
    }
    out
}

/// 解析 `appcmd list sites` 文本（`SITE "name" (id:1,...,state:Started)`）。
#[cfg_attr(not(windows), allow(dead_code))]
fn parse_site_states_text(text: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("SITE ") else {
            continue;
        };
        let Some(open) = rest.find('"') else {
            continue;
        };
        let rest = &rest[open + 1..];
        let Some(close) = rest.find('"') else {
            continue;
        };
        let name = &rest[..close];
        let tail = &rest[close + 1..];
        let Some(pos) = tail.rfind("state:") else {
            continue;
        };
        let state = tail[pos + 6..]
            .split([',', ')'])
            .next()
            .unwrap_or("")
            .trim();
        if !name.is_empty() && !state.is_empty() {
            out.insert(name.to_string(), state.to_string());
        }
    }
    out
}

/// 为 IIS 站点启用日志记录并勾选 `BytesSent`（sc-bytes）。`None` = 全部站点。
#[cfg(windows)]
pub fn enable_sc_bytes(site: Option<&str>) -> Result<String, String> {
    let default_root = default_log_root();
    let Some((config_path, xml)) = read_config_with_path() else {
        return Err("读取 IIS 配置失败（applicationHost.config）".to_string());
    };
    let sites = log_sites_from_xml(&xml, &default_root);
    let targets: Vec<&IisLogSite> = match site {
        Some(name) => sites
            .iter()
            .filter(|s| s.name.eq_ignore_ascii_case(name.trim()))
            .collect(),
        None => sites.iter().collect(),
    };
    if targets.is_empty() {
        return Err(match site {
            Some(name) => format!("未找到 IIS 站点：{name}"),
            None => "未找到任何 IIS 站点".to_string(),
        });
    }
    let Some(appcmd) = appcmd_path() else {
        return Err("未找到 appcmd.exe（IIS 未安装？）".to_string());
    };
    // 修改前留一份覆盖式备份（失败不阻断；IIS 自身也有配置历史）
    let backup = config_path.with_extension("config.pek-bak");
    let _ = std::fs::copy(&config_path, &backup);

    let mut changed = 0usize;
    let mut skipped = 0usize;
    let mut errors: Vec<String> = Vec::new();
    for s in targets {
        if s.name.contains('\'') || s.name.contains(']') {
            errors.push(format!(
                "{}：站点名含单引号/方括号，请手动在 IIS 中勾选",
                s.name
            ));
            continue;
        }
        if !supported_format(&s.format) {
            if site.is_some() {
                return Err(format!(
                    "站点 {} 的日志格式为 {}，请先在 IIS 中改为 W3C 再启用",
                    s.name, s.format
                ));
            }
            skipped += 1;
            continue;
        }
        let need_log = !s.enabled;
        let w3c = s.format.trim().is_empty() || s.format.eq_ignore_ascii_case("W3C");
        let need_bytes = w3c && !has_flag(&s.flags, "BytesSent");
        if !need_log && !need_bytes {
            skipped += 1;
            continue;
        }
        if need_log {
            let filter = format!("/[name='{}'].logFile.enabled:true", s.name);
            if let Err(e) = run_hidden(
                &appcmd,
                &[
                    "set".to_string(),
                    "config".to_string(),
                    "-section:system.applicationHost/sites".to_string(),
                    filter,
                    "/commit:apphost".to_string(),
                ],
            ) {
                errors.push(format!("{}：{e}", s.name));
                continue;
            }
        }
        if need_bytes {
            let filter = format!(
                "/[name='{}'].logFile.logExtFileFlags:{}",
                s.name,
                flags_with_bytes(&s.flags)
            );
            if let Err(e) = run_hidden(
                &appcmd,
                &[
                    "set".to_string(),
                    "config".to_string(),
                    "-section:system.applicationHost/sites".to_string(),
                    filter,
                    "/commit:apphost".to_string(),
                ],
            ) {
                errors.push(format!("{}：{e}", s.name));
                continue;
            }
        }
        changed += 1;
    }
    invalidate_cache();
    if !errors.is_empty() {
        return Err(format!(
            "已处理 {changed} 个站点，失败 {} 个：{}",
            errors.len(),
            errors.join("；")
        ));
    }
    let tail = if changed > 0 {
        "（当前日志文件的字段表不会重写，新字段从 IIS 新建日志文件后生效，通常次日或日志滚动后）"
    } else {
        ""
    };
    Ok(format!(
        "已为 {changed} 个站点启用日志/sc-bytes，{skipped} 个无需修改{tail}"
    ))
}

#[cfg(not(windows))]
#[allow(dead_code)]
pub fn enable_sc_bytes(_site: Option<&str>) -> Result<String, String> {
    Err("仅 Windows/IIS 支持此操作".to_string())
}

/// 清除应用池/站点状态缓存（IIS 配置修改后调用）。
#[cfg_attr(not(windows), allow(dead_code))]
pub fn invalidate_cache() {
    *CACHE.lock().unwrap_or_else(|e| e.into_inner()) = None;
    #[cfg(windows)]
    {
        *STATE_CACHE.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

#[cfg(windows)]
fn appcmd_path() -> Option<PathBuf> {
    let root = std::env::var_os("windir")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
    let path = root
        .join("System32")
        .join("inetsrv")
        .join("appcmd.exe");
    path.is_file().then_some(path)
}

/// 执行命令并捕获输出（Windows：不弹控制台窗口）。
#[cfg(windows)]
fn run_hidden(program: &Path, args: &[String]) -> Result<String, String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let out = std::process::Command::new(program)
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| format!("启动 {} 失败：{e}", program.display()))?;
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        let msg = if stderr.trim().is_empty() { stdout } else { stderr };
        let name = program
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        return Err(format!("{name} 执行失败（{}）：{}", out.status, msg.trim()));
    }
    Ok(stdout)
}

#[cfg(not(windows))]
#[allow(dead_code)]
pub fn discover_log_sites() -> Vec<DiscoveredLog> {
    Vec::new()
}

#[cfg(windows)]
fn default_log_root() -> PathBuf {
    let drive = std::env::var_os("SystemDrive")
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "C:".to_string());
    PathBuf::from(format!(r"{drive}\inetpub\logs\LogFiles"))
}

/// 从 applicationHost.config 提取全部站点日志配置（可单测的纯逻辑）。
///
/// 关键点：
/// - site 级 `<logFile>` 常只写部分属性（例如只有 `logFormat`），其余从
///   `<siteDefaults><logFile>` 继承（目录默认 `%SystemDrive%\inetpub\logs\LogFiles`）；
/// - `%VAR%` 环境变量展开（IIS 默认值就是 `%SystemDrive%\...`）；
/// - **不因日志文件不存在而丢弃站点**（站点停用/新站尚未产生日志是常态）。
#[cfg_attr(not(windows), allow(dead_code))]
fn log_sites_from_xml(xml: &str, default_root: &Path) -> Vec<IisLogSite> {
    let mut out = Vec::new();
    let Ok(value) = dhrust::config::read_xml_to_json(xml) else {
        return out;
    };
    let root = value.get("configuration").unwrap_or(&value);
    let Some(host) = root.get("system.applicationHost") else {
        return out;
    };
    let Some(sites) = host.get("sites") else {
        return out;
    };
    let defaults = sites.get("siteDefaults").and_then(|d| d.get("logFile"));
    let default_dir = defaults
        .map(|l| str_attr(l, "directory"))
        .unwrap_or_default();
    let default_format = defaults
        .map(|l| str_attr(l, "logFormat"))
        .unwrap_or_default();
    let default_enabled = defaults.map(|l| str_attr(l, "enabled")).unwrap_or_default();
    let default_flags = defaults
        .map(|l| str_attr(l, "logExtFileFlags"))
        .unwrap_or_default();
    let Some(list) = sites.get("site") else {
        return out;
    };
    for site in as_values(list) {
        let name = str_attr(site, "name");
        if name.is_empty() {
            continue;
        }
        let id = str_attr(site, "id");
        let log = site.get("logFile");
        let pick = |key: &str, fallback: &str| -> String {
            log.map(|l| str_attr(l, key))
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| fallback.to_string())
        };
        out.push(IisLogSite {
            name,
            id,
            dir: resolve_log_dir(&pick("directory", &default_dir), default_root),
            format: pick("logFormat", &default_format),
            enabled: !pick("enabled", &default_enabled).eq_ignore_ascii_case("false"),
            flags: pick("logExtFileFlags", &default_flags),
        });
    }
    out
}

/// 日志根目录：空 → 默认根；支持 `%VAR%` 与相对路径。
///
/// 站点实际日志目录通常是 `<根目录>\W3SVC<站点Id>`（在 `describe_log_site` 中展开）。
#[cfg_attr(not(windows), allow(dead_code))]
fn resolve_log_dir(directory: &str, default_root: &Path) -> PathBuf {
    let expanded = expand_env_vars(directory.trim());
    if expanded.trim().is_empty() {
        return default_root.to_path_buf();
    }
    let path = PathBuf::from(expanded);
    if path.is_absolute() {
        path
    } else {
        default_root.join(path)
    }
}

/// 展开 `%NAME%` 环境变量（未知名原样保留）。
#[cfg_attr(not(windows), allow(dead_code))]
fn expand_env_vars(text: &str) -> String {
    expand_env_vars_with(text, |name| std::env::var(name).ok())
}

#[cfg_attr(not(windows), allow(dead_code))]
fn expand_env_vars_with<F>(text: &str, lookup: F) -> String
where
    F: Fn(&str) -> Option<String>,
{
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        let Some(ch) = text[i..].chars().next() else {
            break;
        };
        if ch != '%' {
            out.push(ch);
            i += ch.len_utf8();
            continue;
        }
        let Some(rel) = text[i + 1..].find('%') else {
            out.push('%');
            i += 1;
            continue;
        };
        let end = i + 1 + rel;
        let name = &text[i + 1..end];
        let valid = !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '(' || c == ')');
        if let (true, Some(value)) = (valid, lookup(name)) {
            out.push_str(&value);
            i = end + 1;
        } else {
            out.push('%');
            i += 1;
        }
    }
    out
}

/// 目录里最近修改的 `.log` 文件（IIS 按日期滚动，取最新即当前文件）。
#[cfg_attr(not(windows), allow(dead_code))]
fn latest_log_file(dir: &Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let is_log = path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("log"));
        if !is_log {
            continue;
        }
        let modified = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::UNIX_EPOCH);
        if best.as_ref().is_none_or(|(at, _)| modified >= *at) {
            best = Some((modified, path));
        }
    }
    best.map(|(_, path)| path)
}

/// 解析 applicationHost.config：应用池 → `(站点, 应用路径, 物理路径)`。
///
/// 只关心 `<sites><site><application applicationPool=...>` 结构；
/// `<location>` 等其它节点忽略。XML 解析复用 `dhrust::config::read_xml_to_json`。
fn apps_by_pool(xml: &str) -> HashMap<String, Vec<IisSite>> {
    let mut out: HashMap<String, Vec<IisSite>> = HashMap::new();
    let Ok(value) = dhrust::config::read_xml_to_json(xml) else {
        return out;
    };
    let root = value.get("configuration").unwrap_or(&value);
    let Some(host) = root.get("system.applicationHost") else {
        return out;
    };
    let Some(sites) = host.get("sites").and_then(|v| v.get("site")) else {
        return out;
    };
    for site in as_values(sites) {
        let site_name = str_attr(site, "name");
        let Some(applications) = site.get("application") else {
            continue;
        };
        for app in as_values(applications) {
            let pool = str_attr(app, "applicationPool");
            if pool.is_empty() {
                continue;
            }
            let path = {
                let p = str_attr(app, "path");
                if p.is_empty() { "/".to_string() } else { p }
            };
            out.entry(pool.to_ascii_lowercase())
                .or_default()
                .push(IisSite {
                    site: site_name.clone(),
                    path,
                    physical_path: app_physical_path(app),
                });
        }
    }
    out
}

fn app_physical_path(app: &Value) -> String {
    let Some(value) = app.get("virtualDirectory") else {
        return String::new();
    };
    let list = as_values(value);
    let root = list.iter().find(|vd| {
        let p = str_attr(vd, "path");
        p.is_empty() || p == "/"
    });
    root.or_else(|| list.first())
        .map(|vd| str_attr(vd, "physicalPath"))
        .unwrap_or_default()
}

fn as_values(value: &Value) -> Vec<&Value> {
    value
        .as_array()
        .map(|list| list.iter().collect())
        .unwrap_or_else(|| vec![value])
}

fn str_attr(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string()
}

/// 读取进程命令行（Windows，`NtQueryInformationProcess`）。
#[cfg(windows)]
fn command_line_of(pid: u32) -> Option<String> {
    use std::ffi::c_void;
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    #[repr(C)]
    struct UnicodeString {
        length: u16,
        maximum_length: u16,
        buffer: *mut u16,
    }

    #[link(name = "ntdll")]
    unsafe extern "system" {
        fn NtQueryInformationProcess(
            process_handle: *mut c_void,
            process_information_class: u32,
            process_information: *mut c_void,
            process_information_length: u32,
            return_length: *mut u32,
        ) -> i32;
    }

    // ProcessCommandLineInformation（Windows 8.1+）
    const PROCESS_COMMAND_LINE_INFORMATION: u32 = 60;

    unsafe {
        let mut handle = OpenProcess(
            PROCESS_QUERY_INFORMATION | PROCESS_QUERY_LIMITED_INFORMATION,
            0,
            pid,
        );
        if handle.is_null() {
            handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        }
        if handle.is_null() {
            return None;
        }
        let result = (|| {
            let mut needed = 0u32;
            let status = NtQueryInformationProcess(
                handle,
                PROCESS_COMMAND_LINE_INFORMATION,
                std::ptr::null_mut(),
                0,
                &mut needed,
            );
            if status >= 0 {
                return None;
            }
            // 少数系统首次调用不返回长度：给一个足够大的回退缓冲区
            let size = if needed == 0 { 64 * 1024 } else { needed };
            let words = (size as usize + 7) / 8 + 4;
            let mut buf = vec![0u64; words];
            let mut got = 0u32;
            let status = NtQueryInformationProcess(
                handle,
                PROCESS_COMMAND_LINE_INFORMATION,
                buf.as_mut_ptr() as *mut c_void,
                (buf.len() * 8) as u32,
                &mut got,
            );
            if status < 0 {
                return None;
            }
            let us = std::ptr::read_unaligned(buf.as_ptr() as *const UnicodeString);
            let bytes = us.length as usize;
            if us.buffer.is_null() || bytes == 0 || bytes % 2 != 0 {
                return None;
            }
            let base = buf.as_ptr() as usize;
            let start = us.buffer as usize;
            if start < base || start.saturating_add(bytes) > base + buf.len() * 8 {
                return None;
            }
            let chars = std::slice::from_raw_parts(us.buffer, bytes / 2);
            Some(String::from_utf16_lossy(chars))
        })();
        CloseHandle(handle);
        result
    }
}

#[cfg(not(windows))]
fn command_line_of(_pid: u32) -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<configuration>
  <system.applicationHost>
    <sites>
      <site name="gicisky.net" id="1">
        <application path="/" applicationPool="gicisky.net">
          <virtualDirectory path="/" physicalPath="D:\wwwroot\gicisky.net" />
        </application>
        <application path="/api" applicationPool="gicisky.net">
          <virtualDirectory path="/" physicalPath="D:\wwwroot\gicisky-api" />
        </application>
      </site>
      <site name="ykt.hlktech.com" id="2">
        <application path="/" applicationPool="ykt">
          <virtualDirectory path="/" physicalPath="D:\wwwroot\ykt" />
        </application>
      </site>
    </sites>
  </system.applicationHost>
</configuration>"#;

    #[test]
    fn parses_app_pool_from_command_line() {
        let cmd = r#""C:\Windows\System32\inetsrv\w3wp.exe" -ap "gicisky.net" -v "v4.0""#;
        assert_eq!(parse_app_pool(cmd).as_deref(), Some("gicisky.net"));
        assert_eq!(
            parse_app_pool(r#"w3wp.exe -ap DefaultAppPool -v v4.0"#).as_deref(),
            Some("DefaultAppPool")
        );
        assert_eq!(parse_app_pool(r#"w3wp.exe -v v4.0"#), None);
    }

    #[test]
    fn maps_pool_to_sites_and_physical_paths() {
        let map = apps_by_pool(SAMPLE);
        let apps = map.get("gicisky.net").expect("pool missing");
        assert_eq!(apps.len(), 2);
        assert_eq!(apps[0].site, "gicisky.net");
        assert_eq!(apps[0].path, "/");
        assert_eq!(apps[0].physical_path, r"D:\wwwroot\gicisky.net");
        assert_eq!(apps[1].path, "/api");
        assert_eq!(apps[1].physical_path, r"D:\wwwroot\gicisky-api");

        let ykt = map.get("ykt").expect("pool missing");
        assert_eq!(ykt.len(), 1);
        assert_eq!(ykt[0].physical_path, r"D:\wwwroot\ykt");
    }

    #[test]
    fn display_uses_site_name_and_joins_paths() {
        let worker = IisWorker {
            app_pool: "gicisky.net".to_string(),
            sites: vec![
                IisSite {
                    site: "gicisky.net".to_string(),
                    path: "/".to_string(),
                    physical_path: r"D:\wwwroot\gicisky.net".to_string(),
                },
                IisSite {
                    site: "gicisky.net".to_string(),
                    path: "/api".to_string(),
                    physical_path: r"D:\wwwroot\gicisky-api".to_string(),
                },
            ],
        };
        let (app, path) = display(&worker);
        assert_eq!(app, "gicisky.net 等 2 个应用");
        assert_eq!(path, r"D:\wwwroot\gicisky.net；D:\wwwroot\gicisky-api");
    }

    #[test]
    fn maps_sites_to_iis_log_dirs() {
        let xml = r#"<configuration><system.applicationHost><sites>
            <site name="site1" id="1" />
            <site name="site2" id="2"><logFile directory="custom" /></site>
            <site name="site3" id="3"><logFile directory="off" enabled="false" /></site>
        </sites></system.applicationHost></configuration>"#;
        let root = Path::new("/logs");
        let sites = log_sites_from_xml(xml, root);
        // 日志关闭的站点也要保留（面板仍列出，附原因），不再直接丢弃
        assert_eq!(sites.len(), 3);
        assert_eq!(sites[0].name, "site1");
        assert_eq!(sites[0].id, "1");
        assert_eq!(sites[0].dir, root.to_path_buf());
        assert!(sites[0].enabled);
        assert_eq!(sites[1].name, "site2");
        assert_eq!(sites[1].dir, root.join("custom"));
        assert_eq!(sites[2].name, "site3");
        assert!(!sites[2].enabled);
    }

    #[test]
    fn inherits_site_defaults_and_site_overrides() {
        let xml = r#"<configuration><system.applicationHost><sites>
            <siteDefaults>
                <logFile logFormat="NCSA" />
            </siteDefaults>
            <site name="Default Web Site" id="1" />
            <site name="shop.example.com" id="2"><logFile logFormat="W3C" /></site>
            <site name="api.example.com" id="3"><logFile directory="D:\weblogs\api" /></site>
        </sites></system.applicationHost></configuration>"#;
        let root = Path::new("/fallback");
        let sites = log_sites_from_xml(xml, root);
        assert_eq!(sites.len(), 3);
        // 未写 logFile 的站点继承 siteDefaults 的格式与目录（目录为空 → 默认根）
        assert_eq!(sites[0].name, "Default Web Site");
        assert_eq!(sites[0].format, "NCSA");
        assert_eq!(sites[0].dir, root.to_path_buf());
        // 站点级 logFormat 覆盖默认值
        assert_eq!(sites[1].format, "W3C");
        // 站点级 directory 覆盖默认值
        assert_eq!(sites[2].format, "NCSA");
        #[cfg(windows)]
        assert_eq!(sites[2].dir, PathBuf::from(r"D:\weblogs\api"));
        #[cfg(not(windows))]
        assert_eq!(sites[2].dir, root.join(r"D:\weblogs\api"));
    }

    #[cfg(windows)]
    #[test]
    fn inherits_site_defaults_directory_and_expands_env() {
        // IIS 默认配置：只有 siteDefaults 写 directory（%SystemDrive%），站点不写 logFile
        let xml = r#"<configuration><system.applicationHost><sites>
            <siteDefaults><logFile directory="%SystemDrive%\inetpub\logs\LogFiles" /></siteDefaults>
            <site name="Default Web Site" id="1" />
            <site name="shop.example.com" id="2" />
        </sites></system.applicationHost></configuration>"#;
        let sites = log_sites_from_xml(xml, Path::new(r"C:\fallback"));
        let drive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".to_string());
        let want = PathBuf::from(format!(r"{drive}\inetpub\logs\LogFiles"));
        assert_eq!(sites.len(), 2);
        assert_eq!(sites[0].dir, want);
        assert_eq!(sites[1].dir, want);
    }

    #[cfg(windows)]
    #[test]
    fn finds_log_in_w3svc_subdir_or_base_dir() {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let base = std::env::temp_dir().join(format!("pek-iis-log-{}-{stamp}", std::process::id()));
        let sub = base.join("W3SVC7");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("u_ex251020.log"), "#Fields: date time\n").unwrap();
        let found = describe_log_site_with_state(
            IisLogSite {
                name: "shop".to_string(),
                id: "7".to_string(),
                dir: base.clone(),
                format: "W3C".to_string(),
                enabled: true,
                flags: String::new(),
            },
            &HashMap::new(),
        );
        assert_eq!(found.file, Some(sub.join("u_ex251020.log")));
        assert!(found.note.is_empty());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn expands_known_env_vars_in_log_dir() {
        assert_eq!(
            expand_env_vars_with(r"%DRIVE%\logs\%SITE%", |name| match name {
                "DRIVE" => Some("D:".to_string()),
                "SITE" => Some("shop".to_string()),
                _ => None,
            }),
            r"D:\logs\shop"
        );
        // 未定义变量与落单百分号原样保留
        assert_eq!(expand_env_vars_with("100% %NOPE%", |_| None), "100% %NOPE%");
        assert_eq!(expand_env_vars_with(r"C:\a\b", |_| None), r"C:\a\b");
    }

    #[test]
    fn unsupported_log_format_is_flagged() {
        assert!(supported_format("W3C"));
        assert!(supported_format("ncsa"));
        assert!(supported_format(""));
        assert!(!supported_format("IIS"));
    }

    #[test]
    fn parses_shared_config_redirection() {
        let xml = r#"<configuration><redirection><configFile path="\\fs01\iis\applicationHost.config" /></redirection></configuration>"#;
        let value = dhrust::config::read_xml_to_json(xml).unwrap();
        assert_eq!(
            shared_config_path(&value).as_deref(),
            Some(r"\\fs01\iis\applicationHost.config")
        );
        // 兼容 <configuration><configFile path="..."/></configuration> 形式
        let xml2 = r#"<configuration><configFile path="D:\iis\applicationHost.config" /></configuration>"#;
        let value2 = dhrust::config::read_xml_to_json(xml2).unwrap();
        assert_eq!(
            shared_config_path(&value2).as_deref(),
            Some(r"D:\iis\applicationHost.config")
        );
    }

    #[test]
    fn log_flags_inherit_and_detect_bytes_sent() {
        let xml = r#"<configuration><system.applicationHost><sites>
            <siteDefaults><logFile logExtFileFlags="Date, Time, ClientIP" /></siteDefaults>
            <site name="a" id="1" />
            <site name="b" id="2"><logFile logExtFileFlags="Date, Time, BytesSent" /></site>
        </sites></system.applicationHost></configuration>"#;
        let sites = log_sites_from_xml(xml, Path::new("/logs"));
        assert_eq!(sites[0].flags, "Date, Time, ClientIP");
        assert!(!has_flag(&sites[0].flags, "BytesSent"));
        assert!(has_flag(&sites[1].flags, "BytesSent"));
        assert_eq!(
            flags_with_bytes("Date, Time, ClientIP"),
            "Date, Time, ClientIP, BytesSent"
        );
        // 字段为空时补 IIS 默认字段，避免只勾选 BytesSent 丢掉日期/时间等列
        assert_eq!(
            flags_with_bytes(""),
            format!("{DEFAULT_LOG_FLAGS}, BytesSent")
        );
        // 已包含时不重复追加
        assert_eq!(
            flags_with_bytes("Date, BytesSent"),
            "Date, BytesSent"
        );
    }

    #[test]
    fn parses_site_states_from_appcmd() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<appcmd>
  <SITE SITE.NAME="Default Web Site" SITE.ID="1" state="Started" />
  <SITE SITE.NAME="shop" SITE.ID="2" state="Stopped" />
</appcmd>"#;
        let map = parse_site_states_xml(xml);
        assert_eq!(
            map.get("Default Web Site").map(String::as_str),
            Some("Started")
        );
        assert_eq!(map.get("shop").map(String::as_str), Some("Stopped"));

        let text = "SITE \"Default Web Site\" (id:1,bindings:http/*:80:,state:Started)\r\nSITE \"shop\" (id:2,state:Stopped)";
        let map = parse_site_states_text(text);
        assert_eq!(map.get("shop").map(String::as_str), Some("Stopped"));
    }

    #[test]
    fn falls_back_to_process_user_pool_name() {
        let map = apps_by_pool(SAMPLE);
        assert_eq!(
            pool_from_user(r"IIS APPPOOL\gicisky.net", &map).as_deref(),
            Some("gicisky.net")
        );
        assert_eq!(
            pool_from_user("ykt", &map).as_deref(),
            Some("ykt")
        );
        assert_eq!(pool_from_user(r"NT AUTHORITY\SYSTEM", &map), None);
    }

    /// Windows 实机验证 NtQueryInformationProcess 命令行读取（不依赖 IIS）。
    #[cfg(windows)]
    #[test]
    fn reads_current_process_command_line() {
        let cmd = command_line_of(std::process::id()).expect("command line should be readable");
        assert!(!cmd.trim().is_empty());
    }
}
