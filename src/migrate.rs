//! 从 C# 版星尘（StarAgent）迁移子服务到 Pek.RAgent（面板「🔄 迁移 C# 星尘」）。
//!
//! 背景：C# 版星尘将逐步被 Pek.RAgent 取代，但存量服务器上的子服务配置还在 C# 的
//! `Config/StarAgent.config` 里。本模块提供**面板一键迁移**（不依赖 SSH；面板按钮
//! → `GET /star/starAgentScan` 预览 → `POST /star/starAgentMigrate` 执行）：
//!
//! 1. **只读**解析 C# 配置（不生成、不补齐、不写盘）；
//! 2. 把子服务条目转成 Pek.RAgent 的 [`AppConfig`]，**相对路径按 C# 安装目录绝对化**
//!    （两版基础目录不同，绝对化后同一批程序仍从原目录运行、配置/数据不搬家）；
//! 3. 通过 C# 星尘本机 UDP RPC（`dhrust::net::api_rpc`，与 C# `ApiClient` 同契约）
//!    逐个 `StopService`，让 C# 侧自己停干净；
//! 4. 停止并（默认）禁用 C# 星尘系统服务，避免重启或守护再次拉起；
//! 5. 写入 Pek.RAgent 配置并启动（默认只启动 `Enable=true` 的条目）。
//!
//! 安全约束：源目录等于当前 Pek.RAgent 安装目录时直接拒绝；C# 服务名与当前服务名相同时
//! **不停服**（避免停掉自己）。所有失败都降级为告警，不阻断迁移（C# 未运行是常见情况）。

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use dhrust::net::api_rpc;

use crate::config::{self, AppConfig};
use crate::deploy::DeployMode;
use crate::manager::AppManager;
use crate::util;

/// C# 版星尘默认服务名（未在配置中显式写 `ServiceName` 时）。
pub const CSHARP_SERVICE_NAME: &str = "StarAgent";
/// C# 版星尘默认本地端口（UDP RPC / 面板共用）。
pub const CSHARP_LOCAL_PORT: u16 = 5500;

/// 常见安装目录（面板「扫描」未指定目录时逐个探测）。
pub fn candidates() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    #[cfg(windows)]
    {
        for p in [
            "C:\\StarAgent",
            "C:\\Program Files\\StarAgent",
            "C:\\Program Files (x86)\\StarAgent",
            "C:\\Agent",
        ] {
            dirs.push(PathBuf::from(p));
        }
    }
    #[cfg(not(windows))]
    {
        for p in [
            "/www/Agent",
            "/opt/staragent",
            "/opt/StarAgent",
            "/usr/local/staragent",
            "/root/StarAgent",
        ] {
            dirs.push(PathBuf::from(p));
        }
    }
    dirs
}

/// 星尘配置文件路径（两版同名同格式）。
fn config_file(dir: &Path) -> PathBuf {
    dir.join("Config").join("StarAgent.config")
}

/// 自动探测：第一个存在 `Config/StarAgent.config` 的 **C# 版**候选目录。
///
/// 排除已经是 Pek.RAgent 自己的配置（含扩展键 `AgentId`）——本机把 Rust 版装在
/// 候选路径（如 `C:\StarAgent`）时，不应在子服务页弹出“迁移自己”的横幅。
pub fn detect() -> Option<PathBuf> {
    candidates().into_iter().find(|d| {
        let path = config_file(d);
        path.is_file() && !looks_like_rust_config(&path)
    })
}

/// 配置是否来自 Pek.RAgent（C# 版没有 `AgentId` 等扩展键）。
fn looks_like_rust_config(path: &Path) -> bool {
    std::fs::read_to_string(path)
        .map(|text| text.contains("<AgentId>"))
        .unwrap_or(false)
}

/// 扫描结果（面板预览）。
#[derive(Debug, Clone)]
pub struct Scan {
    /// 源安装目录（绝对化）
    pub dir: PathBuf,
    /// 源配置文件路径
    pub config_path: PathBuf,
    /// C# 星尘服务名（来自配置 `ServiceName`，缺省 `StarAgent`）
    pub service_name: String,
    /// 该服务在当前系统是否已安装
    pub service_installed: bool,
    /// 该服务当前是否运行中
    pub service_running: bool,
    /// C# 本地端口（UDP RPC / 面板）
    pub local_port: u16,
    /// 子服务（原始值，未绝对化——预览按 C# 视角展示）
    pub apps: Vec<AppConfig>,
    /// 提示（配置看起来像 Rust 版自身、没有任何子服务等）
    pub warnings: Vec<String>,
}

