//! 配置：`Config/StarAgent.config`（XML，与 C# StarAgent **同名同格式，双端完全互通**，带中文注释）。
//!
//! - 字段名与 C# `StarAgentSetting`/`ServiceInfo` 对齐（PascalCase）；C# 特有字段
//!   （Code/Secret/Channel/SyncTime/UseAutorun 等）读写时原样保留不丢失；
//! - 注释由内置模板（`res/StarAgent.config.template`）保障：首次生成即带完整字段说明，
//!   程序修改配置值（含 Web 面板）时按元素就地更新、注释与排版保留（dhrust::config XML 管线）；
//! - 兼容迁移：旧 `Config/Agent.toml`（TOML 版）与 `Config/Agent.json` 自动转换（原文件改名 `.bak`）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use dhrust::config::toml;
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

use crate::util;

/// 内置配置模板（C# StarAgent.config 风格、带中文注释；值须与 `Default::default()` 一致）。
const TEMPLATE: &str = include_str!("../res/StarAgent.config.template");

/// 默认服务名（Windows 服务 / systemd 单元 / launchd 任务）。
///
/// 用 `StarAgentRust` 与 C# 版 StarAgent（服务名 `StarAgent`、端口 5500）错开：
/// 服务名与端口双重区分后，两版星尘可在**同一台机器同时安装、并行运行**。
pub const DEFAULT_SERVICE_NAME: &str = "StarAgentRust";
/// 旧默认服务名（与 C# 版同名）。安装新名服务前会清理**指向本程序**的旧注册，
/// 避免两套服务指向同一 exe 造成重复拉起（见 `service` 模块 `cleanup_legacy`）。
pub const LEGACY_SERVICE_NAME: &str = "StarAgent";
/// 本地控制端口默认值。5501：与 C# 版 StarAgent（5500）错开，二者可同时并存；
/// DHDeploy.Agent.Rust 按节点类型调用（Rust 类型节点 → 5501；非 Rust/空 → 5500）。
pub const DEFAULT_LOCAL_PORT: u16 = 5501;

/// 代理配置。
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "PascalCase", default)]
pub struct AgentConfig {
    /// 服务名。默认 StarAgentRust（与 C# 版 StarAgent 错开，可同机并存）
    pub service_name: String,
    /// 显示名
    pub display_name: String,
    /// 服务描述
    pub description: String,
    /// 本地控制端口。默认 5501（与 C# 版 StarAgent 的 5500 错开，二者可同时并存）
    pub local_port: u16,
    /// 仅本机访问。true 时只绑定 127.0.0.1；false 绑定 0.0.0.0（允许远程，面板鉴权兜底）。
    /// 默认 false——与 C# 行为一致，无头服务器需远程访问管理面板
    pub local_only: bool,
    /// 重启进程或服务的延迟时间（毫秒），默认 3000
    pub delay: u64,
    /// 启动等待时间（毫秒）。该时间内进程退出视为启动失败，默认 3000
    pub start_wait: u64,
    /// 最大失败次数。超过后不再尝试启动，默认 20
    pub max_fails: i32,
    /// 守护检查周期（毫秒），默认 30000
    pub guard_period: u64,
    /// 调试开关。开启后应用输出重定向到 Log 目录
    pub debug: bool,
    /// 星尘服务端地址。暂未对接，保留字段以便脚本兼容（`-server` 参数可写入）
    pub server: String,
    /// 项目名。暂未对接，保留字段（`-project` 参数可写入）
    pub project: String,
    /// 启动挂钩。对 .NET 应用注入 Stardust.dll（未引用星尘 SDK 时）
    pub startup_hook: bool,
    /// 看门狗。保护其它服务，每分钟检查一次；多个进程名逗号分隔
    pub watch_dog: String,
    /// Web 面板用户名。默认 admin
    pub web_user_name: String,
    /// Web 面板密码。默认 admin
    pub web_user_password: String,
    /// Web 面板鉴权级别。None 不鉴权；LocalOnly 本地免鉴权、远程需鉴权（默认）；Full 全部鉴权
    pub web_auth_level: String,
    /// 后台资源采样间隔（毫秒）。默认 1000；0 = 关闭后台采样（回退为面板请求时现采）
    pub sample_interval: u64,
    /// 网站流量统计。解析 nginx/apache 访问日志（自动发现常见站点 + `web_logs` 手动补充），默认开启
    pub web_traffic: bool,
    /// 网站日志手动配置。`名称=路径;名称2=路径2`；自动发现不到时的补充（如 Caddy 自定义日志）
    pub web_logs: String,
    /// 端口流量统计。Linux 创建独立 nftables 计数表（只计数不改转发，关闭/卸载自动清理；
    /// 无 nft 或权限不足时自动降级为连接视图），默认开启
    pub port_traffic: bool,
    /// 端口流量统计端口列表。形如 `22,80,443,3306`；留空 = 自动取系统监听端口
    pub port_traffic_ports: String,
    /// 流量历史保留天数（SQLite 每日归档 `Data/traffic.db`，模型见 `Entity/Model.xml`）。默认 90；0 = 永久保留
    pub traffic_history_days: u32,
    /// 日志清理自定义路径（分号分隔；目录=清空内容、文件=截断清空）。
    /// 供 Web 面板「日志清理」页在平台内置分类之外额外扫描/清理
    pub log_cleanup_paths: String,
    /// 在线插件源地址（`catalog.json` URL；仅 https，127.0.0.1 例外）。空 = 关闭在线插件
    pub plugin_store_url: String,
    /// 在线插件源 Ed25519 公钥（hex；32 字节裸公钥或 44 字节 SPKI DER）。非空时强制校验 `catalog.json.sig`
    pub plugin_store_pubkey: String,
    /// AI 助手。启用后可在面板「AI 助手」页对话分析服务器问题（OpenAI 兼容接口）
    pub ai_enabled: bool,
    /// AI 接口地址。OpenAI 兼容 Base（如 `https://api.deepseek.com/v1`；也可直接填完整 `.../chat/completions`）
    pub ai_base_url: String,
    /// AI 模型名（如 `deepseek-chat` / `deepseek-reasoner`）
    pub ai_model: String,
    /// AI API Key（Bearer 令牌）
    pub ai_api_key: String,
    /// 在线终端。启用后可在面板「在线终端」页执行服务器命令（免 SSH；命令以本程序/服务账户权限运行，全部记入审计）
    pub terminal_enabled: bool,
    /// 应用服务集合
    pub apps: Vec<AppConfig>,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            service_name: DEFAULT_SERVICE_NAME.to_string(),
            display_name: "星尘代理(Rust)".to_string(),
            description: "星尘节点守护代理（Pek.RAgent）。提供进程守护、影子目录部署与本地控制接口。".to_string(),
            local_port: DEFAULT_LOCAL_PORT,
            // 默认允许远程访问（服务器部署多为无头环境；面板有密码鉴权）。
            // 安全提示：仍使用默认密码时，启动日志会输出提醒
            local_only: false,
            delay: 3000,
            start_wait: 3000,
            max_fails: 20,
            guard_period: 30_000,
            debug: false,
            server: String::new(),
            project: String::new(),
            startup_hook: false,
            watch_dog: String::new(),
            web_user_name: "admin".to_string(),
            web_user_password: "admin".to_string(),
            web_auth_level: "LocalOnly".to_string(),
            sample_interval: 1000,
            // 网站流量：只读日志文件、无系统副作用，默认开启（面板直接可见）
            web_traffic: true,
            web_logs: String::new(),
            // 端口流量：默认开启——Linux 创建独立 nftables 计数表（只计数、不改转发、
            // 关闭/卸载自动清理；无 nft/权限不足自动降级连接视图，Windows 为连接视图）
            port_traffic: true,
            port_traffic_ports: String::new(),
            // 流量历史：每日归档保留 90 天（0 = 永久）
            traffic_history_days: 90,
            log_cleanup_paths: String::new(),
            plugin_store_url: String::new(),
            plugin_store_pubkey: String::new(),
            ai_enabled: true,
            ai_base_url: "https://api.deepseek.com/v1".to_string(),
            ai_model: "deepseek-chat".to_string(),
            ai_api_key: String::new(),
            terminal_enabled: true,
            apps: sample_apps(),
        }
    }
}

