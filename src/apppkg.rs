//! 应用包一键部署（面板「📦 上传应用包」）。
//!
//! 目标：把发行包（`.zip` / `.tar.gz`）直接交给星尘，自动完成
//! 「解压 → 识别主程序 → 落到工作目录 → 注册子服务 → 启动」，不再需要人工填写
//! 文件名/工作目录/部署模式等参数。
//!
//! 与 [`crate::deploy`] 的分工：
//! - 本模块只把「一个发行包」变成「工作目录里的可运行文件 + 一条正确的子服务配置」；
//! - 影子复制、拉起、守护、探活仍由 [`crate::manager`] / [`crate::deploy`] 完成
//!   （配置统一按影子模式，升级=重复上传，旧版本影子目录自动清理）。
//!
//! 包布局兼容两类（实测 DHDeploy Agent 两种发布包）：
//! - 根级布局：`dhdeploy-agent-rust.exe` + `install.ps1`（Windows 包）；
//! - 顶层目录：`dhdeploy-agent-linux/dhdeploy-agent-rust` + `install.sh`（Linux 包）。

use std::fs;
use std::path::{Component, Path, PathBuf};

use crate::config::AppConfig;
use crate::manager::AppManager;
use crate::util;

/// 上传包大小上限（与程序升级一致）。
pub const MAX_PACKAGE_SIZE: usize = 64 * 1024 * 1024;
/// 解压后总大小上限（防 zip bomb 把节点磁盘写满）。
const MAX_UNPACKED_SIZE: u64 = 512 * 1024 * 1024;
/// 解压条目数上限（防海量小文件拖垮磁盘/文件句柄）。
const MAX_ENTRY_COUNT: usize = 20_000;

/// 部署结果（面板回显）。
#[derive(Debug, Clone)]
pub struct Deployed {
    /// 服务名
    pub name: String,
    /// 启动程序（工作目录内的绝对路径）
    pub program: PathBuf,
    /// 工作目录
    pub work_dir: PathBuf,
    /// 写入的文件数
    pub files: usize,
    /// 是否更新了已有服务
    pub updated: bool,
    /// 是否已启动（原本运行中会重启）
    pub started: bool,
}

/// 包类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PackageKind {
    /// zip（PK 魔数）
    Zip,
    /// tar.gz / tgz（gzip 魔数）
    TarGz,
}

/// 按魔数判定包类型（不信扩展名：浏览器/脚本常把文件名改成 .zip）。
fn package_kind(data: &[u8]) -> Result<PackageKind, String> {
    if data.starts_with(b"PK\x03\x04") || data.starts_with(b"PK\x05\x06") {
        return Ok(PackageKind::Zip);
    }
    if data.starts_with(&[0x1F, 0x8B]) {
        return Ok(PackageKind::TarGz);
    }
    Err("不支持的包格式：请上传 .zip 或 .tar.gz（.tgz）发行包".to_string())
}

/// 部署一个应用包。
///
/// - `package_name`：上传文件名（仅用于推导服务名与日志）；
/// - `name_hint`：用户指定的服务名（留空则按包内主程序名推导）；
/// - `arguments`：启动参数（留空则新建无参数 / 升级保留原有参数）；
/// - `auto_start`：部署后是否启动（原本运行中的服务会重启）。
pub fn deploy_bytes(
    manager: &AppManager,
    package_name: &str,
    data: &[u8],
    name_hint: Option<&str>,
    arguments: Option<&str>,
    auto_start: bool,
) -> Result<Deployed, String> {
    if data.is_empty() {
        return Err("上传内容为空".to_string());
    }
    if data.len() > MAX_PACKAGE_SIZE {
        return Err(format!(
            "包过大（{} MB，上限 {} MB）",
            data.len() / 1024 / 1024,
            MAX_PACKAGE_SIZE / 1024 / 1024
        ));
    }

    let base = manager.base().to_path_buf();
    let staging = staging_dir(&base)?;
    let rs = deploy_staged(
        manager,
        &staging,
        package_name,
        data,
        name_hint,
        arguments,
        auto_start,
    );
    // 无论成败都清理暂存目录（成功路径的文件已复制到工作目录）
    let _ = fs::remove_dir_all(&staging);
    rs
}

