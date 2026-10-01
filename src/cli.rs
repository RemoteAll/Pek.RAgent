//! 命令行与控制台菜单。
//!
//! 命令与 C# StarAgent / NewLife.Agent 对齐（`-` 前缀可省略、大小写不敏感）：
//! - **服务级**：`-status` / `-install`（安装并启动）/ `-i`（仅安装）/ `-reinstall` /
//!   `-uninstall`（停止并卸载）/ `-u`（仅卸载）/ `-start` / `-stop` / `-restart` / `-run` / `-s`
//! - **应用级**：`-ListServices` / `-StartService <名称>` / `-StopService <名称>` / `-RestartService <名称>`
//! - **升级**：`-update [文件]`（用新版本文件升级当前程序并重启服务）；`-selftest`（升级管线内部使用）
//! - **其它**：`-ShowMachineInfo`；位置参数 zip 一次性拉起（`pek-ragent app.zip urls=http://*:8080`）
//!
//! 应用级命令通过本地控制接口（默认 127.0.0.1:5500）与运行中的代理通信，
//! 与 DHDeploy 使用同一契约。

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::agent::Agent;
use crate::config::AgentConfig;
use crate::deploy::{self, PrepareContext};
use crate::service::{ServiceManager, ServiceState};
use crate::sys::{self, SpawnRequest};
use crate::util;

/// 应用操作类型。
#[derive(Clone, Copy, PartialEq, Eq)]
enum AppOp {
    Start,
    Stop,
    Restart,
}

impl AppOp {
    fn action(self) -> &'static str {
        match self {
            AppOp::Start => "StartService",
            AppOp::Stop => "StopService",
            AppOp::Restart => "RestartService",
        }
    }

    fn text(self) -> &'static str {
        match self {
            AppOp::Start => "启动",
            AppOp::Stop => "停止",
            AppOp::Restart => "重启",
        }
    }
}

/// 服务操作类型。
#[derive(Clone, Copy)]
enum SvcOp {
    Start,
    Stop,
    Restart,
}

/// 入口。返回退出码。
pub fn run(args: &[String], base: &Path) -> i32 {
    // 延迟启动（升级场景，兼容 C#：-upgrade / -delay 先等 3 秒）
    if args.iter().any(|a| {
        let t = a.trim();
        t.eq_ignore_ascii_case("-upgrade") || t.eq_ignore_ascii_case("-delay")
    }) {
        util::log_info("延迟启动，等待 3 秒");
        std::thread::sleep(Duration::from_secs(3));
    }

    // -server / -project 参数保存（脚本兼容；当前版本暂不对接服务端）
    save_server_args(base, args);

    let Some(first) = args.iter().find(|a| !a.trim().is_empty()) else {
        return menu(base);
    };

    let cmd = first.trim().trim_start_matches('-').to_ascii_lowercase();
    match cmd.as_str() {
        "s" | "service" => agent_run(base, true),
        "run" | "simulate" | "console" => agent_run(base, false),
        "status" | "state" => cmd_status(base),
        "install" => cmd_install(base, true),
        "i" | "installonly" => cmd_install(base, false),
        "reinstall" => cmd_reinstall(base),
        "uninstall" => cmd_uninstall(base, true),
        "u" | "remove" => cmd_uninstall(base, false),
        "start" => cmd_svc_ctl(base, SvcOp::Start),
        "stop" => cmd_svc_ctl(base, SvcOp::Stop),
        "restart" => cmd_svc_ctl(base, SvcOp::Restart),
        "listservices" | "list" | "services" => cmd_list_apps(base),
        "startservice" => cmd_app_op_cli(base, args, AppOp::Start),
        "stopservice" => cmd_app_op_cli(base, args, AppOp::Stop),
        "restartservice" => cmd_app_op_cli(base, args, AppOp::Restart),
        "addservice" | "add" => cmd_add_service(base, args),
        "showmachineinfo" | "machineinfo" | "info" => {
            println!("{}", sys::machine_info());
            0
        }
        "help" | "h" | "?" => {
            print_help();
            0
        }
        "version" | "ver" | "v" => {
            println!(
                "Pek.RAgent v{}（StarAgent 的 Rust 实现）",
                env!("CARGO_PKG_VERSION")
            );
            0
        }
        "selftest" => cmd_selftest(base),
        "update" => cmd_update(base, args),
        "ensure-running" => cmd_ensure_running(base),
        _ => {
            if cmd.ends_with(".zip")
                || args
                    .iter()
                    .any(|a| a.trim().to_ascii_lowercase().ends_with(".zip"))
            {
                return zip_deploy(base, args);
            }

            println!("未知命令：{}", first);
            print_help();
            2
        }
    }
}

