# Install gesta on Windows with one command.
#
#   irm https://raw.githubusercontent.com/zencli/gesta/main/install.ps1 | iex
#
# Written for PowerShell 5.1, which every Windows 10 and 11 machine has, so that the installer
# does not need the newer shell to be installed first.
$ErrorActionPreference = 'Stop'

$Repo    = 'zencli/gesta'
$Version = if ($env:GESTA_VERSION) { $env:GESTA_VERSION } else { 'main' }
$BinDir  = Join-Path $env:LOCALAPPDATA 'gesta\bin'
$Target  = Join-Path $BinDir 'gesta.exe'

function Write-Step($m) { Write-Host $m }

function Get-Platform {
    $arch = if ($env:PROCESSOR_ARCHITECTURE -eq 'ARM64') { 'aarch64' } else { 'x86_64' }
    "$arch-pc-windows-msvc"
}

function Install-Binary($src) {
    New-Item -ItemType Directory -Force -Path $BinDir | Out-Null
    $tmp = Join-Path $BinDir ".gesta.$PID.exe"
    Copy-Item $src $tmp -Force
    Move-Item $tmp $Target -Force
}

function Try-Prebuilt {
    if ($env:GESTA_FROM -eq 'source') { return $false }
    $plat = Get-Platform
    $url  = "https://github.com/$Repo/releases/download/$Version/gesta-$plat.exe"
    $tmp  = Join-Path $env:TEMP "gesta-$PID.exe"
    Write-Step "Trying a prebuilt binary for $plat"
    try {
        $old = $ProgressPreference
        $ProgressPreference = 'SilentlyContinue'
        Invoke-WebRequest -Uri $url -OutFile $tmp -UseBasicParsing -TimeoutSec 120
        $ProgressPreference = $old
        Install-Binary $tmp
        Remove-Item $tmp -Force -ErrorAction SilentlyContinue
        return $true
    } catch {
        Write-Step "No prebuilt binary at $Version for $plat; building from source"
        Remove-Item $tmp -Force -ErrorAction SilentlyContinue
        return $false
    }
}

function Build-From-Source {
    $cargo = Get-Command cargo -ErrorAction SilentlyContinue
    if (-not $cargo) {
        throw "Neither a prebuilt binary nor Rust is available. Install Rust from https://rustup.rs and run this again."
    }
    New-Item -ItemType Directory -Force -Path (Split-Path $BinDir) | Out-Null
    # `--branch main` fails against a remote with no such tracking ref, so the default branch is
    # requested by leaving the flag out and a version by --tag.
    $ref = if ($Version -eq 'main') { @() } else { @('--tag', $Version) }
    # --locked first, so a build gets the dependency versions the project was tested with.
    & cargo install --git "https://github.com/$Repo.git" @ref --root (Split-Path $BinDir) --locked
    if ($LASTEXITCODE -ne 0) {
        & cargo install --git "https://github.com/$Repo.git" @ref --root (Split-Path $BinDir)
    }
    if ($LASTEXITCODE -ne 0) { throw 'cargo install failed. Run: rustup update' }
}

$ok = Try-Prebuilt
if (-not $ok) { Build-From-Source }

if (-not (Test-Path $Target)) { throw "the binary did not land at $Target" }

# The user PATH needs no administrator rights, and an install that asks for elevation is an
# install people abandon.
$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
if (-not $userPath) { $userPath = '' }
if ($userPath -notlike "*$BinDir*") {
    $joined = if ($userPath) { "$userPath;$BinDir" } else { $BinDir }
    [Environment]::SetEnvironmentVariable('Path', $joined, 'User')
    Write-Step ''
    Write-Step "$BinDir was added to your user PATH. Open a new terminal for it to take effect."
}

Write-Step ''
Write-Step "gesta installed: $Target"
Write-Step ''
Write-Step 'Then in a project:'
Write-Step '  gesta init'
Write-Step '  gesta changeset create "first change" --author me'
Write-Step '  gesta status'
