# Pek.RAgent 协作指令（Rust）

> 组织级通用规范以**用户级指令**（`%APPDATA%\Code\User\prompts\跨项目接续协议.instructions.md`，随 Settings Sync 漫游）与 Pek.Skills 资产为准；本文件只写本仓库特有事实，简体中文回复。

## 项目
- Pek.RAgent＝星尘代理 Rust 实现（crate `pek-ragent`，edition 2024；GitHub `RemoteAll/Pek.RAgent`，分支 main）：多应用守护、**影子目录部署（默认）**、跨平台服务管理与本地 HTTP 控制接口。
- Windows 服务名 `StarAgentRust`（与 C# StarAgent 5500 并存，默认端口 **5501**）；配置 `Config/StarAgent.config`（XML，与 C# 双端互通）。
- 版本标记 `PEK_RAGENT_VERSION_TAG`（结尾 `Pek.RAgent/<版本>`）**禁止删除或改名**——Pek.RPanlServer「代理发行」上传识别依赖。

## 关键机制（改动前必读）
- **影子模式**：单文件程序复制到 `shadow/{名称}-{md5前8位}/` 运行，源文件不被占用 → 直接覆盖源文件即自动升级（5s 检测 → 重建影子 → 重启）；升级惯例见用户级指令。
- **停止链**：先温和后强制、覆盖进程树（Windows `taskkill /PID /T` → 3s → `/T /F` → 2s 终检；Unix 组长按进程组）；【接管】仅基于 `data/state.json` 记录的 PID）。
- **多实例**：配置按名称 upsert（**不能同名**）；多实例用不同子服务名分别注册（各自目录/配置/资源）。停止按 PID 定点，无同名连坐。
- 自升级管线：热检测 + 影子冒烟 + 原子替换 + 移交（Unix `execv` 原地接管；Windows 服务交 SCM 失败恢复）。
- **WAF（Pek.RWaf）**：HTTP 控制接口挂 `pek_rwaf` 中间件（`server.rs::build_router` 首行）——管理端预设（拦恶意爬虫/sqlmap/curl 默认 UA；路径探测拦截；CC 300/分钟）；`Config/Waf.json` 首启生成 + 10s 热重载；`/star/db|file|ai` 经 `skipAttackPrefixes` 跳过攻击检测（业务护栏负责）。**网络层 curl 实测须带浏览器 UA**（回环豁免）。

## 构建与验证
- `cargo build --release`；`cargo test`；打包 `powershell -ExecutionPolicy Bypass -File scripts/build-release.ps1`（**默认自动递升补丁号**，`-NoBump` 关闭；版本守卫取自 `../DH.RustBase/tools/version-guard.ps1`；`-Targets` 默认 all = windows + 4 个 Linux musl 目标；zig 自动发现 `E:\Soft\zig-*`）。
- 依赖均为 path：`dhrust=../DH.RustBase`、`pek-rcode=../Pek.RCode`、`pek-radmin=../Pek.RAdmin`、`pek-rwaf=../Pek.RWaf`——**编译报缺 API/疑似落后时先在本机拉新这几库再复测**（用户级指令「源码依赖自动拉新」）。
- 中文注释优先；含中文 `.ps1` 必须 UTF-8 with BOM；不搞无关全仓格式化。

## 流程
- 每轮收尾三件套：① 决策/台账新增节（**含用户原话**）② `tools/continuity/sync-memory.ps1` 回灌 `docs/项目记忆.md` ③ 回归验证如实汇报（贴实际输出）。
- `docs/` 台账随仓库走（已入 git）；`docs/项目接续指南.md` 为开工先读。
- GitHub 推送走 IP 直连（本机 hosts 劫持）：`git -c http.curloptResolve=github.com:443:<IP> push origin main`（IP 用 `Resolve-DnsName github.com -Server 223.5.5.5` 获取）。
