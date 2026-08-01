#!/bin/bash
# Installs the pesto Usenet posting CLI on Linux.
#
# Downloads the latest pesto-v* release binary from GitHub, installs it to
# a per-user directory, adds that directory to PATH (by appending to your
# shell rc file if it's not there yet), and creates the hooks folder pesto
# scans on every upload (~/.config/pesto/hooks, or $XDG_CONFIG_HOME/pesto/hooks).
#
# Optionally fetches a ready-made post-upload hook script and/or a
# pre-filled config.toml from URLs you provide, so a distributor (e.g. an
# indexer that wants its users on pesto) can point people at a single
# command that leaves them fully configured, with no manual file editing.
#
# This script itself is indexer-agnostic: it has no knowledge of any
# specific indexer, URL, or API key. Pass --hook-url / --config-url to
# point it at files you host yourself.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/franzopl/pesto/main/scripts/install.sh | bash
#
#   # with a distributor-supplied hook:
#   curl -fsSL https://raw.githubusercontent.com/franzopl/pesto/main/scripts/install.sh | bash -s -- \
#       --hook-url "https://your-domain.example/pesto-hook.sh"
#
# Flags:
#   --hook-url <URL>      Download a ready-to-use post-upload hook script into the hooks folder.
#   --config-url <URL>    Download a pre-filled config.toml (never overwrites an existing one).
#   --install-dir <DIR>   Where to install the pesto binary (default: ~/.local/bin).
#   --musl                Force the musl build (default: auto-detected).
#   --no-path-update      Don't touch your shell rc file.
#   --no-config-wizard    Don't run `pesto --config` even if no config.toml exists yet.
#   --no-api-key-prompt   Don't prompt for an indexer API key, even if the downloaded hook needs one.

set -euo pipefail

REPO="franzopl/pesto"
INSTALL_DIR="${HOME}/.local/bin"
HOOK_URL=""
CONFIG_URL=""
FORCE_MUSL=""
NO_PATH_UPDATE=""
NO_CONFIG_WIZARD=""
NO_API_KEY_PROMPT=""

log()  { printf '[pesto-install] %s\n' "$1"; }
warn() { printf '[pesto-install] WARNING: %s\n' "$1" >&2; }
die()  { printf '[pesto-install] ERROR: %s\n' "$1" >&2; exit 1; }

while [ $# -gt 0 ]; do
    case "$1" in
        --hook-url) HOOK_URL="$2"; shift 2 ;;
        --config-url) CONFIG_URL="$2"; shift 2 ;;
        --install-dir) INSTALL_DIR="$2"; shift 2 ;;
        --musl) FORCE_MUSL=1; shift ;;
        --no-path-update) NO_PATH_UPDATE=1; shift ;;
        --no-config-wizard) NO_CONFIG_WIZARD=1; shift ;;
        --no-api-key-prompt) NO_API_KEY_PROMPT=1; shift ;;
        -h|--help) grep '^#' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) die "Unknown argument: $1" ;;
    esac
done

# `curl URL | bash` means bash reads this very script's source from stdin
# (fd 0) as it executes it. Any later command that reads stdin without an
# explicit redirect - `pesto --config`'s prompts included - inherits that
# same fd and can read leftover, not-yet-consumed bytes of this script's own
# source instead of the terminal (confirmed in practice: the config wizard's
# first prompt came back containing a line of this file's own code). Reattach
# stdin to the real terminal once, up front, so every interactive read below
# just works. Guarded: without a tty (e.g. a non-interactive CI run), this is
# a no-op and those reads degrade the same way they already did before.
if [ -r /dev/tty ]; then
    exec < /dev/tty
fi

command -v curl >/dev/null 2>&1 || die "curl is required but not found."

case "$(uname -s)" in
    Linux) ;;
    Darwin) die "No macOS build is published yet. Install with: cargo install pesto-poster" ;;
    *) die "Unsupported OS: $(uname -s). Install with: cargo install pesto-poster" ;;
esac

# ── pick the right asset (glibc vs musl) ────────────────────────────────────

ASSET_NAME="pesto-linux-x86_64"
if [ -n "$FORCE_MUSL" ]; then
    ASSET_NAME="pesto-linux-x86_64-musl"
elif [ -f /etc/alpine-release ] || (command -v ldd >/dev/null 2>&1 && ldd --version 2>&1 | grep -qi musl); then
    ASSET_NAME="pesto-linux-x86_64-musl"
fi

# ── locate the latest pesto-v* release ──────────────────────────────────────
# GitHub's /releases/latest is not safe here: parmesan-v* and penne-v*
# releases share this repo and are cut independently, so "latest" by date
# is not necessarily a pesto release. Walk the release list instead and
# take the newest tag matching pesto-v*.
log "Looking up the latest pesto release..."

RELEASES_JSON="$(curl -fsSL -H "User-Agent: pesto-install-script" "https://api.github.com/repos/${REPO}/releases")"

DOWNLOAD_URL="$(printf '%s' "$RELEASES_JSON" | grep -o "\"browser_download_url\": *\"[^\"]*${ASSET_NAME}\"" | head -n1 | sed -E 's/.*"(https:[^"]+)"/\1/')"
TAG_NAME="$(printf '%s' "$RELEASES_JSON" | grep -o '"tag_name": *"pesto-v[^"]*"' | head -n1 | sed -E 's/.*"(pesto-v[^"]+)"/\1/')"