/// 迁移选项。
#[derive(Debug, Clone)]
pub struct Options {
    /// C# 星尘安装目录
    pub dir: PathBuf,
    /// 迁移前经 C# UDP RPC 停掉其子服务
    pub stop_source_apps: bool,
    /// 停掉 C# 星尘系统服务
    pub stop_source_service: bool,
    /// 同时把 C# 星尘服务设为禁用（开机不再自起）
    pub disable_source_service: bool,
    /// 迁移后启动 `Enable=true` 的子服务
    pub start_apps: bool,
    /// 仅迁移这些名称（空 = 全部）
    pub only: Vec<String>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            dir: PathBuf::new(),
            stop_source_apps: true,
            stop_source_service: true,
            disable_source_service: true,
            start_apps: true,
            only: Vec::new(),
        }
    }
}

/// 单个子服务的迁移结果。
#[derive(Debug, Clone)]
pub struct Imported {
    /// 服务名
    pub name: String,
    /// 迁移后的启动程序（相对路径已绝对化）
    pub file_name: String,
    /// 迁移后的工作目录
    pub working_directory: String,
    /// 部署模式
    pub mode: String,
    /// 是否启用
    pub enable: bool,
    /// 是否已启动
    pub started: bool,
    /// 附加说明（如“更新已有条目”/启动失败原因）
    pub note: String,
}

/// 迁移报告（面板回显）。
#[derive(Debug, Clone, Default)]
pub struct Report {
    /// 源目录
    pub dir: String,
    /// C# 服务名
    pub service_name: String,
    /// 迁移的子服务
    pub imported: Vec<Imported>,
    /// 经 C# RPC 成功停止的子服务数
    pub source_apps_stopped: usize,
    /// C# 星尘服务是否已停止
    pub source_service_stopped: bool,
    /// C# 星尘服务是否已禁用
    pub source_service_disabled: bool,
    /// 是否直接结束了残留的 C# 星尘进程（非服务形态）
    pub source_agent_process_stopped: bool,
    /// 告警（不阻断迁移）
    pub warnings: Vec<String>,
}

/// 扫描 C# 版星尘配置（只读）。
///
/// `dir` 为空时自动探测 [`candidates`]；解析失败/目录不存在返回 `Err`（面板据此提示）。
pub fn scan(dir: Option<&Path>) -> Result<Scan, String> {
    let dir = match dir.map(|d| d.to_path_buf()).filter(|d| !d.as_os_str().is_empty()) {
        Some(d) => util::lexical_normalize(&d),
        None => detect().ok_or_else(|| {
            format!(
                "未找到 C# 版星尘安装目录（已尝试：{}）；请在「源目录」填写其安装路径",
                candidates()
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join("、")
            )
        })?,
    };

    let config_path = config_file(&dir);
    if !config_path.is_file() {
        return Err(format!(
            "该目录下没有星尘配置：{}（请填写 C# 版星尘的安装目录）",
            config_path.display()
        ));
    }

    let (cfg, raw) = config::read_config_file(&config_path)?;

    // C# 服务名/端口：读原始键（缺失回退 C# 默认值，避免把 Rust 版默认值 5501 当来源端口）
    let raw_text = |key: &str| {
        raw.get(key)
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };
    let service_name = raw_text("ServiceName").unwrap_or_else(|| CSHARP_SERVICE_NAME.to_string());
    let local_port = raw
        .get("LocalPort")
        .and_then(|v| v.as_str())
        .and_then(|s| s.trim().parse::<u16>().ok())
        .unwrap_or(CSHARP_LOCAL_PORT);

    let mut warnings = Vec::new();
    if service_name.eq_ignore_ascii_case(config::DEFAULT_SERVICE_NAME) {
        warnings.push(
            "源配置里的服务名是 StarAgentRust（看起来已经是 Pek.RAgent 自己的配置），请确认没有把当前节点迁到自己身上"
                .to_string(),
        );
    }
    if raw.get("AgentId").and_then(|v| v.as_str()).map(|s| !s.trim().is_empty()).unwrap_or(false) {
        warnings.push(
            "源配置含 Pek.RAgent 扩展键（AgentId 等），可能不是 C# 版星尘；迁移仅导入子服务条目"
                .to_string(),
        );
    }

    let apps: Vec<AppConfig> = cfg
        .apps
        .iter()
        .filter(|a| !a.name.trim().is_empty())
        .cloned()
        .collect();
    if apps.is_empty() {
        warnings.push("源配置里没有任何子服务（Services 为空），迁移后只需在本面板添加服务".to_string());
    }

    let (service_installed, service_running) = query_service(&service_name);

    Ok(Scan {
        dir,
        config_path,
        service_name,
        service_installed,
        service_running,
        local_port,
        apps,
        warnings,
    })
}

