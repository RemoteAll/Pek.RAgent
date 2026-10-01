# Pek.RAgent —— 星尘代理（StarAgent 的 Rust 实现）

部署在每台应用服务器 / 边缘节点上的节点守护代理，以系统服务方式运行，负责**多应用进程守护、影子目录部署、运行中文件替换与本地控制接口**。

当前版本**暂不对接 StarServer / StarWeb**，定位为独立可用的本地代理；`DHDeploy` 所需的本地命令接口（`localhost:5500` 的 `RestartService / StartService / StopService`）已完整提供。

---

## 1. 功能特性

| 功能 | 说明 |
|------|------|
| 跨平台服务化 | Windows 服务（SCM）/ Linux systemd / macOS launchd；支持菜单与命令行安装、卸载、启动、停止、重启、状态查询 |
| 多应用守护 | 配置驱动；自动拉起、退出自动重拉、失败退避、最大失败次数（默认 20 次后放弃） |
| 影子目录部署（默认） | 应用解压到 `{工作目录}/../shadow/{名称}-{zip的MD5前8位}` 中运行；工作目录保持干净（仅放配置与数据文件） |
| **运行中文件可替换** | 可执行文件运行在影子目录，工作目录中的部署包/文件在应用运行期间**可随意上传覆盖**，不会被占用；重启后自动切换到新影子目录并清理旧版 |
| 安全文件替换 | 目标文件被占用时先改名为 `*.del` 再写入新文件（Windows 允许重命名运行中的文件），应用停止后自动清理 |
| 内存限制 | `MaxMemory` 超限自动重启应用（Windows 读私有内存、Linux 读 `/proc`） |
| 文件变动重启 | `ReloadOnChange=true` 时按 5 秒周期监视 `*.dll;*.exe;*.zip;*.jar`，变更后停止应用，稳定 `Delay` 毫秒后重启 |
| 健康检查 | 启动后按 `HealthCheck`（http/tcp 地址）探测，失败仅记录日志（对齐 C# 行为） |
| 看门狗 | 应用通过 `GET /Ping?processId=&watchdogTimeout=` 喂狗，超时未喂自动重启对应应用 |
| 进程接管 | 代理重启后接管仍存活的子进程（`data/state.json`），**不会重复拉起** |
| 本地 HTTP 控制接口 | 默认 `127.0.0.1:5500`，兼容 DHDeploy 的调用契约；仅本机访问（可配） |
| 位置参数 zip 拉起 | `pek-ragent app.zip urls=http://*:8080`（影子目录运行的一次性应用） |
| 配置热更新 | `Config/Agent.json` 被外部修改后自动重新加载并应用 |
| 日志 | 控制台 + `Log/` 目录按天文件；行格式与文件头全量对齐 DH.NCore（`HH:mm:ss.fff 线程ID 类型 名称 正文`）；`RUST_LOG=debug` 调整级别 |

---

## 2. 构建

要求 Rust 1.80+（edition 2024）。仓库已包含 `.cargo/config.toml`（rsproxy 镜像，国内直连 crates.io 超时时使用）。

```bash
cargo build --release
# 产物：target/release/pek-ragent.exe（Windows）/ target/release/pek-ragent
```

> 依赖：`dhrust`（DH.RustBase，path 依赖，提供日志 / 定时器 / HTTP 服务端内核）、`windows-service`（Windows 服务运行时）、`zip`、`serde`、`md-5`、`libc`（Unix）等。

### 2.1 一键打包（Windows + Linux）

```powershell
# 全部平台（Windows 本机 MSVC + Linux musl 交叉编译），产物输出到 dist\
powershell -ExecutionPolicy Bypass -File scripts\build-release.ps1

# 可选：-Targets 只建某平台（可多选，如 linux,linux-arm64）；-Clean 先清旧产物与 zig 缓存；
#       -CleanAll 额外 cargo clean（清空全部编译缓存，最省磁盘，下次全量重建）
powershell -ExecutionPolicy Bypass -File scripts\build-release.ps1 -Targets linux -Clean
```

