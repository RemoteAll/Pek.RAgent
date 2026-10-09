# Pek.RAgent —— 星尘代理（StarAgent 的 Rust 实现）

部署在每台应用服务器 / 边缘节点上的节点守护代理，以系统服务方式运行，负责**多应用进程守护、影子目录部署、运行中文件替换与本地控制接口**。

当前版本**暂不对接 StarServer / StarWeb**，定位为独立可用的本地代理；`DHDeploy` 所需的本地命令接口（默认 `localhost:5501` 上的 `RestartService / StartService / StopService`）已完整提供——默认端口（5501）与服务名（`StarAgentRust`）均与 **C# 版 StarAgent（5500 / StarAgent）错开**，两者可同时安装、并行运行；DHDeploy.Agent.Rust 按节点类型调用（Rust 类型节点 → 5501；非 Rust/空 → 5500）。

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
| 文件变动重启 | `ReloadOnChange=true`（默认）时按 5 秒周期监视**配置的程序文件本身**（含无扩展名的 Linux 可执行文件），变更后停止应用，稳定 `Delay` 毫秒后重启 |
| 健康检查 | 启动后按 `HealthCheck`（http/https/tcp 地址）探测，失败仅记录日志（对齐 C# 行为；https 含 TLS） |
| 看门狗 | 应用通过 `GET /Ping?processId=&watchdogTimeout=` 喂狗，超时未喂自动重启对应应用 |
| 进程接管 | 代理重启后接管仍存活的子进程（`data/state.json`），**不会重复拉起** |
| 本地 HTTP 控制接口 | 默认 `0.0.0.0:5501`（`LocalOnly=true` 时仅 `127.0.0.1`；与 C# 版 StarAgent 的 5500 错开可并存），兼容 DHDeploy 的调用契约 |
| 位置参数 zip 拉起 | `pek-ragent app.zip urls=http://*:8080`（影子目录运行的一次性应用） |
| 配置热更新 | `Config/StarAgent.config` 被外部修改后自动重新加载并应用 |
| Web 管理面板 | 内置浏览器管理界面（对齐 C# 面板契约）：状态/子服务/流量/控制/配置/星尘设置/日志（含操作日志子页）/看门狗/数据库/文件管理/日志清理/插件/AI 助手/在线终端/用户管理/服务器校时；**多用户与菜单权限**（面板用户存 SQLite；登录后仅显示被授权菜单，服务端逐接口强制校验）；默认管理员 `admin`/`admin`（配置文件凭据，超级权限，在用户列表首行显示）；鉴权级别 `WebAuthLevel`（None/LocalOnly/Full，默认 LocalOnly：本机免登录、远程需令牌）；**操作审计**（登录与全部变更类操作自动落库 `Agent_OperationLog`，密码等敏感字段脱敏）；前端页编译期内嵌。**「控制」页含「DHDeploy 访问控制」卡片**（探测本机 `127.0.0.1:8282` 控制接口）：**① 服务远程访问**（整个节点监听 `0.0.0.0` ⇄ `127.0.0.1` 热重绑——仅本机时外部完全无法连接，适合平台走 WS 中继的节点）**② 管理面板访问**（仅本机/允许远程/关闭面板）；一键切换、变更入审计 |
| 资源采样器 | 后台线程按 `SampleInterval`（默认 1 秒）采样整机 CPU/网络/磁盘/TCP/线程句柄并缓存；面板请求读快照（多客户端读数一致、请求零采集；CPU 与任务管理器/宝塔同粒度）；`SampleInterval=0` 可关闭改由请求时现采 |
| 流量统计 | **网站流量**（解析 nginx/Apache/Caddy 访问日志，零侵入）：自动发现站点（宝塔 `/www/server/panel/vhost/nginx`、`/etc/nginx/sites-enabled` 等）+ `WebLogs` 手动补充，按站点聚合今日/累计流量、请求数、UV 与状态码分布；**端口流量**（Linux：`nftables` 独立计数表 `inet pek_stats`，需 root）：各端口 TCP/UDP 收发字节与实时速率，nft 不可用时自动降级连接视图（Windows 仅连接视图）；面板“📈 流量”页实时查看（接口 `/star/webTraffic`、`/star/portTraffic`）；**历史数据**：每日归档（跨天自动保存到 SQLite：`Data/traffic.db`，由 **Pek.RCode** 按 XCode 规范模型 `Entity/Model.xml` 管理；网站按站点、端口按天；旧版 JSON 归档首次运行自动迁移），面板按最近 7/30/90 天查看趋势图与每日明细（接口 `/star/trafficHistory`） |
| 日志 | 控制台 + `Log/` 目录按天文件；行格式与文件头全量对齐 DH.NCore（`HH:mm:ss.fff 线程ID 类型 名称 正文`）；`RUST_LOG=debug` 调整级别 |
| **AI 助手** | 服务器问题分析助手（面板「🤖 AI 助手」页）：对话式分析，自动附带服务器实况快照（主机/代理/子服务/看门狗/最近日志）；**支持自定义模型**——OpenAI 兼容接口（默认接入 DeepSeek `deepseek-chat`，可改 `deepseek-reasoner` 或任何兼容服务/本地 Ollama），配置项 `AiEnabled`/`AiBaseUrl`/`AiModel`/`AiApiKey`（面板配置页可改）；接口 `/star/aiStatus`、`/star/aiChat`；菜单权限 `ai` 可授予子用户 |
| **在线终端** | 免 SSH 的真终端（「🖥 在线终端」页，xterm.js + WebSocket + PTY）：单区域流式、光标闪烁、命令回显，**支持 vim/top 等交互式程序与 Ctrl+C 中断、pause 等待等完整键盘交互**；会话内 `cd`/环境变量保持、刷新自动重连并回放最近输出、空闲 30 分钟回收；命令以服务账户权限运行（Linux 通常 root、Windows LocalSystem）；**会话打开/关闭记入操作审计**（有意不采集击键——可能含密码）；配置项 `TerminalEnabled` 可整体关闭；菜单权限 `terminal`；接口 `GET /star/termWs`（WebSocket）、`POST /star/termReset` |

