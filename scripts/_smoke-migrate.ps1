# Smoke test: migrate child services from C# StarAgent (/star/starAgentScan + /star/starAgentMigrate)
# Boots an isolated agent instance (port 5598) with a fake C# source directory and verifies:
#   scan preview -> import (absolute paths, mode normalization) -> idempotent re-import -> error paths.
# No real C# service is touched: source config uses ServiceName=StarAgentSmokeTest (not installed).
$ErrorActionPreference = 'Continue'
$root = 'g:\Code\Rust\Pek.RAgent'
$exe = Join-Path $root 'target\debug\pek-ragent.exe'
$base = Join-Path $env:TEMP 'ragent-migrate-smoke'
$src = Join-Path $env:TEMP 'ragent-csharp-src'
$port = 5598
$srv = "http://127.0.0.1:$port"
$ok = 0; $fail = 0

function Check($name, $cond) {
    if ($cond) { Write-Host "PASS $name"; $script:ok++ } else { Write-Host "FAIL $name"; $script:fail++ }
}
function ReadJson($file) {
    $text = [IO.File]::ReadAllText($file, [Text.Encoding]::UTF8)
    return ($text | ConvertFrom-Json)
}
function Get-Json($url) {
    $tmp = [IO.Path]::GetTempFileName()
    & curl.exe -s -m 30 -o $tmp $url
    $j = ReadJson $tmp
    Remove-Item $tmp -Force
    return $j
}
function Post-Json($url) {
    $tmp = [IO.Path]::GetTempFileName()
    & curl.exe -s -m 60 -X POST -H 'Content-Type: application/json' -o $tmp $url
    $j = ReadJson $tmp
    Remove-Item $tmp -Force
    return $j
}

# --- reset workspaces ---
foreach ($d in @($base, $src)) {
    if (Test-Path $d) { Remove-Item $d -Recurse -Force -ErrorAction SilentlyContinue }
    New-Item -ItemType Directory -Force (Join-Path $d 'Config') | Out-Null
}

# --- fake C# StarAgent config (same XML format as C#) ---
$csCfg = @"
<?xml version="1.0" encoding="utf-8"?>
<StarAgent>
  <LocalPort>5599</LocalPort>
  <ServiceName>StarAgentSmokeTest</ServiceName>
  <Services>
    <ServiceInfo Priority="High" Name="demo-agent" FileName="demo-agent.exe" Arguments="--port 8080" WorkingDirectory="..\agents\demo" Enable="true" Mode="11" ReloadOnChange="true" />
    <ServiceInfo Priority="Normal" Name="dotnet-app" FileName="dotnet" Arguments="App.dll" WorkingDirectory="..\server" Enable="false" Mode="10" HealthCheck="http://127.0.0.1:6600/ping" />
  </Services>
</StarAgent>
"@
[IO.File]::WriteAllText((Join-Path $src 'Config\StarAgent.config'), $csCfg, (New-Object Text.UTF8Encoding($false)))

# --- agent config ---
$cfg = @"
<?xml version="1.0" encoding="utf-8"?>
<StarAgent>
  <LocalPort>$port</LocalPort>
  <LocalOnly>true</LocalOnly>
  <Tls>false</Tls>
  <AutoUpgradeUrl></AutoUpgradeUrl>
  <PairingEnabled>false</PairingEnabled>
</StarAgent>
"@
[IO.File]::WriteAllText((Join-Path $base 'Config\StarAgent.config'), $cfg, (New-Object Text.UTF8Encoding($false)))

# --- boot isolated instance ---
$oldBase = $env:PEK_RAGENT_BASE
$env:PEK_RAGENT_BASE = $base
$proc = Start-Process -FilePath $exe -ArgumentList '-run' -PassThru -WindowStyle Hidden
$ready = $false
for ($i = 0; $i -lt 60; $i++) {
    Start-Sleep -Milliseconds 250
    try { $r = Get-Json "$srv/star/services"; if ($r.code -eq 0) { $ready = $true; break } } catch {}
}
Check 'instance ready' $ready
if (-not $ready) {
    Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue
    $env:PEK_RAGENT_BASE = $oldBase
    exit 1
}