| 产物（`dist\`，含 `SHA256SUMS.txt`） | 说明 |
|------|------|
| `pek-ragent-v{x}-x86_64-pc-windows-msvc.zip` | Windows 可执行文件 |
| `pek-ragent-v{x}-x86_64-unknown-linux-musl.tar.gz` | Linux x86_64 静态单文件（已带执行位，解压即用） |
| `pek-ragent-v{x}-aarch64-unknown-linux-musl.tar.gz` | Linux ARM64 静态单文件（鲲鹏/飞腾/树莓派等） |
| `pek-ragent-v{x}-riscv64gc-unknown-linux-musl.tar.gz` | Linux RISC-V 64 静态单文件（赛昉/进迭时空等） |
| `pek-ragent-v{x}-loongarch64-unknown-linux-musl.tar.gz` | Linux LoongArch64 静态单文件（龙芯等） |

- 已启用 **release 增量编译**（`[profile.release] incremental = true`），重复打包只重编改动部分；
- Linux 交叉编译前置（**打包脚本会自动补齐**：缺 rustup 目标自动 `target add`、缺 cargo-zigbuild 自动安装、缺 zig 自动从清华 PyPI 镜像下载到 `tools\zig`）：cargo-zigbuild + x86_64 / aarch64 / riscv64gc / loongarch64 的 musl 目标 + zig；手动准备清单、国内加速与常见问题见 **`docs/build-env.md`**；
- 磁盘占用受控：zig 交叉缓存经 `.cargo/config.toml` 的 `[env]` 重定向到 `target\zig-cache\`，随 `-Clean` / `-CleanAll` 一键回收；
- 提示：zig 链接时可能输出 `ignoring deprecated linker optimization setting`，属工具链无害提示；
- 多平台支持规划（国产化 Linux / 嵌入式路由系统 / macOS 等）：见 `docs/platform-support.md`。

---

## 3. 快速开始

1. 把 `pek-ragent` 可执行文件放到部署目录（如 `C:\StarAgent`、`/opt/staragent`）；
2. 运行 `pek-ragent` 打开**控制台菜单**，选择 `2` 安装并启动服务；或直接执行 `pek-ragent -install`（Linux 需 `sudo`）；
3. 编辑 `Config/Agent.json` 添加应用（见第 5 节），执行 `pek-ragent -restart` 生效；
4. 验证：`pek-ragent -ListServices`，或 `curl http://127.0.0.1:5500/GetServices`。

---

## 4. 命令行一览

命令行参数与 C# StarAgent / NewLife.Agent 对齐（`-` 前缀可省略、大小写不敏感）。

### 4.1 服务级

| 命令 | 说明 |
|------|------|
| `-status` | 显示服务状态（安装/运行/路径/端口/子服务概览） |
| `-install` | 安装**并启动**系统服务（可附 `-server URL`，暂仅保存） |
| `-i` | 仅安装系统服务 |
| `-reinstall` | 重新安装（卸载 → 安装 → 启动） |
| `-uninstall` | 停止并卸载系统服务 |
| `-u` | 仅卸载系统服务 |
| `-start` / `-stop` / `-restart` | 启动 / 停止 / 重启系统服务 |
| `-run` | 前台运行（模拟运行；回车或 Ctrl+C 退出） |
| `-s` | 以服务方式运行（由系统服务管理器调用） |

### 4.2 应用级（经本地控制接口）

| 命令 | 说明 |
|------|------|
| `-ListServices` | 查看子服务列表（启用/运行/进程/启动时间） |
| `-StartService <名称>` | 启动子服务（同时启用，允许守护自动拉起） |
| `-StopService <名称>` | 停止子服务（同时禁用，防止守护重新拉起） |
| `-RestartService <名称>` | 重启子服务（先禁用→停止→启用→启动；停止失败会中止，避免重复拉起） |

### 4.3 其它

| 命令 | 说明 |
|------|------|
| `-ShowMachineInfo` | 显示本机信息（系统/主机/用户/CPU 型号/内存使用/运行时长 + 网络接口 + 磁盘列表；信息面对齐 C# StarAgent `ShowMachineInfo`） |
| `pek-ragent app.zip 参数…` | 位置参数 zip 一次性拉起（影子目录；支持 `-name`、`-shadow`） |
| `-help` / `-version` | 帮助 / 版本 |

### 4.4 控制台菜单

无参数启动进入菜单（自动识别状态：第 2、3 项随服务的安装/运行状态切换；已安装时页首显示服务**实际安装目录**（读取服务注册信息）；第 6–9 项子服务操作仅在**代理运行中**——本地控制接口可达（服务或前台模式均可）——时显示）：

```text
================= Pek.RAgent 星尘代理 v0.1.0 =================
 服务：星尘代理（StarAgent）
 安装目录：C:\StarAgent
 状态：运行中

 序号 功能名称            命令行参数
 1、 显示状态            -status
 2、 卸载服务            -uninstall
 3、 停止服务            -stop
 4、 重启服务            -restart
 5、 模拟运行            -run
 6、 查看子服务          -ListServices
 7、 启动子服务          -StartService
 8、 停止子服务          -StopService
 9、 重启子服务          -RestartService
 t、 服务器信息          -ShowMachineInfo
 0、 退出
```