---

## 2. 构建

要求 Rust 1.80+（edition 2024）。仓库已包含 `.cargo/config.toml`（rsproxy 镜像，国内直连 crates.io 超时时使用）。

```bash
cargo build --release
# 产物：target/release/pek-ragent.exe（Windows）/ target/release/pek-ragent
```

> 依赖：`dhrust`（DH.RustBase，path 依赖，features `net`/`http-tls`/`toml`/`service`/`zip-extract`——提供日志 / 定时器 / HTTP 服务端内核 / **HTTPS 客户端**（rustls+ring；AI 助手、在线插件源、健康检查等 https 调用）/ **跨平台服务管理**（SCM·systemd·procd·SysV·launchd，见 `dhrust::service`）/ **ZIP 解压**（`dhrust::zip::extract_zip`）/ 面板鉴权与限流）、`zip`、`serde`、`libc`（Unix）等。

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
3. 编辑 `Config/StarAgent.config` 添加应用（见第 5 节），执行 `pek-ragent -restart` 生效；
4. 验证：`pek-ragent -ListServices`，或 `curl http://127.0.0.1:5501/GetServices`。

### 3.1 Linux 部署（静态单文件）

发布包（`dist/`）按架构选择：`uname -m` 输出 `x86_64` → `pek-ragent-v0.1.4-x86_64-unknown-linux-musl.tar.gz`；`aarch64`（ARM64）→ `...-aarch64-unknown-linux-musl.tar.gz`；另提供 `riscv64gc` / `loongarch64` 包。均为 **静态 musl 单文件**（约 3MB，零运行库依赖），压缩包内即一个 `pek-ragent`。

```bash
# 本机（Windows PowerShell）上传；dist 下同名 .tar.gz 解压后即 pek-ragent + install.sh
scp dist/pek-ragent-v0.1.4-x86_64-unknown-linux-musl.tar.gz root@server:/tmp/

# 服务器上解压安装（systemd：Restart=always / KillMode=process / OOMScoreAdjust=-1000 随单元自动生成）
sudo mkdir -p /opt/staragent
sudo tar -xzf /tmp/pek-ragent-*.tar.gz -C /opt/staragent
cd /opt/staragent
sudo bash install.sh              # 一键：补可执行位并安装启动服务
./pek-ragent -status              # 状态；日志在 Log/ 目录
```

> 包内文件已带可执行位（`tar -xzf` 解压即用）；若通过 scp 直接传**单个文件**等不保留权限的途径获取，会出现 `-bash: ./pek-ragent: Permission denied`，执行一次 `chmod +x pek-ragent` 即可（或直接用包内 `bash install.sh`，它自动补权限）。

> **从旧服务名迁移（已装过旧版的机器）**：默认服务名已改为 `StarAgentRust`（C# 版为 `StarAgent`，两者可同机并存）。迁移：编辑 `Config/StarAgent.config` 的 `<ServiceName>`/`<DisplayName>` 为新值 → `sudo bash install.sh`（或 `./pek-ragent -reinstall`）。安装时**自动清理**指向本程序的旧名服务（systemd 单元 / init.d / Windows 服务 / launchd 任务）；若旧注册指向 C# 版星尘（不同程序），保留不动。不迁移也可：旧配置读取后按旧名继续运行。
>
> **防误删**：若配置中的服务名与其它程序（如 C# 版）的注册同名冲突，`-uninstall`/`-reinstall` 会**拒绝执行**并提示改名——绝不会删除其它程序的注册。

**访问 Web 管理面板**（`Config/StarAgent.config` 默认 `LocalOnly=false`，监听 `0.0.0.0:5501`，可直接通过服务器 IP 访问）：

```bash
# 默认：浏览器打开 http://服务器IP:5501/（admin/admin）
# 安全提醒：仍使用默认密码时启动日志会明确提示；请尽快修改密码，
#           并放行防火墙与云安全组：
firewall-cmd --add-port=5501/tcp   # firewalld；ufw 对应 sudo ufw allow 5501
# 云服务器还需在控制台安全组放行 5501/TCP

# 可选（更安全）：仅本机访问 + SSH 隧道
vi Config/StarAgent.config         # 将 "LocalOnly" 改为 true
./pek-ragent -restart
ssh -L 5501:127.0.0.1:5501 root@server   # 然后打开 http://127.0.0.1:5501/
```

> 旧版本生成的配置若为 `LocalOnly=true`，改回 `false` 并重启即可远程访问。

> 面板鉴权级别由 `WebAuthLevel` 控制（默认 `LocalOnly`）：服务器本机访问免登录，远程访问需 `admin` 登录（Bearer Token）；改为 `Full` 则全部需登录。

### 3.2 运行时升级（"上传即升级"，不停止、不改名）

Linux 内核保护"正在执行的 ELF"（**直接覆盖上传会报 `ETXTBSY`**）；Pek.RAgent 提供与 1Panel 类似的
"上传即升级"体验——**无需改名、无需停服**，服务端自动完成三步：

1. **影子冒烟**：先以独立进程从影子位置运行新版本自检（`-selftest`），通过才允许替换（失败则当前程序保持不变）；
2. **原子替换**：Unix 直接 rename；Windows 采用"改名让位"策略替换运行中的 exe（瞬态占用自动重试）；
3. **自动重启**：替换后旧进程退出，重启助手（`-ensure-running -upgrade`）等待并确保服务管理器
   拉起新版本（不依赖 SCM 失败恢复次数——耗尽或较慢时由助手显式启动）。

任选一种上传方式：

