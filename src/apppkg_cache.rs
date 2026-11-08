//! 应用包本地缓存：同一 SHA-256 只下载一次。
//!
//! 为什么需要它（2026-11-08，用户提出「不能重复下载」）：
//! 部署链路里 `fetch_package` 原本每次都从平台全量拉包。以下场景会白烧流量：
//! - 部署失败后重试（同一版本反复拉）
//! - 回滚到上一版
//! - 同一台节点上有多个实例共用同一个包
//!
//! 缓存以**内容哈希**为键（不是版本号）：包被重新打包但版本号不变时，哈希必然不同，
//! 不会误用旧包；反过来同内容不同版本号时也能命中。
//!
//! 命中时会**重新校验 SHA-256**（防磁盘损坏/人为篡改），不一致就丢弃重下。
//!
//! 清理策略：最多保留 [`MAX_FILES`] 个、累计不超过 [`MAX_BYTES`]，按最后访问时间淘汰。

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// 缓存总大小上限（超出后按最后访问时间删最旧的）
const MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024; // 2 GB
/// 最多保留的包个数
const MAX_FILES: usize = 20;
/// 超过这个时间没被访问的包直接清掉（默认 30 天）
const MAX_AGE: Duration = Duration::from_secs(30 * 24 * 3600);

/// 缓存目录（`{base}/cache/packages`）。
pub fn dir() -> PathBuf {
    crate::util::base_dir().join("cache").join("packages")
}

/// 取缓存；命中返回包内容，未命中/校验失败返回 `None`。
pub fn get(sha256: &str) -> Option<Vec<u8>> {
    get_in(&dir(), sha256)
}

/// 写入缓存（失败只记日志，不影响部署主流程）。
pub fn put(sha256: &str, body: &[u8]) {
    put_in(&dir(), sha256, body);
}

/// 缓存根目录下的条目路径（键必须是 64 位 hex，防路径穿越）。
fn entry_path(dir: &Path, sha256: &str) -> Option<PathBuf> {
    if sha256.len() != 64 || !sha256.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some(dir.join(sha256.to_ascii_lowercase()))
}

pub fn get_in(dir: &Path, sha256: &str) -> Option<Vec<u8>> {
    let path = entry_path(dir, sha256)?;
    let body = std::fs::read(&path).ok()?;
    // 内容校验：磁盘损坏或文件被替换时不能拿脏包去部署
    if !dhrust::sign::sha256_hex(&body).eq_ignore_ascii_case(sha256) {
        crate::util::log_format(
            "应用包缓存校验失败，已丢弃并重新下载：{}…",
            &[&sha256[..12]],
        );
        let _ = std::fs::remove_file(&path);
        return None;
    }
    // 命中即刷新访问时间（LRU 依据）
    if let Ok(f) = std::fs::OpenOptions::new().write(true).open(&path) {
        let _ = f.set_modified(SystemTime::now());
    }
    Some(body)
}

pub fn put_in(dir: &Path, sha256: &str, body: &[u8]) {
    let Some(path) = entry_path(dir, sha256) else {
        return;
    };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    if let Err(e) = dhrust::io::write_all_bytes_atomic(&path, body, false) {
        crate::util::log_format("应用包缓存写入失败（不影响本次部署）：{}", &[&e.to_string()]);
        return;
    }
    prune(dir);
}

/// 按「保留最近 N 个 + 总量上限 + 过期清理」淘汰。
fn prune(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let now = SystemTime::now();
    let mut items: Vec<(PathBuf, u64, SystemTime)> = Vec::new();
    for e in entries.flatten() {
        let Ok(md) = e.metadata() else { continue };
        if !md.is_file() {
            continue;
        }
        let at = md.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        items.push((e.path(), md.len(), at));
    }
    // 新 → 旧
    items.sort_by(|a, b| b.2.cmp(&a.2));
    let mut total = 0u64;
    for (i, (path, size, at)) in items.iter().enumerate() {
        let expired = now.duration_since(*at).map(|d| d > MAX_AGE).unwrap_or(false);
        total += *size;
        if i >= MAX_FILES || total > MAX_BYTES || expired {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "ragent-pkgcache-{tag}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn put_then_get_roundtrip() {
        let dir = temp_dir("roundtrip");
        let body = b"PK\x03\x04demo-package-content".to_vec();
        let sha = dhrust::sign::sha256_hex(&body);
        assert!(get_in(&dir, &sha).is_none(), "首次应为未命中");
        put_in(&dir, &sha, &body);
        assert_eq!(get_in(&dir, &sha).unwrap(), body, "写入后应命中且内容一致");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_entry_is_dropped() {
        let dir = temp_dir("corrupt");
        let body = b"real-package".to_vec();
        let sha = dhrust::sign::sha256_hex(&body);
        put_in(&dir, &sha, &body);
        // 人为改坏缓存文件：命中时必须校验出来并丢弃，不能把脏包拿去部署
        std::fs::write(dir.join(sha.to_ascii_lowercase()), b"tampered").unwrap();
        assert!(get_in(&dir, &sha).is_none(), "内容不符应视为未命中");
        assert!(
            !dir.join(sha.to_ascii_lowercase()).exists(),
            "校验失败的缓存文件应被删除"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_invalid_key() {
        let dir = temp_dir("badkey");
        assert!(get_in(&dir, "../../evil").is_none());
        put_in(&dir, "not-a-hash", b"x");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0, "非法键不得落盘");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn prune_keeps_newest_only() {
        let dir = temp_dir("prune");
        // 造 MAX_FILES + 3 个包，逐个设置递增的 mtime
        for i in 0..(MAX_FILES + 3) {
            let body = format!("pkg-{i}").into_bytes();
            let sha = dhrust::sign::sha256_hex(&body);
            put_in(&dir, &sha, &body);
            let path = dir.join(sha.to_ascii_lowercase());
            let f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
            let t = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000 + i as u64 * 60);
            f.set_modified(t).unwrap();
        }
        prune(&dir);
        let left = std::fs::read_dir(&dir).unwrap().count();
        assert!(left <= MAX_FILES, "应只保留 {MAX_FILES} 个，实际 {left}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
