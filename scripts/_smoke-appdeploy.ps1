# Smoke test: one-click app package deploy (/star/appDeploy)
# Boots an isolated agent instance (port 5597), uploads a real DHDeploy Agent zip package,
# then verifies registration / file layout / running state / upgrade path, and cleans up.
$ErrorActionPreference = 'Continue'
$root = 'g:\Code\Rust\Pek.RAgent'
$exe = Join-Path $root 'target\debug\pek-ragent.exe'
$base = Join-Path $env:TEMP 'ragent-appdeploy-smoke'
$port = 5597
$srv = "http://127.0.0.1:$port"
$pkg = 'g:\Project\DHDeploy\dist\dhdeploy-agent-windows-x64-v1.0.41.zip'
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
    & curl.exe -s -m 20 -o $tmp $url
    $j = ReadJson $tmp
    Remove-Item $tmp -Force
    return $j
}
function Post-Json($url, $json) {
    $tmp = [IO.Path]::GetTempFileName()
    [IO.File]::WriteAllText($tmp, $json)
    $out = [IO.Path]::GetTempFileName()
    & curl.exe -s -m 30 -X POST -H 'Content-Type: application/json' --data-binary "@$tmp" -o $out $url
    $j = ReadJson $out
    Remove-Item $tmp -Force
    Remove-Item $out -Force
    return $j
}
function Upload-Pkg($zip, $query) {
    $out = [IO.Path]::GetTempFileName()
    & curl.exe -s -m 120 -X POST -H 'Content-Type: application/octet-stream' --data-binary "@$zip" -o $out "$srv/star/appDeploy?$query"
    $j = ReadJson $out
    Remove-Item $out -Force
    return $j
}

if (-not (Test-Path $pkg)) { Write-Host "SKIP: package not found $pkg"; exit 2 }

# --- reset workspace ---
if (Test-Path $base) { Remove-Item $base -Recurse -Force -ErrorAction SilentlyContinue }
New-Item -ItemType Directory -Force (Join-Path $base 'Config') | Out-Null
$cfg = @"
<?xml version="1.0" encoding="utf-8"?>
<StarAgent>
  <LocalPort>$port</LocalPort>
  <LocalOnly>true</LocalOnly>
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
if (-not $ready) { Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue; exit 1 }

# --- deploy (auto start) ---
$pkgName = Split-Path $pkg -Leaf
$r = Upload-Pkg $pkg "fileName=$([uri]::EscapeDataString($pkgName))&autoStart=1"
Check 'appDeploy ok' ($r.code -eq 0)
Check 'service name auto-detected (dhdeploy-agent-rust)' ($r.data.name -eq 'dhdeploy-agent-rust')
Check 'program exists in app dir' (Test-Path $r.data.program)
Check 'working dir under apps/' ($r.data.workingDirectory -like '*apps\dhdeploy-agent-rust')
Check 'files copied (>=2)' ($r.data.files -ge 2)
Check 'reported started' ($r.data.started -eq $true)

$svc = (Get-Json "$srv/star/services").data.services | Where-Object { $_.Name -eq 'dhdeploy-agent-rust' }
Check 'service registered' ($null -ne $svc)
Check 'service enabled' ($svc.Enable -eq $true)
Check 'service running' ($svc.Running -eq $true)
Check 'config persisted to XML' ((Get-Content (Join-Path $base 'Config\StarAgent.config') -Raw -Encoding UTF8) -match 'dhdeploy-agent-rust')

# --- upgrade (same package again; should update + restart) ---
$r = Upload-Pkg $pkg "fileName=$([uri]::EscapeDataString($pkgName))&autoStart=1"
Check 'appDeploy (again) ok' ($r.code -eq 0)
Check 'upgrade flagged updated' ($r.data.updated -eq $true)
Check 'upgrade restarted' ($r.data.started -eq $true)
$entries = @((Get-Json "$srv/star/services").data.services | Where-Object { $_.Name -eq 'dhdeploy-agent-rust' })
Check 'single service entry after upgrade' ($entries.Count -eq 1)

# --- tar.gz package deploy (Windows exe packed as tar.gz; top-level dir layout) ---
$tgzDir = Join-Path $env:TEMP 'ragent-tgz-src'
if (Test-Path $tgzDir) { Remove-Item $tgzDir -Recurse -Force -ErrorAction SilentlyContinue }
New-Item -ItemType Directory -Force (Join-Path $tgzDir 'demo-tgz') | Out-Null
Copy-Item $exe (Join-Path $tgzDir 'demo-tgz\demo-tgz.exe')
$tgz = Join-Path $env:TEMP 'demo-tgz.tar.gz'
& tar.exe -czf $tgz -C $tgzDir demo-tgz
$r = Upload-Pkg $tgz "fileName=demo-tgz.tar.gz&autoStart=0"
Check 'tar.gz deploy ok' ($r.code -eq 0)
Check 'tar.gz name auto-detected' ($r.data.name -eq 'demo-tgz')
Check 'tar.gz program exists' ($r.data.program -and (Test-Path $r.data.program))
$null = Post-Json "$srv/star/removeService" '{"serviceName":"demo-tgz"}'
Remove-Item $tgzDir -Recurse -Force -ErrorAction SilentlyContinue
Remove-Item $tgz -Force -ErrorAction SilentlyContinue

# --- reject unsupported package ---
$bad = Join-Path $env:TEMP 'ragent-bad-package.bin'
[IO.File]::WriteAllBytes($bad, [byte[]](1..64))
$out = [IO.Path]::GetTempFileName()
& curl.exe -s -m 30 -X POST -H 'Content-Type: application/octet-stream' --data-binary "@$bad" -o $out "$srv/star/appDeploy?fileName=bad.bin&autoStart=0"
$j = ReadJson $out
Remove-Item $out -Force
Remove-Item $bad -Force
Check 'unsupported package rejected (code!=0)' ($j.code -ne 0)

# --- cleanup: stop + remove service, kill instance ---
$null = Post-Json "$srv/star/stopService" '{"serviceName":"dhdeploy-agent-rust"}'
$null = Post-Json "$srv/star/removeService" '{"serviceName":"dhdeploy-agent-rust"}'
Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 800
Get-Process pek-ragent -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $exe } | Stop-Process -Force -ErrorAction SilentlyContinue
Get-Process dhdeploy-agent-rust -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
$env:PEK_RAGENT_BASE = $oldBase
Start-Sleep -Milliseconds 500
Remove-Item $base -Recurse -Force -ErrorAction SilentlyContinue

Write-Host "== SMOKE RESULT: $ok passed, $fail failed =="
