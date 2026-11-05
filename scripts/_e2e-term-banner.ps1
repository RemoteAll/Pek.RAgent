# 在线终端「服务器概况」banner 冒烟（隔离实例 + WebSocket 取首帧）
# 用法：powershell -NoProfile -ExecutionPolicy Bypass -File scripts\_e2e-term-banner.ps1 [-Port 5508]
param([int]$Port = 5508)
$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
$exe = Join-Path $repo 'target\debug\pek-ragent.exe'
if (-not (Test-Path $exe)) { throw "未找到调试产物：$exe（先运行 cargo build）" }

$root = Join-Path $env:TEMP 'ragent-term-banner'
if (Test-Path $root) { Remove-Item $root -Recurse -Force }
$base = Join-Path $root 'agent'
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
  <TerminalEnabled>true</TerminalEnabled>
</StarAgent>
"@
[IO.File]::WriteAllText((Join-Path $base 'Config\StarAgent.config'), $xml, (New-Object Text.UTF8Encoding($false)))

$env:PEK_RAGENT_BASE = $base
$proc = Start-Process -FilePath $exe -ArgumentList '-run' -PassThru -WindowStyle Hidden
$origin = "http://127.0.0.1:$Port"
function Read-FirstText([System.Net.WebSockets.ClientWebSocket]$ws) {
    $all = New-Object System.Text.StringBuilder
    for ($i = 0; $i -lt 5; $i++) {
        $buf = New-Object byte[] 262144
        $seg = New-Object 'System.ArraySegment[byte]' -ArgumentList @(,$buf)
        $t = $ws.ReceiveAsync($seg, [Threading.CancellationToken]::None)
        if (-not $t.Wait(8000)) { break }
        [void]$all.Append([Text.Encoding]::UTF8.GetString($buf, 0, $t.Result.Count))
        if ($all.ToString() -match 'Pek.RAgent 在线终端') { break }
    }
    return $all.ToString()
}
try {
    for ($i = 0; $i -lt 80; $i++) {
        try { $r = Invoke-RestMethod "$origin/api/me" -TimeoutSec 2; if ($null -ne $r) { break } } catch { }
        Start-Sleep -Milliseconds 250
    }
    $j = Invoke-RestMethod "$origin/star/termBanner" -TimeoutSec 20
    if ($j.code -ne 0) { throw "termBanner 返回失败：$($j.message)" }
    $text1 = [string]$j.data.text
    "1) termBanner 已返回（$($text1.Length) 字符）"
    $clean = $text1 -replace "`e\[[0-9;]*m", ''
    "2) 首帧（去色后）预览："
    ($clean -split "`r`n" | Where-Object { $_ -ne '' } | Select-Object -First 9) | ForEach-Object { "   $_" }
    foreach ($k in @('Pek.RAgent 在线终端', '主机', '系统', 'CPU', '内存', '磁盘', 'Web', '代理')) {
        if ($clean -notmatch [regex]::Escape($k)) { throw "banner 缺少字段：$k" }
    }
    "3) 字段齐全（主机/系统/CPU/内存/磁盘/Web/代理）"
    # WS 仍能正常连上（banner 不再占用 WS 首帧，避免与 PTY 输出交错）
    $ws = New-Object System.Net.WebSockets.ClientWebSocket
    $ws.ConnectAsync([Uri]"ws://127.0.0.1:$Port/star/termWs?sid=banner-e2e&cols=100&rows=30", [Threading.CancellationToken]::None).Wait(8000) | Out-Null
    $first = Read-FirstText $ws
    if ($first -match 'Pek.RAgent 在线终端') { throw 'WS 首帧不应再携带 banner（应由前端在清屏后写入）' }
    "4) WS 正常连接且首帧不再带 banner"
    try { $ws.CloseAsync([System.Net.WebSockets.WebSocketCloseStatus]::NormalClosure, 'done', [Threading.CancellationToken]::None).Wait(3000) | Out-Null } catch { }
    "=== TERM BANNER E2E PASSED ==="
}
finally {
    if ($proc -and -not $proc.HasExited) { Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue }
    Remove-Item Env:\PEK_RAGENT_BASE -ErrorAction SilentlyContinue
    if (-not $env:KEEP_E2E) { Remove-Item $root -Recurse -Force -ErrorAction SilentlyContinue }
}
