# Builds the Linux PTY helper (inside WSL) and the .NET host app.
#   ./build.ps1            # Release
#   ./build.ps1 -Config Debug
param(
    [string]$Config = "Release",
    [string]$Distro = "Ubuntu"
)
$ErrorActionPreference = "Stop"
$root = $PSScriptRoot

Write-Host "==> Building Linux helper (wslpty) inside WSL '$Distro'..." -ForegroundColor Cyan
$wslRoot = (wsl.exe -d $Distro -e wslpath -a "$root" | Out-String).Trim()
wsl.exe -d $Distro -e bash -lc "cd '$wslRoot/native' && sh build.sh"
if ($LASTEXITCODE -ne 0) { throw "native build failed" }

Write-Host "==> Building .NET host (WslTerminal, $Config)..." -ForegroundColor Cyan
dotnet build "$root\src\WslTerminal\WslTerminal.csproj" -c $Config --nologo
if ($LASTEXITCODE -ne 0) { throw "dotnet build failed" }

$exe = Join-Path $root "src\WslTerminal\bin\$Config\net9.0-windows\WslTerminal.exe"
Write-Host "==> Done." -ForegroundColor Green
Write-Host "    Launch GUI : $exe"
Write-Host "    PTY proof  : $exe --selftest"
Write-Host "    Emu tests  : $exe --vttest    Render test: $exe --rendertest"
