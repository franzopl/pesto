#!/bin/bash
# Post-upload hook: send the NZB (and optional NFO) to a Newznab-compatible indexer.
# For video files, captures 6 screenshots with ffmpeg, uploads them to ImgBB,
# and includes the URLs in the release submission.
#
# Install:
#   cp generic-indexer.sh ~/.config/pesto/hooks/
#   chmod +x ~/.config/pesto/hooks/generic-indexer.sh
#
# Any script placed in that folder runs automatically after every upload, so
# this is all you need to do — do NOT also add a `post_hook` entry pointing
# at this same file in config.toml, or it will run twice per upload (once
# from post_hook, once from the directory scan). Pick exactly one mechanism.
#
# Edit the variables below before use.
# Dependencies: curl, ffmpeg (only required for video files), jq

# ╔══════════════════════════════════════════════════════════════╗
# ║                      CONFIGURATION                           ║
# ╚══════════════════════════════════════════════════════════════╝

IMGBB_API_KEY="YOUR_IMGBB_API_KEY"   # https://api.imgbb.com/

INDEXER_API_URL="https://indexer.example.com/v1/releases"
INDEXER_API_KEY="YOUR_API_KEY"

CATEGORY_ID=0  # Newznab category (0 = auto-detect). e.g. 5040 for TV/HD, 2040 for Movies/HD

# ╔══════════════════════════════════════════════════════════════╗
# ║                   END OF CONFIGURATION                       ║
# ╚══════════════════════════════════════════════════════════════╝

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

set -euo pipefail

# ── helpers ──────────────────────────────────────────────────────────────────

log()  { echo "[Indexer] $*"; }
err()  { echo "[Indexer] ERROR: $*" >&2; }
die()  { err "$*"; exit 1; }

require_cmd() {
    command -v "$1" &>/dev/null || die "$1 is required but not installed."
}

# ── sanity checks ─────────────────────────────────────────────────────────────

[ -z "$PESTO_NZB" ] || [ ! -f "$PESTO_NZB" ] && die "NZB not found (PESTO_NZB=$PESTO_NZB)."
require_cmd curl

# ── detect video file ─────────────────────────────────────────────────────────

VIDEO_EXTENSIONS="mkv|mp4|avi|mov|m2ts|ts|wmv|flv|webm|mpg|mpeg|m4v|vob|m2v|mts"
VIDEO_FILE=""

if [ -n "${PESTO_INPUT_PATHS:-}" ]; then
    while IFS= read -r path; do
        if [[ "$path" =~ \.($VIDEO_EXTENSIONS)$ ]]; then
            VIDEO_FILE="$path"
            break
        fi
    done <<< "$PESTO_INPUT_PATHS"
fi

# ── take screenshots and upload to ImgBB ─────────────────────────────────────

SCREENSHOT_URLS=()

if [ -n "$VIDEO_FILE" ] && [ -f "$VIDEO_FILE" ]; then
    require_cmd ffmpeg
    require_cmd jq

    if [ "$IMGBB_API_KEY" = "YOUR_IMGBB_API_KEY" ]; then
        err "IMGBB_API_KEY is not set — skipping screenshots."
    else
        log "Video file detected: $(basename "$VIDEO_FILE")"
        log "Capturing 6 screenshots..."

        TMPDIR_SHOTS=$(mktemp -d)
        trap 'rm -rf "$TMPDIR_SHOTS"' EXIT

        # Get video duration in seconds
        DURATION=$(ffprobe -v error -show_entries format=duration \
            -of default=noprint_wrappers=1:nokey=1 "$VIDEO_FILE" 2>/dev/null || echo "0")
        DURATION=${DURATION%.*}   # truncate to integer

        if [ "${DURATION:-0}" -lt 30 ]; then
            err "Could not determine video duration — skipping screenshots."
        else
            # Evenly space 6 captures: at 10%, 24%, 38%, 52%, 66%, 80% of duration
            # (avoid first/last 10% which are usually intro/credits)
            OFFSETS=(10 24 38 52 66 80)
            SHOT_INDEX=0

            for PCT in "${OFFSETS[@]}"; do
                SEEK=$(( DURATION * PCT / 100 ))
                SHOT_FILE="${TMPDIR_SHOTS}/shot_${SHOT_INDEX}.jpg"

                if ffmpeg -ss "$SEEK" -i "$VIDEO_FILE" \
                        -vframes 1 -q:v 2 \
                        "$SHOT_FILE" -y -loglevel error 2>/dev/null; then

                    # Upload to ImgBB
                    IMGBB_RESPONSE=$(curl -s \
                        -F "key=${IMGBB_API_KEY}" \
                        -F "image=@${SHOT_FILE}" \
                        "https://api.imgbb.com/1/upload")

                    IMG_URL=$(echo "$IMGBB_RESPONSE" | jq -r '.data.url // empty' 2>/dev/null || true)

                    if [ -n "$IMG_URL" ]; then
                        SCREENSHOT_URLS+=("$IMG_URL")
                        log "Screenshot $((SHOT_INDEX + 1))/6 uploaded: $IMG_URL"
                    else
                        err "ImgBB upload failed for shot ${SHOT_INDEX}: $IMGBB_RESPONSE"
                    fi
                else
                    err "ffmpeg failed at offset ${SEEK}s."
                fi

                SHOT_INDEX=$(( SHOT_INDEX + 1 ))
            done
        fi
    fi
fi

# ── build indexer request ─────────────────────────────────────────────────────

log "Sending: $(basename "$PESTO_NZB")"

ARGS=(
    -s -X POST "${INDEXER_API_URL}?apikey=${INDEXER_API_KEY}"
    -F "nzb_file=@${PESTO_NZB}"
    -F "category_id=${CATEGORY_ID}"
)

if [ -n "${PESTO_NFO:-}" ] && [ -f "$PESTO_NFO" ]; then
    ARGS+=(-F "nfo_file=@${PESTO_NFO}")
    log "With NFO: $(basename "$PESTO_NFO")"
fi

if [ -n "${PESTO_NAME:-}" ]; then
    ARGS+=(-F "name=${PESTO_NAME}")
fi

# Attach screenshot URLs as a JSON array (max 6 enforced by the API)
if [ ${#SCREENSHOT_URLS[@]} -gt 0 ]; then
    # Build JSON array: ["url1","url2",...]
    SHOTS_JSON="["
    for i in "${!SCREENSHOT_URLS[@]}"; do
        [ $i -gt 0 ] && SHOTS_JSON+=","
        SHOTS_JSON+="\"${SCREENSHOT_URLS[$i]}\""
    done
    SHOTS_JSON+="]"

    ARGS+=(-F "screenshot_urls=${SHOTS_JSON}")
    log "Attaching ${#SCREENSHOT_URLS[@]} screenshot URL(s)."
fi

# ── submit ────────────────────────────────────────────────────────────────────

RESPONSE=$(curl "${ARGS[@]}")
HTTP_STATUS=$(echo "$RESPONSE" | jq -r '.status // empty' 2>/dev/null || true)

if echo "$RESPONSE" | grep -qE '"public_id"|"id"'; then
    PUB_ID=$(echo "$RESPONSE" | grep -oP '(?<="public_id":")[^"]*' \
             || echo "$RESPONSE" | grep -oP '(?<="id":)[0-9]+' || echo "?")
    log "OK — release id: $PUB_ID"
else
    err "Submission failed: $RESPONSE"
    exit 1
fi
