//! 应用运行时：单个应用的启动、停止、守护检查（退出检测/内存限制/文件变动）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

use crate::config::{AgentConfig, AppConfig};
use crate::deploy::{self, PrepareContext};
use crate::sys::{self, Handle, SpawnRequest};
use crate::util;

/// 应用状态快照。
#[derive(Clone, Debug)]
pub struct AppStatus {
    /// 是否运行中
    pub running: bool,
    /// 进程号
    pub pid: u32,
    /// 进程名
    pub process_name: String,
    /// 开始时间文本
    pub start_time: String,
}

/// 检查结果。
#[derive(Clone, Debug, Default)]
pub struct CheckOutcome {
    /// 是否需要持久化配置（任务模式自动禁用）
    pub changed: bool,
}

/// 启动结果。
#[derive(Clone, Debug, Default)]
pub struct StartResult {
    /// 本次是否成功启动（或接管）
    pub started: bool,
    /// 是否任务模式（运行一次后自动禁用，需要持久化）
    pub task: bool,
}

/// 应用运行时内部状态。
struct AppState {
    cfg: AppConfig,
    handle: Option<Handle>,
    pid: u32,
    process_name: String,
    start_time: Option<SystemTime>,
    /// 期望运行（托管模式亦为 true）
    running: bool,
    /// 托管（仅解压，外部宿主运行）
    hosted: bool,
    error_count: i32,
    fail_limit_logged: bool,
    next_start: Option<Instant>,
    work_dir: PathBuf,
    shadow: Option<PathBuf>,
    /// ReloadOnChange：文件变动快照（路径 → 最后修改秒）
    files: HashMap<PathBuf, u64>,
    files_primed: bool,
    reload_ready: Option<Instant>,
}

/// 应用运行时。
pub struct AppRuntime {
    /// 应用名
    pub name: String,
    base: PathBuf,
    state: Mutex<AppState>,
}

impl AppRuntime {
    /// 实例化。
    pub fn new(base: &Path, cfg: AppConfig) -> AppRuntime {
        AppRuntime {
            name: cfg.name.clone(),
            base: base.to_path_buf(),
            state: Mutex::new(AppState {
                cfg,
                handle: None,
                pid: 0,
                process_name: String::new(),
                start_time: None,
                running: false,
                hosted: false,
                error_count: 0,
                fail_limit_logged: false,
                next_start: None,
                work_dir: PathBuf::new(),
                shadow: None,
                files: HashMap::new(),
                files_primed: false,
                reload_ready: None,
            }),
        }
    }

    /// 更新配置（用于配置热更新）。
    pub fn set_cfg(&self, cfg: AppConfig) {
        let mut st = self.state.lock().unwrap();
        if serde_json::to_string(&st.cfg).unwrap_or_default()
            != serde_json::to_string(&cfg).unwrap_or_default()
        {
            st.cfg = cfg;
            st.error_count = 0;
            st.fail_limit_logged = false;
            st.next_start = None;
        }
    }

    /// 配置克隆。
    pub fn cfg(&self) -> AppConfig {
        self.state.lock().unwrap().cfg.clone()
    }

    /// 状态快照。
    pub fn status(&self) -> AppStatus {
        let st = self.state.lock().unwrap();
        if !st.running {
            return AppStatus {
                running: false,
                pid: 0,
                process_name: String::new(),
                start_time: String::new(),
            };
        }

        AppStatus {
            running: true,
            pid: st.pid,
            process_name: st.process_name.clone(),
            start_time: st
                .start_time
                .map(|t| {
                    let dt: chrono::DateTime<chrono::Local> = t.into();
                    dt.format("%Y-%m-%d %H:%M:%S").to_string()
                })
                .unwrap_or_default(),
        }
    }

    /// 接管已存在的进程（代理重启后继续守护，避免拉起重复实例）。
    pub fn adopt(&self, pid: u32, process_name: String) {
        let mut st = self.state.lock().unwrap();
        st.handle = Some(Handle::Adopted(pid));
        st.pid = pid;
        st.process_name = process_name;
        st.start_time = Some(SystemTime::now());
        st.running = true;
        st.hosted = false;
        st.error_count = 0;
        st.next_start = None;
        // 接管不经过 start()，需在此补算工作目录（文件变动监视/停止清理等依赖它；缺失会静默跳过）
        let wd = crate::deploy::work_dir(&self.base, &st.cfg);
        st.work_dir = wd;
        util::log_format(
            "接管已存在进程：应用[{}] PID={}",
            &[&self.name, &pid.to_string()],
        );
    }

