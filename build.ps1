# Builds the Rust WSL Terminal:
#   1) the Linux PTY helpers (wslpty, wslptyd) inside WSL  -> artifacts/
#   2) the Rust GUI (wslterm.exe)                          -> rust/target/<config>/
#
#   ./build.ps1                 # release build
#   ./build.ps1 -Config debug   # debug build (console window, faster compile)
#   ./build.ps1 -Distro Ubuntu  # which WSL distro to compile the C helpers in
param(
    [ValidateSet("release", "debug")][string]$Config = "release",
    [string]$Distro = "Ubuntu"
)
$ErrorActionPreference = "Stop"
$root = $PSScriptRoot

# 1) Linux helpers (forkpty needs libutil; static so they run in any distro).
Write-Host "==> Building Linux helpers (wslpty, wslptyd) inside WSL '$Distro'..." -ForegroundColor Cyan
$wslRoot = (wsl.exe -d $Distro -e wslpath -a "$root" | Out-String).Trim()
wsl.exe -d $Distro -e bash -lc "cd '$wslRoot/native' && sh build.sh"
if ($LASTEXITCODE -ne 0) { throw "native build failed" }

# 2) Rust GUI.
Write-Host "==> Building the Rust GUI (wslterm, $Config)..." -ForegroundColor Cyan
Push-Location (Join-Path $root "rust")
try {
    if ($Config -eq "release") { cargo build --release -p wslterm } else { cargo build -p wslterm }
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }
}
finally { Pop-Location }

$exe = Join-Path $root "rust\target\$Config\wslterm.exe"
$mb = "{0:N1}" -f ((Get-Item $exe).Length / 1MB)
Write-Host "==> Done ($mb MB)." -ForegroundColor Green
Write-Host "    App     : $exe"
Write-Host "    Helpers : $root\artifacts\wslptyd , $root\artifacts\wslpty"
Write-Host ""
Write-Host "    Run wslterm.exe with the artifacts\ folder beside it (it stages"
Write-Host "    wslptyd into WSL at runtime). Running it from rust\target\$Config\"
Write-Host "    works as-is, since it finds artifacts\ by walking up to the repo root."
