# Pre-upload hook: abort if a release with the same name already exists on a Newznab indexer.
#
# The check uses the Newznab search API (t=search) with the release name.
# Matching replicates the server-side normalization: lowercase, dots/dashes/underscores
# replaced with spaces. An upload is aborted only on a confirmed exact match.
# Network errors are non-fatal (fail-open) so a temporary outage never blocks all uploads.
#
# Install:
#   Copy-Item newznab-dedup.ps1 "$env:APPDATA\pesto\pre-hooks\"
#
# For the config.toml / --pre-hook route instead, see the "Pre-upload hook"
# section of the pesto README.
#
# Dependencies: none beyond PowerShell itself (uses Invoke-RestMethod).

# ============================================================
#                       CONFIGURATION
# ============================================================

$IndexerApiUrl = "https://indexer.example.com/api"
$IndexerApiKey = "YOUR_API_KEY"

# Maximum seconds to wait for the search response.
$TimeoutSec = 15

# ============================================================
#                   END OF CONFIGURATION
# ============================================================

# --- pesto variables available in this hook ---
# PESTO_NAME — release name
# (PESTO_NZB / PESTO_NFO / PESTO_PASSWORD are NOT available in pre-hooks —
# nothing has been generated yet at this point in the upload.)

function Log  { param($msg) Write-Host "[newznab-dedup] $msg" }
function Warn { param($msg) Write-Host "[newznab-dedup] WARNING: $msg" }

# Replicate the server-side normalize_name():
#   lowercase, replace [.-_] with space, collapse multiple spaces.
function Normalize-Name {
    param([string]$Name)
    $lower = $Name.ToLowerInvariant() -replace '[.\-_]', ' '
    return ($lower -replace '\s+', ' ').Trim()
}

$name = $env:PESTO_NAME

if (-not $name) {
    Warn "PESTO_NAME is empty — skipping duplicate check."
    exit 0
}

if ($IndexerApiKey -eq "YOUR_API_KEY") {
    Warn "IndexerApiKey is not configured — skipping duplicate check."
    exit 0
}

$normalizedName = Normalize-Name $name
$query = [Uri]::EscapeDataString($normalizedName) -replace '%20', '+'
$url = "${IndexerApiUrl}?t=search&apikey=${IndexerApiKey}&q=${query}"

Log "Checking for existing release: $name"

try {
    $raw = Invoke-RestMethod -Uri $url -Method Get -TimeoutSec $TimeoutSec -ErrorAction Stop
} catch {
    Warn "Search request failed or timed out — allowing upload. ($_)"
    exit 0
}

# Invoke-RestMethod already returns an [xml] object when the response's
# Content-Type is XML; fall back to parsing manually otherwise (some
# Newznab indexers mislabel the content type).
if ($raw -is [string]) {
    try {
        [xml]$response = $raw
    } catch {
        Warn "Response is not valid XML — allowing upload."
        exit 0
    }
} else {
    $response = $raw
}

# Check for API-level error (<error code="..." description="..."/>).
if ($response.error) {
    $errorDesc = $response.error.description
    Warn "API returned an error: $(if ($errorDesc) { $errorDesc } else { 'unknown' }) — allowing upload."
    exit 0
}

$items = $response.rss.channel.item
if (-not $items) {
    Log "No results found — proceeding with upload."
    exit 0
}

# Compare each title (normalized) against the normalized release name.
foreach ($item in @($items)) {
    $title = $item.title
    if (-not $title) { continue }
    if ((Normalize-Name $title) -eq $normalizedName) {
        Log "DUPLICATE DETECTED — release already exists on the indexer:"
        Log "  Local : $name"
        Log "  Remote: $title"
        Log "Upload aborted. Remove this hook (or use --no-hooks) to override if this is intentional."
        exit 1
    }
}

Log "No exact match found ($(@($items).Count) partial result(s)) — proceeding with upload."
exit 0