```bash
# 方式一（推荐）：Web 面板 —— 登录后进入"配置"页 → "程序升级"卡片 → 选择文件上传
#   （服务端自动完成：校验 → 影子自检 → 替换 → 重启；失败时当前程序保持不变）

# 方式二：命令行一条命令（本地或 SSH；文件路径与文件名任意）
sudo /www/Agent/pek-ragent -update /tmp/pek-ragent-new    # 升级 + 重启服务

# 方式三：SFTP/scp 上传到 Update 子目录（文件名随意，约 10 秒内自动升级）
#   /www/Agent/Update/<任意文件名>
#   兼容旧约定：命名为 /www/Agent/pek-ragent.new 同样有效；
#   安装脚本场景：cd /www/Agent && sudo bash install.sh（自动识别 pek-ragent.new）
```

说明：
- 检测节奏：文件需静置 ≥10 秒（防半截上传）+ ELF/PE 头校验；从上传到服务恢复通常 10~40 秒；
- 校验/冒烟不通过会**拒绝替换**并改名 `*.failed` 留证（日志记录原因，当前程序不受影响）；
- **外部替换兜底**：通过其他方式（先删后传 / mv / 同步工具"临时文件+重命名"）换上的程序文件
  也会被自动检测——校验 + 影子自检通过后自动重启生效（约 10~20 秒）；
- 直接"覆盖写"正在运行的程序文件会被 Linux 内核拒绝（`ETXTBSY`）——任何程序都无法绕过，
  请使用上述三种上传方式之一；
- 前台 `-run` 模式退出后不自动拉起（需手动重启），日志有提示。

---

## 4. 命令行一览

命令行参数与 C# StarAgent / NewLife.Agent 对齐（`-` 前缀可省略、大小写不敏感）。

### 4.1 服务级

| 命令 | 说明 |
|------|------|
| `-status` | 显示服务状态（服务管理器类型/运行状态/路径/配置/端口/子服务概览 + 最近日志 + 版本与发布时间） |
| `-install` | 安装**并启动**系统服务（可附 `-server URL`，暂仅保存） |
| `-i` | 仅安装系统服务 |
| `-reinstall` | 重新安装（卸载 → 安装 → 启动） |
| `-uninstall` | 停止并卸载系统服务 |
| `-u` | 仅卸载系统服务 |
| `-start` / `-stop` / `-restart` | 启动 / 停止 / 重启系统服务 |
| `-run` | 前台运行（模拟运行；回车或 Ctrl+C 退出） |
| `-s` | 以服务方式运行（由系统服务管理器调用） |
| `-update [文件]` | 用新版本文件升级并重启服务（见 3.2；文件路径任意，省略时使用 `Update` 目录 / `{exe}.new` 约定） |

输出示例（逐行对齐 C# `ShowStatus` 的结构：服务/描述/状态/路径 + 空行 + 版本行；其后为 Pek.RAgent 附加信息）：

```text
$ pek-ragent -status
服务：星尘代理(Rust)(StarAgentRust)
描述：星尘节点守护代理（Pek.RAgent）。提供进程守护、影子目录部署与本地控制接口。
状态：systemd 运行中
路径：/www/Agent/pek-ragent

Pek.RAgent	版本：0.1.4	发布：2026-10-09 10:00:00

配置：/www/Agent/Config/StarAgent.config
本地端口：5501（仅本机：是）
子服务：2 个，运行中 1

最近日志（2026_10_01.log）：
11:49:41.564 04 N dhrust-conn Web 面板登录成功：admin（127.0.0.1）
```

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
| `-PluginStoreKeygen [目录]` | 生成在线插件源签名密钥对（Ed25519）：`plugin-store.key`（私钥 hex，离线保管）+ `plugin-store.pub`（公钥 hex，填入面板「配置」→ 插件源公钥） |
| `-PluginStorePublish <插件源目录> <插件.zip> [--base-url https://…/] [--key 私钥]` | 发布插件（推荐）：校验包 → 计算 SHA-256 → 更新 `catalog.json`（url+sha256）→ 重新签名 `.sig`；首次需 `--base-url`（写入 `store.json` 记忆）；同 id 旧版本包自动清理 |
| `-PluginStoreRemove <插件源目录> <插件id> [--key 私钥]` | 下线插件：删除目录条目与包文件并重新签名 |
| `-PluginStoreSign <catalog.json> [私钥文件]` | 仅对目录签名（手动维护目录时使用） |
| `pek-ragent app.zip 参数…` | 位置参数 zip 一次性拉起（影子目录；支持 `-name`、`-shadow`） |
| `-selftest` / `-ensure-running` | 升级管线内部命令（影子自检 / 重启助手），由程序自动调用，无需手工执行 |
| `-help` / `-version` | 帮助 / 版本 |

### 4.4 控制台菜单

无参数启动时，先输出与 C# `ShowStatus` 相同的**状态块**（服务/描述/状态/路径 + 版本行），再进入菜单循环（第 2、3 项随服务的安装/运行状态切换；第 6–9 项子服务操作仅在**代理运行中**——本地控制接口可达（服务或前台模式均可）——时显示）：

