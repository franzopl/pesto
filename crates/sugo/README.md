# sugo

**SABnzbd-API-compatible web UI for [`penne`](../penne).**

A lightweight, single-binary web front end for `penne`'s download engine —
server-rendered (`askama` + htmx, no JS build step), consuming `penne`
directly as a Rust library (no subprocess/JSON spawning). Exposes a subset
of SABnzbd's real `/api` wire format so existing tools (Sonarr, Radarr,
Prowlarr, SAB-aware mobile apps) can point at it without any new
integration work.

> **Status:** first vertical slice — config, the background job engine, the
> core SABnzbd `mode`s (`version`, `addfile`, `addurl`, `queue`, `history`,
> `fullstatus`, `get_config`), and the htmx dashboard/history/settings pages
> are implemented and tested end-to-end against a fake NNTP server. PAR2
> verify/extraction progress isn't streamed live yet (only the status
> transition is); per-category routing, priorities, and `set_config` are
> deferred. See `crates/sugo`'s own history for what's next.

## Quick start

```bash
# Write a config with at least one [[servers]] entry and a [web].api_key
# (see Configuration below), then:
cargo run -p sugo

# Or point at a specific file / bind address:
cargo run -p sugo -- --config path/to/config.toml --bind 0.0.0.0:8085
```

Open `http://<bind_addr>/` to upload an `.nzb` and watch it download, or
point a SABnzbd-compatible client at the same address with the configured
API key.

## Configuration

`sugo` reads the *same* TOML shape as `penne` itself — `[[servers]]`,
`download_dir`, `connections`, `retries`, `mode` — plus its own `[web]`
table. An existing `penne` `config.toml` works unchanged; just add:

```toml
[web]
bind_addr = "127.0.0.1:8085"   # default shown
api_key = "choose-a-long-random-string"
# data_dir = "/path/to/state"  # job queue/history snapshot + staged .nzb files;
                                # defaults to the XDG data dir
```

`api_key` is required — every `/api` call and the UI itself reject requests
without a matching one (unconfigured means "reject everything", never "open
access").

### Default config path

| OS | Path |
|----|------|
| Linux/macOS | `$XDG_CONFIG_HOME/sugo/config.toml`, falling back to `~/.config/sugo/config.toml` |
| Windows | `%APPDATA%\sugo\config.toml` |

## Supported SABnzbd `/api` modes

| Mode | Notes |
|------|-------|
| `version` | |
| `addfile` | Multipart `.nzb` upload (POST). |
| `addurl` | Fetches the `.nzb` from a URL (via `reqwest`). |
| `queue` | List, plus `name=delete\|pause\|resume`. |
| `history` | List, plus `name=delete`. |
| `fullstatus` | Minimal status summary. |
| `get_config` | Stub — enough for `*arr`'s "Test" handshake to pass; real per-category config isn't implemented yet. |

Every call needs `?apikey=<key>` matching `[web].api_key`.

## Architecture

- `src/job/` — the job model, in-memory queue/history (JSON-snapshotted to
  `<data_dir>/state.json` so a restart doesn't lose the queue), and
  `pipeline.rs`, a per-job port of `penne`'s own CLI pipeline
  (`crates/penne/src/bin/penne.rs`): nzb load → queue build → disk-space
  check → `download_queue` → deobfuscate → (mode-gated) PAR2 verify/repair →
  extract → cleanup → cache clear. One job runs at a time (`penne`'s
  `DownloadClient` isn't pooled across runs, so nothing is gained by racing
  two jobs' NNTP connections against each other yet).
- `src/api/` — the SABnzbd-compatible `/api` dispatch.
- `src/web/` — the htmx pages.
- `src/sse.rs` — `/events/:job_id`, the live progress feed the dashboard
  subscribes to.
