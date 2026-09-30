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

To enable yEnc body and control-line encryption for posting tasks dispatched by UpaPasta, configure the `[encryption]` section in `config.toml`:

```toml
[encryption]
password = "MySecretPassword"
```

When configured, uploads automatically encrypt bodies with XChaCha20-Poly1305, encrypt control lines with Radix 253 FF1, and embed `<meta type="yenc_encrypted">true</meta>` with `<meta type="password">` in generated NZBs, compatible with Penne and Sugo (support in SABnzbd and NZBGet is in development).

> **Threat Model & Confidentiality:**
> Content encryption at the article layer (XChaCha20-Poly1305) protects Usenet articles from parties lacking the NZB and password. However, because the encryption password is conventionally embedded in `<meta type="password">` within the generated NZB, confidentiality depends strictly on private distribution of the `.nzb` file.

> **Note on Season Consolidation:**
> Combined season packs are currently unsupported for encrypted uploads. Because each episode upload generates an independent random session salt and separate segment index space, consolidating multiple episodes into a single NZB with duplicate segment indices violates the specification. Preflight validation rejects encrypted season uploads before any episode transfer begins. When encryption is enabled in season mode, individual per-episode NZBs are created.
