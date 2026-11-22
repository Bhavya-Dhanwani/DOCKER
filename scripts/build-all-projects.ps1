# Build every compose file and Dockerfile found under a folder with Sasta Docker; one PASS/FAIL line each.
# Usage: .\scripts\build-all-projects.ps1 -Root D:\work [-Timeout 900]
param([string]$Root = 'D:\work', [int]$Timeout = 900, [string]$Out = "$PSScriptRoot\..\.testhome\projects.txt")
$d = Join-Path $PSScriptRoot '..\target\release\docker.exe'
$skip = 'node_modules|\\target\\|\\\.testhome|\\\.git\\|\\launch\\docker\\'
$files = Get-ChildItem $Root -Recurse -ErrorAction SilentlyContinue -Include docker-compose*.yml, docker-compose*.yaml, compose.yml, compose.yaml, Dockerfile, dockerfile |
    Where-Object { $_.FullName -notmatch $skip }
Set-Content $Out "# $(Get-Date -Format s) $($files.Count) files under $Root"
function Run-Timed($dir, $argv) {
    $log = New-TemporaryFile
    $p = Start-Process $d -ArgumentList $argv -WorkingDirectory $dir -NoNewWindow -PassThru -RedirectStandardOutput $log -RedirectStandardError "$log.err"
    $null = $p.Handle # keep the process handle: without it Start-Process -PassThru loses the exit code
    if (-not $p.WaitForExit($Timeout * 1000)) { $p.Kill(); return @(124, 'timeout') }
    $p.WaitForExit()
    $tail = (Get-Content $log, "$log.err" -Tail 40 -ErrorAction SilentlyContinue | Where-Object { $_ -match 'rror|fail|FAIL|not found|denied|unsupported' } | Select-Object -Last 3) -join ' | '
    Remove-Item $log, "$log.err" -ErrorAction SilentlyContinue
    @($p.ExitCode, $tail)
}
foreach ($f in $files) {
    $dir = $f.DirectoryName
    $isCompose = $f.Name -match 'compose'
    $argv = if ($isCompose) { @('compose', '-f', $f.Name, 'build') } else { @('build', '-f', $f.Name, '-t', 'sweep-test', '.') }
    $r = Run-Timed $dir $argv
    $line = "{0} {1}" -f $(if ($r[0] -eq 0) { 'PASS' } else { 'FAIL' }), $f.FullName
    if ($r[0] -ne 0) { $line += "`n     -> $($r[1])" }
    Add-Content $Out $line
}
Add-Content $Out '# done'
