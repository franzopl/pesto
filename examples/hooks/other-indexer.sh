#!/bin/bash
# Post-upload hook: send the NZB to an indexer via the legacy upload API.
#
# Install:
#   cp other-indexer.sh ~/.config/pesto/hooks/
#   chmod +x ~/.config/pesto/hooks/other-indexer.sh
#
# Edit the variables below before use.

# --- CONFIGURATION ---
UPLOAD_URL="https://indexer.example.com/api-upload"
USER="your-username"
API_KEY="your-api-key"
CATID="video"  # category ID; common values: tv, movie, video, xxx
               # or a numeric ID (e.g. 5040 = TV > HD, 2040 = Movies > HD)

# --- pesto variables ---
# PESTO_NZB  — path to the generated .nzb
# PESTO_NAME — release name

if [ -z "$PESTO_NZB" ] || [ ! -f "$PESTO_NZB" ]; then
    echo "[Indexer] Error: NZB not found (PESTO_NZB=$PESTO_NZB)."
    exit 1
fi

echo "[Indexer] Sending: $(basename "$PESTO_NZB")"

ARGS=(
    -k -s -L -m 60
    -F "catid=${CATID}"
    -F "nzb=@${PESTO_NZB}"
    -F "upload=upload"
)

if [ -n "$PESTO_NAME" ]; then
    ARGS+=(-F "rlsname=${PESTO_NAME}")
fi

RESPONSE=$(curl "${ARGS[@]}" "${UPLOAD_URL}?user=${USER}&api=${API_KEY}")

if echo "$RESPONSE" | grep -qi "<response>"; then
    echo "[Indexer] OK"
else
    echo "[Indexer] FAILED: $RESPONSE"
    exit 1
fi
