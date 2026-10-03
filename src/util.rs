//! 基础辅助：基础目录、路径归一化、日志初始化。
//!
//! 2026-10-01 审计：通用工具（原子写/通配匹配/参数切分/环境变量解析/尾读/最新文件/文件 MD5）
//! 已下沉 `dhrust::io` / `dhrust::sign`，调用点直连 dhrust；本文件保留 StarAgent 专属逻辑。
//!
//! 与 C# StarAgent 对齐：基础目录即程序所在目录（`".".GetBasePath()`），
//! 启动时把当前目录切换到基础目录，配置中的相对路径（如 `../apps/xxx`）据此解析。

use std::path::{Path, PathBuf};
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

/// 词法归一化路径（不访问文件系统，可处理尚不存在的路径；消除 `.` 与 `..`）。
/// 实现已下沉 `dhrust::io::lexical_normalize`（2026-10-03）。
pub use dhrust::io::lexical_normalize;

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


/// JSON 字符串转义（不含两端引号）。
pub fn escape_json(text: &str) -> String {
    dhrust::web::json_escape(text)
}







#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lexical_normalize_reexport_works() {
        // 实现已下沉 dhrust::io；此处验证再导出可用（用例细节在 dhrust 侧）
        assert_eq!(
            lexical_normalize(Path::new("a/b/../c")),
            PathBuf::from("a/c")
        );
    }
}
