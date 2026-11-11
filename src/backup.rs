//! 部署前备份：把目标目录按过滤规则打成 zip，放在同级目录。
//!
//! **两条部署链路共用本模块**（不各写一份）：
//! - 直连部署：`deploy_http::run` 的 `overlay` / `overwrite` 分支；
//! - 平台中转：`panel_ws` 的 `appUpgrade` / `appDeploy` / `deployOverwrite`，
//!   最终都落到 `apppkg::overwrite_into` / `apppkg::deploy_staged`，同样在这里备份。
//!
//! 过滤口径对齐 C# 版 Agent 的 `DeployService.CompressWithFilter`：日志、缓存、
//! 构建产物、上传目录、SQLite 临时文件不进备份，避免备份体积失控
//! （用户要求：「避免备份文件太大，因为有些文件是不需要备份的」）。

use std::fs;
use std::path::{Path, PathBuf};

/// 同一应用保留的备份份数（更早的自动删除）。
pub const BACKUP_KEEP: usize = 5;

/// 排除的目录名（任一层命中即整棵跳过，大小写不敏感）。
const SKIP_DIRS: &[&str] = &[
    "log", "logs", "temp", "tmp", "cache", ".git", ".vs", "obj", "node_modules", "uploads",
];

/// 排除的扩展名（大小写不敏感）。
const SKIP_EXTS: &[&str] = &[
    ".log", ".tmp", ".temp", ".cache", ".bak", ".old", ".db-wal", ".db-shm", ".db-journal",
    // 备份文件本身不该再进备份（历史上把 .backup-*.zip 放同目录时尤其要注意）
    ".zip",
];

/// 排除的相对路径（小写、正斜杠）。
const SKIP_PATHS: &[&str] = &["data/log.db"];

/// 备份结果。
#[derive(Clone, Debug)]
pub struct Backup {
    /// 备份文件路径
    pub file: PathBuf,
    /// 已备份文件数
    pub files: usize,
    /// 按排除规则跳过的文件数
    pub skipped: usize,
    /// 本次清理掉的旧备份份数
    pub pruned: usize,
}

impl Backup {
    /// 一句话摘要（拼日志/回执用）。
    pub fn summary(&self) -> String {
        let tail = if self.pruned > 0 {
            format!("，清理旧备份 {} 份", self.pruned)
        } else {
            String::new()
        };
        format!(
            "已备份 {} 个文件到 {}（跳过 {} 个日志/缓存类文件{}）",
            self.files,
            self.file.display(),
            self.skipped,
            tail
        )
    }
}

/// 部署前备份。
///
/// - 目标目录不存在或为空 → 返回 `Ok(None)`（新建应用没有旧内容可备份）；
/// - 备份失败 → 返回 `Err`，**调用方应中止部署**（宁可这次不部署，也不要无备份覆盖）。
pub fn backup_target(target: &Path, app_name: &str) -> Result<Option<Backup>, String> {
    if !target.is_dir() {
        return Ok(None);
    }
    let parent = target.parent().ok_or_else(|| {
        format!(
            "部署目录没有父目录，无法放置备份：{}",
            target.display()
        )
    })?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let name = sanitize(app_name);
    let file = parent.join(format!(".backup-{name}-{stamp}.zip"));

    let f = fs::File::create(&file)
        .map_err(|e| format!("创建备份文件失败（{}）：{e}", file.display()))?;
    let mut z = zip::ZipWriter::new(f);
    let opts = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .large_file(false);

    let mut files = 0usize;
    let mut skipped = 0usize;
    let walked = walk(target, target, &mut z, opts, &mut files, &mut skipped);
    if let Err(e) = walked {
        let _ = z.finish();
        let _ = fs::remove_file(&file);
        return Err(format!("备份失败（{}）：{e}", target.display()));
    }
    if let Err(e) = z.finish() {
        let _ = fs::remove_file(&file);
        return Err(format!("写出备份失败（{}）：{e}", file.display()));
    }
    if files == 0 {
        // 目录里只有日志/缓存之类的可跳过内容：不留下空备份
        let _ = fs::remove_file(&file);
        return Ok(None);
    }
    let pruned = cleanup_old_backups(parent, &name);
    Ok(Some(Backup {
        file,
        files,
        skipped,
        pruned,
    }))
}

/// 清理同应用的历史备份，只保留最近 [`BACKUP_KEEP`] 份。
fn cleanup_old_backups(parent: &Path, name: &str) -> usize {
    let prefix = format!(".backup-{name}-");
    let mut found: Vec<(u64, PathBuf)> = Vec::new();
    let Ok(rd) = fs::read_dir(parent) else {
        return 0;
    };
    for e in rd.filter_map(|x| x.ok()) {
        let p = e.path();
        let Some(fname) = p.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if !fname.starts_with(&prefix) || !fname.ends_with(".zip") {
            continue;
        }
        // 时间戳是 epoch 秒：按数值排序即可（比字符串排序稳）
        let ts: u64 = fname[prefix.len()..]
            .trim_end_matches(".zip")
            .parse()
            .unwrap_or(0);
        found.push((ts, p));
    }
    if found.len() <= BACKUP_KEEP {
        return 0;
    }
    found.sort_by(|a, b| b.0.cmp(&a.0)); // 新的在前
    let mut pruned = 0usize;
    for (_, p) in found.into_iter().skip(BACKUP_KEEP) {
        if fs::remove_file(&p).is_ok() {
            pruned += 1;
        }
    }
    pruned
}

