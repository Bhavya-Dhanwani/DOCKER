# Windows -> Linux-VM end-to-end test. Uses an EXISTING WSL distro as the "VM" (default Ubuntu), so nothing is
# created or imported. Needs target\release\docker.exe, the Linux binary, and an alpine store to copy in:
#   scripts\e2e-vm.ps1 [-Distro Ubuntu] [-Store .testhome\store]
param([string]$Distro = 'Ubuntu', [string]$Store = '.testhome\store')
$ErrorActionPreference = 'Continue'
Set-Location "$PSScriptRoot\.."
$docker = (Resolve-Path 'target\release\docker.exe').Path
$linuxBin = (Resolve-Path 'target\aarch64-unknown-linux-musl\release\docker').Path
$storeWin = (Resolve-Path $Store).Path
$work = Join-Path (Get-Location) '.vmtest'
Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force "$work\data\sub", "$work\ctx" | Out-Null
Set-Content "$work\data\file.txt" 'from-windows' -NoNewline
Set-Content "$work\ctx\hello.txt" 'copied-from-windows' -NoNewline
Set-Content "$work\ctx\Dockerfile" "FROM alpine`nCOPY hello.txt /app/hello.txt`nRUN echo built-in-vm > /app/built`nCMD [`"cat`", `"/app/hello.txt`", `"/app/built`"]`n"

$env:RUST_DOCKER_DISTRO = $Distro
$env:RUST_DOCKER_LINUX_BIN = $linuxBin
$env:RUST_DOCKER_VM_BIN = '/root/vm-docker'
$env:RUST_DOCKER_VM_HOME = '/root/vmtest'
$wslStore = '/mnt/' + $storeWin.Substring(0, 1).ToLower() + ($storeWin.Substring(2) -replace '\\', '/')

$script:fail = 0
function Check($name, [bool]$ok) { if ($ok) { "PASS $name" } else { "FAIL $name"; $script:fail++ } }
function Vm($sh) { wsl.exe -d $Distro -u root --exec sh -c $sh 2>&1 }
function D { & $docker @args 2>&1 | Out-String }

# pkill -f matches whole command lines, so it gets its own call whose text has nothing else to match
Vm "pkill -f '[s]leep 2147483647'" | Out-Null
Vm "rm -rf /root/vmtest /root/vm-docker; mkdir -p /root/vmtest && cp -r '$wslStore' /root/vmtest/store" | Out-Null

"== vm setup"
$dry = D vm init --distro $Distro --use-existing --dry-run
Check 'dry-run prints the install step and changes nothing' (($dry -match 'install the Linux docker engine') -and ((Vm 'test -e /root/vm-docker && echo yes || echo no') -match 'no'))
$init = D vm init --distro $Distro --use-existing
Check 'vm init installs the engine into the VM' (($init -match 'engine: Sasta Docker') -and ((Vm '/root/vm-docker version') -match 'linux'))
$st = D vm status
Check 'vm status reports running + engine' (($st -match 'running') -and ($st -match 'engine:\s+Sasta Docker'))
$ver = D version
Check 'version shows client and VM' (($ver -match 'Client: Sasta Docker.*windows') -and ($ver -match 'VM:\s+Sasta Docker.*linux'))

"== commands forwarded to the VM"
Check 'images lists the VM store' ((D images) -match 'alpine:latest')
Check 'run prints container output' ((D run --network none alpine echo hello-from-vm) -match 'hello-from-vm')
& $docker run --network none alpine sh -c 'exit 7' 2>&1 | Out-Null
Check 'container exit code reaches PowerShell (7)' ($LASTEXITCODE -eq 7)
& $docker bogus 2>&1 | Out-Null
$usageCode = $LASTEXITCODE
Check 'usage errors keep exit code 2' ($usageCode -eq 2)
& $docker run nosuchimage 2>&1 | Out-Null
Check 'missing image fails non-zero' ($LASTEXITCODE -ne 0)

$id = (D run -d --network none --name winweb alpine sh -c 'echo started; sleep 120').Trim()
Check 'run -d prints an id' ($id.Length -ge 12)
Start-Sleep 1
Check 'ps lists it as Up' ((D ps) -match 'winweb.*Up')
Check 'logs reads container output' ((D logs winweb) -match 'started')
Check 'exec runs inside the container' ((D exec winweb hostname) -match 'container')
Check 'stop + rm work' (((D stop winweb) -match 'winweb') -and ((D rm winweb) -match 'winweb') -and -not ((D ps -a) -match 'winweb'))

"== Windows paths"
Check '-v with a relative Windows path (bind mount from a Windows drive)' ((D run --network none -v .\.vmtest\data:/data:ro alpine cat /data/file.txt) -match 'from-windows')
$ro = & $docker run --network none -v "$work\data:/data:ro" alpine sh -c 'touch /data/new' 2>&1; $roCode = $LASTEXITCODE
Check ':ro is enforced for Windows-drive volumes' (($roCode -ne 0) -and -not (Test-Path "$work\data\new"))
& $docker run --network none -v "$work\data:/data" alpine sh -c 'echo written > /data/out.txt' 2>&1 | Out-Null
Check 'a read-write mount writes back to the Windows folder' ((Test-Path "$work\data\out.txt") -and ((Get-Content "$work\data\out.txt" -Raw) -match 'written'))
$b = D build -t winapp -f "$work\ctx\Dockerfile" "$work\ctx"
Check 'build with a Windows context + Dockerfile path' ($b -match 'Successfully built and tagged winapp:latest')
Check 'built image has the COPY (from Windows) and the RUN (in the VM)' ((D run --network none winapp) -match 'copied-from-windowsbuilt-in-vm')
$e = D run . echo hi
Check 'running a directory explains --native' ($e -match '--native')

"== Windows-native backend (Job Objects) is still there"
& $docker --native run --mem 64 . cmd /c exit 3 2>&1 | Out-Null
Check '--native run works and returns the exit code (3)' ($LASTEXITCODE -eq 3)
& $docker --native run nosuchdir echo 2>&1 | Out-Null
Check '--native with a non-directory fails instead of using the VM' ($LASTEXITCODE -ne 0)

"== regression: capturing output must not hang when the VM has to be woken up"
wsl.exe --terminate $Distro | Out-Null
Start-Sleep 1
$psi = New-Object Diagnostics.ProcessStartInfo
$psi.FileName = $docker; $psi.Arguments = 'images'
$psi.RedirectStandardOutput = $true; $psi.RedirectStandardError = $true; $psi.UseShellExecute = $false
$p = [Diagnostics.Process]::Start($psi)
$t = $p.StandardOutput.ReadToEndAsync()
# a hidden keepalive that inherited our output pipe would keep it open forever
Check 'output reaches end-of-stream (keepalive does not inherit the pipe)' ($t.Wait(20000) -and $t.Result -match 'alpine')

"== cleanup"
D rmi winapp | Out-Null
Vm "pkill -f '[s]leep 2147483647'" | Out-Null
Vm "rm -rf /root/vmtest /root/vm-docker" | Out-Null
Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue
Check 'no leftover test files in the VM' ((Vm 'test -e /root/vm-docker -o -e /root/vmtest && echo left || echo clean') -match 'clean')
if ($script:fail -eq 0) { 'ALL PASSED' } else { "$($script:fail) FAILED"; exit 1 }
