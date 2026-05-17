# Roadmap — `pesto`

Fast, lean Usenet poster in Rust. Inspired by `nyuu`, with only the essentials.
Each phase must leave the program in a working, testable state.

## Phase 0 — Foundation ✅

- [x] `cargo init` with `main.rs` + `lib.rs`
- [x] Module skeleton (`config`, `yenc`, `nntp`, `article`, `nzb`)
- [x] CLI parsing with `clap`
- [x] TOML config loading + merge with flags
- [x] Basic CI (`fmt`, `clippy`, `test`)

## Phase 1 — yEnc encoding ✅

- [x] yEnc encoder following the specification (escaping of `=`, NUL, CR, LF)
- [x] `=ybegin` / `=yend` lines with CRC32
- [x] File segmentation into parts (`=ypart` for multi-part)
- [x] Tests against known yEnc vectors

## Phase 2 — NNTP client ✅

- [x] TCP + TLS connection (`rustls`) on port 563
- [x] Handshake and `AUTHINFO USER/PASS` authentication
- [x] `POST` command (article upload, handling of 240/441 responses)
- [x] Article assembly: headers (`Subject`, `From`, `Newsgroups`,
      `Message-ID`) + yEnc body
- [x] Unique `Message-ID` generation per segment

## Phase 3 — Parallel posting ✅

- [x] Pool of N concurrent TLS connections (`tokio`)
- [x] Work queue: segments distributed across connections
- [x] Retry of failed segments
- [x] Progress bar / throughput in the terminal
- [x] Flags `--connections`, `--ssl`, `--groups`

## Phase 4 — `.nzb` generation ✅

- [x] Collect `Message-ID`s, sizes and CRCs of posted segments
- [x] Write a valid `.nzb` XML file (nzb DTD)
- [x] Flag `--out` for the `.nzb` path

## Phase 5 — MVP polish ✅

- [x] Actionable error messages (network, auth, I/O)
- [x] Ctrl-C handling / clean shutdown
- [x] `README` with usage examples
- [x] End-to-end integration test (mock NNTP)

**The MVP is complete.** `pesto` posts files to Usenet and writes an `.nzb`.

## Phase 6 — Posting obfuscation ✅

- [x] `--obfuscate` flag and `obfuscate` config option
- [x] Random subject and yEnc file name per file
- [x] Real file name preserved in the `.nzb` `<file name>` attribute
- [x] Tests for obfuscated-name generation and `.nzb` output

## Phase 7 — PAR2 generation

Own pure-Rust PAR2 creator — no `par2cmdline` / `parpar` dependency. Parity is
computed in the *same single read pass* used for posting: each article, as it
is read and yEnc-encoded for upload, is also accumulated into the Reed-Solomon
recovery buffers. A PAR2 input slice groups several consecutive articles, since
Reed-Solomon cost grows with `file_size² / par2_slice_size`; the group size
targets ~1000 input slices to keep the encode affordable for large files.

### 7a — GF(2^16) field and Reed-Solomon matrix

- [x] GF(2^16) arithmetic (generator `0x1100B`), log/antilog tables
- [x] PAR2 input-constant and recovery-exponent generation, bit-exact with
      `par2cmdline`
- [x] Tests cross-checked against known `par2cmdline` constants

### 7b — PAR2 packet format

- [x] Packet framing, MD5 packet hashes, recovery set ID
- [x] Main, File Description, Input File Slice Checksum, Recovery Slice and
      Creator packets
- [x] Volume-split layout (index + `volNNN+MMM` files, exponential counts)

### 7c — Streaming Reed-Solomon encoder

- [x] Accumulate input slices one at a time into N recovery buffers
- [x] Per-slice MD5 + CRC32 and per-file MD5 computed while streaming
- [x] Validate generated PAR2 with `par2cmdline` (verify + repair)

### 7d — Pipeline integration

- [x] Refactor the poster into a single-reader producer feeding the posting
      pool through a bounded channel