/// 应用服务配置。字段与 C# `Stardust.Models.ServiceInfo` 对齐。
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "PascalCase", default)]
pub struct AppConfig {
    /// 名称。全局唯一
    pub name: String,
    /// 文件名。可执行文件、zip 包或系统命令；为空时默认 `{Name}.zip`
    pub file_name: String,
    /// 启动参数。原样透传（按空白与双引号切分）
    pub arguments: Option<String>,
    /// 工作目录。相对路径按程序目录解析；为空时默认 `../apps/{Name}`
    pub working_directory: Option<String>,
    /// 运行用户（仅 Linux 有效，尽力而为）
    pub user_name: Option<String>,
    /// 启用。停止服务时会置为 false，启动服务时置为 true
    pub enable: bool,
    /// 部署模式：shadow（默认，影子目录）/ standard / hosted / task
    pub mode: String,
    /// 允许多实例。健康检查时不再按进程名匹配
    pub allow_multiple: bool,
    /// 环境变量。形如 `A=1;B=2`
    pub environments: Option<String>,
    /// 自动停止。随宿主退出时同时停止应用进程
    pub auto_stop: bool,
    /// 检测文件变动自动重启（轮询 *.dll;*.exe;*.zip;*.jar）
    pub reload_on_change: bool,
    /// 最大内存（MB）。超过上限自动重启，0 不限制
    pub max_memory: u32,
    /// OOM 分值（仅 Linux）。-1000 禁止被杀，0 普通
    pub oom_score_adjust: i32,
    /// 健康检查。http/tcp 地址，如 `http://localhost:6600/health`
    pub health_check: Option<String>,
    /// 覆盖文件。部署包内需拷贝覆盖到工作目录的文件或子目录，`;` 分隔，支持 `*` 模糊匹配
    pub overwrite: Option<String>,
    /// 调试输出。输出重定向到 `Log/app-{Name}.log`
    pub debug: bool,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            file_name: String::new(),
            arguments: None,
            working_directory: None,
            user_name: None,
            enable: false,
            mode: "shadow".to_string(),
            allow_multiple: false,
            environments: None,
            auto_stop: false,
            reload_on_change: true,
            max_memory: 0,
            oom_score_adjust: 0,
            health_check: None,
            overwrite: None,
            debug: false,
        }
    }
}

impl AppConfig {
    /// 部署模式原始值。空串按 shadow 处理。
    pub fn mode_text(&self) -> String {
        let m = self.mode.trim();
        if m.is_empty() {
            "shadow".to_string()
        } else {
            m.to_string()
        }
    }
}

/// 内置示例应用（首次生成配置时写入，默认禁用）。
fn sample_apps() -> Vec<AppConfig> {
    vec![
        AppConfig {
            name: "test".to_string(),
            file_name: "ping".to_string(),
            arguments: Some("newlifex.com".to_string()),
            ..Default::default()
        },
        AppConfig {
            name: "webapp".to_string(),
            file_name: "webapp.zip".to_string(),
            arguments: Some("urls=http://*:8080".to_string()),
            working_directory: Some("../apps/webapp".to_string()),
            mode: "shadow".to_string(),
            ..Default::default()
        },
    ]
}

/// 配置文件路径（与 C# StarAgent 同名同格式，双端可共用同一份文件）。
pub fn config_path(base: &Path) -> PathBuf {
    base.join("Config").join("StarAgent.config")
}

/// 旧版 TOML 配置路径（存在时自动迁移为 XML）。
pub fn legacy_toml_path(base: &Path) -> PathBuf {
    base.join("Config").join("Agent.toml")
}

/// 旧版 JSON 配置路径（存在时自动迁移为 XML）。
pub fn legacy_json_path(base: &Path) -> PathBuf {
    base.join("Config").join("Agent.json")
}

impl AgentConfig {
    /// 加载配置。
    ///
    /// - `StarAgent.config`（XML，与 C# 同格式）存在：直接读取（损坏时备份 `.bad` 并用默认配置重建）；
    /// - 否则 `Agent.toml`（旧版）存在：自动迁移为 XML（原文件改名 `.toml.bak`）；
    /// - 否则 `Agent.json`（旧版）存在：自动迁移为 XML（原文件改名 `.json.bak`）；
    /// - 都没有：从内置模板生成带注释的默认配置。
    pub fn load(base: &Path) -> AgentConfig {
        let path = config_path(base);

        let (mut cfg, need_save) = if path.exists() {
            match read_xml_config(&path) {
                Ok(cfg) => (cfg, false),
                Err(e) => {
                    let bad = PathBuf::from(format!("{}.bad", path.display()));
                    let _ = std::fs::rename(&path, &bad);
                    util::log_error(&format!(
                        "配置文件解析失败，已备份到 {}：{}",
                        bad.display(),
                        e
                    ));
                    (AgentConfig::default(), true)
                }
            }
        } else if legacy_toml_path(base).exists() {
            match migrate_from_toml(base) {
                Ok(cfg) => (cfg, true),
                Err(e) => {
                    util::log_error(&format!("迁移旧版 Agent.toml 失败：{}", e));
                    (AgentConfig::default(), true)
                }
            }
        } else if legacy_json_path(base).exists() {
            match migrate_from_json(base) {
                Ok(cfg) => (cfg, true),
                Err(e) => {
                    util::log_error(&format!("迁移旧版 Agent.json 失败：{}", e));
                    (AgentConfig::default(), true)
                }
            }
        } else {
            (AgentConfig::default(), true)
        };

        cfg.normalize();

        if need_save {
            if let Err(e) = cfg.save(base) {
                util::log_error(&format!("生成默认配置失败：{}", e));
            } else {
                util::log_info(&format!("已生成默认配置 {}", path.display()));
            }
        }
        cfg
    }