/// 递归写 zip，按排除规则跳过。
fn walk(
    root: &Path,
    dir: &Path,
    z: &mut zip::ZipWriter<fs::File>,
    opts: zip::write::SimpleFileOptions,
    files: &mut usize,
    skipped: &mut usize,
) -> Result<(), String> {
    let rd = fs::read_dir(dir).map_err(|e| format!("读取目录失败（{}）：{e}", dir.display()))?;
    let mut entries: Vec<PathBuf> = rd.filter_map(|x| x.ok().map(|x| x.path())).collect();
    entries.sort();
    for path in entries {
        let fname = path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        if fname.is_empty() {
            continue;
        }
        let lower_name = fname.to_ascii_lowercase();
        let rel = path
            .strip_prefix(root)
            .map_err(|_| format!("路径越界：{}", path.display()))?;
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        let rel_lower = rel_str.to_ascii_lowercase();

        if SKIP_PATHS.iter().any(|p| rel_lower == *p || rel_lower.starts_with(&format!("{p}/"))) {
            *skipped += 1;
            continue;
        }
        if path.is_dir() {
            if SKIP_DIRS.contains(&lower_name.as_str()) {
                *skipped += 1;
                continue;
            }
            walk(root, &path, z, opts, files, skipped)?;
            continue;
        }
        let ext = path
            .extension()
            .map(|e| format!(".{}", e.to_string_lossy().to_ascii_lowercase()))
            .unwrap_or_default();
        if !ext.is_empty() && SKIP_EXTS.contains(&ext.as_str()) {
            *skipped += 1;
            continue;
        }
        let data = fs::read(&path).map_err(|e| format!("读取文件失败（{}）：{e}", path.display()))?;
        z.start_file(rel_str.as_str(), opts.unix_permissions(0o644))
            .map_err(|e| format!("写备份条目失败（{rel_str}）：{e}"))?;
        use std::io::Write;
        z.write_all(&data)
            .map_err(|e| format!("写备份内容失败（{rel_str}）：{e}"))?;
        *files += 1;
    }
    Ok(())
}

/// 备份文件名里的应用名归一化（去掉路径分隔符等危险字符）。
fn sanitize(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let cleaned = cleaned.trim_matches('.').to_string();
    if cleaned.is_empty() {
        "app".to_string()
    } else {
        cleaned
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "ra-backup-test-{tag}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    /// 备份会带过滤：日志/缓存/构建产物进不去，普通文件进得去。
    #[test]
    fn backup_skips_logs_and_caches() {
        let root = temp_dir("skip");
        let app = root.join("web");
        fs::create_dir_all(app.join("Logs")).unwrap();
        fs::create_dir_all(app.join("node_modules")).unwrap();
        fs::write(app.join("app.dll"), b"bin").unwrap();
        fs::write(app.join("appsettings.json"), b"{}").unwrap();
        fs::write(app.join("run.log"), b"noise").unwrap();
        fs::write(app.join("Logs/today.log"), b"noise").unwrap();
        fs::write(app.join("node_modules/x.js"), b"noise").unwrap();

        let b = backup_target(&app, "demo").unwrap().expect("应产生备份");
        assert_eq!(b.files, 2, "只有 app.dll 与 appsettings.json 进备份");
        assert!(b.skipped >= 3, "日志/依赖目录应被跳过");
        assert!(b.file.file_name().unwrap().to_string_lossy().starts_with(".backup-demo-"));

        let f = fs::File::open(&b.file).unwrap();
        let mut z = zip::ZipArchive::new(f).unwrap();
        let mut names: Vec<String> = (0..z.len())
            .map(|i| z.by_index(i).unwrap().name().to_string())
            .collect();
        names.sort();
        assert_eq!(names, vec!["app.dll", "appsettings.json"]);
        let _ = fs::remove_dir_all(&root);
    }

    /// 目标目录不存在时返回 None（新建应用无需备份）。
    #[test]
    fn backup_returns_none_for_missing_dir() {
        let root = temp_dir("missing");
        let b = backup_target(&root.join("nope"), "demo").unwrap();
        assert!(b.is_none());
        let _ = fs::remove_dir_all(&root);
    }

    /// 只保留最近 N 份。
    #[test]
    fn backup_keeps_only_latest() {
        let root = temp_dir("keep");
        let app = root.join("web");
        fs::create_dir_all(&app).unwrap();
        fs::write(app.join("a.txt"), b"x").unwrap();
        // 造 N+2 份历史备份
        for i in 0..(BACKUP_KEEP + 2) {
            fs::write(root.join(format!(".backup-demo-{}0000.zip", i + 1)), b"old").unwrap();
        }
        let b = backup_target(&app, "demo").unwrap().unwrap();
        assert!(b.pruned > 0, "超过保留份数应清理");
        let left = fs::read_dir(&root)
            .unwrap()
            .filter_map(|x| x.ok())
            .filter(|e| {
                let n = e.file_name().to_string_lossy().to_string();
                n.starts_with(".backup-demo-") && n.ends_with(".zip")
            })
            .count();
        assert_eq!(left, BACKUP_KEEP, "最终应只剩 {BACKUP_KEEP} 份");
        let _ = fs::remove_dir_all(&root);
    }
}
