# Changelog

All notable changes to `pesto` are documented here.
Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

---

## [Unreleased]

---

## [0.3.35] — 2026-07-01

### Fixed
- **JSON progress output now matches the terminal renderer's accuracy**
  (`--output-format json`):
  - `progress_pct` is now computed from bytes (`done_bytes`/`total_bytes`)
    instead of segment counts, and `total_bytes` is pre-seeded with the same
    PAR2 byte estimate the terminal panel already used
    (`Started.par2_bytes_hint`), absorbing the real PAR2 segments as they
    arrive via `QueueExtended` instead of adding on top. Previously the JSON
    stream tracked segments with no PAR2 pre-seed, so `progress_pct` visibly
    dropped once PAR2 segments were queued.
  - Added `par2_encode_started` / `par2_encode_progress` JSON events (mirrors
    of `Par2EncodeStarted` / `Par2InputProgress`), which were previously only
    rendered in the terminal UI and silently dropped in JSON mode. Consumers
    can now show real progress for the PAR2 computation phase, not just the
    write phase (`par2_write_started` / `par2_slice_written`).

---

## [0.3.34] — 2026-06-30

### Added
- **NNTP keepalive** — workers send `MODE READER` on idle connections to prevent
  server-side disconnections during PAR2 computation, check-phase waits, and
  `--each` transitions. Configurable via `[server] keepalive` (default 60 s;
  set to `0` to disable). Workers poll independently so no connection blocks
  another.

- **Multi-hook support** — `pre_hook` / `post_hook` config fields now accept a
  list of strings (`pre_hooks` / `post_hooks`). Single-value configs remain
  fully backward-compatible (old scalar fields are merged into the array at
  parse time). The CLI flags `--pre-hook` and `--post-hook` can be repeated to
  register multiple hooks in one invocation.

- **Newznab dedup hook example** — `examples/hooks/newznab-dedup.sh` added as a
  reference implementation for checking an upload against a Newznab indexer
  before posting.

### Fixed
- **NFO selection logic** — flat movie folders (single MKV at root alongside
  subtitles/NFO) now run `mediainfo` on the root video instead of the generic
  folder tree. Course and collection folders (videos nested in subdirectories)
  continue to use the folder NFO. Series detection (`S01` / `S01E01` patterns)
  still takes priority.