/// 暂存目录：`{base}/Data/apppkg-{pid}-{nanos}`（与工作目录同盘，保证原子改名可用）。
fn staging_dir(base: &Path) -> Result<PathBuf, String> {
    let data_dir = base.join("Data");
    fs::create_dir_all(&data_dir).map_err(|e| format!("创建 Data 目录失败：{e}"))?;
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = data_dir.join(format!("apppkg-{}-{}", std::process::id(), nonce));
    fs::create_dir_all(&dir).map_err(|e| format!("创建暂存目录失败：{e}"))?;
    Ok(dir)
}

/// 具体流程（暂存目录由调用方负责清理）。
fn deploy_staged(
    manager: &AppManager,
    staging: &Path,
    package_name: &str,
    data: &[u8],
    name_hint: Option<&str>,
    arguments: Option<&str>,
    auto_start: bool,
) -> Result<Deployed, String> {
    let kind = package_kind(data)?;
    let unpack = staging.join("unpack");
    fs::create_dir_all(&unpack).map_err(|e| format!("创建解压目录失败：{e}"))?;

    match kind {
        PackageKind::Zip => {
            let pkg = staging.join("package.zip");
            fs::write(&pkg, data).map_err(|e| format!("写入临时包失败：{e}"))?;
            check_zip_limits(&pkg)?;
            // 双重防护：先按中央目录快速拒绝，解压时再按**实际写入字节**限制（声明 size 不可信）
            dhrust::zip::extract_zip_limited(
                &pkg,
                &unpack,
                MAX_UNPACKED_SIZE,
                MAX_ENTRY_COUNT,
            )?;
        }
        PackageKind::TarGz => {
            let pkg = staging.join("package.tar.gz");
            fs::write(&pkg, data).map_err(|e| format!("写入临时包失败：{e}"))?;
            extract_tar_gz(&pkg, &unpack)?;
        }
    }

    // 顶层目录下沉：Linux 发布包常带一层 `dhdeploy-agent-linux/`
    let root = descend_root(&unpack);

    // 探测用名称：用户指定 > 包文件名（去扩展名与版本/平台后缀）
    let probe = name_hint
        .map(sanitize_name)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| sanitize_name(&package_stem(package_name)));

    let rel = find_program(&root, &probe)?;
    let program_src = root.join(&rel);
    ensure_platform(&program_src)?;

    // 服务名：用户指定 > 主程序名（`dhdeploy-agent-rust.exe` → `dhdeploy-agent-rust`）
    let name = name_hint
        .map(sanitize_name)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| sanitize_name(&program_stem(&rel)));
    if name.is_empty() {
        return Err("无法推导服务名称，请在名称一栏填写".to_string());
    }

    let base = manager.base().to_path_buf();
    let existing = manager.config().find_app(&name).cloned();
    let mut app = existing.clone().unwrap_or_else(|| AppConfig {
        name: name.clone(),
        mode: "shadow".to_string(),
        reload_on_change: true,
        ..Default::default()
    });

    // 工作目录：沿用已有配置；新应用按 deploy 约定（`{base}/../apps/{name}`）
    let work = crate::deploy::work_dir(&base, &app);
    let files = copy_tree(&root, &work)?;
    if files == 0 {
        return Err("包内没有可部署的文件".to_string());
    }

    let program = work.join(&rel);
    app.name = name.clone();
    app.file_name = program.display().to_string();
    if app
        .working_directory
        .as_deref()
        .map(|v| v.trim().is_empty())
        .unwrap_or(true)
    {
        app.working_directory = Some(work.display().to_string());
    }
    if app.mode.trim().is_empty() {
        app.mode = "shadow".to_string();
    }
    // 参数：显式传入则覆盖；留空时新建保持无参数、升级保留原参数
    if let Some(args) = arguments.map(str::trim).filter(|s| !s.is_empty()) {
        app.arguments = Some(args.to_string());
    }
    // 启用：升级沿用原状态；新部署按「部署后是否启动」决定
    app.enable = existing.as_ref().map(|e| e.enable).unwrap_or(auto_start);

    let was_running = manager
        .find_runtime(&name)
        .map(|rt| rt.status().running)
        .unwrap_or(false);

    manager.upsert_app(app);
    let action = if existing.is_some() { "更新" } else { "新增" };
    util::log_format(
        "部署应用包 {}{} → {}（{} 个文件，工作目录 {}）",
        &[
            action,
            name.as_str(),
            program.display().to_string().as_str(),
            files.to_string().as_str(),
            work.display().to_string().as_str(),
        ],
    );

    let mut started = false;
    if was_running {
        // 升级场景：文件已替换，重启使新版本生效（影子模式运行中也能安全替换）
        match manager.restart_app(&name, "应用包部署更新") {
            Ok(_) => started = true,
            Err(e) => util::log_error(&format!("应用[{name}]部署后重启失败：{e}")),
        }
    } else if auto_start {
        match manager.start_app(&name) {
            Ok(_) => started = true,
            Err(e) => util::log_error(&format!("应用[{name}]部署后启动失败：{e}")),
        }
    }

    Ok(Deployed {
        name,
        program,
        work_dir: work,
        files,
        updated: existing.is_some(),
        started,
    })
}