/// 前台/服务方式运行代理。
fn agent_run(base: &Path, service_mode: bool) -> i32 {
    // 重新加载配置（run 时以磁盘配置为准）
    let agent = Agent::boot(base);
    if service_mode {
        agent.run_service()
    } else {
        agent.run_foreground()
    }
}

// ————— 服务级命令 —————

/// 显示状态（`-status`；输出结构对齐 C# `ShowStatus`：状态块 + 附加信息 + 最近日志）。
fn cmd_status(base: &Path) -> i32 {
    let cfg = AgentConfig::load(base);
    let svc = ServiceManager::new(base, &cfg);
    let state = svc.query();

    print_status_core(&cfg, &svc, state);

    // —— 以下为 Pek.RAgent 附加信息（配置/端口/子服务/日志） ——
    println!();
    println!("配置：{}", crate::config::config_path(base).display());
    println!(
        "本地端口：{}（仅本机：{}）",
        cfg.local_port,
        if cfg.local_only { "是" } else { "否" }
    );

    match fetch_services(base) {
        Some(list) => {
            let running = list.iter().filter(|e| e.2).count();
            println!("子服务：{} 个，运行中 {}", list.len(), running);
        }
        None => {
            println!(
                "子服务：{} 个（代理未运行，无法读取运行状态）",
                cfg.apps.len()
            );
        }
    }

    // 最近日志（对齐 C# 状态输出附带的日志尾）
    print_recent_logs(base, 5);

    0
}

/// 状态块（对齐 C# `ShowStatus`：服务/描述/状态/路径 + 空行 + 版本行）。
///
/// 菜单启动与 `-status` 共用（C# 无参数运行时会先输出该块再进入菜单）。
fn print_status_core(cfg: &AgentConfig, svc: &ServiceManager, state: ServiceState) {
    println!();
    // 显示名与服务名相同时只显示一个（同 C#）
    if cfg.display_name == svc.name {
        println!("服务：{}", svc.name);
    } else {
        println!("服务：{}({})", cfg.display_name, svc.name);
    }
    println!("描述：{}", cfg.description);

    // 状态：{管理器} {状态}；Windows 附加管理员/普通用户提示（unix 无此区分）
    #[cfg(windows)]
    let status = format!(
        "{}{}",
        state.text(),
        if is_elevated() {
            "（管理员）"
        } else {
            "（普通用户）"
        }
    );
    #[cfg(not(windows))]
    let status = state.text().to_string();
    println!("状态：{} {}", svc.init_name(), status);

    if state != ServiceState::NotInstalled {
        // 优先显示服务注册的实际程序路径（当前进程可能是从开发输出目录等其它位置启动的）
        let exe = svc.installed_exe().unwrap_or_else(|| svc.exe.clone());
        println!("路径：{}", exe.display());
    }

    // 版本与发布时间（对齐 C#：`{名称}\t版本：{x}\t发布：{yyyy-MM-dd HH:mm:ss}`）
    println!();
    println!(
        "Pek.RAgent\t版本：{}\t发布：{}",
        env!("CARGO_PKG_VERSION"),
        build_time_text()
    );
}

/// 打印最近日志尾（`-status` 附带显示，对齐 C# 状态输出中的日志行）。
fn print_recent_logs(base: &Path, count: usize) {
    let dir = base.join("Log");
    let Some(path) = dhrust::io::latest_file_by_ext(&dir, ".log") else {
        return;
    };
    let lines = dhrust::io::read_tail(&path, count);
    if lines.is_empty() {
        return;
    }

    println!();
    println!(
        "最近日志（{}）：",
        path.file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default()
    );
    for line in lines {
        println!("{line}");
    }
}

/// 构建时间文本（`build.rs` 注入的 Unix 秒，本地时区格式化）。
fn build_time_text() -> String {
    std::option_env!("PEK_RAGENT_BUILD_UNIX")
        .and_then(|s| s.parse::<i64>().ok())
        .and_then(|secs| chrono::DateTime::from_timestamp(secs, 0))
        .map(|utc| {
            utc.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M:%S")
                .to_string()
        })
        .unwrap_or_default()
}

