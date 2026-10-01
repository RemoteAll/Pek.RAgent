//! 基础辅助：基础目录、路径归一化、日志初始化、安全写文件、通配匹配、参数切分。
//!
//! 与 C# StarAgent 对齐：基础目录即程序所在目录（`".".GetBasePath()`），
//! 启动时把当前目录切换到基础目录，配置中的相对路径（如 `../apps/xxx`）据此解析。

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use dhrust::logs::{ConsoleLog, CompositeLog, ILog, LogLevel, TextFileLog};

/// 基础目录：环境变量 `PEK_RAGENT_BASE` 优先（便于开发调试），其次可执行文件目录，最后当前目录。
pub fn base_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("PEK_RAGENT_BASE") {
        let dir = dir.trim();
        if !dir.is_empty() {
            return lexical_normalize(Path::new(dir));
        }
    }

    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            return parent.to_path_buf();
        }
    }

    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// 词法归一化路径。不访问文件系统，可处理尚不存在的路径；消除 `.` 与 `..`。
pub fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => match out.components().next_back() {
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                _ => out.push(".."),
            },
            other => out.push(other.as_os_str()),
        }
    }

    if out.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        out
    }
}

/// 相对路径按基础目录解析为绝对路径。
pub fn resolve(base: &Path, path: &str) -> PathBuf {
    let p = Path::new(path.trim());
    if p.is_absolute() {
        lexical_normalize(p)
    } else {
        lexical_normalize(&base.join(p))
    }
}

/// 初始化全局日志：控制台 + 文件（`Log/` 目录，按天一个文件）。
///
/// 行格式 `HH:mm:ss.fff 线程ID 类型 名称 正文` 与文件头字段均对齐 DH.NCore
/// （由 dhrust::logs 实现）；服务模式（无控制台）下控制台日志写不出去但不影响文件日志。
/// Windows 下启用 UTF-8 代码页，保证中文菜单与日志正常。
pub fn init_logging(base: &Path, use_console: bool, level: LogLevel) {
    #[cfg(windows)]
    dhrust::logs::enable_windows_console();

    let mut logs: Vec<Arc<dyn ILog>> = Vec::new();
    if use_console {
        logs.push(Arc::new(ConsoleLog::with_color(true)));
    }
    logs.push(TextFileLog::create(base.join("Log")) as Arc<dyn ILog>);

    dhrust::logs::set_log(Arc::new(CompositeLog::new(logs)));
    dhrust::logs::set_level(level);
}

/// 写信息日志。
pub fn log_info(message: &str) {
    dhrust::logs::write_line(message);
}

/// 写错误日志。
pub fn log_error(message: &str) {
    dhrust::logs::log().error(message);
}

/// 带占位符格式化的信息日志（`{}` 占位）。
pub fn log_format(template: &str, args: &[&str]) {
    let mut msg = String::with_capacity(template.len() + 32);
    let mut parts = template.split("{}");
    if let Some(first) = parts.next() {
        msg.push_str(first);
    }
    for (i, part) in parts.enumerate() {
        msg.push_str(args.get(i).copied().unwrap_or("?"));
        msg.push_str(part);
    }
    log_info(&msg);
}

/// 原子写文本文件：先写 `.tmp` 再改名替换，避免中途失败留下半截内容。
pub fn write_file_atomic(path: &Path, text: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let tmp = PathBuf::from(format!("{}.tmp", path.display()));
    std::fs::write(&tmp, text)?;

    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(_) => {
            // 目标被占用时降级为直接写入（尽力而为），并清理临时文件
            let rs = std::fs::write(path, text);
            let _ = std::fs::remove_file(&tmp);
            rs
        }
    }
}

/// JSON 字符串转义（不含两端引号）。
pub fn escape_json(text: &str) -> String {
    dhrust::web::json_escape(text)
}

/// 简单通配匹配：`*`（任意串）与 `?`（单字符），大小写不敏感。
pub fn wildcard_match(pattern: &str, text: &str) -> bool {
    fn matches(p: &[char], t: &[char]) -> bool {
        if p.is_empty() {
            return t.is_empty();
        }

        match p[0] {
            '*' => (0..=t.len()).any(|i| matches(&p[1..], &t[i..])),
            '?' => !t.is_empty() && matches(&p[1..], &t[1..]),
            c => {
                !t.is_empty()
                    && c.eq_ignore_ascii_case(&t[0])
                    && matches(&p[1..], &t[1..])
            }
        }
    }

    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    matches(&p, &t)
}