- [x] Compute parity during the read pass; post PAR2 articles after the data
- [x] Include the PAR2 files in the `.nzb`
- [x] `--par2 <percent>` flag and config option; 10% default

### 7e — Performance ✅

- [x] SIMD GF multiply (AVX2 `pshufb` GF(2^16)), scalar fallback
- [x] Recovery buffers partitioned across threads (`rayon`)
- [x] `block_in_place` so the CPU-bound encoder does not stall the runtime

### 7f — Generation modes ✅

- [x] `--par2-only` flag: write parity files next to the source, no posting
- [x] `--dry-run` flag: process files without touching the network

## Phase 8 — Configuration & UX ✅

- [x] Default config path (`$XDG_CONFIG_HOME/pesto/config.toml`), loaded
      automatically when `--config` is omitted
- [x] Random `from` identity by default (random name and length); fixed value
      only when the user pins one
- [x] Interactive setup wizard (`pesto --config` with no value)
- [x] Orientation screen when `pesto` is run with no arguments
- [x] Expanded `--help` and `config.example.toml`; new options
      `line_length`, `retries`, `retry_delay`, `[output].nzb`

## Phase 9 — Directory uploads ✅

Accept directories as arguments, not just individual files. A directory may be
a TV-show season, or any folder with nested subfolders. The whole tree is
posted as one logical upload, and PAR2 must let a downloader rebuild the
original directory layout — not just a flat list of files.

### 9a — Directory traversal ✅

- [x] Accept directory paths as `FILE` arguments alongside plain files
- [x] Recursive walk producing the file list, with the relative path of each
      file preserved (root folder name kept as the top-level component)
- [x] Deterministic ordering (sorted) so runs and PAR2 sets are reproducible
- [x] Decide handling of empty directories, symlinks and hidden/dot files;
      document the chosen behaviour (see `src/walk.rs` module docs)
- [x] Reject or warn on unreadable entries with an actionable message

### 9b — Structure-preserving PAR2 and `.nzb` ✅

- [x] Store the relative path (with `/` separators) in the PAR2 File
      Description packets so `par2` repair restores the directory tree
- [x] Carry the same relative path into the `.nzb` `<file name>` attribute
- [x] One PAR2 recovery set covering the entire directory, not per-file
- [x] Verify with `par2cmdline` that a repair recreates nested subfolders
- [x] Fix latent multi-file bug: PAR2 numbers input blocks in File-ID order,
      so the encoder must process files sorted by File ID, not by name
- [x] `--par2-only` writes the recovery set beside the root folder, so the
      stored relative names resolve correctly

### 9c — Obfuscation for directories ✅

- [x] `--obfuscate` randomises subjects and yEnc names across the whole tree
- [x] Real relative paths still preserved in PAR2 and `.nzb` so the structure
      is recoverable despite obfuscated article names
- [x] Tests for an obfuscated multi-folder upload round-trip

### 9d — UX and naming ✅

- [x] Use the root folder name as the default `.nzb` name; the subject base
      is the file's relative path, which already starts with the root folder
- [x] Progress panel reports total files/bytes across the whole tree
- [x] Aggregate counts (files, subfolders, total size) in the summary output
- [x] `--help`, `README` and `config.example.toml` updated for folder uploads

## Phase 10 — `upapasta` integration ✅

`upapasta` is a Python orchestrator that wraps `nyuu` for the actual posting
step. Replacing `nyuu` with `pesto` removes the Node.js dependency and brings
the full Rust performance to the pipeline.

The bridge between the two programs is `--output-format json`: `pesto`
emits newline-delimited JSON events to stdout; `upapasta` reads them to drive
its own progress display and obtain the final NZB path.

- [x] Stabilize the public API of `lib.rs` (types and functions needed by
      a Rust consumer; keep the `async fn post(config, files)` surface minimal)
- [x] Document all JSON event types emitted by `--output-format json` so
      `upapasta` can parse them reliably
