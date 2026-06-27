#!/bin/bash
# Example hook — prints all available environment variables.
# Works as both a pre-upload hook and a post-upload hook.
#
# Install as post-upload hook (non-fatal):
#   cp print-vars.sh ~/.config/pesto/hooks/
#   chmod +x ~/.config/pesto/hooks/print-vars.sh
#
# Install as pre-upload hook (fatal — aborts on non-zero exit):
#   cp print-vars.sh ~/.config/pesto/pre-hooks/
#   chmod +x ~/.config/pesto/pre-hooks/print-vars.sh
#
# Pre-upload hooks run before compression, PAR2, and NNTP. They receive the
# same PESTO_* variables as post-upload hooks, except PESTO_NZB and PESTO_NFO
# which are always empty (the NZB has not been written yet).
# Scripts in pre-hooks/ are executed in alphabetical order; the first one that
# exits non-zero aborts the upload immediately.
#
# pesto sets the following variables before running every hook:
#   PESTO_NAME        — release name / entry label (derived from the input path)
#   PESTO_BYTES       — total bytes posted (decimal)
#   PESTO_INPUT_PATHS — colon-separated list of input paths that were posted
#   PESTO_SERVER      — NNTP server hostname
#   PESTO_GROUP       — first Usenet newsgroup
#   PESTO_GROUPS      — colon-separated list of all newsgroups
#   PESTO_PASSWORD    — archive password (empty when none)
#   PESTO_CATEGORY    — NZB category (empty when none)
#   PESTO_NZB_NAME    — value of --nzb-name, emitted as <meta type="name"> in the .nzb (empty when not set)
#   PESTO_OBFUSCATE   — obfuscation mode in use
#   PESTO_PAR2        — PAR2 redundancy percentage (e.g. 10)
#   PESTO_TAGS        — space-separated list of NZB tags (empty when none)
#
# Post-upload hooks additionally receive:
#   PESTO_NZB         — absolute path to the generated .nzb file (empty when not written)
#   PESTO_NFO         — absolute path to the .nfo file (empty when --nfo was not used)

echo "=== pesto post-upload hook ==="
echo "  PESTO_NAME        = $PESTO_NAME"
echo "  PESTO_BYTES       = $PESTO_BYTES"
echo "  PESTO_INPUT_PATHS = $PESTO_INPUT_PATHS"
echo "  PESTO_SERVER      = $PESTO_SERVER"
echo "  PESTO_GROUP       = $PESTO_GROUP"
echo "  PESTO_GROUPS      = $PESTO_GROUPS"
echo "  PESTO_PASSWORD    = $PESTO_PASSWORD"
echo "  PESTO_CATEGORY    = $PESTO_CATEGORY"
echo "  PESTO_NZB_NAME    = $PESTO_NZB_NAME"
echo "  PESTO_OBFUSCATE   = $PESTO_OBFUSCATE"
echo "  PESTO_PAR2        = $PESTO_PAR2"
echo "  PESTO_TAGS        = $PESTO_TAGS"
echo "  PESTO_NZB         = $PESTO_NZB"
echo "  PESTO_NFO         = $PESTO_NFO"
echo "=============================="
