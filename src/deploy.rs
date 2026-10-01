//! 部署：解压部署包、影子目录、可执行文件检索、运行中文件安全替换。
//!
//! 与 C# StarAgent 的部署策略对齐（`ShadowDeployStrategy` / `StandardDeployStrategy` /
//! `HostedStrategy` / `TaskStrategy`）：
//! - **Shadow（默认）**：解压到 `{工作目录}/../shadow/{名称}-{zip的MD5前8位}`，进程在影子目录运行；
//!   工作目录保持干净（只放配置与数据），运行中的文件永不占用工作目录，
//!   因此上传/覆盖新版本文件时不会被占用，重启后自动切换到新影子目录；
//! - **Standard**：解压到工作目录运行；
//! - **Hosted**：仅解压，由外部宿主（IIS/Nginx）运行；Windows 下带 IIS 离线切换处理；
//! - **Task**：运行一次，不守护，完成后自动禁用；
//! - 替换被占用文件：先尝试原子改名；失败则把目标文件改名为 `*.del`（Windows 允许重命名运行中的文件），
//!   再写入新文件；`*.del` 在应用停止后清理。

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::config::{AgentConfig, AppConfig};
use crate::util;

/// 部署模式（数值与 C# `DeployMode` 对齐，便于配置互通）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeployMode {
    /// 标准：解压到工作目录运行
    Standard = 10,
    /// 影子：解压到影子目录运行（默认）
    Shadow = 11,
    /// 托管：仅解压，外部宿主运行
    Hosted = 12,
    /// 任务：运行一次
    Task = 13,
}

impl DeployMode {
    /// 解析模式文本（名称或数值；兼容旧版 0-4 数值）。
    pub fn parse(text: &str) -> DeployMode {
        match text.trim() {
            "" => DeployMode::Shadow,
            s if s.eq_ignore_ascii_case("standard") || s == "10" => DeployMode::Standard,
            s if s.eq_ignore_ascii_case("shadow") || s == "11" => DeployMode::Shadow,
            s if s.eq_ignore_ascii_case("hosted") || s == "12" => DeployMode::Hosted,
            s if s.eq_ignore_ascii_case("task") || s == "13" => DeployMode::Task,
            // 旧版模式：0=Default→Shadow，1=Extract→Hosted，2=ExtractAndRun→Standard，3=RunOnce→Task，4=Multiple→Shadow
            "0" | "4" => DeployMode::Shadow,
            "1" => DeployMode::Hosted,
            "2" => DeployMode::Standard,
            "3" => DeployMode::Task,
            _ => DeployMode::Shadow,
        }
    }

    /// 名称。
    pub fn as_str(self) -> &'static str {
        match self {
            DeployMode::Standard => "standard",
            DeployMode::Shadow => "shadow",
            DeployMode::Hosted => "hosted",
            DeployMode::Task => "task",
        }
    }
}

/// IIS 应用离线页面。
const APP_OFFLINE_HTML: &str = "<!DOCTYPE html><html><head><meta charset=\"utf-8\"/><title>应用维护中</title></head><body><h1>应用正在更新，请稍候...</h1></body></html>";

/// 部署准备上下文。
pub struct PrepareContext<'a> {
    /// 基础目录
    pub base: &'a Path,
    /// 全局配置
    pub global: &'a AgentConfig,
    /// 是否处于多次重启的调试状态
    pub retry: bool,
    /// 显式指定影子目录（zip 发布会话使用）
    pub shadow_override: Option<&'a Path>,
}

/// 部署准备结果。
pub struct Prepared {
    /// 启动程序（可执行文件、dotnet、java 或系统命令）
    pub program: String,
    /// 参数
    pub args: Vec<String>,
    /// 环境变量
    pub envs: Vec<(String, String)>,
    /// 工作目录
    pub work_dir: PathBuf,
    /// 实际运行文件
    pub run_file: Option<PathBuf>,
    /// 影子目录
    pub shadow: Option<PathBuf>,
    /// 是否托管模式（不拉起进程）
    pub hosted: bool,
    /// 是否任务模式（运行一次）
    pub task: bool,
    /// 部署模式
    pub mode: DeployMode,
}

