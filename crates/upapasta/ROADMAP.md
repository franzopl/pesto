# UpaPasta v2 — active roadmap

UpaPasta is the upload manager and catalog UI. It uploads through `pesto`,
does not download Usenet content, and keeps the TUI responsive during all
blocking work. Completed phases are preserved in
[`docs/roadmap-history/upapasta-ROADMAP-legacy.md`](../../docs/roadmap-history/upapasta-ROADMAP-legacy.md).

## Current status

The core browser, queue, catalog, history, upload configuration, progress
screen, NZB vault, Prowlarr search/configuration, watch mode and real pause
flow are implemented. The remaining work is parity, integration polish and
release readiness.

## Priority 1 — UX and workflow

- [ ] Add a first-run setup wizard.
- [ ] Support directory-level queue selection with clear release boundaries.
- [ ] Improve error handling and user-facing recovery guidance.
- [ ] Tune TUI performance during long uploads.
- [ ] Add configurable themes/colors.

## Priority 2 — Catalog and indexer integration

- [ ] Add NNTP article-availability checks for local or discovered NZBs.
- [ ] Add the optional automated Prowlarr NZB backup workflow, always requiring
      user confirmation before downloading an NZB.
- [ ] Add catalog tags, stronger search/filtering, bulk actions, export,
      duplicate detection and orphan detection.
- [ ] Add TMDb metadata enrichment and improve NFO generation.
- [ ] Decide how and when the combined season NZB is submitted to the indexer.

## Priority 3 — Upload integration

- [ ] Expose and verify multi-server posting, retry state and per-server health
      clearly in the TUI.
- [ ] Define the migration path from the legacy Python implementation, then
      retire or archive it.

## Non-goals

- Download Usenet article bodies or replace `penne`.
- Manage imports into external download clients.
- Index or transcode media.
- Require Prowlarr for normal uploads.

## Completion criteria

Every feature must keep keyboard-only operation, avoid blocking the render loop,
use `pesto`'s public library API, and include tests with mocked external
services. Before completion, run:

```bash
cargo fmt --check
cargo clippy -p upapasta --all-targets -- -D warnings
cargo check -p upapasta
cargo test -p upapasta
```

## References

- [`crates/upapasta/README.md`](README.md)
- [`crates/upapasta/Cargo.toml`](Cargo.toml)
- [`pesto` roadmap](../../ROADMAP.md)
