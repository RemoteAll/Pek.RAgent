<#
Pek.RAgent 一键发布打包（Windows 主机）

功能：
  - Windows：本机 MSVC release 构建（target\release\pek-ragent.exe）
  - Linux  ：cargo-zigbuild + zig 交叉编译（x86_64-unknown-linux-musl，静态单文件）
  - 产物输出到 dist\（zip / tar.gz / SHA256SUMS.txt）

用法：
  powershell -ExecutionPolicy Bypass -File scripts\build-release.ps1
  可选参数：
    -Targets all|windows|linux   默认 all
    -Clean                       先清理 dist 旧产物与 zig 缓存，再构建
    -CleanAll                    额外执行 cargo clean（清空全部编译缓存，最省磁盘）

前置条件（仅 Linux 交叉构建需要）：
  cargo install --locked cargo-zigbuild
  rustup target add x86_64-unknown-linux-musl
  zig 可执行文件（本机位于 G:\Tools\zig\zig-0.16.0\zig.exe；
  也可用环境变量 CARGO_ZIGBUILD_ZIG_PATH 指定）

说明：
  release 已启用增量编译（Cargo.toml 的 [profile.release] incremental = true），
  重复打包只重编改动部分；zig 链接时可能提示
  “ignoring deprecated linker optimization setting”，属工具链无害提示。
#>

param(
    [ValidateSet('all', 'windows', 'linux')]
    [string]$Targets = 'all',
    [switch]$Clean,
    [switch]$CleanAll
)

$ErrorActionPreference = 'Stop'
try { [Console]::OutputEncoding = [Text.Encoding]::UTF8 } catch { }

$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

# 显式启用增量编译（与 Cargo.toml 中的 profile 设置一致）
$env:CARGO_INCREMENTAL = '1'

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
Write-Host "Pek.RAgent v$ver 打包开始（$Targets）"
New-Item -ItemType Directory -Force -Path 'dist' | Out-Null
$shaLines = New-Object System.Collections.Generic.List[string]

# ---- Windows ----
if ($Targets -eq 'all' -or $Targets -eq 'windows') {
    Write-Host '== Windows release 构建（MSVC，增量） =='
    cargo build --release --locked
    if ($LASTEXITCODE -ne 0) { throw 'Windows 构建失败' }

    $zip = "dist\pek-ragent-v$ver-x86_64-pc-windows-msvc.zip"
    Compress-Archive -Path 'target\release\pek-ragent.exe' -DestinationPath $zip -Force
    $shaLines.Add((Get-FileHash $zip -Algorithm SHA256).Hash.ToLower() + '  ' + (Split-Path $zip -Leaf))
}

# ---- Linux（zig 交叉编译） ----
if ($Targets -eq 'all' -or $Targets -eq 'linux') {
    # 定位 zig
    $zig = $env:CARGO_ZIGBUILD_ZIG_PATH
    if (-not $zig -or -not (Test-Path $zig)) {
        $zig = (Get-ChildItem 'G:\Tools\zig' -Recurse -Filter zig.exe -ErrorAction SilentlyContinue |
                Sort-Object FullName -Descending | Select-Object -First 1).FullName
    }
    if (-not $zig) {
        throw '未找到 zig.exe：请安装 zig（如 G:\Tools\zig）或设置环境变量 CARGO_ZIGBUILD_ZIG_PATH 后重试'
    }
    $env:CARGO_ZIGBUILD_ZIG_PATH = $zig
    Write-Host "zig：$zig"

    Write-Host '== Linux musl 交叉构建（cargo-zigbuild，增量） =='
    cargo zigbuild --release --locked --target x86_64-unknown-linux-musl
    if ($LASTEXITCODE -ne 0) { throw 'Linux 交叉构建失败' }

    $bin = 'target\x86_64-unknown-linux-musl\release\pek-ragent'
    $tgz = "dist\pek-ragent-v$ver-x86_64-unknown-linux-musl.tar.gz"

    # 用 python 的 tarfile 打包，保证文件带执行位（Windows 自带 bsdtar 无法设置权限）
    $py = $null
    foreach ($cand in @('py', 'python')) {
        $src = (Get-Command $cand -ErrorAction SilentlyContinue).Source
        if ($src -and ($src -notlike '*WindowsApps*')) { $py = $cand; break }
    }
    $packed = $false
    if ($py) {
        $helper = Join-Path $env:TEMP 'pek-ragent-tar.py'
        $pyCode = @'
import os, tarfile, time, sys
src, dst, name = sys.argv[1], sys.argv[2], sys.argv[3]
ti = tarfile.TarInfo(name)
ti.size = os.path.getsize(src)
ti.mode = 0o755
ti.mtime = int(time.time())
ti.uname = 'root'
ti.gname = 'root'
with open(src, 'rb') as f, tarfile.open(dst, 'w:gz') as t:
    t.addfile(ti, f)
'@
        Set-Content -Path $helper -Value $pyCode -Encoding ASCII
        & $py $helper $bin $tgz 'pek-ragent'
        $packed = ($LASTEXITCODE -eq 0)
        Remove-Item $helper -Force -ErrorAction SilentlyContinue
    }
    if (-not $packed) {
        Write-Warning '未找到可用的 python：tar.gz 内文件不带执行位，解压后请手动 chmod +x pek-ragent'
        tar -czf $tgz -C (Split-Path $bin) 'pek-ragent'
    }
    $shaLines.Add((Get-FileHash $tgz -Algorithm SHA256).Hash.ToLower() + '  ' + (Split-Path $tgz -Leaf))
}

# ---- 校验和 + 汇总 ----
if ($shaLines.Count -gt 0) {
    Set-Content -Path 'dist\SHA256SUMS.txt' -Value $shaLines -Encoding ASCII
}

Write-Host ''
Write-Host '== 打包完成，产物（dist） =='
Get-ChildItem 'dist' | Sort-Object Name | Format-Table Name, @{ n = 'KB'; e = { [math]::Round($_.Length / 1KB, 0) } } -AutoSize | Out-String | Write-Host
Write-Host '提示：-Clean 清理 zig 缓存与旧产物；-CleanAll 额外清空 target（全量重建、最省空间）。'