/// 计算应用的工作目录（与 C# `Fix` 约定一致）。
pub fn work_dir(base: &Path, app: &AppConfig) -> PathBuf {
    if let Some(wd) = app.working_directory.as_deref() {
        if !wd.trim().is_empty() {
            return util::resolve(base, wd);
        }
    }

    let file_name = app.file_name.trim();
    if !file_name.is_empty() && (file_name.contains('/') || file_name.contains('\\')) {
        let p = util::resolve(base, file_name);
        if let Some(parent) = p.parent() {
            return parent.to_path_buf();
        }
    }

    util::resolve(base, &format!("../apps/{}", app.name))
}

/// 定位 zip 包：绝对路径直接判断，相对路径先在工作目录找，再按基础目录解析。
fn locate_zip(base: &Path, work: &Path, file_name: &str) -> Option<PathBuf> {
    if !file_name.to_ascii_lowercase().ends_with(".zip") {
        return None;
    }

    let p = Path::new(file_name);
    if p.is_absolute() {
        return p.is_file().then(|| p.to_path_buf());
    }

    let cand = work.join(p);
    if cand.is_file() {
        return Some(cand);
    }

    let cand = util::resolve(base, file_name);
    cand.is_file().then_some(cand)
}

/// 部署准备：按模式解压部署包并解析出可执行文件与启动参数。
pub fn prepare(ctx: &PrepareContext, app: &AppConfig) -> Result<Prepared, String> {
    let work = work_dir(ctx.base, app);
    std::fs::create_dir_all(&work).map_err(|e| format!("创建工作目录失败 {}：{}", work.display(), e))?;

    let mode = DeployMode::parse(&app.mode_text());
    if ctx.retry {
        util::log_format("重新部署应用[{}]", &[&app.name]);
    }
    let file_name = app.file_name.trim().to_string();
    let mut args_text = app.arguments.clone().unwrap_or_default();
    let zip = locate_zip(ctx.base, &work, &file_name);

    let mut shadow: Option<PathBuf> = None;
    let mut run_file: Option<PathBuf> = None;
    let mut hosted = false;

    match mode {
        DeployMode::Shadow => {
            if let Some(z) = &zip {
                let hash = dhrust::sign::md5_file_hex(z).map_err(|e| format!("计算压缩包哈希失败：{}", e))?;
                let hash8 = hash.get(..8).unwrap_or(&hash).to_ascii_lowercase();

                let sdir = match ctx.shadow_override {
                    Some(s) => s.join(format!("{}-{}", app.name, hash8)),
                    None => shadow_base(&work).join(format!("{}-{}", app.name, hash8)),
                };

                if !sdir.is_dir() {
                    clean_old_shadows(
                        sdir.parent().unwrap_or(Path::new(".")),
                        &app.name,
                    );
                    util::log_format("影子模式，解压到影子目录：{}", &[&sdir.display().to_string()]);
                    extract_zip(z, &sdir)?;

                    copy_config_to_workdir(&sdir, &work);
                    copy_overwrite_files(&sdir, &work, app.overwrite.as_deref());
                }

                shadow = Some(sdir.clone());
                run_file = find_exe(&sdir, &app.name, &mut args_text);
            } else {
                util::log_format("影子模式降级为标准模式（未找到 {}）", &[&file_name]);
                run_file = find_exe(&work, &app.name, &mut args_text);
            }
        }
        DeployMode::Standard | DeployMode::Task => {
            if let Some(z) = &zip {
                util::log_format("解压到工作目录：{}", &[&work.display().to_string()]);
                extract_zip(z, &work)?;
            }
            run_file = find_exe(&work, &app.name, &mut args_text);
        }
        DeployMode::Hosted => {
            if let Some(z) = &zip {
                util::log_format("托管模式，解压到工作目录：{}", &[&work.display().to_string()]);
                extract_zip_hosted(z, &work)?;
            } else {
                return Err(format!("托管模式需要 zip 包：{}", file_name));
            }
            hosted = true;
        }
    }

    // 解析启动程序与参数
    let (program, args, final_run_file) = match run_file {
        Some(rf) => build_program(rf, &args_text),
        None => {
            if file_name.is_empty() {
                return Err("文件名为空".to_string());
            }

            if file_name.contains('/') || file_name.contains('\\') {
                let full = util::resolve(ctx.base, &file_name);
                if full.is_file() {
                    build_program(full, &args_text)
                } else {
                    return Err(format!("无法找到可执行文件：{}", full.display()));
                }
            } else {
                // 不含路径分隔符的简单命令名（如 ping），按系统命令通过 PATH 解析
                (file_name.clone(), dhrust::io::split_args(&args_text), None)
            }
        }
    };

    let envs = build_envs(&work, app, ctx.global, &program, final_run_file.as_deref());
    let task = mode == DeployMode::Task;

    Ok(Prepared {
        program,
        args,
        envs,
        work_dir: work,
        run_file: final_run_file,
        shadow,
        hosted,
        task,
        mode,
    })
}

