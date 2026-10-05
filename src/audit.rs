//! 面板用户与操作审计（薄壳：核心实现在 `pek_rcode::panel`，2026-10-05 下沉）。
//!
//! - 数据表：`Agent_PanelUser` / `Agent_OperationLog`（同流量历史库 `Data/traffic.db`，
//!   模型见 `Entity/Model.xml`）；内置管理员（配置文件 `WebUserName`/`WebPassword`，
//!   超级权限）不落本表；
//! - 本文件仅保留：Agent 权限表、数据目录 → 共享存储的桥接与原有函数签名（调用点零改动）；
//!   用户 / 审计核心逻辑与测试见 `pek_rcode::panel`（多项目共用组装线）。
//!
//! 说明：`Agent_OperationLog` 当前**无 `Category` 列**（与产测工具表结构差异），
//! 审计写入自动省略该列（`AuditEntry.category = None`）；查询的类别过滤参数由薄壳传空。

use std::path::Path;
use std::sync::{Arc, OnceLock};

use serde_json::Value as Json;

use crate::history;

pub(crate) use pek_rcode::panel::{AuditEntry, TABLE_OPLOG, TABLE_USER};

/// 面板存储（流量历史库共享；注册表单飞打开）。
fn store(base: &Path) -> Result<Arc<pek_rcode::store::SharedStore>, String> {
    history::storage(base)
}

/// 全部可选菜单权限（key 与前端导航 `data-panel` 一致；顺序即展示顺序）。
/// `users`（用户管理）不在授予范围：仅内置管理员可访问。
pub(crate) const ALL_PERMISSIONS: &[(&str, &str)] = &[
    ("dashboard", "状态"),
    ("services", "子服务"),
    ("traffic", "流量"),
    ("control", "控制"),
    ("config", "配置"),
    ("starconfig", "星尘设置"),
    ("logs", "日志"),
    ("watchdog", "看门狗"),
    ("database", "数据库"),
    ("fileman", "文件管理"),
    ("cleanup", "日志清理"),
    ("plugins", "插件"),
    ("ai", "AI 助手"),
    ("terminal", "在线终端"),
    ("audit", "操作日志"),
];

/// 权限与用户存储门面（单例；权限表见 [`ALL_PERMISSIONS`]）。
fn auth() -> &'static pek_rcode::panel::PanelAuth {
    static AUTH: OnceLock<pek_rcode::panel::PanelAuth> = OnceLock::new();
    AUTH.get_or_init(|| pek_rcode::panel::PanelAuth::new(ALL_PERMISSIONS))
}

// ————— 用户（保持原签名：调用点零改动） —————

/// 按用户名查找（大小写不敏感）。
pub(crate) fn find_user(
    base: &Path,
    user_name: &str,
) -> Result<Option<pek_rcode::panel::PanelUser>, String> {
    auth().find_user(&*store(base)?, user_name)
}

/// 列出全部用户（JSON 视图，含权限中文名）。
pub(crate) fn list_users_json(base: &Path) -> Result<Json, String> {
    auth().list_users_json(&*store(base)?)
}

/// 登录校验：返回启用且密码正确的用户。
pub(crate) fn verify_login(
    base: &Path,
    user_name: &str,
    password: &str,
) -> Result<Option<pek_rcode::panel::PanelUser>, String> {
    auth().verify_login(&*store(base)?, user_name, password)
}

/// 保存用户（不存在则创建，要求 `password` 非空；存在则更新，`password` 为空表示不改）。
pub(crate) fn save_user(
    base: &Path,
    user_name: &str,
    password: Option<&str>,
    permissions: &[String],
    enabled: bool,
    remark: &str,
) -> Result<(), String> {
    auth().save_user(&*store(base)?, user_name, password, permissions, enabled, remark)
}

/// 修改用户密码（校验旧密码；供用户自助改密）。
pub(crate) fn change_user_password(
    base: &Path,
    user_name: &str,
    old_password: &str,
    new_password: &str,
) -> Result<(), String> {
    auth().change_user_password(&*store(base)?, user_name, old_password, new_password)
}

/// 删除用户。
pub(crate) fn delete_user(base: &Path, user_name: &str) -> Result<(), String> {
    auth().delete_user(&*store(base)?, user_name)
}

// ————— 审计 —————

/// 写入一条审计记录（失败只记程序日志，不影响业务）。
pub(crate) fn record(base: &Path, entry: &AuditEntry) {
    match store(base) {
        Ok(store) => pek_rcode::panel::record(&store, entry),
        Err(e) => dhrust::logs::log().error(&format!("操作日志写入失败（存储不可用）：{e}")),
    }
}

/// 分页查询操作日志（保持原签名：`page/size/user/keyword/success`；类别过滤不启用）。
pub(crate) fn query_logs(
    base: &Path,
    page: usize,
    size: usize,
    user: &str,
    keyword: &str,
    success: Option<bool>,
) -> Result<Json, String> {
    pek_rcode::panel::query_logs(&*store(base)?, page, size, "", user, keyword, success)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 薄壳连通性冒烟：经 `history::storage` + `pek_rcode::panel` 完成用户 CRUD 与审计写入
    /// （核心逻辑测试见 `pek-rcode::panel` 模块）。
    #[test]
    fn shell_user_crud_and_audit_smoke() {
        let base = std::env::temp_dir().join(format!(
            "ragent-audit-smoke-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&base).unwrap();

        save_user(&base, "alice", Some("pw123"), &["config".into()], true, "冒烟").unwrap();
        let u = find_user(&base, "ALICE").unwrap().unwrap();
        assert!(u.verify("pw123"));
        assert!(verify_login(&base, "alice", "pw123").unwrap().is_some());
        assert!(verify_login(&base, "alice", "bad").unwrap().is_none());

        record(
            &base,
            &AuditEntry {
                category: None,
                user: "alice".into(),
                action: "login".into(),
                title: "登录".into(),
                method: "POST".into(),
                path: "/api/login".into(),
                success: true,
                ..Default::default()
            },
        );
        let logs = query_logs(&base, 1, 50, "alice", "", None).unwrap();
        assert_eq!(logs["total"], 1);
        assert_eq!(logs["items"][0]["action"], "login");

        delete_user(&base, "alice").unwrap();
        assert!(find_user(&base, "alice").unwrap().is_none());

        pek_rcode::store::drop_for_test(&base);
        let _ = std::fs::remove_dir_all(&base);
    }
}