- [x] In `upapasta`: replace the `nyuu` subprocess call in `upfolder.py` with
      a `pesto` subprocess call; parse JSON events for progress and NZB path
      (nyuu kept as automatic fallback when pesto is not in PATH)
- [x] Verify that the `upapasta` obfuscation / PAR2 / compression pipeline
      produces the same result when using `pesto` instead of `nyuu`
- [x] Update `upapasta` install instructions and `README` to reflect the new
      dependency (Rust binary instead of Node.js)

> **Decision point:** `upapasta` still handles PAR2, compression (RAR/7z),
> metadata enrichment, history, and webhooks. `pesto` owns *only* the
> yEnc + NNTP + PAR2 + NZB layer. Do not duplicate orchestration logic in
> `pesto`; keep both tools thin and composable.

## Phase 11 — Reliability & resilience ✅

### 11a — Multiple servers with failover ✅

- [x] Support N servers in config (`[[servers]]` array of tables)
- [x] Connections that fail reopen on the next available server (round-robin
      rotation on each retry attempt)
- [x] Per-server connection count; workers assigned to servers proportionally

### 11b — Upload resume ✅

- [x] Persist post state (posted `Message-ID`s) to a `.pesto-state` sidecar
      file (JSON, keyed by `file_name + part`)
- [x] On the next run, skip already-posted articles and reuse their
      `Message-ID` so the `.nzb` is still complete and correct
- [x] `--no-resume` flag to force a clean start

### 11c — Post-verification via `STAT` ✅

- [x] After posting each article, confirm with `STAT <message-id>` that the
      server registered it (`verify = true` / `--verify`)
- [x] Automatically repost articles that fail the check; rotate servers on
      consecutive STAT failures
- [x] Off by default — use `--verify` or `posting.verify = true` to enable

### 11d — Rate limiting ✅

- [x] `upload_rate` config option (e.g. `"50 MiB/s"`) and `--rate` flag
- [x] Per-worker token-bucket throttle; global rate divided across connections
      so total throughput stays at or below the configured ceiling

## Phase 12 — Performance ✅

### 12a — Double-buffered I/O ✅

- [x] Per-file async reader task feeds a bounded channel of capacity 2 so the
      OS can always be reading article N+1 while the producer accumulates
      PAR2 data and sends article N to the worker pool
- [x] Benefit is largest when the posting channel is full (workers are the
      bottleneck): the producer never sits idle waiting for a disk read

### 12b — Buffer pool ✅

- [x] `Shared::acquire_buffer` / `release_buffer` methods wrapping an
      `Arc<Mutex<Vec<Vec<u8>>>>` pool pre-seeded with `connections + 4` buffers
- [x] Reader task acquires from pool (or allocates on miss); workers return
      the buffer immediately after yEnc encoding; `--par2-only` path returns
      after each article; resume fast-path also returns without allocation

## Phase 13 — Compression before posting ✅

Bundle files into a single archive before yEnc-encoding and uploading.
The default format is **7z in store mode** (no compression — PAR2 handles
integrity; store keeps the pipeline fast and avoids double-compressing already
compressed media).

- [x] `--compress [FORMAT]` flag — bundle without password; FORMAT one of
      `7z` (default), `zip`, `rar`
- [x] `--password [PASSWORD]` flag — bundle with password; bare flag generates
      a random 24-character alphanumeric password and prints it; does NOT imply
      `--compress` independently (each flag has its own purpose)
- [x] `[compression] format` config key; `--compress` / `--password` override it
- [x] `7z` and `zip` via the `7z` CLI (p7zip); `rar` via the `rar` binary
      (not distributed; user must install separately)
- [x] 7z with password uses `-mhe=on` (encrypts archive headers, hiding file
      names); zip uses standard password (no header encryption — zip spec
      limitation); rar uses `-hp` (header encryption)
- [x] With `--obfuscate full`, the archive file name is randomised (32-hex)
- [x] Password stored in `<meta type="password">` in the `.nzb` so NZBGet /
      SABnzbd can extract automatically
