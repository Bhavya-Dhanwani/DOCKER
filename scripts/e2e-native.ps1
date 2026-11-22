# Windows WITHOUT WSL2: the CLI must keep working in native mode (Job Objects) and explain what is missing.
# SASTA_NO_WSL=1 makes the CLI behave as if WSL2 were not installed. Usage: scripts\e2e-native.ps1 [path\to\docker.exe]
param([string]$Docker = "$PSScriptRoot\..\target\release\docker.exe")
$ErrorActionPreference = 'Continue'
$env:SASTA_NO_WSL = '1'
$fails = 0
function Check($name, $ok) { if ($ok) { "PASS $name" } else { "FAIL $name"; $script:fails++ } }
function Run { param([string[]]$a) $o = & $Docker @a 2>&1 | Out-String; [pscustomobject]@{ Out = $o; Code = $LASTEXITCODE } }

$v = Run @('version')
Check 'version says native Windows mode' ($v.Out -match 'Engine: native Windows mode')
Check 'version still shows the client' ($v.Out -match 'Client: Sasta Docker')

$s = Run @('vm', 'status')
Check 'vm status explains there is no VM and how to get one' ($s.Out -match 'none' -and $s.Out -match 'wsl --install')

$i = Run @('vm', 'init')
Check 'vm init fails with an explanation, not a crash' ($i.Code -ne 0 -and $i.Out -match 'no Linux VM')

$r = Run @('run', 'alpine', 'echo', 'hi')
Check 'running an image fails clearly' ($r.Code -ne 0 -and $r.Out -match 'images need a Linux engine' -and $r.Out -match 'wsl --install')

$dir = Join-Path $env:TEMP "sasta-native-$PID"
New-Item -ItemType Directory -Force $dir | Out-Null
$n = Run @('run', '--mem', '128', $dir, 'cmd', '/c', 'echo native-ok')
Check 'a Windows folder runs as a native container' ($n.Code -eq 0 -and $n.Out -match 'native-ok')
$e = Run @('run', $dir, 'cmd', '/c', 'exit 7')
Check 'its exit code is passed through' ($e.Code -eq 7)

foreach ($c in @('ps', 'ps -a', 'images', 'volume ls', 'network ls', 'system df')) {
    $x = Run ($c -split ' ')
    Check "'$c' works without WSL (lists, no error)" ($x.Code -eq 0 -and $x.Out -notmatch 'docker:')
}
$st = Run @('stats', '--no-stream', '--json')
Check 'stats --json works (empty when nothing runs)' ($st.Code -eq 0)

Remove-Item $dir -Recurse -Force -ErrorAction SilentlyContinue
Remove-Item Env:SASTA_NO_WSL
if ($fails -eq 0) { 'ALL PASSED' } else { "$fails FAILED"; exit 1 }
