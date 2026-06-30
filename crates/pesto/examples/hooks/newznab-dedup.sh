#!/bin/bash
# Pre-upload hook: abort if a release with the same name already exists on a Newznab indexer.
#
# The check uses the Newznab search API (t=search) with the release name.
# Matching replicates the server-side normalization: lowercase, dots/dashes/underscores
# replaced with spaces. An upload is aborted only on a confirmed exact match.
# Network errors are non-fatal (fail-open) so a temporary outage never blocks all uploads.
#
# Install:
#   cp newznab-dedup.sh ~/.config/pesto/pre-hooks/
#   chmod +x ~/.config/pesto/pre-hooks/newznab-dedup.sh
#
# Dependencies: curl, sed, tr (all standard Unix tools)

# ╔══════════════════════════════════════════════════════════════╗
# ║                      CONFIGURATION                          ║
# ╚══════════════════════════════════════════════════════════════╝

INDEXER_API_URL="https://indexer.example.com/api"
INDEXER_API_KEY="YOUR_API_KEY"

# Maximum seconds to wait for the search response.
CURL_TIMEOUT=15

# ╔══════════════════════════════════════════════════════════════╗
# ║                   END OF CONFIGURATION                      ║
# ╚══════════════════════════════════════════════════════════════╝

set -uo pipefail

log()  { echo "[newznab-dedup] $*"; }
warn() { echo "[newznab-dedup] WARNING: $*" >&2; }

if [ -z "${PESTO_NAME:-}" ]; then
    warn "PESTO_NAME is empty — skipping duplicate check."
    exit 0
fi

if [ "$INDEXER_API_KEY" = "YOUR_API_KEY" ]; then
    warn "INDEXER_API_KEY is not configured — skipping duplicate check."
    exit 0
fi

# Replicate the server-side normalize_name():
#   lowercase, replace [.-_] with space, collapse multiple spaces.
normalize() {
    echo "$1" | tr '[:upper:]' '[:lower:]' | sed 's/[.\-_]/ /g; s/  */ /g; s/^ //; s/ $//'
}

NORMALIZED_NAME=$(normalize "$PESTO_NAME")

# URL-encode the normalized query (spaces → +).
ENCODED_QUERY=$(printf '%s' "$NORMALIZED_NAME" | sed 's/ /+/g')

log "Checking for existing release: $PESTO_NAME"

RESPONSE=$(curl -s --max-time "$CURL_TIMEOUT" \
    "${INDEXER_API_URL}?t=search&apikey=${INDEXER_API_KEY}&q=${ENCODED_QUERY}" 2>/dev/null) || true

if [ -z "$RESPONSE" ]; then
    warn "Search request failed or timed out — allowing upload."
    exit 0
fi

# Check for API-level error (<error code="..." description="..."/>).
if echo "$RESPONSE" | grep -qi '<error '; then
    ERROR_DESC=$(echo "$RESPONSE" | grep -oP '(?<=description=")[^"]*' 2>/dev/null | head -1 || true)
    warn "API returned an error: ${ERROR_DESC:-unknown} — allowing upload."
    exit 0
fi

# Extract all <title> values from <item> elements.
# Handles both plain <title>name</title> and <title><![CDATA[name]]></title>.
TITLES=$(echo "$RESPONSE" \
    | grep -oP '(?<=<title>)(\s*<!\[CDATA\[)?[^\]<][^\]<]*(?:\]\]>)?' \
    | sed 's/^[[:space:]]*<!\[CDATA\[//; s/\]\]>//' \
    || true)

if [ -z "$TITLES" ]; then
    log "No results found — proceeding with upload."
    exit 0
fi

# Compare each title (normalized) against the normalized release name.
while IFS= read -r title; do
    [ -z "$title" ] && continue
    NORMALIZED_TITLE=$(normalize "$title")
    if [ "$NORMALIZED_TITLE" = "$NORMALIZED_NAME" ]; then
        log "DUPLICATE DETECTED — release already exists on the indexer:"
        log "  Local : $PESTO_NAME"
        log "  Remote: $title"
        log "Upload aborted. Use --no-hooks to override if this is intentional."
        exit 1
    fi
done <<< "$TITLES"

ITEM_COUNT=$(echo "$RESPONSE" | grep -oc '<item>' || true)
log "No exact match found (${ITEM_COUNT:-0} partial result(s)) — proceeding with upload."
exit 0