/// 安装服务。
fn cmd_install(base: &Path, start: bool) -> i32 {
    let cfg = AgentConfig::load(base);
    let svc = ServiceManager::new(base, &cfg);

    println!("正在安装服务 {}（{}）...", cfg.display_name, svc.name);
    match svc.install(start) {
        Ok(()) => {
            if start {
                println!("服务安装成功，并已启动。");
            } else {
                println!("服务安装成功，可使用 -start 启动。");
            }
            0
        }
        Err(e) => {
            eprintln!("安装失败：{}", e);
            1
        }
    }
}

/// 重新安装服务。
fn cmd_reinstall(base: &Path) -> i32 {
    let cfg = AgentConfig::load(base);
    let svc = ServiceManager::new(base, &cfg);

    println!("正在重新安装服务 {}...", svc.name);
    match svc.reinstall() {
        Ok(()) => {
            println!("服务重新安装成功，并已启动。");
            0
        }
        Err(e) => {
            eprintln!("重新安装失败：{}", e);
            1
        }
    }
}

/// 卸载服务。
fn cmd_uninstall(base: &Path, stop: bool) -> i32 {
    let cfg = AgentConfig::load(base);
    let svc = ServiceManager::new(base, &cfg);

    println!("正在卸载服务 {}...", svc.name);
    match svc.uninstall(stop) {
        Ok(()) => {
            // 流量统计清理：删除 nftables 计数表（inet pek_stats；未启用/无表时静默）
            crate::portstat::cleanup();
            println!("服务卸载成功。");
            0
        }
        Err(e) => {
            eprintln!("卸载失败：{}", e);
            1
        }
    }
}

/// 服务启停控制。
fn cmd_svc_ctl(base: &Path, op: SvcOp) -> i32 {
    let cfg = AgentConfig::load(base);
    let svc = ServiceManager::new(base, &cfg);

    let rs = match op {
        SvcOp::Start => svc.start(),
        SvcOp::Stop => svc.stop(),
        SvcOp::Restart => svc.restart(),
    };

    match rs {
        Ok(()) => {
            let text = match op {
                SvcOp::Start => "启动成功",
                SvcOp::Stop => "停止成功",
                SvcOp::Restart => "重启成功",
            };
            println!("服务{}。", text);
            0
        }
        Err(e) => {
            let text = match op {
                SvcOp::Start => "启动失败",
                SvcOp::Stop => "停止失败",
                SvcOp::Restart => "重启失败",
            };
            eprintln!("{}：{}", text, e);
            1
        }
    }
}

// ————— 程序升级 —————

/// 影子自检：供升级管线从影子位置验证新版程序可用（不启动服务、不绑定端口）。
///
/// 设计原则：只做快速同步检查（配置可解析、程序目录可写），任何情况下都应立即退出，
/// 避免拖慢升级流程；退出码 0 表示通过。
fn cmd_selftest(base: &Path) -> i32 {
    // 配置可解析（兼容旧配置）
    let _ = AgentConfig::load(base);

    // 程序目录可写（升级暂存、日志等依赖）
    let probe = base.join(".selftest");
    if let Err(e) = std::fs::write(&probe, b"ok") {
        eprintln!("自检失败：程序目录不可写：{e}");
        return 1;
    }
    let _ = std::fs::remove_file(&probe);

    println!("selftest ok（v{}）", env!("CARGO_PKG_VERSION"));
    0
}

/// 用新版本文件升级当前程序并重启服务（含影子冒烟，失败时当前程序保持不变）。
///
/// 用法：`pek-ragent -update [文件路径]`；省略路径时使用暂存约定路径 `{exe}.new`。
/// 文件名与目录不限：任意可读文件均可（内部会暂存到程序目录再执行升级管线）。
fn cmd_update(base: &Path, args: &[String]) -> i32 {
    let Ok(current) = std::env::current_exe() else {
        eprintln!("无法定位当前程序文件");
        return 1;
    };
    let exe = util::lexical_normalize(&current);
    let staged = PathBuf::from(format!("{}.new", exe.display()));

    // 源文件：命令行中的第一个非选项参数（文件名/路径任意）
    let src = args
        .iter()
        .map(|a| a.trim())
        .find(|a| !a.is_empty() && !a.starts_with('-') && !a.eq_ignore_ascii_case("update"))
        .map(PathBuf::from);

    if let Some(src) = &src {
        if *src != staged {
            if let Err(e) = std::fs::copy(src, &staged) {
                eprintln!("读取升级文件失败：{}（{e}）", src.display());
                return 1;
            }
        }
    }

    if !staged.is_file() {
        eprintln!(
            "未找到升级文件：{}（用法：pek-ragent -update <文件路径>；或把新版本文件放入 Update 目录）",
            staged.display()
        );
        return 1;
    }

    // 停止服务释放旧程序文件占用（服务未安装/未运行时忽略错误）
    let cfg = AgentConfig::load(base);
    let svc = ServiceManager::new(base, &cfg);
    let _ = svc.stop();

    match crate::agent::apply_upgrade(&staged, &exe, 0) {
        Ok(()) => {
            println!("升级完成：程序文件已替换，正在启动服务……");
            match svc.start() {
                Ok(()) => {
                    println!("服务已启动。");
                    0
                }
                Err(e) => {
                    eprintln!("程序已更新，但服务启动失败：{e}（可稍后执行 -start 重试）");
                    1
                }
            }
        }
        Err(e) => {
            let _ = std::fs::remove_file(&staged);
            eprintln!("升级失败（当前程序保持不变）：{e}");
            // 恢复服务运行（旧版本）
            let _ = svc.start();
            1
        }
    }
}

