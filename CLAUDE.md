# CLAUDE.md

**This is the canonical guide for all agents working in this monorepo.**

> The official language of this project is **English**. All code, comments,
> documentation, commit messages and identifiers must be written in English.

## Project Overview (2026)

This repository is now a **Cargo Workspace** containing four main crates:

- **`pesto`** — Core library + lightweight high-performance CLI (`pesto`); posting only
- **`upapasta`** — Full-featured Rust application with rich TUI, catalog, watch mode, metadata, and orchestration (replaces the old Python version)
- **`parmesan`** — High-performance PAR2 library (create, verify, repair)
- **`penne`** — NZB downloader CLI/engine: reads a `.nzb`, fetches articles over NNTP, reassembles files, verifies/repairs with PAR2 (see `crates/penne/ROADMAP.md`, early skeleton stage)

`upapasta` uses the `pesto` library **directly** via Rust API (no subprocess calls or JSON parsing).
`penne` likewise reuses `pesto` as a library (`.nzb` parsing, NNTP connection/auth) and `parmesan`
(via `pesto::par2`) rather than duplicating that logic.

### Current Focus (Phase 40b+)

We are now developing **UpaPasta v2 in Rust**. The Python version in `/home/francisco/dev/franzopl/upapasta` is considered legacy and will be retired once feature parity is reached.
`penne` (branch `penne-dev`) is a parallel, independent effort — see its own `ROADMAP.md` for phasing.

## Architecture Principles

- **`pesto`** remains minimal, extremely fast, and focused on the hot path (yEnc → article → NNTP pipeline). Posting only — it never downloads.
- **`upapasta`** is responsible for UX, business logic, catalog, watch mode, metadata enrichment (TMDb, NFO), and orchestration. It uploads and catalogues; it does not download Usenet content.
- **`penne`** owns everything download-side: NZB parsing for retrieval, article fetch, yEnc decode, file assembly, PAR2 verify/repair, archive extraction. A web UI (SABnzbd-like) on top of `penne` is planned but deferred until the CLI/engine reach feature parity with a real downloader.
- All shared types, config, and progress events live in `pesto` (as public API); `penne` reuses them instead of redefining its own NZB/NNTP primitives where the semantics are identical.
- Prefer **direct library integration** over CLI spawning in `upapasta` and `penne`.
- TUI must be responsive, keyboard-driven, and pleasant to use daily.

## Design Principles (Updated)

- **Performance first** in `pesto`, **excellent UX** in `upapasta`.
- Keep `pesto` CLI minimal. Complex features belong in `upapasta`.
- Use `ratatui` + `crossterm` for the TUI.
- All new code in `upapasta` must be async-friendly and integrate cleanly with `pesto::post()`.
- Maintain compatibility with existing user configuration where possible.

## Current Directory Structure

```
crates/
├── parmesan/          # PAR2 engine
├── penne/             # NZB downloader CLI/engine (crates/penne/src/bin/penne.rs) — early skeleton
├── pesto/             # Core library + CLI binary (crates/pesto/src/bin/pesto.rs)
└── upapasta/          # Main TUI application (our current focus)
```

## Development Workflow (UpaPasta v2)

When working on `upapasta`:

1. Always check neighboring files for style, component patterns, and naming.
2. Use existing `pesto` public API instead of reimplementing upload logic.
3. Prefer `ratatui` widgets and state management patterns already established.
4. Run `cargo check -p upapasta` and `cargo clippy -p upapasta` frequently.
5. Keep the TUI responsive even during long uploads (use channels for progress).

**Pre-commit checklist for upapasta:**

```bash
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo check -p upapasta
cargo test -p upapasta
```

---

## Legacy Notes

The old Python implementation (`/home/francisco/dev/franzopl/upapasta`) should only be referenced for understanding desired UX and feature list. Do not port code directly — reimplement idiomatically in Rust.

## Design principles

- **Speed first.** The hot path is: read file → yEnc → send over the NNTP
  connection. Avoid needless allocations, buffer copies and lock contention on
  that path. Prefer streaming I/O over loading whole files into memory.
- **Connection-pool concurrency.** Throughput comes from N parallel NNTP
  connections posting articles simultaneously. That is the core architecture,
  not an add-on.
- **Minimal scope.** Before adding a flag or option, ask whether it is
  fundamental to posting files. If not, it waits until after the MVP.
- **Fail clearly.** Network, authentication and I/O errors must produce
  actionable messages, not panics.

## Current Focus (Phase 40b)

We are implementing a clean, responsive TUI in `upapasta` using `ratatui`.

**Preferred patterns in upapasta:**
- Use `App` struct with `State` for screens (Dashboard, FileBrowser, History, etc.)
- Prefer event-driven architecture with `crossterm` event stream
- Use `tokio::sync::mpsc` to receive progress events from `pesto::post()`
- Keep components small and composable (see `ratatui` examples)
- All business logic should live in services, not in UI widgets

## Stack (UpaPasta v2)

**pesto:**
- `tokio`, `rustls` + `tokio-rustls`, `clap`, `serde` + `toml`, `tracing`

**upapasta:**
- `ratatui` + `crossterm` (TUI)
- `pesto` (as library)
- `directories`, `chrono`, `serde_json`, `tokio-util`, `sled` or `rusqlite` (catalog)

Keep dependency tree small. New crates in `upapasta` must be justified by UX or orchestration value.

## Configuration

Server and credentials come from a TOML file (see `config.example.toml`). Any
field can be overridden by a CLI flag. Credentials must never be logged.

## Commands

```bash
cargo build --release      # optimized binary at target/release/pesto
cargo run -- <args>        # debug run
cargo test                 # tests
cargo clippy -- -D warnings
cargo fmt
```

> Note: the Rust toolchain (`cargo`/`rustc`) is not yet installed in this
> environment. Install it via <https://rustup.rs> before building.

## Testing

**Never execute hooks during tests.** Tests must not trigger indexers, external
services, or any side-effecting integration. Use mocks or stubs for all
external dependencies (HTTP clients, NNTP connections, file watchers, indexers).
A test that sends real data to an external system is a bug, not a test.

## Pre-commit checklist

**Run all three checks locally and confirm they pass before every `git commit`
and `git push`.** The CI gate enforces the same checks; a push that breaks CI
is a wasted round-trip.

```bash
cargo fmt --check          # must produce no output
cargo clippy --all-targets -- -D warnings   # must exit 0
cargo test                 # all tests must pass
```

Never skip or work around these steps (e.g. `--no-verify`). If a check fails,
fix the root cause before committing.

## Conventions

- Formatting: `cargo fmt` (defaults).
- Lints: code must pass `cargo clippy --all-targets -D warnings`.
- Commits: short imperative messages. Group by roadmap phase.
- yEnc and NNTP have specifications; when changing them, cite the relevant
  part of the spec in a comment instead of "tweaking until it works".
