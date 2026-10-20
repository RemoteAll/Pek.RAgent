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
    let root = std::env::var_os("windir")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(r"C:\Windows"));
    let path = root
        .join("System32")
        .join("inetsrv")
        .join("config")
        .join("applicationHost.config");
    std::fs::read_to_string(path).ok()
}

#[cfg(not(windows))]
fn read_apps_config() -> Option<String> {
    None
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
