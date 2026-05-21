#!/bin/bash
# Post-upload hook: send the NZB (and optional NFO) to a generic Newznab-compatible indexer.
#
# Install:
#   cp generic-indexer.sh ~/.config/pesto/hooks/
#   chmod +x ~/.config/pesto/hooks/generic-indexer.sh
#
# Edit the variables below before use.

# --- CONFIGURATION ---
API_URL="https://indexer.example.com/v1/releases"
API_KEY="your-api-key"
CATEGORY_ID=0  # 0 = auto-detect

# --- pesto variables ---
# PESTO_NZB  — path to the generated .nzb
# PESTO_NFO  — path to the .nfo (empty when --nfo was not used)
# PESTO_NAME — release name

if [ -z "$PESTO_NZB" ] || [ ! -f "$PESTO_NZB" ]; then
    echo "[Indexer] Error: NZB not found (PESTO_NZB=$PESTO_NZB)."
    exit 1
fi

echo "[Indexer] Sending: $(basename "$PESTO_NZB")"

ARGS=(
    -s -X POST "${API_URL}?apikey=${API_KEY}"
    -F "nzb_file=@${PESTO_NZB}"
    -F "category_id=${CATEGORY_ID}"
)

if [ -n "$PESTO_NFO" ] && [ -f "$PESTO_NFO" ]; then
    ARGS+=(-F "nfo_file=@${PESTO_NFO}")
    echo "[Indexer] With NFO: $(basename "$PESTO_NFO")"
fi

RESPONSE=$(curl "${ARGS[@]}")

if echo "$RESPONSE" | grep -q '"public_id"'; then
    PUB_ID=$(echo "$RESPONSE" | grep -oP '(?<="public_id":")[^"]*')
    echo "[Indexer] OK — public_id: $PUB_ID"
else
    echo "[Indexer] FAILED: $RESPONSE"
    exit 1
fi