- **Merged season NZB tags** — `run_merge_season` now reads `nzb_tags` from the
  config file or `--nzb-tag` CLI flags instead of always writing an empty tag
  list. Fixes silent tag omission in merged season NZBs (#26).

- **Segment repost group targeting** — `repost_missing_segments` now posts to
  the groups selected for the upload run (`outcome.groups`) instead of all
  configured groups (`config.groups`). Previously timed-out segments retried
  at end-of-run were cross-posted to every configured newsgroup, making the NZB
  inconsistent with the articles actually on the server.

### Changed
- **Structured trace logging** — upload trace log now includes `file=`,
  `segment=`, `conn=`, `attempt=`, and `retries=` fields for machine-readable
  parsing.

---

## [0.3.33] — 2026-06-27

### Added
- **Hook environment variables expanded** — post-upload and pre-upload hooks
  now receive five additional variables alongside the existing set:
  - `PESTO_CATEGORY` — value of `--nzb-category` (empty string when not set);
    useful for passing the Newznab category code directly to indexer API calls
    without parsing the NZB file.
  - `PESTO_GROUPS` — colon-separated list of all newsgroups (previously only
    `PESTO_GROUP` exposed the first one).
  - `PESTO_NZB_NAME` — value of `--nzb-name` (empty string when not set).
  - `PESTO_OBFUSCATE` — obfuscation mode in use: `none`, `full`, or `paranoid`.
  - `PESTO_PAR2` — PAR2 redundancy percentage (e.g. `10`).
  - `PESTO_TAGS` — space-separated list of NZB tags set via `--nzb-tag`
    (empty string when none).

- **Pre-upload hook directory** — executable scripts placed in
  `~/.config/pesto/pre-hooks/` now run automatically before every upload in
  alphabetical order, matching the post-hook directory behaviour. The directory
  is suppressed by `--no-hooks`; the `--pre-hook` flag and `output.pre_hook`
  config value are not affected.

### Fixed
- **`--no-hooks` now also leaves `--pre-hook` / `--post-hook` running in the
  `pesto` CLI**: the library side was aligned in 0.3.31, but the CLI binary still
  blocked both explicit hooks behind the `--no-hooks` guard. Explicit hooks now
  always run; `--no-hooks` disables only the executable scripts in
  `~/.config/pesto/hooks/` and `~/.config/pesto/pre-hooks/`.

### Docs
- Pre-upload hook section updated: documents the `pre-hooks/` directory,
  clarifies `--no-hooks` scope, and adds the full list of env vars available
  to pre-hooks (`PESTO_GROUPS`, `PESTO_CATEGORY`, `PESTO_NZB_NAME`,
  `PESTO_OBFUSCATE`, `PESTO_PAR2`, `PESTO_TAGS`).

---

## [0.3.31] — 2026-06-25

### Fixed
- **Release label truncated for directory inputs** — `file_stem()` strips
  everything after the last dot, so a release like
  `Show.S01.NTSC.DVD.DD5.1-Group` posted as a directory would have its label
  (and therefore `PESTO_NAME`, NZB filename, and NFO filename) truncated to
  `Show.S01.NTSC.DVD.DD5` at the hook stage. The fix applies `file_stem()`
  only when the input path has a recognised media or archive extension (mkv,
  mp4, avi, ts, m2ts, iso, rar, zip, 7z, …); directory inputs and names
  without such an extension use the full `file_name()` unchanged.

- **PAR2 file descriptions correct for directory uploads**

  1. *Wire name stripping*: PAR2 File Descriptions and yEnc subjects now use
     the path relative to the release root (first component stripped).
     Download clients place all files in the release folder; `par2 repair`
     run from inside that folder can now find files without an extra path
     prefix. File IDs are computed consistently so the pre-sort order and
     the written packets agree.

  2. *Zero-byte file hash alignment*: files with `size == 0` were never fed
     to the PAR2 worker, so `worker.finish()` returned one fewer hash than
     there were files. All File Descriptions for files sorted after the empty
     file received the wrong `file_len` and md5 values, producing
     `"Incorrectly sized verification packet"` errors in par2cmdline. The fix
     inserts a synthetic `FileHashes` entry (md5 of the empty string,
     `length = 0`) at the correct position without affecting the RS encoder.
     The IFSC packet for empty files now correctly carries zero checksums, as
     required by the PAR2 spec.

- **Obfuscated uploads: 0-byte files use their real name on the wire** —
  download clients identify obfuscated files via md5_16k matching and cannot
  match empty files. Since a 0-byte file has no content to protect, pesto now
  publishes it under its real wire name even in `--obfuscate` mode, so the
  client can at least derive the correct filename from the article subject.

### Added
- **Warning for releases containing 0-byte files** — download clients identify
  obfuscated files by their md5_16k hash and cannot match empty files, so they
  end up misplaced after download. pesto now emits a `status` warning at the
  start of the upload listing the affected files and suggesting
  `--compress=rar` or `--compress=7z` as a clean alternative. The upload
  proceeds normally; nothing is blocked.

- **`generic-indexer` hook: automatic screenshots for video releases** — when
  the uploaded file is a video (mkv, mp4, m2ts, etc.), the hook now captures
  6 evenly-spaced frames via `ffmpeg` (at 10 / 24 / 38 / 52 / 66 / 80 % of
  the total duration, avoiding intros and credits), uploads each frame to
  ImgBB, and passes the resulting URLs to the indexer via the
  `screenshot_urls` field. Screenshots are captured at the video's **native
  resolution** (no forced rescaling). If `ffmpeg`, `ffprobe`, or `jq` are not
  available, a warning is printed and the hook proceeds without screenshots.
  The ImgBB API key is set at the top of each script. Implemented for all
  three variants: `generic-indexer.sh`, `generic-indexer.ps1`, and
  `generic-indexer.bat`.

---

## [0.3.30] — 2026-06-23

### Fixed
- **NNTP write operations now respect the configured `timeout`**: all
  `write_all` and `flush` calls in `Connection` are now wrapped in
  `tokio::time::timeout(read_timeout, …)`. Previously, when a server silently
  dropped a TCP connection (no FIN/RST), writes would stall for the OS TCP
  retransmission timeout — ~2 minutes on Windows, up to ~15 minutes on Linux —
  regardless of the `[server].timeout` setting. The user-configured timeout now
  bounds both reads and writes uniformly. A timed-out write surfaces as
  `"NNTP write timed out after {N}s (connection likely dead)"`. Affected paths:
  `enqueue_post`, `flush_pipeline`, `post_parts`, `post`, and `send_command`
  (AUTHINFO / STAT / QUIT). Reported by **Johnmde** (issue #30).

---

## [0.3.29] — 2026-06-23

### Fixed
- **Default pipeline depth changed from adaptive to 1**: pesto was pipelining
  `POST` commands — sending `POST\r\n` plus the article body in one burst
  without waiting for the server's `340` response. This violates RFC 3977 and
  caused strict servers such as Newshosting to reject every pipelined article
  with `441 Posting Failed. Article header field contains invalid characters`.
  The first article (posted sequentially during the adaptive warm-up) always
  succeeded, making the failure pattern hard to diagnose. The fix sets
  `pipeline_depth = 1` as the new default: each connection posts one article at
  a time in strict request-response order. Throughput is unchanged because it
  comes from the pool of parallel connections, not from intra-connection
  pipelining. Users who need higher depth for high-latency links can still set
  `pipeline_depth` explicitly in `config.toml`. Thanks to **Brimm** for
  reporting the issue and providing test credentials that isolated the root cause.

---

## [0.3.28] — 2026-06-23

### Changed
- **BDInfo is now a pure Rust library dependency**: Blu-ray disc analysis is
  now performed by [`bdinfo-rs-core`](https://github.com/agentjp/bdinfo-rs)
  (LGPL-2.1) linked directly into pesto. The previous subprocess-based backends
  — `go-bdinfo` and `BDInfoCLI-ng` (.NET 8) — have been removed entirely. No
  external BDInfo tool needs to be installed.
- **Minimum Rust version bumped to 1.96**: required by `bdinfo-rs-core 1.0.1`.

### Removed
- **`go-bdinfo` and `BDInfoCLI-ng` runtime dependencies**: pesto no longer
  shells out to external BDInfo executables. Playlist selection, stream
  analysis, and QUICK SUMMARY generation all happen in-process via
  `bdinfo-rs-core`. The mediainfo fallback path is kept for the rare case where
  the in-process scan fails (e.g. severely damaged disc structure).

---

## [0.3.27] — 2026-06-22

### Changed
- **BDInfoCLI-ng is now the default BDInfo backend**: pesto now prefers
  [`BDInfo`](https://github.com/tetrahydroc/BDInfoCLI) (tetrahydroc's .NET 8
  fork) over `bdinfo` (go-bdinfo). The tool is tried first; go-bdinfo is kept
  as a fallback. Download pre-built binaries from the
  [Releases page](https://github.com/tetrahydroc/BDInfoCLI/releases).
- **Scan only the main playlist with BDInfoCLI-ng**: pesto passes the playlist
  selected by `find_main_mpls` directly to `BDInfo -m`, scanning only the main
  feature instead of the whole disc.

### Fixed
- **Wrong playlist passed to BDInfoCLI-ng on seamless-branch discs**: the
  initial implementation relied on BDInfo's own playlist sorting (`-l`), which
  orders by total duration and can put seamless-branch playlists ahead of the
  actual main feature (e.g. Drive 2011 DUAL: 00006.MPLS at 3h17 vs 00000.MPLS
  at 1h40). BDInfoCLI-ng now uses the same `find_main_mpls` heuristic as the
  mediainfo fallback path, which selects by mediainfo-reported duration and is
  already tested against this class of disc.

---

## [0.3.26] — 2026-06-22

### Added
- **BDInfo integration for Blu-ray NFO generation**: when
  [`bdinfo`](https://github.com/autobrr/go-bdinfo) is installed and in `PATH`,
  pesto uses it to generate Blu-ray NFO sections instead of `mediainfo`. BDInfo
  automatically selects the main feature playlist, reports all audio and
  subtitle streams with correct language tags, and produces a compact summary.
  Install with `go install github.com/autobrr/go-bdinfo/cmd/bdinfo@latest`.
- **Binary MPLS parser** (`parse_mpls`, `parse_stn_table`): when BDInfo is not
  available, pesto parses the main `.mpls` file directly to extract PID→language
  mappings and injects `Language` tags into the `mediainfo` output for any
  stream that is missing them.
- **Warning when BDInfo is absent**: if `bdinfo` is not found, pesto prints a
  message to stderr advising installation for accurate Blu-ray NFO output.

### Fixed
- **Phantom `=== Blu-ray Disc: BDMV ===` section**: `BDMV/BACKUP/index.bdmv`
  (a mandatory copy required by the Blu-ray spec) was being treated as a second
  disc root, producing a spurious `[no playable stream found]` section in every
  Blu-ray NFO. The `BDMV/BACKUP/` subtree is now skipped during disc-root
  detection.
- **Missing `Language` tags on audio and subtitle streams**: NFOs were generated
  by running `mediainfo` on the raw `.m2ts` stream file, which does not carry
  language metadata. Language information on Blu-ray lives in the `.mpls`
  playlist. pesto now runs `mediainfo` on the main feature `.mpls` (selected by
  longest duration), and the MPLS parser supplements any remaining gaps.
- **Wrong playlist selection by file size**: the previous heuristic picked the
  largest `.mpls` by file size, which could select short looping playlists
  (e.g. a 252-clip seamless-branch playlist on Top Gun: Maverick) over the
  actual main feature. Selection is now based on duration via `mediainfo`,
  matching the approach used for DVD title set selection.

### Fixed (parmesan)
- **PAR2 padding inflation on Blu-ray / DVD disc structures**: discs with many
  small files (`.clpi`, `.mpls`, `BDMV` metadata, etc.) much smaller than the
  computed slice size caused each such file to occupy a full slice of zeros,
  inflating the effective parity ratio well beyond what the user requested (a
  10% request could silently produce 20–30% parity). `calculate_geometry` now
  detects this condition — when the padded-to-actual-data ratio exceeds 1.15 —
  and iteratively halves the slice size until the ratio is acceptable or the
  slice count approaches a CPU-cost ceiling (~6 000 slices). Peak memory usage
  is unaffected: recovery buffer memory is proportional to
  `total_slices × slice_size ≈ total_padded_bytes`, which is invariant to slice
  size for a fixed input set.

---

## [0.3.24] — 2026-06-19

### Added
- **nyuu-compatible short flags**: the most common connection flags now accept
  the same single-letter aliases used by nyuu, making it easier to migrate
  scripts and integrate with tools that target nyuu:
  `-s` (`--host`), `-P` (`--port`), `-u` (`--username`), `-p` (`--auth-password`),
  `-n` (`--connections`), `-g` (`--groups`), `-f` (`--from`).
  The `-o` short form for `--out` was already present. Long-form flags are
  unchanged.

### Changed
- **`--no-hooks` no longer suppresses `--post-hook` / `--pre-hook`**: the flag
  now disables only the directory scripts in `~/.config/pesto/hooks/`. Hooks
  passed explicitly via `--post-hook` or `--pre-hook` are unaffected, so you
  can combine `--no-hooks --post-hook <cmd>` to run a single explicit hook
  without triggering the directory scripts.

### Fixed
- **`435 Already exists` is no longer treated as a post failure**: when a
  connection dropped after the server had already accepted an article, the retry
  re-sent the same Message-ID and the server answered `441 … 435 Already exists
  in history`. pesto treated that as a fatal error and burned the full retry
  budget (reconnect + auth + POST) on segments that were already posted. All
  three `441` POST paths now recognise a `435`/"already exists" rejection as
  success (RFC 3977 §6.2.2) and continue. (#23)
- **NNTP reads now have a timeout**: when a TCP connection died silently (no
  RST/FIN), `read_response` blocked until the OS keepalive aborted the socket
  (~4.5 min on Windows, ~2 h on Linux). Each connection now enforces a
  per-command read timeout, configurable via `[server].timeout` (per-server in
  `[[servers]]`), defaulting to a conservative 120 s — generous enough never to
  trip a slow-but-healthy upload. (#23)
- **End-of-run retry can find the source file again**: `FailedTask` only stored
  the published/base file name, so the retry pass did `PathBuf::from(file_name)`
  and resolved it against the current working directory, failing with `os error
  2` unless the CWD happened to be the source folder. It now also stores the
  absolute `file_path` and re-reads from that. (#23)
- **End-of-run retry re-posts with the original Message-ID**: the retry pass
  previously generated a *fresh* Message-ID per attempt, so an article that had
  actually reached the server during the run (lost `240` ack) would be posted a
  second time under a new ID — a duplicate. `FailedTask` now carries the
  original Message-ID and the retry re-uses it, letting the server deduplicate
  via `435 Already exists` (now treated as success). This mirrors nyuu's
  same-Message-ID repost strategy. In-run retries already reused the ID. (#23)

## [0.3.23] — 2026-06-16

### Fixed
- **`--each` no longer emits or reposts orphan `.nfo`/`.nzb` files**: when a
  segment failed during an `--each` upload, pesto still wrote the `.nfo` next to
  the source files (no `.nzb` is produced on failure, so the path fell back to
  the input directory). A later `pesto … --each --resume` then picked that
  `.nfo` up as a new top-level entry and posted it as a standalone release.
  The CLI now generates the `.nfo` only when the upload actually succeeded, and
  `--each`/`--season`/watch enumeration skips any top-level `.nfo`/`.nzb` file
  so a stray artifact is never reposted. `.nfo` files *inside* a release folder
  (e.g. scene releases) are still uploaded as normal content. (#22)

### Added
- **upapasta now writes verbose per-upload session logs**: pesto's internal
  DEBUG traces (NNTP connections, retries, per-segment results) are routed to a
  timestamped file in `~/.config/upapasta/logs/` for each upload, mirroring
  what the pesto CLI already does. All progress events are also written to
  `upload.log` without filtering.

### Fixed
- **Missing articles after check now block NZB writing and hooks**: when the
  post-upload STAT check found missing articles and repost still left some
  unresolved, `run_upload` was writing the NZB and running post-upload hooks
  anyway (incomplete upload sent to indexers). `had_failures` is now set when
  articles remain missing after repost; NZB writing and hooks are skipped, and
  `status=failed` is recorded in the session log summary.
- **Repost logic moved into `run_upload`**: the automatic repost of missing
  articles after check was only implemented in the `pesto` CLI, not in
  `run_upload` (used by upapasta). Upapasta was detecting missing articles,
  logging them, and continuing without reposting. The full repost + second
  STAT pass is now part of `run_upload` so all callers benefit.

### Added
- **CHECK progress bar in the upapasta dashboard**: when the post-upload STAT
  check is running, a dedicated `CHECK` bar appears below the UPLOAD bar showing
  `checked/total articles` with a cyan progress gauge. If missing articles are
  found, the bar turns yellow and shows `Reposting N missing article(s)…` while
  the reposts are in flight.
- **Check events now emit log lines in the dashboard**: `CheckStarted`,
  `CheckDone` (✓ or ✗), `CheckRetrying`, and `CheckWaiting` all produce
  visible lines in the Logs panel so the user can see the outcome without
  inspecting the session log file.
- **Check toggle in the upapasta upload panel**: the post-upload STAT check
  (`check`) is now exposed as a toggleable field in the upload confirmation
  panel (alongside Verify). Toggle with `←→` or `Enter`; the setting is
  persisted across sessions like other upload preferences. Also shown in the
  Effective Upload Settings summary on the Dashboard and the Config overrides
  panel.

### Changed
- **Session log always ends with a one-line summary**: after every upload
  (success, failure, or cancel) a structured summary line is appended to the
  session log file, e.g.
  `2026-06-11T16:20:11Z  summary  status=ok  label="Movie.mkv"  bytes=4321.5MiB  nzb=Movie.nzb`.
  The file is never empty after a run; `tail -1` is a reliable way to check
  the outcome of any past upload.
- **Session logs now record only errors and warnings by default**: the
  per-upload session log (written to `~/.config/upapasta/logs/` and the pesto
  CLI equivalent) was previously fixed at DEBUG level, capturing every NNTP
  command and progress event. It is now fixed at WARN, keeping the log small
  and focused on actionable failures. Pass `-vv` or set `RUST_LOG=debug` when
  full trace detail is needed.
- **Check-phase and retry errors now emit structured log events**: failures
  during the post-upload STAT pass, missing-article reposts, and segment
  retries were previously only written to stderr (`eprintln!`). They now also
  emit `error!`/`warn!` tracing events so the reason is captured in the session
  log.

### Security
- **Credentials and server hostname redacted in all log levels**: the NNTP
  username, server hostname, and server greeting text were logged in plain text
  at DEBUG level (and hostname also at INFO level), appearing in session logs
  and `-vv` output. All sensitive fields now emit `<redacted>`; the password
  masking token is also standardised to `<redacted>` (was `[MASKED]`).

### Fixed
- **Pipeline error messages no longer repeat the first rejection's message-id**:
  when a pipelined batch fails mid-way, articles that never received a server
  response were logged with the same error text (including the message-id) as
  the first rejected article. They now log `"pipeline interrupted after previous
  failure"`, making it clear which article was actually rejected by the server.

### Fixed
- **`check_delay` in the config file now implies `check`**: the post-upload
  STAT check was only auto-enabled when `--check-delay` was passed on the CLI.
  Setting `posting.check_delay` in `config.toml` without an explicit
  `check = true` silently skipped the check; it now enables it, matching the
  documented CLI behaviour. (#17)
- **Every segment of a file now shares one `Date:` header**: in full obfuscation
  with `date = "random"`, the date was resolved once per *segment*, so each
  segment of a file got a different wire timestamp while the `.nzb` records a
  single date per file — the wire and the NZB disagreed for all but the first
  segment. The date is now resolved once per file and threaded through to every
  segment; paranoid mode keeps its per-article dates. Fixed dates and
  retry/repost dates are also preserved exactly instead of being regenerated.
  (#16)

## [0.3.22] — 2026-06-10

### Added
- **Per-upload debug log saved with the history**: every upload now writes a
  DEBUG-level log to `<history_dir>/logs/<timestamp>_<name>.log`, independent of
  the `-v` flag, so any run can be analysed afterwards — including exactly which
  articles a server rejected and why — without reproducing it with `-vv`. Only
  the 50 most recent pesto logs are kept; files that don't match pesto's naming
  (e.g. legacy upapasta logs sharing the directory) are never pruned. Disable
  per-run with `--no-session-log` or permanently with `output.session_log = false`.
- **Per-article `Date` header logged at DEBUG**: each posted article now logs its
  generated `Date:` value next to its `Message-ID`, so `TooOld`/duplicate
  rejections can be traced to the exact timestamp that triggered them.

### Changed
- **`"random"` date window narrowed from 24 hours to 2 hours**: the 24-hour
  window still tripped `441 437 ... TooOld` rejections on some servers (e.g.
  blocknews) for the random subset of articles whose `Date` landed near the old
  end of the window, failing a small group of segments on every obfuscated
  upload. The window is now 2 hours, which stays well inside server acceptance
  limits while still preventing identical timestamps across a batch.

## [0.3.21] — 2026-06-09

### Added
- **Upload flags summary before posting**: a compact settings block is printed
  below the file tree at the start of each upload showing which non-default
  options are active (obfuscation mode, compression format, password, PAR2
  percentage, resume, verify). Only lines that differ from the default are
  shown, so a plain upload produces no extra output.
- **`resume` option documented in `config.example.toml`**: the `[output]`
  section now includes a commented-out `resume = false` entry with a full
  description of the `.pesto-state` sidecar file lifecycle.

### Changed
- **`"random"` date window narrowed to 24 hours**: previously `date = "random"`
  picked a timestamp anywhere in the last 30 days, causing `TooOld` rejections
  on servers with short retention windows. The window is now 24 hours, which
  is safe for all known NNTP servers while still preventing identical timestamps
  across articles in the same batch.
- **Obfuscation automatically enables random dates**: when any obfuscation mode
  is active and no explicit `date` is configured, pesto now defaults to
  `"random"` instead of omitting the `Date:` header. This closes the timing
  side-channel where all articles in an obfuscated batch shared the same
  server-assigned timestamp. Override with `--date now` or a fixed timestamp
  if needed.

## [0.3.20] — 2026-06-09

### Fixed
- **NZB now reflects actual poster and date from the wire**: generated `.nzb`
  files previously always used `config.from` and `SystemTime::now()` regardless
  of what was sent on the wire. In full/paranoid obfuscation mode this produced
  incorrect metadata. `PostedSegment` now carries the `date` timestamp used for
  each article; `nzb::generate` reads it back instead of inventing a fresh
  value. The `poster` field is also taken from the segment's `from` so rotating
  identities in paranoid mode are correctly reflected per `<file>` element.
- **NZB parse preserves poster/date per segment**: `nzb::parse` now populates
  `PostedSegment.from` and `PostedSegment.date` so the parse → generate → parse
  round-trip used by `--merge-season` produces correct output.

## [0.3.19] — 2026-06-09

### Added
- **Automatic retry of upload failures**: segments that fail during the main
  upload run (e.g. `NNTP connection closed by server`) are now automatically
  reposted before the check STAT pass. A fresh `Message-ID` is generated so
  the recovered segment is fully valid. Retries honour the configured
  `retries` and `retry_delay` values.
- **`FailedTask` in `PostOutcome`**: the public API now exposes
  `PostOutcome::failed_tasks` — a typed list of segments that could not be
  posted, carrying enough information for callers to attempt their own retry
  logic.
- **`repost_failed_tasks` public function**: re-posts a slice of `FailedTask`
  values with fresh `Message-ID`s and returns the resulting `PostedSegment`s.

### Changed
- **NZB is not written when segments remain unrecoverable**: if one or more
  segments still fail after all retry attempts, the NZB file is withheld to
  prevent an incomplete NZB from reaching download clients. The resume state
  file is preserved and the exact `--resume` command is printed so the upload
  can be completed later.
- **Exit code 1 on unrecoverable failures**: the process now exits with a
  non-zero code when any segment could not be posted even after retry,
  making the failure visible to scripts, automation, and Sonarr/Radarr hooks.
- Notifications and post-upload hooks are skipped (marked as failed) when
  there are unrecoverable segment failures.

## [0.3.18] — 2026-06-08

### Added
- **`--obfuscate=paranoid` mode** (experimental, hidden from `--help`): every
  individual article gets a unique, freshly generated subject and `From` header,
  making segment grouping by wire metadata impossible. Requires the NZB to
  download. Must be set explicitly; the `--obfuscate` flag alone still selects
  `full`.
- **Per-file `From` rotation in `full` obfuscation**: each file in a batch
  gets a distinct random sender address, improving anonymity across multi-file
  uploads.
- **Variable-length alphanumeric obfuscated names** (schizo-style): subjects
  and yEnc `name=` fields are now 10–30 random `[A-Za-z0-9]` characters
  instead of a fixed 32-character hex string, eliminating the fingerprint that
  made obfuscated posts identifiable.
- **Random TLD in `From` header**: the generated sender domain now uses a
  random 2–5 character alphabetic TLD instead of a fixed list of real TLDs.
- **`PostOutcome.groups`**: the actual newsgroup used for each post is now
  surfaced in the outcome, enabling accurate NZB generation when only one of
  many configured groups is selected per post.

### Fixed
- **NZB `name=` attribute carried an obfuscated filename**: `--obfuscate=full`
  was randomising the `name=` attribute in the generated `.nzb`, breaking
  download clients that rely on it to restore the original filename without
  PAR2. The NZB now always carries the real filename; only the wire subject and
  yEnc `name=` field are obfuscated.
- **NZB `<groups>` listed all configured groups**: the NZB was including every
  group from `config.groups` even when only one was actually used for posting.
  The actual posted group is now written to the NZB.

### Changed
- **`ObfuscateMode::Subject` removed**: the mode that only obfuscated the
  subject (not the yEnc `name=`) has been removed. Use `full` for standard
  obfuscation. (Existing configs using `subject` will need to be updated.)
- **`--obfuscate` without a value now selects `full`** (unchanged from before,
  but now explicitly documented).

### Docs
- Updated `ROADMAP.md` with completed obfuscation milestones.

## [0.3.17] — 2026-06-08

### Added
- **Parallel STAT pass**: the post-upload check now runs N parallel NNTP
  connections instead of a single sequential one. By default the number of
  connections matches the upload connection count (`posting.connections`),
  giving a proportional speedup — 50 connections → ~50× faster check.
  Override with `--check-connections <N>` or `posting.check_connections` in
  `config.toml`.
- **Check progress bar with countdown**: the terminal now shows a live panel
  during the check delay (`waiting for propagation · 28s remaining`) and a
  progress bar during the STAT pass, coloured green on success and red on
  missing articles.

### Fixed
- **Panic in parallel check workers**: `worker_idx` was passed directly as a
  server index, causing an out-of-bounds panic when the number of workers
  exceeded the number of configured servers (the common single-server case).
  Fixed by taking `worker_idx % servers.len()`.
- **Check panel invisible after upload**: the check renderer is a fresh
  `RenderState` with no upload events, leaving `started = false` and causing
  `draw_panel` to return immediately. `CheckStarted` and `CheckWaiting` now
  set `started = true` so the panel renders correctly.
- **`--check-delay` without `--check` was silently ignored**: passing
  `--check-delay <N>` alone now activates the STAT pass automatically.
- **Repost errors now visible in terminal**: per-attempt repost failures are
  emitted as `Status` events so the reason for a failed repost appears in the
  panel instead of only in the debug log.

### Changed
- **Post-upload check retry interval**: increased from 5 s to 20 s between
  each STAT retry.
- **Default `check_retries`**: raised from 2 to 3.
- **Automatic repost of missing articles**: when `--check` finds articles not
  confirmed by the server, pesto re-reads each missing segment from the
  original file, re-encodes it as yEnc, and reposts it with the same
  `Message-ID` so the existing `.nzb` remains valid. A second STAT pass
  confirms the reposts landed.

### Docs
- Added a dedicated **Post-upload check** subsection under *Reliability*
  documenting both verification modes (`--verify` vs `--check`/`--check-delay`),
  retry mechanics, parallel connections, and terminal output.
- Added `--check`, `--check-delay`, `--check-retries`, and
  `--check-connections` to the flags table.

## [0.3.16] — 2026-06-08

### Added
- **Automatic repost of missing articles**: when `--check` finds articles not
  confirmed by the server, pesto now automatically re-reads each missing
  segment from the original file at the correct byte offset, re-encodes it as
  yEnc, and reposts it with the **same `Message-ID`** so the existing `.nzb`
  remains valid without regeneration.
- After reposting, a second STAT pass confirms the articles landed. Terminal
  output reports each step clearly:
  ```
  check: 1 article(s) not found — reposting…
  check: reposted 1/1 article(s)
  check: all article(s) confirmed after repost
  ```

## [0.3.15] — 2026-06-08

### Fixed
- **`--check-delay` implies `--check`**: passing `--check-delay <N>` alone now
  activates the post-upload STAT pass automatically. Previously both flags had
  to be specified together; the delay value was silently ignored without
  `--check`.

### Changed
- **Post-upload check retry interval**: increased from 5 s to **20 s** between
  each STAT retry, giving slow-propagating servers adequate time between
  attempts.
- **Default `check_retries`**: raised from 2 to **3** (covers up to 40 s of
  additional propagation time after `check_delay` expires).
- **Terminal retry feedback**: when an article is not found on a STAT attempt,
  a yellow notice is shown in the check panel:
  `⏳ article not found — retry 1/3 in 20s`. Clears automatically on the next
  progress update.

### Docs
- Added a dedicated **Post-upload check** subsection under *Reliability*
  explaining both verification modes (`--verify` vs `--check`/`--check-delay`),
  the implied-`--check` behaviour, retry mechanics, and the terminal output.
- Added `--check`, `--check-delay`, and `--check-retries` to the flags table.

## [0.3.14] — 2026-06-08

### Fixed
- **`--verify` mode**: `stat()` now normalises angle brackets before sending
  the `STAT` command (RFC 3977 §6.2.4). The sequential verify path was passing
  `Message-ID`s that already contained `<…>`, causing the server to receive
  malformed commands like `STAT <<<id@domain>>>` and reject them as "not found",
  triggering useless retries. Thanks to **@fabricionaweb** for the fix.

### Changed
- **Docs**: removed references to the `curupira.sh` hook example, which is no
  longer shipped in the repository. The `generic-indexer.sh` example is now
  used in all installation snippets.

### Tests
- Added two async unit tests for `nntp::Connection::stat()` covering both
  calling conventions: message-id with and without angle brackets.

## [0.3.13] — 2026-06-05

### Fixed
- **Windows compatibility**: `find_binary` now uses `std::env::split_paths`
  instead of hardcoded `split(':')`, so `7z.exe` and `rar.exe` are correctly
  located on Windows where PATH uses `;` as separator. Also checks `.exe`,
  `.cmd`, and `.bat` extensions on Windows.

## [0.3.12] — 2026-06-03

### Added
- **`pesto --merge-season <dir>`** — offline command that reads all `.nzb` files
  in a directory, groups them by season identifier (`S01`, `S02`, …) and writes
  one combined season `.nzb` per group beside the source files. No server
  connection is required. Each included episode is printed to the terminal with
  its file and segment counts. Useful when a folder was posted with `--each` and
  a combined season NZB is needed after the fact.
- **`pesto::nzb::parse()`** — public function that reconstructs a
  `Vec<PostedSegment>` from an existing `.nzb` document, enabling NZB-level
  tooling without re-posting.

### Changed
- **upapasta season pack retry**: when one or more episodes in a `--season`
  upload fail with segment errors (e.g. server rejects an article with `441`),
  `upapasta` now automatically retries only the failed episodes. Resume state
  (`resume = true`) is forced for every episode in a season pack so that
  already-accepted segments are skipped on retry and only the missing parts are
  re-sent. If every failed episode recovers, the combined season NZB is
  generated and forwarded to the indexer as normal.
- **upapasta Prowlarr HTTP timeout**: increased from 15 s to 90 s to prevent
  spurious timeouts on slow Prowlarr instances.

## [0.3.11] — 2026-05-31

### Removed
- **Newznab indexer upload** (`[output.indexer]`, `--no-upload`). The built-in
  `t=addnzb` POST path had poor real-world server support and could not carry
  an NFO file. Post-upload hooks (e.g. `curupira.sh`, `generic-indexer.sh`)
  cover the same use case with full NFO support and more flexibility.
  The `[output.indexer]` TOML key is still read to supply the Prowlarr URL
  and API key for the upapasta search/download integration.

## [0.3.10] — 2026-05-30

### Fixed
- `--each` / `--season`: NZB files for entries with long release names could
  end up **zero bytes** on disk (the user-visible copy in `--nzb-dir` and the
  archive copy alike). The history archiver truncated the upload name to 80
  characters, which dropped the file extension; the resulting
  `<stamp>_<name>.nzb` path then collided with the canonical archive copy that
  the NZB is already hard-linked to. `hard_link` failed with `EEXIST` and the
  `fs::copy` fallback copied the file onto itself — `O_TRUNC` zeroed the shared
  inode before the source was read. `archive_nzb` now detects when source and
  destination are the same file (device + inode) and skips the copy entirely.

## [0.3.9] — 2026-05-27

### Changed
- NZB files are now placed **next to the uploaded file** by default when
  `nzb_dir` is not configured and `--out` is not passed. Previously they were
  only written to the internal archive (`~/.config/pesto/nzb/`), which left
  no user-visible copy without explicit configuration. This makes `.nzb` and
  `.nfo` behaviour consistent — both now land beside the source files.
- `pesto --config` wizard now includes an **Output** section with two new
  questions: the NZB output directory (blank = next to uploaded file) and
  whether to generate `.nfo` files. The generated TOML always contains an
  `[output]` block with comments explaining the defaults.

## [0.3.8] — 2026-05-27

### Fixed
- NZB stem no longer truncates release names that contain dots (e.g.
  `Show.S01E01.1080p` was previously shortened to `Show`). The stem is now
  derived from `file_name()` for directories and files with multiple dots,
  matching the behaviour described in the inline comments.

## [0.3.7] — 2026-05-27

### Changed
- `--watch` leaves processed entries in place by default. Files are no longer
  moved or deleted after a successful upload unless explicitly configured.

## [0.3.6] — 2026-05-27

### Added
- NZB archive: every upload now writes a canonical copy to
  `~/.config/pesto/nzb/TIMESTAMP_stem.nzb`, ensuring the NZB is never lost
  regardless of `--out` or `nzb_dir` settings.
- `pesto-poster` crate alias and `README.md` symlink for crates.io publish.

### Fixed
- `--watch` settle check prevents uploads from starting before a file has
  finished being written; failed uploads are retried automatically.

### Changed
- `upapasta`: `nzb_conflict` field added to `Config` initializer.

## [0.3.5] — 2026-05-26

### Fixed
- Strip Windows `\\?\` prefix from `mediainfo` **Complete name** field in NFO
  output, which appeared when `canonicalize()` returned an extended-length path
  on Windows.

## [0.3.3] — 2026-05-26

### Fixed
- `mediainfo` now receives the absolute path to the media file (via
  `canonicalize`), so the **Complete name** field in the NFO no longer shows
  a relative-path prefix (`.\` on Windows, `./` on Linux).

## [0.3.2] — 2026-05-25

### Fixed
- `--nfo` now generates the `.nfo` file unconditionally — it no longer
  requires a successful upload. The flag works correctly with `--dry-run`
  and when the upload fails or is cancelled. NFO is a local artifact (reads
  files, runs `mediainfo`) and has no network dependency.
- `mediainfo` failures now produce an actionable message: the error includes
  whether the binary was not found in `PATH` or exited with a non-zero status
  and its stderr output. Previously the failure was silent.

### Changed
- `nfo.rs` gains `tracing` instrumentation (`debug`/`warn`/`info`) throughout
  `generate()`, `generate_season()`, `write()` and `run_mediainfo()`, making
  NFO decisions visible under `--verbose`.

## [0.3.1] — 2026-05-24

### Added
- `bench/` benchmark suite: `yenc.sh`, `par2.sh`, `posting.sh` with shared
  `lib.sh`; produces copy-paste–ready Markdown tables and CSV results per host.
- `bench/yencode.js` moved from root into `bench/` (was `bench_yencode.js`).
- Performance section in `README.md` with yEnc and PAR2 throughput tables vs
  `node-yencode` and `parpar`.

### Changed
- `node_modules/`, `bench_*.sh`, `GEMINI.md` removed from git tracking.
- `.gitignore` updated to cover `node_modules/`, `bench/results/`,
  `bench/par2_out/`, and legacy `bench_*_out/` directories.

## [0.2.24] — 2026-05-23

### Fixed
- Optimized SIMD paths to avoid register spills, improving performance on Intel mobile CPUs (Raptor Lake) from 500 MB/s to 2.1 GB/s.

## [0.2.23] — 2026-05-23

### Performance
- **yEnc Performance Parity (>2.2 GB/s)**:
  - SIMD-accelerated yEnc escaping using `PSHUFB` expansion tables.
  - Refactored encoder to use direct pointer writes, eliminating vector bounds checks.
  - Optimized AVX2 path for 32-byte chunks.
  - Benchmarked to exceed `node-yencode` throughput (reaching ~2200 MB/s on modern CPUs).

### Added
- Comprehensive yEnc benchmark suite vs `node-yencode` (`bench_pesto_yenc_vs_node.sh`).

## [0.2.22] — 2026-05-23

### Changed
- **`parmesan` extracted as a versioned crate (`parmesan-par2 v0.1.0`)**: the
  PAR2 sub-crate now has its own `CHANGELOG.md`, `README.md`, and full
  crates.io metadata. Published independently as `parmesan-par2` (the name
  `parmesan` was already taken on crates.io).

## [0.2.21] — 2026-05-23

### Fixed
- Silenced `dead_code` warnings in `par2/encoder.rs` that appeared after the
  workspace restructure.

### Added
- `GEMINI.md`: Gemini-specific agent guide for contributors using the Gemini
  CLI.

## [0.2.20] — 2026-05-22

### Performance
- **Shuffle2x AVX2 path (`flush_avx2_shuffle2x_work`)**: new default AVX2
  dispatch replaces the plain nibble-shuffle kernel; measured improvement on
  i5-10400 and Intel Ice Lake.
- **8-way unroll for AVX2+GFNI / 4-way for AVX-512+GFNI** (Phase 25e):
  reduces loop overhead on wide SIMD paths.
- **Background flush worker** (Phase 25f): PAR2 I/O no longer blocks the
  encoder pipeline.
- **`--par2-only` bypass** (Phase 25g): article pipeline is skipped entirely
  when only PAR2 generation is requested, cutting startup latency.
- Fixed 210 ms fixed delay on startup caused by `sysinfo::System::new_all()`
  — replaced with a scoped, lazy call.

## [0.2.19] — 2026-05-22

### Changed
- **AVX-512+GFNI PAR2 path enabled in production**: `flush_avx512_gfni` is now
  active by default on CPUs that report AVX-512F + AVX-512BW + GFNI (Intel Ice
  Lake and newer). Previously gated behind the `par2-avx2-gfni-unsafe` feature;
  validated via `gfni_recovery_matches_scalar` on Intel Ice Lake Xeon (AWS m6i).

## [0.2.18] — 2026-05-22

### Added
- **`--no-hooks`**: skip all post-upload hooks for a single run (both
  `--post-hook` commands and scripts in `~/.config/pesto/hooks/`).
  Useful when testing or reposting without triggering hook side-effects.

### Fixed
- **Season NZB folder name**: the consolidated season `.nzb` now includes
  `<meta type="name">` set to the season stem (e.g.
  `Show.S01.1080p.WEB-DL`) so that SABnzbd and NZBGet name the download
  folder after the season pack instead of the first episode.

## [0.2.17] — 2026-05-21

### Added
- **Verbose mode & diagnostics (Phase 26)**
  - `-v` / `--verbose` flag (repeat for more detail: `-v` = INFO, `-vv` = DEBUG,
    `-vvv` = TRACE); backed by `tracing` + `tracing-subscriber` with `RUST_LOG` override
  - `--log-file FILE` redirects log output to a file; terminal panel stays active
  - System info logged at startup: OS, architecture, CPU features (AVX2+GFNI, SSSE3, NEON)
  - NNTP network trace at DEBUG/TRACE: every command sent and response received,
    with per-command round-trip time in milliseconds
  - TLS handshake and server greeting logged at DEBUG
  - Worker pool events at INFO: connecting, authenticated, connection invalidated
  - Upload plan at INFO: file count, segment count, PAR2 geometry, connection pool size
  - Reed-Solomon SIMD path selection logged at INFO (`simd=avx2+gfni threads=6`)
  - Retry and failover decisions logged at WARN with attempt/error details
  - Compression command logged at DEBUG with password arguments masked as `[MASKED]`
  - Per-phase timing: each phase emits `elapsed_ms` + `phase` on completion
    (compress, par2_compute, par2_write, post, check)
  - One-line timing summary at upload completion
  - Network performance summary: segments posted, failed, total retries
  - Terminal panel suppressed automatically at `-vv` when logs share stderr
  - Credentials never appear in log output (`AUTHINFO PASS [MASKED]`)
- **README**: SIMD acceleration table added under PAR2 recovery data section,
  with dispatch chain, benchmark numbers and `--features bench-internals` usage

## [0.2.16] — 2026-05-20

### Added
- **High-Performance PAR2 Encoder**:
  - Implemented AVX-512 + GFNI, AVX2, and SSSE3 optimized paths for Reed-Solomon encoding.
  - 2D parallelization (chunks × recovery blocks) with hybrid CPU detection.
  - Hardware-accelerated CRC32 and memory prefetching.
- **Terminal UI Restructure**:
  - New multi-panel layout with dedicated PAR2 encoding progress bars.
  - Grouped status matrix and sparkline throughput history.
  - Detailed final upload summaries (average speed, total elapsed time).

### Fixed
- Corrected recovery data generation in AVX2/GFNI paths.
- Fixed progress counter resets between files.
- Improved ETA stability by clamping high-bound ranges.

## [0.2.5] — 2026-05-19

### Fixed
- `rust-version` corrected from `1.75` to `1.87` (code already used APIs
  stable since 1.87; clippy MSRV check was failing in CI)
- Removed useless `PathBuf::from()` wrapping in `config.rs` history_dir map
  (clippy `useless_conversion` warning)
- Applied `cargo fmt` to `config.rs`, `history.rs`, `main.rs`, `progress.rs`
  (CI format check was failing)

### Docs
- Published to crates.io as `pesto-poster` (binary name remains `pesto`)
- README: rust-version badge updated to 1.87+; `cargo install pesto-poster`
  documented in Installing section

### Changed
- History catalog location decoupled from the `upapasta` directory; configurable
  via `output.history_dir` (default: `~/.config/pesto`; set to
  `~/.config/upapasta` to share with upapasta).

### Docs
- `config.example.toml` now documents all implemented sections and fields
  (`[notify]`, `posting.date`, `posting.no_archive`, etc.).
- README: new Prerequisites section, expanded All Flags table (9 missing flags
  added), Installing section with pre-built binary links.
- ROADMAP: Phase 22 (public release preparation) added.

---

## [0.2.4] — 2025

### Added
- **Phase 21 — Terminal UX overhaul**
  - Smooth sub-character progress bars (`▏▎▍▌▋▊▉█`).
  - Color-coded connection status matrix (green = uploading, yellow =
    authenticating, red = retrying, grey = idle). Suppressed when `NO_COLOR`
    is set or stderr is not a TTY.
  - Sparkline throughput history (10-sample rolling graph).
  - Confidence-based ETA: single value when throughput is stable; range with
    instability marker (`~`) when it fluctuates.
  - Directory tree preview printed before upload starts.
  - `--quiet` / `-q` flag and `output.quiet` config key for single-line
    minimal output (spinner + percentage + ETA).
  - `--bell` flag and `output.bell` config key — writes ASCII BEL on
    completion.
  - Buffer pool visualizer in the panel (shown only under memory pressure).
  - Adaptive panel refresh rate: backs off from 200 ms to 500 ms when
    rendering is slow.

---

## [0.2.3] — 2025

### Fixed
- Password not propagated to the consolidated season NZB when using
  `--season --password`.

---

## [0.2.2] — 2025

### Added
- Pre-built binaries for Linux (glibc and musl) and Windows via GitHub Actions
  release workflow.

### Fixed
- NZB/NFO filename truncating release tag on re-post (versioned suffix
  `.v2.nzb` instead of `.nzb.v2.nzb`).
- Race condition in obfuscated directory tests.

---

## [0.2.1] — 2025

### Fixed
- NZB/NFO filename truncating release tag when the upload name contained dots.

---

## [0.2.0] — 2025

### Added
- **Phase 18 — Post-upload hooks and NFO generation**
  - `--nfo` flag and `output.nfo` config key: generates a `.nfo` text file
    using `mediainfo` (optional) or a directory listing fallback.
  - `--post-hook <CMD>` flag and `output.post_hook` config key: run a shell
    command after each successful upload with `PESTO_*` environment variables.
  - Hooks directory: any executable in `~/.config/pesto/hooks/` runs
    automatically after each upload.
  - Bundled examples: `print-vars.sh` and `curupira.sh`.
- **Phase 16 — Observability**
  - Per-phase progress panel covering compression, PAR2 computation, and
    PAR2 volume writing — not only the posting step.
  - `--output-format json` (`--no-nfo` accepted as no-op for `upapasta`
    compatibility).
  - Upload history log written to `~/.config/pesto/history.jsonl` (same format
    as upapasta's catalog); NZB archived to `~/.config/pesto/nzb/`.
    `--history` / `--no-history` flag and `output.history` config key.
  - Completion notifications via `[notify]` config section: Discord/Slack/
    Telegram webhooks and ntfy.sh. `--notify` / `--no-notify` flags.
- **Phase 15 — NZB metadata**
  - `--nzb-name`, `--nzb-password`, `--nzb-category` flags and corresponding
    `output.*` config keys; written as `<meta>` elements in the `.nzb`.
- **Phase 14 — Posting features**
  - `--date` flag and `posting.date` config key (`now`, `random`, RFC 2822).
  - `--no-archive` flag and `posting.no_archive` — adds `X-No-Archive: yes`.
  - `--message-id-domain` flag and `posting.message_id_domain`.

### Fixed
- `--obfuscate=full` leaking real filenames in NZB and NFO.
- Spurious "rar" note printed even when compression was not in use.
- NZB `<head>` block always emitted (was missing when no metadata was set).

---

## [0.1.0] — 2025 — MVP

### Added
- **Phase 0** — Project scaffold: `main.rs`, `lib.rs`, module skeleton,
  `clap` CLI, TOML config loading, basic CI.
- **Phase 1** — yEnc encoder following the specification (escaping, CRC32,
  `=ybegin` / `=ypart` / `=yend` lines). Tests against known vectors.
- **Phase 2** — NNTP client: TCP + TLS (`rustls`), `AUTHINFO`, `POST` command,
  article assembly with `Message-ID` generation.
- **Phase 3** — Parallel posting: pool of N concurrent TLS connections,
  work queue, retry on failure, progress bar.
- **Phase 4** — `.nzb` generation from posted `Message-ID`s.
- **Phase 5** — MVP polish: actionable error messages, Ctrl-C / clean
  shutdown, integration test with mock NNTP.
- **Phase 6** — Posting obfuscation: `--obfuscate` flag with `none`,
  `subject`, and `full` modes.
- **Phase 7** — PAR2 generation: pure-Rust GF(2^16) Reed-Solomon encoder,
  PAR2 packet format, streaming encoder computing parity in the same pass as
  posting. AVX2 SIMD acceleration with scalar fallback. `--par2 <percent>` and
  `--par2-only` flags.
- **Phase 8** — Configuration UX: XDG config path, random `From` identity by
  default, interactive setup wizard (`pesto --config`), orientation screen.
- **Phase 9** — Directory uploads: recursive walk, structure-preserving PAR2
  and NZB, obfuscation for directories.
- **Phase 10** — `upapasta` integration: stable `lib.rs` API, JSON event
  stream for subprocess integration.
- **Phase 11** — Reliability: multiple NNTP servers with failover
  (`[[servers]]`), upload resume (`.pesto-state` sidecar), post-verification
  via `STAT`, rate limiting.
- **Phase 12** — Performance: double-buffered I/O, buffer pool.
- **Phase 13** — Compression: `--compress` (7z/zip/rar via external tools),
  `--password` (random or explicit); password stored in NZB metadata.
- **Phase 14-pre** — Batch and watch modes: `--each`, `--season`, `--jobs`,
  `--watch`.
- **Phase 19** — Test coverage: unit tests for all modules; mock-NNTP
  integration tests for retry and resume logic.

[Unreleased]: https://github.com/franzopl/pesto/compare/v0.3.33...HEAD
[0.3.33]: https://github.com/franzopl/pesto/compare/v0.3.32...v0.3.33
[0.2.5]: https://github.com/franzopl/pesto/compare/v0.2.4...v0.2.5
[0.2.4]: https://github.com/franzopl/pesto/compare/v0.2.2...v0.2.4
[0.2.3]: https://github.com/franzopl/pesto/compare/v0.2.2...v0.2.3
[0.2.2]: https://github.com/franzopl/pesto/compare/v0.2.1...v0.2.2
[0.2.1]: https://github.com/franzopl/pesto/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/franzopl/pesto/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/franzopl/pesto/releases/tag/v0.1.0
