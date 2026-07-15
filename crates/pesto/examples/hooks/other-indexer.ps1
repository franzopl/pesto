# Post-upload hook: send the NZB to an indexer via the legacy upload API.
#
# Install:
#   Copy-Item other-indexer.ps1 "$env:APPDATA\pesto\hooks\"
#
# Any script placed in that folder runs automatically after every upload, so
# this is all you need to do — do NOT also add a `post_hook` entry pointing
# at this same file in config.toml, or it will run twice per upload (once
# from post_hook, once from the directory scan). Pick exactly one mechanism.
#
# Edit the variables below before use.
# Dependencies: none beyond PowerShell itself (uses System.Net.Http).

# ============================================================
#                       CONFIGURATION
# ============================================================

$UploadUrl = "https://indexer.example.com/api-upload"
$User      = "your-username"
$ApiKey    = "your-api-key"
$CatId     = "video"  # category ID; common values: tv, movie, video, xxx
                       # or a numeric ID (e.g. 5040 = TV > HD, 2040 = Movies > HD)
$Language  = ""        # leave empty to auto-detect from the NFO (mediainfo output),
                        # or set a fixed value (e.g. "Portuguese") to always send it

# ============================================================
#                   END OF CONFIGURATION
# ============================================================

# --- pesto variables available in this hook ---
# PESTO_NZB          — path to the generated .nzb
# PESTO_NFO          — path to the .nfo (empty when --nfo was not used)
# PESTO_NAME         — release name
# PESTO_INPUT_PATHS  — colon-separated list of uploaded file paths
# PESTO_BYTES        — total uploaded bytes
# PESTO_SERVER       — server hostname
# PESTO_GROUP        — first Usenet group
# PESTO_GROUPS       — colon-separated list of all Usenet groups
# PESTO_PASSWORD     — yEnc password (if any)
# PESTO_TAGS         — space-separated list of NZB tags (empty when none)

$ErrorActionPreference = "Stop"

function Log { param($msg) Write-Host "[Indexer] $msg" }

# Multipart POST helper returning the raw response body as text. This
# indexer's legacy upload API replies with a plain-text status line (not
# JSON), so — unlike generic-indexer.ps1's helper — this does not attempt
# ConvertFrom-Json on the result.
# Uses System.Net.Http directly instead of `Invoke-RestMethod -Form` (which
# needs PowerShell 6.0+) because pesto's hook runner falls back to Windows
# PowerShell 5.1 when `pwsh` is not installed (issue #41).
function Invoke-MultipartUpload {
    param([string]$Uri, [hashtable]$Fields)

    Add-Type -AssemblyName System.Net.Http
    $client  = New-Object System.Net.Http.HttpClient
    $content = New-Object System.Net.Http.MultipartFormDataContent
    try {
        foreach ($key in $Fields.Keys) {
            $value = $Fields[$key]
            if ($value -is [System.IO.FileInfo]) {
                $stream = [System.IO.File]::OpenRead($value.FullName)
                $part   = New-Object System.Net.Http.StreamContent($stream)
                $content.Add($part, $key, [System.IO.Path]::GetFileName($value.FullName))
            } else {
                $content.Add((New-Object System.Net.Http.StringContent([string]$value)), $key)
            }
        }
        $resp = $client.PostAsync($Uri, $content).GetAwaiter().GetResult()
        return $resp.Content.ReadAsStringAsync().GetAwaiter().GetResult()
    } finally {
        $content.Dispose()
        $client.Dispose()
    }
}

# ── sanity checks ─────────────────────────────────────────────────────────────

$nzb  = $env:PESTO_NZB
$nfo  = $env:PESTO_NFO
$name = $env:PESTO_NAME

if (-not $nzb -or -not (Test-Path $nzb)) {
    Write-Error "[Indexer] Error: NZB not found (PESTO_NZB=$nzb)."
    exit 1
}

Log "Sending: $(Split-Path $nzb -Leaf)"

$form = @{
    catid  = $CatId
    nzb    = Get-Item $nzb
    upload = "upload"
}

if ($name) {
    $form.rlsname = $name
}

if ($nfo -and (Test-Path $nfo)) {
    $form.nfo = Get-Item $nfo
    Log "With NFO: $(Split-Path $nfo -Leaf)"

    # Auto-detect language from the first Audio section in the mediainfo NFO.
    if (-not $Language) {
        $foundAudio = $false
        foreach ($line in (Get-Content $nfo)) {
            if ($line -match '^Audio') {
                $foundAudio = $true
            } elseif ($foundAudio -and $line -match 'Language') {
                $Language = ($line -replace '.*:\s*', '').Trim()
                break
            }
        }
    }
}

if ($Language) {
    $form.language = $Language
    Log "Language: $Language"
}

# ── submit ────────────────────────────────────────────────────────────────────

try {
    $response = Invoke-MultipartUpload -Uri "${UploadUrl}?user=${User}&api=${ApiKey}" -Fields $form
} catch {
    Write-Error "[Indexer] FAILED: $_"
    exit 1
}

if ($response -match '(?i)successfully') {
    Log "OK"
} else {
    Write-Error "[Indexer] FAILED: $response"
    exit 1
}