    /// 保存配置（按元素就地更新：保留现有文件中的注释、排版与 C# 特有字段；
    /// 缺失字段按模板格式插入；`Services` 节整段重建）。
    pub fn save(&self, base: &Path) -> std::io::Result<()> {
        let path = config_path(base);
        let current = std::fs::read_to_string(&path).ok();
        let text = render_xml(self, current.as_deref())
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        dhrust::io::write_all_text_atomic(&path, &text)
    }

    /// 归一化：补默认值，修正应用的缺省文件名与工作目录（与 C# `ServiceManager.Fix` 一致）。
    pub fn normalize(&mut self) {
        if self.service_name.trim().is_empty() {
            self.service_name = DEFAULT_SERVICE_NAME.to_string();
        }
        self.service_name = self.service_name.trim().to_string();
        if self.display_name.trim().is_empty() {
            self.display_name = "星尘代理(Rust)".to_string();
        }
        if self.local_port == 0 {
            self.local_port = DEFAULT_LOCAL_PORT;
        }
        if self.delay == 0 {
            self.delay = 3000;
        }
        if self.start_wait == 0 {
            self.start_wait = 3000;
        }
        if self.max_fails <= 0 {
            self.max_fails = 20;
        }
        if self.guard_period < 5_000 {
            self.guard_period = 30_000;
        }
        if self.web_user_name.trim().is_empty() {
            self.web_user_name = "admin".to_string();
        }
        if self.web_user_password.is_empty() {
            self.web_user_password = "admin".to_string();
        }
        if self.web_auth_level.trim().is_empty() {
            self.web_auth_level = "LocalOnly".to_string();
        }
        // 采样间隔：0 = 关闭后台采样；非 0 时限定 200ms~60s（防误配打爆 CPU 或采样过粗）
        if self.sample_interval != 0 {
            self.sample_interval = self.sample_interval.clamp(200, 60_000);
        }
        self.web_logs = self.web_logs.trim().to_string();
        self.port_traffic_ports = self.port_traffic_ports.trim().to_string();
        self.log_cleanup_paths = self.log_cleanup_paths.trim().to_string();
        self.plugin_store_url = self.plugin_store_url.trim().to_string();
        self.plugin_store_pubkey = self.plugin_store_pubkey.trim().to_string();
        self.ai_base_url = self.ai_base_url.trim().to_string();
        self.ai_model = self.ai_model.trim().to_string();
        self.ai_api_key = self.ai_api_key.trim().to_string();
        // AI 默认值：地址/模型为空时落回默认（误清空配置时保持可用）
        if self.ai_base_url.is_empty() {
            self.ai_base_url = "https://api.deepseek.com/v1".to_string();
        }
        if self.ai_model.is_empty() {
            self.ai_model = "deepseek-chat".to_string();
        }
        // 流量历史保留天数：0 = 永久；非 0 时限定 7~3650 天（防误配清空全部历史）
        if self.traffic_history_days != 0 {
            self.traffic_history_days = self.traffic_history_days.clamp(
                crate::history::MIN_RETENTION_DAYS,
                crate::history::MAX_RETENTION_DAYS,
            );
        }

        for app in &mut self.apps {
            let name = app.name.trim().to_string();
            app.name = name;
            if app.name.is_empty() {
                continue;
            }
            if app.file_name.trim().is_empty() {
                app.file_name = format!("{}.zip", app.name);
            }
            let workdir_empty = app
                .working_directory
                .as_deref()
                .map(|s| s.trim().is_empty())
                .unwrap_or(true);
            if workdir_empty {
                app.working_directory = Some(format!("../apps/{}", app.name));
            }
        }
    }

    /// 按名称查找应用（大小写不敏感）。
    pub fn find_app(&self, name: &str) -> Option<&AppConfig> {
        self.apps
            .iter()
            .find(|e| e.name.eq_ignore_ascii_case(name.trim()))
    }

    /// 按名称查找应用（可变）。
    pub fn find_app_mut(&mut self, name: &str) -> Option<&mut AppConfig> {
        self.apps
            .iter_mut()
            .find(|e| e.name.eq_ignore_ascii_case(name.trim()))
    }

    /// 设置应用启用状态。返回是否找到并产生变化。
    pub fn set_app_enable(&mut self, name: &str, enable: bool) -> bool {
        match self.find_app_mut(name) {
            Some(app) => {
                let changed = app.enable != enable;
                app.enable = enable;
                changed
            }
            None => false,
        }
    }

    /// 新增或更新子服务配置（供 `-AddService` 与安装脚本调用）。
    ///
    /// - 已存在同名（大小写不敏感）：覆盖程序路径，目录/参数传 `None` 时保留原值；
    /// - 不存在：新增条目；
    /// - 两者均置为启用（注册即启用，星尘启动/重载时会拉起）。
    /// 返回 `true` 表示产生变化；名称或程序路径为空时返回 `false`。
    pub fn upsert_app(
        &mut self,
        name: &str,
        file_name: &str,
        working_directory: Option<&str>,
        arguments: Option<&str>,
    ) -> bool {
        let name = name.trim();
        let file_name = file_name.trim();
        if name.is_empty() || file_name.is_empty() {
            return false;
        }
        match self.find_app_mut(name) {
            Some(app) => {
                app.file_name = file_name.to_string();
                if let Some(dir) = working_directory {
                    app.working_directory = Some(dir.trim().to_string());
                }
                if let Some(args) = arguments {
                    app.arguments = Some(args.trim().to_string());
                }
                app.enable = true;
                true
            }
            None => {
                self.apps.push(AppConfig {
                    name: name.to_string(),
                    file_name: file_name.to_string(),
                    working_directory: working_directory.map(|s| s.trim().to_string()),
                    arguments: arguments.map(|s| s.trim().to_string()),
                    enable: true,
                    ..AppConfig::default()
                });
                true
            }
        }
    }
}

// ————— XML 读写与迁移辅助（模板保注释；dhrust::config XML 管线，对齐 C# StarAgent.config） —————

/// 读取 XML 配置（键值 + `Services/ServiceInfo` 属性列表）。
fn read_xml_config(path: &Path) -> Result<AgentConfig, String> {
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let json = dhrust::config::read_xml_to_json(&text).map_err(|e| e.to_string())?;
    let root = json
        .as_object()
        .and_then(|o| o.values().next())
        .cloned()
        .unwrap_or(Json::Null);
    if !root.is_object() {
        return Err("未识别的配置结构（缺少根节点）".to_string());
    }
    Ok(config_from_json(&root))
}

