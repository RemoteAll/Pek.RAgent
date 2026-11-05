# 启动一个隔离 RAgent 实例用于浏览器手工验证「证书」页（假 nginx 站点 + 空白名单）
# 用法：powershell -NoProfile -ExecutionPolicy Bypass -File scripts\_e2e-cert-ui-start.ps1 [-Port 5506]
# 输出起始 base 目录；验证完请 Stop-Process 对应 PID 并删除该目录。

param(
    [int]$Port = 5506
)
$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
$exe = Join-Path $repo 'target\debug\pek-ragent.exe'
if (-not (Test-Path $exe)) { throw "未找到调试产物：$exe（先运行 cargo build）" }

$root = Join-Path $env:TEMP 'ragent-cert-ui'
if (Test-Path $root) { Remove-Item $root -Recurse -Force }
$base = Join-Path $root 'agent'
$vhost = Join-Path $root 'vhost'
New-Item -ItemType Directory -Force (Join-Path $base 'Config'), $vhost | Out-Null

$sites = @{
    'zx.hlktech.com.conf' = "server {`n    listen 80;`n    server_name zx.hlktech.com;`n    root C:/www/zx;`n}`n"
    'bms.hlktech.com.conf' = "server {`n    listen 443 ssl;`n    server_name bms.hlktech.com;`n    ssl_certificate /www/server/panel/vhost/cert/bms.hlktech.com/fullchain.pem;`n    ssl_certificate_key /www/server/panel/vhost/cert/bms.hlktech.com/privkey.pem;`n    root C:/www/bms;`n}`n"
}
foreach ($k in $sites.Keys) {
    [IO.File]::WriteAllText((Join-Path $vhost $k), $sites[$k], (New-Object Text.UTF8Encoding($false)))
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
Start-Sleep -Milliseconds 1500
"base=$base"
"pid=$($proc.Id)"
"url=http://127.0.0.1:$Port/#cert"