- [x] PAR2 computed over the archive; temporary archive deleted after posting

## Phase 14 — Batch and watch modes ✅

Derived from `upapasta` use-cases that belong in the posting layer rather than
the orchestrator.

### 14-pre-a — `--each`: per-file releases from a directory ✅

- [x] When a directory is given with `--each`, treat each top-level entry
      (file or subfolder) as an independent upload with its own NZB
- [x] PAR2 and NZB naming follow the entry name; output files placed next to
      the directory (or in `--out` destination if specified)
- [x] Runs sequentially; combine with `--jobs` (below) for parallelism

### 14-pre-b — `--season`: batch NZB for TV seasons ✅

- [x] Post each file in a directory independently (same as `--each`) **and**
      produce one consolidated season NZB that references all message IDs
- [x] Consolidated NZB takes the directory name; individual NZBs are still
      written alongside each file
- [x] Use case: TV season folder where each episode is a separate Usenet post
      but indexers want a single NZB for the whole season

### 14-pre-c — `--jobs N`: parallel independent uploads ✅

- [x] When `--each` or `--season` produces multiple independent uploads, run
      up to N of them in parallel (each with its own connection pool)
- [x] Default: 1 (sequential); `--jobs 0` means number of logical CPUs
- [x] Total connection count across all jobs does not exceed `connections * N`:
      the semaphore limits concurrency to N jobs, each opening at most
      `connections` NNTP connections

### 14-pre-d — Watch / daemon mode ✅

- [x] `--watch DIR`: poll DIR for new entries and post each automatically
- [x] Optionally imply `--each` so each new entry becomes its own release
- [x] Configurable poll interval (`--watch-interval`, default 30 s)
- [x] On SIGTERM/Ctrl-C: finish any in-progress upload, then exit cleanly
- [x] Move completed entries to a `--watch-done DIR` folder (or delete) so
      they are not re-posted on the next poll
- [x] Designed for headless/server environments; integrates with `upapasta`
      as a replacement for its `--watch` mode

## Phase 14 — Posting features ✅

### 14a — Cross-posting optimisation ✅

- [x] When multiple groups are configured, send each article in a single
      `POST` with all groups in the `Newsgroups:` header instead of separate
      articles per group (already the case: `Article::newsgroups` is a
      `Vec<String>` joined with commas in `serialize()`)
- [x] `.nzb` generation already records the single `Message-ID` per article

### 14b — Configurable `Date:` header ✅

- [x] `date` config option (`[posting].date`): `"now"` (current UTC time),
      `"random"` (random time within the last 30 days), or a fixed RFC 2822
      timestamp. Omit to let the server supply the date (default behaviour)
- [x] `--date` flag overrides the config
- [x] RFC 2822 formatting implemented without external crates

### 14c — Anonymous server support ✅

- [x] `auth` section is fully optional; `AUTHINFO` is skipped automatically
      when neither `username` nor `password` is configured

### 14d — `X-No-Archive` header ✅

- [x] `no_archive` boolean config option (`[posting].no_archive`) and
      `--no-archive` flag
- [x] When enabled, adds `X-No-Archive: yes` to every posted article

### 14e — Configurable `Message-ID` domain ✅

- [x] `message_id_domain` config option (`[posting].message_id_domain`)
- [x] `--message-id-domain` flag
- [x] When set, all articles use the fixed domain; when absent a fresh random
      domain is generated per article (existing privacy-preserving behaviour)

## Phase 15 — NZB & metadata ✅

### 15a — Extended NZB metadata ✅

- [x] `--nzb-name`, `--nzb-password`, `--nzb-category` flags and
      corresponding config keys (`output.nzb_name`, `output.nzb_password`,
      `output.nzb_category`)
- [x] Emit `<meta type="name">`, `<meta type="password">` and
      `<meta type="category">` elements in the `.nzb` when set
