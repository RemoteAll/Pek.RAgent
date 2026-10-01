# Pek.RAgent 多平台支持规划

> 建档：2026-10-01。背景：项目后续需要支持（部署到）各种操作系统与硬件环境——国产化 Linux、嵌入式/路由系统、更多 CPU 架构等。
> 说明：若"路由中的各种系统"另指星尘生态的 StarGateway 域名路由/消息路由场景，请告知，本规划可再调整（见文末 §6）。

## 1. 能力现状（2026-10-01）

| 能力 | 状态 |
|------|------|
| Windows 服务化 | ✅ SCM 全生命周期本机实测通过 |
| Linux 服务化 | 🟡 systemd / OpenWrt procd / SysV·OpenRC 三种适配已实现（自动探测 + `PEK_RAGENT_INIT` 可强制），**未实机验证** |
| macOS 服务化（launchd） | 🟡 代码就绪，**未实机验证** |
| 构建产物 | ✅ Windows x86_64（MSVC）、Linux 静态 musl **x86_64 / aarch64 / riscv64 / loongarch64**，由 `scripts\build-release.ps1` 一键产出 |
| 运行时依赖 | 静态 musl 单文件：不依赖 glibc，主流发行版（含麒麟/统信/openEuler/Alpine 等）理论通吃 |
| 平台适配结构 | `service/`（windows / systemd / launchd / unsupported 兜底）+ `sys.rs` 平台分支；新增平台 = 新增适配器 |

## 2. 目标平台矩阵与差距

| 类别 | 代表系统 | 差距 | 成本 |
|------|----------|------|------|
| 国产化 Linux x86_64 | 麒麟（Kylin/OpenKylin）、统信 UOS、openEuler、Anolis | 无需改码，**实机验证 + systemd 单元细节走查** | 低 |
| 国产化/服务器 ARM64 | 鲲鹏、飞腾、树莓派 | ✅ 交叉产物已打通（aarch64-unknown-linux-musl）；待实机验证 | 低 |
| 国产化/新兴架构 | RISC-V（赛昉/进迭时空等）、龙芯 LoongArch | ✅ 交叉产物已打通（riscv64gc / loongarch64 musl）；待实机验证 | 低 |
| 嵌入式/路由系统 | OpenWrt（procd）、Buildroot（无 init） | **无 systemd**：需新增 init 适配（procd/OpenRC/SysV）；注意只读文件系统与低内存 | 中 |
| 老发行版 | CentOS 7 等（老 glibc） | 静态 musl 不依赖 glibc，通常无碍；如遇特殊情况可加 gnu 产物（zig 支持指定 glibc 版本，如 `gnu.2.17`） | 低 |
| macOS | macOS 12+（x86_64/arm64） | launchd 代码已有：**需 macOS 实机或 SDK** 才能产物与验证 | 中 |
| 自研嵌入式 | SmartOS / SmartA2 / SmartA4 | 需先确认目标三元组与 libc、服务化方式 | 待调研 |

## 3. 实现要点

### 3.1 构建与发布（已落地）

- `scripts\build-release.ps1` 多目标：`-Targets all|windows|linux|linux-arm64`（`all` = 三平台全出）
- 产物 `dist\`：Windows zip、x86_64/ARM64 musl tar.gz（带执行位）、`SHA256SUMS.txt`
- 已支持架构：x86_64 / aarch64 / **riscv64（RISC-V）** / **loongarch64（龙芯）**（均为 musl 静态）
- 新增架构流程：`rustup target add <triple>` → 脚本 `$linuxTargets` 加一行 → 重跑（zig 0.16 支持该架构即可）；`-Targets` 支持多选（如 `-Targets linux,linux-arm64`）

### 3.2 服务化适配（嵌入式是关键增量）✅ 已实现（2026-10-01）

探测顺序：systemd（`/run/systemd/system`）→ OpenWrt procd（`/etc/rc.common` / `/sbin/procd`）→ SysVinit/OpenRC（`/etc/init.d`）；可用 `PEK_RAGENT_INIT=systemd|procd|sysv` 强制指定。

- `src/service/linux.rs`（探测与分发 + 公共辅助）、`procd.rs`、`sysv.rs`、`inits.rs`（脚本模板与决策，**带单测**，共 22 项测试全绿）
- 安装命令统一走 `ServiceManager`，未识别 init 时给出提示（可先用 `-run`）
- **待实机验证**：x86 OpenWrt 虚拟机（procd）→ 真机；Alpine/老系统（SysV·OpenRC）

### 3.3 系统识别（对接星尘时使用）

- C# 侧已有 `Stardust.Models.OSKinds`：Windows 各版本 / Debian 系 / RedHat 系 / 国产 Linux（Deepin、UOS、Kylin、openEuler、Anolis、TencentOS…）/ Alpine / OpenWrt / Buildroot / macOS / Android / SmartOS
- 若需向 StarServer 上报系统类型与资产分类：将 `OSKindHelper.Parse` 语义移植为 Rust（Linux 读 `/etc/os-release`，Windows 读注册表版本号）

### 3.4 运行约束（嵌入式/路由设备）

- 体积：strip 后 x86_64 ≈ 2.5 MB、ARM64 ≈ 2.3 MB——对常见路由设备可接受
- 低配模式：可放大 `GuardPeriod`、减少日志频率；影子目录需要可写路径（`/tmp` 或 overlay 分区）
- 只读根文件系统：配置/日志/数据目录后续应支持自定义路径（小改动）

## 4. 验证清单（建议）

| 平台 | 方式 |
|------|------|
| 麒麟 / UOS / openEuler | 虚拟机装系统 → 拷贝 musl 包 → `-run` 冒烟 → `-install` 服务化 → 重启机器验证自启 |
| ARM64（树莓派/鲲鹏） | 官方系统烧录或云主机 → 同上 |
| OpenWrt | 先用 x86 版 OpenWrt 虚拟机验证 procd 适配，再上真机 |
| macOS | 真机 `-run` + `launchctl` 验证 |

## 5. 优先级建议

1. **P0**：国产化 Linux x86_64 实机验证（零代码、补"未实机"空白）
2. **P1**：ARM64 / RISC-V / 龙芯实机验证（产物已就绪，直接可测）
3. **P2**：OpenWrt / Buildroot 实机验证（procd/SysV 适配已实现，x86 OpenWrt 虚拟机起步）
4. **P3**：macOS / SmartOS（取决于资源与需求）

## 6. 与星尘生态的关系（待确认）

- `OSKinds` 中的"嵌入式/路由器系统"（OpenWrt/Buildroot）与国产 Linux，即本规划重点覆盖对象
- 如果"各种系统"还包括 **StarGateway（域名路由/流量转发）场景联动** 或 StarServer 消息路由对接，属另一条线（README §11 已有"StarServer / StarWeb 对接"），可另立设计文档