/// 确保系统服务处于运行状态（升级重启助手）：等待服务管理器自动拉起，超时后显式启动。
///
/// 由升级完成后的新版本进程以 `-ensure-running -upgrade` 调用（`-upgrade` 先延迟 3 秒等旧进程退出）：
/// 1. 服务未安装、或指向其他程序时不做处理；
/// 2. 等待服务管理器自动拉起（SCM 失败恢复动作有 5s/10s/30s 三档窗口；systemd 通常秒级），
///    最长等待 40 秒，一旦运行立即完成；
/// 3. 超时仍未运行则显式启动（此时恢复动作窗口已过，不会重复拉起）。
fn cmd_ensure_running(base: &Path) -> i32 {
    let cfg = AgentConfig::load(base);
    let svc = ServiceManager::new(base, &cfg);

    if svc.query() == ServiceState::NotInstalled {
        util::log_info("重启助手：未安装系统服务（前台运行模式），请手动重启程序");
        println!("未安装系统服务（前台运行模式），请手动重启程序。");
        return 0;
    }

    // 服务指向其他程序（非本部署）时不操作
    let Ok(current) = std::env::current_exe() else {
        return 1;
    };
    let current = util::lexical_normalize(&current);
    match svc.installed_exe() {
        Some(path) if same_path(&path, &current) => {}
        Some(path) => {
            util::log_info(&format!(
                "重启助手：系统服务指向其他程序（{}），无需处理",
                path.display()
            ));
            println!("系统服务指向其他程序（{}），无需处理。", path.display());
            return 0;
        }
        None => {
            util::log_info("重启助手：无法确定系统服务指向的程序，跳过自动拉起（可手动执行 -start）");
            println!("无法确定系统服务指向的程序，跳过自动拉起（可手动执行 -start）。");
            return 1;
        }
    }

    // 等待服务管理器自动拉起（覆盖 SCM 5s/10s/30s 三档恢复窗口）
    let deadline = Instant::now() + Duration::from_secs(40);
    loop {
        if svc.query() == ServiceState::Running {
            util::log_info("重启助手：服务已由服务管理器自动拉起");
            println!("服务已由服务管理器自动拉起。");
            return 0;
        }
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_secs(2));
    }

    // 超时：显式启动（恢复动作窗口已过，不会重复拉起）
    match svc.start() {
        Ok(()) => {
            util::log_info("重启助手：服务已显式启动（新版本）");
            println!("服务已显式启动（新版本）。");
            0
        }
        Err(e) => {
            if svc.query() == ServiceState::Running {
                util::log_info("重启助手：服务已在运行");
                println!("服务已在运行。");
                return 0;
            }
            util::log_error(&format!("重启助手：服务自动拉起失败：{e}（请手动执行 -start 重试）"));
            eprintln!("服务自动拉起失败：{e}（请手动执行 -start 重试）");
            1
        }
    }
}

/// 路径是否指向同一程序（Windows 大小写不敏感）。
fn same_path(a: &Path, b: &Path) -> bool {
    #[cfg(windows)]
    {
        a.to_string_lossy().eq_ignore_ascii_case(&b.to_string_lossy())
    }
    #[cfg(not(windows))]
    {
        a == b
    }
}

// ————— 应用级命令 —————