- [x] Compatible with NZBGet and SABnzbd metadata conventions
- [x] Archive password from `--password` still populates `<meta type="password">`
      when `--nzb-password` is not explicitly set

### 15b — Automatic NZB upload to indexers ✅

- [x] `[output.indexer]` config section: `url`, `api_key`, `category`
- [x] After a successful post, upload the generated `.nzb` via the Newznab API
      (`POST /api?t=addnzb&apikey=KEY&cat=CATEGORY` with multipart `nzbfile`)
- [x] `--no-upload` flag to suppress the upload for a single run

### 15c — `.nfo` generation

Moved to **Phase 18a**. NFO generation is now implemented natively in pesto
(`src/nfo.rs`) and exposed via `--nfo` / `output.nfo`.

## Phase 16 — Observability & UX

### 16a — Per-phase progress with ETA ✅

Prior to this phase the terminal panel only covered the posting step; compression
and PAR2 recovery writing were silent (or a single `eprintln!`).

- [x] Terminal renderer installed **before** compression so the panel covers
      every phase from start to finish
- [x] Compression phase: `compress()` runs in `spawn_blocking`; a parallel
      200 ms polling task watches the archive file size on disk and emits
      `CompressProgress` events; panel shows a bar, speed, and ETA
      (tight bound in store mode: archive ≈ sum of input sizes)
- [x] PAR2 recovery computation: status line shows elapsed time
      (`▸ computing PAR2 recovery data · 0:12`) so the user knows the RS
      encode is running, not stalled
- [x] PAR2 volume writing: `Par2WriteStarted { total }` + `Par2SliceWritten`
      events; panel shows `▸ PAR2 [████░░░░] X/Y slices · ETA N:NN`
- [x] Non-TTY / CI mode: dedicated plain log lines for each phase
- [x] Five new `ProgressEvent` variants: `CompressStarted`, `CompressProgress`,
      `CompressDone`, `Par2WriteStarted`, `Par2SliceWritten`

### 16b — CLI bug fixes ✅

- [x] `--password` bare flag (no value → random password) failed when
      followed by another flag; fixed by switching from `Option<Option<String>>`
      to `Option<String>` with `default_missing_value = ""`
- [x] `--obfuscate` without a value consumed the following positional file
      argument as its MODE; fixed with `require_equals = true`
- [x] `--password` (archive) and `--password` (server auth) were two flags
      with the same long name; server auth flag renamed to `--auth-password`
- [x] `Message-ID` domain leaked the user's server hostname (e.g.
      `blocknews.net`); `generate_message_id()` now generates a random
      8–15 character domain + random TLD per article, independent of `from`

### 16c — JSON output mode ✅

- [x] `--output-format json` flag (already wired; `spawn_json_emitter` in
      `progress.rs` translates every `ProgressEvent` to a JSON line on stdout)
- [x] Events: `started`, `segment_done`, `queue_extended`, `status`, `failed`,
      `interrupted`, `finished`, `nzb_written`, `compress_*`, `par2_write_*`
- [x] `--no-nfo` accepted as a no-op for backward compatibility with `upapasta`
      invocations that still pass the flag

### 16d — Upload history log ✅

- [x] After each successful upload, append a JSON record to
      `~/.config/upapasta/history.jsonl` — the **same file and format** used
      by upapasta's `catalog.py`, so both tools share a single history visible
      from the upapasta TUI
- [x] Fields written: `data_upload`, `nome_original`, `categoria` (auto-
      detected: Anime / TV / Movie / Generic), `nome_ofuscado`, `senha_rar`,
      `tamanho_bytes`, `grupo_usenet`, `servidor_nntp`, `redundancia_par2`,
      `duracao_upload_s`, `caminho_nzb`, `subject`
- [x] NZB archived to `~/.config/upapasta/nzb/<stamp>_<name>.nzb` (hard-link,
      fallback copy), matching upapasta behaviour
- [x] `--history` / `--no-history` flag (default: enabled); config key
      `output.history`; disabled automatically for `--par2-only` and `--dry-run`

