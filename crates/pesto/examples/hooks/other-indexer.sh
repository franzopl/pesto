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
LANGUAGE=""    # leave empty to auto-detect from the NFO (mediainfo output),
               # or set a fixed value (e.g. "Portuguese") to always send it

# --- pesto variables ---
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

if [ -n "$PESTO_NFO" ] && [ -f "$PESTO_NFO" ]; then
    ARGS+=(-F "nfo=@${PESTO_NFO}")
    echo "[Indexer] With NFO: $(basename "$PESTO_NFO")"

    # Auto-detect language from the first Audio section in the mediainfo NFO.
    if [ -z "$LANGUAGE" ]; then
        LANGUAGE=$(awk '/^Audio/{found=1} found && /Language/{gsub(/.*: */,""); print; exit}' "$PESTO_NFO")
    fi
fi

if [ -n "$LANGUAGE" ]; then
    ARGS+=(-F "language=${LANGUAGE}")
    echo "[Indexer] Language: ${LANGUAGE}"
fi

RESPONSE=$(curl "${ARGS[@]}" "${UPLOAD_URL}?user=${USER}&api=${API_KEY}")

if echo "$RESPONSE" | grep -qi "successfully"; then
    echo "[Indexer] OK"
else
    echo "[Indexer] FAILED"
    exit 1
fi