---

## 5. 配置文件 `Config/Agent.json`

放置在**程序所在目录**的 `Config/Agent.json`（首次运行自动生成示例）。字段名为 PascalCase，与 C# `ServiceInfo` 语义对齐；缺省字段自动取默认值。

### 5.1 全局字段

| 字段 | 默认 | 说明 |
|------|------|------|
| `ServiceName` | `StarAgent` | 服务名（Windows 服务名 / systemd 单元名 / launchd 标签） |
| `DisplayName` | `星尘代理` | 显示名 |
| `Description` | … | 服务描述 |
| `LocalPort` | `5500` | 本地控制接口端口（DHDeploy 依赖 5500） |
| `LocalOnly` | `true` | 仅绑定 127.0.0.1；置 false 绑定 0.0.0.0（无鉴权，慎用） |
| `Delay` | `3000` | 重启/文件变动后重新启动的延迟（毫秒） |
| `StartWait` | `3000` | 健康检查等待时间（毫秒） |
| `MaxFails` | `20` | 最大失败次数，超过后不再自动拉起 |
| `GuardPeriod` | `30000` | 守护检查周期（毫秒） |
| `Debug` | `false` | 调试输出（多次重启时应用输出重定向到 `Log/app-*.log`） |
| `Server` / `Project` | 空 | 预留；`-server` / `-project` 参数会保存于此，暂不对接 |
| `StartupHook` | `false` | 对未引用星尘 SDK 的 .NET 应用注入 `Stardust.dll` |
| `Apps` | 示例 | 应用列表 |

### 5.2 应用字段（`Apps[]`）

| 字段 | 默认 | 说明 |
|------|------|------|
| `Name` | — | 名称，全局唯一 |
| `FileName` | `{Name}.zip` | 可执行文件、zip 包或系统命令 |
| `Arguments` | 空 | 启动参数（空白与双引号切分；`urls=http://*:8080` 等原样透传） |
| `WorkingDirectory` | `../apps/{Name}` | 工作目录；相对路径按程序目录解析 |
| `UserName` | 空 | 运行用户（仅 Linux 尽力支持） |
| `Enable` | `false` | 启用；`-StartService` 自动置 true，`-StopService` 自动置 false 并持久化 |
| `Mode` | `shadow` | 部署模式：`shadow` / `standard` / `hosted` / `task`（兼容 C# 数值 0-4、10-13） |
| `AllowMultiple` | `false` | 允许多实例（多实例时健康检查不按进程名匹配） |
| `Environments` | 空 | 环境变量，形如 `A=1;B=2` |
| `AutoStop` | `false` | 随宿主退出时同时停止该应用 |
| `ReloadOnChange` | `false` | 文件变动自动重启（5 秒周期轮询） |
| `MaxMemory` | `0` | 最大内存（MB），超限重启；0 不限制 |
| `OomScoreAdjust` | `0` | OOM 分值（仅 Linux） |
| `HealthCheck` | 空 | 健康检查：`http://…` 或 `tcp://host:port` |
| `Overwrite` | 空 | 部署包内需拷贝覆盖到工作目录的文件/子目录，`;` 分隔，支持 `*` |
| `Debug` | `false` | 应用输出重定向到 `Log/app-{Name}.log` |

### 5.3 示例

```json
{
  "ServiceName": "StarAgent",
  "LocalPort": 5500,
  "Apps": [
    {
      "Name": "webapp",
      "FileName": "webapp.zip",
      "Arguments": "urls=http://*:8080",
      "WorkingDirectory": "apps/webapp",
      "Mode": "shadow",
      "Enable": true,
      "AutoStop": true,
      "MaxMemory": 2048
    },
    {
      "Name": "test",
      "FileName": "ping",
      "Arguments": "newlifex.com",
      "Enable": false
    }
  ]
}
```

---

## 6. 部署模式与影子目录

| 模式 | 行为 |
|------|------|
| `shadow`（默认） | 解压到 `{工作目录}/../shadow/{名称}-{MD5前8位}` 运行；工作目录只保留配置/数据；新版本重启后切换影子目录并清理旧版 |
| `standard` | 解压到工作目录直接运行；提供运行中文件安全替换（占用时改名 `*.del`） |
| `hosted` | 仅解压，由外部宿主（IIS/Nginx）运行；Windows 下带 IIS 离线切换（`app_offline.htm`） |
| `task` | 运行一次后自动禁用，不守护 |

