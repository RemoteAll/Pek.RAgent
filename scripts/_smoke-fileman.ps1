# Smoke test: file manager + log cleanup APIs (temporary instance on 5599)
$ErrorActionPreference = 'Continue'
$srv = 'http://127.0.0.1:5599'
$base = 'C:\Users\qcjxb\AppData\Local\Temp\pek-smoke'
$fs = Join-Path $base 'fs'
$ok = 0; $fail = 0

function Check($name, $cond) {
    if ($cond) { Write-Host "PASS $name"; $script:ok++ } else { Write-Host "FAIL $name"; $script:fail++ }
}
function Get-Json($url) {
    $tmp = [IO.Path]::GetTempFileName()
    & curl.exe -s -m 20 -o $tmp $url
    $text = [IO.File]::ReadAllText($tmp, [Text.Encoding]::UTF8)
    Remove-Item $tmp -Force
    return ($text | ConvertFrom-Json)
}
function Post-Json($url, $json) {
    $tmp = [IO.Path]::GetTempFileName()
    [IO.File]::WriteAllText($tmp, $json)
    $out = [IO.Path]::GetTempFileName()
    & curl.exe -s -m 30 -X POST -H 'Content-Type: application/json' --data-binary "@$tmp" -o $out $url
    Remove-Item $tmp -Force
    $text = [IO.File]::ReadAllText($out, [Text.Encoding]::UTF8)
    Remove-Item $out -Force
    return ($text | ConvertFrom-Json)
}
function Enc($s) { return [uri]::EscapeDataString($s) }

# --- reset workspace ---
if (Test-Path $fs) { Remove-Item $fs -Recurse -Force }
New-Item -ItemType Directory -Force $fs | Out-Null

# --- fileList ---
$r = Get-Json "$srv/star/fileList?path=$(Enc $base)"
Check 'fileList returns base path' ($r.code -eq 0 -and $r.data.path -eq $base)
$r = Get-Json "$srv/star/fileList?path=$(Enc 'C:\Windows')"
Check 'fileList C:\Windows readable' ($r.code -eq 0 -and $r.data.items.Count -gt 10)
$r = Get-Json "$srv/star/fileList?path=$(Enc 'C:\definitely-not-exist-xyz')"
Check 'fileList missing dir -> 404' ($r.code -eq 404)

# --- mkdir / newfile / write / read ---
$r = Post-Json "$srv/star/fileMkdir" (@{ path = $base; name = 'fs2' } | ConvertTo-Json -Compress)
Check 'fileMkdir' ($r.code -eq 0)
$r = Post-Json "$srv/star/fileNewFile" (@{ path = $fs; name = 'hello.txt' } | ConvertTo-Json -Compress)
Check 'fileNewFile' ($r.code -eq 0)
$r = Post-Json "$srv/star/fileWrite" (@{ path = (Join-Path $fs 'hello.txt'); content = "hello world`nline2" } | ConvertTo-Json -Compress)
Check 'fileWrite' ($r.code -eq 0)
$r = Get-Json "$srv/star/fileRead?path=$(Enc (Join-Path $fs 'hello.txt'))"
Check 'fileRead content' ($r.code -eq 0 -and $r.data.content -eq "hello world`nline2")

# --- upload (binary) ---
$src = Join-Path $env:TEMP 'pek-upload-test.bin'
[IO.File]::WriteAllBytes($src, [byte[]](1..250))
$r = (curl.exe -s -m 30 -X POST "$srv/star/fileUpload?path=$(Enc $fs)&name=bin.dat" --data-binary "@$src" | ConvertFrom-Json)
Check 'fileUpload' ($r.code -eq 0 -and $r.data.size -eq 250)

# --- download (hash compare) ---
$dst = Join-Path $env:TEMP 'pek-download-test.bin'
& curl.exe -s -m 30 -o $dst "$srv/star/fileDownload?path=$(Enc (Join-Path $fs 'bin.dat'))"
$h1 = (Get-FileHash $src).Hash; $h2 = (Get-FileHash $dst).Hash
Check 'fileDownload bytes match' ($h1 -eq $h2)

# --- search ---
$r = Get-Json "$srv/star/fileSearch?path=$(Enc $fs)&q=HELLO"
Check 'fileSearch finds hello.txt' ($r.code -eq 0 -and $r.data.items.Count -ge 1)