/// 查看子服务。
fn cmd_list_apps(base: &Path) -> i32 {
    let Some(list) = fetch_services(base) else {
        print_agent_not_running();
        return 1;
    };

    if list.is_empty() {
        println!("没有配置任何子服务。");
        return 0;
    }

    println!("所有子服务列表：");
    println!(
        "{:<5} {:<20} {:<6} {:<8} {:<8} {:<16} {}",
        "序号", "服务名称", "启用", "状态", "进程Id", "进程名称", "启动时间"
    );
    for (i, (name, enable, running, pid, pname, start_time)) in list.iter().enumerate() {
        println!(
            "{:<7} {:<22} {:<8} {:<10} {:<10} {:<18} {}",
            i + 1,
            name,
            if *enable { "是" } else { "否" },
            if *running { "运行中" } else { "停止" },
            if *running {
                pid.to_string()
            } else {
                String::new()
            },
            pname,
            start_time
        );
    }

    0
}

/// 命令行注册子服务：`-AddService <名称> <程序路径> [工作目录] [启动参数]`。
///
/// 供安装脚本（如 DHDeploy Agent 的 install.sh）调用：
/// 写入配置（启用）后，星尘在运行则立即重载并启动；否则待星尘启动时自动拉起。
fn cmd_add_service(base: &Path, args: &[String]) -> i32 {
    // 收集命令词之后的位置参数（对前导空串/其它前缀鲁棒）
    let mut params: Vec<&str> = Vec::new();
    let mut seen_cmd = false;
    for a in args {
        let t = a.trim();
        if t.is_empty() {
            continue;
        }
        if !seen_cmd {
            if t.trim_start_matches('-')
                .eq_ignore_ascii_case("addservice")
                || t.trim_start_matches('-').eq_ignore_ascii_case("add")
            {
                seen_cmd = true;
            }
            continue;
        }
        params.push(t);
    }

    if params.len() < 2 || params[0].starts_with('-') || params[1].starts_with('-') {
        println!("用法：-AddService <名称> <程序路径> [工作目录] [启动参数]");
        println!("示例：pek-ragent -AddService myapp /opt/myapp/myapp /opt/myapp");
        return 2;
    }

    let name = params[0];
    let file = params[1];
    let dir = params.get(2).copied();
    let pargs = params.get(3).copied();

    let mut cfg = AgentConfig::load(base);
    let existed = cfg.find_app(name).is_some();
    if !cfg.upsert_app(name, file, dir, pargs) {
        println!("注册失败：服务名称与程序路径不能为空");
        return 1;
    }
    if let Err(e) = cfg.save(base) {
        println!("写入配置文件失败：{e}");
        return 1;
    }
    println!(
        "已{}子服务 [{}] → {}（启用{}）",
        if existed { "更新" } else { "注册" },
        name,
        file,
        dir.map(|d| format!("，工作目录 {d}")).unwrap_or_default()
    );

    if !probe_agent_alive(base) {
        println!("星尘未运行：配置已保存，星尘启动时会自动拉起该服务");
        return 0;
    }

    match api_get(base, "ReloadConfig", Duration::from_secs(30)) {
        Ok(_) => println!("星尘已重新加载配置"),
        Err(e) => println!("重载请求失败（{e}），星尘将在下一轮自动检测（≤30 秒）"),
    }

    cmd_app_op(base, AppOp::Start, name)
}

/// 命令行应用操作：`-StartService <名称>`。
fn cmd_app_op_cli(base: &Path, args: &[String], op: AppOp) -> i32 {
    let name = args
        .iter()
        .skip(1)
        .map(|a| a.trim())
        .find(|a| !a.is_empty() && !a.starts_with('-'));

    let Some(name) = name else {
        eprintln!("请提供服务名称：-{}Service <名称>", op.text());
        return 2;
    };

    cmd_app_op(base, op, name)
}

/// 执行应用操作（经本地控制接口）。
fn cmd_app_op(base: &Path, op: AppOp, name: &str) -> i32 {
    let action = format!(
        "{}?serviceName={}",
        op.action(),
        dhrust::web::url_encode(name)
    );

    match api_get(base, &action, Duration::from_secs(60)) {
        Ok(text) => {
            let value: serde_json::Value =
                serde_json::from_str(&text).unwrap_or(serde_json::Value::Null);
            let success = value["Success"].as_bool().unwrap_or(false);
            let message = value["Message"]
                .as_str()
                .map(|s| s.to_string())
                .unwrap_or_else(|| text.clone());

            println!("{}", message);
            if success {
                0
            } else {
                1
            }
        }
        Err(e) => {
            eprintln!("调用失败：{}", e);
            print_agent_not_running();
            1
        }
    }
}

// ————— 菜单 —————