    /// 启动应用。Ok(started/task) 表示启动结果；Ok(false) 表示已在运行；Err 表示失败（已记录退避）。
    pub fn start(&self, global: &AgentConfig) -> Result<StartResult, String> {
        let mut st = self.state.lock().unwrap();
        if st.running {
            return Ok(StartResult::default());
        }

        let cfg = st.cfg.clone();
        if cfg.name.trim().is_empty() {
            return Err("应用名称为空".to_string());
        }

        let retry = st.error_count > 0;
        let ctx = PrepareContext {
            base: &self.base,
            global,
            retry,
            shadow_override: None,
        };

        let prepared = match deploy::prepare(&ctx, &cfg) {
            Ok(p) => p,
            Err(e) => {
                self.record_failure(&mut st, global);
                return Err(e);
            }
        };

        st.work_dir = prepared.work_dir.clone();
        st.shadow = prepared.shadow.clone();

        util::log_format(
            "部署模式：{} RunFile={}",
            &[
                prepared.mode.as_str(),
                &prepared
                    .run_file
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default(),
            ],
        );

        // 托管模式：仅解压，外部宿主运行
        if prepared.hosted {
            st.running = true;
            st.hosted = true;
            st.pid = 0;
            st.start_time = Some(SystemTime::now());
            util::log_format("应用[{}]已托管，外部宿主运行", &[&self.name]);
            return Ok(StartResult {
                started: true,
                task: false,
            });
        }

        let log_file = if cfg.debug || (retry && global.debug) {
            Some(self.base.join("Log").join(format!("app-{}.log", cfg.name)))
        } else {
            None
        };

        let req = SpawnRequest {
            program: &prepared.program,
            args: &prepared.args,
            cwd: &prepared.work_dir,
            envs: &prepared.envs,
            log_file: log_file.as_deref(),
            detached: false,
        };

        let child = match sys::spawn(&req) {
            Ok(c) => c,
            Err(e) => {
                let msg = format!("启动进程失败 {}：{}", prepared.program, e);
                self.record_failure(&mut st, global);
                return Err(msg);
            }
        };

        let pid = child.id();
        let process_name = sys::process_name(pid).unwrap_or_else(|| prepared.program.clone());
        util::log_format(
            "启动成功：应用[{}] PID={} 进程={} 工作目录={}",
            &[
                &self.name,
                &pid.to_string(),
                &process_name,
                &prepared.work_dir.display().to_string(),
            ],
        );
        if let Some(shadow) = &prepared.shadow {
            util::log_format("影子目录：{}", &[&shadow.display().to_string()]);
        }

        st.handle = Some(Handle::Owned(child));
        st.pid = pid;
        st.process_name = process_name;
        st.start_time = Some(SystemTime::now());
        st.running = true;
        st.hosted = false;
        st.error_count = 0;
        st.fail_limit_logged = false;
        st.next_start = None;
        st.files.clear();
        st.files_primed = false;

        // OOM 分值（仅 Linux）
        if cfg.oom_score_adjust != 0 {
            sys::set_oom_score_adjust(pid, cfg.oom_score_adjust);
        }

        // 任务模式：运行一次，运行完成后不再守护（自动禁用由管理器持久化）
        let is_task = prepared.task;
        if is_task {
            util::log_format("任务模式，运行一次：应用[{}]", &[&self.name]);
            st.running = false;
            st.cfg.enable = false;
            return Ok(StartResult {
                started: true,
                task: true,
            });
        }

        // 健康检查（失败只记日志，与 C# 行为一致）
        if let Some(hc) = cfg.health_check.as_deref() {
            if !hc.trim().is_empty() {
                let wait = global.start_wait;
                drop(st);
                std::thread::sleep(Duration::from_millis(wait));

                if !sys::is_alive(pid) {
                    util::log_format("健康检查失败：进程已退出（应用[{}]）", &[&self.name]);
                    let mut st = self.state.lock().unwrap();
                    st.running = false;
                    if let Some(Handle::Owned(mut c)) = st.handle.take() {
                        let _ = c.try_wait();
                    }
                    self.record_failure(&mut st, global);
                    return Err("启动后进程退出".to_string());
                }

                util::log_format("执行健康检查 {}", &[hc]);
                match health_check(hc) {
                    Ok(()) => util::log_format("健康检查通过（应用[{}]）", &[&self.name]),
                    Err(e) => util::log_format("健康检查失败（仅记录）：{}", &[&e]),
                }
            }
        }

        Ok(StartResult {
            started: true,
            task: false,
        })
    }

    /// 记录一次失败并安排退避重试。
    fn record_failure(&self, st: &mut AppState, global: &AgentConfig) {
        st.running = false;
        st.error_count += 1;
        let delay = global.delay.max(1_000);
        st.next_start = Some(Instant::now() + Duration::from_millis(delay));
        if st.error_count >= global.max_fails && !st.fail_limit_logged {
            st.fail_limit_logged = true;
            util::log_format(
                "应用[{}]累计错误次数达到最大值 {}，不再自动尝试启动",
                &[&self.name, &global.max_fails.to_string()],
            );
        }
    }

