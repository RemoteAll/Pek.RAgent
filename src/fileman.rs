//! 文件管理（Web 面板「文件管理」页）：浏览、上传/下载、在线编辑、新建、删除、
//! 重命名、复制/移动、压缩/解压、权限修改与搜索。
//!
//! 安全模型（对齐宝塔文件管理）：
//! - 全部动作走面板鉴权（`WebPanel::check_auth`，随 `WebAuthLevel` 生效）；
//! - 路径词法归一化（相对路径按程序基础目录解析），不访问文件系统即可消除 `.`/`..`；
//! - **删除/重命名/移动**对“根”“一级目录（如 `/etc`、`C:\Windows`）”“程序目录本身”
//!   设保护（目录内部仍可操作）——防止误操作损毁系统；
//! - 上传文件名与压缩包条目名做目录穿越拦截（`..`、绝对路径、分隔符）；
//! - 下载上限 256MB、在线编辑上限 2MB（二进制拒读），大文件请用 SFTP。

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Local};
use dhrust::net::controller::{arg, json_body, json_error, json_result, ActionResult};
use dhrust::net::http::HttpResponse;
use dhrust::net::router::Ctx;
use serde_json::{json, Value as Json};

use crate::util;
use crate::webpanel::WebPanel;

/// 在线编辑的文件大小上限（2MB）。
const MAX_EDIT_SIZE: u64 = 2 * 1024 * 1024;
/// 面板下载的文件大小上限（256MB）。
const MAX_DOWNLOAD_SIZE: u64 = 256 * 1024 * 1024;
/// 批量操作（删除/复制/移动/压缩）单次条目上限。
const MAX_BATCH: usize = 100;
/// 搜索返回条数与扫描条数上限。
const SEARCH_MAX_RESULTS: usize = 200;
const SEARCH_MAX_SCAN: usize = 20_000;

// ————— 路径与保护 —————

/// 归一化用户提交的路径（相对路径按 `base` 解析；不访问文件系统）。
pub(crate) fn resolve_path(base: &Path, input: &str) -> Result<PathBuf, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("缺少路径参数".to_string());
    }
    let p = Path::new(trimmed);
    let joined = if p.is_absolute() {
        p.to_path_buf()
    } else {
        base.join(p)
    };
    Ok(util::lexical_normalize(&joined))
}

/// 是否受保护路径（禁止删除/重命名/移动**其本身**；目录内部不禁止）。
///
/// - 根（`/`、盘符根 `C:\`）；
/// - 一级目录（如 `/etc`、`/usr`、`C:\Windows`、`C:\Program Files`）——内部可操作；
/// - 程序基础目录本身。
pub(crate) fn is_protected(path: &Path, base: &Path) -> bool {
    let p = util::lexical_normalize(path);
    // 根 / 盘符根（无父目录）
    let Some(parent) = p.parent() else {
        return true;
    };
    // 一级目录（父目录即根）
    if parent.parent().is_none() {
        return true;
    }
    // 程序目录本身
    if p == util::lexical_normalize(base) {
        return true;
    }
    false
}

/// 修改类操作的保护校验。
fn ensure_modifiable(path: &Path, base: &Path) -> Result<(), String> {
    if is_protected(path, base) {
        return Err(format!(
            "该路径受保护，禁止删除/重命名/移动：{}",
            path.display()
        ));
    }
    Ok(())
}

/// 校验文件/目录名称（无分隔符、非 `.`/`..`、长度合理）。
fn validate_name(name: &str) -> Result<(), String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("名称不能为空".to_string());
    }
    if name == "." || name == ".." {
        return Err("名称不合法".to_string());
    }
    if name.contains('/') || name.contains('\\') {
        return Err("名称不能包含路径分隔符".to_string());
    }
    if name.len() > 255 {
        return Err("名称过长".to_string());
    }
    Ok(())
}

/// 取文件/目录的显示名。
fn file_label(path: &Path) -> String {
    path.file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| path.display().to_string())
}

/// 快捷入口（前端“常用位置”）：Linux 常见目录 / Windows 盘符 + 程序目录。
fn quick_roots(base: &Path) -> Vec<String> {
    let mut roots: Vec<String> = Vec::new();
    #[cfg(unix)]
    {
        for p in ["/", "/www", "/opt", "/var/log", "/tmp"] {
            if Path::new(p).is_dir() {
                roots.push(p.to_string());
            }
        }
    }
    #[cfg(windows)]
    {
        for letter in b'A'..=b'Z' {
            let root = format!("{}:\\", letter as char);
            if Path::new(&root).exists() {
                roots.push(root);
            }
        }
    }
    let base_text = base.display().to_string();
    if !roots.iter().any(|r| r == &base_text) {
        roots.push(base_text);
    }
    roots
}

// ————— 元数据展示 —————

/// 时间格式化（本地时间）。
fn fmt_time(t: std::time::SystemTime) -> String {
    let dt: DateTime<Local> = t.into();
    dt.format("%Y-%m-%d %H:%M:%S").to_string()
}

/// 权限文本（Unix 八进制；Windows 为空）。
#[cfg(unix)]
fn perm_text(md: &fs::Metadata) -> String {
    use std::os::unix::fs::PermissionsExt;
    format!("{:o}", md.permissions().mode() & 0o7777)
}

#[cfg(windows)]
fn perm_text(_md: &fs::Metadata) -> String {
    String::new()
}

