#!/usr/bin/env bash
# Post-upload hook: send the NZB (and optional NFO) to an indexer that takes
# a single multipart upload with the API key as a query parameter and replies
# with a JSON object containing a "guid" field.
#
# Install:
#   cp different-indexer.sh ~/.config/pesto/hooks/
#   chmod +x ~/.config/pesto/hooks/different-indexer.sh
#
# Any script placed in that folder runs automatically after every upload, so
# this is all you need to do — do NOT also add a `post_hook` entry pointing
# at this same file in config.toml, or it will run twice per upload (once
# from post_hook, once from the directory scan). Pick exactly one mechanism.
#
# Edit the variables below before use, or export INDEXER_API_KEY in your
# environment instead of hardcoding it here.

# ╔══════════════════════════════════════════════════════════════╗
# ║                      CONFIGURATION                          ║
# ╚══════════════════════════════════════════════════════════════╝

API_KEY="${INDEXER_API_KEY:-YOUR_API_KEY}"
API_URL="${INDEXER_UPLOAD_URL:-https://indexer.example.com/v1/upload}"
CATEGORY="${INDEXER_CATEGORY:-}"  # optional Newznab category override, e.g. 2040

# ╔══════════════════════════════════════════════════════════════╗
# ║                   END OF CONFIGURATION                      ║
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

set -u

log()  { echo "[Indexer] $*"; }
err()  { echo "[Indexer] ERROR: $*" >&2; }
die()  { err "$*"; exit 1; }

if [[ -z "${API_KEY:-}" || "$API_KEY" == "YOUR_API_KEY" ]]; then
  die "Set INDEXER_API_KEY (or edit API_KEY in this script)."
fi

[[ -n "${PESTO_NZB:-}" && -f "$PESTO_NZB" ]] || die "NZB not found (PESTO_NZB=${PESTO_NZB:-})."

url="$API_URL?apikey=$API_KEY"
if [[ -n "$CATEGORY" ]]; then
  url="$url&cat=$CATEGORY"
fi

ARGS=(-fsS -X POST "$url" -F "file=@${PESTO_NZB};type=application/x-nzb")

if [[ -n "${PESTO_NFO:-}" && -f "$PESTO_NFO" ]]; then
  ARGS+=(-F "nfo=@${PESTO_NFO};type=text/plain")
  log "With NFO: $(basename "$PESTO_NFO")"
fi

log "Sending: $(basename "$PESTO_NZB")"
RESPONSE="$(curl "${ARGS[@]}" 2>&1)"

if [[ $? -eq 0 ]]; then
  guid="$(printf '%s' "$RESPONSE" | sed -n 's/.*"guid"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p')"
  if [[ -n "$guid" ]]; then
    log "OK — release guid: $guid"
  else
    log "OK — $RESPONSE"
  fi
else
  err "Submission failed: $RESPONSE"
  exit 1
fi