/// 组装启动程序与参数（.dll → dotnet，.jar → java）。
fn build_program(run_file: PathBuf, args_text: &str) -> (String, Vec<String>, Option<PathBuf>) {
    let ext = run_file
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    let mut args = dhrust::io::split_args(args_text);

    match ext.as_str() {
        "dll" => {
            let mut v = vec![run_file.to_string_lossy().into_owned()];
            v.append(&mut args);
            ("dotnet".to_string(), v, Some(run_file))
        }
        "jar" => {
            let mut v = vec!["-jar".to_string(), run_file.to_string_lossy().into_owned()];
            v.append(&mut args);
            ("java".to_string(), v, Some(run_file))
        }
        _ => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if let Ok(meta) = std::fs::metadata(&run_file) {
                    let mut perm = meta.permissions();
                    perm.set_mode(perm.mode() | 0o755);
                    let _ = std::fs::set_permissions(&run_file, perm);
                }
            }
            (run_file.to_string_lossy().into_owned(), args, Some(run_file))
        }
    }
}

/// 组装环境变量（BasePath、自定义、GC 上限、启动挂钩）。
fn build_envs(
    work: &Path,
    app: &AppConfig,
    global: &AgentConfig,
    program: &str,
    run_file: Option<&Path>,
) -> Vec<(String, String)> {
    let mut envs = vec![("BasePath".to_string(), work.to_string_lossy().into_owned())];

    if let Some(text) = app.environments.as_deref() {
        envs.extend(dhrust::io::parse_environments(text));
    }

    let run_is_dll = run_file
        .and_then(|p| p.extension())
        .map(|e| e.to_string_lossy().eq_ignore_ascii_case("dll"))
        .unwrap_or(false);
    if app.max_memory > 0 && (program.eq_ignore_ascii_case("dotnet") || run_is_dll) {
        let bytes = app.max_memory as u64 * 1024 * 1024;
        envs.push(("DOTNET_GCHeapHardLimit".to_string(), format!("{:x}", bytes)));
    }

    if global.startup_hook {
        if let Some(dir) = run_file.and_then(|p| p.parent()) {
            let has_runtimeconfig = std::fs::read_dir(dir)
                .map(|it| {
                    it.flatten().any(|e| {
                        e.file_name()
                            .to_string_lossy()
                            .to_ascii_lowercase()
                            .ends_with(".runtimeconfig.json")
                    })
                })
                .unwrap_or(false);
            let hook = dir.join("Stardust.dll");
            if has_runtimeconfig && hook.is_file() {
                envs.push((
                    "DOTNET_STARTUP_HOOKS".to_string(),
                    hook.to_string_lossy().into_owned(),
                ));
            }
        }
    }

    envs
}

/// 影子目录基础路径：`{工作目录}/../shadow`；无权限时退到系统临时目录。
pub fn shadow_base(work: &Path) -> PathBuf {
    let base = util::lexical_normalize(&work.join("../shadow"));
    if std::fs::create_dir_all(&base).is_ok() {
        return base;
    }

    let tmp = std::env::temp_dir().join("pek-ragent-shadow");
    let _ = std::fs::create_dir_all(&tmp);
    tmp
}

/// 清理指定应用的历史影子目录（保留当前版本由调用方控制）。
pub fn clean_old_shadows(shadow_base: &Path, name: &str) {
    let Ok(entries) = std::fs::read_dir(shadow_base) else {
        return;
    };

    let prefix = format!("{}-", name.to_ascii_lowercase());
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let dir_name = entry.file_name().to_string_lossy().to_ascii_lowercase();
        if dir_name.starts_with(&prefix) {
            util::log_format("删除旧版影子目录 {}", &[&path.display().to_string()]);
            let _ = std::fs::remove_dir_all(&path);
        }
    }
}

/// 把影子目录中的配置文件拷贝到工作目录（仅当工作目录不存在同名文件）。
fn copy_config_to_workdir(shadow: &Path, work: &Path) {
    let Ok(entries) = std::fs::read_dir(shadow) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let ext = path
            .extension()
            .map(|e| e.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        if !matches!(ext.as_str(), "json" | "config" | "xml" | "yml" | "ini") {
            continue;
        }

        let Some(name) = path.file_name() else { continue };
        let dst = work.join(name);
        if !dst.exists() {
            util::log_format("拷贝配置文件 {}", &[&name.to_string_lossy()]);
            let _ = std::fs::copy(&path, &dst);
        }
    }
}