/// 目录递归统计（文件数, 字节数）。
fn dir_stats(path: &Path) -> (u64, u64) {
    let mut files = 0u64;
    let mut bytes = 0u64;
    let Ok(rd) = fs::read_dir(path) else {
        return (files, bytes);
    };
    for entry in rd.flatten() {
        let p = entry.path();
        let Ok(md) = entry.metadata() else { continue };
        if md.is_dir() {
            let (f, b) = dir_stats(&p);
            files += f;
            bytes += b;
        } else {
            files += 1;
            bytes += md.len();
        }
    }
    (files, bytes)
}

/// 目录条目 JSON（列表接口单条）。
fn entry_json(name: String, path: &Path, md: &fs::Metadata, is_link: bool) -> Json {
    // 符号链接按跟随后的类型展示（可进入指向的目录）
    let is_dir = if is_link {
        fs::metadata(path).map(|m| m.is_dir()).unwrap_or(false)
    } else {
        md.is_dir()
    };
    json!({
        "name": name,
        "isDir": is_dir,
        "isLink": is_link,
        "size": if is_dir { 0 } else { md.len() },
        "mtime": md.modified().map(fmt_time).unwrap_or_default(),
        "perm": perm_text(md),
    })
}

/// 列出目录内容（目录在前，再按名称不区分大小写排序）。
fn list_dir(dir: &Path) -> Result<Vec<Json>, String> {
    let rd = fs::read_dir(dir).map_err(|e| format!("无法读取目录 {}：{}", dir.display(), e))?;
    let mut items: Vec<(bool, String, Json)> = Vec::new();
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        // DirEntry::metadata 不跟随符号链接（链接本身元数据）
        let Ok(md) = entry.metadata() else { continue };
        let is_link = md.file_type().is_symlink();
        let item = entry_json(name.clone(), &entry.path(), &md, is_link);
        let is_dir = item.get("isDir").and_then(|v| v.as_bool()).unwrap_or(false);
        items.push((is_dir, name.to_lowercase(), item));
    }
    items.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| a.1.cmp(&b.1))
    });
    Ok(items.into_iter().map(|(_, _, v)| v).collect())
}

// ————— 文件系统操作 —————

/// 删除文件或目录（符号链接只删链接本身）；返回（文件数, 字节数）。
fn remove_path(path: &Path) -> Result<(u64, u64), String> {
    let md = fs::symlink_metadata(path)
        .map_err(|e| format!("无法访问 {}：{}", path.display(), e))?;
    let ft = md.file_type();
    if ft.is_symlink() {
        // 文件链接与目录链接（含 Windows junction）统一处理
        if fs::remove_dir(path).is_err() {
            fs::remove_file(path).map_err(|e| format!("删除失败 {}：{}", path.display(), e))?;
        }
        Ok((1, 0))
    } else if ft.is_dir() {
        let stats = dir_stats(path);
        fs::remove_dir_all(path).map_err(|e| format!("删除失败 {}：{}", path.display(), e))?;
        Ok(stats)
    } else {
        let size = md.len();
        fs::remove_file(path).map_err(|e| format!("删除失败 {}：{}", path.display(), e))?;
        Ok((1, size))
    }
}

/// 递归复制（符号链接按普通文件复制其内容；指向目录的链接跳过）。
fn copy_recursive(src: &Path, dst: &Path) -> Result<(u64, u64), String> {
    let lm = fs::symlink_metadata(src)
        .map_err(|e| format!("无法访问 {}：{}", src.display(), e))?;
    if lm.file_type().is_symlink() {
        let follow = fs::metadata(src);
        if follow.map(|m| m.is_dir()).unwrap_or(false) {
            return Ok((0, 0)); // 指向目录的链接：跳过（防循环）
        }
        let size = lm.len();
        fs::copy(src, dst).map_err(|e| format!("复制失败 {}：{}", src.display(), e))?;
        return Ok((1, size));
    }
    if lm.is_dir() {
        fs::create_dir_all(dst).map_err(|e| format!("创建目录失败 {}：{}", dst.display(), e))?;
        let mut files = 0u64;
        let mut bytes = 0u64;
        let rd = fs::read_dir(src).map_err(|e| format!("无法读取目录 {}：{}", src.display(), e))?;
        for entry in rd.flatten() {
            let (f, b) = copy_recursive(&entry.path(), &dst.join(entry.file_name()))?;
            files += f;
            bytes += b;
        }
        Ok((files, bytes))
    } else {
        let size = lm.len();
        fs::copy(src, dst).map_err(|e| format!("复制失败 {}：{}", src.display(), e))?;
        Ok((1, size))
    }
}

/// 修改权限（仅 Unix；`mode` 为八进制值）。
#[cfg(unix)]
fn chmod(path: &Path, mode: u32) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let perms = fs::Permissions::from_mode(mode & 0o7777);
    fs::set_permissions(path, perms)
        .map_err(|e| format!("设置权限失败 {}：{}", path.display(), e))
}

#[cfg(windows)]
fn chmod(_path: &Path, _mode: u32) -> Result<(), String> {
    Err("修改权限仅支持 Linux/macOS".to_string())
}

// ————— ZIP 压缩 / 解压 —————

/// ZIP 写入选项（Deflate 压缩）。
fn zip_options() -> zip::write::SimpleFileOptions {
    zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .unix_permissions(0o644)
}

