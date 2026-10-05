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
struct Target {
    kind: TargetKind,
    path: PathBuf,
}

/// 清理分类（面板一行）。
struct Category {
    key: &'static str,
    name: &'static str,
    description: &'static str,
    targets: Vec<Target>,
}

// ————— 分类构建 —————

/// 收集存在的文件目标（去重）。
fn push_file(targets: &mut Vec<Target>, seen: &mut HashSet<PathBuf>, path: &str) {
    let p = PathBuf::from(path);
    if p.is_file() && seen.insert(p.clone()) {
        targets.push(Target {
            kind: TargetKind::File,
            path: p,
        });
    }
}

/// 收集存在的目录目标（去重）。
fn push_dir(targets: &mut Vec<Target>, seen: &mut HashSet<PathBuf>, path: &str) {
    let p = PathBuf::from(path);
    if p.is_dir() && seen.insert(p.clone()) {
        targets.push(Target {
            kind: TargetKind::Dir,
            path: p,
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
    }

    #[cfg(windows)]
    {
        cats.push(Category {
            key: "system",
            name: "系统日志",
            description: "Windows 组件日志（CBS/DISM 更新与部署日志）",
            targets: windows_system_targets(),
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

    // 同分类内去重：被目录目标覆盖的子路径移除（避免重复计数与重复清理）
    for c in &mut cats {
        dedup_targets(&mut c.targets);
    }
    cats.retain(|c| !c.targets.is_empty());
    cats
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
            });
        } else if path.is_file() {
            targets.push(Target {
                kind: TargetKind::File,
                path,
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

#[cfg(windows)]
fn windows_system_targets() -> Vec<Target> {
    let mut targets = Vec::new();
    let mut seen = HashSet::new();
    for p in [
        "C:\\Windows\\Logs\\CBS\\CBS.log",
        "C:\\Windows\\Logs\\DISM\\dism.log",
    ] {
        push_file(&mut targets, &mut seen, p);
    }
    targets
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
            let Ok(md) = entry.metadata() else { continue };
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

/// 清空目录：`*.log` 文件截断保留（供仍持句柄的进程继续写入），
/// 其余文件删除、子目录递归清理（空后删除；含保留文件时保留目录）。
fn clear_dir_keeping_logs(dir: &Path) -> Result<(), String> {
    let rd = fs::read_dir(dir).map_err(|e| format!("无法读取目录 {}：{}", dir.display(), e))?;
    for entry in rd.flatten() {
        let p = entry.path();
        let Ok(md) = fs::symlink_metadata(&p) else {
            continue;
        };
        if md.is_dir() {
            clear_dir_keeping_logs(&p)?;
            let _ = fs::remove_dir(&p); // 子目录已清空则删除（含截断保留的 .log 时保留）
        } else if p
            .extension()
            .map(|e| e.eq_ignore_ascii_case("log"))
            .unwrap_or(false)
        {
            truncate_file(&p)?;
        } else {
            remove_any(&p)?;
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
            let (files, bytes, _) = dir_stats_limited(&t.path, DIR_SCAN_LIMIT);
            clear_dir_keeping_logs(&t.path)?;
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
pub fn log_clean_scan(panel: &WebPanel, ctx: &Ctx) -> ActionResult {
    if !panel.check_auth(ctx) {
        return json_error(401, "Unauthorized");
    }
    let cfg = panel.config();
    let categories = build_categories(panel.base(), &cfg);
    let mut total_size = 0u64;
    let mut total_count = 0u64;
    let list: Vec<Json> = categories
        .iter()
        .map(|c| {
            let mut size = 0u64;
            let mut count = 0u64;
            let mut truncated = false;
            let targets: Vec<Json> = c
                .targets
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
                "key": c.key,
                "name": c.name,
                "description": c.description,
                "size": size,
                "count": count,
                "truncated": truncated,
                "targets": targets,
            })
        })
        .collect();
    json_result(
        0,
        "",
        Some(json!({
            "categories": list,
            "totalSize": total_size,
            "totalCount": total_count,
        })),
    )
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

    let mut freed = 0u64;
    let mut files = 0u64;
    let mut details: Vec<Json> = Vec::new();
    for c in &selected {
        let mut cat_freed = 0u64;
        let mut cat_files = 0u64;
        let mut errors: Vec<String> = Vec::new();
        for t in &c.targets {
            match clean_target(t) {
                Ok((f, b)) => {
                    cat_files += f;
                    cat_freed += b;
                }
                Err(e) => errors.push(e),
            }
        }
        freed += cat_freed;
        files += cat_files;
        util::log_format(
            "日志清理：{} 释放 {} 字节（{} 个文件）{}",
            &[
                &c.name.to_string(),
                &cat_freed.to_string(),
                &cat_files.to_string(),
                &if errors.is_empty() {
                    String::new()
                } else {
                    format!("，{} 个错误", errors.len())
                },
            ],
        );
        details.push(json!({
            "key": c.key,
            "name": c.name,
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
            },
            Target {
                kind: TargetKind::Dir,
                path: child,
            },
            Target {
                kind: TargetKind::File,
                path: file,
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
}
