//! 日志清理（Web 面板「日志清理」页，对齐宝塔「日志清理」插件）：
//! 扫描系统 / 网站 / Nginx 缓存 / 代理自身 / Redis / 数据库等日志与缓存占用，
//! 勾选后一键清理。
//!
//! 清理方式（对齐宝塔行为，兼顾“正在被写入”的场景）：
//! - 日志文件：**截断**（`set_len(0)`）而非删除——持有句柄的进程继续正常写入；
//! - 缓存/日志目录：清空内容——目录内 `*.log` 文件同样截断保留、其余文件与子目录删除
//!   （Nginx 缓存按需自动重建）；
//! - journald：`journalctl --rotate && --vacuum-time=1s` 真空。
//!
//! 安全：清理路径全部由后端按平台内置规则构造（请求只提交分类 key，不携带路径），
//! 自定义路径来自管理员配置 `LogCleanupPaths`（分号分隔）。
//!
//! 安全护栅（2026-10-21 审计加固）：
//! - 所有目标（内置 + 自定义）过统一黑名单：根目录、系统关键目录（Windows/System32/WinSxS/Program Files/
//!   ProgramData 一级子目录、Unix /etc /usr /var/lib 等）、程序目录及其上级一律拒绝；
//! - 目录清理**不跟随符号链接/重解析点**（junction 既不进入也不删除，避免误清链接目标）；
//! - 子目录条目数/耗时双上限（防超大目录长时间占用磁盘与请求线程）；
//! - 清理在独立线程执行，不阻塞 HTTP 请求线程。

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use dhrust::net::controller::{json_body, json_error, json_result, ActionResult};
use dhrust::net::router::Ctx;
use serde_json::{json, Value as Json};

use crate::config::AgentConfig;
use crate::util;
use crate::webpanel::WebPanel;

/// 单目录统计的文件条目上限（防超大缓存目录拖慢请求）。
const DIR_SCAN_LIMIT: usize = 50_000;
/// 单目标清理的条目上限（防超大目录长时间占用）。
const CLEAR_MAX_ENTRIES: usize = 200_000;
/// 单目标清理的时长上限（秒）。
const CLEAR_MAX_SECONDS: u64 = 120;
/// 系统关键目录一级子目录中允许清理的名字（仅临时/日志/缓存语义）。
const SAFE_ONE_LEVEL_NAMES: &[&str] = &["temp", "tmp", "logs", "log", "cache", "caches"];

/// 清理目标类型。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[cfg_attr(windows, allow(dead_code))]
enum TargetKind {
    /// 日志文件：截断清空
    File,
    /// 缓存目录：清空内容（保留目录）
    Dir,
    /// journald 日志：journalctl 真空
    Journal,
}

impl TargetKind {
    fn text(self) -> &'static str {
        match self {
            TargetKind::File => "file",
            TargetKind::Dir => "dir",
            TargetKind::Journal => "journal",
        }
    }
}

/// 单个清理目标。
#[derive(Clone)]
struct Target {
    kind: TargetKind,
    path: PathBuf,
    /// 内置的明确安全目标，可越过目录黑名单（如 `/var/lib/systemd/coredump`、Docker 容器日志）。
    /// **仅内置常量构目标使用；自定义路径始终受完整守卫约束。**
    trusted: bool,
}

/// 清理分类（面板一行）。
struct Category {
    key: &'static str,
    name: &'static str,
    description: &'static str,
    targets: Vec<Target>,
}

// ————— 分类构建 —————

/// 安全判定规则。
#[derive(Clone, Copy)]
enum DenyRule {
    /// 仅禁止该目录本身（子目录可用，如 `/var`、`C:\Windows`）。
    Exact,
    /// 禁止该目录及其全部子目录。
    Tree,
    /// 禁止该目录与其一级子目录（更深层可由管理员显式指定）；仅 Windows 使用。
    #[cfg_attr(not(windows), allow(dead_code))]
    OneLevel,
}

/// 路径规范化键（统一分隔符；Windows 大小写不敏感），仅用于安全判定。
fn path_key(p: &Path) -> String {
    let text = p.to_string_lossy().replace('/', std::path::MAIN_SEPARATOR_STR);
    let text = text
        .trim_end_matches(std::path::MAIN_SEPARATOR_STR)
        .to_string();
    #[cfg(windows)]
    {
        text.to_ascii_lowercase()
    }
    #[cfg(not(windows))]
    {
        text
    }
}

/// 系统关键目录黑名单（禁止作为清理目标）。
fn deny_rules() -> Vec<(PathBuf, DenyRule)> {
    let mut rules: Vec<(PathBuf, DenyRule)> = Vec::new();
    #[cfg(windows)]
    {
        let sys = system_root_dir();
        rules.push((sys.clone(), DenyRule::Exact));
        for rel in [
            "System32",
            "SysWOW64",
            "WinSxS",
            "servicing",
            "Fonts",
            "assembly",
            "Microsoft.NET",
            "Boot",
        ] {
            rules.push((sys.join(rel), DenyRule::Tree));
        }
        for key in ["ProgramFiles", "ProgramFiles(x86)"] {
            if let Some(v) = std::env::var_os(key) {
                rules.push((PathBuf::from(v), DenyRule::OneLevel));
            }
        }
        rules.push((program_data_dir(), DenyRule::OneLevel));
        // 系统盘根目录（由 SystemRoot 推导，避免 SystemDrive 只有 "C:" 时 join 出相对路径）
        let drive_root = sys
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("C:\\"));
        rules.push((drive_root.join("Users"), DenyRule::OneLevel));
        for rel in [
            "PerfLogs",
            "Recovery",
            "boot",
            "config.msi",
            "$Recycle.Bin",
            "System Volume Information",
        ] {
            rules.push((drive_root.join(rel), DenyRule::Tree));
        }
    }
    #[cfg(unix)]
    {
        for (p, r) in [
            ("/etc", DenyRule::Tree),
            ("/usr", DenyRule::Tree),
            ("/bin", DenyRule::Tree),
            ("/sbin", DenyRule::Tree),
            ("/lib", DenyRule::Tree),
            ("/lib64", DenyRule::Tree),
            ("/boot", DenyRule::Tree),
            ("/proc", DenyRule::Tree),
            ("/sys", DenyRule::Tree),
            ("/dev", DenyRule::Tree),
            ("/run", DenyRule::Tree),
            ("/var/lib", DenyRule::Tree),
            ("/var/spool", DenyRule::Tree),
            ("/var", DenyRule::Exact),
            ("/root", DenyRule::Exact),
            ("/home", DenyRule::Exact),
            ("/opt", DenyRule::Exact),
            ("/srv", DenyRule::Exact),
            ("/mnt", DenyRule::Exact),
            ("/media", DenyRule::Exact),
            ("/snap", DenyRule::Exact),
        ] {
            rules.push((PathBuf::from(p), r));
        }
    }
    rules
}

/// 判定清理目标是否危险；返回拒绝原因。
///
/// 覆盖：根目录、系统关键目录、程序目录（仅允许清理其 `Log` 子目录）。
fn deny_reason(path: &Path, base: &Path) -> Option<String> {
    deny_reason_impl(path, base, true)
}