```text
服务：星尘代理(Rust)(StarAgentRust)
描述：星尘节点守护代理（Pek.RAgent）。提供进程守护、影子目录部署与本地控制接口。
状态：systemd 运行中
路径：/www/Agent/pek-ragent

Pek.RAgent	版本：0.1.4	发布：2026-10-09 10:00:00

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

## 5. 配置文件 `Config/StarAgent.config`

放置在**程序所在目录**的 `Config/StarAgent.config`（XML 格式，**与 C# StarAgent 同名同格式，双端完全互通**，可直接相互接管同一份配置；**首次运行自动生成带中文注释的完整模板**）。字段名为 PascalCase，与 C# `StarAgentSetting`/`ServiceInfo` 对齐；缺省字段自动取默认值。

- **注释保留**：通过程序（含 Web 面板）修改配置值时，文件中的注释与排版会保留；也支持手工编辑（保存后自动重新加载）；
- **C# 字段保留**：C# 特有字段（`Code`/`Secret`/`Channel`/`SyncTime`/`UseAutorun`/`UserName`/`Dpi`/`Resolution` 等）及 `ServiceInfo` 上 Rust 不认识的属性（如 `AutoStart`/`Priority`）读写时均原样保留不丢失；
- **双向互通**：Rust 新增字段以平级元素/属性形式写入（C# `XmlSerializer` 对未知元素/属性自动忽略），C# 保存的文件 Rust 照常读取；部署模式 `Mode` 按 C# 数值（10-13）写入；
- **旧版迁移**：检测到旧 `Config/Agent.toml`（TOML 版）或 `Config/Agent.json` 时自动转换为 XML，原文件改名 `.toml.bak` / `.json.bak`。

### 5.1 全局字段

| 字段 | 默认 | 说明 |
|------|------|------|
| `ServiceName` | `StarAgentRust` | 服务名（Windows 服务名 / systemd 单元名 / launchd 标签）。与 C# 版 `StarAgent` 错开，可同机并存；旧名 `StarAgent` 若指向本程序，安装时自动清理 |
| `DisplayName` | `星尘代理(Rust)` | 显示名 |
| `Description` | … | 服务描述 |
| `LocalPort` | `5501` | 本地控制接口端口（TCP 面板与 UDP RPC 共用）。默认 5501：与 C# 版 StarAgent（5500）错开，二者可同时并存；DHDeploy.Agent.Rust 按节点类型调用（Rust→5501，非 Rust/空→5500） |
| `LocalOnly` | `false` | 为 true 时仅绑定 127.0.0.1（远程不可达）；默认 false 绑定 0.0.0.0（允许远程访问，面板凭据兜底） |
| `Delay` | `3000` | 重启/文件变动后重新启动的延迟（毫秒） |
| `StartWait` | `3000` | 健康检查等待时间（毫秒） |
| `MaxFails` | `20` | 最大失败次数，超过后不再自动拉起 |
| `GuardPeriod` | `30000` | 守护检查周期（毫秒） |
| `Debug` | `false` | 调试输出（多次重启时应用输出重定向到 `Log/app-*.log`） |
| `Server` / `Project` | 空 | 预留；`-server` / `-project` 参数会保存于此，暂不对接 |
| `StartupHook` | `false` | 对未引用星尘 SDK 的 .NET 应用注入 `Stardust.dll` |
| `WatchDog` | 空 | 看门狗：逗号分隔的进程名，每分钟检查存活（面板 `/api/watchdog`） |
| `WebUserName` / `WebPassword` | `admin` | Web 面板登录凭据（面板「用户」页管理：内置管理员行可改用户名/密码） |
| `WebAuthLevel` | `LocalOnly` | 面板鉴权级别：None 不鉴权 / LocalOnly 本机（回环地址）免登录、远程需 Token / Full 全部需 Token；修改后自动生效（无需重启） |
| `SampleInterval` | `1000` | 后台资源采样间隔（毫秒）：面板 CPU/网络/磁盘速率按此窗口差分；`0`=关闭后台采样（改为面板请求时现采）。修改需重启服务后生效 |
| `WebTraffic` | `true` | 网站流量统计开关：后台解析站点访问日志（自动发现常见目录 + `WebLogs` 手动补充），零侵入只读；关闭后停止读取；统计口径默认响应体字节（`$body_bytes_sent`），日志行尾追加 `$bytes_sent` 时自动改用实际发送字节（响应头+响应体，推荐配置见 8. 面板“流量”节） |
| `WebLogs` | 空 | 网站日志手动补充（高级选项，面板配置页不展示，一般无需填写）：`名称=路径;名称2=路径2`（绝对路径；自动发现不到时补充，如 Caddy 自定义日志） |
| `PortTraffic` | `true` | 端口流量统计开关（默认开启）：Linux 在独立 nftables 表 `inet pek_stats` 中按端口计数收发字节（需 root 与 `nft` 命令；**只计数不改转发**，关闭/卸载时自动删表）；无 nft 或权限不足、Windows/其它平台自动降级为连接视图（无字节数） |
| `PortTrafficPorts` | 空 | 端口流量统计端口列表（高级选项，面板配置页不展示，一般无需填写）：`22,80,443`（逗号分隔）；留空自动取系统监听端口（上限 64 个） |
| `TrafficHistoryDays` | `90` | 流量历史保留天数：每日归档落 SQLite `Data/traffic.db`（Pek.RCode + XCode 模型 `Entity/Model.xml`；网站按站点、端口按天；跨天自动保存、进程重启续算不重复）；`7~3650`，`0`=永久保留 |
| `LogCleanupPaths` | 空 | 日志清理自定义路径（高级选项，面板配置页不展示，直接编辑配置文件，修改后自动生效）：分号分隔（绝对路径；目录=清空内容、文件=截断清空），在面板“🧹 日志清理”页以“自定义路径”分类显示与清理 |
| `PluginStoreUrl` | 空 | 在线插件源地址（`catalog.json` 的 URL）：仅允许 https（127.0.0.1 例外便于本地调试）；面板「🧩 插件」页据此展示在线插件并可一键安装/更新；留空 = 关闭在线插件。修改后自动生效 |
| `PluginStorePubKey` | 空 | 在线插件源 Ed25519 公钥（hex；32 字节裸公钥或 44 字节 SPKI DER）：非空时对 `catalog.json` 强制验签（需发布同名 `.sig`），防插件源被篡改；留空 = 仅 HTTPS + SHA-256 校验 |
| `AiEnabled` | `true` | AI 助手开关（面板「🤖 AI 助手」页）；关闭后接口直接拒绝 |
| `AiBaseUrl` | `https://api.deepseek.com/v1` | AI 接口地址：OpenAI 兼容 Base（自动补 `/chat/completions`），也可直接填完整地址；接入其他厂商/本地模型时修改（如 `http://127.0.0.1:11434/v1`） |
| `AiModel` | `deepseek-chat` | 模型名（如 `deepseek-chat` / `deepseek-reasoner`） |
| `AiApiKey` | 空 | API Key（Bearer 令牌）；留空时 AI 页提示先到配置页填写 |
| `TerminalEnabled` | `true` | 在线终端开关（面板「🖥 在线终端」页）；关闭后接口直接拒绝 |
| `Services` | 示例 | 应用列表（`<ServiceInfo>` 元素，属性形式） |

