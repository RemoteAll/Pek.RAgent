<#
Pek.RAgent 一键发布打包（Windows 主机）

功能：
  - Windows：本机 MSVC release 构建（target\release\pek-ragent.exe）
  - Linux  ：cargo-zigbuild + zig 交叉编译（静态单文件，musl）
              支持 x86_64 与 aarch64（ARM64，国产化/鲲鹏/飞腾/树莓派等）
  - 产物输出到 dist\（zip / tar.gz / SHA256SUMS.txt）

用法：
  powershell -ExecutionPolicy Bypass -File scripts\build-release.ps1
  可选参数：
    -Targets 默认 all；可多选：-Targets linux,linux-arm64
              all = windows + linux(x86_64) + linux-arm64(aarch64) + linux-riscv64 + linux-loongarch64
    -Clean                       先清理 dist 旧产物与 zig 缓存，再构建
    -CleanAll                    额外执行 cargo clean（清空全部编译缓存，最省磁盘）

前置条件（仅 Linux 交叉构建需要；缺失时脚本自动补齐，完整清单见 docs/build-env.md）：
  - cargo-zigbuild：cargo install --locked cargo-zigbuild      （缺失时自动安装）
  - rustup 目标组件（缺失时自动安装）：
      x86_64 / aarch64 / riscv64gc / loongarch64 的 -unknown-linux-musl
  - zig 可执行文件（查找顺序：CARGO_ZIGBUILD_ZIG_PATH → G:\Tools\zig → 项目 tools\zig
      → 自动从清华 PyPI 镜像下载到 tools\zig）
  国内加速示例（自动安装 Rust 目标组件时可先设置）：
    $env:RUSTUP_DIST_SERVER='https://mirrors.tuna.tsinghua.edu.cn/rustup'
    $env:RUSTUP_UPDATE_ROOT='https://mirrors.tuna.tsinghua.edu.cn/rustup/rustup'

说明：
  release 已启用增量编译（Cargo.toml 的 [profile.release] incremental = true），
  重复打包只重编改动部分；zig 链接时可能提示
  “ignoring deprecated linker optimization setting”，属工具链无害提示。

目标扩展（新架构）：在下方 $linuxTargets 处加一行，并确保 zig 支持该架构即可。
#>

param(
    [ValidateSet('all', 'windows', 'linux', 'linux-arm64', 'linux-riscv64', 'linux-loongarch64')]
    [string[]]$Targets = @('all'),
    [switch]$Clean,
    [switch]$CleanAll
)

# 是否选中某目标（-Targets 支持多选，如 -Targets linux,linux-arm64）
function Test-Want([string]$name) { $Targets -contains 'all' -or $Targets -contains $name }

$ErrorActionPreference = 'Stop'
try { [Console]::OutputEncoding = [Text.Encoding]::UTF8 } catch { }

$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

# 显式启用增量编译（与 Cargo.toml 中的 profile 设置一致）
$env:CARGO_INCREMENTAL = '1'

# zig 版本（自动下载时使用；与 cargo-zigbuild 0.23.4 的已验证组合）
$ZigVersion = '0.16.0'