/// 拷贝“覆盖文件/子目录”到工作目录（支持 `*` 模糊匹配，`;` 分隔）。
fn copy_overwrite_files(shadow: &Path, work: &Path, overwrite: Option<&str>) {
    let Some(text) = overwrite else { return };
    let patterns: Vec<&str> = text
        .split(';')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();
    if patterns.is_empty() {
        return;
    }

    let Ok(entries) = std::fs::read_dir(shadow) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();

        let matched = patterns.iter().any(|pat| {
            let pat_clean = pat.trim_end_matches(['*', '/']).trim_end_matches('/');
            dhrust::io::wildcard_match(pat, &name)
                || pat_clean.eq_ignore_ascii_case(&name)
                || pat.trim_end_matches('*').trim_end_matches('/').eq_ignore_ascii_case(&name)
        });
        if !matched {
            continue;
        }

        if path.is_file() {
            let dst = work.join(&name);
            util::log_format("覆盖文件 {}", &[&name]);
            let _ = copy_file_safe(&path, &dst);
        } else if path.is_dir() {
            util::log_format("覆盖目录 {}", &[&name]);
            copy_dir_recursive(&path, &work.join(&name));
        }
    }
}

/// 安全拷贝文件（占用时改名替换）。
pub fn copy_file_safe(src: &Path, dst: &Path) -> std::io::Result<()> {
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let tmp = PathBuf::from(format!("{}.{}.tmp", dst.display(), std::process::id()));
    std::fs::copy(src, &tmp)?;
    safe_replace_file(&tmp, dst)
}

/// 递归拷贝目录（覆盖已存在文件）。
fn copy_dir_recursive(src: &Path, dst: &Path) {
    let _ = std::fs::create_dir_all(dst);
    let Ok(entries) = std::fs::read_dir(src) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let target = dst.join(entry.file_name());
        if path.is_dir() {
            copy_dir_recursive(&path, &target);
        } else {
            let _ = copy_file_safe(&path, &target);
        }
    }
}

/// 安全替换文件：
/// 1. 原子改名（Unix 可直接覆盖；Windows 目标未占用时亦可）；
/// 2. 目标被占用（运行中）时，把目标改名为 `*.del` 再写入新文件（Windows 允许重命名运行中的文件）；
///    `*.del` 删除失败不报错，待应用停止后由 `cleanup_temp_files` 清理。
pub fn safe_replace_file(src: &Path, dst: &Path) -> std::io::Result<()> {
    if !dst.exists() {
        return std::fs::rename(src, dst).or_else(|_| {
            std::fs::copy(src, dst)?;
            let _ = std::fs::remove_file(src);
            Ok(())
        });
    }

    match std::fs::rename(src, dst) {
        Ok(()) => Ok(()),
        Err(_) => {
            let bak = del_path(dst);
            std::fs::rename(dst, &bak)?;
            match std::fs::rename(src, dst) {
                Ok(()) => {
                    // 运行中的文件删除会失败，留给后续清理
                    let _ = std::fs::remove_file(&bak);
                    Ok(())
                }
                Err(e) => {
                    // 回滚
                    let _ = std::fs::rename(&bak, dst);
                    Err(e)
                }
            }
        }
    }
}

/// 生成唯一的 `*.del` 路径。
fn del_path(dst: &Path) -> PathBuf {
    let base = PathBuf::from(format!("{}.del", dst.display()));
    if !base.exists() {
        return base;
    }

    for i in 1..10_000 {
        let candidate = PathBuf::from(format!("{}.{}.del", dst.display(), i));
        if !candidate.exists() {
            return candidate;
        }
    }

    base
}

/// 清理目录中的 `*.del` 与 `*.tmp` 临时文件（尽力而为）。
pub fn cleanup_temp_files(dir: &Path, recursive: bool) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if recursive {
                    cleanup_temp_files(&path, true);
                }
                continue;
            }

            let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
            if name.ends_with(".del") || name.ends_with(".tmp") {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
}

