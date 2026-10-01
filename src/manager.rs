//! 应用管理器：多应用统一拉起、守护、控制与状态持久化。
//!
//! - 守护周期：`GuardPeriod`（默认 30 秒），检查退出/内存/文件变动并按需拉起；
//! - 状态持久化：`data/state.json` 记录运行中 PID，代理重启后“接管”避免重复拉起；
//! - 看门狗：应用通过 `/Ping` 喂狗，超时未喂则重启对应应用；
//! - 配置热更新：`Config/Agent.toml` 被外部修改后自动重新加载并应用。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};

use crate::app::{AppRuntime, AppStatus};
use crate::config::AgentConfig;
use crate::util;

/// 应用管理器。
pub struct AppManager {
    base: PathBuf,
    inner: Mutex<Inner>,
}

struct Inner {
    config: AgentConfig,
    apps: Vec<Arc<AppRuntime>>,
    /// 看门狗：PID → 截止时间
    dogs: HashMap<u32, Instant>,
    shutting_down: bool,
    config_stamp: Option<SystemTime>,
}

/// 保存的进程状态（代理重启后接管）。
#[derive(Serialize, Deserialize, Default, Clone)]
#[serde(rename_all = "PascalCase", default)]
struct SavedApp {
    pid: u32,
    process_name: String,
}

#[derive(Serialize, Deserialize, Default)]
#[serde(rename_all = "PascalCase", default)]
struct SavedState {
    apps: HashMap<String, SavedApp>,
}

impl AppManager {
    /// 实例化并恢复上次运行状态（接管仍存活的进程）。
    pub fn new(base: &Path, config: AgentConfig) -> Arc<AppManager> {
        let mut apps = Vec::new();
        for cfg in &config.apps {
            if !cfg.name.trim().is_empty() {
                apps.push(Arc::new(AppRuntime::new(base, cfg.clone())));
            }
        }

        let stamp = std::fs::metadata(crate::config::config_path(base))
            .and_then(|m| m.modified())
            .ok();

        let manager = Arc::new(AppManager {
            base: base.to_path_buf(),
            inner: Mutex::new(Inner {
                config,
                apps,
                dogs: HashMap::new(),
                shutting_down: false,
                config_stamp: stamp,
            }),
        });

        manager.restore_state();
        manager
    }

    /// 配置克隆。
    pub fn config(&self) -> AgentConfig {
        self.inner.lock().unwrap().config.clone()
    }

    /// 基础目录。
    pub fn base(&self) -> &Path {
        &self.base
    }

    /// 是否正在关闭。
    pub fn shutting_down(&self) -> bool {
        self.inner.lock().unwrap().shutting_down
    }

    /// 保存配置到磁盘。
    pub fn save_config(&self) {
        let cfg = self.config();
        if let Err(e) = cfg.save(&self.base) {
            util::log_error(&format!("保存配置失败：{}", e));
        }
        // 保存后同步“文件指纹”：避免守护周期把自身写入误判为外部修改而整份重载
        // （否则内存与磁盘短暂不一致时会出现“刚改的配置被回滚”现象）
        let path = crate::config::config_path(&self.base);
        let stamp = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        self.inner.lock().unwrap().config_stamp = stamp;
    }

    /// 恢复状态文件并接管存活进程。
    fn restore_state(&self) {
        // 状态目录以 `Data` 为准（与 Config/Log 命名一致）；兼容旧版小写 `data`
        let mut path = self.base.join("Data").join("state.json");
        if !path.exists() {
            let legacy = self.base.join("data").join("state.json");
            if legacy.exists() {
                path = legacy;
            }
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            return;
        };
        let Ok(state) = serde_json::from_str::<SavedState>(&text) else {
            util::log_error("状态恢复：state.json 解析失败，跳过接管");
            return;
        };

        let apps: Vec<Arc<AppRuntime>> = self.inner.lock().unwrap().apps.clone();
        for (name, saved) in state.apps {
            if saved.pid == 0 {
                continue;
            }
            if !crate::sys::is_alive(saved.pid) {
                util::log_format(
                    "状态恢复：应用[{}] PID={} 已不存在，跳过接管",
                    &[&name, &saved.pid.to_string()],
                );
                continue;
            }
            if let Some(rt) = apps
                .iter()
                .find(|e| e.name.eq_ignore_ascii_case(&name))
            {
                rt.adopt(saved.pid, saved.process_name);
            } else {
                util::log_format("状态恢复：应用[{}] 不在当前配置中，跳过接管", &[&name]);
            }
        }
    }

