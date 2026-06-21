# Assemble the Windows release artifacts from a release build:
#   - build\Red-<version>-x86_64-windows.zip          (portable install)
#   - build\Red-<version>-x86_64-windows.exe + .sha256 (self-update payload)
#
#   pwsh scripts/bundle-windows.ps1 [-Version 0.8.0]
#
# The zip is a first-cut portable install (Red.exe + LICENSE + README + a
# `.red-portable` marker the self-updater uses to tell a real install from a dev
# build). The exe carries the default Windows icon for now; embedding
# assets/red.svg as the .exe icon (via the `winresource` crate in build.rs) and
# shipping a signed MSI are documented follow-ups — see
# docs/plans/future/cross-platform.md. Authenticode signing is left to the
# release pipeline; self-update integrity currently rests on the .sha256 sidecar.
param(
  [string]$Version = ""
)
$ErrorActionPreference = "Stop"
$Root = Resolve-Path (Join-Path $PSScriptRoot "..")

if (-not $Version) {
  $Version = (git -C $Root describe --tags --always) -replace '^v',''
}

Write-Host "> cargo build --release"
cargo build -p red --release
$Bin = Join-Path $Root "target\release\Red.exe"
if (-not (Test-Path $Bin)) { throw "missing $Bin" }

$Build = Join-Path $Root "build"
New-Item -ItemType Directory -Force -Path $Build | Out-Null

# --- portable zip ---------------------------------------------------------
$Stage = Join-Path $Build "Red-windows"
Write-Host "> staging $Stage (v$Version)"
if (Test-Path $Stage) { Remove-Item -Recurse -Force $Stage }
New-Item -ItemType Directory -Force -Path $Stage | Out-Null
Copy-Item $Bin (Join-Path $Stage "Red.exe")
Copy-Item (Join-Path $Root "LICENSE") (Join-Path $Stage "LICENSE.txt") -ErrorAction SilentlyContinue
"Red $Version`r`nRoughly Enough Data — a fast database explorer.`r`nRun Red.exe to start." |
  Out-File -Encoding utf8 (Join-Path $Stage "README.txt")
# Marker the self-updater checks before replacing Red.exe — present only in a
# distributed portable install, never in a `cargo run` dev tree.
"This file marks a portable Red install and enables in-app self-update." |
  Out-File -Encoding ascii (Join-Path $Stage ".red-portable")

$Zip = Join-Path $Build "Red-$Version-x86_64-windows.zip"
Write-Host "> packaging $Zip"
if (Test-Path $Zip) { Remove-Item -Force $Zip }
Compress-Archive -Path (Join-Path $Stage "*") -DestinationPath $Zip

# --- self-update payload: bare exe + checksum sidecar ---------------------
$ExeOut = Join-Path $Build "Red-$Version-x86_64-windows.exe"
Copy-Item $Bin $ExeOut -Force
$Hash = (Get-FileHash -Algorithm SHA256 -LiteralPath $ExeOut).Hash.ToLower()
"$Hash  $(Split-Path $ExeOut -Leaf)" |
  Out-File -Encoding ascii -NoNewline "$ExeOut.sha256"
Write-Host "> done: $Zip"
Write-Host "        $ExeOut (+ .sha256)"
