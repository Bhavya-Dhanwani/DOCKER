# Sasta Docker installer for Windows (no administrator needed):
#   irm https://raw.githubusercontent.com/Bhavya-Dhanwani/DOCKER/main/install.ps1 | iex
# From CMD:
#   powershell -NoProfile -ExecutionPolicy Bypass -Command "irm https://raw.githubusercontent.com/Bhavya-Dhanwani/DOCKER/main/install.ps1 | iex"
$ErrorActionPreference = 'Stop'
$arch = if ($env:PROCESSOR_ARCHITECTURE -eq 'ARM64') { 'arm64' } else { 'x64' }
$file = "SastaDocker-Setup-$arch.exe"
$url = "https://github.com/Bhavya-Dhanwani/DOCKER/releases/latest/download/$file"
$dest = Join-Path $env:TEMP $file
Write-Host "Downloading $file ..."
Invoke-WebRequest $url -OutFile $dest
Unblock-File $dest -ErrorAction SilentlyContinue   # downloaded by this script, so Windows does not show its "protected your PC" screen
Write-Host "Starting the installer ..."
# the installer asks which command to use (docker or sdocker); SASTA_CMD=sdocker answers it up front
$installerArgs = @(); if ($env:SASTA_CMD) { $installerArgs += "/CMDNAME=$($env:SASTA_CMD)" }
if ($installerArgs) { Start-Process $dest -ArgumentList $installerArgs -Wait } else { Start-Process $dest -Wait }
Remove-Item $dest -ErrorAction SilentlyContinue
Write-Host "Done. Open a new terminal and try:  docker run alpine echo hello   (first time: docker vm init, needs WSL2)"