### 16e — Completion notifications ✅

- [x] `[notify]` config section with `webhook_url` (Discord / Slack /
      Telegram / generic HTTP POST) and `ntfy_topic` fields
- [x] On upload completion (or failure), fire a POST with a summary payload;
      payload format mirrors upapasta `_webhook.py` (Discord: `{"content"}`,
      Slack/Telegram: `{"text"}`, generic: rich JSON object)
- [x] ntfy.sh: plain-text body with `Title`, `Priority` and `Tags` headers
- [x] `--notify` / `--no-notify` flags override the config for a run
- [x] Errors are non-fatal — a failed notification never aborts the upload
- [x] Notifications suppressed automatically for `--par2-only` and `--dry-run`

## Phase 17 — Security & privacy

### 17a — Password-protected RAR archives ✅

Standard Usenet clients (NZBGet, SABnzbd) do not understand custom encryption
applied before yEnc. What they do support is the `<meta type="password">` NZB
field, which they use automatically when extracting **password-protected RAR
archives** (`rar -p` / `rar -hp`). Encryption at this level is therefore
implemented as part of Phase 13 (compression), not as a separate byte-stream
cipher.

- [x] `--password <pass>` flag and `posting.password` config option
      (implemented in Phase 13 as `--password` / `compress_password`)
- [x] When compressing to RAR (Phase 13), pass `-hp<pass>` (header encryption,
      hides filenames) to `rar`; ZIP uses standard password; 7z uses `-mhe=on`
- [x] Store the password in `<meta type="password">` in the `.nzb` so
      NZBGet / SABnzbd can extract automatically
- [x] Requires Phase 13 compression to be active; `--password` without
      `--compress` implies `--compress 7z` (default format)

> **Note on AES-256-GCM:** encrypting the raw byte stream before yEnc-encoding
> was considered but removed from scope. No standard download client understands
> this layer, so the downloaded files would be undecryptable without a custom
> tool. If archival-only encryption ever becomes a requirement it should be
> tracked as a separate, clearly non-interoperable feature.

### 17b — Configurable `Message-ID` domain

Already tracked under Phase 14e.

## Phase 18 — Post-upload hooks & NFO generation ✅

### 18a — NFO file generation ✅

Generates a `.nfo` text file that describes the uploaded content:

- [x] `--nfo` flag and `output.nfo = true` config key
- [x] Runs `mediainfo` on the first video file found (lowest-sorted) when the
      input contains recognisable video extensions (mkv, mp4, avi, ts, …)
- [x] For TV season directories: `mediainfo` output for the first episode
- [x] Falls back to a recursive directory/file listing (with sizes) when no
      video file is present or `mediainfo` is not installed
- [x] `.nfo` is written alongside the `.nzb` (same directory, same stem)
- [x] Path is exposed as `PESTO_NFO` to post-upload hook scripts
- [x] `src/nfo.rs` module; no mandatory external dependency — `mediainfo` is
      optional and failure is handled gracefully

### 18b — Post-upload hook ✅

Executes a user-supplied command after each successful upload so external
scripts (Python, Bash, PowerShell, …) can react without polling.

- [x] `--post-hook <CMD>` flag and `output.post_hook` config key
- [x] Runs via the OS shell (`sh -c` on Unix, `cmd /c` on Windows) so any
      interpreter is supported without special handling in pesto
- [x] Environment variables set before the hook runs:
  - `PESTO_NZB` — absolute path to the generated `.nzb` file
  - `PESTO_NFO` — absolute path to the `.nfo` file (empty when not generated)
  - `PESTO_NAME` — original release name / entry label
  - `PESTO_BYTES` — total bytes posted (decimal string)
  - `PESTO_GROUP` — first Usenet newsgroup
  - `PESTO_PASSWORD` — archive password (empty when none)
  - `PESTO_SERVER` — NNTP server hostname
