<#
.SYNOPSIS
    Installs the pesto Usenet posting CLI on Windows.

.DESCRIPTION
    Downloads the latest pesto-v* release binary from GitHub, installs it to
    a per-user directory, adds that directory to the user PATH, and creates
    the hooks folder pesto scans on every upload (%APPDATA%\pesto\hooks).

    Optionally fetches a ready-made post-upload hook script and/or a
    pre-filled config.toml from URLs you provide, so a distributor (e.g. an
    indexer that wants its users on pesto) can point people at a single
    command that leaves them fully configured, with no manual file editing.

    This script itself is indexer-agnostic: it has no knowledge of any
    specific indexer, URL, or API key. Pass -HookUrl / -ConfigUrl to point
    it at files you host yourself.

.PARAMETER HookUrl
    URL of a ready-to-use post-upload hook script (.ps1/.bat/.cmd/.exe/.py).
    Downloaded into the pesto hooks folder under its original filename.

.PARAMETER ConfigUrl
    URL of a pre-filled config.toml. Only written when no config.toml exists
    yet at the destination — an existing config is never overwritten.

.PARAMETER InstallDir
    Directory pesto.exe is installed into. Defaults to
    "$env:LOCALAPPDATA\pesto\bin".

.PARAMETER NoPathUpdate
    Skip adding InstallDir to the user PATH.

.PARAMETER NoConfigWizard
    Skip running `pesto --config` at the end when no config.toml exists yet.

.PARAMETER NoApiKeyPrompt
    Skip the interactive prompt for an indexer API key, even if the
    downloaded hook (-HookUrl) contains the YOUR_API_KEY placeholder.

.EXAMPLE
    Plain install, run interactively:
        irm https://raw.githubusercontent.com/franzopl/pesto/main/scripts/install.ps1 | iex