/// XML（JSON 形态）→ 配置。缺失键取默认值；C# 特有字段忽略但保留在文件中。
fn config_from_json(root: &Json) -> AgentConfig {
    let mut cfg = AgentConfig::default();
    let Some(obj) = root.as_object() else {
        return cfg;
    };

    fn text_of(obj: &serde_json::Map<String, Json>, key: &str) -> Option<String> {
        obj.get(key).and_then(|v| v.as_str()).map(|s| s.to_string())
    }
    fn text_nonempty(obj: &serde_json::Map<String, Json>, key: &str) -> Option<String> {
        text_of(obj, key).filter(|s| !s.trim().is_empty())
    }
    fn bool_of(obj: &serde_json::Map<String, Json>, key: &str) -> Option<bool> {
        obj.get(key)
            .and_then(|v| v.as_str())
            .map(|s| s.trim().eq_ignore_ascii_case("true"))
    }
    fn parse_of<T: std::str::FromStr>(obj: &serde_json::Map<String, Json>, key: &str) -> Option<T> {
        obj.get(key)
            .and_then(|v| v.as_str())
            .and_then(|s| s.trim().parse::<T>().ok())
    }

    // C# StarAgentSetting 字段
    if let Some(v) = bool_of(obj, "Debug") {
        cfg.debug = v;
    }
    if let Some(v) = parse_of::<u16>(obj, "LocalPort") {
        cfg.local_port = v;
    }
    if let Some(v) = text_of(obj, "Project") {
        cfg.project = v;
    }
    if let Some(v) = text_of(obj, "Server") {
        cfg.server = v;
    }
    if let Some(v) = parse_of::<u64>(obj, "Delay") {
        cfg.delay = v;
    }
    if let Some(v) = bool_of(obj, "StartupHook") {
        cfg.startup_hook = v;
    }

    // Pek.RAgent 扩展字段
    if let Some(v) = text_nonempty(obj, "ServiceName") {
        cfg.service_name = v;
    }
    if let Some(v) = text_nonempty(obj, "DisplayName") {
        cfg.display_name = v;
    }
    if let Some(v) = text_nonempty(obj, "Description") {
        cfg.description = v;
    }
    if let Some(v) = bool_of(obj, "LocalOnly") {
        cfg.local_only = v;
    }
    if let Some(v) = parse_of::<u64>(obj, "StartWait") {
        cfg.start_wait = v;
    }
    if let Some(v) = parse_of::<i32>(obj, "MaxFails") {
        cfg.max_fails = v;
    }
    if let Some(v) = parse_of::<u64>(obj, "GuardPeriod") {
        cfg.guard_period = v;
    }
    if let Some(v) = text_of(obj, "WatchDog") {
        cfg.watch_dog = v;
    }
    if let Some(v) = text_nonempty(obj, "WebUserName") {
        cfg.web_user_name = v;
    }
    if let Some(v) = text_of(obj, "WebPassword").filter(|s| !s.is_empty()) {
        cfg.web_user_password = v;
    }
    if let Some(v) = text_nonempty(obj, "WebAuthLevel") {
        cfg.web_auth_level = v;
    }
    if let Some(v) = parse_of::<u64>(obj, "SampleInterval") {
        cfg.sample_interval = v;
    }
    if let Some(v) = bool_of(obj, "WebTraffic") {
        cfg.web_traffic = v;
    }
    if let Some(v) = text_of(obj, "WebLogs") {
        cfg.web_logs = v;
    }
    if let Some(v) = bool_of(obj, "PortTraffic") {
        cfg.port_traffic = v;
    }
    if let Some(v) = text_of(obj, "PortTrafficPorts") {
        cfg.port_traffic_ports = v;
    }
    if let Some(v) = parse_of::<u32>(obj, "TrafficHistoryDays") {
        cfg.traffic_history_days = v;
    }
    if let Some(v) = text_of(obj, "LogCleanupPaths") {
        cfg.log_cleanup_paths = v;
    }
    if let Some(v) = text_of(obj, "PluginStoreUrl") {
        cfg.plugin_store_url = v;
    }
    if let Some(v) = text_of(obj, "PluginStorePubKey") {
        cfg.plugin_store_pubkey = v;
    }
    if let Some(v) = bool_of(obj, "AiEnabled") {
        cfg.ai_enabled = v;
    }
    if let Some(v) = text_of(obj, "AiBaseUrl") {
        cfg.ai_base_url = v;
    }
    if let Some(v) = text_of(obj, "AiModel") {
        cfg.ai_model = v;
    }
    if let Some(v) = text_of(obj, "AiApiKey") {
        cfg.ai_api_key = v;
    }
    if let Some(v) = bool_of(obj, "TerminalEnabled") {
        cfg.terminal_enabled = v;
    }

    // 应用列表：<Services><ServiceInfo Name=".." FileName=".." ... /></Services>
    let services = obj.get("Services").and_then(|s| s.get("ServiceInfo"));
    let items: Vec<&Json> = match services {
        Some(Json::Array(list)) => list.iter().collect(),
        Some(single @ Json::Object(_)) => vec![single],
        _ => Vec::new(),
    };
    let mut apps = Vec::new();
    for item in items {
        if let Some(app) = app_from_json(item) {
            apps.push(app);
        }
    }
    if !apps.is_empty() {
        cfg.apps = apps;
    }

    cfg
}

/// `ServiceInfo`（属性形式）→ 应用配置。
fn app_from_json(item: &Json) -> Option<AppConfig> {
    let svc = item.as_object()?;
    let name = svc
        .get("Name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if name.is_empty() {
        return None;
    }

    let text = |key: &str| {
        svc.get(key)
            .and_then(|v| v.as_str())
            .map(|v| v.to_string())
            .filter(|v| !v.trim().is_empty())
    };
    let flag = |key: &str| {
        svc.get(key)
            .and_then(|v| v.as_str())
            .map(|v| v.trim().eq_ignore_ascii_case("true"))
            .unwrap_or(false)
    };
    let num = |key: &str| {
        svc.get(key)
            .and_then(|v| v.as_str())
            .and_then(|v| v.trim().parse::<u64>().ok())
    };

    Some(AppConfig {
        name,
        file_name: text("FileName").unwrap_or_default(),
        arguments: text("Arguments"),
        working_directory: text("WorkingDirectory"),
        user_name: text("UserName"),
        enable: flag("Enable"),
        // 保留原始形态（C# 数值 10-13/0-4 或文本），由 `DeployMode::parse` 归一化
        mode: text("Mode").unwrap_or_else(|| "shadow".to_string()),
        allow_multiple: flag("AllowMultiple"),
        environments: text("Environments"),
        auto_stop: flag("AutoStop"),
        reload_on_change: flag("ReloadOnChange"),
        max_memory: num("MaxMemory").unwrap_or(0) as u32,
        oom_score_adjust: svc
            .get("OomScoreAdjust")
            .and_then(|v| v.as_str())
            .and_then(|v| v.trim().parse::<i32>().ok())
            .unwrap_or(0),
        health_check: text("HealthCheck"),
        overwrite: text("Overwrite"), // Pek.RAgent 扩展属性
        debug: flag("Debug"),         // Pek.RAgent 扩展属性
    })
}

