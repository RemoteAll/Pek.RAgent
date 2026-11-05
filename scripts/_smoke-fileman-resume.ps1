# Smoke test: 文件夹上传配套（批量建目录 / 多层同名预检）+ 断点上传 + 断点下载（Range）
#   1) fileMkdirs 建整棵目录树（含空目录），拒绝 `..` 与盘符
#   2) 分片上传中断 → fileUploadStatus 显示已收 → 从断点续传 → SHA-256 一致
#   3) fileCheck(paths) 返回多层目录里的同名文件
#   4) fileDownload 支持 Range：206 + Content-Range + ETag、If-Range 失配 412、后缀区间、越界 416
#   5) 整文件仍受 256MB 限制；超大文件可用 Range 分片下载
$ErrorActionPreference = 'Continue'
$port = 5599
$srv = "http://127.0.0.1:$port"
$base = Join-Path $env:TEMP 'ragent-smoke-resume'
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
function Post-Bin($url, $file) {
    $out = [IO.Path]::GetTempFileName()
    & curl.exe -s -m 120 -X POST -H 'Content-Type: application/octet-stream' --data-binary "@$file" -o $out $url
    $j = ReadJson $out
    Remove-Item $out -Force
    return $j
}
# 返回 @{ code = HTTP 状态码; headers = 响应头文本; body = 落盘文件（调用方删） }
function Get-Range($url, $rangeHeader, $ifRange) {
    $hdr = [IO.Path]::GetTempFileName()
    $body = [IO.Path]::GetTempFileName()
    $curlArgs = @('-s', '-m', '120', '-D', $hdr, '-o', $body, '-w', '%{http_code}')
    if ($rangeHeader) { $curlArgs += @('-H', "Range: $rangeHeader") }
    if ($ifRange) { $curlArgs += @('-H', "If-Range: $ifRange") }
    $code = (& curl.exe @curlArgs $url)
    return @{ code = $code; headers = [IO.File]::ReadAllText($hdr); body = $body; hdr = $hdr }
}
function Hdr($text, $name) {
    foreach ($line in ($text -split "`r?`n")) {
        if ($line -match "^(?i)$([regex]::Escape($name)):\s*(.+)$") { return $Matches[1].Trim() }
    }
    return $null
}
# If-Range 的值带双引号，PowerShell 5.1 传原生参数会把引号吃掉 → 改用 HttpClient 精确发送
function Get-StatusWithHeaders($url, $headers) {
    Add-Type -AssemblyName System.Net.Http | Out-Null
    $c = New-Object System.Net.Http.HttpClient
    $req = New-Object System.Net.Http.HttpRequestMessage('GET', $url)
    foreach ($k in $headers.Keys) { [void]$req.Headers.TryAddWithoutValidation($k, $headers[$k]) }
    $resp = $c.SendAsync($req).Result
    $code = [int]$resp.StatusCode
    $bytes = $resp.Content.ReadAsByteArrayAsync().Result
    $c.Dispose()
    return @{ code = $code; len = $bytes.Length }
}
function New-Fixture($path, $bytes) {
    $fs = [IO.File]::Create($path)
    $block = New-Object byte[] (1MB)
    for ($i = 0; $i -lt 1MB; $i++) { $block[$i] = [byte](($i * 31 + 7) % 251) }
    $left = $bytes
    while ($left -gt 0) {
        $n = [Math]::Min($block.Length, $left)
        $fs.Write($block, 0, $n)
        $left -= $n
    }
    $fs.Close()
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
    # --- 1) 批量建目录（保留空目录；拒绝穿越/盘符） ---
    $r = Post-Json "$srv/star/fileMkdirs" (@{ path = $fsDir; dirs = @('a', 'a/b', 'a/b/c', 'empty') } | ConvertTo-Json -Compress)
    Check 'fileMkdirs ok' ($r.code -eq 0 -and $r.data.created -eq 4)
    Check 'fileMkdirs nested created' (Test-Path (Join-Path $fsDir 'a/b/c'))
    Check 'fileMkdirs empty dir created' (Test-Path (Join-Path $fsDir 'empty'))
    $r = Post-Json "$srv/star/fileMkdirs" (@{ path = $fsDir; dirs = @('a/../../evil') } | ConvertTo-Json -Compress)
    Check 'fileMkdirs rejects ..' ($r.code -ne 0)
    if ([Environment]::OSVersion.Platform -eq 'Win32NT') {
        $r = Post-Json "$srv/star/fileMkdirs" (@{ path = $fsDir; dirs = @('C:evil') } | ConvertTo-Json -Compress)
        Check 'fileMkdirs rejects drive prefix' ($r.code -ne 0)
    }
    # 重复建同一个目录视为成功（拖拽重试友好）
    $r = Post-Json "$srv/star/fileMkdirs" (@{ path = $fsDir; dirs = @('a/b') } | ConvertTo-Json -Compress)
    Check 'fileMkdirs idempotent' ($r.code -eq 0)

    # --- 2) 断点上传：先传 1 片，模拟中断，再从断点续传 ---
    $big = Join-Path $env:TEMP 'ragent-resume.bin'
    New-Fixture $big (20MB)
    $total = (Get-Item $big).Length
    $chunkSize = 8MB
    $id = 'smoke' + [guid]::NewGuid().ToString('N').Substring(0, 10)
    function Send-Chunk($offset) {
        $len = [Math]::Min($chunkSize, $total - $offset)
        $buf = New-Object byte[] $len
        $in = [IO.File]::OpenRead($big)
        $in.Position = $offset
        $read = 0
        while ($read -lt $len) { $n = $in.Read($buf, $read, $len - $read); if ($n -le 0) { break }; $read += $n }
        $in.Close()
        $part = Join-Path $env:TEMP "ragent-resume-part-$offset.bin"
        [IO.File]::WriteAllBytes($part, $buf)
        $u = "$srv/star/fileUploadChunk?path=$(Enc $fsDir)&name=resume.bin&id=$id&offset=$offset&total=$total&overwrite=1"
        $res = Post-Bin $u $part
        Remove-Item $part -Force
        return $res
    }
    $r = Send-Chunk 0
    Check 'first chunk accepted' ($r.code -eq 0 -and $r.data.done -eq $false -and $r.data.received -eq $chunkSize)
    # 模拟「关页面/断网」：不调 abort，直接查已收字节
    $r = Get-Json "$srv/star/fileUploadStatus?path=$(Enc $fsDir)&id=$id"
    Check 'upload status reports partial' ($r.code -eq 0 -and $r.data.received -eq $chunkSize)
    $offset = [int64]$r.data.received
    $last = $null
    while ($offset -lt $total) {
        $r = Send-Chunk $offset
        if ($r.code -ne 0) { Write-Host "  chunk@$offset failed: $($r.message)"; break }
        $last = $r
        $offset += [Math]::Min($chunkSize, $total - ($offset))
    }
    Check 'resumed upload completed' ($last -and $last.data.done -eq $true -and $last.data.received -eq $total)
    Check 'resumed upload hash matches' ((Get-FileHash $big).Hash -eq (Get-FileHash (Join-Path $fsDir 'resume.bin')).Hash)
    # 再来一次同样的 id + offset=0 应被拒（服务端已收满/已落盘）
    $r = Get-Json "$srv/star/fileUploadStatus?path=$(Enc $fsDir)&id=$id"
    Check 'upload temp cleaned after completion' ($r.data.received -eq 0)

    # --- 3) fileCheck(paths)：多层目录同名预检 ---
    $targets = @(
        (Join-Path $fsDir 'resume.bin'),
        (Join-Path $fsDir 'a/b/c/none.txt')
    )
    $r = Post-Json "$srv/star/fileCheck" (@{ paths = $targets } | ConvertTo-Json -Compress)
    Check 'fileCheck(paths) finds existing' ($r.code -eq 0 -and $r.data.existingPaths.Count -eq 1)
    Check 'fileCheck(paths) returned the right one' ($r.data.existingPaths[0] -eq $targets[0])

    # --- 4) Range 下载 ---
    $url = "$srv/star/fileDownload?path=$(Enc (Join-Path $fsDir 'resume.bin'))"
    $full = Get-Range $url $null $null
    Check 'full download 200' ($full.code -eq '200')
    Check 'full download length = total' ((Get-Item $full.body).Length -eq $total)
    Check 'full download advertises Accept-Ranges' ((Hdr $full.headers 'Accept-Ranges') -eq 'bytes')
    $etag = Hdr $full.headers 'ETag'
    Check 'full download has ETag' ([bool]$etag)
    Remove-Item $full.body, $full.hdr -Force -ErrorAction SilentlyContinue

    $r = Get-Range $url 'bytes=0-0' $null
    Check 'range 0-0 -> 206' ($r.code -eq '206')
    Check 'range 0-0 -> 1 byte' ((Get-Item $r.body).Length -eq 1)
    Check 'range 0-0 -> Content-Range' ((Hdr $r.headers 'Content-Range') -eq "bytes 0-0/$total")
    Remove-Item $r.body, $r.hdr -Force -ErrorAction SilentlyContinue

    $r = Get-Range $url 'bytes=1000000-1999999' $null
    Check 'mid range -> 206 + 1MB' ($r.code -eq '206' -and (Get-Item $r.body).Length -eq 1000000)
    Remove-Item $r.body, $r.hdr -Force -ErrorAction SilentlyContinue

    $r = Get-Range $url 'bytes=-10' $null
    $tail = [IO.File]::ReadAllBytes($r.body)
    $in = [IO.File]::OpenRead($big); $in.Position = $total - 10
    $expectTail = New-Object byte[] 10; $null = $in.Read($expectTail, 0, 10); $in.Close()
    Check 'suffix range returns last 10 bytes' ($r.code -eq '206' -and [Convert]::ToBase64String($tail) -eq [Convert]::ToBase64String($expectTail))
    Remove-Item $r.body, $r.hdr -Force -ErrorAction SilentlyContinue

    $r = Get-StatusWithHeaders $url @{ Range = 'bytes=0-99'; 'If-Range' = '"deadbeef"' }
    Check 'If-Range mismatch -> 412' ($r.code -eq 412)

    $r = Get-StatusWithHeaders $url @{ Range = 'bytes=0-99'; 'If-Range' = $etag }
    Check 'If-Range match -> 206' ($r.code -eq 206 -and $r.len -eq 100)

    $r = Get-Range $url "bytes=$total-" $null
    Check 'out-of-range -> 416 + Content-Range */total' ($r.code -eq '416' -and (Hdr $r.headers 'Content-Range') -eq "bytes */$total")
    Remove-Item $r.body, $r.hdr -Force -ErrorAction SilentlyContinue

    # --- 5) >256MB 文件：整文件拒、分片可下 ---
    $huge = Join-Path $fsDir 'huge.bin'
    $hf = [IO.File]::Create($huge); $hf.SetLength(300MB); $hf.Close()
    $hurl = "$srv/star/fileDownload?path=$(Enc $huge)"
    $r = Get-Range $hurl $null $null
    $msg = [IO.File]::ReadAllText($r.body, [Text.Encoding]::UTF8)
    Check '>256MB full download rejected' ($r.code -eq '400' -and $msg -like '*分片*')
    Remove-Item $r.body, $r.hdr -Force -ErrorAction SilentlyContinue
    $r = Get-Range $hurl 'bytes=299000000-299999999' $null
    Check '>256MB range download works' ($r.code -eq '206' -and (Get-Item $r.body).Length -eq 1000000)
    Remove-Item $r.body, $r.hdr -Force -ErrorAction SilentlyContinue

    # --- 6) 小文件整取仍正常（回归） ---
    $small = Join-Path $fsDir 'small.txt'
    [IO.File]::WriteAllText($small, 'hello-range')
    $url = "$srv/star/fileDownload?path=$(Enc $small)"
    $r = Get-Range $url $null $null
    Check 'small file download still 200' ($r.code -eq '200' -and (Get-Content $r.body -Raw).Trim() -eq 'hello-range')
    Remove-Item $r.body, $r.hdr -Force -ErrorAction SilentlyContinue

    Remove-Item $big -Force -ErrorAction SilentlyContinue
}
finally {
    Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue
    Start-Sleep -Milliseconds 600
    Get-Process pek-ragent -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $exe } | Stop-Process -Force -ErrorAction SilentlyContinue
    $env:PEK_RAGENT_BASE = $oldBase
    Start-Sleep -Milliseconds 400
    Remove-Item $base -Recurse -Force -ErrorAction SilentlyContinue
}

Write-Host ""
Write-Host "== 结果：PASS $ok / FAIL $fail =="
if ($fail -gt 0) { exit 1 } else { exit 0 }