/// `check_rules = false` 时跳过目录黑名单（仅用于内置 trusted 目标），仍校验绝对路径与程序目录。
fn deny_reason_impl(path: &Path, base: &Path, check_rules: bool) -> Option<String> {
    if path.as_os_str().is_empty() {
        return Some("路径为空".to_string());
    }
    if !path.is_absolute() {
        return Some("必须为绝对路径".to_string());
    }
    if path.parent().is_none() {
        return Some("禁止清理根目录".to_string());
    }
    if check_rules {
        let key = path_key(path);
        let sep = std::path::MAIN_SEPARATOR;
        for (rule_path, rule) in deny_rules() {
            let rk = path_key(&rule_path);
            if rk.is_empty() || key == rk {
                return Some(format!("禁止清理系统关键目录：{}", rule_path.display()));
            }
            let inside = key.starts_with(&format!("{rk}{sep}"));
            match rule {
                DenyRule::Exact => {}
                DenyRule::Tree if inside => {
                    return Some(format!("禁止清理系统关键目录：{}", rule_path.display()));
                }
                DenyRule::OneLevel if inside => {
                    let rest = &key[rk.len() + 1..];
                    if !rest.contains(sep) {
                        let safe = SAFE_ONE_LEVEL_NAMES
                            .iter()
                            .any(|n| n.eq_ignore_ascii_case(rest));
                        if !safe {
                            return Some(format!(
                                "禁止清理系统关键目录的一级子目录：{}",
                                rule_path.display()
                            ));
                        }
                    }
                }
                _ => {}
            }
        }
    }
    // 程序目录：本身与上级禁止，内部仅允许 Log 子目录
    let sep = std::path::MAIN_SEPARATOR;
    let key = path_key(path);
    let base_key = path_key(base);
    if !base_key.is_empty() {
        if key == base_key || base_key.starts_with(&format!("{key}{sep}")) {
            return Some("禁止清理程序目录或其上级目录".to_string());
        }
        if key.starts_with(&format!("{base_key}{sep}")) {
            let log_key = path_key(&base.join("Log"));
            if key != log_key && !key.starts_with(&format!("{log_key}{sep}")) {
                return Some("程序目录内仅允许清理 Log 子目录".to_string());
            }
        }
    }
    None
}

/// 收集存在的文件目标（去重）。
fn push_file(targets: &mut Vec<Target>, seen: &mut HashSet<PathBuf>, path: &str) {
    push_file_with(targets, seen, path, false);
}

/// 收集存在的文件目标（trusted：跳过目录黑名单，仅内置使用）。
#[cfg_attr(windows, allow(dead_code))]
fn push_file_trusted(targets: &mut Vec<Target>, seen: &mut HashSet<PathBuf>, path: &str) {
    push_file_with(targets, seen, path, true);
}

fn push_file_with(targets: &mut Vec<Target>, seen: &mut HashSet<PathBuf>, path: &str, trusted: bool) {
    let p = PathBuf::from(path);
    if p.is_file() && seen.insert(p.clone()) {
        targets.push(Target {
            kind: TargetKind::File,
            path: p,
            trusted,
        });
    }
}

/// 收集存在的目录目标（去重）。
fn push_dir(targets: &mut Vec<Target>, seen: &mut HashSet<PathBuf>, path: &str) {
    push_dir_with(targets, seen, path, false);
}

/// 收集存在的目录目标（trusted：跳过目录黑名单，仅内置使用）。
#[cfg_attr(windows, allow(dead_code))]
fn push_dir_trusted(targets: &mut Vec<Target>, seen: &mut HashSet<PathBuf>, path: &str) {
    push_dir_with(targets, seen, path, true);
}

fn push_dir_with(targets: &mut Vec<Target>, seen: &mut HashSet<PathBuf>, path: &str, trusted: bool) {
    let p = PathBuf::from(path);
    if p.is_dir() && seen.insert(p.clone()) {
        targets.push(Target {
            kind: TargetKind::Dir,
            path: p,
            trusted,
        });
    }
}