/// 目录递归加入 zip（空目录也保留条目）。
fn zip_add_dir(
    zip: &mut zip::ZipWriter<fs::File>,
    dir: &Path,
    prefix: &str,
    skip: &Path,
    files: &mut u64,
) -> Result<(), String> {
    zip.add_directory(format!("{prefix}/"), zip_options())
        .map_err(|e| format!("写入压缩包失败：{}", e))?;
    let rd = fs::read_dir(dir).map_err(|e| format!("无法读取目录 {}：{}", dir.display(), e))?;
    for entry in rd.flatten() {
        let p = entry.path();
        if p == skip {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        let rel = format!("{prefix}/{name}");
        let Ok(md) = entry.metadata() else { continue };
        if md.is_dir() {
            zip_add_dir(zip, &p, &rel, skip, files)?;
        } else {
            let Ok(data) = fs::read(&p) else { continue };
            zip.start_file(rel, zip_options())
                .map_err(|e| format!("写入压缩包失败：{}", e))?;
            zip.write_all(&data)
                .map_err(|e| format!("写入压缩包失败：{}", e))?;
            *files += 1;
        }
    }
    Ok(())
}

/// 压缩多个路径到 zip 文件；返回（文件数, 压缩包字节数）。
fn compress_zip(dst: &Path, sources: &[PathBuf]) -> Result<(u64, u64), String> {
    let file = fs::File::create(dst).map_err(|e| format!("创建压缩包失败：{}", e))?;
    let mut zip = zip::ZipWriter::new(file);
    let mut files = 0u64;
    for src in sources {
        if src == dst {
            continue;
        }
        let name = file_label(src);
        if name.is_empty() {
            continue;
        }
        let Ok(md) = fs::symlink_metadata(src) else {
            continue;
        };
        if md.is_dir() {
            zip_add_dir(&mut zip, src, &name, dst, &mut files)?;
        } else if md.is_file() {
            let Ok(data) = fs::read(src) else { continue };
            zip.start_file(name, zip_options())
                .map_err(|e| format!("写入压缩包失败：{}", e))?;
            zip.write_all(&data)
                .map_err(|e| format!("写入压缩包失败：{}", e))?;
            files += 1;
        }
    }
    zip.finish().map_err(|e| format!("完成压缩包失败：{}", e))?;
    let size = fs::metadata(dst).map(|m| m.len()).unwrap_or(0);
    Ok((files, size))
}

/// 解压 zip 到目标目录（目录穿越条目跳过）；返回（文件数, 字节数）。
fn extract_zip_file(src: &Path, target: &Path) -> Result<(u64, u64), String> {
    let file = fs::File::open(src).map_err(|e| format!("无法打开 {}：{}", src.display(), e))?;
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|e| format!("不是有效的 zip 文件（{}）：{}", src.display(), e))?;
    let mut files = 0u64;
    let mut bytes = 0u64;
    for i in 0..archive.len() {
        let Ok(mut entry) = archive.by_index(i) else {
            continue;
        };
        // enclosed_name：拒绝 `..`、绝对路径等穿越条目
        let Some(rel) = entry.enclosed_name() else {
            continue;
        };
        let out = target.join(rel);
        if entry.is_dir() {
            fs::create_dir_all(&out).map_err(|e| format!("创建目录失败：{}", e))?;
            continue;
        }
        if let Some(parent) = out.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("创建目录失败：{}", e))?;
        }
        let mut f = fs::File::create(&out).map_err(|e| format!("写入失败 {}：{}", out.display(), e))?;
        std::io::copy(&mut entry, &mut f).map_err(|e| format!("写入失败 {}：{}", out.display(), e))?;
        files += 1;
        bytes += entry.size();
    }
    Ok((files, bytes))
}

// ————— 搜索 —————

/// 按名称搜索（不区分大小写；目录深度优先，限量防重负载）。
fn search_dir(root: &Path, q: &str) -> Json {
    let needle = q.to_lowercase();
    let mut results: Vec<Json> = Vec::new();
    let mut scanned = 0usize;
    let mut truncated = false;
    let mut stack = vec![root.to_path_buf()];
    'outer: while let Some(dir) = stack.pop() {
        let Ok(rd) = fs::read_dir(&dir) else { continue };
        for entry in rd.flatten() {
            scanned += 1;
            if scanned > SEARCH_MAX_SCAN {
                truncated = true;
                break 'outer;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            let Ok(md) = entry.metadata() else { continue };
            let path = entry.path();
            if name.to_lowercase().contains(&needle) {
                let item = entry_json(name, &path, &md, md.file_type().is_symlink());
                let mut obj = item;
                if let Some(m) = obj.as_object_mut() {
                    m.insert("path".to_string(), json!(path.display().to_string()));
                }
                results.push(obj);
                if results.len() >= SEARCH_MAX_RESULTS {
                    truncated = true;
                    break 'outer;
                }
            }
            if md.is_dir() {
                stack.push(path);
            }
        }
    }
    json!({ "items": results, "truncated": truncated, "scanned": scanned })
}

// ————— 动作：浏览 / 读写 —————

