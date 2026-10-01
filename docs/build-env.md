# 构建环境准备（新机器清单）

> 目标：任意一台新的 Windows 开发机，几分钟内备齐"Windows + 多架构 Linux"打包环境。
> 提示：`scripts\build-release.ps1` 已内置**自动补齐**（rustup 目标组件、cargo-zigbuild、zig），
> 新机器通常直接跑打包脚本即可；本清单用于离线环境、排障，或想了解每一步细节时参考。

## 0. 最短路径（脚本自动版）

```powershell
powershell -ExecutionPolicy Bypass -File scripts\build-release.ps1
# 缺失的组件自动安装/下载（rustup 目标 / cargo-zigbuild / zig）；产物输出到 dist\
```

## 1. 依赖清单

| 组件 | 用途 | 要求 | 参考版本 |
|------|------|------|----------|
| Rust（rustup + MSVC 工具链） | 本机编译/链接 | 1.80+（edition 2024） | 1.98.x（stable-x86_64-pc-windows-msvc） |
| Visual Studio Build Tools（MSVC） | Windows 链接器 link.exe | VS2019+ | 本机已具备（cargo build 正常） |
| cargo-zigbuild | Linux 交叉编译封装 | 0.23.x | 0.23.4 |
| zig | 交叉链接器（自带 libc） | 0.13+ | 0.16.0 |
| rustup 目标组件（musl） | Linux 各架构标准库 | — | x86_64 / aarch64 / riscv64gc / loongarch64 |
| python（可选） | tar.gz 打包时写入执行位 | 3.x | 3.12 |

## 2. 逐条安装命令

### 2.1 Rust 目标组件（国内建议 TUNA 镜像）

```powershell
$env:RUSTUP_DIST_SERVER = 'https://mirrors.tuna.tsinghua.edu.cn/rustup'
$env:RUSTUP_UPDATE_ROOT = 'https://mirrors.tuna.tsinghua.edu.cn/rustup/rustup'
rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl riscv64gc-unknown-linux-musl loongarch64-unknown-linux-musl
```

### 2.2 cargo-zigbuild

```powershell
cargo install --locked cargo-zigbuild
# 工程内 .cargo/config.toml 已把 crates.io 指向 rsproxy 镜像，无需额外配置
```

### 2.3 zig（三选一）

**方式 A（国内推荐）：清华 PyPI 的 `ziglang` wheel —— wheel 本质是 zip，解压即得完整 zig**

```powershell
# 示例版本 0.16.0（与 cargo-zigbuild 0.23.4 的已验证组合）
$r = Invoke-WebRequest 'https://pypi.tuna.tsinghua.edu.cn/simple/ziglang/' -UseBasicParsing
$href = ($r.Content -split "`n" | Select-String 'ziglang-0\.16\.0-py3-none-win_amd64\.whl' | Select-Object -First 1).ToString()
$url = [regex]::Match($href, 'href="([^"]+)"').Groups[1].Value
$url = [Uri]::new([Uri]'https://pypi.tuna.tsinghua.edu.cn/simple/ziglang/', $url).AbsoluteUri
curl.exe -L --fail -o ziglang.whl $url
tar -xf ziglang.whl     # 解压出 ziglang\ 目录（含完整 zig 发行版）
# 之后将 ziglang\zig.exe 的路径用于 CARGO_ZIGBUILD_ZIG_PATH（或放入 G:\Tools\zig / tools\zig）
```

**方式 B：官网下载** `https://ziglang.org/download/`（如 `zig-x86_64-windows-0.16.0.zip`；国内较慢）

**方式 C：pip** `pip install ziglang` 后，设置 `CARGO_ZIGBUILD_PYTHON_PATH` 指向 python（cargo-zigbuild 支持从 pip 包调用 zig）

### 2.4（可选）python

tar.gz 里需要给二进制写入"执行位"；Windows 自带 bsdtar 做不到，打包脚本用 python 的 tarfile 处理。
无 python 时脚本仍可打包，但需在 Linux 解压后 `chmod +x pek-ragent`。

## 3. zig 查找顺序（打包脚本行为）

1. 环境变量 `CARGO_ZIGBUILD_ZIG_PATH`
2. `G:\Tools\zig`（本机惯例目录）
3. 项目内 `tools\zig`（脚本自动下载的位置，已 gitignore）
4. 都没有 → 自动从清华 PyPI 镜像下载到 `tools\zig`（curl 失败自动改用 Invoke-WebRequest 重试；镜像不可用时自动回退官网 zip）。

## 4. 验证

```powershell
# 任一（或全部）目标
powershell -ExecutionPolicy Bypass -File scripts\build-release.ps1 -Targets linux-riscv64

# 期望：dist\ 出现对应 tar.gz（内含带执行位的 pek-ragent），无代码编译警告
```

## 5. 常见问题

| 现象 | 处理 |
|------|------|
| `error: no such command: zigbuild` | `cargo install --locked cargo-zigbuild`（打包脚本会自动安装） |
| 找不到 zig | 见第 3 节查找顺序；或 `$env:CARGO_ZIGBUILD_ZIG_PATH='...\zig.exe'` |
| rustup 组件下载慢/卡住 | 用 2.1 的 TUNA 镜像环境变量（备选 rsproxy：`https://rsproxy.cn`） |
| 无法从官网下载 zig | 用 2.3 方式 A（清华 PyPI，实测 94MB 秒级） |
| 提示 `ignoring deprecated linker optimization setting` | zig 链接器无害提示，可忽略 |
| tar.gz 解压后不能直接运行 | 打包需 python（见 2.4）；或解压后 `chmod +x pek-ragent` |
| 首次打包慢 / 磁盘增长 | 正常（依赖全量编译）；缓存可用 `-Clean` / `-CleanAll` 回收，见 README §2.1 |
