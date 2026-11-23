# Benchmarks one engine on this PC and writes bench/<engine>.json.
#   scripts\bench.ps1 -Engine sasta     # the docker.exe of this repo (target\release), engine in its own WSL2 distro
#   scripts\bench.ps1 -Engine docker    # Docker Desktop, started here and quit again at the end
# Run them one after the other (each one starts from `wsl --shutdown`, so only one engine's VM is alive at a time).
param([Parameter(Mandatory)][ValidateSet('sasta', 'docker')]$Engine, [int]$Runs = 15, [int]$Containers = 20)
$ErrorActionPreference = 'Continue'
$root = Split-Path $PSScriptRoot
$out = Join-Path $root 'bench'; New-Item -ItemType Directory -Force $out | Out-Null
$dk = if ($Engine -eq 'sasta') { Join-Path $root 'target\release\docker.exe' } else { 'C:\Program Files\Docker\Docker\resources\bin\docker.exe' }

function Stop-Docker-Desktop {
    Get-Process 'Docker Desktop', 'com.docker.*', 'docker-*' -ErrorAction SilentlyContinue | Where-Object { $_.Path -like '*Docker*' } | Stop-Process -Force -ErrorAction SilentlyContinue
}
function Mb($names) {
    $b = 0; foreach ($p in Get-Process -ErrorAction SilentlyContinue) { if ($names | Where-Object { $p.Name -like $_ }) { $b += $p.WorkingSet64 } }
    [math]::Round($b / 1MB)
}
# everything the engine costs the PC while it sits there: the WSL2 VM plus the engine's own processes
$ramNames = if ($Engine -eq 'sasta') { 'vmmem*', 'docker', 'docker-gui' } else { 'vmmem*', 'com.docker.*', 'Docker Desktop', 'docker-*' }
function Ms($sb) { $w = [Diagnostics.Stopwatch]::StartNew(); & $sb | Out-Null; [math]::Round($w.Elapsed.TotalMilliseconds) }
function Median($a) { $s = $a | Sort-Object; $s[[int][math]::Floor($s.Count / 2)] }

Stop-Docker-Desktop
wsl --shutdown; Start-Sleep 8
$base = Mb $ramNames   # nothing running: the PC's own noise floor for these process names

$t0 = Get-Date
if ($Engine -eq 'docker') {
    Start-Process 'C:\Program Files\Docker\Docker\Docker Desktop.exe'
    for ($i = 0; $i -lt 90; $i++) { Start-Sleep 3; & $dk info *> $null; if ($LASTEXITCODE -eq 0) { break } }
} else {
    & $dk ps *> $null   # the first command starts the VM
}
$ready = [math]::Round(((Get-Date) - $t0).TotalSeconds, 1)
if ($LASTEXITCODE -ne 0 -and $Engine -eq 'docker') { throw 'Docker Desktop did not come up' }

& $dk pull alpine *> $null
Start-Sleep 60   # let it settle
$idle = Mb $ramNames

# cold start of a container, as the user sees it (the whole command)
$starts = 1..$Runs | ForEach-Object { Ms { & $dk run --rm alpine true } }

# pulling (needs the network: this number moves with the connection)
$pulls = @{}
foreach ($img in 'nginx:alpine', 'redis:alpine') {
    & $dk rmi -f $img *> $null
    $pulls[$img] = Ms { & $dk pull $img }
}

# memory with many containers running
$ids = 1..$Containers | ForEach-Object { & $dk run -d nginx:alpine }
Start-Sleep 10
$loaded = Mb $ramNames
$runningNow = (& $dk ps -q | Measure-Object).Count
& $dk rm -f @($ids) *> $null

# build: cold, then again with everything cached
$b = Join-Path $env:TEMP "sasta-bench-$Engine"; Remove-Item $b -Recurse -Force -ErrorAction SilentlyContinue; New-Item -ItemType Directory $b | Out-Null
Set-Content "$b\data.txt" ('x' * 100000)
Set-Content "$b\Dockerfile" "FROM alpine`nRUN echo 1 > /a`nCOPY data.txt /d`nRUN cat /d > /b`nRUN echo 2 > /c`nRUN echo done`n"
& $dk rmi -f bench:1 *> $null
$buildCold = Ms { & $dk build -t bench:1 $b }
$buildCached = Ms { & $dk build -t bench:1 $b }
& $dk rmi -f bench:1 *> $null

# disk: the VM disk file on the host, after all of the above
$vhdx = if ($Engine -eq 'sasta') { Get-ChildItem 'D:\WSL\sasta-home\vm\rust-docker\ext4.vhdx' } else { Get-ChildItem "$env:LOCALAPPDATA\Docker\wsl" -Recurse -Filter ext4.vhdx -ErrorAction SilentlyContinue | Sort-Object Length -Descending | Select-Object -First 1 }
$install = if ($Engine -eq 'sasta') { (Get-ChildItem (Join-Path $root 'target\release') -File | Where-Object { $_.Name -in 'docker.exe', 'docker-gui.exe' -or $_.Name -like 'docker-linux-*' } | Measure-Object Length -Sum).Sum } else { (Get-ChildItem 'C:\Program Files\Docker' -Recurse -File -ErrorAction SilentlyContinue | Measure-Object Length -Sum).Sum }

$res = [ordered]@{
    engine = $Engine; date = (Get-Date -Format 'yyyy-MM-dd'); runs = $Runs; containers = $runningNow
    base_ram_mb = $base; ready_s = $ready; idle_ram_mb = $idle; loaded_ram_mb = $loaded
    start_ms = $starts; start_median_ms = (Median $starts); start_min_ms = ($starts | Measure-Object -Minimum).Minimum
    pull_ms = $pulls; build_cold_ms = $buildCold; build_cached_ms = $buildCached
    vhdx_mb = [math]::Round($vhdx.Length / 1MB); install_mb = [math]::Round($install / 1MB)
}
$res | ConvertTo-Json | Set-Content (Join-Path $out "$Engine.json") -Encoding utf8
$res | ConvertTo-Json

if ($Engine -eq 'docker') { Stop-Docker-Desktop }
wsl --shutdown