/// 控制台菜单（无参数启动）。
fn menu(base: &Path) -> i32 {
    // 启动时输出一次状态块（对齐 C#：无参数运行先 ShowStatus，再进入菜单循环）
    {
        let cfg = AgentConfig::load(base);
        let svc = ServiceManager::new(base, &cfg);
        let state = svc.query();
        print_status_core(&cfg, &svc, state);
        if !cfg.server.is_empty() {
            println!("服务端：{}（当前版本暂不对接）", cfg.server);
        }
    }

    loop {
        let cfg = AgentConfig::load(base);
        let svc = ServiceManager::new(base, &cfg);
        let state = svc.query();
        let installed = state != ServiceState::NotInstalled;
        let running = state == ServiceState::Running;
        // 代理运行中判定（探测本地控制接口——服务模式与前台模拟运行模式均可探测到）
        let agent_alive = probe_agent_alive(base);

        // 菜单列表（每轮重绘；服务状态信息在启动时已输出，与 C# 一致）
        println!();
        println!(" 序号 功能名称            命令行参数");
        println!(" 1、 显示状态            -status");
        if installed {
            println!(" 2、 卸载服务            -uninstall");
        } else {
            println!(" 2、 安装服务            -install");
        }
        if running {
            println!(" 3、 停止服务            -stop");
        } else {
            println!(" 3、 启动服务            -start");
        }
        println!(" 4、 重启服务            -restart");
        println!(" 5、 模拟运行            -run");
        // 子服务操作依赖运行中的代理（本地控制接口）；未运行时隐藏，避免“选中必失败”的无意义项
        if agent_alive {
            println!(" 6、 查看子服务          -ListServices");
            println!(" 7、 启动子服务          -StartService");
            println!(" 8、 停止子服务          -StopService");
            println!(" 9、 重启子服务          -RestartService");
        }
        println!(" t、 服务器信息          -ShowMachineInfo");
        println!(" 0、 退出");
        print!(" 请输入命令序号：");
        let _ = std::io::stdout().flush();

        let mut line = String::new();
        match std::io::stdin().read_line(&mut line) {
            // EOF（无终端 / 管道输入结束）：退出菜单，避免空输入死循环刷屏
            Ok(0) => return 0,
            Ok(_) => {}
            Err(_) => return 1,
        }

        let key = line.trim().to_ascii_lowercase();
        if key.is_empty() {
            continue;
        }

        match key.as_str() {
            "0" | "q" | "quit" | "exit" => return 0,
            "1" => {
                cmd_status(base);
            }
            "2" => {
                if installed {
                    cmd_uninstall(base, true);
                } else {
                    cmd_install(base, true);
                }
            }
            "3" => {
                if running {
                    cmd_svc_ctl(base, SvcOp::Stop);
                } else {
                    cmd_svc_ctl(base, SvcOp::Start);
                }
            }
            "4" => {
                cmd_svc_ctl(base, SvcOp::Restart);
            }
            "5" => {
                agent_run(base, false);
            }
            "6" => {
                cmd_list_apps(base);
            }
            "7" => {
                menu_app_op(base, AppOp::Start);
            }
            "8" => {
                menu_app_op(base, AppOp::Stop);
            }
            "9" => {
                menu_app_op(base, AppOp::Restart);
            }
            "t" => {
                println!("{}", sys::machine_info());
            }
            other => println!("无效命令序号：[{}]", other),
        }
    }
}

/// 菜单内选择应用并操作。
fn menu_app_op(base: &Path, op: AppOp) {
    let Some(list) = fetch_services(base) else {
        print_agent_not_running();
        return;
    };

    if list.is_empty() {
        println!("没有配置任何子服务。");
        return;
    }

    println!("请选择要{}的服务：", op.text());
    for (i, (name, enable, running, pid, pname, _)) in list.iter().enumerate() {
        let status = if *running {
            format!("运行中 PID={} {}", pid, pname)
        } else {
            "已停止".to_string()
        };
        println!(
            " {}. {} [{}] {}",
            i + 1,
            name,
            if *enable { "启用" } else { "禁用" },
            status
        );
    }
    println!(" 0. 返回主菜单");
    print!(" 请输入服务序号或名称（0 返回）：");
    let _ = std::io::stdout().flush();

    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return;
    }
    let input = line.trim();
    if input.is_empty() || input == "0" {
        return;
    }

    let name = if let Ok(index) = input.parse::<usize>() {
        if index >= 1 && index <= list.len() {
            list[index - 1].0.clone()
        } else {
            input.to_string()
        }
    } else {
        input.to_string()
    };

    println!("准备{}服务：{}", op.text(), name);
    cmd_app_op(base, op, &name);
}

// ————— zip 一次性拉起（位置参数，等价 C# `staragent app.zip args…`） —————

