# Pek.RAgent 一键回归（单测 → 构建 → 星尘迁移 e2e → 应用包部署 e2e）
#
# 用法：
#   powershell -NoProfile -ExecutionPolicy Bypass -File tools\regression.ps1
#   ... -SkipBuild         跳过 cargo build
#   ... -Only test,build   只跑指定步骤（test,build,migrate,appdeploy）
#
# 约定（见全局「回归脚本与测试门槛」）：
#   - 退出码 0/1 可直接接 CI；报告落 artifacts\regression\report-<时间戳>.txt
#   - 每步有**通过数下限**（只能抬高，降低要在决策记录写明依据）
#   - 已知缺口标 KNOWN / 缺工件标 SKIP，都不计入 PASS，且必须写明原因
#   - 本文件含中文，必须 UTF-8 with BOM（PS 5.1 按 ANSI 读无 BOM 会语法报错）
param(
    [switch]$SkipBuild,
    [string[]]$Only = @()
)

$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
Set-Location $root

$stamp = (Get-Date).ToString('yyyyMMdd-HHmmss')
$reportDir = Join-Path $root 'artifacts\regression'
New-Item -ItemType Directory -Force $reportDir | Out-Null
$report = Join-Path $reportDir ("report-$stamp.txt")

# 通过数下限（2026-11-10 实测；只允许抬高）
$Baseline = @{
    test     = 233  # cargo test --bin pek-ragent（233 passed / 2 ignored）
    migrate  = 25   # scripts/_smoke-migrate.ps1（25 passed, 0 failed）
    appdeploy = 0   # 需 DHDeploy Agent 发布包；缺件即 SKIP
}

$results = New-Object System.Collections.ArrayList

function Write-Report {
    param([string]$Text)
    Write-Host $Text
    Add-Content -Path $report -Value $Text -Encoding UTF8
}

# 调外部命令：$ErrorActionPreference='Stop' 会把程序正常写 stderr 当终止错误（PS 5.1 实战坑）
function Invoke-External {
    param([string]$Exe, [string[]]$ArgList)
    $prev = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        $lines = & $Exe @ArgList 2>&1 | ForEach-Object { "$_" }
        $code = $LASTEXITCODE
    } finally {
        $ErrorActionPreference = $prev
    }
    return @{ Code = $code; Text = ($lines -join "`n") }
}

function Add-Result {
    param([string]$Name, [string]$Status, [string]$Detail)
    [void]$results.Add([pscustomobject]@{ Name = $Name; Status = $Status; Detail = $Detail })
    $icon = switch ($Status) { 'PASS' { '✅' } 'FAIL' { '❌' } 'SKIP' { '⏭' } 'KNOWN' { '⚠' } default { '·' } }
    Write-Report ("$icon {0} — {1} {2}" -f $Name, $Status, $Detail)
}

$want = { param($k) ($Only.Count -eq 0) -or ($Only -contains $k) }

Write-Report '== Pek.RAgent 一键回归 =='
Write-Report ("时间：{0}" -f (Get-Date).ToString('yyyy-MM-dd HH:mm:ss'))
Write-Report ("根目录：{0}" -f $root)
Write-Report ''

