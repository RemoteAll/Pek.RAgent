//! 文件管理（Web 面板「文件管理」页）：浏览、上传/下载、在线编辑、新建、删除、
//! 重命名、复制/移动、压缩/解压、权限修改与搜索。
//!
//! 安全模型（对齐宝塔文件管理）：
//! - 全部动作走面板鉴权（`WebPanel::check_auth`，随 `WebAuthLevel` 生效）；
//! - 路径词法归一化（相对路径按程序基础目录解析），不访问文件系统即可消除 `.`/`..`；
//! - **删除/重命名/移动**对“根”“一级目录（如 `/etc`、`C:\Windows`）”“程序目录本身”
//!   设保护（目录内部仍可操作）——防止误操作损毁系统；
//! - 上传文件名与压缩包条目名做目录穿越拦截（`..`、绝对路径、分隔符）；
//! - 下载上限 256MB、在线编辑上限 10MB（二进制拒读），大文件请用 SFTP。

use std::fs;
#[cfg(windows)]
use std::path::Path;
use std::path::PathBuf;

use dhrust::net::controller::{ActionResult, arg, json_body, json_error, json_result};
use dhrust::net::http::HttpResponse;
use dhrust::net::router::Ctx;
#[cfg(windows)]
use serde_json::Value as Json;
use serde_json::json;

use crate::util;
use crate::webpanel::WebPanel;

// ————— 业务实现（下沉 dhrust::fileman；本模块只保留面板 HTTP 适配层） —————

pub(crate) use dhrust::fileman::{
    MAX_BATCH, MAX_DOWNLOAD_SIZE, MAX_EDIT_SIZE, chmod, compress_zip, copy_recursive,
    dir_or_file_stats, ensure_modifiable, extract_zip_file, file_label, list_dir, percent_encode,
    quick_roots, remove_path, resolve_path, search_dir, size_recursive, validate_name,
};

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
        return json_error(400, "文件过大（上限 10MB），请使用下载或 SFTP");
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
        body.get("path")
            .and_then(|v| v.as_str())
            .unwrap_or_default(),
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
    if let Some(parent) = path.parent()
        && !parent.is_dir()
    {
        return json_error(400, &format!("上级目录不存在：{}", parent.display()));
    }
    if let Err(e) = dhrust::io::write_all_text(&path, &content) {
        return json_error(500, &format!("保存失败：{}", e));
    }
    util::log_format(
        "文件管理：保存文件 {}（{} 字节）",
        &[&path.display().to_string(), &content.len().to_string()],
    );
    json_result(
        0,
        "已保存",
        Some(json!({ "path": path.display().to_string(), "size": content.len() })),
    )
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
                &[
                    if is_dir { "目录" } else { "文件" },
                    &dst.display().to_string(),
                ],
            );
            json_result(
                0,
                "已创建",
                Some(json!({ "path": dst.display().to_string() })),
            )
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
        &[
            &paths.len().to_string(),
            &files.to_string(),
            &freed.to_string(),
        ],
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
            json_result(
                0,
                "已重命名",
                Some(json!({ "path": dst.display().to_string() })),
            )
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
        &format!(
            "已{} {} 项",
            if copy { "复制" } else { "移动" },
            paths.len()
        ),
        Some(json!({ "files": files, "bytes": bytes })),
    )
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
        .map(|c| {
            if c.is_ascii() && c != '"' && c != '\\' {
                c
            } else {
                '_'
            }
        })
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
                &[
                    &paths.len().to_string(),
                    &dst.display().to_string(),
                    &size.to_string(),
                ],
            );
            json_result(
                0,
                "压缩完成",
                Some(
                    json!({ "path": dst.display().to_string(), "fileCount": files, "size": size }),
                ),
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
    if !target.is_dir()
        && let Err(e) = fs::create_dir_all(&target)
    {
        return json_error(500, &format!("创建目标目录失败：{}", e));
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

/// `POST /star/fileSize {"path"}`：按需计算文件夹大小（手动点击“计算”时调用；超限返回部分值）。
pub fn file_dir_size(panel: &WebPanel, ctx: &Ctx) -> ActionResult {
    if !panel.check_auth(ctx) {
        return json_error(401, "Unauthorized");
    }
    let input = json_body(ctx)
        .and_then(|v| v.get("path").and_then(|s| s.as_str()).map(String::from))
        .unwrap_or_default();
    let path = match resolve_path(panel.base(), &input) {
        Ok(p) => p,
        Err(e) => return json_error(400, &e),
    };
    // fs::metadata 跟随符号链接（允许计算链接指向的目录）
    let md = match fs::metadata(&path) {
        Ok(m) => m,
        Err(e) => return json_error(404, &format!("路径不存在：{}（{}）", path.display(), e)),
    };
    if !md.is_dir() {
        return json_error(400, "仅支持文件夹");
    }
    let (bytes, files, dirs, partial) = size_recursive(&path);
    util::log_format(
        "文件管理：计算目录大小 {}（{} 字节，{} 文件，{} 子目录）",
        &[
            &path.display().to_string(),
            &bytes.to_string(),
            &files.to_string(),
            &dirs.to_string(),
        ],
    );
    json_result(
        0,
        if partial {
            "已计算（目录过大，结果为部分统计）"
        } else {
            "已计算"
        },
        Some(json!({
            "path": path.display().to_string(),
            "bytes": bytes,
            "files": files,
            "dirs": dirs,
            "partial": partial,
        })),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    // 仅测试使用的下沉能力（面板 handler 不直接调用）
    use dhrust::fileman::{dir_stats, is_protected};

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
    fn dir_size_recursive_counts() {
        let dir = temp_dir("size");
        fs::create_dir_all(dir.join("a/b")).unwrap();
        fs::write(dir.join("f1.txt"), b"12345").unwrap();
        fs::write(dir.join("a/f2.txt"), b"123").unwrap();
        fs::write(dir.join("a/b/f3.txt"), b"1").unwrap();
        let (bytes, files, dirs, partial) = size_recursive(&dir);
        assert_eq!(bytes, 9);
        assert_eq!(files, 3);
        assert_eq!(dirs, 2);
        assert!(!partial);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolves_absolute_relative_and_dotdot_paths() {
        let base = Path::new("/opt/app");
        assert_eq!(
            resolve_path(base, "/var/log/../log/x").unwrap(),
            PathBuf::from("/var/log/x")
        );
        assert_eq!(
            resolve_path(base, "data").unwrap(),
            PathBuf::from("/opt/app/data")
        );
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