/// 解压 tar.gz（防路径穿越/链接逃逸/zip bomb；Unix 下保留可执行位）。
fn extract_tar_gz(pkg: &Path, target: &Path) -> Result<usize, String> {
    let file = fs::File::open(pkg).map_err(|e| format!("打开压缩包失败：{e}"))?;
    let reader = flate2::read::GzDecoder::new(std::io::BufReader::new(file));
    // 限制解压流读取量（防极端 gzip 炸弹；正常包远小于此值）
    let limited = std::io::Read::take(reader, MAX_UNPACKED_SIZE.saturating_add(64 * 1024));
    let mut archive = tar::Archive::new(limited);

    let mut count = 0usize;
    let mut total: u64 = 0;
    let entries = archive
        .entries()
        .map_err(|e| format!("读取 tar 失败：{e}"))?;
    for entry in entries {
        let mut entry = entry.map_err(|e| format!("读取 tar 条目失败：{e}"))?;
        let path = entry
            .path()
            .map_err(|e| format!("tar 条目路径非法：{e}"))?
            .into_owned();
        if is_unsafe_tar_path(&path) {
            return Err(format!("包内路径非法（路径穿越）：{}", path.display()));
        }
        let kind = entry.header().entry_type();
        if kind.is_symlink() || kind.is_hard_link() {
            return Err(format!(
                "包内包含符号链接/硬链接（拒绝，防链接逃逸）：{}",
                path.display()
            ));
        }
        if !kind.is_file() && !kind.is_dir() {
            return Err(format!("包内包含不支持的条目类型：{}", path.display()));
        }
        count += 1;
        if count > MAX_ENTRY_COUNT {
            return Err(format!("压缩包条目过多（上限 {MAX_ENTRY_COUNT}）"));
        }
        total = total.saturating_add(entry.size());
        if total > MAX_UNPACKED_SIZE {
            return Err(format!(
                "解压后总大小超过上限（{} MB）",
                MAX_UNPACKED_SIZE / 1024 / 1024
            ));
        }
        entry
            .unpack_in(target)
            .map_err(|e| format!("解压条目失败 {}：{e}", path.display()))?;
    }
    if count == 0 {
        return Err("压缩包为空".to_string());
    }
    Ok(count)
}

/// 预扫描 zip 条目数与解压后总大小（防 zip bomb；不读文件内容，只读中央目录）。
fn check_zip_limits(pkg: &Path) -> Result<(), String> {
    let file = fs::File::open(pkg).map_err(|e| format!("打开压缩包失败：{e}"))?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| format!("读取压缩包失败：{e}"))?;
    if archive.len() > MAX_ENTRY_COUNT {
        return Err(format!(
            "压缩包条目过多（{}，上限 {MAX_ENTRY_COUNT}）",
            archive.len()
        ));
    }
    let mut total: u64 = 0;
    for i in 0..archive.len() {
        let entry = archive
            .by_index(i)
            .map_err(|e| format!("读取压缩包条目失败：{e}"))?;
        // 符号链接条目直接拒绝（解压器不会创建链接，但避免后续误用）
        if entry.unix_mode().map(|m| m & 0o170000 == 0o120000).unwrap_or(false) {
            return Err(format!(
                "压缩包包含符号链接（拒绝）：{}",
                entry.name()
            ));
        }
        total = total.saturating_add(entry.size());
        if total > MAX_UNPACKED_SIZE {
            return Err(format!(
                "解压后总大小超过上限（{} MB）",
                MAX_UNPACKED_SIZE / 1024 / 1024
            ));
        }
    }
    Ok(())
}

