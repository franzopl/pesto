# Changelog — penne

All notable changes to `penne` are documented here.
Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

---

## [Unreleased]

## [0.4.0] — 2026-08-11

### Added

- **`penne check --json` now includes `checked_at`, `method`, `retries`,
  `sample_size` and `servers`/`server`.** `checked_at` is an RFC3339 UTC
  timestamp taken when each outcome line is emitted (not a single run-start
  stamp, since NZBs/servers can finish at different times). `method` and
  `retries` echo the check configuration back so a consumer doesn't have to
  track the CLI invocation alongside the JSON output. `sample_size` only
  appears when `--sample <N>` was used, to flag the result as partial
  coverage rather than a full check. `servers` lists the hostnames tried, in
  tier priority order, for the combined-server case; `--independent-servers`
  keeps its existing per-line `server` field instead.

## [0.3.0] — 2026-08-08

### Changed

- **`--sample <N>` now spreads its selection evenly across each file instead
  of taking the first `N` segments — BREAKING (observable behavior change,
  not just internals).** Prompted by an external caller (`curupirashare`)
  reporting a provider whose storage degrades partway through large files:
  a "first N" sample only ever sees the beginning, so it could pass a
  release its downstream `--stat=body` check exists specifically to catch.
  `penne::queue::sample` now picks indices at a fixed step (`total / N`,
  same stratified approach `curupirashare`'s own routine-check sampling
  already used successfully) instead of a contiguous run from the start —
  `--sample 2` on a 10-segment file now checks segments 1 and 6, not 1 and
  2. No change to `--sample`'s meaning as a count, its `0`→`1` clamping, or
  its interaction with `--stat=body`'s per-segment bandwidth cost.

## [0.2.0] — 2026-08-08

### Added

