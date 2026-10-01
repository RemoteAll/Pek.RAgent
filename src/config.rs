//! 配置：`Config/Agent.json`。
//!
//! 字段名沿用 C# `ServiceInfo` 语义（PascalCase），便于熟悉星尘的运维人员迁移；
//! 所有字段缺省即用默认值，新增字段可直接追加，不破坏旧文件。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::util;

/// 默认服务名（Windows 服务 / systemd 单元 / launchd 任务）。
pub const DEFAULT_SERVICE_NAME: &str = "StarAgent";
/// 本地控制端口（DHDeploy 依赖 5500 调用 RestartService/StartService/StopService）。
pub const DEFAULT_LOCAL_PORT: u16 = 5500;

/// 代理配置。
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "PascalCase", default)]
pub struct AgentConfig {
    /// 服务名。默认 StarAgent，与现有部署脚本/服务名保持一致
    pub service_name: String,
    /// 显示名
    pub display_name: String,
    /// 服务描述
    pub description: String,
    /// 本地控制端口。默认 5500，DHDeploy 依赖该端口
    pub local_port: u16,
    /// 仅本机访问。默认 true（只绑定 127.0.0.1）
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
    /// 应用服务集合
    pub apps: Vec<AppConfig>,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            service_name: DEFAULT_SERVICE_NAME.to_string(),
            display_name: "星尘代理".to_string(),
            description: "星尘节点守护代理（Pek.RAgent）。提供进程守护、影子目录部署与本地控制接口。".to_string(),
            local_port: DEFAULT_LOCAL_PORT,
            local_only: true,
            delay: 3000,
            start_wait: 3000,
            max_fails: 20,
            guard_period: 30_000,
            debug: false,
            server: String::new(),
            project: String::new(),
            startup_hook: false,
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
            reload_on_change: false,
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

/// 配置文件路径。
pub fn config_path(base: &Path) -> PathBuf {
    base.join("Config").join("Agent.json")
}

impl AgentConfig {
    /// 加载配置。文件不存在时生成默认配置并落盘；内容损坏时备份为 `.bad` 并用默认配置继续。
    pub fn load(base: &Path) -> AgentConfig {
        let path = config_path(base);

        let mut cfg = if path.exists() {
            match std::fs::read_to_string(&path) {
                Ok(text) => {
                    let text = text.trim_start_matches('\u{feff}');
                    match serde_json::from_str::<AgentConfig>(text) {
                        Ok(cfg) => cfg,
                        Err(e) => {
                            let bad = PathBuf::from(format!("{}.bad", path.display()));
                            let _ = std::fs::rename(&path, &bad);
                            util::log_error(&format!(
                                "配置文件解析失败，已备份到 {}：{}",
                                bad.display(),
                                e
                            ));
                            AgentConfig::default()
                        }
                    }
                }
                Err(e) => {
                    util::log_error(&format!("读取配置文件失败：{}", e));
                    AgentConfig::default()
                }
            }
        } else {
            let cfg = AgentConfig::default();
            if let Err(e) = cfg.save(base) {
                util::log_error(&format!("生成默认配置失败：{}", e));
            } else {
                util::log_info(&format!("已生成默认配置 {}", path.display()));
            }
            cfg
        };

        cfg.normalize();
        cfg
    }

    /// 保存配置（美化 JSON）。
    pub fn save(&self, base: &Path) -> std::io::Result<()> {
        let path = config_path(base);
        let text = serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".to_string());
        util::write_file_atomic(&path, &text)
    }

    /// 归一化：补默认值，修正应用的缺省文件名与工作目录（与 C# `ServiceManager.Fix` 一致）。
    pub fn normalize(&mut self) {
        if self.service_name.trim().is_empty() {
            self.service_name = DEFAULT_SERVICE_NAME.to_string();
        }
        self.service_name = self.service_name.trim().to_string();
        if self.display_name.trim().is_empty() {
            self.display_name = "星尘代理".to_string();
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
}