    /// 持久化运行状态。
    pub fn persist_state(&self) {
        let apps = self.inner.lock().unwrap().apps.clone();
        let mut state = SavedState::default();
        for rt in apps {
            let status = rt.status();
            if status.running && status.pid > 0 {
                state.apps.insert(
                    rt.name.clone(),
                    SavedApp {
                        pid: status.pid,
                        process_name: status.process_name,
                    },
                );
            }
        }

        // 统一写入新目录（自旧目录读取的状态随首次持久化自动迁移到 `Data`）
        let dir = self.base.join("Data");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("state.json");
        let text = serde_json::to_string_pretty(&state).unwrap_or_else(|_| "{}".to_string());
        // 内容未变化时不写盘：守护周期（默认 30 秒）高频触发，避免无谓的磁盘写入与原子替换
        if std::fs::read_to_string(&path)
            .map(|old| old == text)
            .unwrap_or(false)
        {
            return;
        }
        let _ = dhrust::io::write_all_text_atomic(&path, &text);
    }

    /// 启动全部启用应用（代理启动时调用）。
    pub fn start_all(&self) {
        let cfg = self.config();
        let apps = self.inner.lock().unwrap().apps.clone();
        let mut task_changed = false;
        for rt in apps {
            let app_cfg = cfg.find_app(&rt.name).cloned();
            if let Some(app_cfg) = app_cfg {
                rt.set_cfg(app_cfg);
                if rt.cfg().enable {
                    match rt.start(&cfg) {
                        Ok(r) => {
                            if r.task {
                                self.set_app_enable_quiet(&rt.name, false);
                                task_changed = true;
                            }
                        }
                        Err(e) => {
                            util::log_error(&format!("启动应用[{}]失败：{}", rt.name, e));
                        }
                    }
                }
            }
        }

        if task_changed {
            self.save_config();
        }
        self.persist_state();
    }

    /// 守护周期检查（代理运行期间周期调用）。
    pub fn check_all(&self) {
        if self.shutting_down() {
            return;
        }

        self.reload_config_if_changed();

        // 看门狗超时
        let expired: Vec<u32> = {
            let mut inner = self.inner.lock().unwrap();
            let now = Instant::now();
            let mut expired = Vec::new();
            inner.dogs.retain(|pid, deadline| {
                if !crate::sys::is_alive(*pid) {
                    return false;
                }
                if *deadline <= now {
                    expired.push(*pid);
                    return false;
                }
                true
            });
            expired
        };
        for pid in expired {
            if let Some(rt) = self.find_by_pid(pid) {
                util::log_format(
                    "应用[{}]看门狗超时（PID={}），准备重启",
                    &[&rt.name, &pid.to_string()],
                );
                let _ = self.restart_app(&rt.name, "看门狗超时");
            }
        }

        // 应用检查
        let cfg = self.config();
        let apps = self.inner.lock().unwrap().apps.clone();
        let mut task_changed = false;
        for rt in apps {
            let outcome = rt.check(&cfg);
            if outcome.changed {
                task_changed = true;
                self.set_app_enable_quiet(&rt.name, false);
            }
        }
        if task_changed {
            self.save_config();
        }

        self.persist_state();
    }

    /// 按名称查找运行时。
    pub fn find_runtime(&self, name: &str) -> Option<Arc<AppRuntime>> {
        self.inner
            .lock()
            .unwrap()
            .apps
            .iter()
            .find(|e| e.name.eq_ignore_ascii_case(name.trim()))
            .cloned()
    }

    /// 按进程号查找运行时。
    pub fn find_by_pid(&self, pid: u32) -> Option<Arc<AppRuntime>> {
        if pid == 0 {
            return None;
        }

        let apps = self.inner.lock().unwrap().apps.clone();
        apps.into_iter().find(|e| e.status().pid == pid)
    }

    /// 启动应用（同时启用配置）。
    pub fn start_app(&self, name: &str) -> Result<bool, String> {
        let rt = self
            .find_runtime(name)
            .ok_or_else(|| format!("服务不存在：{}", name))?;

        self.set_app_enable(name, true);
        self.save_config();

        let cfg = self.config();
        rt.set_cfg(cfg.find_app(name).cloned().unwrap_or(rt.cfg()));

        let rs = rt.start(&cfg)?;
        if rs.task {
            self.set_app_enable_quiet(name, false);
            self.save_config();
        }
        self.persist_state();

        Ok(rs.started)
    }