# --- 1) scan ---
$enc = [uri]::EscapeDataString($src)
$scan = Get-Json "$srv/star/starAgentScan?dir=$enc"
Check 'scan code=0' ($scan.code -eq 0)
Check 'scan found' ($scan.data.found -eq $true)
Check 'scan 2 apps' (@($scan.data.apps).Count -eq 2)
Check 'scan localPort=5599' ($scan.data.localPort -eq 5599)
Check 'scan serviceName' ($scan.data.serviceName -eq 'StarAgentSmokeTest')
Check 'scan service not installed' ($scan.data.serviceInstalled -eq $false)
$dotnet = @($scan.data.apps) | Where-Object { $_.name -eq 'dotnet-app' }
Check 'scan mode normalized (standard)' ($dotnet.mode -eq 'standard')

# --- 2) scan bad dir -> found=false + candidates ---
$bad = Get-Json ("$srv/star/starAgentScan?dir=" + [uri]::EscapeDataString((Join-Path $env:TEMP 'no-such-staragent-dir')))
Check 'bad dir code=0' ($bad.code -eq 0)
Check 'bad dir found=false' ($bad.data.found -eq $false)
Check 'bad dir candidates listed' (@($bad.data.candidates).Count -gt 0)

# --- 3) migrate (no source stop, no start) ---
$url = "$srv/star/starAgentMigrate?dir=$enc&names=demo-agent,dotnet-app&stopApps=0&stopService=0&disableService=0&startApps=0"
$mig = Post-Json $url
Check 'migrate code=0' ($mig.code -eq 0)
Check 'migrate imported 2' (@($mig.data.apps).Count -eq 2)
Check 'migrate no start' (@($mig.data.apps | Where-Object { $_.started }).Count -eq 0)

$svcs = Get-Json "$srv/star/services"
$demo = @($svcs.data.services) | Where-Object { $_.Name -eq 'demo-agent' }
$dot  = @($svcs.data.services) | Where-Object { $_.Name -eq 'dotnet-app' }
Check 'demo imported' ($null -ne $demo)
Check 'dotnet imported' ($null -ne $dot)
Check 'demo working dir absolute' ([IO.Path]::IsPathRooted($demo.WorkingDirectory))
Check 'dotnet mode standard' ($dot.Mode -eq 'standard')
Check 'dotnet health kept' ($dot.HealthCheck -eq 'http://127.0.0.1:6600/ping')

# --- 4) re-import is idempotent (update, still 2 entries) ---
$mig2 = Post-Json $url
Check 're-migrate code=0' ($mig2.code -eq 0)
$svcs2 = Get-Json "$srv/star/services"
Check 'no duplicate entries' (@($svcs2.data.services | Where-Object { $_.Name -eq 'demo-agent' }).Count -eq 1)

# --- 5) error paths ---
$noDir = Post-Json "$srv/star/starAgentMigrate?dir=&names=a"
Check 'empty dir rejected' ($noDir.code -eq 400)
$noPick = Post-Json "$srv/star/starAgentMigrate?dir=$enc&names=&stopApps=0&stopService=0&startApps=0"
Check 'all services when names empty' ($noPick.code -eq 0)

# --- 6) stopService=1 with fake service name must not hang / must not fail ---
$sw = [Diagnostics.Stopwatch]::StartNew()
$stopTry = Post-Json "$srv/star/starAgentMigrate?dir=$enc&names=demo-agent&stopApps=0&stopService=1&disableService=1&startApps=0"
$sw.Stop()
Check 'stop-source codes ok' ($stopTry.code -eq 0)
Check 'stop-source fast (not installed)' ($sw.Elapsed.TotalSeconds -lt 15)

# --- cleanup ---
Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 500
Get-Process pek-ragent -ErrorAction SilentlyContinue | Where-Object { $_.Path -like '*target\debug*' } | Stop-Process -Force -ErrorAction SilentlyContinue
$env:PEK_RAGENT_BASE = $oldBase
Remove-Item $base -Recurse -Force -ErrorAction SilentlyContinue
Remove-Item $src -Recurse -Force -ErrorAction SilentlyContinue

Write-Host ""
Write-Host "RESULT: $ok passed, $fail failed"
if ($fail -gt 0) { exit 1 }
exit 0