/// 目录下 `*.log` 文件（不递归）。
fn glob_logs(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(rd) = fs::read_dir(dir) else {
        return out;
    };
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
        if name.ends_with(".log") {
            let p = entry.path();
            if p.is_file() {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

/// 按平台构建分类清单（仅保留含实际目标的分类）。
fn build_categories(base: &Path, cfg: &AgentConfig) -> Vec<Category> {
    let mut cats: Vec<Category> = Vec::new();

    #[cfg(unix)]
    {
        cats.push(Category {
            key: "system",
            name: "系统日志",
            description: "系统运行日志（syslog/messages/journald 等；未开启 audit 审计时可放心清理）",
            targets: linux_system_targets(),
        });
        cats.push(Category {
            key: "website",
            name: "网站日志",
            description: "网站访问与错误日志（nginx/apache；将清理此前的所有访问日志）",
            targets: linux_website_targets(),
        });
        cats.push(Category {
            key: "nginx_cache",
            name: "Nginx 缓存",
            description: "Nginx 反向代理缓存与临时文件（清理后按需自动重建）",
            targets: linux_nginx_cache_targets(),
        });
        cats.push(agent_category(base));
        cats.push(Category {
            key: "redis",
            name: "Redis 日志",
            description: "Redis 数据库运行日志",
            targets: linux_redis_targets(),
        });
        cats.push(Category {
            key: "mysql",
            name: "数据库日志",
            description: "MySQL/MariaDB 慢查询与运行日志",
            targets: linux_mysql_targets(),
        });
        cats.push(Category {
            key: "system_cache",
            name: "系统缓存",
            description: "包管理器缓存、崩溃转储与 Docker 容器日志（可放心清理；**不含 /tmp 与 /var/tmp**）",
            targets: linux_system_cache_targets(),
        });
        cats.push(Category {
            key: "user_cache",
            name: "用户缓存",
            description: "各用户 ~/.cache 与回收站（程序缓存，删除后按需重建；正在使用的文件自动跳过）",
            targets: linux_user_cache_targets(),
        });
    }

    #[cfg(windows)]
    {
        cats.push(Category {
            key: "system",
            name: "系统日志",
            description: "Windows 组件日志（CBS/DISM 更新与部署日志、Windows 更新日志）",
            targets: windows_system_targets(),
        });
        cats.push(Category {
            key: "system_cache",
            name: "系统缓存",
            description: "Windows 错误报告、更新会话日志、缩略图缓存与临时文件（ProgramData / Windows\\Temp / 各用户 Temp，可放心清理；不含程序安装缓存 Package Cache）",
            targets: windows_system_cache_targets(),
        });
        cats.push(Category {
            key: "website",
            name: "网站日志",
            description: "网站访问与错误日志（宝塔 Windows / nginx / IIS）",
            targets: windows_website_targets(),
        });
        cats.push(agent_category(base));
    }

    if let Some(custom) = custom_category(&cfg.log_cleanup_paths) {
        cats.push(custom);
    }

    // 安全守卫：内置与自定义目标统一过一遍危险路径检查（防御未来误加/误配）
    // trusted 目标（内置常量，如 coredump/Docker 日志）跳过目录黑名单，但仍校验程序目录。
    for c in &mut cats {
        c.targets.retain(|t| {
            let reason = if t.trusted {
                deny_reason_impl(&t.path, base, false)
            } else {
                deny_reason(&t.path, base)
            };
            match reason {
                None => true,
                Some(reason) => {
                    util::log_format(
                        "日志清理：跳过不安全的目标 {}（{}）",
                        &[&t.path.display().to_string(), &reason],
                    );
                    false
                }
            }
        });
    }

    // 同分类内去重：被目录目标覆盖的子路径移除（避免重复计数与重复清理）
    for c in &mut cats {
        dedup_targets(&mut c.targets);
    }
    cats.retain(|c| !c.targets.is_empty());
    cats
}

/// 自定义路径中的不安全项（供扫描接口展示）。
fn custom_warnings(text: &str, base: &Path) -> Vec<Json> {
    let mut out = Vec::new();
    for part in text.split(';') {
        let p = part.trim();
        if p.is_empty() {
            continue;
        }
        if let Some(reason) = deny_reason(Path::new(p), base) {
            out.push(json!({ "path": p, "reason": reason }));
        }
    }
    out
}

/// 去除被同分类其它目录目标覆盖的路径（目录清理会一并清空其内容）。
fn dedup_targets(targets: &mut Vec<Target>) {
    let dirs: Vec<PathBuf> = targets
        .iter()
        .filter(|t| t.kind == TargetKind::Dir)
        .map(|t| t.path.clone())
        .collect();
    if dirs.is_empty() {
        return;
    }
    targets.retain(|t| {
        if t.kind == TargetKind::Dir {
            // 目录：移除被其它目录覆盖的子目录
            !dirs.iter().any(|d| d != &t.path && t.path.starts_with(d))
        } else {
            // 文件/日志：移除位于目录目标内部的条目
            !dirs.iter().any(|d| t.path.starts_with(d))
        }
    });
}

/// 代理自身日志分类。
fn agent_category(base: &Path) -> Category {
    let mut targets = Vec::new();
    for p in glob_logs(&base.join("Log")) {
        targets.push(Target {
            kind: TargetKind::File,
            path: p,
            trusted: false,
        });
    }
    Category {
        key: "agent",
        name: "代理日志",
        description: "星尘代理（本程序）运行日志",
        targets,
    }
}

/// 自定义清理路径（`LogCleanupPaths`，分号分隔；目录递归清空、文件截断）。
fn custom_category(text: &str) -> Option<Category> {
    let mut targets = Vec::new();
    for part in text.split(';') {
        let p = part.trim();
        if p.is_empty() {
            continue;
        }
        let path = PathBuf::from(p);
        if path.is_dir() {
            targets.push(Target {
                kind: TargetKind::Dir,
                path,
                trusted: false,
            });
        } else if path.is_file() {
            targets.push(Target {
                kind: TargetKind::File,
                path,
                trusted: false,
            });
        }
    }
    if targets.is_empty() {
        None
    } else {
        Some(Category {
            key: "custom",
            name: "自定义路径",
            description: "管理员配置的额外清理路径（配置项 LogCleanupPaths）",
            targets,
        })
    }
}

#[cfg(unix)]
fn linux_system_targets() -> Vec<Target> {
    let mut targets = Vec::new();
    let mut seen = HashSet::new();
    // 常见系统日志文件
    const KNOWN: &[&str] = &[
        "/var/log/syslog",
        "/var/log/messages",
        "/var/log/secure",
        "/var/log/auth.log",
        "/var/log/kern.log",
        "/var/log/daemon.log",
        "/var/log/cron",
        "/var/log/maillog",
        "/var/log/mail.log",
        "/var/log/mail.err",
        "/var/log/boot.log",
        "/var/log/dmesg",
        "/var/log/yum.log",
        "/var/log/dnf.log",
        "/var/log/firewalld",
        "/var/log/sshd.log",
    ];
    for p in KNOWN {
        push_file(&mut targets, &mut seen, p);
    }
    // /var/log 顶层 *.log
    for p in glob_logs(Path::new("/var/log")) {
        let text = p.to_string_lossy().to_string();
        push_file(&mut targets, &mut seen, &text);
    }
    // journald（目录存在才显示；清理走 journalctl 真空）
    let journal = PathBuf::from("/var/log/journal");
    if journal.is_dir() {
        targets.push(Target {
            kind: TargetKind::Journal,
            path: journal,
            trusted: false,
        });
    }
    targets
}

#[cfg(unix)]
fn linux_website_targets() -> Vec<Target> {
    let mut targets = Vec::new();
    let mut seen = HashSet::new();
    const DIRS: &[&str] = &[
        "/www/wwwlogs",
        "/var/log/nginx",
        "/var/log/httpd",
        "/var/log/apache2",
        "/usr/local/nginx/logs",
        "/www/server/nginx/logs",
    ];
    for dir in DIRS {
        for p in glob_logs(Path::new(dir)) {
            let text = p.to_string_lossy().to_string();
            push_file(&mut targets, &mut seen, &text);
        }
    }
    targets
}

#[cfg(unix)]
fn linux_nginx_cache_targets() -> Vec<Target> {
    let mut targets = Vec::new();
    let mut seen = HashSet::new();
    const DIRS: &[&str] = &[
        "/www/server/nginx/proxy_cache_dir",
        "/www/server/nginx/nginx_cache",
        "/var/cache/nginx",
        "/usr/local/nginx/proxy_temp",
    ];
    for dir in DIRS {
        push_dir(&mut targets, &mut seen, dir);
    }
    targets
}

#[cfg(unix)]
fn linux_redis_targets() -> Vec<Target> {
    let mut targets = Vec::new();
    let mut seen = HashSet::new();
    for p in glob_logs(Path::new("/var/log/redis")) {
        let text = p.to_string_lossy().to_string();
        push_file(&mut targets, &mut seen, &text);
    }
    for p in ["/var/log/redis.log", "/www/server/redis/redis.log"] {
        push_file(&mut targets, &mut seen, p);
    }
    targets
}

#[cfg(unix)]
fn linux_mysql_targets() -> Vec<Target> {
    let mut targets = Vec::new();
    let mut seen = HashSet::new();
    for dir in ["/var/log/mysql", "/var/log/mariadb", "/www/server/data"] {
        for p in glob_logs(Path::new(dir)) {
            let text = p.to_string_lossy().to_string();
            push_file(&mut targets, &mut seen, &text);
        }
    }
    targets
}

/// 目录下一层中匹配后缀的文件（不递归；空后缀=全部文件）。
#[cfg_attr(windows, allow(dead_code))]
fn glob_suffix(dir: &Path, suffix: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(rd) = fs::read_dir(dir) else {
        return out;
    };
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
        if name.ends_with(suffix) {
            let p = entry.path();
            if p.is_file() {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

/// Linux 系统缓存与崩溃转储（含 Docker 容器日志；**有意不含 `/tmp`、`/var/tmp`**）。
///
/// - 包管理器缓存目录：删除安全（安装时按需重建）；
/// - `/var/crash`、`/var/lib/systemd/coredump`：崩溃报告/转储；
///   `/var/lib` 整体受守卫保护，此处用 trusted 目标明确豁免；
/// - Docker `json-file` 日志：**截断**（容器持有句柄，截断后继续写入），不删目录；
/// - `/tmp`、`/var/tmp` 存在运行中程序的 socket/锁/临时目录，清空会引发服务异常，故不纳入。
#[cfg(unix)]
fn linux_system_cache_targets() -> Vec<Target> {
    let mut targets = Vec::new();
    let mut seen = HashSet::new();
    for dir in [
        "/var/cache/apt/archives",
        "/var/cache/yum",
        "/var/cache/dnf",
        "/var/cache/zypper",
        "/var/cache/PackageKit",
        "/var/cache/man",
        "/var/crash",
    ] {
        push_dir(&mut targets, &mut seen, dir);
    }
    push_dir_trusted(&mut targets, &mut seen, "/var/lib/systemd/coredump");
    if let Ok(rd) = fs::read_dir("/var/lib/docker/containers") {
        let mut dirs: Vec<PathBuf> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        dirs.sort();
        for dir in dirs {
            for p in glob_suffix(&dir, "-json.log") {
                let text = p.to_string_lossy().to_string();
                push_file_trusted(&mut targets, &mut seen, &text);
            }
        }
    }
    targets
}

/// Linux 各用户缓存与回收站（`~/.cache`、`~/.local/share/Trash`；不跟随重解析点）。
#[cfg(unix)]
fn linux_user_cache_targets() -> Vec<Target> {
    let mut targets = Vec::new();
    let mut seen = HashSet::new();
    let mut homes: Vec<PathBuf> = Vec::new();
    if Path::new("/root").is_dir() {
        homes.push(PathBuf::from("/root"));
    }
    if let Ok(rd) = fs::read_dir("/home") {
        let mut sub: Vec<PathBuf> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        sub.sort();
        homes.extend(sub);
    }
    for home in homes {
        for rel in [".cache", ".local/share/Trash"] {
            let p = home.join(rel);
            push_dir(&mut targets, &mut seen, &p.to_string_lossy());
        }
    }
    targets
}

/// Windows 系统盘根目录（`%SystemRoot%`，回退 `C:\Windows`）。
/// 用环境变量解析，兼容系统盘不是 C: 的机器。
#[cfg(windows)]
fn system_root_dir() -> PathBuf {
    std::env::var_os("SystemRoot")
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
        .unwrap_or_else(|| PathBuf::from("C:\\Windows"))
}

/// Windows 公共数据目录（`%ProgramData%`，回退 `C:\ProgramData`）。
#[cfg(windows)]
fn program_data_dir() -> PathBuf {
    std::env::var_os("ProgramData")
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
        .unwrap_or_else(|| PathBuf::from("C:\\ProgramData"))
}

#[cfg(windows)]
fn windows_system_targets() -> Vec<Target> {
    let mut targets = Vec::new();
    let mut seen = HashSet::new();
    let root = system_root_dir();
    for rel in ["Logs\\CBS\\CBS.log", "Logs\\DISM\\dism.log"] {
        let p = root.join(rel);
        push_file(&mut targets, &mut seen, &p.to_string_lossy());
    }
    // Windows 更新日志（.etl，递归清空；目录会被系统按需重建）
    let upd = root.join("Logs\\WindowsUpdate");
    push_dir(&mut targets, &mut seen, &upd.to_string_lossy());
    targets
}

/// Windows 系统缓存与临时文件（`%ProgramData%` + `%SystemRoot%\Temp` + 各用户 Temp）。
///
/// 只纳入「删除后系统自动重建、不影响程序卸载/修复」的目录：
/// - Windows 错误报告归档/队列/临时文件（WER）、更新会话日志（USOShared）、图标缩略图缓存（Caches）、
///   系统与第三方临时文件（`ProgramData\Temp`、`SystemRoot\Temp`）、**各用户 profile 的 Temp**
///   （应用池/服务账户的 `%TEMP%` 经常堆积大量不再使用的 `.tmp`）。
/// - **不含 `ProgramData\Package Cache`**：VC++/VS 安装缓存，删除会导致后续修复/卸载失败；
/// - **不含 Windows Defender 定义库 / AppRepository / SystemData**：属系统关键数据。
#[cfg(windows)]
fn windows_system_cache_targets() -> Vec<Target> {
    let mut targets = Vec::new();
    let mut seen = HashSet::new();
    let pd = program_data_dir();
    for rel in [
        "Microsoft\\Windows\\WER\\ReportArchive",
        "Microsoft\\Windows\\WER\\ReportQueue",
        "Microsoft\\Windows\\WER\\Temp",
        "Microsoft\\Windows\\Caches",
        "USOShared\\Logs",
        "Temp",
    ] {
        let p = pd.join(rel);
        push_dir(&mut targets, &mut seen, &p.to_string_lossy());
    }
    let root = system_root_dir();
    let tmp = root.join("Temp");
    push_dir(&mut targets, &mut seen, &tmp.to_string_lossy());
    for p in user_temp_dirs(&root) {
        push_dir(&mut targets, &mut seen, &p.to_string_lossy());
    }
    targets
}

/// 各用户 profile 下的临时目录（跳过 `Public`/`Default`，不跟随重解析点）。
#[cfg(windows)]
fn user_temp_dirs(system_root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let users = system_root
        .parent()
        .map(|p| p.join("Users"))
        .unwrap_or_else(|| PathBuf::from("C:\\Users"));
    let Ok(rd) = fs::read_dir(&users) else {
        return out;
    };
    for entry in rd.flatten() {
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        if !ft.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if name.eq_ignore_ascii_case("Public") || name.eq_ignore_ascii_case("Default") {
            continue;
        }
        for rel in ["AppData\\Local\\Temp", "AppData\\LocalLow\\Temp"] {
            let t = entry.path().join(rel);
            if t.is_dir() {
                out.push(t);
            }
        }
    }
    out.sort();
    out
}

#[cfg(windows)]
fn windows_website_targets() -> Vec<Target> {
    let mut targets = Vec::new();
    let mut seen = HashSet::new();
    for dir in [
        "C:\\BtSoft\\wwwlogs",
        "C:\\BtSoft\\nginx\\logs",
        "C:\\nginx\\logs",
    ] {
        for p in glob_logs(Path::new(dir)) {
            let text = p.to_string_lossy().to_string();
            push_file(&mut targets, &mut seen, &text);
        }
    }
    // IIS 日志（目录递归清空）
    push_dir(&mut targets, &mut seen, "C:\\inetpub\\logs\\LogFiles");
    targets
}

// ————— 扫描与清理 —————

/// 目录递归统计（条目上限保护）：（文件数, 字节数, 是否截断）。
fn dir_stats_limited(path: &Path, limit: usize) -> (u64, u64, bool) {
    let mut files = 0u64;
    let mut bytes = 0u64;
    let mut visited = 0usize;
    let mut truncated = false;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = fs::read_dir(&dir) else { continue };
        for entry in rd.flatten() {
            visited += 1;
            if visited > limit {
                truncated = true;
                break;
            }
            let Ok(md) = fs::symlink_metadata(entry.path()) else {
                continue;
            };
            // 符号链接/目录联接：不跟随、不统计（与清理行为一致）
            if md.file_type().is_symlink() {
                continue;
            }
            if md.is_dir() {
                stack.push(entry.path());
            } else {
                files += 1;
                bytes += md.len();
            }
        }
        if truncated {
            break;
        }
    }
    (files, bytes, truncated)
}

/// 统计单个目标。
fn scan_target(t: &Target) -> (u64, u64, bool) {
    match t.kind {
        TargetKind::File => {
            let md = fs::metadata(&t.path);
            let size = md.map(|m| m.len()).unwrap_or(0);
            (if size > 0 { 1 } else { 0 }, size, false)
        }
        TargetKind::Dir | TargetKind::Journal => {
            dir_stats_limited(&t.path, DIR_SCAN_LIMIT)
        }
    }
}

/// 删除文件或目录（符号链接只删链接本身）。
fn remove_any(path: &Path) -> Result<(), String> {
    let md = fs::symlink_metadata(path)
        .map_err(|e| format!("无法访问 {}：{}", path.display(), e))?;
    if md.file_type().is_symlink() {
        if fs::remove_dir(path).is_err() {
            fs::remove_file(path).map_err(|e| format!("删除失败 {}：{}", path.display(), e))?;
        }
        Ok(())
    } else if md.is_dir() {
        fs::remove_dir_all(path).map_err(|e| format!("删除失败 {}：{}", path.display(), e))
    } else {
        fs::remove_file(path).map_err(|e| format!("删除失败 {}：{}", path.display(), e))
    }
}

/// 截断文件为 0 字节（保留文件本体；返回原字节数）。
fn truncate_file(path: &Path) -> Result<u64, String> {
    let before = fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    if before == 0 {
        return Ok(0);
    }
    let f = fs::OpenOptions::new()
        .write(true)
        .open(path)
        .map_err(|e| format!("打开失败 {}：{}", path.display(), e))?;
    f.set_len(0)
        .map_err(|e| format!("清空失败 {}：{}", path.display(), e))?;
    Ok(before)
}

/// 清理子目录遍历上限（条目数 / 耗时）与局部错误收集。
struct ClearLimits {
    entries: usize,
    deadline: std::time::Instant,
    truncated: bool,
    /// 条目级错误（仅记日志，最多保留 20 条防刷屏）。
    errors: Vec<String>,
}

impl ClearLimits {
    fn new() -> Self {
        Self {
            entries: 0,
            deadline: std::time::Instant::now()
                + std::time::Duration::from_secs(CLEAR_MAX_SECONDS),
            truncated: false,
            errors: Vec::new(),
        }
    }

    /// 计一个条目；返回 false 表示已达上限，应停止。
    fn tick(&mut self) -> bool {
        self.entries += 1;
        if self.entries > CLEAR_MAX_ENTRIES {
            self.truncated = true;
            return false;
        }
        if self.entries.is_multiple_of(256) && std::time::Instant::now() > self.deadline {
            self.truncated = true;
            return false;
        }
        true
    }

    /// 记录条目级失败（不中断整体清理）。
    fn note_error(&mut self, msg: String) {
        if self.errors.len() < 20 {
            util::log_format("日志清理：{}", &[&msg]);
            self.errors.push(msg);
        }
    }
}

/// 清空目录：`*.log` 文件截断保留（供仍持句柄的进程继续写入），
/// 其余文件删除、子目录递归清理（空后删除；含保留文件时保留目录）。
///
/// **不跟随符号链接/目录联接**（既不进入也不删除）——避免误清链接目标或破坏重定向；
/// 条目数/耗时超限时停止并记日志（已清理部分不回滚，不会造成损坏）。
fn clear_dir_keeping_logs(dir: &Path, limits: &mut ClearLimits) -> Result<(), String> {
    let rd = fs::read_dir(dir).map_err(|e| format!("无法读取目录 {}：{}", dir.display(), e))?;
    for entry in rd.flatten() {
        if !limits.tick() {
            util::log_format(
                "日志清理：{} 已达条目/时长上限，剩余内容未处理",
                &[&dir.display().to_string()],
            );
            break;
        }
        let p = entry.path();
        let Ok(md) = fs::symlink_metadata(&p) else {
            continue;
        };
        if md.file_type().is_symlink() {
            // 符号链接/目录联接：保留（删除会破坏目标程序的重定向配置）
            continue;
        }
        if md.is_dir() {
            // 子目录：失败不中断整体清理（正在被占用/权限不足的条目跳过）
            match clear_dir_keeping_logs(&p, limits) {
                Ok(()) => {
                    let _ = fs::remove_dir(&p); // 子目录已清空则删除（含截断保留的 .log 时保留）
                }
                Err(e) => limits.note_error(format!("子目录跳过 {}：{}", p.display(), e)),
            }
        } else if p
            .extension()
            .map(|e| e.eq_ignore_ascii_case("log"))
            .unwrap_or(false)
        {
            if let Err(e) = truncate_file(&p) {
                limits.note_error(format!("文件跳过 {}：{}", p.display(), e));
            }
        } else if let Err(e) = remove_any(&p) {
            limits.note_error(format!("文件跳过 {}：{}", p.display(), e));
        }
    }
    Ok(())
}

/// 清理单个目标；返回（清理文件数, 释放字节数）。目标已不存在时静默跳过。
fn clean_target(t: &Target) -> Result<(u64, u64), String> {
    if !t.path.exists() {
        return Ok((0, 0));
    }
    match t.kind {
        TargetKind::File => {
            let before = truncate_file(&t.path)?;
            Ok((if before > 0 { 1 } else { 0 }, before))
        }
        TargetKind::Dir => {
            // 顶层目标若是符号链接/目录联接，直接跳过（避免清空其指向的未知位置）
            if let Ok(md) = fs::symlink_metadata(&t.path)
                && md.file_type().is_symlink()
            {
                return Err(format!(
                    "目录为重解析点/符号链接，已跳过：{}",
                    t.path.display()
                ));
            }
            let (files, bytes, _) = dir_stats_limited(&t.path, DIR_SCAN_LIMIT);
            let mut limits = ClearLimits::new();
            clear_dir_keeping_logs(&t.path, &mut limits)?;
            Ok((files, bytes))
        }
        TargetKind::Journal => {
            #[cfg(unix)]
            {
                let (_, before, _) = dir_stats_limited(&t.path, DIR_SCAN_LIMIT);
                let rs = dhrust::sys::process::run_shell(
                    "journalctl --rotate >/dev/null 2>&1; journalctl --vacuum-time=1s",
                    Path::new("/"),
                    60,
                );
                let (_, after, _) = dir_stats_limited(&t.path, DIR_SCAN_LIMIT);
                if rs.timed_out {
                    return Err("journalctl 清理超时".to_string());
                }
                if rs.exit_code != 0 && after >= before {
                    let hint = rs.error.trim();
                    let hint = if hint.is_empty() { rs.output.trim() } else { hint };
                    return Err(format!("journalctl 清理失败：{}", hint));
                }
                Ok((0, before.saturating_sub(after)))
            }
            #[cfg(windows)]
            {
                let _ = t;
                Err("仅 Linux 支持 journald 清理".to_string())
            }
        }
    }
}

// ————— HTTP 动作 —————

/// `GET /star/logCleanScan`：扫描各分类占用。
///
/// 扫描需递归遍历可能很大的目录（含各用户 Temp），故与清理一样放独立线程执行，
/// 避免阻塞 HTTP 请求线程。
pub fn log_clean_scan(panel: &WebPanel, ctx: &Ctx) -> ActionResult {
    if !panel.check_auth(ctx) {
        return json_error(401, "Unauthorized");
    }
    let cfg = panel.config();
    let categories = build_categories(panel.base(), &cfg);
    let warnings = custom_warnings(&cfg.log_cleanup_paths, panel.base());

    let jobs: Vec<(String, String, String, Vec<Target>)> = categories
        .iter()
        .map(|c| {
            (
                c.key.to_string(),
                c.name.to_string(),
                c.description.to_string(),
                c.targets.clone(),
            )
        })
        .collect();
    let (list, total_size, total_count) = {
        let thread_jobs = jobs.clone();
        match std::thread::Builder::new()
            .name("ragent-logscan".to_string())
            .spawn(move || scan_jobs(thread_jobs))
        {
            Ok(handle) => match handle.join() {
                Ok(v) => v,
                Err(_) => return json_error(500, "扫描线程异常退出"),
            },
            Err(e) => {
                util::log_format(
                    "日志清理：无法创建扫描线程（{}），改为同步执行",
                    &[&e.to_string()],
                );
                scan_jobs(jobs)
            }
        }
    };

    json_result(
        0,
        "",
        Some(json!({
            "categories": list,
            "totalSize": total_size,
            "totalCount": total_count,
            "warnings": warnings,
        })),
    )
}

/// 执行扫描（供后台线程调用）：（分类 JSON 列表, 总字节, 总文件数）。
fn scan_jobs(jobs: Vec<(String, String, String, Vec<Target>)>) -> (Vec<Json>, u64, u64) {
    let mut total_size = 0u64;
    let mut total_count = 0u64;
    let list: Vec<Json> = jobs
        .into_iter()
        .map(|(key, name, description, targets)| {
            let mut size = 0u64;
            let mut count = 0u64;
            let mut truncated = false;
            let detail: Vec<Json> = targets
                .iter()
                .map(|t| {
                    let (files, bytes, tr) = scan_target(t);
                    size += bytes;
                    count += files;
                    truncated |= tr;
                    json!({
                        "path": t.path.display().to_string(),
                        "kind": t.kind.text(),
                        "size": bytes,
                        "count": files,
                    })
                })
                .collect();
            total_size += size;
            total_count += count;
            json!({
                "key": key,
                "name": name,
                "description": description,
                "size": size,
                "count": count,
                "truncated": truncated,
                "targets": detail,
            })
        })
        .collect();
    (list, total_size, total_count)
}

/// `POST /star/logCleanRun`：按分类 key 清理（`{keys: ["system", ...]}`）。
pub fn log_clean_run(panel: &WebPanel, ctx: &Ctx) -> ActionResult {
    if !panel.check_auth(ctx) {
        return json_error(401, "Unauthorized");
    }
    let Some(body) = json_body(ctx) else {
        return json_error(400, "缺少请求体");
    };
    let keys: Vec<String> = body
        .get("keys")
        .and_then(|v| v.as_array())
        .map(|list| {
            list.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    if keys.is_empty() {
        return json_error(400, "未选择清理分类");
    }

    let cfg = panel.config();
    let categories = build_categories(panel.base(), &cfg);
    let selected: Vec<&Category> = categories
        .iter()
        .filter(|c| keys.iter().any(|k| k == c.key))
        .collect();
    if selected.is_empty() {
        return json_error(400, "未匹配到清理分类（可能目标已不存在）");
    }

    // 清理含递归磁盘 IO，放独立线程执行，避免阻塞 HTTP 请求线程（大目录可达数十秒）
    let jobs: Vec<(String, String, Vec<Target>)> = selected
        .iter()
        .map(|c| (c.key.to_string(), c.name.to_string(), c.targets.clone()))
        .collect();
    let results = {
        let thread_jobs = jobs.clone();
        match std::thread::Builder::new()
            .name("ragent-logclean".to_string())
            .spawn(move || run_clean_jobs(thread_jobs))
        {
            Ok(handle) => match handle.join() {
                Ok(v) => v,
                Err(_) => return json_error(500, "清理线程异常退出"),
            },
            Err(e) => {
                util::log_format(
                    "日志清理：无法创建清理线程（{}），改为同步执行",
                    &[&e.to_string()],
                );
                run_clean_jobs(jobs)
            }
        }
    };

    let mut freed = 0u64;
    let mut files = 0u64;
    let mut details: Vec<Json> = Vec::new();
    for (key, name, cat_freed, cat_files, errors) in results {
        freed += cat_freed;
        files += cat_files;
        details.push(json!({
            "key": key,
            "name": name,
            "freedBytes": cat_freed,
            "files": cat_files,
            "errors": errors,
        }));
    }

    json_result(
        0,
        &format!("清理完成，共释放 {} 字节", freed),
        Some(json!({ "freedBytes": freed, "files": files, "details": details })),
    )
}

/// 执行清理任务（供后台线程调用）：（key, 名称, 释放字节, 文件数, 错误列表）。
fn run_clean_jobs(
    jobs: Vec<(String, String, Vec<Target>)>,
) -> Vec<(String, String, u64, u64, Vec<String>)> {
    let mut out = Vec::new();
    for (key, name, targets) in jobs {
        let mut freed = 0u64;
        let mut files = 0u64;
        let mut errors: Vec<String> = Vec::new();
        for t in &targets {
            match clean_target(t) {
                Ok((f, b)) => {
                    files += f;
                    freed += b;
                }
                Err(e) => errors.push(e),
            }
        }
        util::log_format(
            "日志清理：{} 释放 {} 字节（{} 个文件）{}",
            &[
                &name,
                &freed.to_string(),
                &files.to_string(),
                &if errors.is_empty() {
                    String::new()
                } else {
                    format!("，{} 个错误", errors.len())
                },
            ],
        );
        out.push((key, name, freed, files, errors));
    }
    out
}

/// `GET /star/logCleanConfig`：读取自定义清理路径（配置项 `LogCleanupPaths`，分号分隔）。
pub fn log_clean_config(panel: &WebPanel, ctx: &Ctx) -> ActionResult {
    if !panel.check_auth(ctx) {
        return json_error(401, "Unauthorized");
    }
    let cfg = panel.config();
    let custom: Vec<String> = cfg
        .log_cleanup_paths
        .split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect();
    json_result(0, "", Some(json!({ "custom": custom })))
}

/// `POST /star/logCleanConfig {"custom": ["绝对路径", ...]}`：保存自定义清理路径。
///
/// 校验：必须为绝对路径且存在（目录=清空内容、文件=截断）；自动去重；保存后热生效。
pub fn log_clean_config_save(panel: &WebPanel, ctx: &Ctx) -> ActionResult {
    if !panel.check_auth(ctx) {
        return json_error(401, "Unauthorized");
    }
    let Some(body) = json_body(ctx) else {
        return json_error(400, "缺少请求体");
    };
    let Some(list) = body.get("custom").and_then(|v| v.as_array()) else {
        return json_error(400, "缺少 custom 数组");
    };
    let mut paths: Vec<String> = Vec::new();
    for item in list {
        let p = item.as_str().map(str::trim).unwrap_or("").to_string();
        if p.is_empty() {
            continue;
        }
        let path = Path::new(&p);
        if !path.is_absolute() {
            return json_error(400, &format!("路径必须为绝对路径：{p}"));
        }
        if !path.is_dir() && !path.is_file() {
            return json_error(400, &format!("路径不存在：{p}"));
        }
        if let Some(reason) = deny_reason(path, panel.base()) {
            return json_error(400, &format!("不允许的清理路径：{p}（{reason}）"));
        }
        if !paths.contains(&p) {
            paths.push(p);
        }
    }
    let joined = paths.join(";");
    panel.update_config(|cfg| cfg.log_cleanup_paths = joined.clone());
    util::log_format(
        "日志清理：自定义路径已更新（{} 项）",
        &[&paths.len().to_string()],
    );
    json_result(0, "已保存", Some(json!({ "custom": paths })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pek-logclean-{}-{}-{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn truncates_log_file() {
        let dir = temp_dir("truncate");
        let file = dir.join("app.log");
        fs::write(&file, b"0123456789").unwrap();
        let target = Target {
            kind: TargetKind::File,
            path: file.clone(),
            trusted: false,
        };
        let (files, freed) = clean_target(&target).unwrap();
        assert_eq!((files, freed), (1, 10));
        assert_eq!(fs::metadata(&file).unwrap().len(), 0);
        // 再清一次无释放
        let (files, freed) = clean_target(&target).unwrap();
        assert_eq!((files, freed), (0, 0));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn clears_cache_directory_keeping_directory() {
        let dir = temp_dir("cache");
        let cache = dir.join("cache");
        fs::create_dir_all(cache.join("sub")).unwrap();
        fs::write(cache.join("a.bin"), vec![0u8; 100]).unwrap();
        fs::write(cache.join("sub/b.bin"), vec![0u8; 50]).unwrap();
        let target = Target {
            kind: TargetKind::Dir,
            path: cache.clone(),
            trusted: false,
        };
        let (files, freed) = clean_target(&target).unwrap();
        assert_eq!((files, freed), (2, 150));
        assert!(cache.is_dir());
        assert!(fs::read_dir(&cache).unwrap().next().is_none());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn clears_directory_truncating_log_files() {
        let dir = temp_dir("dirlogs");
        let cache = dir.join("logs");
        fs::create_dir_all(cache.join("sub")).unwrap();
        fs::write(cache.join("app.log"), b"1234567890").unwrap();
        fs::write(cache.join("sub/err.LOG"), b"abcdef").unwrap();
        fs::write(cache.join("data.bin"), b"xyz").unwrap();
        let target = Target {
            kind: TargetKind::Dir,
            path: cache.clone(),
            trusted: false,
        };
        let (files, freed) = clean_target(&target).unwrap();
        assert_eq!(files, 3);
        assert_eq!(freed, 19);
        // .log 截断保留；其余删除；子目录因含保留文件而保留
        assert_eq!(fs::metadata(cache.join("app.log")).unwrap().len(), 0);
        assert_eq!(fs::metadata(cache.join("sub/err.LOG")).unwrap().len(), 0);
        assert!(!cache.join("data.bin").exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn dir_stats_respects_limit() {
        let dir = temp_dir("limit");
        for i in 0..20 {
            fs::write(dir.join(format!("f{i}.log")), b"x").unwrap();
        }
        let (files, bytes, truncated) = dir_stats_limited(&dir, 5);
        assert!(files <= 6);
        assert!(bytes <= 6);
        assert!(truncated);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn custom_category_parses_config() {
        let dir = temp_dir("custom");
        let file = dir.join("a.log");
        fs::write(&file, b"x").unwrap();
        let cache = dir.join("cache");
        fs::create_dir(&cache).unwrap();
        let text = format!(
            "{} ; {} ; /nonexistent-path-xyz",
            file.display(),
            cache.display()
        );
        let cat = custom_category(&text).unwrap();
        assert_eq!(cat.targets.len(), 2);
        assert_eq!(cat.targets[0].kind, TargetKind::File);
        assert_eq!(cat.targets[1].kind, TargetKind::Dir);
        assert!(custom_category("").is_none());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn dedup_removes_paths_covered_by_parent_directory() {
        let parent = PathBuf::from("/var/mylogs");
        let child = PathBuf::from("/var/mylogs/cache");
        let file = PathBuf::from("/var/mylogs/app.log");
        let mut targets = vec![
            Target {
                kind: TargetKind::Dir,
                path: parent.clone(),
                trusted: false,
            },
            Target {
                kind: TargetKind::Dir,
                path: child,
                trusted: false,
            },
            Target {
                kind: TargetKind::File,
                path: file,
                trusted: false,
            },
        ];
        dedup_targets(&mut targets);
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].path, parent);
    }

    #[test]
    fn glob_logs_finds_only_log_files() {
        let dir = temp_dir("glob");
        fs::write(dir.join("a.log"), b"1").unwrap();
        fs::write(dir.join("b.LOG"), b"2").unwrap();
        fs::write(dir.join("c.txt"), b"3").unwrap();
        let list = glob_logs(&dir);
        assert_eq!(list.len(), 2);
        fs::remove_dir_all(&dir).unwrap();
    }

    /// Windows 系统缓存目标必须落在 ProgramData / SystemRoot / 用户目录之内，
    /// 且绝不能包含安装缓存、杀毒库等关键目录（清掉会导致修复/卸载/防护异常）。
    #[cfg(windows)]
    #[test]
    fn windows_system_cache_targets_stay_within_safe_dirs() {
        let pd = program_data_dir();
        let root = system_root_dir();
        let users = root
            .parent()
            .map(|p| p.join("Users"))
            .unwrap_or_else(|| PathBuf::from("C:\\Users"));
        for t in windows_system_cache_targets() {
            assert!(
                t.path.starts_with(&pd)
                    || t.path.starts_with(&root)
                    || t.path.starts_with(&users),
                "意外的清理目标：{}",
                t.path.display()
            );
            let text = t
                .path
                .to_string_lossy()
                .to_ascii_lowercase()
                .replace('/', "\\");
            for bad in ["package cache", "defender", "apprepository", "systemdata"] {
                assert!(!text.contains(bad), "不得清理系统关键目录 {bad}：{text}");
            }
        }
    }

    /// 有可清理目录时才出现「系统缓存」分类（空机器不显示空分类）。
    #[cfg(windows)]
    #[test]
    fn system_cache_category_appears_when_targets_exist() {
        let has_any = windows_system_cache_targets().iter().any(|t| t.path.is_dir());
        let cfg = AgentConfig::default();
        let base = std::env::temp_dir().join("pek-logclean-none");
        let cats = build_categories(&base, &cfg);
        let present = cats.iter().any(|c| c.key == "system_cache");
        assert_eq!(
            present,
            has_any,
            "system_cache 分类出现与否应与目标目录是否存在一致（ProgramData={}）",
            program_data_dir().display()
        );
    }

    // ————— 安全守卫（2026-10-21 审计） —————

    #[test]
    fn deny_reason_rejects_dangerous_targets() {
        let base = std::env::temp_dir().join("pek-logclean-guardbase");
        #[cfg(windows)]
        {
            for p in [
                "C:\\",
                "C:\\Windows",
                "C:\\Windows\\System32",
                "C:\\Windows\\System32\\config",
                "C:\\Windows\\WinSxS",
                "C:\\ProgramData",
                "C:\\ProgramData\\Microsoft",
                "C:\\Program Files",
                "C:\\Program Files\\Microsoft",
                "C:\\Users\\Public",
            ] {
                assert!(deny_reason(Path::new(p), &base).is_some(), "应拒绝：{p}");
            }
            for p in [
                "C:\\Windows\\Logs\\CBS",
                "C:\\Windows\\Temp",
                "C:\\ProgramData\\Temp",
                "C:\\ProgramData\\Microsoft\\Windows\\WER\\ReportArchive",
                "C:\\Logs\\myapp",
            ] {
                assert!(
                    deny_reason(Path::new(p), &base).is_none(),
                    "应允许：{p}（{:?}）",
                    deny_reason(Path::new(p), &base)
                );
            }
            assert!(
                deny_reason(Path::new("relative\\logs"), &base).is_some(),
                "相对路径必须拒绝"
            );
            // 各用户 Temp 允许清理（profile 根仍拒绝）
            for p in [
                "C:\\Users\\someuser\\AppData\\Local\\Temp",
                "C:\\Users\\someuser\\AppData\\LocalLow\\Temp",
                "C:\\Users\\someuser\\AppData\\Local\\Temp\\sub",
            ] {
                assert!(
                    deny_reason(Path::new(p), &base).is_none(),
                    "应允许：{p}（{:?}）",
                    deny_reason(Path::new(p), &base)
                );
            }
            assert!(
                deny_reason(Path::new("C:\\Users\\someuser"), &base).is_some(),
                "用户 profile 根必须拒绝"
            );
        }
        #[cfg(unix)]
        {
            for p in [
                "/",
                "/etc",
                "/etc/nginx",
                "/usr",
                "/usr/lib",
                "/var",
                "/var/lib/mysql",
                "/root",
                "/opt",
                "/home",
            ] {
                assert!(deny_reason(Path::new(p), &base).is_some(), "应拒绝：{p}");
            }
            for p in [
                "/var/log",
                "/var/log/nginx",
                "/opt/myapp/logs",
                "/home/app/logs",
                "/tmp/cache",
            ] {
                assert!(
                    deny_reason(Path::new(p), &base).is_none(),
                    "应允许：{p}（{:?}）",
                    deny_reason(Path::new(p), &base)
                );
            }
            assert!(
                deny_reason(Path::new("relative/logs"), &base).is_some(),
                "相对路径必须拒绝"
            );
        }
    }

    #[test]
    fn deny_reason_protects_program_dir() {
        let base = temp_dir("guardbase");
        let cases = [
            (base.clone(), true),
            (base.join("Config"), true),
            (base.join("Data"), true),
            (base.join("Logs"), true),
            (base.join("Log"), false),
            (base.join("Log").join("sub"), false),
        ];
        for (p, denied) in cases {
            let got = deny_reason(&p, &base).is_some();
            assert_eq!(
                got,
                denied,
                "{} 期望 {}（实际 {:?}）",
                p.display(),
                denied,
                deny_reason(&p, &base)
            );
        }
        fs::remove_dir_all(&base).unwrap();
    }

    /// 内置目标不得被守卫误伤（否则分类会静默消失）。
    #[test]
    fn builtin_targets_pass_safety_guard() {
        let cfg = AgentConfig::default();
        let base = std::env::temp_dir().join("pek-logclean-guard-builtin");
        for c in build_categories(&base, &cfg) {
            for t in &c.targets {
                assert!(
                    deny_reason(&t.path, &base).is_none(),
                    "内置目标被守卫拦截：{}（分类 {}）",
                    t.path.display(),
                    c.key
                );
            }
        }
    }

    /// 目录内符号链接/联接：既不进入其目标，也不删除链接本身。
    #[test]
    fn clear_skips_symlink_entries() {
        let dir = temp_dir("symlink");
        let cache = dir.join("cache");
        let outside = dir.join("outside");
        fs::create_dir_all(&cache).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("keep.bin"), b"keep").unwrap();
        fs::write(cache.join("drop.bin"), b"x").unwrap();
        #[cfg(unix)]
        let linked = std::os::unix::fs::symlink(&outside, cache.join("link")).is_ok();
        #[cfg(windows)]
        let linked = std::os::windows::fs::symlink_dir(&outside, cache.join("link")).is_ok();
        let target = Target {
            kind: TargetKind::Dir,
            path: cache.clone(),
            trusted: false,
        };
        clean_target(&target).unwrap();
        assert!(!cache.join("drop.bin").exists(), "普通文件应被清理");
        if linked {
            assert!(outside.join("keep.bin").exists(), "链接目标不得被清理");
            assert!(cache.join("link").exists(), "链接本身不得被删除");
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    /// Linux 缓存目标必须落在预期目录内，且绝不包含 /tmp、/var/tmp 或系统关键目录。
    #[cfg(unix)]
    #[test]
    fn linux_cache_targets_stay_within_safe_dirs() {
        let base = std::env::temp_dir().join("pek-logclean-guardbase");
        let mut all = linux_system_cache_targets();
        all.extend(linux_user_cache_targets());
        for t in &all {
            let text = t.path.to_string_lossy().to_string();
            let in_home = text.starts_with("/home/")
                && (text.ends_with("/.cache") || text.ends_with("/.local/share/Trash"));
            let ok = in_home
                || text == "/root/.cache"
                || text == "/root/.local/share/Trash"
                || text.starts_with("/var/cache/")
                || text == "/var/crash"
                || text == "/var/lib/systemd/coredump"
                || text.starts_with("/var/lib/docker/containers/");
            assert!(ok, "意外的清理目标：{text}");
            for bad in ["/tmp", "/var/tmp"] {
                assert!(
                    !text.ends_with(bad) && !text.contains(&format!("{bad}/")),
                    "不得清理 {bad}：{text}"
                );
            }
        }
        // /var/lib 整体受守卫保护：普通目标拒绝，内置 trusted 目标放行
        assert!(deny_reason(Path::new("/var/lib/systemd/coredump"), &base).is_some());
        assert!(deny_reason_impl(Path::new("/var/lib/systemd/coredump"), &base, false).is_none());
        // 即便 trusted，也不得触碰程序目录
        assert!(deny_reason_impl(&base, &base, false).is_some());
    }

    /// 超大目录（条目超限）会提前停止，且不 panic。
    #[test]
    fn clear_limits_stop_after_max_entries() {
        let dir = temp_dir("limits");
        for i in 0..10 {
            fs::write(dir.join(format!("f{i}.bin")), b"x").unwrap();
        }
        let mut limits = ClearLimits::new();
        limits.entries = CLEAR_MAX_ENTRIES - 3;
        let _ = clear_dir_keeping_logs(&dir, &mut limits);
        assert!(limits.truncated, "超限应标记 truncated");
        assert!(
            fs::read_dir(&dir).unwrap().count() > 0,
            "超限后应有未处理文件"
        );
        fs::remove_dir_all(&dir).unwrap();
    }
}
