# RAgent nginx 证书部署冒烟（隔离实例 + 临时 vhost + 假 nginx；验证改写/备份/幂等/回滚）
# 用法：powershell -NoProfile -ExecutionPolicy Bypass -File scripts\_e2e-cert-nginx.ps1 [-Port 5504]

param(
    [int]$Port = 5504
)
$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
$exe = Join-Path $repo 'target\debug\pek-ragent.exe'
if (-not (Test-Path $exe)) { throw "未找到调试产物：$exe（先运行 cargo build）" }
$ssl = 'C:\Program Files\Git\usr\bin\openssl.exe'
if (-not (Test-Path $ssl)) { throw "未找到 openssl：$ssl" }

$root = Join-Path $env:TEMP ("ragent-nginx-e2e-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
$base = Join-Path $root 'agent'
$vhost = Join-Path $root 'vhost'
New-Item -ItemType Directory -Force (Join-Path $base 'Config'), $vhost | Out-Null
$conf = Join-Path $vhost 'app.conf'
$confText = @"
server {
    listen 80;
    server_name app.e2e.local;
    root C:/www/app;
    location / {
        proxy_pass http://127.0.0.1:8080;
    }
}
"@
[IO.File]::WriteAllText($conf, $confText, (New-Object Text.UTF8Encoding($false)))

# 假 nginx（仅用于本机验证：-t / -s reload 返回 0）
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
  <CertManagedSites>app.e2e.local</CertManagedSites>
  <CertNginxDirs>$vhost</CertNginxDirs>
  <CertNginxBin>$fakeNginx</CertNginxBin>
</StarAgent>
"@
[IO.File]::WriteAllText((Join-Path $base 'Config\StarAgent.config'), $xml, (New-Object Text.UTF8Encoding($false)))

# 两套证书（用于替换与回滚验证）
$cert1 = Join-Path $root 'c1.pem'; $key1 = Join-Path $root 'k1.pem'
$cert2 = Join-Path $root 'c2.pem'; $key2 = Join-Path $root 'k2.pem'
cmd /c ('"' + $ssl + '" req -x509 -newkey rsa:2048 -nodes -keyout "' + $key1 + '" -out "' + $cert1 + '" -days 30 -subj "/CN=app.e2e.local" -addext "subjectAltName=DNS:app.e2e.local" 2>nul') | Out-Null
cmd /c ('"' + $ssl + '" req -x509 -newkey rsa:2048 -nodes -keyout "' + $key2 + '" -out "' + $cert2 + '" -days 30 -subj "/CN=app.e2e.local" -addext "subjectAltName=DNS:app.e2e.local" 2>nul') | Out-Null

$env:PATH = (Split-Path $ssl) + ';' + $env:PATH
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
function Deploy([string]$certId, [string]$certFile, [string]$keyFile, [string]$siteKey, [bool]$dryRun) {
    $body = @{
        certId   = $certId
        certName = $certId
        certPem  = [IO.File]::ReadAllText($certFile)
        keyPem   = [IO.File]::ReadAllText($keyFile)
        sites    = @($siteKey)
        action   = 'auto'
        dryRun   = $dryRun
    } | ConvertTo-Json
    return Invoke-RestMethod -Method Post -Uri "$origin/star/certDeploy" -Body $body -ContentType 'application/json'
}

try {
    Wait-Up
    $sites = Invoke-RestMethod "$origin/star/sites"
    if ($sites.code -ne 0) { throw "盘点失败：$($sites.message)" }
    $list = @($sites.data.inventory.sites)
    if ($list.Count -ne 1) { throw "应发现 1 个站点，实际 $($list.Count)" }
    $site = $list[0]
    if ($site.domains -notcontains 'app.e2e.local') { throw "站点域名不正确：$($site.domains -join ',')" }
    if (-not $site.managed) { throw "站点未按白名单标记为可管理" }
    "1) 盘点：kind=$($site.kind) key=$($site.key) ssl=$($site.ssl) managed=$($site.managed)"

    # 演练：不写文件
    $before = (Get-FileHash $conf -Algorithm SHA256).Hash
    $dry = Deploy 'e2e-nginx-1' $cert1 $key1 $site.key $true
    if ($dry.code -ne 0) { throw "演练请求失败：$($dry.message)" }
    if (-not $dry.data.ok) { throw "演练应成功：$($dry.data.items[0].message)" }
    if ($dry.data.items[0].action -ne 'enable_https') { throw "应识别为 enable_https：$($dry.data.items[0].action)" }
    if ((Get-FileHash $conf -Algorithm SHA256).Hash -ne $before) { throw "演练不应修改站点配置" }
    "2) 演练：action=$($dry.data.items[0].action)，文件未变"

    # 证书与私钥不配对 → 演练即拒绝（nginx -t 查不出这种错，只能运行期暴露）
    $bad = Deploy 'e2e-nginx-bad' $cert1 $key2 $site.key $true
    if ($bad.code -ne 0) { throw "不匹配用例请求失败：$($bad.message)" }
    if ($bad.data.ok) { throw "证书与私钥不匹配时应判失败" }
    if ($bad.data.items[0].message -notlike '*不匹配*') { throw "错误信息应说明不匹配：$($bad.data.items[0].message)" }
    "2.5) 私钥不匹配被拒绝：$($bad.data.items[0].message)"

    # 真实执行：启用 HTTPS
    $run = Deploy 'e2e-nginx-1' $cert1 $key1 $site.key $false
    if ($run.code -ne 0) { throw "执行请求失败：$($run.message)" }
    if (-not $run.data.ok) { throw "执行应成功：$($run.data.items[0].message)" }
    $text = [IO.File]::ReadAllText($conf)
    if ($text -notmatch 'listen 443 ssl;') { throw "未插入 443 监听：`n$text" }
    if ($text -notmatch 'ssl_certificate ') { throw "未写入证书指令：`n$text" }
    if ($text -notmatch 'root C:/www/app;') { throw "原站配置丢失：`n$text" }
    $backupDir = Join-Path $base 'Data\CertBackups'
    if (-not (Test-Path $backupDir)) { throw "未生成配置备份" }
    $certFile = Join-Path $base ('Data\Certs\e2e-nginx-1\fullchain.pem')
    if (-not (Test-Path $certFile)) { throw "未写入证书文件" }
    "3) 执行成功：已插入 443 + 证书指令，原配置保留，备份与证书文件已生成"

    # 幂等：第二次替换不同证书，不应重复插入指令
    $run2 = Deploy 'e2e-nginx-2' $cert2 $key2 $site.key $false
    if ($run2.code -ne 0 -or -not $run2.data.ok) { throw "第二次执行失败：$($run2.message)" }
    $text2 = [IO.File]::ReadAllText($conf)
    $count = ([regex]::Matches($text2, 'ssl_certificate ')).Count
    if ($count -ne 1) { throw "证书指令重复（$count 次）：`n$text2" }
    if ($text2 -notmatch 'Data\\Certs\\e2e-nginx-2|Data/Certs/e2e-nginx-2') { throw "未指向新证书：`n$text2" }
    "4) 幂等替换成功：证书指令仅 1 条，已指向第二套证书"

    # 回滚：假 nginx 返回非 0 → 配置应恢复
    $snapshot = [IO.File]::ReadAllText($conf)
    [IO.File]::WriteAllText($fakeNginx, "@echo off`r`nexit /b 1`r`n", (New-Object Text.UTF8Encoding($false)))
    $bad = Deploy 'e2e-nginx-3' $cert1 $key1 $site.key $false
    if ($bad.code -ne 0) { throw "回滚用例请求失败：$($bad.message)" }
    if ($bad.data.ok) { throw "nginx -t 失败时应判定失败" }
    if (-not ($bad.data.items[0].message -like '*已回滚*')) { throw "失败信息应说明已回滚：$($bad.data.items[0].message)" }
    if ([IO.File]::ReadAllText($conf) -ne $snapshot) { throw "回滚后配置与执行前不一致" }
    "5) 回滚验证通过：nginx -t 失败 → 配置恢复原样"

    "=== RAGENT NGINX CERT E2E PASSED ==="
}
finally {
    try { if ($proc -and -not $proc.HasExited) { Stop-Process -Id $proc.Id -Force } } catch { }
    Remove-Item Env:PEK_RAGENT_BASE -ErrorAction SilentlyContinue
    Start-Sleep -Milliseconds 300
    Remove-Item -Recurse -Force $root -ErrorAction SilentlyContinue
}