/// 位置参数 zip 发布：解压到影子目录并拉起（一次性，不纳入守护）。
fn zip_deploy(base: &Path, args: &[String]) -> i32 {
    let mut name: Option<String> = None;
    let mut shadow: Option<PathBuf> = None;
    let mut zip: Option<PathBuf> = None;
    let mut passthrough: Vec<String> = Vec::new();

    let mut i = 0;
    while i < args.len() {
        let a = args[i].trim();
        if a.eq_ignore_ascii_case("-name") && i + 1 < args.len() {
            name = Some(args[i + 1].clone());
            i += 2;
            continue;
        }
        if a.eq_ignore_ascii_case("-shadow") && i + 1 < args.len() {
            shadow = Some(PathBuf::from(args[i + 1].trim()));
            i += 2;
            continue;
        }
        if a.to_ascii_lowercase().ends_with(".zip") {
            zip = Some(PathBuf::from(a));
            i += 1;
            continue;
        }
        passthrough.push(a.to_string());
        i += 1;
    }

    let Some(zip) = zip else {
        eprintln!("未指定 zip 文件");
        return 2;
    };

    // 相对路径：先按当前目录，再按基础目录
    let zip = if zip.is_absolute() {
        zip
    } else {
        let cwd_candidate = std::env::current_dir()
            .map(|d| d.join(&zip))
            .unwrap_or_else(|_| zip.clone());
        if cwd_candidate.is_file() {
            cwd_candidate
        } else {
            base.join(&zip)
        }
    };

    if !zip.is_file() {
        eprintln!("找不到 zip 文件：{}", zip.display());
        return 2;
    }

    let app_name = name.unwrap_or_else(|| {
        zip.file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "app".to_string())
    });

    let mut cfg = AgentConfig::load(base);
    cfg.normalize();
    let work = zip.parent().unwrap_or(base);

    // 构造一次性应用（影子模式），复用统一部署准备
    let app_cfg = crate::config::AppConfig {
        name: app_name.clone(),
        file_name: zip
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_default(),
        arguments: if passthrough.is_empty() {
            None
        } else {
            Some(passthrough.join(" "))
        },
        working_directory: Some(work.to_string_lossy().into_owned()),
        mode: "shadow".to_string(),
        enable: true,
        ..Default::default()
    };

    let ctx = PrepareContext {
        base,
        global: &cfg,
        retry: false,
        shadow_override: shadow.as_deref(),
    };

    let prepared = match deploy::prepare(&ctx, &app_cfg) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("部署失败：{}", e);
            return 1;
        }
    };

    if prepared.hosted {
        println!("{} 为托管模式，已完成解压。", app_name);
        return 0;
    }

    let request = SpawnRequest {
        program: &prepared.program,
        args: &prepared.args,
        cwd: &prepared.work_dir,
        envs: &prepared.envs,
        log_file: None,
        detached: true,
    };

    match sys::spawn(&request) {
        Ok(child) => {
            println!(
                "已启动 {} PID={}，工作目录 {}",
                app_name,
                child.id(),
                prepared.work_dir.display()
            );
            if let Some(shadow) = &prepared.shadow {
                println!("影子目录 {}", shadow.display());
            }
            0
        }
        Err(e) => {
            eprintln!("启动失败：{}", e);
            1
        }
    }
}

// ————— 辅助 —————

/// 调用本地控制接口。
fn api_get(base: &Path, action: &str, timeout: Duration) -> Result<String, String> {
    let cfg = AgentConfig::load(base);
    let url = format!("http://127.0.0.1:{}/{}", cfg.local_port, action);
    dhrust::net::http_client::blocking_get_text(&url, timeout).map_err(|e| e.to_string())
}

/// 拉取子服务列表：`(名称, 启用, 运行中, 进程Id, 进程名称, 启动时间)`。
#[allow(clippy::type_complexity)]
fn fetch_services(base: &Path) -> Option<Vec<(String, bool, bool, u32, String, String)>> {
    let text = api_get(base, "GetServices", Duration::from_secs(5)).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    let services = value["Services"].as_array()?;

    let mut out = Vec::new();
    for item in services {
        out.push((
            item["Name"].as_str().unwrap_or("").to_string(),
            item["Enable"].as_bool().unwrap_or(false),
            item["Running"].as_bool().unwrap_or(false),
            item["ProcessId"].as_u64().unwrap_or(0) as u32,
            item["ProcessName"].as_str().unwrap_or("").to_string(),
            item["StartTime"].as_str().unwrap_or("").to_string(),
        ));
    }
    Some(out)
}