### 5.2 应用字段（`<ServiceInfo>` 属性）

| 字段 | 默认 | 说明 |
|------|------|------|
| `Name` | — | 名称，全局唯一 |
| `FileName` | `{Name}.zip` | 可执行文件、zip 包或系统命令 |
| `Arguments` | 空 | 启动参数（空白与双引号切分；`urls=http://*:8080` 等原样透传） |
| `WorkingDirectory` | `../apps/{Name}` | 工作目录；相对路径按程序目录解析 |
| `UserName` | 空 | 运行用户（仅 Linux 尽力支持） |
| `Enable` | `false` | 启用；`-StartService` 自动置 true，`-StopService` 自动置 false 并持久化 |
| `Mode` | `shadow` | 部署模式：`shadow` / `standard` / `hosted` / `task`（保存时写 C# 数值 10-13；兼容旧数值 0-4） |
| `AllowMultiple` | `false` | 允许多实例（多实例时健康检查不按进程名匹配） |
| `Environments` | 空 | 环境变量，形如 `A=1;B=2` |
| `AutoStop` | `false` | 随宿主退出时同时停止该应用 |
| `ReloadOnChange` | `true` | 文件变动自动重启（5 秒周期监视程序文件本身） |
| `MaxMemory` | `0` | 最大内存（MB），超限重启；0 不限制 |
| `OomScoreAdjust` | `0` | OOM 分值（仅 Linux） |
| `HealthCheck` | 空 | 健康检查：`http://…`、`https://…`（含 TLS）或 `tcp://host:port` |
| `Overwrite` | 空 | 部署包内需拷贝覆盖到工作目录的文件/子目录，`;` 分隔，支持 `*` |
| `Debug` | `false` | 应用输出重定向到 `Log/app-{Name}.log` |

### 5.3 示例

```xml
<?xml version="1.0" encoding="utf-8"?>
<StarAgent>
  <!--本地端口。默认5501（与 C# 版 StarAgent 的 5500 错开，可同时并存）-->
  <LocalPort>5501</LocalPort>

  <!--应用服务集合-->
  <Services>
    <ServiceInfo Name="webapp" FileName="webapp.zip" Arguments="urls=http://*:8080" Enable="true" Mode="11" AutoStop="true" MaxMemory="2048" />
    <ServiceInfo Name="test" FileName="ping" Arguments="newlifex.com" Enable="false" />
  </Services>
</StarAgent>
```

> `Mode` 写 C# 数值（10=standard，11=shadow，12=hosted，13=task）；手工编写时也可用文本名（`shadow` 等），旧版 0-4 数值同样兼容。

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
├─ StarAgent/            ← 程序目录（Config/StarAgent.config、Log、data）
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

默认监听 `127.0.0.1:5501`（`LocalOnly=true`）。**C# 版 DHDeploy 通过 `ApiHttpClient("http://localhost:5500/")` 固定调用 5500（C# 星尘）；DHDeploy.Agent.Rust 按节点类型选择（Rust 类型节点 → 5501 本代理；非 Rust/空 → 5500）。接口格式必须保持兼容；UDP RPC 与 TCP 共用同一端口。**

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
Invoke-RestMethod 'http://localhost:5501/RestartService?serviceName=webapp'

# 查看子服务
Invoke-RestMethod 'http://localhost:5501/GetServices' | ConvertTo-Json -Depth 5
```

```bash
curl 'http://127.0.0.1:5501/RestartService?serviceName=webapp'
```

---

## 8. Web 管理面板

内置管理面板，浏览器访问 `http://127.0.0.1:5501/`（`LocalOnly=false` 时允许远程）：