- [x] Hook exit status is logged; a non-zero exit never aborts or fails the
      upload — the post already succeeded at that point
- [x] Hook is suppressed for `--par2-only`, `--dry-run`, and failed uploads

### 18c — Hooks directory & bundled examples ✅

- [x] Any executable file placed in `~/.config/pesto/hooks/` is run
      automatically after each successful upload (sorted alphabetically)
- [x] Unix: executability determined by file permission bits (`chmod +x`)
- [x] Windows: `.exe`, `.cmd`, `.bat`, `.ps1`, `.py` extensions treated as
      runnable; no `chmod` required
- [x] One failing hook is logged and skipped; the remaining hooks still run
- [x] `examples/hooks/print-vars.sh` — starter hook that prints every
      `PESTO_*` variable; installed to `~/.config/pesto/hooks/` by default
- [x] `examples/hooks/curupira.sh` — production-ready hook that uploads the
      `.nzb` (and optional `.nfo`) to [Curupira.cc](https://curupira.cc) via
      its REST API; adapted from the equivalent `upapasta` hook with
      `UPAPASTA_*` variables replaced by `PESTO_*`

## Phase 19 — Test coverage

Raise unit-test coverage across all modules so that regressions in the hot
path and configuration logic are caught before they reach production.

Priority order (easiest → most complex):

### 19a — Pure utility functions (no I/O) ✅

- [x] `indexer.rs`: `urlencoded` — ASCII passthrough, special chars, space,
      slash, at-sign, UTF-8 multi-byte sequences
- [x] `nfo.rs`: `is_video` — recognised and unrecognised extensions, no
      extension, mixed case; `build_listing` — single file, directory with
      nested subdirectories, empty input

### 19b — File-system helpers (temp dir fixtures) ✅

- [x] `nfo.rs`: `find_media_file` — single video file, directory with mixed
      extensions, nested directories, no video file present
- [x] `nfo.rs`: `collect_videos` — sorted order guaranteed; symlinks and
      unreadable entries skipped without panic
- [x] `walk.rs`: existing tests cover the happy path; add tests for symlinks,
      unreadable directories, and dot-file exclusion edge cases

### 19c — Config parsing (TOML round-trips) ✅

- [x] `config.rs`: required fields missing → error with actionable message
- [x] `config.rs`: all optional fields at their defaults vs. fully populated
- [x] `config.rs`: CLI flag overrides config value for every overridable field
- [x] `config.rs`: `parse_upload_rate` — bare bytes, KiB/s, MiB/s, GiB/s,
      case-insensitive, unknown unit → error

### 19d — Article and NZB logic ✅

- [x] `article.rs`: edge cases for zero-length files and articles whose yEnc
      body is exactly the line-length limit
- [x] `nzb.rs`: round-trip for multi-file multi-segment NZB with PAR2 entries

### 19e — Poster core (mock NNTP) ✅

- [x] `poster.rs`: pure helpers — `par2_base`, `resolve_date`,
      `build_server_assignments` fully unit-tested
- [x] `poster.rs`: `RateLimiter` — zero-rate never sleeps; full bucket serves
      small requests immediately
- [x] `poster.rs`: dry-run — no network access, segments produced with correct
      part/total counts and unique Message-IDs
- [x] `poster.rs`: dry_run disables resume by design — documented and tested
- [x] `poster.rs` (mock NNTP): retry logic — article rejected twice (440),
      succeeds on the third attempt; server receives exactly one POST
- [x] `poster.rs` (mock NNTP): resume fast-path — `ResumeState` with all
      segments pre-recorded causes workers to skip; server receives zero POSTs

> **Tooling note:** integration tests that need a real NNTP connection should
> use the existing mock-NNTP harness in `tests/`. Pure-logic tests (retry
> counting, NZB assembly) should live in `#[cfg(test)]` modules inside each
> source file. The `poster.rs` tests that touch the network must be gated with
> `#[ignore]` so `cargo test` passes in CI without a live server.