# --- rename / copy / move ---
$r = Post-Json "$srv/star/fileRename" (@{ path = (Join-Path $fs 'hello.txt'); newName = 'hello2.txt' } | ConvertTo-Json -Compress)
Check 'fileRename' ($r.code -eq 0)
$r = Post-Json "$srv/star/fileCopy" (@{ paths = @((Join-Path $fs 'hello2.txt')); target = (Join-Path $base 'fs2') } | ConvertTo-Json -Compress)
Check 'fileCopy' ($r.code -eq 0)
$r = Post-Json "$srv/star/fileMove" (@{ paths = @((Join-Path $base 'fs2\hello2.txt')); target = $fs } | ConvertTo-Json -Compress)
Check 'fileMove back' ($r.code -ne 0)  # target already has same name -> expect reject
Remove-Item (Join-Path $base 'fs2\hello2.txt') -Force
$r = Post-Json "$srv/star/fileMove" (@{ paths = @((Join-Path $fs 'hello2.txt')); target = (Join-Path $base 'fs2') } | ConvertTo-Json -Compress)
Check 'fileMove' ($r.code -eq 0)
$r = Post-Json "$srv/star/fileMove" (@{ paths = @((Join-Path $base 'fs2\hello2.txt')); target = $fs } | ConvertTo-Json -Compress)
Check 'fileMove back ok' ($r.code -eq 0)

# --- compress / extract ---
$r = Post-Json "$srv/star/fileCompress" (@{ paths = @($fs); target = $base; name = 'fs-archive.zip' } | ConvertTo-Json -Compress)
Check 'fileCompress' ($r.code -eq 0 -and $r.data.fileCount -ge 2)
$outDir = Join-Path $base 'fs2'
$r = Post-Json "$srv/star/fileExtract" (@{ path = (Join-Path $base 'fs-archive.zip'); target = $outDir } | ConvertTo-Json -Compress)
Check 'fileExtract' ($r.code -eq 0 -and $r.data.fileCount -ge 2)
Check 'extract produced bin.dat' (Test-Path (Join-Path $outDir 'fs\bin.dat'))

# --- protected path reject ---
$r = Post-Json "$srv/star/fileDelete" (@{ paths = @('C:\Windows') } | ConvertTo-Json -Compress)
Check 'fileDelete protected -> 400' ($r.code -eq 400)
$r = Post-Json "$srv/star/fileDelete" (@{ paths = @($base) } | ConvertTo-Json -Compress)
Check 'fileDelete base itself -> 400' ($r.code -eq 400)

# --- delete batch ---
$r = Post-Json "$srv/star/fileDelete" (@{ paths = @($fs, (Join-Path $base 'fs2'), (Join-Path $base 'fs-archive.zip')) } | ConvertTo-Json -Compress)
Check 'fileDelete batch' ($r.code -eq 0 -and $r.data.deleted -eq 3 -and $r.data.freedBytes -gt 0)

# --- log cleanup: custom paths via config ---
$lcDir = Join-Path $base 'lc'
$lcSub = Join-Path $lcDir 'cache'
New-Item -ItemType Directory -Force $lcSub | Out-Null
[IO.File]::WriteAllText((Join-Path $lcDir 'app.log'), ('x' * 1000))
[IO.File]::WriteAllText((Join-Path $lcSub 'c1.bin'), ('y' * 500))

$cfgJson = (@{ LogCleanupPaths = "$lcDir;$lcSub" } | ConvertTo-Json -Compress)
$r = Post-Json "$srv/api/updateConfig" $cfgJson
Check 'updateConfig LogCleanupPaths' ($r.code -eq 0)

$r = Get-Json "$srv/star/logCleanScan"
$custom = $r.data.categories | Where-Object { $_.key -eq 'custom' }
Check 'logCleanScan has custom category' ($null -ne $custom)
Check 'custom dedup: one target, size 1500' ($null -ne $custom -and $custom.size -eq 1500 -and $custom.count -eq 2 -and $custom.targets.Count -eq 1)
Check 'scan has agent category' (($r.data.categories | Where-Object { $_.key -eq 'agent' }).Count -eq 1)

$r = Post-Json "$srv/star/logCleanRun" '{"keys":["custom"]}'
Check 'logCleanRun custom' ($r.code -eq 0 -and $r.data.freedBytes -eq 1500 -and ($r.data.details[0].errors.Count -eq 0))
Check 'app.log truncated' ((Test-Path (Join-Path $lcDir 'app.log')) -and (Get-Item (Join-Path $lcDir 'app.log')).Length -eq 0)
Check 'lc kept / cache subdir removed' ((Test-Path $lcDir) -and -not (Test-Path $lcSub))
$r = Get-Json "$srv/star/logCleanScan"
$custom2 = $r.data.categories | Where-Object { $_.key -eq 'custom' }
Check 'custom now zero after clean' ($null -eq $custom2 -or $custom2.size -eq 0)

$r = Post-Json "$srv/api/updateConfig" '{"LogCleanupPaths":""}'
Check 'reset LogCleanupPaths' ($r.code -eq 0)

# --- summary ---
Write-Host "== SMOKE RESULT: $ok passed, $fail failed =="