/// 渲染配置为 XML 文本：以现有文件（或内置模板）为骨架——
/// 标量键 upsert（保留注释与排版），`Services` 节整段重建（保留旧条目的未知属性）。
fn render_xml(cfg: &AgentConfig, current: Option<&str>) -> Result<String, String> {
    let base = current.unwrap_or(TEMPLATE);

    // 标量键（含扩展）；缺失时插入并带模板注释
    let comments = template_comments();
    let mut items: Vec<(String, String, String)> = Vec::new();
    {
        let mut push = |key: &str, value: String| {
            let comment = comments.get(key).cloned().unwrap_or_default();
            items.push((key.to_string(), value, comment));
        };
        push("Debug", bool_text(cfg.debug));
        push("Project", cfg.project.clone());
        push("Server", cfg.server.clone());
        push("LocalPort", cfg.local_port.to_string());
        push("Delay", cfg.delay.to_string());
        push("StartupHook", bool_text(cfg.startup_hook));
        push("ServiceName", cfg.service_name.clone());
        push("DisplayName", cfg.display_name.clone());
        push("Description", cfg.description.clone());
        push("LocalOnly", bool_text(cfg.local_only));
        push("StartWait", cfg.start_wait.to_string());
        push("MaxFails", cfg.max_fails.to_string());
        push("GuardPeriod", cfg.guard_period.to_string());
        push("WatchDog", cfg.watch_dog.clone());
        push("WebUserName", cfg.web_user_name.clone());
        push("WebPassword", cfg.web_user_password.clone());
        push("WebAuthLevel", cfg.web_auth_level.clone());
        push("SampleInterval", cfg.sample_interval.to_string());
        push("WebTraffic", bool_text(cfg.web_traffic));
        push("WebLogs", cfg.web_logs.clone());
        push("PortTraffic", bool_text(cfg.port_traffic));
        push("PortTrafficPorts", cfg.port_traffic_ports.clone());
        push("TrafficHistoryDays", cfg.traffic_history_days.to_string());
        push("LogCleanupPaths", cfg.log_cleanup_paths.clone());
        push("PluginStoreUrl", cfg.plugin_store_url.clone());
        push("PluginStorePubKey", cfg.plugin_store_pubkey.clone());
        push("AiEnabled", bool_text(cfg.ai_enabled));
        push("AiBaseUrl", cfg.ai_base_url.clone());
        push("AiModel", cfg.ai_model.clone());
        push("AiApiKey", cfg.ai_api_key.clone());
        push("TerminalEnabled", bool_text(cfg.terminal_enabled));
    }
    let after_scalars =
        dhrust::config::upsert_root_values(base, &items).map_err(|e| e.to_string())?;

    // Services 节重建（保留旧文件里 Rust 不认识的属性，如 C# 的 AutoStart/Priority）
    let previous = service_attrs(&after_scalars);
    let inner = render_services(&previous, &cfg.apps);
    dhrust::config::replace_root_section(&after_scalars, "Services", &inner)
        .map_err(|e| e.to_string())
}

