# Post-upload hook: send the NZB (and optional NFO) to a Newznab-compatible indexer.
# For video files, captures 6 screenshots with ffmpeg, uploads them to ImgBB,
# and includes the URLs in the release submission.
#
# Install:
#   Copy-Item generic-indexer.ps1 "$env:APPDATA\pesto\hooks\"
#
# Any script placed in that folder runs automatically after every upload, so
# this is all you need to do — do NOT also add a `post_hook` entry pointing
# at this same file in config.toml, or it will run twice per upload (once
# from post_hook, once from the directory scan). Pick exactly one mechanism.
#
# Edit the variables below before use.
# Dependencies: curl (Windows 10 1803+), ffmpeg (only required for video files)

# ============================================================
#                       CONFIGURATION
# ============================================================

$ImgbbApiKey  = "YOUR_IMGBB_API_KEY"    # https://api.imgbb.com/

$IndexerApiUrl = "https://indexer.example.com/v1/releases"
$IndexerApiKey = "YOUR_API_KEY"

$CategoryId = 0   # Newznab category (0 = auto-detect). e.g. 5040 for TV/HD, 2040 for Movies/HD

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
# PESTO_GROUP        — Usenet group
# PESTO_PASSWORD     — yEnc password (if any)

$ErrorActionPreference = "Stop"

function Log   { param($msg) Write-Host "[Indexer] $msg" }
function Warn  { param($msg) Write-Host "[Indexer] WARNING: $msg" }

# Multipart POST helper. `Invoke-RestMethod -Form` only exists on PowerShell
# 6.0+ (https://learn.microsoft.com/en-us/powershell/module/microsoft.powershell.utility/invoke-restmethod)
# and pesto's hook runner falls back to Windows PowerShell 5.1 when `pwsh` is
# not installed (issue #41), so this uses System.Net.Http directly instead —
# available on both PS 5.1 (.NET Framework) and PS 7+ (.NET).
# $Fields values that are [System.IO.FileInfo] are sent as file parts; every
# other value is sent as a plain text field.
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
        $body = $resp.Content.ReadAsStringAsync().GetAwaiter().GetResult()
        if (-not $resp.IsSuccessStatusCode) {
            throw "HTTP $([int]$resp.StatusCode): $body"
        }
        return $body | ConvertFrom-Json
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

# ── detect video file ─────────────────────────────────────────────────────────

$videoExtensions = @('.mkv','.mp4','.avi','.mov','.m2ts','.ts','.wmv','.flv',
                     '.webm','.mpg','.mpeg','.m4v','.vob','.m2v','.mts')
$videoFile = $null

if ($env:PESTO_INPUT_PATHS) {
    foreach ($p in ($env:PESTO_INPUT_PATHS -split ':')) {
        if ($p -and (Test-Path $p) -and ($videoExtensions -contains [IO.Path]::GetExtension($p).ToLower())) {
            $videoFile = $p
            break
        }
    }
}

# ── take screenshots and upload to ImgBB ─────────────────────────────────────

$screenshotUrls = @()

if ($videoFile) {
    $ffmpeg  = Get-Command ffmpeg  -ErrorAction SilentlyContinue
    $ffprobe = Get-Command ffprobe -ErrorAction SilentlyContinue

    if (-not $ffmpeg -or -not $ffprobe) {
        Warn "ffmpeg/ffprobe not found in PATH - skipping screenshots."
    } elseif ($ImgbbApiKey -eq "YOUR_IMGBB_API_KEY") {
        Warn "ImgBB API key is not set - skipping screenshots."
    } else {
        Log "Video file detected: $(Split-Path $videoFile -Leaf)"
        Log "Capturing 6 screenshots..."

        # Get duration in seconds
        $durationStr = & ffprobe -v error -show_entries format=duration `
            -of default=noprint_wrappers=1:nokey=1 $videoFile 2>$null
        $duration = [int][double]$durationStr

        if ($duration -lt 30) {
            Warn "Could not determine video duration - skipping screenshots."
        } else {
            $tmpDir = Join-Path $env:TEMP ("pesto-shots-" + [IO.Path]::GetRandomFileName())
            New-Item -ItemType Directory -Path $tmpDir | Out-Null

            try {
                $offsets = @(10, 24, 38, 52, 66, 80)
                $shotIndex = 0

                foreach ($pct in $offsets) {
                    $seek     = [int]($duration * $pct / 100)
                    $shotFile = Join-Path $tmpDir "shot_${shotIndex}.jpg"

                    & ffmpeg -ss $seek -i $videoFile -vframes 1 -q:v 2 `
                        $shotFile -y -loglevel error 2>$null

                    if (Test-Path $shotFile) {
                        # Upload to ImgBB
                        try {
                            $response = Invoke-MultipartUpload -Uri "https://api.imgbb.com/1/upload" -Fields @{
                                key   = $ImgbbApiKey
                                image = Get-Item $shotFile
                            }

                            if ($response.data.url) {
                                $screenshotUrls += $response.data.url
                                Log ("Screenshot $($shotIndex + 1)/6 uploaded: " + $response.data.url)
                            } else {
                                Warn "ImgBB upload failed for shot ${shotIndex}: $($response | ConvertTo-Json -Compress)"
                            }
                        } catch {
                            Warn "ImgBB upload error for shot ${shotIndex}: $_"
                        }
                    } else {
                        Warn "ffmpeg failed at offset ${seek}s."
                    }

                    $shotIndex++
                }
            } finally {
                Remove-Item -Recurse -Force $tmpDir -ErrorAction SilentlyContinue
            }
        }
    }
}

# ── build indexer request ─────────────────────────────────────────────────────

Log "Sending: $(Split-Path $nzb -Leaf)"

$form = @{
    nzb_file    = Get-Item $nzb
    category_id = $CategoryId
}

if ($nfo -and (Test-Path $nfo)) {
    Log "With NFO: $(Split-Path $nfo -Leaf)"
    $form.nfo_file = Get-Item $nfo
}

if ($name) {
    $form.name = $name
}

if ($screenshotUrls.Count -gt 0) {
    $form.screenshot_urls = ($screenshotUrls | ConvertTo-Json -Compress)
    Log "Attaching $($screenshotUrls.Count) screenshot URL(s)."
}

# ── submit ────────────────────────────────────────────────────────────────────

try {
    $response = Invoke-MultipartUpload -Uri "${IndexerApiUrl}?apikey=${IndexerApiKey}" -Fields $form
} catch {
    Write-Error "[Indexer] FAILED: $_"
    exit 1
}

$releaseId = if ($response.public_id) { $response.public_id } `
             elseif ($response.id)     { $response.id }        `
             else                      { "?" }

if ($response.public_id -or $response.id) {
    Log "OK - release id: $releaseId"
} else {
    Write-Error "[Indexer] FAILED: $($response | ConvertTo-Json -Compress)"
    exit 1
}
