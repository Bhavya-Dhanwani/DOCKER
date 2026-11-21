# Test, then build Windows cli+gui and the static Linux engine for this machine's CPU.
# `docker vm init` installs docker-linux-<arch> (copied next to docker.exe) into the hidden Linux VM.
$ErrorActionPreference = 'Stop'
Set-Location "$PSScriptRoot\.."
$arch = if ($env:PROCESSOR_ARCHITECTURE -eq 'ARM64') { 'aarch64' } else { 'x86_64' }
$linux = "$arch-unknown-linux-musl"
rustup target add $linux
cargo test --workspace
cargo build --release -p docker-cli -p docker-gui
cargo build --release -p docker-cli --target $linux
Copy-Item "target\$linux\release\docker" "target\release\docker-linux-$arch" -Force
Get-Item target\release\docker.exe, target\release\docker-gui.exe, "target\release\docker-linux-$arch" | Select-Object Name, Length
