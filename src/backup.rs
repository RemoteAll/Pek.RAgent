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
///
/// 与 C# `DeployService.CompressWithFilter` 的 `excludeDirs` 对齐：
/// `Log / Logs / Temp / Cache / .git / .vs / obj / node_modules / Uploads`。
/// （C# 另列了 `wwwroot\Uploads`，这里按「目录名 = uploads」匹配，任意层级都已覆盖。）
const SKIP_DIRS: &[&str] = &[
    "log", "logs", "temp", "tmp", "cache", ".git", ".vs", "obj", "node_modules", "uploads",
];

/// 排除的扩展名（大小写不敏感）。
///
/// **故意不跳过 `.zip`**（C# 也不跳）：业务资源包常是 zip，跳过会让备份不完整。
/// 备份文件本身落在 Agent 数据目录，不会自我包含。
const SKIP_EXTS: &[&str] = &[
    ".log", ".tmp", ".temp", ".cache", ".bak", ".old", ".db-wal", ".db-shm", ".db-journal",
];

/// 排除的相对路径（小写、正斜杠）——命中即整个目录/文件及其子路径全部跳过。
///
/// - `data/log.db`：C# 的 `Data/Log.db`（系统日志库，运行中持续写入，备份它等于拷一个活动库）；
/// - `bin/debug`、`bin/release`：C# `excludeDirs` 的 `bin\\Debug` / `bin\\Release`。
///   注意是**按路径**而不是把整个 `bin` 整棵跳过，避免误伤某些项目自己的 `bin` 资源目录。
const SKIP_PATHS: &[&str] = &["data/log.db", "bin/debug", "bin/release"];

/// 单文件上限：超过就跳过（C# 同为 100MB）。
///
/// 备份是「出事了能回滚」用，不是灾难备份：一个 2GB 的数据文件会把备份拖成分钟级，
/// 还容易被误认为卡死；真要完整备份应该停服务后单独做。
const MAX_FILE_SIZE: u64 = 100 * 1024 * 1024;

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

/// 备份根目录：`{Agent 数据目录}/Data/DeployBackups/{应用}/`。
///
/// **绝不能放在部署目录同级**：部署目录常位于宝塔站点根（`/www/wwwroot/<域名>/`）之下，
/// 同级放一个含源码/配置的 zip 会变成 `https://域名/.backup-xxx.zip` 可直接下载的泄露面。
/// （`wwwroot` 是 .NET 项目自己的静态资源目录，和站点根不是一回事。）
pub fn backup_dir(backup_root: &Path, app_name: &str) -> PathBuf {
    backup_root
        .join("Data")
        .join("DeployBackups")
        .join(sanitize(app_name))
}

/// 部署前备份。
///
/// - `backup_root`：Agent 数据目录（备份落在 `Data/DeployBackups/{应用}/` 下）；
/// - 目标目录不存在或为空 → 返回 `Ok(None)`（新建应用没有旧内容可备份）；
/// - 备份失败 → 返回 `Err`，**调用方应中止部署**（宁可这次不部署，也不要无备份覆盖）。
pub fn backup_target(
    target: &Path,
    app_name: &str,
    backup_root: &Path,
) -> Result<Option<Backup>, String> {
    if !target.is_dir() {
        return Ok(None);
    }
    let dir = backup_dir(backup_root, app_name);
    fs::create_dir_all(&dir)
        .map_err(|e| format!("创建备份目录失败（{}）：{e}", dir.display()))?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let file = dir.join(format!(".backup-{stamp}.zip"));

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
    let pruned = cleanup_old_backups(&dir);
    Ok(Some(Backup {
        file,
        files,
        skipped,
        pruned,
    }))
}

