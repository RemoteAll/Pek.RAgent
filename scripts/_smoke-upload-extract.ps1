# Smoke test: chunked upload (2GB path) + multi-format extract (2026-10-31)
#   1) fileCheck  -> overwrite confirmation data
#   2) single-shot upload of a file > 64MB is still rejected by the HTTP layer (413)
#   3) chunked upload of the same file succeeds (130MB, 8MB chunks)
#   4) overwrite=0 is rejected, overwrite=1 replaces
#   5) tar.gz extract (default target = sibling dir with the archive stem)
#   6) rar is rejected with a clear message
$ErrorActionPreference = 'Continue'
$port = 5598
$srv = "http://127.0.0.1:$port"
$base = Join-Path $env:TEMP 'ragent-smoke-upload'
$fsDir = Join-Path $base 'fs'
$exe = 'G:\Code\Rust\Pek.RAgent\target\debug\pek-ragent.exe'
$ok = 0
$fail = 0

function Check($name, $cond) {
    if ($cond) { Write-Host "PASS $name"; $script:ok++ } else { Write-Host "FAIL $name"; $script:fail++ }
}
function ReadJson($path) {
    $text = [IO.File]::ReadAllText($path, [Text.Encoding]::UTF8)
    return ($text | ConvertFrom-Json)
}
function Get-Json($url) {
    $tmp = [IO.Path]::GetTempFileName()
    & curl.exe -s -m 30 -o $tmp $url
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
function Enc($s) { return [uri]::EscapeDataString($s) }
# Binary POST -> JSON (never pipe curl output: the GBK console mangles UTF-8 JSON)
function Post-Bin($url, $file) {
    $out = [IO.Path]::GetTempFileName()
    & curl.exe -s -m 120 -X POST -H 'Content-Type: application/octet-stream' --data-binary "@$file" -o $out $url
    $j = ReadJson $out
    Remove-Item $out -Force
    return $j
}

if (-not (Test-Path $exe)) { Write-Host "SKIP: binary not found $exe (run cargo build)"; exit 2 }

# --- reset workspace + config ---
if (Test-Path $base) { Remove-Item $base -Recurse -Force -ErrorAction SilentlyContinue }
New-Item -ItemType Directory -Force $fsDir | Out-Null
New-Item -ItemType Directory -Force (Join-Path $base 'Config') | Out-Null
$cfg = @"
<?xml version="1.0" encoding="utf-8"?>
<StarAgent>
  <LocalPort>$port</LocalPort>
  <LocalOnly>true</LocalOnly>
  <Tls>false</Tls>
</StarAgent>
"@
[IO.File]::WriteAllText((Join-Path $base 'Config\StarAgent.config'), $cfg, (New-Object Text.UTF8Encoding($false)))

# --- boot isolated instance ---
$oldBase = $env:PEK_RAGENT_BASE
$env:PEK_RAGENT_BASE = $base
$proc = Start-Process -FilePath $exe -ArgumentList '-run' -PassThru -WindowStyle Hidden
$ready = $false
for ($i = 0; $i -lt 80; $i++) {
    Start-Sleep -Milliseconds 250
    try { $r = Get-Json "$srv/star/services"; if ($r.code -eq 0) { $ready = $true; break } } catch {}
}
Check 'instance ready' $ready
if (-not $ready) {
    Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue
    exit 1
}

try {
    # --- prepare a 130MB file (> 64MB default body limit) ---
    $big = Join-Path $env:TEMP 'ragent-big.bin'
    $f = [IO.File]::Create($big)
    $f.SetLength(130MB)
    $f.Close()
    $total = (Get-Item $big).Length
    Check 'fixture is 130MB' ($total -eq 130MB)

    # --- 1) fileCheck: no conflict yet ---
    $r = Post-Json "$srv/star/fileCheck" (@{ dir = $fsDir; names = @('big.bin') } | ConvertTo-Json -Compress)
    Check 'fileCheck (empty dir) -> no existing' ($r.code -eq 0 -and $r.data.existing.Count -eq 0)

    # --- 2) single-shot upload of 130MB is rejected by HTTP body limit (proves chunking is required) ---
    $out = [IO.Path]::GetTempFileName()
    $status = (& curl.exe -s -m 120 -w '%{http_code}' -o $out -X POST "$srv/star/fileUpload?path=$(Enc $fsDir)&name=big.bin" --data-binary "@$big")
    Check 'single-shot 130MB rejected (413)' ($status -eq '413')
    Remove-Item $out -Force

    # --- 3) chunked upload: 8MB chunks ---
    $chunkSize = 8MB
    $id = 'smoke' + [guid]::NewGuid().ToString('N').Substring(0, 10)
    $offset = 0
    $done = $false
    $last = $null
    $sw = [Diagnostics.Stopwatch]::StartNew()
    while ($offset -lt $total) {
        $len = [Math]::Min($chunkSize, $total - $offset)
        $buf = New-Object byte[] $len
        $in = [IO.File]::OpenRead($big)
        $in.Position = $offset
        $read = 0
        while ($read -lt $len) {
            $n = $in.Read($buf, $read, $len - $read)
            if ($n -le 0) { break }
            $read += $n
        }
        $in.Close()
        $part = Join-Path $env:TEMP "ragent-part-$offset.bin"
        [IO.File]::WriteAllBytes($part, $buf)
        $url = "$srv/star/fileUploadChunk?path=$(Enc $fsDir)&name=big.bin&id=$id&offset=$offset&total=$total&overwrite=1"
        $r = Post-Bin $url $part
        Remove-Item $part -Force
        if ($r.code -ne 0) { Write-Host "  chunk@$offset failed: $($r.message)"; break }
        $last = $r
        $offset += $len
    }
    $sw.Stop()
    Check 'chunked upload completed' ($last -and $last.code -eq 0 -and $last.data.done -eq $true)
    Check 'chunked upload received == total' ($last.data.received -eq $total)
    Check 'chunked 130MB under 60s' ($sw.Elapsed.TotalSeconds -lt 60)
    $h1 = (Get-FileHash $big).Hash
    $h2 = (Get-FileHash (Join-Path $fsDir 'big.bin')).Hash
    Check 'chunked upload hash matches' ($h1 -eq $h2)
    Write-Host "  chunked upload took $([math]::Round($sw.Elapsed.TotalSeconds,2))s"

    # --- 4) overwrite rules ---
    $r = Post-Json "$srv/star/fileCheck" (@{ dir = $fsDir; names = @('big.bin', 'nope.bin') } | ConvertTo-Json -Compress)
    Check 'fileCheck detects existing' ($r.code -eq 0 -and $r.data.existing.Count -eq 1 -and $r.data.existing[0] -eq 'big.bin')

    $part = Join-Path $env:TEMP 'ragent-small.bin'
    [IO.File]::WriteAllBytes($part, [byte[]](1..100))
    $id2 = 'smoke' + [guid]::NewGuid().ToString('N').Substring(0, 10)
    $r = Post-Bin "$srv/star/fileUploadChunk?path=$(Enc $fsDir)&name=big.bin&id=$id2&offset=0&total=100&overwrite=0" $part
    Check 'chunk upload overwrite=0 rejected' ($r.code -ne 0)
    Check 'rejected upload left original untouched' ((Get-Item (Join-Path $fsDir 'big.bin')).Length -eq $total)
    $r = Post-Bin "$srv/star/fileUploadChunk?path=$(Enc $fsDir)&name=big.bin&id=$id2&offset=0&total=100&overwrite=1" $part
    Check 'chunk upload overwrite=1 accepted' ($r.code -eq 0 -and $r.data.done -eq $true)
    Check 'overwritten file is 100 bytes' ((Get-Item (Join-Path $fsDir 'big.bin')).Length -eq 100)

    # bad offset is rejected
    $id3 = 'smoke' + [guid]::NewGuid().ToString('N').Substring(0, 10)
    $r = Post-Bin "$srv/star/fileUploadChunk?path=$(Enc $fsDir)&name=off.bin&id=$id3&offset=50&total=100&overwrite=1" $part
    Check 'bad offset rejected' ($r.code -ne 0)
    $null = Post-Json "$srv/star/fileUploadAbort?path=$(Enc $fsDir)&id=$id3" '{}'
    Remove-Item $part -Force
    Remove-Item $big -Force -ErrorAction SilentlyContinue

    # --- 5) tar.gz extract, default target = sibling dir with the archive stem ---
    $tgzSrc = Join-Path $env:TEMP 'ragent-tgz-src'
    if (Test-Path $tgzSrc) { Remove-Item $tgzSrc -Recurse -Force -ErrorAction SilentlyContinue }
    New-Item -ItemType Directory -Force (Join-Path $tgzSrc 'pkg/sub') | Out-Null
    [IO.File]::WriteAllText((Join-Path $tgzSrc 'pkg/a.txt'), 'hello-targz')
    [IO.File]::WriteAllText((Join-Path $tgzSrc 'pkg/sub/b.txt'), 'nested')
    $tgz = Join-Path $fsDir 'demo.tar.gz'
    & tar.exe -czf $tgz -C $tgzSrc pkg
    $r = Post-Json "$srv/star/fileExtract" (@{ path = $tgz } | ConvertTo-Json -Compress)
    Check 'tar.gz extract ok' ($r.code -eq 0 -and $r.data.fileCount -eq 2)
    Check 'tar.gz format detected' ($r.data.format -eq 'tar.gz')
    Check 'default target = sibling stem dir' ($r.data.target -eq (Join-Path $fsDir 'demo'))
    Check 'tar.gz content extracted' ((Get-Content (Join-Path $fsDir 'demo/pkg/a.txt') -Raw).Trim() -eq 'hello-targz')
    Remove-Item $tgzSrc -Recurse -Force -ErrorAction SilentlyContinue

    # --- explicit target dir ---
    $ex = Join-Path $fsDir 'explicit-out'
    $r = Post-Json "$srv/star/fileExtract" (@{ path = (Join-Path $fsDir 'demo.tar.gz'); target = $ex } | ConvertTo-Json -Compress)
    Check 'tar.gz extract to explicit target' ($r.code -eq 0 -and (Test-Path (Join-Path $ex 'pkg/a.txt')))

    # --- 6) rar is rejected ---
    $rar = Join-Path $fsDir 'x.rar'
    [IO.File]::WriteAllBytes($rar, [byte[]](0x52, 0x61, 0x72, 0x21, 0x1A, 0x07, 0x00))
    $r = Post-Json "$srv/star/fileExtract" (@{ path = $rar } | ConvertTo-Json -Compress)
    Check 'rar rejected with clear message' ($r.code -ne 0 -and $r.message -like '*RAR*')

    # --- zip still works (regression) ---
    $r = Post-Json "$srv/star/fileCompress" (@{ paths = @($ex); target = $fsDir; name = 're.zip' } | ConvertTo-Json -Compress)
    Check 'fileCompress still ok' ($r.code -eq 0)
    $r = Post-Json "$srv/star/fileExtract" (@{ path = (Join-Path $fsDir 're.zip') } | ConvertTo-Json -Compress)
    Check 'zip extract still ok' ($r.code -eq 0 -and $r.data.fileCount -ge 1)
}
finally {
    Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue
    Start-Sleep -Milliseconds 600
    Get-Process pek-ragent -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $exe } | Stop-Process -Force -ErrorAction SilentlyContinue
    $env:PEK_RAGENT_BASE = $oldBase
    Start-Sleep -Milliseconds 400
    Remove-Item $base -Recurse -Force -ErrorAction SilentlyContinue
}

Write-Host "== SMOKE RESULT: $ok passed, $fail failed =="
if ($fail -gt 0) { exit 1 }