/// 执行迁移：停 C# 侧 → 导入配置 → 按需启动。
pub fn migrate(manager: &AppManager, opts: &Options) -> Result<Report, String> {
    let own_base = manager.base().to_path_buf();
    let scan = scan(Some(&opts.dir))?;
    if same_dir(&scan.dir, &own_base) {
        return Err("源目录就是当前 Pek.RAgent 的安装目录，无需迁移".to_string());
    }

    let selected: Vec<AppConfig> = scan
        .apps
        .iter()
        .filter(|a| {
            opts.only.is_empty()
                || opts
                    .only
                    .iter()
                    .any(|n| n.trim().eq_ignore_ascii_case(a.name.trim()))
        })
        .cloned()
        .collect();
    if selected.is_empty() {
        return Err("没有选中任何子服务（源配置为空或未勾选）".to_string());
    }

    let mut report = Report {
        dir: scan.dir.display().to_string(),
        service_name: scan.service_name.clone(),
        warnings: scan.warnings.clone(),
        ..Report::default()
    };

    // ① 经 C# 星尘本机 UDP RPC 停掉其子服务（C# 未运行时只告警一次，继续后续步骤）
    if opts.stop_source_apps {
        let addr = format!("127.0.0.1:{}", scan.local_port);
        let mut rpc_dead = false;
        for app in &selected {
            if rpc_dead {
                break;
            }
            match api_rpc::stop_service(&addr, app.name.trim(), Duration::from_secs(15)) {
                // 成功/业务失败都继续；只有“连不上 C# 星尘”才终止本循环
                Ok(r) if r.success => report.source_apps_stopped += 1,
                Ok(r) => report.warnings.push(format!(
                    "C# 子服务[{}]未停止：{}",
                    app.name,
                    if r.message.trim().is_empty() {
                        "C# 返回失败"
                    } else {
                        r.message.trim()
                    }
                )),
                Err(e) => {
                    rpc_dead = true;
                    report.warnings.push(format!(
                        "C# 星尘本地 RPC 无响应（{e}）；改为直接停 C# 服务，若仍有子服务占用端口可稍后手动结束"
                    ));
                }
            }
        }
    }

    // ② 停止（并禁用）C# 星尘系统服务；非服务形态时直接结束其进程
    if opts.stop_source_service {
        let own_service = manager.config().service_name.trim().to_string();
        if !own_service.is_empty() && scan.service_name.eq_ignore_ascii_case(&own_service) {
            report.warnings.push(format!(
                "C# 服务名与当前 Pek.RAgent 服务名相同（{}），已跳过停服（避免停掉自己）",
                scan.service_name
            ));
        } else {
            let (stopped, disabled) =
                stop_system_service(&scan.service_name, opts.disable_source_service);
            // 未安装时不把“无需停止”算成“已停止”（面板回显要能区分）
            report.source_service_stopped = stopped && scan.service_installed;
            report.source_service_disabled = disabled;
            if scan.service_installed && !stopped {
                report
                    .warnings
                    .push(format!("C# 星尘服务[{}]未确认停止（超时）", scan.service_name));
            }
            if !scan.service_installed {
                report.warnings.push(format!(
                    "C# 星尘未以系统服务安装（{}）；已尝试结束其在安装目录下的进程，如仍占用端口请在原面板停止",
                    scan.service_name
                ));
                let killed = stop_agent_process(&scan.dir, &scan.service_name, &mut report);
                report.source_agent_process_stopped = killed;
            }
        }
    }

    // ③ 兜底：按可执行文件路径结束残留进程（RPC 不可用、应用未被 C# 纳管等）
    for app in &selected {
        stop_leftover(app, &scan.dir, &mut report);
    }

    // ④ 导入配置（相对路径按 C# 安装目录绝对化；同名条目更新）
    for app in &selected {
        let mut cfg = app.clone();
        cfg.name = cfg.name.trim().to_string();
        absolutize(&scan.dir, &mut cfg);
        // C# 的数值模式（10-13/0-4）归一化为文本（面板下拉与配置阅读都更直观）
        cfg.mode = DeployMode::parse(&cfg.mode).as_str().to_string();
        let existed = manager.config().find_app(&cfg.name).is_some();
        manager.upsert_app(cfg.clone());
        report.imported.push(Imported {
            name: cfg.name.clone(),
            file_name: cfg.file_name.clone(),
            working_directory: cfg.working_directory.clone().unwrap_or_default(),
            mode: cfg.mode_text(),
            enable: cfg.enable,
            started: false,
            note: if existed { "更新已有条目".to_string() } else { String::new() },
        });
    }

    // ⑤ 启动（默认只启动启用状态的条目；启动会同时置启用）
    if opts.start_apps {
        for item in &mut report.imported {
            if !item.enable {
                item.note = join_note(&item.note, "已禁用，未启动");
                continue;
            }
            match manager.start_app(&item.name) {
                Ok(true) => item.started = true,
                Ok(false) => item.note = join_note(&item.note, "启动请求已提交"),
                Err(e) => item.note = join_note(&item.note, &format!("启动失败：{e}")),
            }
        }
    }

    util::log_format(
        "从 C# 星尘迁移完成：源 {}，导入 {} 个子服务（启动 {}），C# 服务停止={} 禁用={}",
        &[
            report.dir.as_str(),
            report.imported.len().to_string().as_str(),
            report.imported.iter().filter(|i| i.started).count().to_string().as_str(),
            (if report.source_service_stopped { "是" } else { "否" }).to_string().as_str(),
            (if report.source_service_disabled { "是" } else { "否" }).to_string().as_str(),
        ],
    );

    Ok(report)
}

