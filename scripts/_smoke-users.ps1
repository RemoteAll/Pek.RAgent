# Smoke test: users / permissions / audit log (temporary instance on 5599)
$ErrorActionPreference = 'Continue'
$srv = 'http://127.0.0.1:5599'
$ok = 0; $fail = 0
function Check($name, $cond) {
    if ($cond) { Write-Host "PASS $name"; $script:ok++ } else { Write-Host "FAIL $name"; $script:fail++ }
}
function Req($method, $path, $body, $token) {
    $tmp = $null
    $args = @('-s', '-m', '20', '-X', $method, "$srv$path")
    if ($token) { $args += @('-H', "Authorization: Bearer $token") }
    if ($null -ne $body) {
        $tmp = [IO.Path]::GetTempFileName()
        [IO.File]::WriteAllText($tmp, $body)
        $args += @('-H', 'Content-Type: application/json', '--data-binary', "@$tmp")
    }
    $out = [IO.Path]::GetTempFileName()
    $args += @('-o', $out)
    & curl.exe @args | Out-Null
    $text = [IO.File]::ReadAllText($out, [Text.Encoding]::UTF8)
    Remove-Item $out -Force
    if ($tmp) { Remove-Item $tmp -Force -ErrorAction SilentlyContinue }
    return ($text | ConvertFrom-Json)
}

# 1. admin login + me
$r = Req 'POST' '/api/login' '{"user":"admin","password":"admin"}' $null
Check 'admin login' ($r.code -eq 0)
$admin = $r.data.token
$r = Req 'GET' '/api/me' $null $admin
Check 'admin me isAdmin with all perms' ($r.code -eq 0 -and $r.data.isAdmin -eq $true -and $r.data.perms.Count -eq 13)

# 2. create user u1 (dashboard + fileman)
$r = Req 'POST' '/star/userSave' '{"name":"u1","password":"u1pass","permissions":["dashboard","fileman"],"enabled":true,"remark":"smoke"}' $admin
Check 'create user u1' ($r.code -eq 0)

# 2b. builtin admin: listed (virtual row) / not deletable / password via userSave
$r = Req 'GET' '/star/userList' $null $admin
$builtin = $r.data.users | Where-Object { $_.isBuiltin -eq $true }
Check 'userList shows builtin admin' ($null -ne $builtin -and $builtin.userName -eq 'admin')
$r = Req 'POST' '/star/userDelete' '{"name":"admin"}' $admin
Check 'builtin admin cannot be deleted' ($r.code -eq 400)
$r = Req 'POST' '/star/userSave' '{"name":"admin","password":"admin2"}' $admin
Check 'change builtin admin password via userSave' ($r.code -eq 0)
$r = Req 'POST' '/api/login' '{"user":"admin","password":"admin"}' $null
Check 'old admin password rejected' ($r.code -eq 401)
$r = Req 'POST' '/api/login' '{"user":"admin","password":"admin2"}' $null
Check 'new admin password works' ($r.code -eq 0)

# 2c. builtin admin rename: collision rejected / rename + login / rename back
$r = Req 'POST' '/star/userSave' '{"name":"admin","newName":"u1"}' $admin
Check 'rename builtin to existing name rejected' ($r.code -eq 400)
$r = Req 'POST' '/star/userSave' '{"name":"admin","newName":"boss"}' $admin
Check 'rename builtin admin (no password change)' ($r.code -eq 0)
$r = Req 'POST' '/api/login' '{"user":"admin","password":"admin2"}' $null
Check 'old admin name rejected after rename' ($r.code -eq 401)
$r = Req 'POST' '/api/login' '{"user":"boss","password":"admin2"}' $null
Check 'renamed admin login works' ($r.code -eq 0)
$r = Req 'POST' '/star/userSave' '{"name":"boss","newName":"admin"}' $admin
Check 'rename back to admin' ($r.code -eq 0)

# 2d. plugins: list / invalid zip rejected / bad id rejected
$r = Req 'GET' '/star/pluginList' $null $admin
Check 'pluginList works (empty)' ($r.code -eq 0 -and $r.data.plugins.Count -eq 0)
$r = Req 'POST' '/star/pluginInstall?name=bad.zip' 'not a zip' $admin
Check 'pluginInstall rejects invalid zip' ($r.code -eq 400)
$r = Req 'POST' '/star/pluginDelete' '{"id":".."}' $admin
Check 'pluginDelete rejects bad id' ($r.code -eq 400)
$r = Req 'GET' '/star/pluginStore' $null $admin
Check 'pluginStore unconfigured by default' ($r.code -eq 0 -and $r.data.configured -eq $false)

# 3. u1 login + me
$r = Req 'POST' '/api/login' '{"user":"u1","password":"u1pass"}' $null
Check 'u1 login' ($r.code -eq 0)
$u1 = $r.data.token
$r = Req 'GET' '/api/me' $null $u1
Check 'u1 perms = dashboard,fileman' ($r.code -eq 0 -and ($r.data.perms -join ',') -eq 'dashboard,fileman')