/// `GET /star/fileList?path=`：目录浏览（path 为空 → 快捷根列表/盘符）。
pub fn file_list(panel: &WebPanel, ctx: &Ctx) -> ActionResult {
    if !panel.check_auth(ctx) {
        return json_error(401, "Unauthorized");
    }
    let base = panel.base();
    let raw_input = arg(ctx, "path").unwrap_or_default();
    // Linux：空路径归一为 `/`（根视图即文件系统根，后续逻辑统一处理）
    #[cfg(unix)]
    let raw = {
        let trimmed = raw_input.trim();
        if trimmed.is_empty() {
            "/".to_string()
        } else {
            trimmed.to_string()
        }
    };
    #[cfg(windows)]
    let raw = raw_input;

    // Windows 根视图：列出盘符（Linux 的空路径已在上面归一为 `/`）
    #[cfg(windows)]
    if raw.trim().is_empty() {
        let mut items: Vec<Json> = Vec::new();
        for letter in b'A'..=b'Z' {
            let root = format!("{}:\\", letter as char);
            if Path::new(&root).exists() {
                items.push(json!({
                    "name": format!("{}:", letter as char),
                    "isDir": true,
                    "isLink": false,
                    "size": 0,
                    "mtime": "",
                    "perm": "",
                }));
            }
        }
        return json_result(
            0,
            "",
            Some(json!({
                "path": "",
                "parent": "",
                "items": items,
                "roots": quick_roots(base),
                "base": base.display().to_string(),
            })),
        );
    }

    let dir = match resolve_path(base, &raw) {
        Ok(p) => p,
        Err(e) => return json_error(400, &e),
    };
    if !dir.is_dir() {
        return json_error(404, &format!("目录不存在：{}", dir.display()));
    }
    match list_dir(&dir) {
        Ok(items) => {
            let parent = dir
                .parent()
                .map(|p| p.display().to_string())
                .unwrap_or_default();
            json_result(
                0,
                "",
                Some(json!({
                    "path": dir.display().to_string(),
                    "parent": parent,
                    "items": items,
                    "roots": quick_roots(base),
                    "base": base.display().to_string(),
                })),
            )
        }
        Err(e) => json_error(500, &e),
    }
}

/// `GET /star/fileRead?path=`：读取文本文件（≤2MB，二进制拒读）。
pub fn file_read(panel: &WebPanel, ctx: &Ctx) -> ActionResult {
    if !panel.check_auth(ctx) {
        return json_error(401, "Unauthorized");
    }
    let path = match resolve_path(panel.base(), &arg(ctx, "path").unwrap_or_default()) {
        Ok(p) => p,
        Err(e) => return json_error(400, &e),
    };
    let Ok(md) = fs::metadata(&path) else {
        return json_error(404, &format!("文件不存在：{}", path.display()));
    };
    if md.is_dir() {
        return json_error(400, "目标是目录，无法编辑");
    }
    if md.len() > MAX_EDIT_SIZE {
        return json_error(400, "文件过大（上限 2MB），请使用下载或 SFTP");
    }
    let Ok(data) = fs::read(&path) else {
        return json_error(500, "读取文件失败（权限不足？）");
    };
    if data.contains(&0) {
        return json_error(400, "二进制文件不支持在线编辑，请下载后处理");
    }
    match String::from_utf8(data) {
        Ok(content) => json_result(
            0,
            "",
            Some(json!({
                "path": path.display().to_string(),
                "content": content,
                "size": md.len(),
            })),
        ),
        Err(_) => json_error(400, "仅支持 UTF-8 文本编辑（该文件编码不受支持）"),
    }
}

/// `POST /star/fileWrite`：保存文本文件（`{path, content}`）。
pub fn file_write(panel: &WebPanel, ctx: &Ctx) -> ActionResult {
    if !panel.check_auth(ctx) {
        return json_error(401, "Unauthorized");
    }
    let Some(body) = json_body(ctx) else {
        return json_error(400, "缺少请求体");
    };
    let path = match resolve_path(
        panel.base(),
        body.get("path").and_then(|v| v.as_str()).unwrap_or_default(),
    ) {
        Ok(p) => p,
        Err(e) => return json_error(400, &e),
    };
    let content = body
        .get("content")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    if path.is_dir() {
        return json_error(400, "目标是目录，无法写入");
    }
    if let Some(parent) = path.parent() {
        if !parent.is_dir() {
            return json_error(400, &format!("上级目录不存在：{}", parent.display()));
        }
    }
    if let Err(e) = dhrust::io::write_all_text(&path, &content) {
        return json_error(500, &format!("保存失败：{}", e));
    }
    util::log_format("文件管理：保存文件 {}（{} 字节）", &[&path.display().to_string(), &content.len().to_string()]);
    json_result(0, "已保存", Some(json!({ "path": path.display().to_string(), "size": content.len() })))
}

/// `POST /star/fileMkdir`：新建目录（`{path: 父目录, name}`）。
pub fn file_mkdir(panel: &WebPanel, ctx: &Ctx) -> ActionResult {
    new_entry(panel, ctx, true)
}

/// `POST /star/fileNewFile`：新建空文件（`{path: 父目录, name}`）。
pub fn file_new_file(panel: &WebPanel, ctx: &Ctx) -> ActionResult {
    new_entry(panel, ctx, false)
}

fn new_entry(panel: &WebPanel, ctx: &Ctx, is_dir: bool) -> ActionResult {
    if !panel.check_auth(ctx) {
        return json_error(401, "Unauthorized");
    }
    let parent = match resolve_path(panel.base(), &arg(ctx, "path").unwrap_or_default()) {
        Ok(p) => p,
        Err(e) => return json_error(400, &e),
    };
    let name = arg(ctx, "name").unwrap_or_default();
    if let Err(e) = validate_name(&name) {
        return json_error(400, &e);
    }
    if !parent.is_dir() {
        return json_error(404, &format!("目录不存在：{}", parent.display()));
    }
    let dst = parent.join(name.trim());
    if dst.exists() {
        return json_error(400, &format!("已存在：{}", dst.display()));
    }
    let result = if is_dir {
        fs::create_dir(&dst)
    } else {
        fs::File::create(&dst).map(|_| ())
    };
    match result {
        Ok(()) => {
            util::log_format(
                "文件管理：新建{} {}",
                &[if is_dir { "目录" } else { "文件" }, &dst.display().to_string()],
            );
            json_result(0, "已创建", Some(json!({ "path": dst.display().to_string() })))
        }
        Err(e) => json_error(500, &format!("创建失败：{}", e)),
    }
}