/// 相对路径绝对化：`WorkingDirectory` 一律绝对化；`FileName` 含路径分隔符且为相对时，
/// 依次在工作目录、C# 安装目录下查找，命中即绝对化（都不存在时按安装目录绝对化）。
///
/// 绝对化后 Pek.RAgent 与 C# 星尘安装在不同目录也能运行同一批程序（配置/数据不搬家）。
pub fn absolutize(src_base: &Path, app: &mut AppConfig) {
    let wd = app
        .working_directory
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| util::resolve(src_base, s));
    if let Some(wd) = &wd {
        app.working_directory = Some(wd.display().to_string());
    }

    let file = app.file_name.trim().to_string();
    if file.is_empty() {
        return;
    }
    let path = Path::new(&file);
    let has_sep = file.contains('/') || file.contains('\\');
    if !has_sep || path.is_absolute() {
        return;
    }

    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(wd) = &wd {
        candidates.push(wd.join(&file));
    }
    candidates.push(src_base.join(&file));
    for cand in &candidates {
        if cand.exists() {
            app.file_name = util::lexical_normalize(cand).display().to_string();
            return;
        }
    }
    app.file_name = util::lexical_normalize(&src_base.join(&file))
        .display()
        .to_string();
}

/// 按可执行文件路径结束残留进程（.dll/.jar/.zip 或裸命令跳过——交给 C# RPC/服务停止）。
fn stop_leftover(app: &AppConfig, src_base: &Path, report: &mut Report) {
    let file = app.file_name.trim();
    if file.is_empty() || (!file.contains('/') && !file.contains('\\')) {
        return;
    }
    let path = Path::new(file);
    let full = if path.is_absolute() {
        path.to_path_buf()
    } else {
        src_base.join(path)
    };
    let ext = full
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if matches!(ext.as_str(), "dll" | "jar" | "zip") || !full.is_file() {
        return;
    }

    let target = std::fs::canonicalize(&full).unwrap_or(full);
    let own_pid = std::process::id();
    let mut killed = 0usize;
    for proc in crate::sys::top_processes(usize::MAX, true) {
        if proc.pid == 0 || proc.pid == own_pid || proc.exe.trim().is_empty() {
            continue;
        }
        if same_exe(&proc.exe, &target) && crate::sys::stop_process(proc.pid, 3_000) {
            killed += 1;
        }
    }
    if killed > 0 {
        report.warnings.push(format!(
            "已直接结束 {} 个残留进程（{}）",
            killed, app.name
        ));
    }
}

