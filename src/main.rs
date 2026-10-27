//! Pek.RAgent：星尘代理（StarAgent 的 Rust 实现）。
//!
//! 入口职责：确定基础目录、切换工作目录、初始化日志、预处理参数（zip 绝对化）、分发命令。
//! 详见 `README.md` 与 `Doc/`。

// `serde_json::json!` 大对象（status 响应字段较多）需要更高的宏递归上限
#![recursion_limit = "256"]

mod agent;
mod ai;
mod app;
mod apppkg;
mod audit;
mod certdeploy;
mod cli;
mod config;
mod deploy;
mod fileman;
mod history;
mod iis;
mod logclean;
mod manager;
mod memsys;
mod migrate;
mod panel_ws;
mod panel_users;
mod plugins;
mod portstat;
mod sampler;
mod self_upgrade;
mod server;
mod service;
mod sites;
mod swap;
mod sys;
mod terminal;
mod tlscfg;
mod udp_rpc;
mod util;
mod weblog;
mod webpanel;

use std::path::{Path, PathBuf};

/// 版本标记字符串源（结尾 NUL 便于平台字节扫描精确截断）。
const VERSION_TAG_STR: &str = concat!("Pek.RAgent/", env!("CARGO_PKG_VERSION"), "\0");

/// 版本标记（编译期嵌入二进制的字节常量，供 Pek.RPanlServer「代理发行」上传时自动识别版本号）。
/// 格式：`Pek.RAgent/<版本>`；请勿删除或改名。
#[used]
#[unsafe(no_mangle)]
pub static PEK_RAGENT_VERSION_TAG: [u8; VERSION_TAG_STR.len()] = {
    let src = VERSION_TAG_STR.as_bytes();
    let mut buf = [0u8; VERSION_TAG_STR.len()];
    let mut i = 0;
    while i < src.len() {
        buf[i] = src[i];
        i += 1;
    }
    buf
};

fn main() {
    // 引用防 fat LTO 裁剪（版本标记供平台识别上传包版本）
    let _ = std::hint::black_box(&PEK_RAGENT_VERSION_TAG);
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
    install_panic_logging();

    let code = cli::run(&args, &base);
    // 异步文件日志同步落盘后再退出（否则最后若干条日志可能在队列中丢失，
    // 例如：重启助手进程的"服务已拉起"结论、CLI 升级的最终提示）
    dhrust::logs::flush();
    std::process::exit(code);
}

/// 全局 panic 钩子：任何线程 panic 都写入日志（线程名/位置/消息）。
///
/// Rust 默认只在 stderr 打印；服务模式下 stderr 可能无人接收，后台线程 panic 会变成
/// “进程还活着但某个功能永久消失”（例如平台实时通道离线）。记录下来才能定位。
fn install_panic_logging() {
    std::panic::set_hook(Box::new(|info| {
        let thread = std::thread::current();
        let name = thread.name().unwrap_or("unnamed");
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "未知位置".to_string());
        let payload = if let Some(s) = info.payload().downcast_ref::<&str>() {
            (*s).to_string()
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "非字符串 panic".to_string()
        };
        let line = format!("未捕获 panic（线程 {name}，{location}）：{payload}");
        util::log_error(&line);
        // 控制台也输出一份（前台/调试模式可见；日志失败时仍有兜底）
        eprintln!("{line}");
    }));
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