### 6.1 目录布局（shadow 模式示例）

```text
├─ StarAgent/            ← 程序目录（Config/Agent.json、Log、data）
└─ apps/
   └─ webapp/            ← 工作目录（部署包、配置文件、数据）——应用运行时不占用其中任何文件
└─ apps/shadow/
   └─ webapp-3f8a2c1d/   ← 影子目录（实际运行的 exe/dll）
```

### 6.2 运行中文件为什么可以替换

1. 应用的可执行文件运行在**影子目录**中，工作目录里的 `webapp.zip` 等部署文件没有任何进程句柄；
2. 上传/覆盖工作目录文件（DHDeploy 部署、手工拷贝、Agent 解压）**不会被占用**；
3. 重启应用时按新 zip 的 MD5 生成新的影子目录并切换，旧影子目录随后清理。

对于 `standard` 模式（文件直接运行于工作目录），Pek.RAgent 写入文件时使用"改名让位"策略（`fs::rename` 失败则把被占用文件改名为 `*.del` 再写入），应用停止后自动清理 `*.del`。

### 6.3 可执行文件检索顺序（与 C# 对齐）

名称完全匹配 → `{名称}.exe`（Windows）→ 第一个参数中的文件名 → 唯一 exe（Windows）→ `*.runtimeconfig.json` 配套 dll → `{名称}.dll` → `{名称}.jar`；`.dll` 用 `dotnet` 启动、`.jar` 用 `java -jar` 启动。

---

## 7. 本地 HTTP 控制接口

默认监听 `127.0.0.1:5500`（`LocalOnly=true`）。**DHDeploy 通过 `ApiHttpClient("http://localhost:5500/")` 调用下面 3 个接口，格式必须保持兼容。**

### 7.1 契约（DHDeploy 兼容）

```text
GET /RestartService?serviceName=X
GET /StartService?serviceName=X
GET /StopService?serviceName=X
```

响应恒为 HTTP 200，JSON（PascalCase）：

```json
{ "Success": true, "Message": "服务重启成功", "ServiceName": "X" }
```

- `StopService` 会同时**禁用**该应用（`Enable=false` 持久化），避免守护重新拉起；
- `StartService` 会同时**启用**；
- 服务不存在时 `Success=false`，`Message` 为中文原因说明。

### 7.2 其它端点

| 端点 | 说明 |
|------|------|
| `GET /GetServices` | 子服务列表（`Services` + `RunningServices`，字段对齐 C# `ServicesInfo`） |
| `GET /Info` | 代理信息（版本/OS/IP/端口/应用数） |
| `GET /Ping?processId=&watchdogTimeout=` | 喂狗 + 返回服务器时间（毫秒） |
| `POST /Ping` | 同上（JSON/form 参数） |
| `POST /KillAndStart` | 杀死指定进程并重新启动（应用自重启辅助） |
| `GET /ShowMachineInfo` | 本机信息文本 |
| `GET /` | 接口帮助 |

### 7.3 调用示例

```powershell
# 重启应用（DHDeploy 同款调用）
Invoke-RestMethod 'http://localhost:5500/RestartService?serviceName=webapp'

# 查看子服务
Invoke-RestMethod 'http://localhost:5500/GetServices' | ConvertTo-Json -Depth 5
```

```bash
curl 'http://127.0.0.1:5500/RestartService?serviceName=webapp'
```

---

## 8. 系统服务

### 8.1 Windows

- 安装：`pek-ragent -install`（需**管理员**；`binPath` = `"{exe}" -s`，自动启动，附加失败恢复策略）；
- 控制：`-start` / `-stop` / `-restart` / `-status`（内部经 `sc.exe`，带状态轮询等待）；
- 卸载：`-uninstall`（先停止再删除）；
- 服务运行时由 `windows-service` crate 与 SCM 交互：响应停止/关闭控制，停止时优雅退出（停止 `AutoStop` 应用、保存状态）。
- 服务进程的工作目录在启动时自动切换到程序目录，配置中的相对路径据此解析。

### 8.2 Linux（自动探测 init：systemd / procd / SysV·OpenRC）