    /// 停止应用（优雅 → 强制）。返回是否已退出。
    pub fn stop(&self, reason: &str) -> bool {
        let (pid, handle, work_dir, was_running, hosted) = {
            let mut st = self.state.lock().unwrap();
            let was_running = st.running;
            st.running = false;
            st.next_start = None;
            st.reload_ready = None;
            let handle = st.handle.take();
            let pid = st.pid;
            let work_dir = st.work_dir.clone();
            let hosted = st.hosted;
            st.hosted = false;
            (pid, handle, work_dir, was_running, hosted)
        };

        if !was_running && handle.is_none() {
            return true;
        }

        if hosted || (pid == 0 && handle.is_none()) {
            util::log_format("应用[{}]停止（托管模式无进程），原因：{}", &[&self.name, reason]);
            return true;
        }

        util::log_format(
            "停止应用[{}] PID={}，原因：{}",
            &[&self.name, &pid.to_string(), reason],
        );

        // 自有子进程用 Child::try_wait 回收（避免僵尸导致 is_alive 误判）；
        // 接管进程按 pid 停止
        let ok = match handle {
            Some(Handle::Owned(mut child)) => {
                sys::signal_graceful(pid);
                let mut exited = wait_child(&mut child, 3_000);
                if !exited {
                    sys::signal_force(pid);
                    exited = wait_child(&mut child, 2_000);
                }
                exited
            }
            _ => sys::stop_process(pid, 3_000),
        };

        {
            let mut st = self.state.lock().unwrap();
            st.handle = None;
            st.pid = 0;
        }

        // 停稳后清理占用期产生的临时文件（*.del 等）
        if !work_dir.as_os_str().is_empty() {
            deploy::cleanup_temp_files(&work_dir, false);
        }

        if ok {
            util::log_format("应用[{}]已退出", &[&self.name]);
        } else {
            util::log_format("应用[{}]停止超时，进程仍可能存在", &[&self.name]);
        }

        ok
    }

    /// 守护检查（由管理器周期调用）：退出检测、内存限制、文件变动重启。
    pub fn check(&self, global: &AgentConfig) -> CheckOutcome {
        let mut outcome = CheckOutcome::default();

        // 1) 进程退出检测
        let mut just_exited = false;
        {
            let mut st = self.state.lock().unwrap();
            if st.running {
                if let Some(handle) = st.handle.as_mut() {
                    if handle.has_exited() {
                        just_exited = true;
                        util::log_format(
                            "应用[{}]进程已退出（PID={}）",
                            &[&self.name, &st.pid.to_string()],
                        );
                        st.handle = None;
                        st.pid = 0;
                        st.running = false;
                        if st.cfg.enable {
                            self.record_failure(&mut st, global);
                        }
                    }
                }

                if st.running && st.error_count != 0 {
                    st.error_count = 0;
                    st.fail_limit_logged = false;
                }
            }
        }
        if just_exited {
            let work_dir = self.state.lock().unwrap().work_dir.clone();
            if !work_dir.as_os_str().is_empty() {
                deploy::cleanup_temp_files(&work_dir, false);
            }
        }

        // 2) 内存限制
        let over_memory = {
            let st = self.state.lock().unwrap();
            if st.running && st.pid > 0 && st.cfg.max_memory > 0 {
                sys::memory_mb(st.pid)
                    .map(|mb| mb > st.cfg.max_memory as u64)
                    .unwrap_or(false)
            } else {
                false
            }
        };
        if over_memory {
            let (pid, max) = {
                let st = self.state.lock().unwrap();
                (st.pid, st.cfg.max_memory)
            };
            let mb = sys::memory_mb(pid).unwrap_or_default();
            util::log_format(
                "应用[{}]内存超限 {}MB > {}MB，准备重启",
                &[&self.name, &mb.to_string(), &max.to_string()],
            );
            self.stop("内存超限");

            // stop 会清空重启计划，内存超限需重新安排（与文件变动不同，这里不等待稳定）
            let mut st = self.state.lock().unwrap();
            if st.cfg.enable {
                let delay = global.delay.max(1_000);
                st.next_start = Some(Instant::now() + Duration::from_millis(delay));
            }
        }

        // 3) 计划启动（失败退避 / 文件变动重启）
        let should_start = {
            let st = self.state.lock().unwrap();
            !st.running
                && st.cfg.enable
                && !st.hosted
                && st.next_start.map(|t| t <= Instant::now()).unwrap_or(false)
                && st.error_count < global.max_fails
        };
        if should_start {
            match self.start(global) {
                Ok(r) => outcome.changed |= r.task,
                Err(e) => {
                    util::log_error(&format!("应用[{}]启动失败：{}", self.name, e));
                }
            }
        }

        outcome
    }