- **登录**：默认 `admin` / `admin`（配置项 `WebUserName` / `WebPassword`，面板「用户」页可在线管理内置管理员用户名与密码）；鉴权级别 `WebAuthLevel`（默认 `LocalOnly`：本机免登录、远程需登录；`Full` 全部需登录；`None` 不鉴权）；Bearer Token 24 小时有效；登录爆破防护：每 IP 5 次失败封禁 5 分钟（窗口 15 分钟）；
- **状态**：服务运行时长/进程信息（PID、端口、进程内存）；资源监控（**整机视角**，口径与宝塔一致，顺序：负载 → CPU → 内存 → 磁盘）：负载（**仅 Linux**：1/5/15 分钟，Windows 不显示；百分比 = 1 分钟均值 /（核数 × 2），对齐宝塔）、CPU 使用率+核数（`busy/(busy+idle+iowait)`，按请求间隔差分采样，不阻塞请求）、内存已用/总量（**已用 = 总 − MemFree − Buffers − Cached − SReclaimable**，缓存不计入已用，同 psutil/宝塔）、磁盘（**全部磁盘各自用量**；过滤 /boot、/boot/efi 与 tmpfs/overlay/squashfs/snap 等虚拟文件系统，对齐 DHDeploy `IsTemporaryVolume`）；**流量与磁盘 IO 趋势图**（对齐宝塔：双页签 + 统计块 + 平滑双曲线，近 3 分钟每 3 秒采样；流量=上行/下行/总发送/总接收，磁盘=读取/写入/每秒读写/IO 延迟）；TCP 连接数；机器 GUID、主机运行时长、本机详情（CPU 型号/内存/磁盘分区/网卡/Top 进程）；
- **子服务**：列表（含运行状态）、启动/停止/重启、添加/编辑/删除（写回 `Config/StarAgent.config`）；
- **控制**：启停重启代理服务自身（分离进程延迟 2 秒执行 `sc stop/start` 或 `systemctl restart`）、释放内存（Windows 回收工作集）；
- **配置**：面板与守护参数在线更新（部分需重启服务后生效）；
- **星尘设置**：`Server` / `LocalPort` / `Project` / `StartupHook` / `Delay` 分组维护；
- **日志**：页内两个**子页签**（交互对齐流量页）——**📋 系统日志**（`Log/` 目录文件列表与尾部内容查看，支持行数/文件/级别过滤）与 **📜 操作日志**（审计查询，见下）；子页标签按权限显隐（URL hash `#logs` / `#logs.audit`，兼容旧 `#audit` 书签）；
- **看门狗**：`WatchDog` 配置的进程名存活状态检查。
- **文件管理**（对齐宝塔）：全盘浏览（根=盘符/`/`，符号链接可进入）、上传（多文件、带进度，≤64MB）/下载（≤256MB）、新建文件与文件夹、批量重命名/复制/移动/删除、ZIP 压缩/解压、在线编辑文本（≤2MB，UTF-8，二进制拒读）、Linux 权限修改（chmod）与递归搜索；删除/改名/移动对根、一级目录（`/etc`、`C:\Windows` 等）与程序目录本身设保护（目录内部照常可操作）；
- **日志清理**（对齐宝塔）：一键扫描并清理 系统日志（syslog/messages/journald）、网站日志（宝塔/nginx/apache）、Nginx 缓存目录、代理自身日志（`Log/`）、Redis/MySQL 日志（额外目录可通过配置文件高级项 `LogCleanupPaths` 追加，面板不提供编辑）；日志文件**截断清空**（保留文件、持句柄进程不受影响）、缓存目录清空（内含 `*.log` 同样截断保留）、journald 走 `journalctl --vacuum`；逐分类显示条数与大小、可展开文件明细。
- **插件**（权限 `plugins`）：`{base}/Plugins/<id>/` 放置含 `plugin.json` 清单的页面包，或面板「🧩 插件」页上传 zip 安装/删除；页面经 `/plugins/<id>/...` 同源服务并以 iframe 嵌入（复用登录态调用面板接口，受权限约束并记入操作日志）；清单字段 `name/description/version/icon/entry/app`（`app` 关联子服务名，可查看状态并启停）。
- **在线插件源（宝塔式商店的安全版）**：`PluginStoreUrl` 指向自建 `catalog.json`（HTTPS），面板展示在线插件并可一键**安装/更新**（同 id 版本变化即更新，替换失败自动回滚旧版本）；安全设计：**仅 https + 强制 SHA-256 校验 + 可选 Ed25519 强制验签**（`PluginStorePubKey`；发布端用 `pek-ragent -PluginStoreKeygen` / `-PluginStoreSign` 生成密钥并签名 `catalog.json.sig`）+ 下载大小上限 32MB + **不执行任何安装脚本**；校验不通过一律拒绝（对抗中间人/被篡改的插件源）。
- **插件发布流程**（发布端两步命令）：① 一次：`-PluginStoreKeygen` 生成密钥 + 面板「配置」填公钥；② 每次发布：`-PluginStorePublish ./store myplugin.zip --base-url https://plugins.example.com/ --key plugin-store.key`（自动校验包、计算 SHA-256、更新 `catalog.json` 并签名 `.sig`；同 id 旧版本包自动清理）→ 把整个 `store` 目录同步到 HTTPS 静态空间即完成发布；下线用 `-PluginStoreRemove`。
- **操作日志**（日志页子页签，权限 `audit`）：登录（含失败）与全部变更类操作（配置/控制/子服务/文件管理/日志清理/数据库/用户管理）自动落库 SQLite `Agent_OperationLog`（参数摘要脱敏——密码/令牌等字段替换为 `***`；含失败与 403 越权记录）；支持关键词/操作者/结果筛选与分页。
- **用户**（仅内置管理员）：多用户管理（用户名/密码/菜单权限勾选/启用/备注/删除）；内置管理员以虚拟条目显示在列表首行（全部权限、可编辑用户名/密码、不可删除）；面板用户存 `Data/traffic.db` 的 `Agent_PanelUser` 表（密码 `SHA-256(salt:password)` 不可逆存储，随数据库备份/还原迁移）；用户登录后仅显示被授权菜单（前端隐藏 + 服务端逐接口校验，越权返回 403 并记审计）。
- **流量**：页签内分为 **📈 网站流量 / 🔌 端口流量** 两个子页面（URL hash 记忆：`#traffic` / `#traffic.ports`）。**网站流量**（解析 nginx/Apache/Caddy 访问日志：每站点今日/累计流量、请求数、UV、状态码分布与实时速率；自动发现宝塔/标准 nginx/apache 站点，`WebLogs` 可手动补充；零侵入只读日志，重启续读不重复统计）+ **端口流量**（Linux nftables 独立计数表：各端口 TCP/UDP 收发字节与速率；无 nft/无权限时自动降级连接视图，Windows 为连接视图；**按端口每日数据**：端口汇总表（总接收/发送/日均/活跃天数/占比，支持 7/30/90 天与关键词筛选）→ 点击端口钻取**该端口每日收发**（堆叠柱状图 + 逐日明细表）；另有按日期的端口每日流量总表）+ **历史数据**（每日归档落 SQLite `Data/traffic.db`——**Pek.RCode** 消费方，XCode 规范模型 `Entity/Model.xml`（与 C# 生态共用），读取走实体缓存：趋势柱状图 + 按天明细，支持最近 7/30/90 天与站点筛选；默认保留 90 天，`TrafficHistoryDays` 可调；旧版 JSON 归档首次运行自动迁移）；进入页签时 3 秒轮询、离开即停（历史数据首次进入/跨天时拉取）。

