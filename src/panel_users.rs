//! 平台统一管控面板子用户（平台权威 + 上线自动补齐；2026-10-26）。
//!
//! 背景：多台节点各自维护面板子用户，人员离职时要逐台清理。现在由 Pek.RPanlServer
//! 维护**统一用户清单**，通过实时通道下发；本模块负责在节点侧落库：
//!
//! - 下行指令（平台 → 节点）：
//!   - `panelUserList`：查询本机子用户（应答 `panelUsers`，不含哈希）；
//!   - `panelUserUpsert`：新增/更新单个用户（带 `passwordHash`+`salt`，平台不传明文）；
//!   - `panelUserDelete`：删除单个用户；
//!   - `panelUserSync`：**全量对齐**（`prune=true` 时删除清单外的子用户）——平台权威；
//! - 上行摘要（节点 → 平台，随 register/心跳）：`caps` + `panelUsersVersion`（清单 SHA-256）
//!   + `panelUsersCount`，平台据此判断节点是否与平台一致（差异时自动下发 `panelUserSync`）。
//!
//! 安全边界：**内置管理员（配置文件 `WebUserName`/`WebPassword`）永远不由平台接管**；
//! 权限 key 按本机白名单过滤；哈希格式在 `pek_radmin` 侧校验；改完立即让该用户在线会话失效。

use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{json, Value as Json};

use crate::audit;
use crate::config::AgentConfig;
use crate::util;

/// 能力标识（平台据此判断节点是否支持统一用户下发）。
pub const CAP: &str = "panelUsers";

/// 摘要缓存（5 秒）：register 与首次心跳间隔很短，避免重复读库。
static SUMMARY: OnceLock<Mutex<(Instant, String, usize)>> = OnceLock::new();

/// 用户清单摘要（`(sha256, 数量)`；5 秒缓存）。
pub fn summary(base: &Path) -> (String, usize) {
    let cache = SUMMARY.get_or_init(|| Mutex::new((Instant::now() - Duration::from_secs(60), String::new(), 0)));
    {
        let g = cache.lock().unwrap_or_else(|e| e.into_inner());
        if g.0.elapsed() < Duration::from_secs(5) {
            return (g.1.clone(), g.2);
        }
    }
    let (digest, count) = match audit::list_users(base) {
        Ok(users) => (
            audit::users_digest(base).unwrap_or_default(),
            users.len(),
        ),
        Err(_) => (String::new(), 0),
    };
    let mut g = cache.lock().unwrap_or_else(|e| e.into_inner());
    *g = (Instant::now(), digest.clone(), count);
    (digest, count)
}

/// 变更后使摘要缓存立即失效（无需等 5 秒）。
pub fn invalidate_summary() {
    if let Some(cache) = SUMMARY.get() {
        let mut g = cache.lock().unwrap_or_else(|e| e.into_inner());
        g.0 = Instant::now() - Duration::from_secs(60);
    }
}

/// 把面板元数据并入报文：`caps`（能力）+ 用户摘要（注册/心跳）。
pub fn merge_summary(base: &Path, v: &mut Json) {
    let (digest, count) = summary(base);
    if let Some(obj) = v.as_object_mut() {
        obj.insert("caps".to_string(), json!([CAP]));
        obj.insert("panelUsersVersion".to_string(), Json::String(digest));
        obj.insert("panelUsersCount".to_string(), json!(count));
    }
}

/// 子用户清单（`panelUsers` 应答；**不含哈希/盐**）+ 可选权限清单。
pub fn list_json(base: &Path) -> Result<Json, String> {
    let users = audit::list_users(base)?;
    let items: Vec<Json> = users
        .iter()
        .map(|u| {
            json!({
                "userName": u.user_name,
                "permissions": u.permissions,
                "enabled": u.enabled,
                "remark": u.remark,
            })
        })
        .collect();
    let perms: Vec<Json> = audit::ALL_PERMISSIONS
        .iter()
        .map(|(k, n)| json!({ "key": k, "name": n }))
        .collect();
    Ok(json!({ "users": items, "permissions": perms }))
}

/// 校验并整理一条下发用户（平台传入的 camelCase JSON）。
struct Incoming {
    name: String,
    hash: String,
    salt: String,
    permissions: Vec<String>,
    enabled: bool,
    remark: String,
}

