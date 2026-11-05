# 造 N 个 nginx 站点（含证书）+ 起隔离实例，测一次站点盘点耗时
# 用法：powershell -NoProfile -ExecutionPolicy Bypass -File scripts\_bench-sites.ps1 [-Port 5507] [-Count 50]

param(
    [int]$Port = 5507,
    [int]$Count = 50
)
$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
$exe = Join-Path $repo 'target\debug\pek-ragent.exe'
if (-not (Test-Path $exe)) { throw "未找到调试产物：$exe（先运行 cargo build）" }
$ssl = 'C:\Program Files\Git\usr\bin\openssl.exe'
if (-not (Test-Path $ssl)) { throw "未找到 openssl：$ssl" }

$root = Join-Path $env:TEMP 'ragent-sites-bench'
if (Test-Path $root) { Remove-Item $root -Recurse -Force }
$base = Join-Path $root 'agent'
$vhost = Join-Path $root 'vhost'
$certDir = Join-Path $root 'certs'
New-Item -ItemType Directory -Force (Join-Path $base 'Config'), $vhost, $certDir | Out-Null

# 一张证书复制 N 份（解析开销与真实一致；生成只做一次）
$oneCert = Join-Path $certDir 'one.pem'
$oneKey = Join-Path $certDir 'one.key'
cmd /c ('"' + $ssl + '" req -x509 -newkey rsa:2048 -nodes -keyout "' + $oneKey + '" -out "' + $oneCert + '" -days 90 -subj "/CN=bench.example.com" -addext "subjectAltName=DNS:*.example.com" 2>nul') | Out-Null

for ($i = 1; $i -le $Count; $i++) {
    $cert = Join-Path $certDir ("site$i.pem")
    Copy-Item $oneCert $cert
    # 一半站点启用 HTTPS（真实场景中多数站点已配证书）
    $sslBlock = if ($i % 2 -eq 0) { "`n    listen 443 ssl;`n    ssl_certificate $cert;`n    ssl_certificate_key $oneKey;" } else { '' }
    $conf = "server {`n    listen 80;`n    server_name site$i.example.com;$sslBlock`n    root C:/www/site$i;`n}`n"
    [IO.File]::WriteAllText((Join-Path $vhost "site$i.conf"), $conf, (New-Object Text.UTF8Encoding($false)))
}
$fakeNginx = Join-Path $root 'nginx.cmd'
[IO.File]::WriteAllText($fakeNginx, "@echo off`r`nexit /b 0`r`n", (New-Object Text.UTF8Encoding($false)))

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
[IO.File]::WriteAllText((Join-Path $base 'Config\StarAgent.config'), $xml, (New-Object Text.UTF8Encoding($false)))

$env:PEK_RAGENT_BASE = $base
$proc = Start-Process -FilePath $exe -ArgumentList '-run' -PassThru -WindowStyle Hidden
$origin = "http://127.0.0.1:$Port"
try {
    for ($i = 0; $i -lt 80; $i++) {
        try { $r = Invoke-RestMethod "$origin/api/me" -TimeoutSec 2; if ($null -ne $r) { break } } catch { }
        Start-Sleep -Milliseconds 250
    }
    "站点数=$Count  实例=$origin  PID=$($proc.Id)"
    for ($k = 1; $k -le 3; $k++) {
        $t = Measure-Command { $resp = Invoke-RestMethod "$origin/star/sites" -TimeoutSec 60 }
        $n = @($resp.data.inventory.sites).Count
        "  第 $k 次盘点：{0:N1} ms（{1} 个站点）" -f $t.TotalMilliseconds, $n
    }
}
finally {
    if ($proc -and -not $proc.HasExited) { Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue }
    Remove-Item Env:\PEK_RAGENT_BASE -ErrorAction SilentlyContinue
    if (-not $env:KEEP_E2E) { Remove-Item $root -Recurse -Force -ErrorAction SilentlyContinue }
}