**网站流量统计口径**：默认取日志中的响应体字节（nginx `$body_bytes_sent` / Apache `%b` / Caddy `size`）。如需统计 **nginx 实际发送流量（响应头+响应体）**，为站点启用扩展日志格式（行尾追加 `$bytes_sent $request_length`），本代理自动识别并改用 `$bytes_sent`：

```nginx
# 站点 conf 顶层（server 块外，即 http 上下文）定义新格式：
log_format agent_bw '$remote_addr - $remote_user [$time_local] "$request" $status $body_bytes_sent "$http_referer" "$http_user_agent" $bytes_sent $request_length';

# 站点 server 块内，替换原有 access_log 行（加格式名）：
access_log /www/wwwlogs/example.com.log agent_bw;
```

> 标准 combined 行尾是带引号的 UA，扩展格式行尾是纯数字（`$bytes_sent` 或 `$bytes_sent $request_length`），两者不会混淆；改动只影响新写入的日志行，历史数据不受影响；`nginx -t && nginx -s reload` 生效。

接口契约与 C# 面板一致（统一 `{code, message?, data?}` 信封），前端页面（`web/index.html`）直接复用 C# 版并通过 `include_bytes!` 编译期内嵌——单文件部署开箱即用，亦可在运行目录放 `wwwroot/index.html` 覆盖。面板页签由 URL hash 记忆（如 `http://127.0.0.1:5501/#traffic`），刷新/重新登录后停留在当前页。

| 端点 | 说明 |
|------|------|
| `POST /api/login`、`POST /api/logout` | 登录（返回 Token）/ 注销 |
| `GET /api/status`、`GET /api/health` | 服务状态 / 健康指标 |
| `POST /api/control` | 启停重启代理（`{"action":"start|stop|restart"}`） |
| `GET /api/freeMemory` | 释放内存 |
| `GET /api/configMetadata`、`POST /api/updateConfig`、`POST /api/changePassword` | 配置元数据 / 更新 / 修改密码 |
| `GET /api/logs`、`GET /api/logFiles`、`GET /api/watchdog` | 日志内容 / 文件列表 / 看门狗 |
| `POST /api/syncTime` | 同步系统时间（`{"timeMs": 毫秒时间戳}`，以浏览器机时间为准；Linux 需 root、Windows 需服务账户/管理员） |
| `GET /star/services`、`POST /star/startService`、`POST /star/stopService`、`POST /star/restartService` | 子服务列表与操作 |
| `POST /star/addService`、`POST /star/removeService` | 子服务新增/更新与删除（持久化到配置） |
| `GET /star/getStarConfig`、`POST /star/updateStarConfig` | 星尘配置读取/更新 |
| `GET /star/machine`、`GET /star/getProcessList` | 本机详情 / Top 进程 |
| `GET /star/webTraffic`、`GET /star/portTraffic` | 网站流量 / 端口流量统计快照（`WebTraffic` / `PortTraffic` 配置控制） |
| `GET /star/trafficHistory` | 流量历史每日归档（`?days=90` 最近 N 天：网站按站/端口按天数据与保留天数） |
| `GET /star/fileList`、`GET /star/fileRead`、`POST /star/fileWrite` | 文件管理：目录浏览 / 读取（在线编辑） / 保存 |
| `POST /star/fileMkdir`、`POST /star/fileNewFile`、`POST /star/fileRename`、`POST /star/fileDelete`、`POST /star/fileCopy`、`POST /star/fileMove` | 文件管理：新建/重命名/删除/复制/移动（均支持批量） |
| `GET /star/fileDownload`、`POST /star/fileUpload`、`POST /star/fileCompress`、`POST /star/fileExtract`、`POST /star/fileChmod`、`GET /star/fileSearch` | 文件管理：下载/上传/压缩(ZIP)/解压/权限/搜索 |
| `GET /star/logCleanScan`、`POST /star/logCleanRun` | 日志清理：扫描分类占用（条数/大小/明细） / 按分类清理（`{"keys":["system",…]}`） |
| `GET /star/pluginList`、`POST /star/pluginInstall?name=`、`POST /star/pluginDelete` | 插件：列表 / 上传安装（zip 请求体）/ 卸载（`{"id":…}`）；插件页面静态服务 `/plugins/<id>/...` |
| `GET /star/pluginStore`、`POST /star/pluginStoreInstall` | 在线插件源：列表（含已安装/可更新状态；未配置时 `configured=false`） / 安装或更新（`{"id":…}`；HTTPS 下载 + SHA-256 + 可选强制验签，校验失败拒绝） |
| `GET /api/me` | 当前登录主体（用户名/是否内置管理员/菜单权限；前端据此渲染菜单） |
| `GET /star/userList`、`POST /star/userSave`、`POST /star/userDelete` | 面板用户管理（仅内置管理员；新增须密码，编辑密码留空不改） |
| `GET /star/auditLogs` | 操作日志分页查询（`?page=&size=&user=&q=&success=`；需 `audit` 菜单权限） |

> `GET /star/services` 对运行中的应用额外返回 `CpuRate`（占整机 CPU %，按面板轮询间隔差分）与 `MemoryMB`（Rust 扩展字段；C# 面板反序列化忽略未知字段）。

> 鉴权：级别由 `WebAuthLevel` 控制——`None` 全部放行；`LocalOnly`（默认）本机回环地址免登录（视为内置管理员）、远程需 `Authorization: Bearer <token>`；`Full` 全部需令牌。未通过返回 `{code:401}`；本机免登录场景下前端自动跳过登录页。
>
> **多用户与菜单权限**：除内置管理员外可创建面板用户并勾选菜单权限（状态/子服务/流量/控制/配置/星尘设置/日志/看门狗/数据库/文件管理/日志清理/插件，以及位于日志页内的“操作日志”子页）；用户仅能看到被授权的菜单，服务端对每个接口做权限校验（403 拒绝并写入操作审计）；用户管理仅内置管理员可用。面板用户与审计日志存于与流量历史同一 SQLite（`Data/traffic.db`，模型见 `Entity/Model.xml`），数据库备份/还原自动覆盖；**旧版仅含两张流量表的备份包还原时不会清空用户表**（自适应）。

