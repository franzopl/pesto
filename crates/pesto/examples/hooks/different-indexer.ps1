# Post-upload hook: send the NZB (and optional NFO) to an indexer that takes
# a single multipart upload with the API key as a query parameter and replies
# with a JSON object containing a "guid" field.
#
# Install:
#   Copy-Item different-indexer.ps1 "$env:APPDATA\pesto\hooks\"
#
# Any script placed in that folder runs automatically after every upload, so
# this is all you need to do — do NOT also add a `post_hook` entry pointing
# at this same file in config.toml, or it will run twice per upload (once
# from post_hook, once from the directory scan). Pick exactly one mechanism.
#
# Edit the variables below before use, or set $env:INDEXER_API_KEY instead of
# hardcoding it here.
# Dependencies: none beyond PowerShell itself (uses System.Net.Http).

# ============================================================
#                       CONFIGURATION
# ============================================================

$ApiKey    = if ($env:INDEXER_API_KEY) { $env:INDEXER_API_KEY } else { "YOUR_API_KEY" }
$UploadUrl = if ($env:INDEXER_UPLOAD_URL) { $env:INDEXER_UPLOAD_URL } else { "https://indexer.example.com/v1/upload" }
$Category  = $env:INDEXER_CATEGORY  # optional Newznab category override, e.g. 2040

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

# Multipart POST helper returning the raw response body as text.
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

if (-not $ApiKey -or $ApiKey -eq "YOUR_API_KEY") {
    Write-Error "[Indexer] Set INDEXER_API_KEY (or edit `$ApiKey` in this script)."
    exit 1
}

$nzb = $env:PESTO_NZB
$nfo = $env:PESTO_NFO

if (-not $nzb -or -not (Test-Path $nzb)) {
    Write-Error "[Indexer] Error: NZB not found (PESTO_NZB=$nzb)."
    exit 1
}

$url = "${UploadUrl}?apikey=${ApiKey}"
if ($Category) {
    $url = "${url}&cat=${Category}"
}

Log "Sending: $(Split-Path $nzb -Leaf)"

$form = @{
    file = Get-Item $nzb
}

if ($nfo -and (Test-Path $nfo)) {
    $form.nfo = Get-Item $nfo
    Log "With NFO: $(Split-Path $nfo -Leaf)"
}

# ── submit ────────────────────────────────────────────────────────────────────

try {
    $response = Invoke-MultipartUpload -Uri $url -Fields $form
} catch {
    Write-Error "[Indexer] Submission failed: $_"
    exit 1
}

if ($response -match '"guid"\s*:\s*"([^"]*)"') {
    Log "OK — release guid: $($Matches[1])"
} else {
    Log "OK — $response"
}
