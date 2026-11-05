//! IIS 站点日志发现与面板辅助（Windows）。
//!
//! 通用 IIS 能力（`applicationHost.config` 解析、站点/应用池/物理路径反查、
//! w3wp ↔ 应用池映射、appcmd 控制）已下沉到 `dhrust::sys::iis`；
//! 本模块只保留 RAgent 专属的：
//!
//! - 网站日志发现（`discover_log_sites`，供 `weblog` 流量统计）；
//! - IIS 日志字段 `sc-bytes` 的在线启用（`enable_sc_bytes`）；
//! - 面板进程/站点展示（`lookup` / `display`）。
//!
//! 非 Windows / 非 IIS 环境返回空或 `None`，不影响原有进程展示。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
#[cfg(windows)]
use std::time::Duration;

use dhrust::sys::iis;
use serde_json::Value;

pub use dhrust::sys::iis::IisWorker;

/// 查询指定进程的 IIS 工作进程信息。
///
/// - 非 `w3wp` → `None`；
/// - 命令行/用户都取不到应用池 → `None`；
/// - IIS 配置缺失时仍返回应用池（`sites` 为空）。
pub fn lookup(pid: u32, process_name: &str, user: &str) -> Option<IisWorker> {
    iis::lookup_worker(pid, process_name, user)
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
    let mut sites = iis::read_host_config_with_path()
        .map(|(_, text)| log_sites_from_xml(&text, &default_root))
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

/// 查询 IIS 站点运行状态（`Started` / `Stopped`；未知/非 Windows 返回空）。
pub fn site_states() -> HashMap<String, String> {
    iis::site_states()
}

/// 为 IIS 站点启用日志记录并勾选 `BytesSent`（sc-bytes）。`None` = 全部站点。
#[cfg(windows)]
pub fn enable_sc_bytes(site: Option<&str>) -> Result<String, String> {
    let default_root = default_log_root();
    let Some((config_path, xml)) = iis::read_host_config_with_path() else {
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
            let args = [
                "set",
                "config",
                "-section:system.applicationHost/sites",
                filter.as_str(),
                "/commit:apphost",
            ];
            if let Err(e) = iis::appcmd(&args, Duration::from_secs(30)) {
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
            let args = [
                "set",
                "config",
                "-section:system.applicationHost/sites",
                filter.as_str(),
                "/commit:apphost",
            ];
            if let Err(e) = iis::appcmd(&args, Duration::from_secs(30)) {
                errors.push(format!("{}：{e}", s.name));
                continue;
            }
        }
        changed += 1;
    }
    iis::invalidate_cache();
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
    let expanded = iis::expand_env_vars(directory.trim());
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


#[cfg(test)]
mod tests {
    use super::*;
    use dhrust::sys::iis::IisSite;

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
    fn unsupported_log_format_is_flagged() {
        assert!(supported_format("W3C"));
        assert!(supported_format("ncsa"));
        assert!(supported_format(""));
        assert!(!supported_format("IIS"));
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

    /// Windows 实机验证 NtQueryInformationProcess 命令行读取（不依赖 IIS）。
    #[cfg(windows)]
    #[test]
    fn reads_current_process_command_line() {
        let cmd = dhrust::sys::process::command_line(std::process::id())
            .expect("command line should be readable");
        assert!(!cmd.trim().is_empty());
    }
}