$prevEnc = [Console]::OutputEncoding
try {
    [Console]::OutputEncoding = [Text.Encoding]::UTF8

    # ———— 1) cargo test ————
    if (& $want 'test') {
        Write-Report '— cargo test（Agent 单测）—'
        $r = Invoke-External 'cargo' @('test', '--bin', 'pek-ragent')
        Add-Content -Path $report -Value $r.Text -Encoding UTF8
        $passed = 0
        if ($r.Text -match 'test result: ok\. (\d+) passed; (\d+) failed') {
            $passed = [int]$Matches[1]
            $failedCount = [int]$Matches[2]
        }
        if ($r.Code -ne 0) {
            Add-Result 'cargo test' 'FAIL' "退出码 $($r.Code)；日志见报告"
        } elseif ($passed -lt $Baseline.test) {
            Add-Result 'cargo test' 'FAIL' "通过 $passed 项 < 下限 $($Baseline.test)"
        } else {
            Add-Result 'cargo test' 'PASS' "通过 $passed 项（下限 $($Baseline.test)）"
        }
        Write-Report ''
    }

    # ———— 2) cargo build ————
    if ($SkipBuild) {
        Add-Result 'cargo build' 'SKIP' '指定了 -SkipBuild'
    } elseif (& $want 'build') {
        Write-Report '— cargo build（debug 可执行文件）—'
        $r = Invoke-External 'cargo' @('build', '--bin', 'pek-ragent')
        Add-Content -Path $report -Value $r.Text -Encoding UTF8
        if ($r.Code -ne 0) { Add-Result 'cargo build' 'FAIL' "退出码 $($r.Code)" }
        else { Add-Result 'cargo build' 'PASS' 'debug exe 已就绪' }
        Write-Report ''
    }

    # ———— 3) 星尘迁移 e2e（自建 5598 实例）————
    if (& $want 'migrate') {
        $path = Join-Path $root 'scripts\_smoke-migrate.ps1'
        if (-not (Test-Path $path)) {
            Add-Result '星尘迁移 e2e' 'SKIP' '脚本不存在：scripts/_smoke-migrate.ps1'
        } else {
            Write-Report '— 星尘迁移 e2e（_smoke-migrate.ps1）—'
            $r = Invoke-External 'powershell' @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', $path)
            Add-Content -Path $report -Value $r.Text -Encoding UTF8
            $passed = 0
            if ($r.Text -match 'RESULT:\s*(\d+) passed,\s*(\d+) failed') {
                $passed = [int]$Matches[1]
                $bad = [int]$Matches[2]
            }
            if ($r.Code -ne 0 -or $passed -lt $Baseline.migrate) {
                Add-Result '星尘迁移 e2e' 'FAIL' "退出码 $($r.Code)，通过 $passed 项（下限 $($Baseline.migrate)）；日志见报告"
            } else {
                Add-Result '星尘迁移 e2e' 'PASS' "通过 $passed 项（下限 $($Baseline.migrate)）"
            }
            Write-Report ''
        }
    }

    # ———— 4) 应用包部署 e2e（需要 DHDeploy Agent 发布包）————
    if (& $want 'appdeploy') {
        $path = Join-Path $root 'scripts\_smoke-appdeploy.ps1'
        if (-not (Test-Path $path)) {
            Add-Result '应用包部署 e2e' 'SKIP' '脚本不存在：scripts/_smoke-appdeploy.ps1'
        } else {
            Write-Report '— 应用包部署 e2e（_smoke-appdeploy.ps1）—'
            $r = Invoke-External 'powershell' @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', $path)
            Add-Content -Path $report -Value $r.Text -Encoding UTF8
            if ($r.Text -like '*SKIP:*') {
                $why = ($r.Text -split "`n" | Where-Object { $_ -like '*SKIP:*' } | Select-Object -First 1)
                Add-Result '应用包部署 e2e' 'SKIP' $why.Trim()
            } elseif ($r.Code -ne 0) {
                Add-Result '应用包部署 e2e' 'FAIL' "退出码 $($r.Code)；日志见报告"
            } else {
                Add-Result '应用包部署 e2e' 'PASS' '脚本自检通过'
            }
            Write-Report ''
        }
    }
} finally {
    [Console]::OutputEncoding = $prevEnc
}

# ———— 已知缺口（不计入 PASS，但必须显式列出原因）————
Add-Result '文件管理 smoke' 'KNOWN' 'scripts/_smoke-fileman.ps1 需外部 5599 实例且接口已变（2026-11-10 实测 27/30），待修'
Add-Result '用户/权限 smoke' 'KNOWN' 'scripts/_smoke-users.ps1 需外部 5599 实例且接口已变（2026-11-10 实测 6/43），待修'

$pass = @($results | Where-Object { $_.Status -eq 'PASS' }).Count
$fail = @($results | Where-Object { $_.Status -eq 'FAIL' }).Count
$skip = @($results | Where-Object { $_.Status -eq 'SKIP' }).Count
$known = @($results | Where-Object { $_.Status -eq 'KNOWN' }).Count
Write-Report ''
Write-Report '================ 汇总 ================'
Write-Report ("共 {0} 步：通过 {1}，失败 {2}，跳过 {3}，已知缺口 {4}" -f $results.Count, $pass, $fail, $skip, $known)
if ($fail -gt 0) {
    Write-Report '失败项：'
    foreach ($x in ($results | Where-Object { $_.Status -eq 'FAIL' })) {
        Write-Report ("  - {0}：{1}" -f $x.Name, $x.Detail)
    }
}
Write-Report ("报告：{0}" -f $report)

if ($fail -gt 0) { exit 1 }
exit 0