/// 非服务形态的 C# 星尘：结束安装目录下与服务同名的进程。
fn stop_agent_process(src_dir: &Path, service_name: &str, report: &mut Report) -> bool {
    let target_dir = std::fs::canonicalize(src_dir).unwrap_or_else(|_| src_dir.to_path_buf());
    let own_pid = std::process::id();
    let mut killed = 0usize;
    for proc in crate::sys::top_processes(usize::MAX, true) {
        if proc.pid == 0 || proc.pid == own_pid || proc.exe.trim().is_empty() {
            continue;
        }
        let exe = PathBuf::from(&proc.exe);
        let Some(parent) = exe.parent() else {
            continue;
        };
        let parent = std::fs::canonicalize(parent).unwrap_or_else(|_| parent.to_path_buf());
        if !same_dir(&parent, &target_dir) {
            continue;
        }
        let stem = exe
            .file_stem()
            .map(|s| s.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        if stem != service_name.to_ascii_lowercase() {
            continue;
        }
        if crate::sys::stop_process(proc.pid, 5_000) {
            killed += 1;
        }
    }
    if killed > 0 {
        report.warnings.push(format!(
            "C# 星尘未以系统服务运行，已结束其进程（{} 个）",
            killed
        ));
    }
    killed > 0
}

/// 查询系统服务状态（`(已安装, 运行中)`）。
fn query_service(name: &str) -> (bool, bool) {
    if name.trim().is_empty() {
        return (false, false);
    }
    #[cfg(windows)]
    {
        let (code, out, _err) = dhrust::sys::process::run("sc", &["query", name]);
        if code != 0 {
            return (false, false);
        }
        (true, out.to_ascii_uppercase().contains("RUNNING"))
    }
    #[cfg(not(windows))]
    {
        let (_code, out, _err) = dhrust::sys::process::run("systemctl", &["is-active", name]);
        let text = out.trim().to_string();
        let running = matches!(text.as_str(), "active" | "activating");
        let installed = !matches!(text.as_str(), "" | "unknown");
        (installed, running)
    }
}

/// 停止（并可禁用）系统服务；返回 `(是否已停止, 是否已禁用)`。
fn stop_system_service(name: &str, disable: bool) -> (bool, bool) {
    if name.trim().is_empty() {
        return (false, false);
    }
    #[cfg(windows)]
    {
        let _ = dhrust::sys::process::run("sc", &["stop", name]);
        let stopped = wait_stopped(name, Duration::from_secs(20));
        let disabled = if disable {
            dhrust::sys::process::run("sc", &["config", name, "start=", "disabled"]).0 == 0
        } else {
            false
        };
        (stopped, disabled)
    }
    #[cfg(not(windows))]
    {
        let _ = dhrust::sys::process::run("systemctl", &["stop", name]);
        let stopped = wait_stopped(name, Duration::from_secs(20));
        let disabled = if disable {
            dhrust::sys::process::run("systemctl", &["disable", name]).0 == 0
        } else {
            false
        };
        (stopped, disabled)
    }
}

/// 轮询等待服务停止（未安装视为已停止）。
fn wait_stopped(name: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        let (installed, running) = query_service(name);
        if !installed || !running {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(300));
    }
}

/// 路径比较（Windows 大小写不敏感；先规范化）。
fn same_dir(a: &Path, b: &Path) -> bool {
    let ca = std::fs::canonicalize(a).unwrap_or_else(|_| a.to_path_buf());
    let cb = std::fs::canonicalize(b).unwrap_or_else(|_| b.to_path_buf());
    #[cfg(windows)]
    {
        ca.to_string_lossy().eq_ignore_ascii_case(&cb.to_string_lossy())
    }
    #[cfg(not(windows))]
    {
        ca == cb
    }
}