/// 解压 zip 到目标目录（防目录穿越；占用文件安全替换；保留可执行位）。
pub fn extract_zip(zip_path: &Path, target: &Path) -> Result<usize, String> {
    let file = std::fs::File::open(zip_path)
        .map_err(|e| format!("打开压缩包失败 {}：{}", zip_path.display(), e))?;
    let mut archive =
        zip::ZipArchive::new(file).map_err(|e| format!("读取压缩包失败：{}", e))?;

    std::fs::create_dir_all(target).map_err(|e| format!("创建目录失败：{}", e))?;

    let mut count = 0usize;
    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| format!("读取压缩包条目失败：{}", e))?;

        let Some(rel) = entry.enclosed_name() else {
            continue; // 目录穿越条目，跳过
        };
        let out_path = target.join(rel);

        if entry.is_dir() {
            std::fs::create_dir_all(&out_path).map_err(|e| format!("创建目录失败：{}", e))?;
            continue;
        }

        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("创建目录失败：{}", e))?;
        }

        let mut data = Vec::with_capacity(entry.size() as usize);
        std::io::copy(&mut entry, &mut data).map_err(|e| format!("解压数据失败：{}", e))?;

        let tmp = PathBuf::from(format!(
            "{}.{}.{}.tmp",
            out_path.display(),
            std::process::id(),
            i
        ));
        std::fs::write(&tmp, &data).map_err(|e| format!("写入临时文件失败：{}", e))?;
        safe_replace_file(&tmp, &out_path)
            .map_err(|e| format!("替换文件失败 {}：{}", out_path.display(), e))?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Some(mode) = entry.unix_mode() {
                if mode & 0o111 != 0 {
                    let _ = std::fs::set_permissions(
                        &out_path,
                        std::fs::Permissions::from_mode(mode | 0o755),
                    );
                }
            }
        }

        count += 1;
    }

    Ok(count)
}

/// 托管模式解压：Windows + IIS（存在 web.config）时先离线再更新，避免文件占用。
fn extract_zip_hosted(zip_path: &Path, work: &Path) -> Result<usize, String> {
    let web_config = work.join("web.config");
    let offline = work.join("app_offline.htm");
    let mut backup: Option<PathBuf> = None;

    if cfg!(windows) && web_config.is_file() {
        let bak = PathBuf::from(format!("{}.bak", web_config.display()));
        let _ = std::fs::write(&offline, APP_OFFLINE_HTML);
        if std::fs::copy(&web_config, &bak).is_ok() {
            let _ = std::fs::remove_file(&web_config);
            backup = Some(bak);
        }
        std::thread::sleep(Duration::from_millis(1_000));
    }

    let rs = extract_zip(zip_path, work);

    if let Some(bak) = backup {
        if web_config.is_file() {
            // 压缩包中已有新 web.config，保留新版本
            let _ = std::fs::remove_file(&bak);
        } else {
            let _ = std::fs::copy(&bak, &web_config);
            let _ = std::fs::remove_file(&bak);
        }
    }
    let _ = std::fs::remove_file(&offline);

    rs
}