// ————— 动作：删除 / 重命名 / 复制 / 移动 —————

/// 提取请求体中的 paths 数组。
fn body_paths(panel: &WebPanel, ctx: &Ctx) -> Result<Vec<PathBuf>, String> {
    let Some(body) = json_body(ctx) else {
        return Err("缺少请求体".to_string());
    };
    let Some(list) = body.get("paths").and_then(|v| v.as_array()) else {
        return Err("缺少 paths 参数".to_string());
    };
    if list.is_empty() {
        return Err("未选择任何项目".to_string());
    }
    if list.len() > MAX_BATCH {
        return Err(format!("单次最多操作 {} 项", MAX_BATCH));
    }
    let mut paths = Vec::new();
    for item in list {
        let text = item.as_str().unwrap_or_default();
        paths.push(resolve_path(panel.base(), text)?);
    }
    Ok(paths)
}

/// `POST /star/fileDelete`：删除（`{paths: []}`）。
pub fn file_delete(panel: &WebPanel, ctx: &Ctx) -> ActionResult {
    if !panel.check_auth(ctx) {
        return json_error(401, "Unauthorized");
    }
    let base = panel.base().to_path_buf();
    let paths = match body_paths(panel, ctx) {
        Ok(p) => p,
        Err(e) => return json_error(400, &e),
    };
    for p in &paths {
        if let Err(e) = ensure_modifiable(p, &base) {
            return json_error(400, &e);
        }
        if !p.exists() && fs::symlink_metadata(p).is_err() {
            return json_error(404, &format!("不存在：{}", p.display()));
        }
    }
    let mut freed = 0u64;
    let mut files = 0u64;
    for p in &paths {
        match remove_path(p) {
            Ok((f, b)) => {
                files += f;
                freed += b;
            }
            Err(e) => return json_error(500, &e),
        }
    }
    util::log_format(
        "文件管理：删除 {} 项（{} 个文件，{} 字节）",
        &[&paths.len().to_string(), &files.to_string(), &freed.to_string()],
    );
    json_result(
        0,
        &format!("已删除 {} 项", paths.len()),
        Some(json!({ "deleted": paths.len(), "files": files, "freedBytes": freed })),
    )
}

/// `POST /star/fileRename`：重命名（`{path, newName}`）。
pub fn file_rename(panel: &WebPanel, ctx: &Ctx) -> ActionResult {
    if !panel.check_auth(ctx) {
        return json_error(401, "Unauthorized");
    }
    let base = panel.base().to_path_buf();
    let path = match resolve_path(base.as_path(), &arg(ctx, "path").unwrap_or_default()) {
        Ok(p) => p,
        Err(e) => return json_error(400, &e),
    };
    let new_name = arg(ctx, "newName").unwrap_or_default();
    if let Err(e) = validate_name(&new_name) {
        return json_error(400, &e);
    }
    if let Err(e) = ensure_modifiable(&path, &base) {
        return json_error(400, &e);
    }
    if fs::symlink_metadata(&path).is_err() {
        return json_error(404, &format!("不存在：{}", path.display()));
    }
    let Some(parent) = path.parent() else {
        return json_error(400, "无法重命名根目录");
    };
    let dst = parent.join(new_name.trim());
    if dst.exists() {
        return json_error(400, &format!("目标已存在：{}", dst.display()));
    }
    match fs::rename(&path, &dst) {
        Ok(()) => {
            util::log_format(
                "文件管理：重命名 {} → {}",
                &[&path.display().to_string(), &dst.display().to_string()],
            );
            json_result(0, "已重命名", Some(json!({ "path": dst.display().to_string() })))
        }
        Err(e) => json_error(500, &format!("重命名失败：{}", e)),
    }
}

/// `POST /star/fileMove`：移动（`{paths: [], target: 目标目录}`）。
pub fn file_move(panel: &WebPanel, ctx: &Ctx) -> ActionResult {
    transfer(panel, ctx, false)
}

/// `POST /star/fileCopy`：复制（`{paths: [], target: 目标目录}`）。
pub fn file_copy(panel: &WebPanel, ctx: &Ctx) -> ActionResult {
    transfer(panel, ctx, true)
}

