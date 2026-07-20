# Changelog — sugo

All notable changes to `sugo` are documented here.
Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

---

## [Unreleased]

### Added

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

Per the plan's explicit scope for this first slice: live progress for the
PAR2 verify/extraction phases (only the status transition is reported
today, not a percentage), per-category routing and priorities, `set_config`,
`addlocalfile`, `rss`, and connection pooling across jobs.
