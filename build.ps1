# Builds the Linux PTY helpers (inside WSL) and the .NET host app.
#   ./build.ps1                        # Release, self-contained single-file exe (no .NET install needed)
#   ./build.ps1 -SelfContained:$false  # quick framework-dependent build (faster; for dev/tests)
#   ./build.ps1 -Config Debug
param(
    [string]$Config = "Release",
    [string]$Distro = "Ubuntu",
    [bool]$SelfContained = $true
)
$ErrorActionPreference = "Stop"
$root = $PSScriptRoot
$proj = Join-Path $root "src\WslTerminal\WslTerminal.csproj"

Write-Host "==> Building Linux helpers (wslpty, wslptyd) inside WSL '$Distro'..." -ForegroundColor Cyan
$wslRoot = (wsl.exe -d $Distro -e wslpath -a "$root" | Out-String).Trim()
wsl.exe -d $Distro -e bash -lc "cd '$wslRoot/native' && sh build.sh"
if ($LASTEXITCODE -ne 0) { throw "native build failed" }

if ($SelfContained) {
    Write-Host "==> Publishing self-contained single-file exe (win-x64, $Config)..." -ForegroundColor Cyan
    # -r + --self-contained activate the single-file/compression props in the
    # csproj, so this bundles the .NET runtime + every managed/native .NET DLL
    # into one WslTerminal.exe (no .NET install needed to run it).
    dotnet publish $proj -c $Config -r win-x64 --self-contained --nologo
    if ($LASTEXITCODE -ne 0) { throw "dotnet publish failed" }

    $pub = Join-Path $root "src\WslTerminal\bin\$Config\net9.0-windows\win-x64\publish"
    $exe = Join-Path $pub "WslTerminal.exe"

    # The Linux helpers are ELF binaries that run *inside* WSL, so they can't live
    # inside the Windows exe — stage them next to it (the app resolves artifacts\
    # at runtime). The shippable bundle is these 3 files.
    $artDst = Join-Path $pub "artifacts"
    New-Item -ItemType Directory -Force -Path $artDst | Out-Null
    Copy-Item (Join-Path $root "artifacts\wslpty")  $artDst -Force
    Copy-Item (Join-Path $root "artifacts\wslptyd") $artDst -Force

    $mb = "{0:N0}" -f ((Get-Item $exe).Length / 1MB)
    Write-Host "==> Done. Self-contained bundle ($mb MB exe, no .NET install needed):" -ForegroundColor Green
    Write-Host "    $pub\"
    Write-Host "      WslTerminal.exe"
    Write-Host "      artifacts\wslpty"
    Write-Host "      artifacts\wslptyd"
}
else {
    Write-Host "==> Building .NET host (framework-dependent, $Config)..." -ForegroundColor Cyan
    dotnet build $proj -c $Config --nologo
    if ($LASTEXITCODE -ne 0) { throw "dotnet build failed" }
    $exe = Join-Path $root "src\WslTerminal\bin\$Config\net9.0-windows\WslTerminal.exe"
    Write-Host "==> Done (framework-dependent; needs the .NET 9 Desktop Runtime)." -ForegroundColor Green
}

Write-Host "    Launch GUI : $exe"
Write-Host "    PTY proof  : $exe --selftest"
Write-Host "    Emu tests  : $exe --vttest    Render test: $exe --rendertest"
