//! 服务管理（薄壳；实现已下沉 `dhrust::service`，2026-10-03 收拢轮）。
//!
//! 本模块只保留 StarAgent 侧的装配信息：服务名/显示名/描述来自配置，
//! 旧默认名 `StarAgent` 参与迁移清理（安装/重装时自动停掉并删除**指向本程序**的旧注册；
//! 指向 C# 版星尘等其它程序的注册保留不动，二者可继续并存）。

use std::path::Path;

use crate::config::{self, AgentConfig};

pub use dhrust::service::{ServiceManager, ServiceState};

#[cfg(windows)]
pub use dhrust::service::windows;

/// 按 StarAgent 配置装配服务管理器（含旧服务名 `StarAgent` 迁移清理信息）。
///
/// `base` 为程序基础目录（用于展示与初始化脚本模板）。
pub fn manager(base: &Path, cfg: &AgentConfig) -> ServiceManager {
    ServiceManager::new(
        base,
        &cfg.service_name,
        &cfg.display_name,
        &cfg.description,
        &[config::LEGACY_SERVICE_NAME],
    )
}
