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