---

## 9. 系统服务

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

## 10. 与 C# StarAgent 的差异（当前版本）

| 项 | 说明 |
|----|------|
| StarServer / StarWeb 对接 | **暂未实现**；`-server` 参数仅保存到配置 |
| Web 管理面板 | **已实现**（对齐 C# 契约，前端直接复用）；差异：无 GC 统计（`gcTotalMemory`/`gcCollections` 恒为 0）、磁盘 IOPS 恒为 0、网卡收发包字节未统计、无“扩展面板”接口 |
| 本地 RPC | **UDP 5501 已实现**（NewLife ApiClient 二进制协议，与 C# StarAgent 同契约；仅限本机；默认端口与 C# 版 5500 错开，可同时并存）：`StartService`/`StopService`/`RestartService` 完整实现，`Ping`/`Info`/`GetServices`/`SetServer` 简化实现（待 StarServer 对接时补齐数据形态）；协议实现与 DHDeploy.Agent.Rust 客户端同源（`dhrust::net::api_rpc`）；同端口 TCP 提供 HTTP 面板契约（Web 面板 + DHDeploy HTTP 契约） |
| 配置格式 | **与 C# 完全互通**：使用同名同格式的 `Config/StarAgent.config`（XML，带中文注释）；C# 特有字段/属性读写均保留；本项目扩展字段以 C# 可忽略的形式写入；支持从旧版 `Agent.toml`（TOML）/`Agent.json` 自动迁移 |
| 未实现功能 | Nginx 配置生成、防火墙端口自动开放、阿里云 DNS、自身升级/修复、`-watch` 看门狗服务 |
| 状态存储 | `data/state.json` 记录运行中 PID（用于接管），原 `Service.csv` 不再使用 |

---

## 11. 开发

```text
src/
├─ main.rs       入口：基础目录/日志/参数预处理
├─ cli.rs        命令行分发 + 控制台菜单
├─ agent.rs      宿主：前台/服务运行、守护与监视定时器、退出清理
├─ manager.rs    应用管理器：多应用守护、控制、状态持久化、看门狗、配置热更新
├─ app.rs        单应用运行时：启动/停止/退出检测/内存/文件变动
├─ deploy.rs     部署：解压、影子目录、可执行文件检索、安全文件替换
├─ server.rs     本地 HTTP 控制接口 + Web 面板路由（dhrust::net 控制器）
├─ webpanel.rs   Web 管理面板：登录鉴权/令牌/限流、/api 与 /star 控制器
├─ web/          面板前端（index.html，编译期内嵌）
├─ service/      平台服务管理（windows / systemd / launchd / unsupported）
├─ sys.rs        平台进程/机器工具（存活/内存/信号/进程枚举/机器信息/工作集回收）
├─ netc.rs       极简 HTTP 客户端 / TCP 连通检查
├─ config.rs     配置模型（Config/StarAgent.config，XML 注释保留/迁移，与 C# 完全互通）
└─ util.rs       基础辅助（路径/日志/通配/参数切分）
```

- 单元测试：`cargo test`（71 项：配置、部署模式、可执行文件检索、影子解压、安全替换、参数切分、僵尸进程判定、面板鉴权（三级级别）/限流/子服务 CRUD/日志/机器信息/整机资源/时间同步/后台采样器/系统采集解析等）；
- 冒烟脚本思路（本机已验证）：临时目录启动 `-run` → `Invoke-RestMethod` 调用接口 → 验证影子目录切换、运行中替换部署包、代理重启后的进程接管、面板登录与各端点。

### 11.1 版本号规范（发布约定）

- 版本号格式为 `主.次.修订`（当前处于 `0.x` 阶段，尚未发布正式稳定版）；
- **每次对外打包必须使用新版本号，绝不重号**：打包前先更新 `Cargo.toml` 的 `version`（打包脚本自动带入包名；程序内 `-status`、Web 面板、`/star/info` 等处同步生效），并顺带更新 README 中的包名示例；
- **修订位 +1（默认）**：每次打包/发版（`0.1.1 → 0.1.2 → …`）；
- **次位 +1（里程碑）**：阶段性大功能定型、或存在不兼容变更（配置格式/协议变化等）时，确认后跳（`0.2.0`、`0.3.0`…）；
- **主位 `1.0.0`**：正式版本（另行约定）。

---

## 12. 路线图

- [ ] StarServer / StarWeb 对接（登录、心跳、指令下发）
- [x] Web 管理面板（子服务 CRUD / 日志 / 配置在线修改 / 看门狗 / 本机信息）
- [x] UDP 本地 RPC（默认 5501；与 C# StarAgent 的 UDP 指令互通、默认端口错开可同时并存；DHDeploy 新版链路实测打通）
- [x] 流量统计（网站：访问日志增量解析+持久化续读；端口：Linux nftables 计数表/连接视图；每日归档 SQLite（Pek.RCode + XCode 模型 Model.xml，旧版 JSON 自动迁移）+ 面板历史趋势）
- [ ] 自身升级（`-upgrade` 完整实现）与 `-repair`
- [ ] Linux 实机验证与发行（systemd 单元模板随包提供）
- [ ] 进程按名称接管 / 多实例精确匹配
- [ ] 多平台支持：国产 Linux（麒麟/统信/openEuler）实机验证、嵌入式/路由系统（OpenWrt/Buildroot）服务化适配、macOS 实机（ARM64 交叉产物已打通；规划见 `docs/platform-support.md`）