/// 清理历史备份，只保留最近 [`BACKUP_KEEP`] 份。
fn cleanup_old_backups(dir: &Path) -> usize {
    const PREFIX: &str = ".backup-";
    let mut found: Vec<(u64, PathBuf)> = Vec::new();
    let Ok(rd) = fs::read_dir(dir) else {
        return 0;
    };
    for e in rd.filter_map(|x| x.ok()) {
        let p = e.path();
        let Some(fname) = p.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if !fname.starts_with(PREFIX) || !fname.ends_with(".zip") {
            continue;
        }
        // 时间戳是 epoch 秒：按数值排序即可（比字符串排序稳）
        let ts: u64 = fname[PREFIX.len()..]
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
        let size = path.metadata().map(|m| m.len()).unwrap_or(0);
        if size > MAX_FILE_SIZE {
            // 与 C# 同口径：超大单文件不进备份（备份要能在分钟级完成，否则等同于卡死）
            *skipped += 1;
            continue;
        }
        z.start_file(rel_str.as_str(), opts.unix_permissions(0o644))
            .map_err(|e| format!("写备份条目失败（{rel_str}）：{e}"))?;
        {
            // 流式写入：不把整个文件读进内存（活动数据库/大资源包也不会撑爆内存）
            let mut f = fs::File::open(&path)
                .map_err(|e| format!("打开文件失败（{}）：{e}", path.display()))?;
            std::io::copy(&mut f, z)
                .map_err(|e| format!("写备份内容失败（{rel_str}）：{e}"))?;
        }
        *files += 1;    }
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

        let b = backup_target(&app, "demo", &root).unwrap().expect("应产生备份");
        assert_eq!(b.files, 2, "只有 app.dll 与 appsettings.json 进备份");
        assert!(b.skipped >= 3, "日志/依赖目录应被跳过");
        // 落点必须在 Agent 数据目录下，**不能落在部署目录同级**
        assert!(
            b.file.starts_with(root.join("Data").join("DeployBackups").join("demo")),
            "备份必须落在 Data/DeployBackups 下，实际：{}",
            b.file.display()
        );
        assert!(
            !b.file.starts_with(&app),
            "备份绝不能落在部署目录里：{}",
            b.file.display()
        );

        let f = fs::File::open(&b.file).unwrap();
        let mut z = zip::ZipArchive::new(f).unwrap();
        let mut names: Vec<String> = (0..z.len())
            .map(|i| z.by_index(i).unwrap().name().to_string())
            .collect();
        names.sort();
        assert_eq!(names, vec!["app.dll", "appsettings.json"]);
        let _ = fs::remove_dir_all(&root);
    }

    /// 与 C# `CompressWithFilter` 口径对齐：
    /// `Data/Log.db`、`bin/Debug|Release` 中间产物、`.log` 不进备份；业务 zip 必须备份（C# 也不跳）。
    #[test]
    fn backup_matches_csharp_filter_rules() {
        let root = temp_dir("csharp");
        let app = root.join("web");
        fs::create_dir_all(app.join("Data")).unwrap();
        fs::create_dir_all(app.join("bin/Debug/net8.0")).unwrap();
        fs::create_dir_all(app.join("res")).unwrap();
        fs::write(app.join("App.dll"), b"dll").unwrap();
        fs::write(app.join("Data/DH.db"), b"db").unwrap();
        fs::write(app.join("Data/Log.db"), b"logmsg").unwrap();
        fs::write(app.join("bin/Debug/net8.0/App.dll"), b"noise").unwrap();
        fs::write(app.join("res/plugin.zip"), b"zipdata").unwrap();
        fs::write(app.join("run.log"), b"noise").unwrap();

        let b = backup_target(&app, "demo", &root)
            .unwrap()
            .expect("应产生备份");
        let f = fs::File::open(&b.file).unwrap();
        let mut z = zip::ZipArchive::new(f).unwrap();
        let mut names: Vec<String> = (0..z.len())
            .map(|i| z.by_index(i).unwrap().name().to_string())
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec!["App.dll", "Data/DH.db", "res/plugin.zip"],
            "Log.db / bin 中间产物 / .log 不进备份；业务 zip 必须备份"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// 目标目录不存在时返回 None（新建应用无需备份）。
    #[test]
    fn backup_returns_none_for_missing_dir() {
        let root = temp_dir("missing");
        let b = backup_target(&root.join("nope"), "demo", &root).unwrap();
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
        // 在**新落点**（Agent 数据目录）造 N+2 份历史备份
        let dir = backup_dir(&root, "demo");
        fs::create_dir_all(&dir).unwrap();
        for i in 0..(BACKUP_KEEP + 2) {
            fs::write(dir.join(format!(".backup-{}0000.zip", i + 1)), b"old").unwrap();
        }
        let b = backup_target(&app, "demo", &root).unwrap().unwrap();
        assert!(b.pruned > 0, "超过保留份数应清理");
        let left = fs::read_dir(backup_dir(&root, "demo"))
            .unwrap()
            .filter_map(|x| x.ok())
            .filter(|e| {
                let n = e.file_name().to_string_lossy().to_string();
                n.starts_with(".backup-") && n.ends_with(".zip")
            })
            .count();
        assert_eq!(left, BACKUP_KEEP, "最终应只剩 {BACKUP_KEEP} 份");
        let _ = fs::remove_dir_all(&root);
    }
}