fn parse_incoming(v: &Json) -> Result<Incoming, String> {
    let name = v
        .get("userName")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if name.is_empty() || name.chars().count() > 50 || name.contains(',') || name.contains(char::is_whitespace) {
        return Err(format!("用户名不合法：{name:?}"));
    }
    let hash = v
        .get("passwordHash")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let salt = v
        .get("salt")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let permissions: Vec<String> = v
        .get("permissions")
        .and_then(|x| x.as_array())
        .map(|list| {
            list.iter()
                .filter_map(|x| x.as_str().map(|s| s.trim().to_string()))
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();
    let enabled = v.get("enabled").and_then(|x| x.as_bool()).unwrap_or(true);
    let remark = v
        .get("remark")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    Ok(Incoming {
        name,
        hash,
        salt,
        permissions,
        enabled,
        remark,
    })
}

/// 内置管理员判定（配置文件凭据；平台不接管）。
fn is_builtin(cfg: &AgentConfig, name: &str) -> bool {
    let builtin = cfg.web_user_name.trim();
    !builtin.is_empty() && builtin.eq_ignore_ascii_case(name.trim())
}

/// 新增/更新单个子用户（`panelUserUpsert`）。
pub fn upsert(base: &Path, cfg: &AgentConfig, v: &Json) -> Result<String, String> {
    let user = parse_incoming(v)?;
    if is_builtin(cfg, &user.name) {
        return Err(format!(
            "内置管理员[{}]由各节点配置文件维护，平台不接管",
            user.name
        ));
    }
    audit::save_user_hashed(
        base,
        &user.name,
        &user.hash,
        &user.salt,
        &user.permissions,
        user.enabled,
        &user.remark,
    )?;
    invalidate_summary();
    crate::webpanel::invalidate_user_cache();
    util::log_format("平台下发面板用户已保存：{}", &[&user.name]);
    Ok(user.name)
}

/// 删除单个子用户（`panelUserDelete`）。
pub fn delete(base: &Path, cfg: &AgentConfig, v: &Json) -> Result<String, String> {
    let name = v
        .get("userName")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if name.is_empty() {
        return Err("缺少用户名".to_string());
    }
    if is_builtin(cfg, &name) {
        return Err(format!("内置管理员[{name}]由各节点配置文件维护，平台不接管"));
    }
    audit::delete_user(base, &name)?;
    invalidate_summary();
    crate::webpanel::invalidate_user_cache();
    util::log_format("平台下发删除面板用户：{}", &[&name]);
    Ok(name)
}

/// 全量对齐（`panelUserSync`）：逐个 upsert；`prune=true` 时删除清单外的子用户。
///
/// 返回（新增/更新, 删除, 跳过内置管理员）。
pub fn sync(
    base: &Path,
    cfg: &AgentConfig,
    users: &[Json],
    prune: bool,
) -> Result<(usize, usize, usize), String> {
    let mut applied = 0usize;
    let mut skipped = 0usize;
    let mut incoming_names: Vec<String> = Vec::new();

    for item in users {
        let user = parse_incoming(item)?;
        if is_builtin(cfg, &user.name) {
            skipped += 1;
            continue;
        }
        incoming_names.push(user.name.to_ascii_lowercase());
        audit::save_user_hashed(
            base,
            &user.name,
            &user.hash,
            &user.salt,
            &user.permissions,
            user.enabled,
            &user.remark,
        )?;
        applied += 1;
    }

    let mut removed = 0usize;
    if prune {
        let existing = audit::list_users(base)?;
        for user in existing {
            if is_builtin(cfg, &user.user_name) {
                continue;
            }
            if !incoming_names.iter().any(|n| n == &user.user_name.to_ascii_lowercase()) {
                audit::delete_user(base, &user.user_name)?;
                removed += 1;
            }
        }
    }

    invalidate_summary();
    if applied > 0 || removed > 0 {
        crate::webpanel::invalidate_user_cache();
    }
    util::log_format(
        "平台全量同步面板用户：应用 {} 个，删除 {} 个，跳过内置 {} 个",
        &[
            applied.to_string().as_str(),
            removed.to_string().as_str(),
            skipped.to_string().as_str(),
        ],
    );
    Ok((applied, removed, skipped))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AgentConfig;
    use std::path::PathBuf;

    fn temp_base(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "ragent-panel-users-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    fn hash(salt: &str, pw: &str) -> String {
        dhrust::sign::salted_sha256_hex(salt, pw)
    }

    #[test]
    fn upsert_list_delete_and_digest() {
        let base = temp_base("upsert");
        let cfg = AgentConfig::default();

        let salt = "aabbccdd";
        let v = json!({
            "userName": "alice",
            "passwordHash": hash(salt, "pw123"),
            "salt": salt,
            "permissions": ["services", "logs", "bogus"],
            "enabled": true,
            "remark": "售后",
        });
        assert_eq!(upsert(&base, &cfg, &v).unwrap(), "alice");

        // 权限白名单过滤
        let user = audit::find_user(&base, "alice").unwrap().unwrap();
        assert_eq!(user.permissions, vec!["services".to_string(), "logs".to_string()]);
        assert!(user.verify("pw123"));
        let list = list_json(&base).unwrap();
        assert_eq!(list["users"].as_array().unwrap().len(), 1);
        let (digest1, count) = {
            invalidate_summary();
            summary(&base)
        };
        assert_eq!(count, 1);
        assert!(!digest1.is_empty());

        // 更新不留密码 = 不改密码
        let v2 = json!({ "userName": "alice", "permissions": ["services"], "enabled": false });
        upsert(&base, &cfg, &v2).unwrap();
        let user = audit::find_user(&base, "alice").unwrap().unwrap();
        assert!(!user.enabled);
        assert!(user.verify("pw123"), "未提供新哈希时应保留原密码");

        // 删除
        delete(&base, &cfg, &json!({ "userName": "alice" })).unwrap();
        assert!(audit::find_user(&base, "alice").unwrap().is_none());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn builtin_admin_is_protected() {
        let base = temp_base("builtin");
        let cfg = AgentConfig {
            web_user_name: "boss".to_string(),
            ..Default::default()
        };
        let v = json!({ "userName": "BOSS", "passwordHash": hash("s", "p"), "salt": "s" });
        let err = upsert(&base, &cfg, &v).unwrap_err();
        assert!(err.contains("内置管理员"), "{err}");
        let err = delete(&base, &cfg, &json!({ "userName": "boss" })).unwrap_err();
        assert!(err.contains("内置管理员"), "{err}");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn sync_applies_and_prunes() {
        let base = temp_base("sync");
        let cfg = AgentConfig {
            web_user_name: "admin".to_string(),
            ..Default::default()
        };
        upsert(
            &base,
            &cfg,
            &json!({ "userName": "u1", "passwordHash": hash("s1", "p1"), "salt": "s1" }),
        )
        .unwrap();
        upsert(
            &base,
            &cfg,
            &json!({ "userName": "u2", "passwordHash": hash("s2", "p2"), "salt": "s2" }),
        )
        .unwrap();

        let users = vec![
            json!({ "userName": "u1", "passwordHash": hash("s3", "p3"), "salt": "s3", "permissions": ["logs"] }),
            json!({ "userName": "u3", "passwordHash": hash("s4", "p4"), "salt": "s4" }),
            json!({ "userName": "admin", "passwordHash": hash("s5", "p5"), "salt": "s5" }),
        ];
        let (applied, removed, skipped) = sync(&base, &cfg, &users, true).unwrap();
        assert_eq!(applied, 2);
        assert_eq!(removed, 1, "u2 不在清单内应被清理");
        assert_eq!(skipped, 1, "内置管理员应跳过");

        assert!(audit::find_user(&base, "u2").unwrap().is_none());
        assert!(audit::find_user(&base, "u3").unwrap().is_some());
        assert!(audit::find_user(&base, "u1").unwrap().unwrap().verify("p3"));

        // 不 prune 时保留多余用户
        let (_, removed, _) = sync(&base, &cfg, &[json!({ "userName": "u1" })], false).unwrap();
        assert_eq!(removed, 0);
        assert!(audit::find_user(&base, "u3").unwrap().is_some());
        let _ = std::fs::remove_dir_all(&base);
    }
}