/// 拆分命令行参数字符串（支持双引号包裹；与 C# ProcessStartInfo 的常见用法对齐）。
///
/// 例：`urls=http://*:8080 "a b" c` → `["urls=http://*:8080", "a b", "c"]`
pub fn split_args(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    let mut has_token = false;

    for c in text.chars() {
        match c {
            '"' => {
                in_quotes = !in_quotes;
                has_token = true;
            }
            ' ' | '\t' if !in_quotes => {
                if has_token {
                    out.push(std::mem::take(&mut cur));
                    has_token = false;
                }
            }
            _ => {
                cur.push(c);
                has_token = true;
            }
        }
    }

    if has_token {
        out.push(cur);
    }

    out
}

/// 解析 `k=v;k2=v2` 形式的环境变量串。
pub fn parse_environments(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for item in text.split(';') {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        if let Some(p) = item.find('=') {
            let key = item[..p].trim();
            let value = item[p + 1..].trim();
            if !key.is_empty() {
                out.push((key.to_string(), value.to_string()));
            }
        }
    }
    out
}

/// 计算文件 MD5（十六进制小写）。与 C# `fi.MD5().ToHex()` 对齐。
pub fn md5_file(path: &Path) -> std::io::Result<String> {
    use md5::{Digest, Md5};
    use std::io::Read;

    let mut file = std::fs::File::open(path)?;
    let mut hasher = Md5::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }

    Ok(format!("{:x}", hasher.finalize()))
}

/// 日志目录中最新（文件名最大）的 `.log` 文件。
pub fn latest_log_file(dir: &Path) -> Option<PathBuf> {
    let mut best: Option<(String, PathBuf)> = None;
    let entries = std::fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.to_ascii_lowercase().ends_with(".log") {
            continue;
        }
        if best.as_ref().map(|(b, _)| name > *b).unwrap_or(true) {
            best = Some((name, path));
        }
    }
    best.map(|(_, path)| path)
}

/// 读取文件尾部若干行（整文件读入；日志文件规模下可接受）。
pub fn read_tail(path: &Path, count: usize) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(count);
    lines[start..].iter().map(|s| s.to_string()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lexical_normalize_handles_parent() {
        assert_eq!(lexical_normalize(Path::new("a/b/../c")), PathBuf::from("a/c"));
        assert_eq!(lexical_normalize(Path::new("../apps/x")), PathBuf::from("../apps/x"));
    }

    #[test]
    fn split_args_basic() {
        let rs = split_args("urls=http://*:8080 \"a b\" c");
        assert_eq!(rs, vec!["urls=http://*:8080", "a b", "c"]);
    }

    #[test]
    fn parse_envs_basic() {
        let rs = parse_environments("A=1; B=hello world ;broken;C=");
        assert_eq!(
            rs,
            vec![
                ("A".to_string(), "1".to_string()),
                ("B".to_string(), "hello world".to_string()),
                ("C".to_string(), String::new()),
            ]
        );
    }

    #[test]
    fn wildcard_matches() {
        assert!(wildcard_match("app.*", "APP.exe"));
        assert!(wildcard_match("*", "anything"));
        assert!(!wildcard_match("app.*", "xapp.exe"));
    }

    #[test]
    fn log_tail_and_latest_file() {
        let dir = std::env::temp_dir().join(format!(
            "ragent-util-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("2026_01_01.log"), "a\nb\nc\n").unwrap();
        std::fs::write(dir.join("2026_01_02.log"), "x\ny\n").unwrap();
        std::fs::write(dir.join("ignore.txt"), "no\n").unwrap();

        let latest = latest_log_file(&dir).expect("应找到最新日志");
        assert!(latest.ends_with("2026_01_02.log"));
        assert_eq!(read_tail(&latest, 1), vec!["y".to_string()]);
        assert_eq!(read_tail(&latest, 10).len(), 2);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