    /// 文件变动监视（快速周期，默认 5 秒）：变更后停止应用，稳定 `Delay` 毫秒后重启。
    ///
    /// 注意：`stop()` 会清空 `reload_ready`/`next_start`，因此停止动作完成后必须重新写入，
    /// 否则会停留在“已停止且永不重启”的状态（冒烟测试曾实测到该问题）。
    pub fn monitor_reload(&self, global: &AgentConfig) {
        let reload_action = {
            let mut st = self.state.lock().unwrap();
            let mut action = 0u8; // 0 无，1 停止，2 重启
            if st.cfg.reload_on_change && !st.hosted {
                let work_dir = st.work_dir.clone();
                if !work_dir.as_os_str().is_empty() {
                    let changed = match watch_target(&work_dir, &st.cfg.file_name) {
                        Some(watch) => file_changed(&watch, &mut st.files),
                        None => false,
                    };
                    if !st.files_primed {
                        st.files_primed = true;
                    } else if changed {
                        let delay = global.delay.max(500);
                        if st.running && st.reload_ready.is_none() {
                            action = 1;
                        }
                        st.reload_ready = Some(Instant::now());
                        st.next_start = Some(Instant::now() + Duration::from_millis(delay));
                    } else if let Some(t) = st.reload_ready {
                        let delay = global.delay.max(500);
                        if t.elapsed() >= Duration::from_millis(delay) {
                            st.reload_ready = None;
                            st.next_start = None;
                            if st.cfg.enable {
                                action = 2;
                            }
                        }
                    }
                }
            }
            action
        };

        match reload_action {
            1 => {
                util::log_format("应用[{}]文件发生改变，停止后等待稳定再重启", &[&self.name]);
                self.stop("文件变动");

                // 停止动作已清空计划，重新写入就绪状态
                let mut st = self.state.lock().unwrap();
                if st.cfg.enable {
                    let delay = global.delay.max(500);
                    st.reload_ready = Some(Instant::now());
                    st.next_start = Some(Instant::now() + Duration::from_millis(delay));
                }
            }
            2 => {
                util::log_format("应用[{}]文件已稳定，重新启动", &[&self.name]);
                if let Err(e) = self.start(global) {
                    util::log_error(&format!("应用[{}]重启失败：{}", self.name, e));
                }
            }
            _ => {}
        }
    }
}

/// 等待子进程退出（轮询 `try_wait` 并回收）。
fn wait_child(child: &mut std::process::Child, timeout_ms: u64) -> bool {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return true,
            Ok(None) => {}
            Err(_) => return true,
        }

        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// 监视目标：配置的程序文件（`FileName`，相对工作目录或绝对路径）。
///
/// 仅监视“程序文件本身”而非整个工作目录——避免子服务写入自身数据
/// （日志/数据库/上传的部署包等）时被误判为程序更新而反复重启；
/// 同时支持无扩展名的 Linux 可执行文件（旧逻辑只认 dll/exe/zip/jar）。
fn watch_target(work_dir: &Path, file_name: &str) -> Option<PathBuf> {
    let name = file_name.trim();
    if name.is_empty() {
        return None;
    }
    let p = Path::new(name);
    let p = if p.is_absolute() {
        p.to_path_buf()
    } else {
        work_dir.join(p)
    };
    if p.is_file() { Some(p) } else { None }
}

/// 单文件“最后修改秒”快照比较（快照表由 `files` 复用，仅一个条目）。
fn file_changed(path: &Path, files: &mut HashMap<PathBuf, u64>) -> bool {
    let secs = std::fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    match files.insert(path.to_path_buf(), secs) {
        Some(prev) => prev != secs,
        None => false, // 首次见到的文件仅记录快照，不视为变化
    }
}

/// 健康检查：`http://` 或 `tcp://host:port` / `host:port`。
fn health_check(spec: &str) -> Result<(), String> {
    let spec = spec.trim();

    if spec.starts_with("http://") || spec.starts_with("https://") {
        // 支持 https（dhrust::net::http_client 含 TLS）；非 2xx 亦视为"有响应=存活"（与旧行为一致）
        return dhrust::net::http_client::blocking_get_text(spec, Duration::from_millis(5_000))
            .map(|_| ())
            .map_err(|e| e.to_string());
    }

    let host_port = spec.strip_prefix("tcp://").unwrap_or(spec);
    if host_port.contains(':') {
        let (host, port) = dhrust::net::split_host_port(host_port, 0);
        if port == 0 {
            return Err(format!("健康检查地址缺少端口：{}", spec));
        }
        return dhrust::net::tcp_check(&host, port, Duration::from_millis(5_000));
    }

    Err(format!("不支持的健康检查地址：{}", spec))
}