    /// 停止应用（同时禁用配置，防止守护自动拉起）。
    pub fn stop_app(&self, name: &str, reason: &str) -> Result<bool, String> {
        let rt = self
            .find_runtime(name)
            .ok_or_else(|| format!("服务不存在：{}", name))?;

        self.set_app_enable(name, false);
        self.save_config();

        rt.set_cfg(self.config().find_app(name).cloned().unwrap_or(rt.cfg()));
        let ok = rt.stop(reason);
        self.persist_state();

        Ok(ok)
    }

    /// 重启应用（保持启用状态；停止失败则中止，避免重复拉起）。
    pub fn restart_app(&self, name: &str, reason: &str) -> Result<bool, String> {
        let rt = self
            .find_runtime(name)
            .ok_or_else(|| format!("服务不存在：{}", name))?;

        // 先禁用（确保停止失败时守护不会再次拉起），停止成功后再启用
        self.set_app_enable(name, false);
        self.save_config();
        rt.set_cfg(self.config().find_app(name).cloned().unwrap_or(rt.cfg()));

        let stopped = rt.stop(reason);
        if !stopped {
            return Err("停止服务失败（进程可能未退出），已中止重启".to_string());
        }
        std::thread::sleep(Duration::from_millis(500));

        self.set_app_enable(name, true);
        self.save_config();
        rt.set_cfg(self.config().find_app(name).cloned().unwrap_or(rt.cfg()));

        let cfg = self.config();
        let rs = rt.start(&cfg)?;
        if rs.task {
            self.set_app_enable_quiet(name, false);
            self.save_config();
        }
        self.persist_state();

        Ok(rs.started)
    }

    /// 设置应用启用状态（含保存）。
    pub fn set_app_enable(&self, name: &str, enable: bool) {
        {
            let mut inner = self.inner.lock().unwrap();
            let mut cfg = inner.config.clone();
            if cfg.set_app_enable(name, enable) {
                inner.config = cfg;
            }
        }
        self.sync_runtimes();
    }

    /// 新增或更新应用配置（Web 面板调用）。返回是否命中的是已有应用。
    pub fn upsert_app(&self, app: crate::config::AppConfig) -> bool {
        let name = app.name.trim().to_string();
        if name.is_empty() {
            return false;
        }

        let mut updated = false;
        {
            let mut inner = self.inner.lock().unwrap();
            let mut cfg = inner.config.clone();
            match cfg.find_app_mut(&name) {
                Some(old) => {
                    *old = app;
                    updated = true;
                }
                None => {
                    let mut app = app;
                    app.name = name.clone();
                    cfg.apps.push(app);
                }
            }
            cfg.normalize();
            inner.config = cfg;
        }

        self.sync_runtimes();
        self.save_config();
        util::log_format(
            "Web 面板{}应用配置[{}]",
            &[if updated { "更新" } else { "新增" }, &name],
        );
        updated
    }

    /// 删除应用配置（Web 面板调用；运行中的实例由 sync_runtimes 停止并移除）。返回是否找到。
    pub fn remove_app(&self, name: &str) -> bool {
        let found = {
            let mut inner = self.inner.lock().unwrap();
            let mut cfg = inner.config.clone();
            let before = cfg.apps.len();
            cfg.apps.retain(|e| !e.name.eq_ignore_ascii_case(name.trim()));
            let found = cfg.apps.len() != before;
            if found {
                inner.config = cfg;
            }
            found
        };

        if found {
            self.sync_runtimes();
            self.save_config();
            self.persist_state();
            util::log_format("Web 面板删除应用配置[{}]", &[name]);
        }
        found
    }

    /// 更新全局配置（Web 面板调用）；闭包修改后立即落盘。
    pub fn update_config(&self, f: impl FnOnce(&mut AgentConfig)) {
        {
            let mut inner = self.inner.lock().unwrap();
            let mut cfg = inner.config.clone();
            f(&mut cfg);
            cfg.normalize();
            inner.config = cfg;
        }
        self.save_config();
    }

    /// 仅更新内存配置（不落盘，批量变更时用）。
    fn set_app_enable_quiet(&self, name: &str, enable: bool) {
        let mut inner = self.inner.lock().unwrap();
        let mut cfg = inner.config.clone();
        if cfg.set_app_enable(name, enable) {
            inner.config = cfg;
        }
    }

