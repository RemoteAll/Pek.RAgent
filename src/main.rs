//! Pek.RAgent：星尘代理（StarAgent 的 Rust 实现）。
//!
//! 入口职责：确定基础目录、切换工作目录、初始化日志、预处理参数（zip 绝对化）、分发命令。
//! 详见 `README.md` 与 `Doc/`。

// `serde_json::json!` 大对象（status 响应字段较多）需要更高的宏递归上限
#![recursion_limit = "256"]

mod agent;
mod app;
mod cli;
mod config;
mod deploy;
mod history;
mod manager;
mod netc;
mod portstat;
mod sampler;
mod server;
mod service;
mod sys;
mod udp_rpc;
mod util;
mod weblog;
mod webpanel;

use std::path::{Path, PathBuf};

fn main() {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let base = util::base_dir();
    let original = std::env::current_dir().unwrap_or_else(|_| base.clone());

    // zip 参数在切换目录前绝对化（相对路径优先按调用目录）
    let args = absolutize_zip_args(raw, &original);

    // 统一工作目录到基础目录（与 C# StarAgent 一致，配置中的相对路径据此解析）
    let _ = std::env::set_current_dir(&base);

    // 日志：控制台 + 文件（Log/，行格式与文件头对齐 DH.NCore）；级别取 RUST_LOG
    let level = dhrust::logs::level_from_env();
    util::init_logging(&base, true, level);

    let code = cli::run(&args, &base);
    // 异步文件日志同步落盘后再退出（否则最后若干条日志可能在队列中丢失，
    // 例如：重启助手进程的"服务已拉起"结论、CLI 升级的最终提示）
    dhrust::logs::flush();
    std::process::exit(code);
}

/// 把 zip 参数绝对化（相对路径优先按调用目录，其次保持原样由部署层兜底）。
fn absolutize_zip_args(args: Vec<String>, cwd: &Path) -> Vec<String> {
    args.into_iter()
        .map(|a| {
            if a.to_ascii_lowercase().ends_with(".zip") {
                let p = Path::new(&a);
                if !p.is_absolute() {
                    let candidate: PathBuf = cwd.join(p);
                    if candidate.is_file() {
                        return candidate.to_string_lossy().into_owned();
                    }
                }
            }
            a
        })
        .collect()
}