# 4. u1 allowed / denied endpoints
$r = Req 'GET' '/star/fileList' $null $u1
Check 'u1 fileList allowed' ($r.code -eq 0)
$r = Req 'GET' '/star/dbInfo' $null $u1
Check 'u1 dbInfo denied (403)' ($r.code -eq 403)
$r = Req 'POST' '/star/dbCreateBackup' '{}' $u1
Check 'u1 dbCreateBackup denied (403)' ($r.code -eq 403)
$r = Req 'GET' '/star/auditLogs' $null $u1
Check 'u1 auditLogs denied (403)' ($r.code -eq 403)
$r = Req 'GET' '/api/status' $null $u1
Check 'u1 status allowed (dashboard)' ($r.code -eq 0)

# 5. admin sees audit records for u1
$r = Req 'GET' '/star/auditLogs?user=u1&size=100' $null $admin
Check 'audit has u1 records (>=4)' ($r.code -eq 0 -and $r.data.total -ge 4)
$actions = ($r.data.items | ForEach-Object { $_.action }) -join ','
Check 'audit includes u1 login' ($actions -match 'login')
Check 'audit includes denied entries' (($r.data.items | Where-Object { $_.success -eq $false }).Count -ge 3)

# 6. disable u1 -> login rejected; delete u1
$r = Req 'POST' '/star/userSave' '{"name":"u1","permissions":["dashboard"],"enabled":false,"remark":""}' $admin
Check 'disable u1' ($r.code -eq 0)
$r = Req 'POST' '/api/login' '{"user":"u1","password":"u1pass"}' $null
Check 'disabled user login rejected' ($r.code -eq 401)
$r = Req 'POST' '/star/userDelete' '{"name":"u1"}' $admin
Check 'delete u1' ($r.code -eq 0)
$r = Req 'GET' '/star/userList' $null $admin
Check 'u1 gone from user list' ($r.code -eq 0 -and ($r.data.users | Where-Object { $_.userName -eq 'u1' }).Count -eq 0)

# 7. keyword filter + pagination shape
$r = Req 'GET' '/star/auditLogs?q=fileList&size=5' $null $admin
Check 'audit keyword filter works' ($r.code -eq 0 -and $r.data.total -ge 0)
$r = Req 'GET' '/star/auditLogs?page=1&size=3' $null $admin
Check 'audit pagination size respected' ($r.code -eq 0 -and $r.data.items.Count -le 3)

# 8. module gating: audit permission grantable
$r = Req 'POST' '/star/userSave' '{"name":"u2","password":"u2pass","permissions":["audit"],"enabled":true}' $admin
Check 'create u2 with audit perm' ($r.code -eq 0)
$r = Req 'POST' '/api/login' '{"user":"u2","password":"u2pass"}' $null
$u2 = $r.data.token
$r = Req 'GET' '/star/auditLogs?size=5' $null $u2
Check 'u2 can read audit logs' ($r.code -eq 0)
$r = Req 'GET' '/star/fileList' $null $u2
Check 'u2 fileList denied (403)' ($r.code -eq 403)
$r = Req 'POST' '/star/userDelete' '{"name":"u2"}' $admin
Check 'cleanup u2' ($r.code -eq 0)

# 9. dbInfo shows 4 tables; backup/restore round-trip keeps users & audit
$r = Req 'GET' '/star/dbInfo' $null $admin
Check 'dbInfo shows 4 tables' ($r.code -eq 0 -and $r.data.tables.Count -eq 4)
$r = Req 'POST' '/star/userSave' '{"name":"keepme","password":"kp123","permissions":["dashboard"],"enabled":true}' $admin
Check 'create keepme' ($r.code -eq 0)
$backupFile = [IO.Path]::GetTempFileName()
& curl.exe -s -m 30 -H "Authorization: Bearer $admin" -o $backupFile "$srv/star/dbBackup" | Out-Null
$magic = [IO.File]::ReadAllBytes($backupFile)[0..1] -join ','
Check 'backup zip downloaded' ($magic -eq '80,75')
$out = [IO.Path]::GetTempFileName()
& curl.exe -s -m 30 -X POST -H "Authorization: Bearer $admin" --data-binary "@$backupFile" -o $out "$srv/star/dbRestore" | Out-Null
$j = ([IO.File]::ReadAllText($out, [Text.Encoding]::UTF8) | ConvertFrom-Json)
Remove-Item $backupFile, $out -Force
Check 'restore ok with user table' ($j.code -eq 0 -and $j.data.rows.Agent_PanelUser -ge 1)
$r = Req 'POST' '/api/login' '{"user":"keepme","password":"kp123"}' $null
Check 'keepme login after restore' ($r.code -eq 0)
$r = Req 'POST' '/star/userDelete' '{"name":"keepme"}' $admin
Check 'cleanup keepme' ($r.code -eq 0)

Write-Host "== USERS SMOKE RESULT: $ok passed, $fail failed =="
