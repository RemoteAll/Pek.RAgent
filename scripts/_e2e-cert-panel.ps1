# RAgent 面板证书页冒烟（隔离实例；不触碰本机 5501 生产实例）
# 覆盖：启动 → /star/sites 盘点 → /star/certConfig 保存白名单 → /star/certDeploy 演练失败提示
# 用法：powershell -NoProfile -ExecutionPolicy Bypass -File scripts\_e2e-cert-panel.ps1 [-Port 5503]

param(
    [int]$Port = 5503
)
$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
$exe = Join-Path $repo 'target\debug\pek-ragent.exe'
if (-not (Test-Path $exe)) { throw "未找到调试产物：$exe（先运行 cargo build）" }

$base = Join-Path $env:TEMP ("ragent-cert-e2e-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
New-Item -ItemType Directory -Force (Join-Path $base 'Config') | Out-Null
$xml = @"
<StarAgent>
  <LocalPort>$Port</LocalPort>
  <LocalOnly>true</LocalOnly>
  <WebUserName>admin</WebUserName>
  <WebPassword>admin</WebPassword>
  <WebAuthLevel>LocalOnly</WebAuthLevel>
  <AutoUpgradeUrl></AutoUpgradeUrl>
  <PairingEnabled>false</PairingEnabled>
</StarAgent>
"@
[IO.File]::WriteAllText((Join-Path $base 'Config\StarAgent.config'), $xml, (New-Object Text.UTF8Encoding($false)))

$env:PEK_RAGENT_BASE = $base
$proc = Start-Process -FilePath $exe -ArgumentList '-run' -PassThru -WindowStyle Hidden
$origin = "http://127.0.0.1:$Port"
function Wait-Up {
    for ($i = 0; $i -lt 80; $i++) {
        try { $r = Invoke-RestMethod "$origin/api/me" -TimeoutSec 2; if ($null -ne $r) { return } } catch { }
        Start-Sleep -Milliseconds 250
    }
    throw "RAgent 未在 20 秒内就绪"
}

try {
    Wait-Up
    "1) RAgent 已启动：$origin"

    $sites = Invoke-RestMethod "$origin/star/sites"
    if ($sites.code -ne 0) { throw "盘点失败：$($sites.message)" }
    if ($null -eq $sites.data.inventory) { throw "盘点缺少 inventory" }
    "2) 盘点正常：web=[$($sites.data.inventory.web -join ',')] sites=$($sites.data.inventory.sites.Count)"

    $cfgBody = @{ managedSites = '*.e2e.local'; nginxDirs = ''; nginxBin = '' } | ConvertTo-Json
    $save = Invoke-RestMethod -Method Post -Uri "$origin/star/certConfig" -Body $cfgBody -ContentType 'application/json'
    if ($save.code -ne 0) { throw "保存白名单失败：$($save.message)" }
    $again = Invoke-RestMethod "$origin/star/sites"
    if ($again.data.whitelist -ne '*.e2e.local') { throw "白名单未生效：$($again.data.whitelist)" }
    "3) 白名单保存并生效：$($again.data.whitelist)"

    # 自签测试证书
    $ssl = 'C:\Program Files\Git\usr\bin\openssl.exe'
    if (-not (Test-Path $ssl)) { throw "未找到 openssl：$ssl" }
    $certPem = Join-Path $base 'cert.pem'
    $keyPem = Join-Path $base 'key.pem'
    $cmd = '"' + $ssl + '" req -x509 -newkey rsa:2048 -nodes -keyout "' + $keyPem + '" -out "' + $certPem + '" -days 30 -subj "/CN=*.e2e.local" -addext "subjectAltName=DNS:*.e2e.local" 2>nul'
    cmd /c $cmd | Out-Null

    $body = @{
        certId   = 'e2e-local'
        certName = '本机演练'
        certPem  = [IO.File]::ReadAllText($certPem)
        keyPem   = [IO.File]::ReadAllText($keyPem)
        sites    = @('nginx:/tmp/e2e-not-exist.conf#1')
        action   = 'auto'
        dryRun   = $true
    } | ConvertTo-Json
    $deploy = Invoke-RestMethod -Method Post -Uri "$origin/star/certDeploy" -Body $body -ContentType 'application/json'
    if ($deploy.code -ne 0) { throw "演练请求失败：$($deploy.message)" }
    $r = $deploy.data
    if ($r.ok) { throw "不存在的站点不应被视为成功" }
    if (-not ($r.items[0].message -like '*站点不存在*')) { throw "失败原因不符合预期：$($r.items[0].message)" }
    "4) 演练拒绝不存在的站点：$($r.items[0].message)"

    $lastFile = Join-Path $base 'Data\CertLastResult.json'
    if (-not (Test-Path $lastFile)) { throw "未写入 CertLastResult.json" }
    "5) 最近结果已落盘"

    "=== RAGENT CERT PANEL E2E PASSED ==="
}
finally {
    try { if ($proc -and -not $proc.HasExited) { Stop-Process -Id $proc.Id -Force } } catch { }
    Remove-Item Env:PEK_RAGENT_BASE -ErrorAction SilentlyContinue
    Start-Sleep -Milliseconds 300
    Remove-Item -Recurse -Force $base -ErrorAction SilentlyContinue
}