fn transfer(panel: &WebPanel, ctx: &Ctx, copy: bool) -> ActionResult {
    if !panel.check_auth(ctx) {
        return json_error(401, "Unauthorized");
    }
    let base = panel.base().to_path_buf();
    let paths = match body_paths(panel, ctx) {
        Ok(p) => p,
        Err(e) => return json_error(400, &e),
    };
    let target = match resolve_path(base.as_path(), &arg(ctx, "target").unwrap_or_default()) {
        Ok(p) => p,
        Err(e) => return json_error(400, &e),
    };
    if !target.is_dir() {
        return json_error(404, &format!("目标目录不存在：{}", target.display()));
    }
    // 预检
    for p in &paths {
        if let Err(e) = ensure_modifiable(p, &base) {
            return json_error(400, &e);
        }
        if !p.exists() && fs::symlink_metadata(p).is_err() {
            return json_error(404, &format!("不存在：{}", p.display()));
        }
        if p.starts_with(&target) {
            return json_error(
                400,
                if copy {
                    "目标目录在选中项内部，无法复制"
                } else {
                    "目标目录在选中项内部，无法移动"
                },
            );
        }
        let dst = target.join(file_label(p));
        if dst == *p {
            return json_error(400, "目标与源相同");
        }
        if dst.exists() {
            return json_error(400, &format!("目标已存在：{}", dst.display()));
        }
    }
    let mut files = 0u64;
    let mut bytes = 0u64;
    for p in &paths {
        let dst = target.join(file_label(p));
        // 统计须在移动前取（rename 后源路径已不存在）
        let stats = dir_or_file_stats(p);
        if copy {
            match copy_recursive(p, &dst) {
                Ok((f, b)) => {
                    files += f;
                    bytes += b;
                }
                Err(e) => return json_error(500, &e),
            }
        } else {
            // 先尝试原子改名；跨卷失败退化为复制 + 删除
            if fs::rename(p, &dst).is_err() {
                match copy_recursive(p, &dst).and_then(|(f, b)| remove_path(p).map(|_| (f, b))) {
                    Ok((f, b)) => {
                        files += f;
                        bytes += b;
                    }
                    Err(e) => return json_error(500, &e),
                }
            } else {
                files += stats.0;
                bytes += stats.1;
            }
        }
    }
    util::log_format(
        "文件管理：{} {} 项到 {}",
        &[
            if copy { "复制" } else { "移动" },
            &paths.len().to_string(),
            &target.display().to_string(),
        ],
    );
    json_result(
        0,
        &format!("已{} {} 项", if copy { "复制" } else { "移动" }, paths.len()),
        Some(json!({ "files": files, "bytes": bytes })),
    )
}

/// 统计路径的文件数/字节（移动成功后对源路径统计会失败——此函数用于移动前的近似统计）。
fn dir_or_file_stats(path: &Path) -> (u64, u64) {
    let Ok(md) = fs::symlink_metadata(path) else {
        return (0, 0);
    };
    if md.is_dir() {
        dir_stats(path)
    } else {
        (1, md.len())
    }
}

// ————— 动作：上传 / 下载 / 压缩 / 解压 / 权限 / 搜索 —————

/// `POST /star/fileUpload?path=目录&name=文件名`：请求体为文件内容。
pub fn file_upload(panel: &WebPanel, ctx: &Ctx) -> ActionResult {
    if !panel.check_auth(ctx) {
        return json_error(401, "Unauthorized");
    }
    let dir = match resolve_path(panel.base(), &arg(ctx, "path").unwrap_or_default()) {
        Ok(p) => p,
        Err(e) => return json_error(400, &e),
    };
    if !dir.is_dir() {
        return json_error(404, &format!("目录不存在：{}", dir.display()));
    }
    let name = arg(ctx, "name").unwrap_or_default();
    if let Err(e) = validate_name(&name) {
        return json_error(400, &e);
    }
    let data = &ctx.req.body;
    if data.is_empty() {
        return json_error(400, "上传内容为空");
    }
    let dst = dir.join(name.trim());
    if dst.is_dir() {
        return json_error(400, "目标是目录，无法覆盖");
    }
    // 先写临时文件再替换（覆盖已存在文件；Windows 占用时安全让位）
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp = dir.join(format!(".pek-upload-{}-{}.tmp", std::process::id(), nonce));
    if let Err(e) = fs::write(&tmp, data) {
        return json_error(500, &format!("写入临时文件失败：{}", e));
    }
    match dhrust::io::safe_replace_file(&tmp, &dst) {
        Ok(()) => {
            util::log_format(
                "文件管理：上传 {}（{} 字节）",
                &[&dst.display().to_string(), &data.len().to_string()],
            );
            json_result(
                0,
                "上传完成",
                Some(json!({ "path": dst.display().to_string(), "size": data.len() })),
            )
        }
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            json_error(500, &format!("保存失败：{}", e))
        }
    }
}

/// `GET /star/fileDownload?path=`：下载文件（≤256MB）。
pub fn file_download(panel: &WebPanel, ctx: &Ctx) -> ActionResult {
    if !panel.check_auth(ctx) {
        return json_error(401, "Unauthorized");
    }
    let path = match resolve_path(panel.base(), &arg(ctx, "path").unwrap_or_default()) {
        Ok(p) => p,
        Err(e) => return json_error(400, &e),
    };
    let Ok(md) = fs::metadata(&path) else {
        return json_error(404, &format!("文件不存在：{}", path.display()));
    };
    if md.is_dir() {
        return json_error(400, "目标是目录，无法下载（可先压缩）");
    }
    if md.len() > MAX_DOWNLOAD_SIZE {
        return json_error(400, "文件过大（上限 256MB），请使用 SFTP 下载");
    }
    let Ok(data) = fs::read(&path) else {
        return json_error(500, "读取文件失败（权限不足？）");
    };
    let name = file_label(&path);
    let ascii: String = name
        .chars()
        .map(|c| if c.is_ascii() && c != '"' && c != '\\' { c } else { '_' })
        .collect();
    let disposition = format!(
        "attachment; filename=\"{}\"; filename*=UTF-8''{}",
        ascii,
        percent_encode(&name)
    );
    util::log_format(
        "文件管理：下载 {}（{} 字节）",
        &[&path.display().to_string(), &data.len().to_string()],
    );
    ActionResult::Response(
        HttpResponse::bytes(200, "application/octet-stream", data)
            .with_header("Content-Disposition", &disposition),
    )
}

