# sugo

**SABnzbd-API-compatible web UI for [`penne`](../penne).**

A lightweight, single-binary web front end for `penne`'s download engine —
server-rendered (`askama` + htmx, no JS build step), consuming `penne`
directly as a Rust library (no subprocess/JSON spawning). Exposes a subset
of SABnzbd's real `/api` wire format so existing tools (Sonarr, Radarr,
Prowlarr, SAB-aware mobile apps) can point at it without any new
integration work.

> **Status:** config, the background job engine, the core SABnzbd `mode`s
> (`version`, `addfile`, `addurl`, `queue`, `history`, `fullstatus`,
> `get_config`), and the htmx dashboard/history/settings pages are
> implemented and tested end-to-end against a fake NNTP server. The
> dashboard updates live over SSE (no polling), with per-file progress,
> speed/ETA, and a real-time PAR2 verify detail. Settings supports
> add/edit/delete servers, categories (with their own destination
> directory), general config (`download_dir`/`retries`/`connections`/
> `mode`), and API key rotation, all from the browser. Archive extraction
> still has no live percentage (`penne::extract` doesn't parse `7z`/`unrar`
> stdout) — only a status transition. `set_config`, `addlocalfile`, `rss`,
> and connection pooling across jobs are deferred. See `crates/sugo`'s own
> history for what's next.

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
access"). All of this — servers, categories, general config, and the API
key — can also be managed from the `/settings` page in the browser instead
of hand-editing the file; every change there is written straight back to
the config file.

### Categories

```toml
[[web.categories]]
name = "movies"
dir = "/downloads/movies"   # optional — omit to keep the default download_dir/<job name> layout
```

`"*"` always exists implicitly and never needs to be listed. A job's
category (set via `mode=addfile`/`addurl`'s `cat=` parameter, or the
dashboard's upload form) picks its destination directory from a matching
entry here, falling back to `[core].download_dir`/`<job name>` when the
category has no `dir` of its own or doesn't match anything configured.

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
| `get_config` | Lists real `[[web.categories]]` entries (always including `"*"`) — enough for `*arr`'s "Test" handshake, and for Sonarr/Radarr to offer your configured categories. |

Every call needs `?apikey=<key>` matching `[web].api_key`.

## Architecture

- `src/job/` — the job model (including per-file progress, speed/ETA, and
  the PAR2 verify pass's live position), in-memory queue/history
  (JSON-snapshotted to `<data_dir>/state.json` so a restart doesn't lose
  the queue), and `pipeline.rs`, a per-job port of `penne`'s own CLI
  pipeline (`crates/penne/src/bin/penne.rs`): nzb load → queue build →
  disk-space check → `download_queue` → deobfuscate → (mode-gated) PAR2
  verify/repair → extract → cleanup → cache clear. Progress is throttled to
  ~350ms per flush (see `FLUSH_INTERVAL`) regardless of how chatty the
  underlying per-segment/per-slice event stream is. One job runs at a time
  (`penne`'s `DownloadClient` isn't pooled across runs, so nothing is
  gained by racing two jobs' NNTP connections against each other yet).
- `src/api/` — the SABnzbd-compatible `/api` dispatch.
- `src/web/` — the htmx pages, including every settings mutation
  (`web/settings.rs`).
- `src/sse.rs` — three SSE streams: `/events/:job_id` (raw JSON, one job,
  a programmatic surface not used by the UI today), `/events/queue` (the
  dashboard's actual live-update mechanism — re-renders and pushes the
  whole queue partial as HTML on every change, so htmx swaps it in with no
  custom JS), and `/events/notifications` (JSON `Finished` events, consumed
  by `templates/base.html`'s small toast script — the one place this crate
  uses hand-written JS).
