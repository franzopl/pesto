#!/bin/bash
# Example pesto post-upload hook — prints all available environment variables.
#
# Install:
#   cp print-vars.sh ~/.config/pesto/hooks/
#   chmod +x ~/.config/pesto/hooks/print-vars.sh
#
# pesto sets the following variables before running every hook:
#   PESTO_NZB       — absolute path to the generated .nzb file
#   PESTO_NFO       — absolute path to the .nfo file (empty when --nfo was not used)
#   PESTO_NAME      — release name / entry label
#   PESTO_BYTES     — total bytes posted (decimal)
#   PESTO_GROUP     — first Usenet newsgroup
#   PESTO_GROUPS    — all Usenet newsgroup
#   PESTO_PASSWORD  — archive password (empty when none)
#   PESTO_SERVER    — NNTP server hostname
#   PESTO_CATEGORY  — value of `--nzb-category`
#   PESTO_NZB_NAME  — value of `--nzb-name` (<meta type="name">)
#   PESTO_TAGS      — space-separated list of NZB tags set via `--nzb-tag`
#   PESTO_OBFUSCATE — obfuscation mode in use (`none`, `full`, or `paranoid`)
#   PESTO_PAR2      — AR2 redundancy percentage (e.g. `10`)

echo "=== pesto post-upload hook ==="
echo "  PESTO_NAME      = $PESTO_NAME"
echo "  PESTO_NZB       = $PESTO_NZB"
echo "  PESTO_NFO       = $PESTO_NFO"
echo "  PESTO_BYTES     = $PESTO_BYTES"
echo "  PESTO_GROUP     = $PESTO_GROUP"
echo "  PESTO_PASSWORD  = $PESTO_PASSWORD"
echo "  PESTO_SERVER    = $PESTO_SERVER"
echo "  PESTO_CATEGORY  = $PESTO_CATEGORY"
echo "  PESTO_NZB_NAME  = $PESTO_NZB_NAME"
echo "  PESTO_TAGS      = $PESTO_TAGS"
echo "  PESTO_OBFUSCATE = $PESTO_OBFUSCATE"
echo "  PESTO_PAR2      = $PESTO_PAR2"
echo "=============================="