/// tar 条目路径是否危险（绝对路径、盘符前缀、或含 `..` 上跳）。
fn is_unsafe_tar_path(path: &Path) -> bool {
    path.is_absolute()
        || path.components().any(|c| {
            matches!(
                c,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
}

/// 顶层目录下沉：包内只有一个目录时继续向下（最多 8 层；根级有文件即停止）。
fn descend_root(unpack: &Path) -> PathBuf {
    let mut cur = unpack.to_path_buf();
    for _ in 0..8 {
        let Ok(rd) = fs::read_dir(&cur) else {
            break;
        };
        let entries: Vec<_> = rd.flatten().collect();
        if entries.len() != 1 {
            break;
        }
        let only = entries[0].path();
        if !only.is_dir() {
            break;
        }
        cur = only;
    }
    cur
}

/// 在解压目录中查找主程序（返回相对 `root` 的路径）。
///
/// 顺序：
/// 1. 标准检索 [`crate::deploy::find_exe`]（名称精确匹配 / `名称.exe` / 唯一 exe /
///    `runtimeconfig.json` 配套 dll / `名称.dll` / `名称.jar`）——根目录优先，再逐层（≤2 层）；
/// 2. 启发式：当前平台的可执行文件（Windows 的 `.exe`；Unix 的「可执行位 + 非脚本」），
///    多个候选时优先无扩展名、再按名称排序取首个。
fn find_program(root: &Path, hint: &str) -> Result<PathBuf, String> {
    for dir in walk_dirs(root, 2) {
        let mut args = String::new();
        if let Some(p) = crate::deploy::find_exe(&dir, hint, &mut args) {
            return Ok(rel_of(root, &p));
        }
    }

    let mut candidates = Vec::new();
    collect_executables(root, 0, &mut candidates);
    if candidates.is_empty() {
        return Err(format!(
            "包内未找到可执行文件（探测名：{hint}）；请确认上传的是对应平台的发行包"
        ));
    }

    candidates.sort_by_key(|p| {
        let name = p
            .file_name()
            .map(|f| f.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        let has_ext = p.extension().is_some();
        // 无扩展名优先；名字越短越可能是主程序（install/readme 已被排除）
        (has_ext, name.len(), name)
    });
    Ok(rel_of(root, &candidates[0]))
}

/// 逐层目录（根优先，深度 ≤ `depth`）。
fn walk_dirs(root: &Path, depth: usize) -> Vec<PathBuf> {
    let mut dirs = vec![root.to_path_buf()];
    let mut frontier = vec![root.to_path_buf()];
    for _ in 0..depth {
        let mut next = Vec::new();
        for dir in frontier {
            let Ok(rd) = fs::read_dir(&dir) else {
                continue;
            };
            for entry in rd.flatten() {
                if entry.path().is_dir() {
                    let p = entry.path();
                    dirs.push(p.clone());
                    next.push(p);
                }
            }
        }
        if next.is_empty() {
            break;
        }
        frontier = next;
    }
    dirs
}

/// 递归收集可执行文件候选（深度 ≤2，跳过明显的脚本/说明文件）。
fn collect_executables(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    let Ok(rd) = fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if depth < 2 {
                collect_executables(&path, depth + 1, out);
            }
            continue;
        }
        if is_program_candidate(&path) {
            out.push(path);
        }
    }
}

/// 是否可能是主程序。
fn is_program_candidate(path: &Path) -> bool {
    let name = path
        .file_name()
        .map(|f| f.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if name.is_empty() {
        return false;
    }

    // 安装/说明/校验等辅助文件
    const EXCLUDED_SUFFIX: &[&str] = &[
        ".sh", ".ps1", ".bat", ".cmd", ".py", ".pl", ".js", ".md", ".txt", ".json", ".log",
        ".sums", ".sig", ".sha256", ".yaml", ".yml", ".toml", ".xml",
    ];
    if EXCLUDED_SUFFIX.iter().any(|s| name.ends_with(s)) {
        return false;
    }
    const EXCLUDED_PREFIX: &[&str] = &["install", "uninstall", "setup", "start.", "stop."];
    if EXCLUDED_PREFIX.iter().any(|s| name.starts_with(s)) {
        return false;
    }

    #[cfg(windows)]
    {
        name.ends_with(".exe")
    }
    #[cfg(not(windows))]
    {
        // Unix：要求「无扩展名」且（可执行位 或 ELF/Mach-O 文件头）
        if path.extension().is_some() {
            return false;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(md) = fs::metadata(path)
                && md.permissions().mode() & 0o111 != 0
            {
                return true;
            }
        }
        looks_like_binary(path)
    }
}

/// 文件头是否为本平台可执行格式（ELF / Mach-O / `#!` 脚本）。
#[cfg_attr(windows, allow(dead_code))]
fn looks_like_binary(path: &Path) -> bool {
    let Ok(head) = read_head(path, 4) else {
        return false;
    };
    matches!(
        head.as_slice(),
        [0x7F, b'E', b'L', b'F'] | [0xFE, 0xED, 0xFA, 0xCE] | [0xCE, 0xFA, 0xED, 0xFE]
    ) || head.starts_with(b"#!")
}

/// 平台兼容性检查：拒绝明显的跨平台包（Windows 传了 Linux 包 / 反之）。
fn ensure_platform(path: &Path) -> Result<(), String> {
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    // dll/jar 交由 dotnet/java 运行时，不在此判定
    if ext == "dll" || ext == "jar" {
        return Ok(());
    }

    let head = read_head(path, 4).unwrap_or_default();
    let is_elf = head == [0x7F, b'E', b'L', b'F'];
    let is_macho = matches!(
        head.as_slice(),
        [0xFE, 0xED, 0xFA, 0xCE] | [0xCE, 0xFA, 0xED, 0xFE] | [0xCF, 0xFA, 0xED, 0xFE]
    );
    #[cfg(not(windows))]
    let is_pe = head.starts_with(b"MZ");
    #[cfg(not(windows))]
    let is_script = head.starts_with(b"#!");

    #[cfg(windows)]
    if is_elf || is_macho {
        return Err("包内可执行文件不是 Windows 程序（是否上传了 Linux/macOS 包？）".to_string());
    }
    #[cfg(not(windows))]
    if is_pe {
        return Err("包内可执行文件是 Windows 程序（是否上传了 Windows 包？）".to_string());
    }
    #[cfg(not(windows))]
    if !(is_elf || is_macho || is_script) {
        return Err("包内主程序格式无法识别（期望 ELF/Mach-O 或脚本）".to_string());
    }
    Ok(())
}

/// 读取文件头 N 字节。
fn read_head(path: &Path, n: usize) -> Result<Vec<u8>, std::io::Error> {
    use std::io::Read;
    let mut f = fs::File::open(path)?;
    let mut buf = vec![0u8; n];
    let read = f.read(&mut buf)?;
    buf.truncate(read);
    Ok(buf)
}

/// 递归复制目录到工作目录（覆盖同名文件；跳过临时文件）。
fn copy_tree(src: &Path, dst: &Path) -> Result<usize, String> {
    fs::create_dir_all(dst).map_err(|e| format!("创建工作目录失败 {}：{e}", dst.display()))?;
    let mut count = 0usize;
    copy_tree_inner(src, dst, &mut count)?;
    Ok(count)
}

/// 递归复制实现。
fn copy_tree_inner(src: &Path, dst: &Path, count: &mut usize) -> Result<(), String> {
    let entries = fs::read_dir(src).map_err(|e| format!("读取目录失败 {}：{e}", src.display()))?;
    for entry in entries.flatten() {
        let path = entry.path();
        let target = dst.join(entry.file_name());
        if path.is_dir() {
            fs::create_dir_all(&target)
                .map_err(|e| format!("创建目录失败 {}：{e}", target.display()))?;
            copy_tree_inner(&path, &target, count)?;
            continue;
        }

        let lower = entry.file_name().to_string_lossy().to_ascii_lowercase();
        if lower.ends_with(".del") || lower.ends_with(".tmp") {
            continue;
        }
        crate::deploy::copy_file_safe(&path, &target)
            .map_err(|e| format!("复制文件失败 {}：{e}", target.display()))?;
        *count += 1;
    }
    Ok(())
}

/// 相对路径（跨盘/异常时回退为绝对路径）。
fn rel_of(root: &Path, path: &Path) -> PathBuf {
    path.strip_prefix(root).unwrap_or(path).to_path_buf()
}

/// 主程序名（去扩展名）。
fn program_stem(rel: &Path) -> String {
    rel.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "app".to_string())
}

/// 包文件名去扩展名（`.tar.gz` 视为一个扩展名）。
fn package_stem(package_name: &str) -> String {
    let mut name = package_name.trim().to_string();
    let lower = name.to_ascii_lowercase();
    for suffix in [".tar.gz", ".tgz", ".zip"] {
        if lower.ends_with(suffix) {
            name.truncate(name.len() - suffix.len());
            break;
        }
    }
    // 去掉常见平台与版本后缀（`dhdeploy-agent-windows-x64-v1.0.41` → `dhdeploy-agent`）
    let mut cut = name.len();
    let lower = name.to_ascii_lowercase();
    for marker in [
        "-windows", "-linux", "-win", "-x64", "-x86", "-arm64", "-aarch64", "-riscv64",
        "-loongarch64", "-musl", "-v0", "-v1", "-v2", "-v3", "-v4", "-v5", "-v6", "-v7", "-v8",
        "-v9",
    ] {
        if let Some(i) = lower.find(marker)
            && i > 0
            && i < cut
        {
            cut = i;
        }
    }
    if cut > 0 {
        name.truncate(cut);
    }
    name
}

/// 名称规范化：仅保留 `[A-Za-z0-9._-]`，其余替换为 `-`，折叠连续分隔符，截断 64。
pub fn sanitize_name(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut last_sep = false;
    for c in text.trim().chars() {
        if c.is_ascii_alphanumeric() || c == '.' || c == '_' {
            out.push(c);
            last_sep = false;
        } else {
            if !last_sep && !out.is_empty() {
                out.push('-');
                last_sep = true;
            }
        }
    }
    // 去掉前导分隔符（`../etc` → `etc`）与尾部分隔符
    while out.starts_with('-') || out.starts_with('.') {
        out.remove(0);
    }
    while out.ends_with('-') || out.ends_with('.') {
        out.pop();
    }
    if out.len() > 64 {
        out.truncate(64);
        while out.ends_with('-') || out.ends_with('.') {
            out.pop();
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ragent-apppkg-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn sanitize_name_rules() {
        assert_eq!(sanitize_name("dhdeploy-agent-rust"), "dhdeploy-agent-rust");
        assert_eq!(sanitize_name("DH Deploy Agent"), "DH-Deploy-Agent");
        assert_eq!(sanitize_name("../etc/passwd"), "etc-passwd");
        assert_eq!(sanitize_name("   "), "");
        assert_eq!(sanitize_name("a//b"), "a-b");
        assert!(sanitize_name(&"x".repeat(100)).len() <= 64);
    }

    #[test]
    fn package_stem_strips_platform_and_version() {
        assert_eq!(
            package_stem("dhdeploy-agent-windows-x64-v1.0.41.zip"),
            "dhdeploy-agent"
        );
        assert_eq!(
            package_stem("dhdeploy-agent-linux-v1.0.41.tar.gz"),
            "dhdeploy-agent"
        );
        assert_eq!(package_stem("myapp.zip"), "myapp");
    }

    #[test]
    fn descend_root_skips_single_top_dir() {
        let tmp = temp_dir("descend");
        let inner = tmp.join("pack").join("dhdeploy-agent-linux");
        std::fs::create_dir_all(&inner).unwrap();
        std::fs::write(inner.join("dhdeploy-agent-rust"), b"\x7FELF").unwrap();
        assert_eq!(descend_root(&tmp.join("pack")), inner);

        // 根级有文件时不继续下沉
        let flat = temp_dir("flat");
        std::fs::write(flat.join("a.exe"), b"MZ").unwrap();
        std::fs::create_dir_all(flat.join("sub")).unwrap();
        assert_eq!(descend_root(&flat), flat);
    }

    #[test]
    fn find_program_prefers_name_and_falls_back_to_single_exe() {
        let tmp = temp_dir("find");
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("README.md"), b"readme").unwrap();
        if cfg!(windows) {
            std::fs::write(tmp.join("helper.exe"), b"MZ").unwrap();
            std::fs::write(tmp.join("dhdeploy-agent-rust.exe"), b"MZ").unwrap();
        } else {
            std::fs::write(tmp.join("install.sh"), b"#!/bin/sh").unwrap();
            let bin = tmp.join("dhdeploy-agent-rust");
            std::fs::write(&bin, b"\x7FELF").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mut perm = std::fs::metadata(&bin).unwrap().permissions();
                perm.set_mode(0o755);
                std::fs::set_permissions(&bin, perm).unwrap();
            }
        }
        let rel = find_program(&tmp, "dhdeploy-agent-rust").unwrap();
        assert_eq!(program_stem(&rel), "dhdeploy-agent-rust");
    }

    #[test]
    fn extract_tar_gz_roundtrip_rejects_traversal() {
        let tmp = temp_dir("tar");
        let pkg = tmp.join("p.tar.gz");
        {
            let file = std::fs::File::create(&pkg).unwrap();
            let enc = flate2::write::GzEncoder::new(file, flate2::Compression::fast());
            let mut builder = tar::Builder::new(enc);
            let mut header = tar::Header::new_gnu();
            header.set_size(5);
            header.set_mode(0o755);
            header.set_cksum();
            builder
                .append_data(&mut header, "app/bin", &b"hello"[..])
                .unwrap();
            builder.finish().unwrap();
        }
        let dst = tmp.join("out");
        std::fs::create_dir_all(&dst).unwrap();
        assert_eq!(extract_tar_gz(&pkg, &dst).unwrap(), 1);
        assert!(dst.join("app").join("bin").is_file());

        // 路径穿越判定（tar crate 的 append_data 会拒绝构造恶意条目，故直接测判别函数）
        assert!(is_unsafe_tar_path(Path::new("../escape.txt")));
        assert!(is_unsafe_tar_path(Path::new("app/../../escape.txt")));
        assert!(is_unsafe_tar_path(Path::new("/etc/passwd")));
        assert!(!is_unsafe_tar_path(Path::new("app/bin")));
        assert!(!is_unsafe_tar_path(Path::new("dhdeploy-agent-linux/install.sh")));
    }

    #[test]
    fn deploy_bytes_registers_service_without_start() {
        use crate::config::AgentConfig;

        let tmp = temp_dir("deploy");
        let base = tmp.join("agent");
        std::fs::create_dir_all(base.join("Config")).unwrap();
        let manager = AppManager::new(&base, AgentConfig::default());

        // 构造一个简单的 zip 包：顶层目录 + 主程序
        let pkg_path = tmp.join("demo.zip");
        {
            let file = std::fs::File::create(&pkg_path).unwrap();
            let mut zw = zip::ZipWriter::new(file);
            let opts: zip::write::SimpleFileOptions = zip::write::SimpleFileOptions::default();
            zw.start_file("demo-app/README.md", opts).unwrap();
            use std::io::Write;
            zw.write_all(b"readme").unwrap();
            #[cfg(windows)]
            let bin = "demo-app/demo-app.exe";
            #[cfg(not(windows))]
            let bin = "demo-app/demo-app";
            zw.start_file(bin, opts).unwrap();
            #[cfg(windows)]
            zw.write_all(b"MZ\x90\x00").unwrap();
            #[cfg(not(windows))]
            zw.write_all(b"\x7FELF\x02\x01\x01").unwrap();
            zw.finish().unwrap();
        }
        let data = std::fs::read(&pkg_path).unwrap();

        let out = deploy_bytes(&manager, "demo-app-v1.0.zip", &data, None, None, false).unwrap();
        assert_eq!(out.name, "demo-app");
        assert!(!out.updated);
        assert!(!out.started);
        assert!(out.program.is_file(), "主程序应存在于工作目录");

        let cfg = manager.config();
        let app = cfg.find_app("demo-app").expect("配置应已注册");
        assert_eq!(app.mode_text(), "shadow");
        assert!(app.file_name.ends_with(if cfg!(windows) {
            "demo-app.exe"
        } else {
            "demo-app"
        }));

        // 再次部署同一包 → 更新而非新增，参数保留
        let out2 = deploy_bytes(
            &manager,
            "demo-app-v1.0.zip",
            &data,
            None,
            Some("--print-table"),
            false,
        )
        .unwrap();
        assert!(out2.updated);
        let app2 = manager.config().find_app("demo-app").cloned().unwrap();
        assert_eq!(app2.arguments.as_deref(), Some("--print-table"));

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
