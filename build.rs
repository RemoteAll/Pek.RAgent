//! 构建脚本：注入构建时间（`-status` 的"发布"行，对齐 C# 程序集发布时间展示）。
//!
//! 不输出任何 `rerun-if` 指令：Cargo 会在每次构建时重跑本脚本，保证时间始终准确。

fn main() {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    // 运行时由 cli::build_time_text 转为本地时间文本
    println!("cargo:rustc-env=PEK_RAGENT_BUILD_UNIX={secs}");
}