/// RFC 5987 百分号编码（用于中文文件名）。
fn percent_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len() * 2);
    for b in text.as_bytes() {
        let c = *b as char;
        let attr = c.is_ascii_alphanumeric()
            || matches!(c, '!' | '#' | '$' | '&' | '+' | '-' | '.' | '^' | '_' | '`' | '|' | '~');
        if attr {
            out.push(c);
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

/// `POST /star/fileCompress`：压缩（`{paths: [], target: 目标目录, name: 压缩包名}`）。
pub fn file_compress(panel: &WebPanel, ctx: &Ctx) -> ActionResult {
    if !panel.check_auth(ctx) {
        return json_error(401, "Unauthorized");
    }
    let base = panel.base().to_path_buf();
    let paths = match body_paths(panel, ctx) {
        Ok(p) => p,
        Err(e) => return json_error(400, &e),
    };
    let target = match resolve_path(base.as_path(), &arg(ctx, "target").unwrap_or_default()) {
        Ok(p) => p,
        Err(e) => return json_error(400, &e),
    };
    if !target.is_dir() {
        return json_error(404, &format!("目标目录不存在：{}", target.display()));
    }
    let mut name = arg(ctx, "name").unwrap_or_default();
    if name.trim().is_empty() {
        name = "archive.zip".to_string();
    }
    let name = name.trim().to_string();
    if let Err(e) = validate_name(&name) {
        return json_error(400, &e);
    }
    let name = if name.to_ascii_lowercase().ends_with(".zip") {
        name
    } else {
        format!("{name}.zip")
    };
    let dst = target.join(&name);
    if dst.exists() {
        return json_error(400, &format!("已存在：{}", dst.display()));
    }
    for p in &paths {
        if let Err(e) = ensure_modifiable(p, &base) {
            return json_error(400, &e);
        }
        if fs::symlink_metadata(p).is_err() {
            return json_error(404, &format!("不存在：{}", p.display()));
        }
    }
    match compress_zip(&dst, &paths) {
        Ok((files, size)) => {
            util::log_format(
                "文件管理：压缩 {} 项 → {}（{} 字节）",
                &[&paths.len().to_string(), &dst.display().to_string(), &size.to_string()],
            );
            json_result(
                0,
                "压缩完成",
                Some(json!({ "path": dst.display().to_string(), "fileCount": files, "size": size })),
            )
        }
        Err(e) => json_error(500, &e),
    }
}

/// `POST /star/fileExtract`：解压 zip（`{path, target?}`，默认解压到所在目录）。
pub fn file_extract(panel: &WebPanel, ctx: &Ctx) -> ActionResult {
    if !panel.check_auth(ctx) {
        return json_error(401, "Unauthorized");
    }
    let base = panel.base().to_path_buf();
    let path = match resolve_path(base.as_path(), &arg(ctx, "path").unwrap_or_default()) {
        Ok(p) => p,
        Err(e) => return json_error(400, &e),
    };
    let Ok(md) = fs::metadata(&path) else {
        return json_error(404, &format!("文件不存在：{}", path.display()));
    };
    if md.is_dir() {
        return json_error(400, "目标是目录，无法解压");
    }
    let target = match arg(ctx, "target") {
        Some(t) if !t.trim().is_empty() => match resolve_path(base.as_path(), &t) {
            Ok(p) => p,
            Err(e) => return json_error(400, &e),
        },
        _ => path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from(".")),
    };
    if !target.is_dir() {
        if let Err(e) = fs::create_dir_all(&target) {
            return json_error(500, &format!("创建目标目录失败：{}", e));
        }
    }
    match extract_zip_file(&path, &target) {
        Ok((files, bytes)) => {
            util::log_format(
                "文件管理：解压 {} → {}（{} 个文件）",
                &[
                    &path.display().to_string(),
                    &target.display().to_string(),
                    &files.to_string(),
                ],
            );
            json_result(
                0,
                "解压完成",
                Some(json!({
                    "target": target.display().to_string(),
                    "fileCount": files,
                    "bytes": bytes,
                })),
            )
        }
        Err(e) => json_error(500, &e),
    }
}

/// `POST /star/fileChmod`：修改权限（`{path, mode: "755"}`，仅 Unix）。
pub fn file_chmod(panel: &WebPanel, ctx: &Ctx) -> ActionResult {
    if !panel.check_auth(ctx) {
        return json_error(401, "Unauthorized");
    }
    let path = match resolve_path(panel.base(), &arg(ctx, "path").unwrap_or_default()) {
        Ok(p) => p,
        Err(e) => return json_error(400, &e),
    };
    let mode_text = arg(ctx, "mode").unwrap_or_default();
    let Ok(mode) = u32::from_str_radix(mode_text.trim().trim_start_matches("0o"), 8) else {
        return json_error(400, "权限格式应为八进制（如 755）");
    };
    if mode > 0o7777 {
        return json_error(400, "权限超出范围");
    }
    if fs::symlink_metadata(&path).is_err() {
        return json_error(404, &format!("不存在：{}", path.display()));
    }
    match chmod(&path, mode) {
        Ok(()) => {
            util::log_format(
                "文件管理：修改权限 {} → {:o}",
                &[&path.display().to_string(), &format!("{:o}", mode)],
            );
            json_result(0, "权限已修改", None)
        }
        Err(e) => json_error(500, &e),
    }
}

/// `GET /star/fileSearch?path=&q=`：按名称搜索。
pub fn file_search(panel: &WebPanel, ctx: &Ctx) -> ActionResult {
    if !panel.check_auth(ctx) {
        return json_error(401, "Unauthorized");
    }
    let root = match resolve_path(panel.base(), &arg(ctx, "path").unwrap_or_default()) {
        Ok(p) => p,
        Err(e) => return json_error(400, &e),
    };
    let q = arg(ctx, "q").unwrap_or_default();
    if q.trim().is_empty() {
        return json_error(400, "缺少搜索关键词");
    }
    if !root.is_dir() {
        return json_error(404, &format!("目录不存在：{}", root.display()));
    }
    let mut data = search_dir(&root, q.trim());
    if let Some(obj) = data.as_object_mut() {
        obj.insert("root".to_string(), json!(root.display().to_string()));
    }
    json_result(0, "", Some(data))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 独立临时目录。
    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pek-fileman-{}-{}-{}",
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
    fn resolves_absolute_relative_and_dotdot_paths() {
        let base = Path::new("/opt/app");
        assert_eq!(
            resolve_path(base, "/var/log/../log/x").unwrap(),
            PathBuf::from("/var/log/x")
        );
        assert_eq!(resolve_path(base, "data").unwrap(), PathBuf::from("/opt/app/data"));
        assert!(resolve_path(base, "  ").is_err());
    }

    #[test]
    fn protects_root_first_level_and_base() {
        let base = Path::new("/opt/star");
        assert!(is_protected(Path::new("/"), base));
        assert!(is_protected(Path::new("/etc"), base));
        assert!(is_protected(Path::new("/usr"), base));
        assert!(!is_protected(Path::new("/etc/nginx"), base));
        assert!(!is_protected(Path::new("/var/log"), base));
        assert!(is_protected(base, base));
    }

    #[test]
    fn validates_names() {
        assert!(validate_name("a.txt").is_ok());
        assert!(validate_name("..").is_err());
        assert!(validate_name("a/b").is_err());
        assert!(validate_name("a\\b").is_err());
        assert!(validate_name("").is_err());
    }

    #[test]
    fn list_entries_sorted_dirs_first() {
        let dir = temp_dir("list");
        fs::create_dir(dir.join("zdir")).unwrap();
        fs::write(dir.join("b.txt"), b"hello").unwrap();
        fs::write(dir.join("A.txt"), b"world!").unwrap();
        let items = list_dir(&dir).unwrap();
        let names: Vec<String> = items
            .iter()
            .map(|v| v.get("name").unwrap().as_str().unwrap().to_string())
            .collect();
        assert_eq!(names, vec!["zdir", "A.txt", "b.txt"]);
        let first = &items[0];
        assert!(first.get("isDir").unwrap().as_bool().unwrap());
        let second = &items[1];
        assert_eq!(second.get("size").unwrap().as_u64().unwrap(), 6);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn delete_copy_move_rename_roundtrip() {
        let dir = temp_dir("ops");
        let src = dir.join("src.txt");
        fs::write(&src, b"payload").unwrap();

        // 复制
        let copy_dst = dir.join("copy.txt");
        let (files, bytes) = copy_recursive(&src, &copy_dst).unwrap();
        assert_eq!((files, bytes), (1, 7));
        assert_eq!(fs::read(&copy_dst).unwrap(), b"payload");

        // 移动（目录）
        let sub = dir.join("sub");
        fs::create_dir(&sub).unwrap();
        fs::write(sub.join("f1.bin"), vec![0u8; 10]).unwrap();
        fs::write(sub.join("f2.bin"), vec![0u8; 20]).unwrap();
        fs::rename(&sub, dir.join("moved")).unwrap();
        let (f, b) = dir_stats(&dir.join("moved"));
        assert_eq!((f, b), (2, 30));

        // 删除（目录递归）
        let (files, bytes) = remove_path(&dir.join("moved")).unwrap();
        assert_eq!((files, bytes), (2, 30));
        assert!(!dir.join("moved").exists());

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn compress_and_extract_roundtrip() {
        let dir = temp_dir("zip");
        let a = dir.join("a.txt");
        let sub = dir.join("sub");
        fs::create_dir(&sub).unwrap();
        fs::write(&a, b"AAAA").unwrap();
        fs::write(sub.join("b.txt"), b"BBBBBB").unwrap();

        let zip_path = dir.join("out.zip");
        let (files, size) = compress_zip(&zip_path, &[a.clone(), sub.clone()]).unwrap();
        assert_eq!(files, 2);
        assert!(size > 0);

        let out = dir.join("extract");
        fs::create_dir(&out).unwrap();
        let (efiles, _) = extract_zip_file(&zip_path, &out).unwrap();
        assert_eq!(efiles, 2);
        assert_eq!(fs::read(out.join("a.txt")).unwrap(), b"AAAA");
        assert_eq!(fs::read(out.join("sub/b.txt")).unwrap(), b"BBBBBB");

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn search_finds_matches_and_limits() {
        let dir = temp_dir("search");
        fs::create_dir_all(dir.join("inner")).unwrap();
        fs::write(dir.join("needle.txt"), b"x").unwrap();
        fs::write(dir.join("inner/needle-2.txt"), b"y").unwrap();
        fs::write(dir.join("other.txt"), b"z").unwrap();
        let data = search_dir(&dir, "NEEDLE");
        let items = data.get("items").unwrap().as_array().unwrap();
        assert_eq!(items.len(), 2);
        assert!(!data.get("truncated").unwrap().as_bool().unwrap());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn percent_encode_encodes_utf8() {
        assert_eq!(percent_encode("a b.txt"), "a%20b.txt");
        assert_eq!(percent_encode("日志.zip"), "%E6%97%A5%E5%BF%97.zip");
    }
}
