# RAgent「一键加入白名单」冒烟（隔离实例 + 临时 vhost；验证 certAllow 追加/幂等/落盘）
# 用法：powershell -NoProfile -ExecutionPolicy Bypass -File scripts\_e2e-cert-allow.ps1 [-Port 5505]

param(
    [int]$Port = 5505
)
$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
$exe = Join-Path $repo 'target\debug\pek-ragent.exe'
if (-not (Test-Path $exe)) { throw "未找到调试产物：$exe（先运行 cargo build）" }

$root = Join-Path $env:TEMP ("ragent-allow-e2e-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
$base = Join-Path $root 'agent'
$vhost = Join-Path $root 'vhost'
New-Item -ItemType Directory -Force (Join-Path $base 'Config'), $vhost | Out-Null
$conf = Join-Path $vhost 'zx.hlktech.com.conf'
$confText = @"
server {
    listen 80;
    server_name zx.hlktech.com;
    root C:/www/zx;
}
"@
[IO.File]::WriteAllText($conf, $confText, (New-Object Text.UTF8Encoding($false)))
$fakeNginx = Join-Path $root 'nginx.cmd'
[IO.File]::WriteAllText($fakeNginx, "@echo off`r`nexit /b 0`r`n", (New-Object Text.UTF8Encoding($false)))

# 白名单留空 = 全部站点只读（复现用户场景）
$xml = @"
<StarAgent>
  <LocalPort>$Port</LocalPort>
  <LocalOnly>true</LocalOnly>
  <WebUserName>admin</WebUserName>
  <WebPassword>admin</WebPassword>
  <WebAuthLevel>LocalOnly</WebAuthLevel>
  <AutoUpgradeUrl></AutoUpgradeUrl>
  <PairingEnabled>false</PairingEnabled>
  <CertManagedSites></CertManagedSites>
  <CertNginxDirs>$vhost</CertNginxDirs>
  <CertNginxBin>$fakeNginx</CertNginxBin>
</StarAgent>
"@
$cfgPath = Join-Path $base 'Config\StarAgent.config'
[IO.File]::WriteAllText($cfgPath, $xml, (New-Object Text.UTF8Encoding($false)))

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
function Allow([string[]]$keys) {
    $body = @{ sites = $keys } | ConvertTo-Json
    return Invoke-RestMethod -Method Post -Uri "$origin/star/certAllow" -Body $body -ContentType 'application/json'
}

try {
    Wait-Up
    $s = Invoke-RestMethod "$origin/star/sites"
    if ($s.code -ne 0) { throw "盘点失败：$($s.message)" }
    $list = @($s.data.inventory.sites)
    if ($list.Count -ne 1) { throw "应发现 1 个站点，实际 $($list.Count)" }
    $site = $list[0]
    if ($site.managed) { throw "白名单为空时站点不应为已托管" }
    "1) 初始盘点：key=$($site.key) managed=$($site.managed)（全部只读，符合安全默认）"

    $r = Allow @($site.key)
    if ($r.code -ne 0) { throw "加入白名单失败：$($r.message)" }
    "2) 加入白名单：$($r.message)"

    $s2 = Invoke-RestMethod "$origin/star/sites"
    $site2 = @($s2.data.inventory.sites)[0]
    if (-not $site2.managed) { throw "加入后站点仍非已托管" }
    $wl = [string]$s2.data.whitelist
    if ($wl -notlike "*$($site.key)*") { throw "白名单内容异常：$wl" }
    "3) 刷新后：managed=$($site2.managed) whitelist=$wl"

    $xmlText = [IO.File]::ReadAllText($cfgPath)
    if ($xmlText -notmatch [regex]::Escape($site.key)) { throw "白名单未写入配置文件：`n$xmlText" }
    "4) 已落盘：Config\StarAgent.config 含该站点 key"

    $r2 = Allow @($site.key)
    $s3 = Invoke-RestMethod "$origin/star/sites"
    $wl2 = [string]$s3.data.whitelist
    if ($wl2.Length -ne $wl.Length) { throw "重复加入导致白名单变化：$wl -> $wl2" }
    "5) 幂等：重复加入未改变白名单（$($r2.message)）"

    $bad = Allow @('')
    if ($bad.code -eq 0) { throw "空站点应被拒绝" }
    "6) 空参数被拒绝：$($bad.message)"

    "=== CERT ALLOW E2E PASSED ==="
}
finally {
    if ($proc -and -not $proc.HasExited) { Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue }
    Remove-Item Env:\PEK_RAGENT_BASE -ErrorAction SilentlyContinue
    if (-not $env:KEEP_E2E) { Remove-Item $root -Recurse -Force -ErrorAction SilentlyContinue }
}