/// 检索可执行文件（顺序与 C# `DeployStrategyBase.FindExeFile` 对齐）。
///
/// 1. 名称完全匹配（不区分大小写）；2. Windows 下 `{名称}.exe`；
/// 3. 第一个参数可能就是可执行文件（命中后从参数中移除）；
/// 4. Windows 下唯一 exe；5. `*.runtimeconfig.json` 配套 dll；
/// 6. `{名称}.dll`；7. `{名称}.jar`。
pub fn find_exe(dir: &Path, name: &str, arguments: &mut String) -> Option<PathBuf> {
    let entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .collect();

    let find = |file_name: &str| -> Option<PathBuf> {
        entries
            .iter()
            .find(|p| {
                p.file_name()
                    .map(|f| f.to_string_lossy().eq_ignore_ascii_case(file_name))
                    .unwrap_or(false)
            })
            .cloned()
    };

    // 1. 名称完全匹配
    if let Some(p) = find(name) {
        return Some(p);
    }

    // 2. name.exe（仅 Windows）
    if cfg!(windows) {
        if let Some(p) = find(&format!("{}.exe", name)) {
            return Some(p);
        }
    }

    // 3. 第一个参数可能就是可执行文件
    let text = arguments.trim();
    if !text.is_empty() {
        let (first, rest) = match text.find(char::is_whitespace) {
            Some(i) => (text[..i].to_string(), text[i + 1..].trim_start().to_string()),
            None => (text.to_string(), String::new()),
        };
        let file_part = Path::new(&first)
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_else(|| first.clone());
        if let Some(p) = find(&file_part) {
            *arguments = rest;
            return Some(p);
        }
    }

    // 4. Windows 下唯一 exe
    if cfg!(windows) {
        let exes: Vec<PathBuf> = entries
            .iter()
            .filter(|p| {
                p.extension()
                    .map(|e| e.to_string_lossy().eq_ignore_ascii_case("exe"))
                    .unwrap_or(false)
            })
            .cloned()
            .collect();
        if exes.len() == 1 {
            return Some(exes[0].clone());
        }
    }

    // 5. runtimeconfig.json 配套 dll
    const RUNTIME_CONFIG: &str = ".runtimeconfig.json";
    if let Some(cfg_file) = entries.iter().find(|p| {
        p.file_name()
            .map(|f| f.to_string_lossy().to_ascii_lowercase().ends_with(RUNTIME_CONFIG))
            .unwrap_or(false)
    }) {
        let fname = cfg_file.file_name().unwrap().to_string_lossy().into_owned();
        let dll_name = format!("{}.dll", &fname[..fname.len() - RUNTIME_CONFIG.len()]);
        if let Some(p) = find(&dll_name) {
            return Some(p);
        }
    }

    // 6. name.dll
    if let Some(p) = find(&format!("{}.dll", name)) {
        return Some(p);
    }

    // 7. name.jar
    if let Some(p) = find(&format!("{}.jar", name)) {
        return Some(p);
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_parsing() {
        assert_eq!(DeployMode::parse(""), DeployMode::Shadow);
        assert_eq!(DeployMode::parse("Shadow"), DeployMode::Shadow);
        assert_eq!(DeployMode::parse("11"), DeployMode::Shadow);
        assert_eq!(DeployMode::parse("0"), DeployMode::Shadow);
        assert_eq!(DeployMode::parse("2"), DeployMode::Standard);
        assert_eq!(DeployMode::parse("standard"), DeployMode::Standard);
        assert_eq!(DeployMode::parse("3"), DeployMode::Task);
    }

    #[test]
    fn find_exe_prefers_exact_name() {
        let dir = std::env::temp_dir().join(format!("ragent-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join("app.dll"), b"dll").unwrap();
        std::fs::write(dir.join("readme.txt"), b"txt").unwrap();

        let mut args = String::new();
        let found = find_exe(&dir, "app", &mut args).unwrap();
        assert_eq!(found.file_name().unwrap(), "app.dll");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg(windows)]
    fn find_exe_single_exe_on_windows() {
        let dir = std::env::temp_dir().join(format!("ragent-test-single-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join("only.exe"), b"exe").unwrap();

        let mut args = String::new();
        let found = find_exe(&dir, "nomatch", &mut args).unwrap();
        assert_eq!(found.file_name().unwrap(), "only.exe");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn find_exe_consumes_first_argument() {
        let dir = std::env::temp_dir().join(format!("ragent-test2-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join("cube.dll"), b"dll").unwrap();

        let mut args = "cube.dll urls=http://*:1080".to_string();
        let found = find_exe(&dir, "nothing", &mut args).unwrap();
        assert_eq!(found.file_name().unwrap(), "cube.dll");
        assert_eq!(args, "urls=http://*:1080");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn safe_replace_overwrites() {
        let dir = std::env::temp_dir().join(format!("ragent-test3-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let src = dir.join("new.txt");
        let dst = dir.join("old.txt");
        std::fs::write(&src, b"new").unwrap();
        std::fs::write(&dst, b"old").unwrap();

        safe_replace_file(&src, &dst).unwrap();
        assert_eq!(std::fs::read(&dst).unwrap(), b"new");
        assert!(!src.exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn extract_zip_roundtrip() {
        let dir = std::env::temp_dir().join(format!("ragent-test4-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let zip_path = dir.join("a.zip");

        {
            let file = std::fs::File::create(&zip_path).unwrap();
            let mut writer = zip::ZipWriter::new(file);
            let options: zip::write::FileOptions<()> =
                zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Stored);
            writer.start_file("sub/hello.txt", options).unwrap();
            use std::io::Write;
            writer.write_all(b"hello").unwrap();
            writer.finish().unwrap();
        }

        let out = dir.join("out");
        let n = extract_zip(&zip_path, &out).unwrap();
        assert_eq!(n, 1);
        assert_eq!(std::fs::read(out.join("sub/hello.txt")).unwrap(), b"hello");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