# 定位 zig.exe：CARGO_ZIGBUILD_ZIG_PATH → G:\Tools\zig → 项目 tools\zig → 自动下载（清华 PyPI 镜像）
function Resolve-Zig {
    $candidates = @()
    if ($env:CARGO_ZIGBUILD_ZIG_PATH) { $candidates += $env:CARGO_ZIGBUILD_ZIG_PATH }
    $candidates += @(Get-ChildItem 'G:\Tools\zig' -Recurse -Filter zig.exe -ErrorAction SilentlyContinue |
                     Sort-Object FullName -Descending | Select-Object -ExpandProperty FullName)
    $localTools = Join-Path $root 'tools\zig'
    $candidates += @(Get-ChildItem $localTools -Recurse -Filter zig.exe -ErrorAction SilentlyContinue |
                     Sort-Object FullName -Descending | Select-Object -ExpandProperty FullName)

    foreach ($c in $candidates) {
        if ($c -and (Test-Path $c)) { return (Resolve-Path $c).Path }
    }

    Write-Host "== 未找到 zig，自动下载 zig $ZigVersion（优先清华 PyPI 镜像，失败自动回退官网） =="
    New-Item -ItemType Directory -Force -Path $localTools | Out-Null
    $ok = $false

    # 1) 清华 PyPI 的 ziglang wheel（本质 zip；curl 失败时用 Invoke-WebRequest 重试）
    $whl = Join-Path $localTools 'ziglang.whl'
    try {
        $simple = Invoke-WebRequest -Uri 'https://pypi.tuna.tsinghua.edu.cn/simple/ziglang/' -UseBasicParsing -TimeoutSec 60
        $line = ($simple.Content -split "`n" | Select-String "ziglang-$ZigVersion-py3-none-win_amd64\.whl" | Select-Object -First 1).ToString()
        $href = [regex]::Match($line, 'href="([^"]+)"').Groups[1].Value
        if (-not $href) { throw '镜像索引中未找到对应 wheel' }
        $url = [Uri]::new([Uri]'https://pypi.tuna.tsinghua.edu.cn/simple/ziglang/', $href).AbsoluteUri

        try {
            curl.exe -L --fail --silent --show-error -o $whl $url
            if ($LASTEXITCODE -ne 0) { throw "curl 下载失败（$LASTEXITCODE）" }
        } catch {
            Write-Host '  curl 失败，改用 Invoke-WebRequest 重试…'
            Invoke-WebRequest -Uri $url -OutFile $whl -UseBasicParsing -TimeoutSec 300
        }

        tar.exe -xf $whl -C $localTools
        if ($LASTEXITCODE -ne 0) { throw 'tar 解压失败' }
        if (Test-Path (Join-Path $localTools 'ziglang')) {
            Rename-Item (Join-Path $localTools 'ziglang') (Join-Path $localTools "zig-$ZigVersion") -ErrorAction SilentlyContinue
            $ok = $true
        }
    } catch {
        Write-Host "  清华镜像不可用：$($_.Exception.Message)"
    } finally {
        Remove-Item $whl -Force -ErrorAction SilentlyContinue
    }

    # 2) 官网 zip 兜底（国内可能较慢）
    if (-not $ok) {
        $zip = Join-Path $localTools 'zig.zip'
        $official = "https://ziglang.org/download/$ZigVersion/zig-x86_64-windows-$ZigVersion.zip"
        try {
            Write-Host "  尝试官网兜底：$official"
            try {
                curl.exe -L --fail --silent --show-error -o $zip $official
                if ($LASTEXITCODE -ne 0) { throw "curl 下载失败（$LASTEXITCODE）" }
            } catch {
                Invoke-WebRequest -Uri $official -OutFile $zip -UseBasicParsing -TimeoutSec 600
            }
            Expand-Archive -LiteralPath $zip -DestinationPath $localTools -Force
            $ok = $true
        } catch {
            Write-Host "  官网兜底也失败：$($_.Exception.Message)"
        } finally {
            Remove-Item $zip -Force -ErrorAction SilentlyContinue
        }
    }

    Remove-Item (Join-Path $localTools 'ziglang-*.dist-info') -Recurse -Force -ErrorAction SilentlyContinue

    $found = (Get-ChildItem $localTools -Recurse -Filter zig.exe -ErrorAction SilentlyContinue |
              Sort-Object FullName -Descending | Select-Object -First 1).FullName
    if ($found) {
        Write-Host "zig 已就绪：$found"
        return $found
    }
    throw "自动下载 zig 失败：请手动准备（详见 docs/build-env.md）：`n  1) 官网 https://ziglang.org/download/ 下载 zig-$ZigVersion 并解压；`n  2) 或清华 PyPI 的 ziglang wheel（zip）解压；`n  然后用环境变量 CARGO_ZIGBUILD_ZIG_PATH 指向 zig.exe，或放到 G:\Tools\zig / 项目 tools\zig"
}

# ---- 清理 ----
if ($Clean -or $CleanAll) {
    Write-Host '== 清理：dist 旧产物 + zig 缓存 =='
    if (Test-Path 'dist') { Remove-Item 'dist\*' -Recurse -Force -ErrorAction SilentlyContinue }
    # 项目内 zig 缓存
    Remove-Item 'target\zig-cache' -Recurse -Force -ErrorAction SilentlyContinue
    # cargo-zigbuild 包装器缓存与 zig 默认缓存残留（可自动重建）
    Remove-Item "$env:LOCALAPPDATA\cargo-zigbuild" -Recurse -Force -ErrorAction SilentlyContinue
    Remove-Item "$env:LOCALAPPDATA\zig" -Recurse -Force -ErrorAction SilentlyContinue
}
if ($CleanAll) {
    Write-Host '== 清理：cargo clean（全部编译缓存，下次全量重建）=='
    cargo clean
    if ($LASTEXITCODE -ne 0) { throw 'cargo clean 失败' }
}

# ---- 版本号 ----
$ver = (Select-String -Path 'Cargo.toml' -Pattern '^version\s*=\s*"([^"]+)"' | Select-Object -First 1).Matches[0].Groups[1].Value
Write-Host "Pek.RAgent v$ver 打包开始（$($Targets -join ', ')）"
New-Item -ItemType Directory -Force -Path 'dist' | Out-Null

# ---- Windows ----
if (Test-Want 'windows') {
    Write-Host '== Windows release 构建（MSVC，增量） =='
    cargo build --release --locked
    if ($LASTEXITCODE -ne 0) { throw 'Windows 构建失败' }

    $zip = "dist\pek-ragent-v$ver-x86_64-pc-windows-msvc.zip"
    Compress-Archive -Path 'target\release\pek-ragent.exe' -DestinationPath $zip -Force
}