/// 可执行文件路径比较（Windows 大小写不敏感）。
fn same_exe(proc_exe: &str, target: &Path) -> bool {
    if proc_exe.trim().is_empty() {
        return false;
    }
    let proc_path = Path::new(proc_exe);
    let a = std::fs::canonicalize(proc_path).unwrap_or_else(|_| proc_path.to_path_buf());
    let b = std::fs::canonicalize(target).unwrap_or_else(|_| target.to_path_buf());
    #[cfg(windows)]
    {
        a.to_string_lossy().eq_ignore_ascii_case(&b.to_string_lossy())
    }
    #[cfg(not(windows))]
    {
        a == b
    }
}

/// 拼接说明（空串时直接返回新说明）。
fn join_note(current: &str, extra: &str) -> String {
    if current.trim().is_empty() {
        extra.to_string()
    } else {
        format!("{current}；{extra}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AgentConfig;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    /// C# 版 StarAgent.config 样本（两版同名同格式；含 Rust 不认识的 Priority 属性）。
    const CSHARP_CONFIG: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<StarAgent>
  <LocalPort>5600</LocalPort>
  <Services>
    <ServiceInfo Priority="High" Name="demo-agent" FileName="demo-agent.exe" Arguments="--port 8080"
      WorkingDirectory="..\agents\demo" Enable="true" Mode="11" ReloadOnChange="true" />
    <ServiceInfo Priority="Normal" Name="site-app" FileName="site-app.zip" Arguments="urls=http://*:8080"
      WorkingDirectory="../apps/site-app" Enable="false" Mode="11" />
    <ServiceInfo Priority="Normal" Name="dotnet-app" FileName="dotnet" Arguments="App.dll"
      WorkingDirectory="..\server" Enable="true" Mode="10" HealthCheck="http://127.0.0.1:6600/ping" />
  </Services>
</StarAgent>
"#;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ragent-migrate-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 造一个“C# 星尘安装目录”（含 Config/StarAgent.config）。
    fn fake_source(tag: &str) -> PathBuf {
        let dir = temp_dir(tag);
        std::fs::create_dir_all(dir.join("Config")).unwrap();
        std::fs::write(config_file(&dir), CSHARP_CONFIG).unwrap();
        dir
    }

    #[test]
    fn scan_reads_csharp_config() {
        let src = fake_source("scan");
        let scan = scan(Some(&src)).unwrap();
        assert_eq!(scan.local_port, 5600, "应读取 C# 的 LocalPort");
        assert_eq!(scan.service_name, CSHARP_SERVICE_NAME);
        assert_eq!(scan.apps.len(), 3);
        let demo = scan.apps.iter().find(|a| a.name == "demo-agent").unwrap();
        assert_eq!(demo.file_name, "demo-agent.exe");
        // 扫描阶段保留 C# 原始模式值（面板展示时再归一化）
        assert_eq!(demo.mode_text(), "11");
        let web = scan.apps.iter().find(|a| a.name == "site-app").unwrap();
        assert!(!web.enable);
        let _ = std::fs::remove_dir_all(&src);
    }

    #[test]
    fn scan_rejects_directory_without_config() {
        let dir = temp_dir("noconfig");
        let err = scan(Some(&dir)).unwrap_err();
        assert!(err.contains("没有星尘配置"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn absolutize_rewrites_relative_paths() {
        let src = fake_source("abs");
        std::fs::create_dir_all(src.join("bin")).unwrap();
        std::fs::write(src.join("bin/demo-agent.exe"), b"MZ").unwrap();

        let mut app = AppConfig {
            name: "demo-agent".to_string(),
            file_name: "bin/demo-agent.exe".to_string(),
            working_directory: Some("bin".to_string()),
            ..Default::default()
        };
        absolutize(&src, &mut app);

        let wd = app.working_directory.clone().unwrap();
        assert!(Path::new(&wd).is_absolute(), "工作目录应绝对化：{wd}");
        assert!(wd.ends_with("bin"), "{wd}");
        assert!(
            Path::new(&app.file_name).is_absolute(),
            "程序路径应绝对化：{}",
            app.file_name
        );
        assert!(Path::new(&app.file_name).is_file(), "应指向真实文件");
        let _ = std::fs::remove_dir_all(&src);
    }

    #[test]
    fn migrate_imports_without_touching_source() {
        let src = fake_source("import");
        let base = temp_dir("agent");
        let manager = AppManager::new(&base, AgentConfig::default());

        let opts = Options {
            dir: src.clone(),
            stop_source_apps: false,
            stop_source_service: false,
            disable_source_service: false,
            start_apps: false,
            only: vec!["demo-agent".to_string(), "dotnet-app".to_string()],
        };
        let report = migrate(&manager, &opts).unwrap();
        assert_eq!(report.imported.len(), 2);

        let cfg = manager.config();
        let demo = cfg.find_app("demo-agent").expect("应导入 demo-agent");
        assert!(
            Path::new(demo.working_directory.as_deref().unwrap()).is_absolute(),
            "工作目录应绝对化"
        );
        assert!(demo.enable);
        let dotnet = cfg.find_app("dotnet-app").expect("应导入 dotnet-app");
        assert_eq!(dotnet.file_name, "dotnet", "裸命令保持原样");
        assert_eq!(dotnet.mode_text(), "standard");
        assert_eq!(dotnet.health_check.as_deref(), Some("http://127.0.0.1:6600/ping"));
        // 未选中的条目不导入
        assert!(cfg.find_app("site-app").is_none());

        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn migrate_refuses_same_directory() {
        let base = temp_dir("self");
        std::fs::create_dir_all(base.join("Config")).unwrap();
        std::fs::write(config_file(&base), CSHARP_CONFIG).unwrap();
        let manager = AppManager::new(&base, AgentConfig::default());

        let opts = Options { dir: base.clone(), ..Options::default() };
        let err = migrate(&manager, &opts).unwrap_err();
        assert!(err.contains("无需迁移"), "{err}");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn migrate_stops_source_apps_via_udp_rpc() {
        // 找一个空闲 UDP 端口（绑定后立即释放，随后交给假 C# 星尘）
        let port = {
            let sock = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
            sock.local_addr().unwrap().port()
        };
        let received: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let shutdown = Arc::new(AtomicBool::new(false));

        let src = temp_dir("rpc");
        std::fs::create_dir_all(src.join("Config")).unwrap();
        std::fs::write(
            config_file(&src),
            format!(
                "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<StarAgent>\n  <LocalPort>{port}</LocalPort>\n  <Services>\n    <ServiceInfo Name=\"a1\" FileName=\"a1.exe\" WorkingDirectory=\"../a1\" Enable=\"true\" Mode=\"11\" />\n  </Services>\n</StarAgent>\n"
            ),
        )
        .unwrap();

        let server = {
            let received = received.clone();
            let shutdown = shutdown.clone();
            std::thread::spawn(move || {
                let _ = api_rpc::serve_udp_until(
                    &format!("127.0.0.1:{port}"),
                    true,
                    shutdown.as_ref(),
                    move |action, args| {
                        received
                            .lock()
                            .unwrap()
                            .push(format!("{action} {}", args.unwrap_or("")));
                        api_rpc::ApiReply {
                            success: true,
                            message: "服务停止成功".to_string(),
                            service_name: "a1".to_string(),
                        }
                    },
                );
            })
        };

        // 等假 C# 星尘就绪（UDP 无连接，用 Ping 探活；只影响测试稳定性）
        let addr = format!("127.0.0.1:{port}");
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if api_rpc::invoke(&addr, "Ping", None, Duration::from_millis(300)).is_ok() {
                break;
            }
            assert!(Instant::now() < deadline, "假 C# 星尘未就绪");
            std::thread::sleep(Duration::from_millis(50));
        }

        let base = temp_dir("agent-rpc");
        let manager = AppManager::new(&base, AgentConfig::default());
        let opts = Options {
            dir: src.clone(),
            stop_source_apps: true,
            stop_source_service: false,
            disable_source_service: false,
            start_apps: false,
            only: Vec::new(),
        };
        let report = migrate(&manager, &opts).unwrap();
        assert_eq!(report.source_apps_stopped, 1, "应通过 C# RPC 停止 1 个子服务");

        let got = received.lock().unwrap().clone();
        assert!(
            got.iter().any(|s| s.starts_with("StopService") && s.contains("a1")),
            "C# 侧应收到 StopService：{got:?}"
        );

        shutdown.store(true, Ordering::Relaxed);
        let _ = server.join();
        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&base);
    }
}