/// 从模板提取“键 → 注释”映射（元素前最近的单行注释）。
fn template_comments() -> HashMap<String, String> {
    let mut map = HashMap::new();
    let mut pending: Option<String> = None;
    for line in TEMPLATE.lines() {
        let trimmed = line.trim();
        if let Some(comment) = trimmed
            .strip_prefix("<!--")
            .and_then(|s| s.strip_suffix("-->"))
        {
            pending = Some(comment.to_string());
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix('<') {
            if let Some(end) = rest.find('>') {
                let name = &rest[..end];
                if !name.starts_with('/') && !name.starts_with('?') {
                    if let Some(comment) = pending.take() {
                        map.insert(name.to_string(), comment);
                    }
                }
            }
        }
    }
    map
}

/// 旧文件中的 `ServiceInfo` 属性表（按出现顺序；用于保留 Rust 不认识的属性）。
fn service_attrs(text: &str) -> Vec<Json> {
    let Ok(json) = dhrust::config::read_xml_to_json(text) else {
        return Vec::new();
    };
    let services = json
        .as_object()
        .and_then(|o| o.values().next())
        .and_then(|root| root.get("Services"))
        .and_then(|s| s.get("ServiceInfo"));
    match services {
        Some(Json::Array(list)) => list.clone(),
        Some(single @ Json::Object(_)) => vec![single.clone()],
        _ => Vec::new(),
    }
}

/// Rust 认识的 `ServiceInfo` 属性（重建时覆盖；其余属性原样保留以兼容 C#）。
const KNOWN_SERVICE_ATTRS: [&str; 16] = [
    "Name",
    "FileName",
    "Arguments",
    "WorkingDirectory",
    "UserName",
    "Enable",
    "Mode",
    "AllowMultiple",
    "Environments",
    "AutoStop",
    "ReloadOnChange",
    "MaxMemory",
    "OomScoreAdjust",
    "HealthCheck",
    "Overwrite",
    "Debug",
];

/// 生成 `<Services>` 节内段（4 空格缩进，每应用单行）。
fn render_services(previous: &[Json], apps: &[AppConfig]) -> String {
    let mut inner = String::new();
    for app in apps {
        // 旧属性为底：保留未知属性（C# 的 AutoStart/Priority 等）
        let mut attrs: Vec<(String, String)> = Vec::new();
        if let Some(old) = previous.iter().find(|o| {
            o.get("Name")
                .and_then(|v| v.as_str())
                .map(|n| n.eq_ignore_ascii_case(&app.name))
                .unwrap_or(false)
        }) {
            if let Some(obj) = old.as_object() {
                for (key, value) in obj {
                    if !KNOWN_SERVICE_ATTRS.contains(&key.as_str()) {
                        attrs.push((key.clone(), value.as_str().unwrap_or("").to_string()));
                    }
                }
            }
        }

        // Rust 字段（固定顺序，对齐 C# `ServiceInfo` 声明序；Mode 写 C# 数值 10-13）
        attrs.push(("Name".to_string(), app.name.clone()));
        attrs.push(("FileName".to_string(), app.file_name.clone()));
        attrs.push((
            "Arguments".to_string(),
            app.arguments.clone().unwrap_or_default(),
        ));
        attrs.push((
            "WorkingDirectory".to_string(),
            app.working_directory.clone().unwrap_or_default(),
        ));
        attrs.push(("UserName".to_string(), app.user_name.clone().unwrap_or_default()));
        attrs.push(("Enable".to_string(), bool_text(app.enable)));
        attrs.push(("Mode".to_string(), mode_number(&app.mode_text())));
        attrs.push(("AllowMultiple".to_string(), bool_text(app.allow_multiple)));
        attrs.push((
            "Environments".to_string(),
            app.environments.clone().unwrap_or_default(),
        ));
        attrs.push(("AutoStop".to_string(), bool_text(app.auto_stop)));
        attrs.push(("ReloadOnChange".to_string(), bool_text(app.reload_on_change)));
        attrs.push(("MaxMemory".to_string(), app.max_memory.to_string()));
        attrs.push((
            "OomScoreAdjust".to_string(),
            app.oom_score_adjust.to_string(),
        ));
        attrs.push((
            "HealthCheck".to_string(),
            app.health_check.clone().unwrap_or_default(),
        ));
        // Pek.RAgent 扩展属性（C# XmlSerializer 读取时忽略未知属性）
        attrs.push(("Overwrite".to_string(), app.overwrite.clone().unwrap_or_default()));
        attrs.push(("Debug".to_string(), bool_text(app.debug)));

        inner.push_str("    <ServiceInfo");
        for (key, value) in &attrs {
            inner.push(' ');
            inner.push_str(key);
            inner.push_str("=\"");
            inner.push_str(&escape_attr(value));
            inner.push('"');
        }
        inner.push_str(" />\n");
    }
    inner
}

/// 布尔 → XML 文本。
fn bool_text(value: bool) -> String {
    if value { "true" } else { "false" }.to_string()
}

/// 部署模式 → C# `DeployMode` 数值（10=Standard，11=Shadow，12=Hosted，13=Task）。
fn mode_number(mode: &str) -> String {
    (crate::deploy::DeployMode::parse(mode) as i32).to_string()
}

/// XML 属性值转义。
fn escape_attr(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// 迁移旧版 TOML 配置：读入后原文件改名 `.toml.bak`（避免再次迁移）。
fn migrate_from_toml(base: &Path) -> Result<AgentConfig, String> {
    let path = legacy_toml_path(base);
    let text = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    let text = text.trim_start_matches('\u{feff}');
    let doc = toml::parse_document(text)?;
    let value = toml::document_to_json(&doc)?;
    let cfg: AgentConfig = serde_json::from_value(value).map_err(|e| e.to_string())?;

    let bak = PathBuf::from(format!("{}.bak", path.display()));
    let _ = std::fs::rename(&path, &bak);
    util::log_format(
        "配置已从 Agent.toml 迁移为 StarAgent.config（原文件备份为 {}）",
        &[&bak.display().to_string()],
    );
    Ok(cfg)
}

/// 迁移旧版 JSON 配置：读入后原文件改名 `.json.bak`（避免再次迁移）。
fn migrate_from_json(base: &Path) -> Result<AgentConfig, String> {
    let path = legacy_json_path(base);
    let text = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    let text = text.trim_start_matches('\u{feff}');
    let cfg: AgentConfig = serde_json::from_str(text).map_err(|e| e.to_string())?;

    let bak = PathBuf::from(format!("{}.bak", path.display()));
    let _ = std::fs::rename(&path, &bak);
    util::log_format(
        "配置已从 Agent.json 迁移为 StarAgent.config（原文件备份为 {}）",
        &[&bak.display().to_string()],
    );
    Ok(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_roundtrip_uses_pascal_case() {
        let mut cfg = AgentConfig::default();
        cfg.normalize();
        let text = serde_json::to_string(&cfg).unwrap();
        assert!(text.contains("\"ServiceName\""));
        assert!(text.contains("\"LocalPort\""));

        let back: AgentConfig = serde_json::from_str(&text).unwrap();
        assert_eq!(back.service_name, cfg.service_name);
        assert_eq!(back.apps.len(), cfg.apps.len());
    }

    #[test]
    fn missing_fields_fall_back_to_defaults() {
        let cfg: AgentConfig = serde_json::from_str(r#"{ "ServiceName": "X" }"#).unwrap();
        assert_eq!(cfg.service_name, "X");
        assert_eq!(cfg.local_port, DEFAULT_LOCAL_PORT);
        assert_eq!(cfg.delay, 3000);
    }

    #[test]
    fn sample_interval_reads_clamps_and_renders() {
        // XML 读取路径（扩展区配置项；XML 值在 JSON 形态下是字符串）
        let json: Json = serde_json::from_str(r#"{ "SampleInterval": "2500" }"#).unwrap();
        let cfg = config_from_json(&json);
        assert_eq!(cfg.sample_interval, 2500);

        // 归一化：下限/上限保护；0 = 关闭后台采样
        let mut cfg = AgentConfig::default();
        cfg.sample_interval = 50;
        cfg.normalize();
        assert_eq!(cfg.sample_interval, 200);
        cfg.sample_interval = 999_999;
        cfg.normalize();
        assert_eq!(cfg.sample_interval, 60_000);
        cfg.sample_interval = 0;
        cfg.normalize();
        assert_eq!(cfg.sample_interval, 0);

        // 渲染：写入模板骨架（保留注释）
        let mut cfg = AgentConfig::default();
        cfg.sample_interval = 2500;
        let text = render_xml(&cfg, None).unwrap();
        assert!(
            text.contains("<SampleInterval>2500</SampleInterval>"),
            "{text}"
        );
    }

    #[test]
    fn traffic_config_reads_renders_and_normalizes() {
        // XML 读取路径（XML 值在 JSON 形态下是字符串）
        let json: Json = serde_json::from_str(
            r#"{ "WebTraffic": "false", "WebLogs": "a=/tmp/a.log", "PortTraffic": "true", "PortTrafficPorts": "22,80", "TrafficHistoryDays": "30" }"#,
        )
        .unwrap();
        let cfg = config_from_json(&json);
        assert!(!cfg.web_traffic);
        assert_eq!(cfg.web_logs, "a=/tmp/a.log");
        assert!(cfg.port_traffic);
        assert_eq!(cfg.port_traffic_ports, "22,80");
        assert_eq!(cfg.traffic_history_days, 30);

        // 默认值：网站与端口流量均默认开启（端口流量在 Linux 为独立计数表，只计数不改转发，
        // 无 nft/权限不足自动降级；关闭/卸载自动清理）
        let mut cfg = AgentConfig::default();
        assert!(cfg.web_traffic);
        assert!(cfg.port_traffic);
        assert_eq!(cfg.traffic_history_days, 90, "默认保留 90 天");

        // 归一化：两侧空白清理；保留天数下限 7 天
        cfg.web_logs = "  x=/tmp/x.log ".to_string();
        cfg.port_traffic_ports = " 22 ".to_string();
        cfg.traffic_history_days = 2;
        cfg.normalize();
        assert_eq!(cfg.web_logs, "x=/tmp/x.log");
        assert_eq!(cfg.port_traffic_ports, "22");
        assert_eq!(cfg.traffic_history_days, 7, "非 0 下限 7 天");

        // 0 = 永久保留（不被下限修正）
        cfg.traffic_history_days = 0;
        cfg.normalize();
        assert_eq!(cfg.traffic_history_days, 0);

        // 渲染：模板骨架带上新字段（注释由模板保障）
        let text = render_xml(&cfg, None).unwrap();
        assert!(text.contains("<WebTraffic>true</WebTraffic>"), "{text}");
        assert!(text.contains("<WebLogs>x=/tmp/x.log</WebLogs>"), "{text}");
        assert!(text.contains("<PortTraffic>true</PortTraffic>"), "{text}");
        assert!(
            text.contains("<PortTrafficPorts>22</PortTrafficPorts>"),
            "{text}"
        );
        assert!(
            text.contains("<TrafficHistoryDays>0</TrafficHistoryDays>"),
            "{text}"
        );
    }

    #[test]
    fn log_cleanup_paths_reads_renders_and_normalizes() {
        // XML 读取路径（XML 值在 JSON 形态下是字符串）
        let json: Json =
            serde_json::from_str(r#"{ "LogCleanupPaths": " /var/log/x;/tmp/y " }"#).unwrap();
        let mut cfg = config_from_json(&json);
        cfg.normalize();
        assert_eq!(cfg.log_cleanup_paths, "/var/log/x;/tmp/y");

        // 默认：空（不显示“自定义路径”分类）
        assert!(AgentConfig::default().log_cleanup_paths.is_empty());

        // 渲染：模板骨架带上新字段（注释由模板保障；空值也应写出空元素）
        let mut cfg = AgentConfig::default();
        cfg.log_cleanup_paths = "/www/wwwlogs;/tmp/cache".to_string();
        let text = render_xml(&cfg, None).unwrap();
        assert!(
            text.contains("<LogCleanupPaths>/www/wwwlogs;/tmp/cache</LogCleanupPaths>"),
            "{text}"
        );
        let empty = render_xml(&AgentConfig::default(), None).unwrap();
        assert!(empty.contains("<LogCleanupPaths"), "{empty}");
    }

    #[test]
    fn plugin_store_fields_read_render_and_normalize() {
        let json: Json = serde_json::from_str(
            r#"{ "PluginStoreUrl": " https://x.example/catalog.json ", "PluginStorePubKey": " abcd " }"#,
        )
        .unwrap();
        let mut cfg = config_from_json(&json);
        cfg.normalize();
        assert_eq!(cfg.plugin_store_url, "https://x.example/catalog.json");
        assert_eq!(cfg.plugin_store_pubkey, "abcd");
        let default = AgentConfig::default();
        assert!(default.plugin_store_url.is_empty());
        assert!(default.plugin_store_pubkey.is_empty());
        let mut cfg = AgentConfig::default();
        cfg.plugin_store_url = "https://x.example/catalog.json".to_string();
        let text = render_xml(&cfg, None).unwrap();
        assert!(
            text.contains("<PluginStoreUrl>https://x.example/catalog.json</PluginStoreUrl>"),
            "{text}"
        );
        assert!(text.contains("<PluginStorePubKey"), "{text}");
    }

    #[test]
    fn ai_fields_read_render_and_normalize() {
        let json: Json = serde_json::from_str(
            r#"{ "AiEnabled": "false", "AiBaseUrl": " https://api.deepseek.com/v1 ", "AiModel": " deepseek-reasoner ", "AiApiKey": " sk-x " }"#,
        )
        .unwrap();
        let mut cfg = config_from_json(&json);
        cfg.normalize();
        assert!(!cfg.ai_enabled);
        assert_eq!(cfg.ai_base_url, "https://api.deepseek.com/v1");
        assert_eq!(cfg.ai_model, "deepseek-reasoner");
        assert_eq!(cfg.ai_api_key, "sk-x");

        // 空地址/模型回退默认（误清空配置时保持可用）
        let json: Json = serde_json::from_str(r#"{ "AiBaseUrl": "", "AiModel": "" }"#).unwrap();
        let mut cfg = config_from_json(&json);
        cfg.normalize();
        assert_eq!(cfg.ai_base_url, "https://api.deepseek.com/v1");
        assert_eq!(cfg.ai_model, "deepseek-chat");

        let default = AgentConfig::default();
        assert!(default.ai_enabled);
        assert!(default.ai_api_key.is_empty());

        let mut cfg = AgentConfig::default();
        cfg.ai_model = "deepseek-reasoner".to_string();
        let text = render_xml(&cfg, None).unwrap();
        assert!(text.contains("<AiModel>deepseek-reasoner</AiModel>"), "{text}");
        assert!(
            text.contains("<AiBaseUrl>https://api.deepseek.com/v1</AiBaseUrl>"),
            "{text}"
        );
        assert!(text.contains("<AiEnabled>true</AiEnabled>"), "{text}");
        assert!(text.contains("<AiApiKey"), "{text}");
    }

    #[test]
    fn terminal_enabled_read_render() {
        let json: Json = serde_json::from_str(r#"{ "TerminalEnabled": "false" }"#).unwrap();
        let cfg = config_from_json(&json);
        assert!(!cfg.terminal_enabled);
        assert!(AgentConfig::default().terminal_enabled);
        let text = render_xml(&AgentConfig::default(), None).unwrap();
        assert!(text.contains("<TerminalEnabled>true</TerminalEnabled>"), "{text}");
    }

    #[test]
    fn normalize_fills_app_defaults() {
        let mut cfg = AgentConfig {
            apps: vec![AppConfig {
                name: "app1".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        };
        cfg.normalize();
        assert_eq!(cfg.apps[0].file_name, "app1.zip");
        assert_eq!(cfg.apps[0].working_directory.as_deref(), Some("../apps/app1"));
        assert_eq!(cfg.apps[0].mode_text(), "shadow");
    }

    #[test]
    fn upsert_app_adds_and_updates() {
        // 默认配置自带示例应用，这里以空列表起步
        let mut cfg = AgentConfig {
            apps: Vec::new(),
            ..AgentConfig::default()
        };

        // 新增：启用、字段落位
        assert!(cfg.upsert_app("app1", "/opt/app1/app", Some("/opt/app1"), Some("-x")));
        assert_eq!(cfg.apps.len(), 1);
        let app = cfg.find_app("APP1").unwrap();
        assert!(app.enable);
        assert_eq!(app.file_name, "/opt/app1/app");
        assert_eq!(app.working_directory.as_deref(), Some("/opt/app1"));
        assert_eq!(app.arguments.as_deref(), Some("-x"));

        // 更新：覆盖程序路径并重新启用，未覆盖字段（如 mode）保留
        cfg.apps[0].mode = "hosted".to_string();
        cfg.apps[0].enable = false;
        assert!(cfg.upsert_app("app1", "/opt/app1/app2", None, None));
        assert_eq!(cfg.apps.len(), 1, "同名更新不应新增条目");
        let app = cfg.apps[0].clone();
        assert_eq!(app.file_name, "/opt/app1/app2");
        assert_eq!(app.mode, "hosted", "未覆盖字段应保留");
        assert!(app.enable, "注册即启用");
        assert_eq!(app.working_directory.as_deref(), Some("/opt/app1"));
        assert_eq!(app.arguments.as_deref(), Some("-x"));

        // 空名称 / 空程序路径：拒绝且不产生条目
        assert!(!cfg.upsert_app("  ", "/x", None, None));
        assert!(!cfg.upsert_app("app2", "  ", None, None));
        assert_eq!(cfg.apps.len(), 1);
    }

    /// 独立临时目录（含 Config 子目录）。
    fn temp_base(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ragent-config-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join("Config")).unwrap();
        dir
    }

    #[test]
    fn generates_xml_with_comments() {
        let base = temp_base("gen");
        let cfg = AgentConfig::load(&base);
        assert_eq!(cfg.service_name, "StarAgentRust");
        assert_ne!(DEFAULT_SERVICE_NAME, LEGACY_SERVICE_NAME);
        assert_eq!(cfg.local_port, DEFAULT_LOCAL_PORT);

        let text = std::fs::read_to_string(config_path(&base)).unwrap();
        assert!(text.contains("<!--本地端口"), "应带注释：\n{text}");
        assert!(text.contains("<ServiceName>StarAgentRust</ServiceName>"), "{text}");
        assert!(text.contains("<Services>"), "{text}");

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn save_keeps_comments_and_updates_values() {
        let base = temp_base("keep");
        let mut cfg = AgentConfig::load(&base);
        cfg.local_port = 5599;
        cfg.apps[0].enable = true;
        cfg.save(&base).unwrap();

        let text = std::fs::read_to_string(config_path(&base)).unwrap();
        assert!(text.contains("<LocalPort>5599</LocalPort>"), "值应更新：\n{text}");
        assert!(text.contains("<!--本地端口"), "注释应保留：\n{text}");
        assert!(text.contains("Enable=\"true\""), "应用值应更新：\n{text}");
        assert!(text.contains("<ServiceInfo"), "应用条目应存在：\n{text}");

        let back = AgentConfig::load(&base);
        assert_eq!(back.local_port, 5599);
        assert!(back.apps[0].enable);

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn save_clears_text_fields_when_empty() {
        // 回归：空值此前写不回文件（upsert 跳过空值），导致“清空 WebLogs/PortTrafficPorts”不生效
        let base = temp_base("clear");
        let mut cfg = AgentConfig::load(&base);
        cfg.web_logs = "demo=/tmp/demo.log".to_string();
        cfg.port_traffic_ports = "22,80".to_string();
        cfg.save(&base).unwrap();
        let text = std::fs::read_to_string(config_path(&base)).unwrap();
        assert!(text.contains("<WebLogs>demo=/tmp/demo.log</WebLogs>"), "{text}");

        // 清空后保存：文件应写回空元素
        cfg.web_logs = String::new();
        cfg.port_traffic_ports = String::new();
        cfg.save(&base).unwrap();
        let text = std::fs::read_to_string(config_path(&base)).unwrap();
        assert!(text.contains("<WebLogs></WebLogs>"), "清空后应为空元素：\n{text}");
        assert!(
            text.contains("<PortTrafficPorts></PortTrafficPorts>"),
            "清空后应为空元素：\n{text}"
        );
        assert!(!text.contains("demo="), "旧值应被清除：\n{text}");

        let back = AgentConfig::load(&base);
        assert!(back.web_logs.is_empty());
        assert!(back.port_traffic_ports.is_empty());

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn migrates_from_legacy_json() {
        let base = temp_base("migrate");
        let mut old = AgentConfig::default();
        old.local_port = 5601;
        old.apps = vec![AppConfig {
            name: "legacy".to_string(),
            file_name: "legacy.zip".to_string(),
            enable: true,
            ..Default::default()
        }];
        std::fs::write(
            legacy_json_path(&base),
            serde_json::to_string_pretty(&old).unwrap(),
        )
        .unwrap();

        let cfg = AgentConfig::load(&base);
        assert_eq!(cfg.local_port, 5601);
        assert_eq!(cfg.apps.len(), 1);
        assert_eq!(cfg.apps[0].name, "legacy");

        assert!(config_path(&base).exists(), "应生成 StarAgent.config");
        assert!(
            PathBuf::from(format!("{}.bak", legacy_json_path(&base).display())).exists(),
            "旧文件应备份为 .json.bak"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn migrates_from_legacy_toml() {
        let base = temp_base("migrate-toml");
        // 手写最小 TOML（模拟旧版 Pek.RAgent 配置）
        std::fs::write(
            legacy_toml_path(&base),
            "ServiceName = \"StarAgent\"\nLocalPort = 5602\n\n[[Apps]]\nName = \"legacy\"\nFileName = \"legacy.zip\"\nEnable = true\n",
        )
        .unwrap();

        let cfg = AgentConfig::load(&base);
        assert_eq!(cfg.local_port, 5602);
        assert_eq!(cfg.apps.len(), 1);
        assert_eq!(cfg.apps[0].name, "legacy");

        assert!(config_path(&base).exists(), "应生成 StarAgent.config");
        assert!(
            PathBuf::from(format!("{}.bak", legacy_toml_path(&base).display())).exists(),
            "旧文件应备份为 .toml.bak"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn reads_csharp_file_and_preserves_csharp_fields() {
        let base = temp_base("csharp");
        // 一份典型的 C# StarAgent.config（含 Rust 不认识的字段/属性）
        let xml = r#"<?xml version="1.0" encoding="utf-8"?>
<StarAgent>
  <Debug>true</Debug>
  <Code>abc123</Code>
  <Channel>Release</Channel>
  <LocalPort>5700</LocalPort>
  <Project>demo</Project>
  <Delay>5000</Delay>
  <Services>
    <ServiceInfo Name="StarServer" FileName="dotnet" Arguments="StarServer.dll" WorkingDirectory="..\server" Enable="true" MaxMemory="128" Priority="2" />
    <ServiceInfo Name="StarWeb" FileName="StarWeb.zip" Arguments="urls=http://*:6680" Enable="false" />
  </Services>
</StarAgent>"#;
        std::fs::write(config_path(&base), xml).unwrap();

        let mut cfg = AgentConfig::load(&base);
        assert!(cfg.debug);
        assert_eq!(cfg.local_port, 5700);
        assert_eq!(cfg.project, "demo");
        assert_eq!(cfg.delay, 5000);
        assert_eq!(cfg.apps.len(), 2);
        assert_eq!(cfg.apps[0].name, "StarServer");
        assert_eq!(cfg.apps[0].file_name, "dotnet");
        assert_eq!(cfg.apps[0].max_memory, 128);
        assert!(cfg.apps[0].enable);
        assert_eq!(cfg.apps[1].arguments.as_deref(), Some("urls=http://*:6680"));

        // 保存后：C# 特有字段保留、未知属性（Priority）保留、Mode 写 C# 数值
        cfg.local_port = 5711;
        cfg.save(&base).unwrap();
        let text = std::fs::read_to_string(config_path(&base)).unwrap();
        assert!(text.contains("<LocalPort>5711</LocalPort>"), "{text}");
        assert!(text.contains("<Code>abc123</Code>"), "C# 字段应保留：\n{text}");
        assert!(text.contains("<Channel>Release</Channel>"), "C# 字段应保留：\n{text}");
        assert!(text.contains("Priority=\"2\""), "未知属性应保留：\n{text}");
        assert!(text.contains("Mode=\"11\""), "Mode 应写 C# 数值：\n{text}");

        // 回读一致
        let back = AgentConfig::load(&base);
        assert_eq!(back.local_port, 5711);
        assert_eq!(back.apps.len(), 2);
        assert_eq!(back.apps[0].max_memory, 128);

        let _ = std::fs::remove_dir_all(&base);
    }
}