- **`HEAD`/`BODY`-based availability checks (`--stat=head`/`--stat=body`),
  alongside the existing `STAT` check, plus `--sample <N>` to bound the
  cost of a `body` check.** Prompted by a real report: a provider's `STAT`
  index claimed 99.99% of a release was present, but every actual download
  attempt failed. `pesto::nntp::Connection::head`/`penne::client::
  DownloadClient::head` (RFC 3977 §6.2.2) fetch just an article's headers —
  still far cheaper than a real download, and on most servers reads from
  the same storage `BODY` does. Turned out *not* to be the case for the
  reporting provider (`head` still said 100% present; only a real `body`
  check, matching the actual download failure, caught it) — a useful,
  if humbling, finding, and the reason `CheckMethod::Body` exists as its
  own tier rather than assuming `Head` always suffices. `--sample <N>`
  (first `N` segment(s) of each file, `--stat` only) keeps a `body` check's
  real bandwidth cost bounded on a large release — deliberately implemented
  as reading every sampled request to completion and closing cleanly,
  never an abandoned mid-transfer read (cheaper still, but a pattern real
  providers' anti-abuse systems watch for). `penne::check::CheckMethod`
  (`Stat` still the default, `Head`, `Body`) lets `--stat` pick which to
  trust; `check_availability`'s report is honest about each method's real
  wire cost instead of always saying "STAT only".
- **Named servers + `--server` selector.** A `[[servers]]` entry can carry a
  `name`; `penne download --server <NAME>` (repeatable) restricts a single
  run to just the named entries instead of drawing on every configured
  server, in their config-file order. Lets one config file hold several
  independent providers and pick which one to use per run (e.g. a quick
  `--stat` against a specific provider) with a single flag, instead of
  hand-editing the config or keeping separate config files around. Omitting
  `--server` is unchanged: every configured server is used, as before this
  flag existed.
- **`explicit_only` servers.** A named `[[servers]]` entry can set
  `explicit_only = true` to be skipped by the default server set (whenever
  `--server` is omitted) and used only when named directly via
  `--server <NAME>` — for a block/quota account that must never be drawn on
  automatically as a silent fallback. Requires `name`; rejected at config
  load otherwise, since such an entry could never be selected.
- **Configurable default `--mode`.** The config file can set `mode`
  ("download", "repair", "unpack", or "delete") as the default processing
  level for `penne download` when `--mode` isn't given on the command line.
  `--mode` still overrides it per run; omitting both falls back to
  `unpack`, unchanged from before this field existed.

### Fixed

- **`penne check` no longer reports a connection failure the same way it
  reports a confirmed-missing article — BREAKING.** Prompted by an external
  caller (a private indexer embedding `penne check` to decide whether a
  release is still fully grabbable before declaring it dead) pointing out
  that `CheckOutcome::missing` mixed two very different things: a segment a
  server explicitly denied via `430`/`423`/`420`, and a segment no server
  ever actually answered for at all (connection refused, reset mid-handshake,
  retries exhausted). The latter used to land in `missing` regardless — a
  provider hiccup or a firewall blip was indistinguishable from confirmed
  data loss. `CheckOutcome` now has a separate `unreachable: Vec<
  UnreachableSegment>` field alongside `missing: Vec<MissingSegment>`, plus
  `CheckOutcome::is_conclusive()` (`true` only when every segment got a real
  present/absent answer from some server). A tier's definitive `430` still
  wins as the final verdict even if a *later* tier in the fallback chain is
  unreachable — only a segment nobody, across every configured tier, ever
  got a real answer for counts as `unreachable`
  (`WorkItem::confirmed_missing`, carried across tiers). `CheckOutcome::
  is_complete()` now also requires `unreachable` to be empty, so a check
  that never got a conclusive answer is no longer reported as "complete"
  just because nothing was confirmed missing. `penne check --json` gains
  `unreachable`/`unreachable_pct`/`unreachable_articles`/`conclusive`
  fields; the CLI gains exit code `3` ("inconclusive": no confirmed-missing
  segment, but at least one unreachable one), alongside the existing `0`
  (complete), `1` (confirmed missing — still wins over inconclusive when
  both are present), and `2` (fatal error).

## [0.1.0] — 2026-07-20

First tagged release. `penne` is a fast, `.nzb`-driven NZB downloader for
Usenet: fetches articles over parallel NNTP connections, yEnc-decodes and
reassembles the original files, verifies/repairs them with PAR2, and
extracts any archive it finds — all through a single `penne download`
command. Companion to [`pesto`](../pesto) (which posts) and
[`parmesan`](../parmesan) (which implements PAR2).

### Added

- **Concurrent, resumable fetch.** Up to `connections` parallel NNTP
  connections per configured server, per-segment retry/backoff, and a
  segment-level resume cache (`<out-dir>/.penne-cache/`) so an interrupted
  run picks up where it left off instead of restarting.
- **Multi-server priority and pooling.** Servers are tried in listed order
  (primary, then backups, consulted only for segments the primary lacked);
  adjacent `[[servers]]` entries sharing a `group` value are drained
  together as one combined worker pool instead of strictly one after the
  other.
- **Streaming file assembly.** Each file is written to disk the instant its
  own segments resolve, interleaved with the rest of the fetch, with
  per-segment direct writes (no whole-file buffering) so memory use doesn't
  scale with file size.
- **De-obfuscation.** Recovers real file names for obfuscated/scene-style
  releases from PAR2 File Description packets (content-sniffed regardless
  of extension) by size + hash; falls back to a best-effort guess from
  archive magic bytes and `.nzb` file order when PAR2 doesn't cover a file,
  clearly distinguished from a PAR2-confirmed recovery.
- **PAR2 verify/repair**, powered by [`parmesan`](../parmesan): missing
  files are recreated whole from recovery data, damaged files are patched
  at just the bad slices. A CRC-32 quick-check
  (`pesto::yenc::crc32_combine`) skips the full re-hash entirely when a
  file's already-known checksum alone proves it matches the recovery set's
  IFSC data. Live progress bar during a full verify pass; PAR2 index
  discovery is scoped to the current release's own files, so a shared
  `download_dir` holding a leftover file from a different, earlier
  download can never get verified/repaired by mistake.
- **Archive extraction** (`.rar`/`.7z`/`.zip`, including multi-volume sets
  and password-protected archives), via the `unrar`/`7z` CLIs.
- **`--mode {download,repair,unpack,delete}`**, mirroring `sabnzbd`'s
  per-category Download/+Repair/+Unpack/+Delete processing levels: each
  mode does everything the previous one does, plus one more step.
  `unpack` (fetch + PAR2 + extract) is the default; `delete` additionally
  removes the compressed volumes and PAR2 recovery data once extraction
  succeeds, leaving only the release's other files.
- **`penne download --stat`**: checks every segment's availability via
  `STAT` (RFC 3977 §6.2.4, pipelined) without downloading anything —
  cheap enough to script ahead of a real download to skip a release
  that's already expired off the indexer's server.
- **Disk-space guard and PAR2-redundancy health warning** ahead of the
  expensive full verify pass, so a release that looks unrepairable (not
  enough recovery data for the damage found) is flagged early.
- **Categorized NNTP error messages** (`pesto::nntp::ErrorHint`) for
  connect/auth failures — too many connections, too many IPs, login
  failed, payment required — instead of a raw, unclassified server
  response.
- **Live terminal UI**: an overall progress panel (bar, speed, ETA,
  capped per-file bars) on stderr while fetching, a lighter one for
  `--stat`, and one for a full PAR2 verify pass — all with a plain,
  one-line-per-percentage fallback when output isn't a terminal.
- **Interactive setup**: `penne --config` writes a TOML config
  (`$XDG_CONFIG_HOME/penne/config.toml` by default) via a guided wizard.

See [`ROADMAP.md`](ROADMAP.md) for the full phase-by-phase history and
design rationale behind each of the above.
