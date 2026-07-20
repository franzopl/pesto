# Changelog — sugo

All notable changes to `sugo` are documented here.
Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

---

## [Unreleased]

### Added

- **Live dashboard over SSE, no more polling.** `/events/queue` re-renders
  the queue partial server-side on every job-state change and pushes it as
  HTML; the dashboard's `hx-ext="sse"` wiring swaps it in instantly instead
  of waiting out the old 3-second `hx-trigger="every 3s"` poll (kept as a
  15s defensive fallback in case SSE ever drops silently). Newly staged
  jobs (`addfile`/`addurl`/browser upload) broadcast immediately so they
  show up before the worker even picks them up.
- **Richer progress**: per-file breakdown (`job::JobFileProgress`), speed
  (a simple EMA) and ETA, downloaded/total bytes as text
  (`pesto::progress::format_size`), and a live PAR2 verify detail
  (`job::JobVerifyProgress` — "verifying X: N/M blocks" for whichever file
  is currently being checked; see the crate's docs for why this is
  per-current-file, not a release-wide percentage). All throttled to at
  most one flush per ~350ms at the source
  (`job::pipeline::FLUSH_INTERVAL`), so every consumer (the API, the SSE
  streams) inherits a sane update cadence for free.
- **Toast notifications** on job completion/failure, via
  `/events/notifications` + a small inline script in `templates/base.html`
  — the one place this crate uses hand-written JS.
- **Settings CRUD**: edit and delete existing `[[servers]]` entries (not
  just add), general config (`download_dir`/`retries`/`connections`/
  `mode`), `[[web.categories]]` (a category maps to its own destination
  directory — `job::stage_and_create` resolves a job's `dest_dir` against
  it), and API key regeneration. Every mutation writes straight back to
  the config file. `mode=get_config` now lists the real configured
  categories instead of a hardcoded stub.
- `mode=queue`'s `size`/`sizeleft`/`timeleft` now use
  `pesto::progress::format_size`/`format_duration` (real ETA-derived
  countdown) instead of hand-rolled megabyte math and a hardcoded
  `"0:00:00"`.

First vertical slice of the SABnzbd-API-compatible web UI planned in
`penne`'s own `ROADMAP.md` ("Later — Web UI"): a separate crate consuming
`penne` as a library, the same relationship `upapasta` has with `pesto`.

- **Config**, reusing `penne::config::RawConfig` for `[[servers]]`/download
  settings (an existing `penne` `config.toml` loads unchanged) plus a
  `[web]` table for bind address, API key, and data directory.
- **Background job engine**: an in-memory pending/active/history queue,
  persisted to a JSON snapshot on every change so a restart doesn't lose
  it; one job processed at a time, each run through `job::pipeline::
  run_job` — a per-job port of `penne`'s own CLI pipeline (nzb load, queue
  build, disk-space check, download, deobfuscate, mode-gated PAR2
  verify/repair, extraction, cleanup, cache clear).
- **SABnzbd-compatible `/api`**: `version`, `addfile` (multipart upload),
  `addurl`, `queue` (list, delete, pause/resume), `history` (list, delete),
  `fullstatus`, and a `get_config` stub — enough for Sonarr/Radarr/Prowlarr's
  SABnzbd download-client "Test" handshake and normal queueing to work
  against it. Every call requires `?apikey=`, checked in constant time.
- **htmx dashboard/history/settings pages** (`askama` templates, vendored
  htmx — no CDN dependency, no JS build step): upload an `.nzb`, watch the
  queue with a polling-refreshed progress table, browse history, and add
  `[[servers]]` entries from the browser (written back to the config file).
- **`/events/:job_id`** SSE endpoint, fed by a single global broadcast
  channel filtered per job, for a future live-updating progress bar (the
  dashboard today refreshes via htmx polling; the wiring for a push-based
  bar is in place).

### Deferred

Archive extraction still has no live progress (`penne::extract` doesn't
parse `7z`/`unrar` stdout — only the status transition is reported); category
*priorities* (as opposed to categories themselves, which now exist);
`set_config`, `addlocalfile`, `rss`; and connection pooling across jobs.