    /// 同步运行时的配置（增删改）。
    fn sync_runtimes(&self) {
        let (config, mut apps) = {
            let inner = self.inner.lock().unwrap();
            (inner.config.clone(), inner.apps.clone())
        };

        // 删除已移除的应用
        apps.retain(|rt| {
            let keep = config.find_app(&rt.name).is_some();
            if !keep {
                rt.stop("配置移除");
                util::log_format("应用[{}]已从配置移除", &[&rt.name]);
            }
            keep
        });

        // 更新或新增
        for app_cfg in &config.apps {
            if app_cfg.name.trim().is_empty() {
                continue;
            }

            match apps
                .iter()
                .find(|e| e.name.eq_ignore_ascii_case(&app_cfg.name))
            {
                Some(rt) => rt.set_cfg(app_cfg.clone()),
                None => {
                    util::log_format("新增应用配置[{}]", &[&app_cfg.name]);
                    apps.push(Arc::new(AppRuntime::new(&self.base, app_cfg.clone())));
                }
            }
        }

        self.inner.lock().unwrap().apps = apps;
    }

    /// 配置热更新：文件被外部修改后重新加载。
    fn reload_config_if_changed(&self) {
        let path = crate::config::config_path(&self.base);
        let Ok(stamp) = std::fs::metadata(&path).and_then(|m| m.modified()) else {
            return;
        };

        {
            let inner = self.inner.lock().unwrap();
            if inner.config_stamp == Some(stamp) {
                return;
            }
        }

        util::log_info("检测到配置文件变化，重新加载");
        self.reload_inner();
    }

    /// 强制重新加载配置（本机控制接口 `/ReloadConfig` 调用）。
    /// 供安装脚本/本机工具注册新服务后立即生效；新增应用仅建运行时，不自动启动。
    pub fn reload_config(&self) {
        util::log_info("收到重载指令，重新加载配置");
        self.reload_inner();
    }

    /// 重载实现：读盘 → 规范化 → 替换内存配置 → 同步运行时。
    fn reload_inner(&self) {
        let path = crate::config::config_path(&self.base);
        let stamp = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        let mut cfg = AgentConfig::load(&self.base);
        cfg.normalize();

        {
            let mut inner = self.inner.lock().unwrap();
            inner.config = cfg;
            inner.config_stamp = stamp;
        }

        self.sync_runtimes();
    }

    /// 文件变动监视（快速周期，默认 5 秒）。
    pub fn monitor_files(&self) {
        if self.shutting_down() {
            return;
        }

        let cfg = self.config();
        let apps = self.inner.lock().unwrap().apps.clone();
        for rt in apps {
            rt.monitor_reload(&cfg);
        }
    }

    /// 喂狗。应用心跳调用。
    pub fn feed_dog(&self, pid: u32, timeout_secs: u32) {
        if pid == 0 || timeout_secs == 0 {
            return;
        }

        let mut inner = self.inner.lock().unwrap();
        inner
            .dogs
            .insert(pid, Instant::now() + Duration::from_secs(timeout_secs as u64));
    }

    /// 列表：配置 + 运行状态。
    pub fn list(&self) -> Vec<(crate::config::AppConfig, AppStatus)> {
        let inner = self.inner.lock().unwrap();
        let mut out = Vec::new();
        for app_cfg in &inner.config.apps {
            let status = inner
                .apps
                .iter()
                .find(|e| e.name.eq_ignore_ascii_case(&app_cfg.name))
                .map(|e| e.status())
                .unwrap_or(AppStatus {
                    running: false,
                    pid: 0,
                    process_name: String::new(),
                    start_time: String::new(),
                });
            out.push((app_cfg.clone(), status));
        }
        out
    }

    /// 关闭：停止设置 AutoStop 的应用并保存状态。
    pub fn shutdown(&self, reason: &str) {
        {
            let mut inner = self.inner.lock().unwrap();
            if inner.shutting_down {
                return;
            }
            inner.shutting_down = true;
        }

        let apps = self.inner.lock().unwrap().apps.clone();
        for rt in apps {
            let cfg = rt.cfg();
            if cfg.enable && cfg.auto_stop {
                rt.stop(reason);
            }
        }

        self.persist_state();
        util::log_format("应用管理器已停止：{}", &[reason]);
    }
}