.EXAMPLE
    Install with a pre-filled hook and config, for distributors piping in
    parameters (a plain `... | iex` cannot take parameters):
        & ([scriptblock]::Create((irm https://raw.githubusercontent.com/franzopl/pesto/main/scripts/install.ps1))) -HookUrl "https://your-domain.example/pesto-hook.ps1" -ConfigUrl "https://your-domain.example/pesto-config.toml"
#>

[CmdletBinding()]
param(
    [string]$HookUrl,
    [string]$ConfigUrl,
    [string]$InstallDir = (Join-Path $env:LOCALAPPDATA "pesto\bin"),
    [switch]$NoPathUpdate,
    [switch]$NoConfigWizard,
    [switch]$NoApiKeyPrompt
)

$ErrorActionPreference = "Stop"
$Repo = "franzopl/pesto"

function Log  { param($msg) Write-Host "[pesto-install] $msg" }
function Warn { param($msg) Write-Host "[pesto-install] WARNING: $msg" -ForegroundColor Yellow }

# Windows PowerShell 5.1 (the default on Windows 10/11) only speaks TLS 1.0
# by default on some configurations; force 1.2 or GitHub's API/CDN will
# refuse the connection.
[Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

# ── locate the latest pesto-v* release ──────────────────────────────────────
# GitHub's /releases/latest is not safe here: parmesan-v* and penne-v*
# releases share this repo and are cut independently, so "latest" by date
# is not necessarily a pesto release. Walk the release list instead and
# take the newest tag matching pesto-v*.
Log "Looking up the latest pesto release..."
$releases = Invoke-RestMethod -Uri "https://api.github.com/repos/$Repo/releases" -Headers @{ "User-Agent" = "pesto-install-script" }
$release = $releases | Where-Object { $_.tag_name -like "pesto-v*" } | Select-Object -First 1

if (-not $release) {
    throw "Could not find a pesto-v* release on GitHub. Check https://github.com/$Repo/releases manually."
}

$asset = $release.assets | Where-Object { $_.name -eq "pesto-windows-x86_64.exe" }
if (-not $asset) {
    throw "Release $($release.tag_name) has no pesto-windows-x86_64.exe asset."
}

Log "Installing $($release.tag_name)..."

# ── download the binary ─────────────────────────────────────────────────────

New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
$exePath = Join-Path $InstallDir "pesto.exe"

Invoke-WebRequest -Uri $asset.browser_download_url -OutFile $exePath
Log "pesto.exe installed to $exePath"

# ── add to PATH (per-user, persisted) ───────────────────────────────────────

if (-not $NoPathUpdate) {
    $userPath = [Environment]::GetEnvironmentVariable("Path", "User")
    $pathEntries = @()
    if ($userPath) { $pathEntries = $userPath -split ";" }

    if ($pathEntries -notcontains $InstallDir) {
        $newPath = if ($userPath) { "$userPath;$InstallDir" } else { $InstallDir }
        [Environment]::SetEnvironmentVariable("Path", $newPath, "User")
        Log "Added $InstallDir to your user PATH (open a new terminal to pick it up)."
    }

    if (($env:Path -split ";") -notcontains $InstallDir) {
        $env:Path = "$env:Path;$InstallDir"
    }
}

# ── hooks folder ─────────────────────────────────────────────────────────────

$hooksDir = Join-Path $env:APPDATA "pesto\hooks"
New-Item -ItemType Directory -Force -Path $hooksDir | Out-Null

if ($HookUrl) {
    $hookName = [IO.Path]::GetFileName(([Uri]$HookUrl).LocalPath)
    if (-not $hookName) { $hookName = "hook.ps1" }
    $hookPath = Join-Path $hooksDir $hookName
    Invoke-WebRequest -Uri $HookUrl -OutFile $hookPath
    Log "Hook script installed to $hookPath"

    $recognized = @(".exe", ".cmd", ".bat", ".ps1", ".py")
    if ($recognized -notcontains [IO.Path]::GetExtension($hookName).ToLower()) {
        Warn "$hookName does not have a recognized extension (.exe/.cmd/.bat/.ps1/.py) - pesto will not run it automatically."
    }

    # Hooks that need a per-user credential use the literal placeholder
    # YOUR_API_KEY (see examples/hooks/generic-indexer.*) since a distributor
    # can bake in their own indexer URL but not a key that belongs to each
    # individual user. Prompt for it here so installs need no manual editing.
    if (-not $NoApiKeyPrompt) {
        $hookContent = Get-Content -Raw -Path $hookPath
        if ($hookContent -match "YOUR_API_KEY") {
            $apiKey = Read-Host "Enter your indexer API key (leave blank to fill in later)"
            if ($apiKey) {
                $hookContent = $hookContent.Replace("YOUR_API_KEY", $apiKey)
                Set-Content -Path $hookPath -Value $hookContent
                Log "API key written to $hookPath"
            } else {
                Warn "No API key entered - edit $hookPath manually before your first upload (replace YOUR_API_KEY)."
            }
        }
    }
}

# ── config.toml ──────────────────────────────────────────────────────────────

$configDir = Join-Path $env:APPDATA "pesto"
$configPath = Join-Path $configDir "config.toml"
New-Item -ItemType Directory -Force -Path $configDir | Out-Null

$haveConfig = Test-Path $configPath

if ($ConfigUrl) {
    if ($haveConfig) {
        Warn "config.toml already exists at $configPath - not overwriting. Delete it first if you want the one from -ConfigUrl."
    } else {
        Invoke-WebRequest -Uri $ConfigUrl -OutFile $configPath
        Log "config.toml installed to $configPath"
        $haveConfig = $true
    }
}

# ── verify ───────────────────────────────────────────────────────────────────

try {
    & $exePath --version | Out-Null
} catch {
    throw "pesto.exe did not run correctly after install: $_"
}

Log "pesto is installed."

if (-not $haveConfig -and -not $NoConfigWizard) {
    Log "Running the setup wizard to configure your Usenet server..."
    & $exePath --config
} elseif (-not $haveConfig) {
    Log "No config.toml yet - run 'pesto --config' to set up your Usenet server."
}

Log "Done. Open a new terminal window if 'pesto' is not immediately recognized."