# ---- Linux（zig 交叉编译，多目标） ----
$linuxTargets = @()
if (Test-Want 'linux') { $linuxTargets += 'x86_64-unknown-linux-musl' }
if (Test-Want 'linux-arm64') { $linuxTargets += 'aarch64-unknown-linux-musl' }
if (Test-Want 'linux-riscv64') { $linuxTargets += 'riscv64gc-unknown-linux-musl' }
if (Test-Want 'linux-loongarch64') { $linuxTargets += 'loongarch64-unknown-linux-musl' }

if ($linuxTargets.Count -gt 0) {
    # 前置：cargo-zigbuild（缺失则自动安装，走工程内 rsproxy 镜像）
    if (-not (Get-Command cargo-zigbuild -ErrorAction SilentlyContinue)) {
        Write-Host '== 缺少 cargo-zigbuild，自动安装：cargo install --locked cargo-zigbuild =='
        cargo install --locked cargo-zigbuild
        if ($LASTEXITCODE -ne 0) {
            throw 'cargo-zigbuild 安装失败：请手动执行 cargo install --locked cargo-zigbuild 后重试（见 docs/build-env.md）'
        }
    }

    # 前置：zig（CARGO_ZIGBUILD_ZIG_PATH → G:\Tools\zig → 项目 tools\zig → 自动下载）
    $zig = Resolve-Zig
    $env:CARGO_ZIGBUILD_ZIG_PATH = $zig
    Write-Host "zig：$zig"

    # python 打包助手（tar.gz 需要执行位；Windows 自带 bsdtar 无法设置权限）
    $py = $null
    foreach ($cand in @('py', 'python')) {
        $src = (Get-Command $cand -ErrorAction SilentlyContinue).Source
        if ($src -and ($src -notlike '*WindowsApps*')) { $py = $cand; break }
    }
    $helper = Join-Path $env:TEMP 'pek-ragent-tar.py'
    if ($py) {
        $pyCode = @'
import io, tarfile, time, sys
# 用法：python helper <dst.tar.gz> <path> <name> [<path> <name> ...]
# .sh 文件行尾归一化为 LF（Windows 工作区可能是 CRLF）；统一 0755/root 属主
dst, args = sys.argv[1], sys.argv[2:]
with tarfile.open(dst, 'w:gz') as t:
    for path, name in zip(args[0::2], args[1::2]):
        with open(path, 'rb') as f:
            data = f.read()
        if name.endswith('.sh'):
            data = data.replace(b'\r\n', b'\n')
        ti = tarfile.TarInfo(name)
        ti.size = len(data)
        ti.mode = 0o755
        ti.mtime = int(time.time())
        ti.uname = 'root'
        ti.gname = 'root'
        t.addfile(ti, io.BytesIO(data))
'@
        Set-Content -Path $helper -Value $pyCode -Encoding ASCII
    }

    $installed = (rustup target list --installed) -join ' '
    foreach ($t in $linuxTargets) {
        if ($installed -notlike "*$t*") {
            Write-Host "== 首次需要：rustup target add $t =="
            rustup target add $t
            if ($LASTEXITCODE -ne 0) { throw "缺少目标 $t：请先执行 rustup target add $t 后重试" }
        }

        Write-Host "== Linux 交叉构建：$t（cargo-zigbuild，增量） =="
        cargo zigbuild --release --locked --target $t
        if ($LASTEXITCODE -ne 0) { throw "交叉构建失败：$t" }

        $bin = "target\$t\release\pek-ragent"
        $tgz = "dist\pek-ragent-v$ver-$t.tar.gz"
        $packed = $false
        if ($py) {
            # 附带 install.sh：`bash install.sh` 一键补可执行位并安装（解决 scp/网盘传输丢可执行位）
            & $py $helper $tgz $bin 'pek-ragent' 'packaging\install.sh' 'install.sh'
            $packed = ($LASTEXITCODE -eq 0)
        }
        if (-not $packed) {
            Write-Warning '未找到可用的 python：tar.gz 内文件不带执行位，解压后请手动 chmod +x pek-ragent'
            tar -czf $tgz -C (Split-Path $bin) 'pek-ragent'
        }
    }
    if ($py) { Remove-Item $helper -Force -ErrorAction SilentlyContinue }
}

# ---- 校验和（覆盖 dist 内全部产物，支持按目标增量构建） ----
$shaLines = Get-ChildItem 'dist\*.zip', 'dist\*.tar.gz' -ErrorAction SilentlyContinue |
    Sort-Object Name |
    ForEach-Object { (Get-FileHash $_.FullName -Algorithm SHA256).Hash.ToLower() + '  ' + $_.Name }
if ($shaLines) {
    Set-Content -Path 'dist\SHA256SUMS.txt' -Value $shaLines -Encoding ASCII
}

Write-Host ''
Write-Host '== 打包完成，产物（dist） =='
Get-ChildItem 'dist' | Sort-Object Name | Format-Table Name, @{ n = 'KB'; e = { [math]::Round($_.Length / 1KB, 0) } } -AutoSize | Out-String | Write-Host
Write-Host '提示：-Clean 清理 zig 缓存与旧产物；-CleanAll 额外清空 target（全量重建、最省空间）。'
