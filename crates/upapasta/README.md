# UpaPasta v2 (Rust)

This is the new pure-Rust implementation of UpaPasta, built as part of the `pesto` monorepo.

## Architecture

- **`pesto`** — Core library + lightweight CLI (`pesto`)
- **`upapasta`** — Full-featured application with TUI, catalog, watch mode, metadata enrichment, and intelligent orchestration
- **`parmesan`** — High-performance PAR2 library

`upapasta` uses the `pesto` library **directly** (no subprocess/JSON parsing), giving better performance, cleaner error handling, and real-time progress events.

## Current Status

Basic crate structure created on branch `upapasta-v2`.

Next milestones:
1. Rich TUI using `ratatui` (file browser, upload queue, history)
2. Direct integration with `pesto::post()`
3. Persistent catalog (replacing the old Python JSONL history)
4. Configuration system compatible with existing users
5. Watch mode with smart rules

This version aims to replace the Python implementation entirely while keeping the familiar `upapasta` UX.

## Configuration

UpaPasta shares configuration with `pesto` (`$XDG_CONFIG_HOME/pesto/config.toml`).

### Encryption

UpaPasta uses pesto's opt-in yEnc body and control-line encryption (experimental
Standards v1.2). Configure the `[encryption]` section in `config.toml`:

```toml
[encryption]
password = "<password>"
```

When configured, uploads are encrypted and the generated NZB carries
`<meta type="yenc_encrypted">true</meta>` with the `<meta type="password">`
transport password. See the root pesto README's Encryption section for the
wire format and status.

Heads up: encrypted uploads need a downloader that supports this encryption.
An ordinary downloader cannot decrypt them.