/// 探测代理是否运行中（本地控制接口可达）。
///
/// 代理可能以服务方式或前台模拟运行方式在跑，仅查服务状态无法覆盖后者，
/// 因此以控制接口探测为准；短超时（500ms）避免菜单卡顿（本机回环上无监听会即时失败）。
fn probe_agent_alive(base: &Path) -> bool {
    api_get(base, "Ping", Duration::from_millis(500)).is_ok()
}

/// 代理未运行时的提示。
fn print_agent_not_running() {
    println!("无法连接星尘代理（本地控制接口未响应）。");
    println!("提示：先执行 -install 安装并启动服务，或使用 -run 在前台运行。");
}

/// 保存 `-server` / `-project` 参数（脚本兼容；当前版本暂不对接服务端）。
fn save_server_args(base: &Path, args: &[String]) {
    let mut server: Option<String> = None;
    let mut project: Option<String> = None;

    for i in 0..args.len() {
        let a = args[i].trim();
        if a.eq_ignore_ascii_case("-server") && i + 1 < args.len() {
            server = Some(args[i + 1].trim().to_string());
        } else if a.eq_ignore_ascii_case("-project") && i + 1 < args.len() {
            project = Some(args[i + 1].trim().to_string());
        }
    }

    if server.is_none() && project.is_none() {
        return;
    }

    let mut cfg = AgentConfig::load(base);
    let mut changed = false;

    if let Some(s) = server {
        if !s.is_empty() && cfg.server != s {
            cfg.server = s;
            changed = true;
            println!("服务端地址已保存：{}（当前版本暂不对接）", cfg.server);
        }
    }
    if let Some(p) = project {
        if !p.is_empty() && cfg.project != p {
            cfg.project = p;
            changed = true;
        }
    }

    if changed {
        let _ = cfg.save(base);
    }
}

/// 是否管理员（仅 Windows 展示用）。
#[cfg(windows)]
fn is_elevated() -> bool {
    std::process::Command::new("net")
        .arg("session")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// 帮助文本。
fn print_help() {
    println!(
        r#"Pek.RAgent —— 星尘代理（StarAgent 的 Rust 实现）v{version}

用法：
  pek-ragent                        进入控制台菜单（推荐）
  pek-ragent -install [-server URL] 安装并启动系统服务
  pek-ragent -i                     仅安装系统服务
  pek-ragent -reinstall             重新安装（卸载后重装并启动）
  pek-ragent -uninstall             停止并卸载系统服务
  pek-ragent -u                     仅卸载系统服务
  pek-ragent -start | -stop | -restart
                                    启动 / 停止 / 重启系统服务
  pek-ragent -status                查看服务状态
  pek-ragent -run                   前台运行（模拟运行；回车或 Ctrl+C 退出）
  pek-ragent -s                     以服务方式运行（由系统服务管理器调用）

子服务（应用）管理：
  pek-ragent -ListServices          查看子服务列表
  pek-ragent -StartService <名称>   启动子服务
  pek-ragent -StopService <名称>    停止子服务（同时禁用，防止自动拉起）
  pek-ragent -RestartService <名称> 重启子服务
  pek-ragent -AddService <名称> <程序路径> [目录] [参数]
                                    注册子服务并启用（安装脚本用；星尘在跑则立即拉起）

一次性拉起 zip（影子目录，不纳入守护）：
  pek-ragent app.zip urls=http://*:8080
  pek-ragent app.zip -name myapp -shadow /data/shadow

程序升级：
  pek-ragent -update <文件路径>      用新版本文件升级并重启服务（推荐）
  （也可将新版本文件直接放入程序目录的 Update 子目录，自动热检测升级；
     Web 面板登录后可在"程序升级"卡片直接上传，无需任何文件名约定）
  （-selftest / -ensure-running 为升级管线内部命令，由程序自动调用）

其它：
  pek-ragent -ShowMachineInfo       显示本机信息
  pek-ragent -help                  显示本帮助

配置文件：Config/StarAgent.config（XML，与 C# StarAgent 同格式互通）
本地控制接口：http://127.0.0.1:5500（RestartService / StartService / StopService 等，兼容 DHDeploy）
"#,
        version = env!("CARGO_PKG_VERSION")
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ensure_running_skips_other_or_missing_service() {
        // 服务未安装（或指向其他程序）时应快速返回，不进入等待循环
        let dir = std::env::temp_dir().join(format!(
            "ragent-ensure-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let code = cmd_ensure_running(&dir);

        assert_eq!(code, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
