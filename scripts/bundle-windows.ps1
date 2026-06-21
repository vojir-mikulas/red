# Assemble build\Red-<version>-x86_64-windows.zip from a release build.
#
#   pwsh scripts/bundle-windows.ps1 [-Version 0.8.0]
#
# A first-cut portable zip: the Red.exe plus LICENSE and a short README. The exe
# carries the default Windows icon for now; embedding assets/red.svg as the .exe
# icon (via the `winresource` crate in build.rs) and shipping a signed MSI
# installer are documented follow-ups — see docs/plans/future/cross-platform.md.
# Authenticode signing of the .exe is left to the release pipeline.
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

$Stage = Join-Path $Root "build\Red-windows"
Write-Host "> staging $Stage (v$Version)"
if (Test-Path $Stage) { Remove-Item -Recurse -Force $Stage }
New-Item -ItemType Directory -Force -Path $Stage | Out-Null
Copy-Item $Bin (Join-Path $Stage "Red.exe")
Copy-Item (Join-Path $Root "LICENSE") (Join-Path $Stage "LICENSE.txt") -ErrorAction SilentlyContinue
"Red $Version`r`nRoughly Enough Data — a fast database explorer.`r`nRun Red.exe to start." |
  Out-File -Encoding utf8 (Join-Path $Stage "README.txt")

$Out = Join-Path $Root "build\Red-$Version-x86_64-windows.zip"
Write-Host "> packaging $Out"
if (Test-Path $Out) { Remove-Item -Force $Out }
Compress-Archive -Path (Join-Path $Stage "*") -DestinationPath $Out
Write-Host "> done: $Out"