- 探测顺序：systemd（`/run/systemd/system`）→ OpenWrt procd（`/etc/rc.common`）→ SysVinit/OpenRC（`/etc/init.d`）；可用环境变量 `PEK_RAGENT_INIT=systemd|procd|sysv` 强制指定（排障用）；
- 安装：`sudo pek-ragent -install`；**systemd** 生成 `/etc/systemd/system/{ServiceName}.service` 并 `enable`：
  ```ini
  [Service]
  Type=simple
  WorkingDirectory="{程序目录}"
  ExecStart="{exe}" -s
  Restart=always
  RestartSec=5
  KillMode=process        # 只杀主进程，避免误杀应用进程（对齐 C#）
  OOMScoreAdjust=-1000    # 禁止被 OOM 杀死
  ```
- 控制：`-start` / `-stop` / `-restart` / `-status`（内部经 `systemctl`）；
- 卸载：`sudo pek-ragent -uninstall`（`disable` + 删除单元文件 + `daemon-reload`）；
- **OpenWrt（procd）**：生成 `/etc/init.d/{ServiceName}`（`USE_PROCD=1` + `respawn`），`enable` 配置自启，控制/卸载同样经该脚本；
- **SysVinit / OpenRC**：生成 LSB 风格 `/etc/init.d/{ServiceName}`，自动尝试 `update-rc.d` / `chkconfig` / `rc-update` 配置自启（都不可用时提示手动配置）。

### 8.3 macOS（launchd，尽力支持）

- 安装：`sudo pek-ragent -install`，生成 `/Library/LaunchDaemons/{ServiceName}.plist` 并 `bootstrap`；
- 控制：`launchctl kickstart -k` / `kill SIGTERM`；
- 卸载：`sudo pek-ragent -uninstall`（`bootout` + 删除 plist）。

> 本仓库在 Windows 上开发验证；Linux/macOS 分支代码已按同一结构实现，但未在对应平台实机跑过，首次部署请先用 `-run` 前台验证。

---

## 9. 与 C# StarAgent 的差异（当前版本）

| 项 | 说明 |
|----|------|
| StarServer / StarWeb 对接 | **暂未实现**；`-server` 参数仅保存到配置 |
| Web 管理面板 | 未实现（原 5580 面板）；控制手段为控制台菜单 + 本地 HTTP 接口 |
| 本地 RPC | 原 UDP 5500 改为 HTTP 5500；对 DHDeploy 的契约保持兼容 |
| 配置格式 | 本项目使用 `Config/Agent.json`；未兼容 C# 的 XML 配置 |
| 未实现功能 | Nginx 配置生成、防火墙端口自动开放、阿里云 DNS、自身升级/修复、`-watch` 看门狗服务 |
| 状态存储 | `data/state.json` 记录运行中 PID（用于接管），原 `Service.csv` 不再使用 |

---

## 10. 开发

```text
src/
├─ main.rs       入口：基础目录/日志/参数预处理
├─ cli.rs        命令行分发 + 控制台菜单
├─ agent.rs      宿主：前台/服务运行、守护与监视定时器、退出清理
├─ manager.rs    应用管理器：多应用守护、控制、状态持久化、看门狗、配置热更新
├─ app.rs        单应用运行时：启动/停止/退出检测/内存/文件变动
├─ deploy.rs     部署：解压、影子目录、可执行文件检索、安全文件替换
├─ server.rs     本地 HTTP 控制接口（dhrust::net::http + router）
├─ service/      平台服务管理（windows / systemd / launchd / unsupported）
├─ sys.rs        平台进程工具（存活/内存/信号/机器信息）
├─ netc.rs       极简 HTTP 客户端 / TCP 连通检查
├─ config.rs     配置模型（Config/Agent.json）
└─ util.rs       基础辅助（路径/日志/通配/参数切分）
```

- 单元测试：`cargo test`（22 项：配置、部署模式、可执行文件检索、影子解压、安全替换、参数切分、僵尸进程判定等）；
- 冒烟脚本思路（本机已验证）：临时目录启动 `-run` → `Invoke-RestMethod` 调用 5500 接口 → 验证影子目录切换、运行中替换部署包、代理重启后的进程接管。

---

## 11. 路线图

- [ ] StarServer / StarWeb 对接（登录、心跳、指令下发）
- [ ] Web 管理面板（子服务 CRUD / 日志 / 配置在线修改）
- [ ] 自身升级（`-upgrade` 完整实现）与 `-repair`
- [ ] Linux 实机验证与发行（systemd 单元模板随包提供）
- [ ] 进程按名称接管 / 多实例精确匹配
- [ ] 多平台支持：国产 Linux（麒麟/统信/openEuler）实机验证、嵌入式/路由系统（OpenWrt/Buildroot）服务化适配、macOS 实机（ARM64 交叉产物已打通；规划见 `docs/platform-support.md`）