[ -n "$DOWNLOAD_URL" ] || die "Could not find a pesto-v* release with asset ${ASSET_NAME}. Check https://github.com/${REPO}/releases manually."

log "Installing ${TAG_NAME} (${ASSET_NAME})..."

# ── download the binary ─────────────────────────────────────────────────────

mkdir -p "$INSTALL_DIR"
EXE_PATH="${INSTALL_DIR}/pesto"

curl -fsSL "$DOWNLOAD_URL" -o "$EXE_PATH"
chmod +x "$EXE_PATH"
log "pesto installed to ${EXE_PATH}"

# ── add to PATH ──────────────────────────────────────────────────────────────

if [ -z "$NO_PATH_UPDATE" ]; then
    case ":$PATH:" in
        *":${INSTALL_DIR}:"*) ;;
        *)
            RC_FILE="${HOME}/.profile"
            case "${SHELL:-}" in
                */zsh) RC_FILE="${HOME}/.zshrc" ;;
                */bash) RC_FILE="${HOME}/.bashrc" ;;
            esac

            EXPORT_LINE="export PATH=\"${INSTALL_DIR}:\$PATH\""
            if [ -f "$RC_FILE" ] && grep -qF "$INSTALL_DIR" "$RC_FILE" 2>/dev/null; then
                :
            else
                printf '\n# Added by the pesto installer\n%s\n' "$EXPORT_LINE" >> "$RC_FILE"
                log "Added ${INSTALL_DIR} to PATH in ${RC_FILE} (restart your shell, or run: export PATH=\"${INSTALL_DIR}:\$PATH\")"
            fi
            export PATH="${INSTALL_DIR}:${PATH}"
            ;;
    esac
fi

# ── hooks folder ─────────────────────────────────────────────────────────────

CONFIG_HOME="${XDG_CONFIG_HOME:-${HOME}/.config}"
HOOKS_DIR="${CONFIG_HOME}/pesto/hooks"
mkdir -p "$HOOKS_DIR"

if [ -n "$HOOK_URL" ]; then
    HOOK_NAME="$(basename "${HOOK_URL%%\?*}")"
    [ -n "$HOOK_NAME" ] || HOOK_NAME="hook.sh"
    HOOK_PATH="${HOOKS_DIR}/${HOOK_NAME}"
    curl -fsSL "$HOOK_URL" -o "$HOOK_PATH"
    chmod +x "$HOOK_PATH"
    log "Hook script installed to ${HOOK_PATH}"

    # Hooks that need a per-user credential use the literal placeholder
    # YOUR_API_KEY (see examples/hooks/generic-indexer.*) since a distributor
    # can bake in their own indexer URL but not a key that belongs to each
    # individual user. Prompt for it here so installs need no manual editing.
    # Read from /dev/tty, not stdin: this script is normally invoked as
    # `curl ... | bash`, so stdin is the script source, not the terminal.
    if [ -z "$NO_API_KEY_PROMPT" ] && grep -q "YOUR_API_KEY" "$HOOK_PATH" 2>/dev/null; then
        api_key=""
        if [ -r /dev/tty ]; then
            read -rp "[pesto-install] Enter your indexer API key (leave blank to fill in later): " api_key < /dev/tty || true
        fi
        if [ -n "$api_key" ]; then
            # Bash's ${var//pattern/replacement} treats & and \ in the
            # replacement specially (like sed) - escape them so an API key
            # containing either lands in the file byte-for-byte.
            api_key_escaped="${api_key//\\/\\\\}"
            api_key_escaped="${api_key_escaped//&/\\&}"
            hook_content="$(cat "$HOOK_PATH")"
            printf '%s\n' "${hook_content//YOUR_API_KEY/$api_key_escaped}" > "$HOOK_PATH"
            log "API key written to ${HOOK_PATH}"
        else
            warn "No API key entered - edit ${HOOK_PATH} manually before your first upload (replace YOUR_API_KEY)."
        fi
    fi
fi

# ── config.toml ──────────────────────────────────────────────────────────────

CONFIG_DIR="${CONFIG_HOME}/pesto"
CONFIG_PATH="${CONFIG_DIR}/config.toml"
mkdir -p "$CONFIG_DIR"

HAVE_CONFIG=""
[ -f "$CONFIG_PATH" ] && HAVE_CONFIG=1

if [ -n "$CONFIG_URL" ]; then
    if [ -n "$HAVE_CONFIG" ]; then
        warn "config.toml already exists at ${CONFIG_PATH} - not overwriting. Delete it first if you want the one from --config-url."
    else
        curl -fsSL "$CONFIG_URL" -o "$CONFIG_PATH"
        log "config.toml installed to ${CONFIG_PATH}"
        HAVE_CONFIG=1
    fi
fi

# ── verify ───────────────────────────────────────────────────────────────────

"$EXE_PATH" --version >/dev/null || die "pesto did not run correctly after install."

log "pesto is installed."

if [ -z "$HAVE_CONFIG" ] && [ -z "$NO_CONFIG_WIZARD" ] && [ -r /dev/tty ]; then
    log "Running the setup wizard to configure your Usenet server..."
    "$EXE_PATH" --config
elif [ -z "$HAVE_CONFIG" ]; then
    log "No config.toml yet - run 'pesto --config' to set up your Usenet server."
fi

log "Done. Open a new terminal window if 'pesto' is not immediately recognized."
