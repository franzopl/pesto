# Post-upload hook: send the NZB (and optional NFO) to a generic Newznab-compatible indexer.
#
# Install:
#   Copy-Item generic-indexer.ps1 "$env:APPDATA\pesto\hooks\"
#
# In config.toml set:
#   post_hook = "powershell -ExecutionPolicy Bypass -File \"%APPDATA%\\pesto\\hooks\\generic-indexer.ps1\""
#
# Edit the variables below before use.

# --- CONFIGURATION ---
$ApiUrl     = "https://indexer.example.com/v1/releases"
$ApiKey     = "your-api-key"
$CategoryId = 0

# --- pesto variables ---
# PESTO_NZB  — path to the generated .nzb
# PESTO_NFO  — path to the .nfo (empty when --nfo was not used)
# PESTO_NAME — release name

$nzb = $env:PESTO_NZB
$nfo = $env:PESTO_NFO

if (-not $nzb -or -not (Test-Path $nzb)) {
    Write-Error "[Indexer] Error: NZB not found (PESTO_NZB=$nzb)."
    exit 1
}

Write-Host "[Indexer] Sending: $(Split-Path $nzb -Leaf)"

$form = @{
    nzb_file    = Get-Item $nzb
    category_id = $CategoryId
}

if ($nfo -and (Test-Path $nfo)) {
    Write-Host "[Indexer] With NFO: $(Split-Path $nfo -Leaf)"
    $form.nfo_file = Get-Item $nfo
}

try {
    $response = Invoke-RestMethod -Method Post `
        -Uri "${ApiUrl}?apikey=${ApiKey}" `
        -Form $form
} catch {
    Write-Error "[Indexer] FAILED: $_"
    exit 1
}

if ($response.public_id) {
    Write-Host "[Indexer] OK — public_id: $($response.public_id)"
} else {
    Write-Error "[Indexer] FAILED: $($response | ConvertTo-Json -Compress)"
    exit 1
}
