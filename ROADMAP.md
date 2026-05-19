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

## Phase 21 — Visual Feedback & Terminal UX

Deliver an impressive, information-dense terminal experience without depending
on external TUI crates for the incremental items. Work ordered by impact vs.
effort ratio — each sub-phase must leave the panel better than before.

### 21a — Smooth progress bars ✅ (priority 1)

Replace the plain `████░░░░` bar with sub-character block rendering so the
bar moves continuously instead of jumping full-cell steps.

- [x] `render_bar` uses the eight-level block sequence
      `▏▎▍▌▋▊▉█` for the fractional leading character
- [x] The filled portion uses `█`; the unfilled portion uses `░` (unchanged)
- [x] No new dependencies; pure Unicode

### 21b — Color-coded connection status matrix ✅ (priority 2)

Paint each connection cell with ANSI colours to communicate state at a glance:

- [x] 🟢 Active/uploading → green cell label
- [x] 🟡 Authenticating / reconnecting → yellow
- [x] 🔴 Retrying / failed → red
- [x] ⚪ Idle → dim/grey
- [x] New `ProgressEvent` variants `ConnectionRetrying { conn }` and
      `ConnectionAuth { conn }` emitted by the NNTP pool
- [x] Colors suppressed when `NO_COLOR` env var is set or stderr is not a TTY

### 21c — Sparkline throughput history ✅ (priority 3)

Show a 10-sample rolling graph of upload speed directly in the panel so
fluctuations are visible without any external tool.

- [x] `RenderState` keeps a ring-buffer of the last 10 per-tick byte deltas
- [x] `render_sparkline(samples) -> String` maps each sample to one of
      ` ▁▂▃▄▅▆▇█` proportional to the max in the window
- [x] Displayed on the right side of the speed line: `12.3 MiB/s ▁▃▅▇█▆▄▂▃█`
- [ ] Degrades gracefully to nothing when terminal is < 60 columns

### 21d — Confidence-based ETA ✅ (priority 4)

Display ETA as a range when throughput is unstable rather than a single
potentially misleading value.

- [x] Track a rolling coefficient of variation (σ/μ) over the last 10 speed
      samples
- [x] When CV < 0.1: show single ETA as today (`ETA 2:34`)
- [x] When CV ≥ 0.1: show range (`ETA 2:10–3:05`) based on ±1σ projection
- [x] When CV ≥ 0.3: append a `~` instability marker (`ETA ~2:30–4:00`)
- [x] No new dependencies; pure arithmetic on existing ring-buffer

### 21e — Directory tree preview ✅ (priority 5)

Print a clean `tree`-style breakdown of the payload in the pre-flight summary
before any encoding/uploading starts.

- [x] `print_tree(files: &[InputFile])` in `progress.rs` renders the file
      list as a hierarchical tree by splitting names on `/`
- [x] Shows per-file size on the right column, total at the bottom
- [x] Only emitted when stderr is a TTY; suppressed in JSON / quiet mode
- [x] Called from `main.rs` after the file list is resolved, before
      `spawn_terminal_renderer`

### 21f — Quiet / minimal mode ✅ (priority 6)

Single-line mode for tmux/screen users who want minimal terminal noise.

- [x] `--quiet` / `-q` flag and `output.quiet = true` config key
- [x] In quiet mode: single line re-drawn in place showing a spinner (`⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏`)
      followed by percentage and ETA, e.g. `⠹  47% · ETA 1:23`
- [x] On completion: replaces the spinner line with a single summary line
- [x] No box-drawing characters; zero ANSI colour (so it degrades cleanly in
      logging pipelines even if accidentally used there)
- [x] `--quiet` suppresses the directory tree preview (21e) and sparklines (21c)

### 21g — Audible bell on completion ✅ (priority 7)

- [x] `--bell` flag and `output.bell = true` config key
- [x] Writes `\a` (ASCII BEL) to stderr on successful completion
- [x] Also fires on failure so the user is notified either way
- [x] Off by default; never emitted in JSON mode

### 21h — Buffer pool visualizer ✅ (priority 8)

Show real-time buffer pool health in the panel so resource pressure is visible.

- [x] New `ProgressEvent::BufferPoolStats { total: usize, free: usize }` emitted
      by `Shared` every N segments (not every segment — keep it cheap)
- [x] `RenderState` renders a compact mini-bar: `buf [████████░░] 8/10`
- [x] Added as a single line below the connection grid when pool is under
      pressure (free < 25% of total); hidden otherwise to reduce clutter

### 21i — Adaptive refresh rate ✅ (priority 9)

Lower the panel redraw frequency when the CPU is loaded so rendering does not
compete with the encoding/uploading hot path.

- [x] Replace the fixed 200 ms ticker in `render_loop` with a dynamic interval
- [x] Start at 200 ms; back off to 500 ms when the last draw took > 5 ms
      (measured with `Instant` around the `draw_panel` call)
- [x] Return to 200 ms when the previous draw was fast again
- [x] Measurement uses monotonic clock; no system calls beyond what tokio
      already uses

### 21j — Interactive TUI mode with ratatui (priority 10)

Full-screen dashboard replacing the scrolling panel. Most complex item;
delivered last so the simpler improvements ship first.

- [ ] Add `ratatui` and `crossterm` to `[dependencies]` behind a
      `tui` Cargo feature (off by default, so the default binary stays small)
- [ ] `--tui` flag activates the dashboard; requires a TTY, otherwise falls
      back to the standard panel with a warning
- [ ] Layout: three panes — (1) real-time speed graph (last 60 s), (2)
      connection grid, (3) file list with per-file progress bars
- [ ] Speed graph uses `ratatui::widgets::Sparkline` or `Chart`; connection
      grid is a `Table`; file list is a `List`
- [ ] Keyboard: `q` quits (after confirming), `p` pauses rate display, `h`
      toggles help overlay
- [ ] All state driven by the same `ProgressEvent` channel so the TUI is a
      drop-in renderer alongside the existing panel

## Phase 22 — Public Release Preparation

Getting `pesto` ready for public consumption. Items ordered by impact: critical
correctness first, then polish, then open-source completeness.

### 22a — Complete `config.example.toml` ✅

Several implemented sections are entirely absent from the example file:

- [x] `[notify]` section — `webhook_url`, `ntfy_topic` (Phase 16e)
- [x] `[output.indexer]` section — `url`, `api_key`, `category` (Phase 15b)
- [x] `output.nzb_name`, `output.nzb_password`, `output.nzb_category` (Phase 15a)
- [x] `output.nfo`, `output.post_hook`, `output.bell`, `output.quiet` (Phases 18, 21f, 21g)
- [x] `posting.date`, `posting.no_archive`, `posting.message_id_domain` (Phase 14b–e)

### 22b — Complete "All flags" table in README ✅

Implemented flags missing from the reference table:

- [x] `--nfo` / `--no-nfo`
- [x] `--quiet` / `-q`
- [x] `--bell`
- [x] `--history` / `--no-history`
- [x] `--notify` / `--no-notify`
- [x] `--post-hook`
- [x] `--date`
- [x] `--no-archive`
- [x] `--message-id-domain`

### 22c — Document external dependencies in README ✅

- [x] Add "Prerequisites" section listing `p7zip` (for `--compress`) and `rar`
      binary (for `--compress=rar`), with install commands for common platforms
- [x] Note that `mediainfo` is optional but recommended for `--nfo`
- [x] Note that `par2cmdline` is not required at runtime (pesto has its own
      implementation)

### 22d — Badges and minimum Rust version ✅

- [x] Add CI status, crates.io version, and license badges to the top of README
- [x] Set `rust-version = "1.75"` in `Cargo.toml`
- [x] Mention the minimum Rust version in the README Build section

### 22e — Installing section (binaries + crates.io) ✅

- [x] Document downloading a pre-built binary from GitHub Releases
- [x] Note how to add the binary to `PATH` on Linux and Windows
- [x] `cargo install pesto-poster` documented; published to crates.io as
      `pesto-poster` (binary name remains `pesto`)

### 22f — CHANGELOG.md ✅

- [x] Create `CHANGELOG.md` covering v0.1.0 → v0.2.4 with highlights per version
- [x] Follow Keep a Changelog format

### 22g — Document all JSON event types ✅

The JSON output mode section in README shows only 5 sample events; the full
set is needed for reliable integration by `upapasta` and other consumers.

- [x] Document all `ProgressEvent` types emitted by `--output-format json`
      (started, segment_done, queue_extended, status, failed, interrupted,
      finished, nzb_written, compress_*, par2_write_*)
- [x] Include field names and types for each event

### 22h — CONTRIBUTING.md ✅

- [x] How to set up the dev environment and run tests
- [x] How to use the mock NNTP harness
- [x] Commit message and PR conventions
- [x] Pointer to ROADMAP.md for picking up work

### 22i — Audit ROADMAP checkboxes ✅

- [x] Review all `[ ]` items; mark completed ones as `[x]`
- [x] Remaining open items: Phase 21c (< 60-column sparkline fallback) and
      Phase 21j (ratatui TUI) — both genuinely not yet implemented; Phase 22e
      (`cargo install`) pending crates.io publication

---

## Phase 23 — Stdin Pipelining & Post-Check ✅

### 23a — Stdin pipelining ✅

Allow `pesto` to receive data from another process via a Unix pipe without
writing an intermediate file to disk manually.

- [x] `-` accepted as a `FILE` argument (one occurrence per invocation)
- [x] `--stdin-name NAME` flag — sets the filename published in the NZB and
      PAR2 metadata (required when `-` is used)
- [x] stdin is read into a named temp file (`tempfile::Builder`) so the poster
      can seek, stat, and pass the data to the existing pipeline unchanged
- [x] The temp file is named after `--stdin-name` so `expand_inputs` picks up
      the right base name without changes to `walk.rs` or `poster.rs`
- [x] Error if stdin is a terminal, if `--stdin-name` is missing, or if `-`
      appears more than once
- [x] Incompatible with `--each` and `--season` (documented)
- [x] PAR2 and compression work normally on the temp file

**Typical usage:**

```bash
tar cf - ./Season01/ | pesto - --stdin-name season01.tar --out season01.nzb
zstd -c bigfile.mkv  | pesto - --stdin-name bigfile.mkv.zst --groups alt.binaries.x
```

### 23b — Deferred post-check ✅

Verify that every posted article is retrievable on the server after uploading
completes, without blocking the upload throughput.

- [x] `--check` flag enables the check phase (config: `posting.check`)
- [x] `--check-delay SECS` — seconds to wait before issuing STAT commands;
      default 30 (config: `posting.check_delay`)
- [x] `--check-retries N` — STAT attempts per article before marking it
      missing; default 2 (config: `posting.check_retries`)
- [x] Check phase runs **after** all articles are posted and the progress
      renderer finishes — upload throughput is never impacted
- [x] Uses a single NNTP connection (STAT is lightweight); reuses primary server
- [x] `ProgressEvent::CheckStarted / CheckProgress / CheckDone` for the
      terminal panel, plain log, and JSON emitter
- [x] Terminal panel shows `▸ check [████░░░░] N/M · X missing` in red when
      articles are missing
- [x] Missing Message-IDs printed to stderr; exit code 1 when any are missing
- [x] `check: "all N article(s) verified"` printed on success
- [x] Skipped automatically on `--dry-run`, `--par2-only`, cancelled runs

**Difference from `--verify`:** `--verify` re-posts immediately during upload
if STAT fails; `--check` runs after all articles are posted, does not re-post,
and is designed for confirming propagation to the server with a delay.

---

## Phase 24 — PAR2 Performance & Compatibility

Improvements to the PAR2 implementation. Each sub-phase is independent and
can ship separately.

### 24a — SSE2/SSSE3 intermediate SIMD path ✅

The encoder currently dispatches to AVX2 or falls back to scalar. CPUs that
have SSSE3 but not AVX2 (Sandy Bridge, Ivy Bridge) take the slow scalar path.
A SSSE3 path using 128-bit `_mm_shuffle_epi8` nibble tables covers every x86-64
CPU made since ~2007.

- [x] Implement `flush_ssse3()` using `__m128i` and `_mm_shuffle_epi8`
      (same 4-nibble algorithm as the AVX2 path, halved register width)
- [x] Runtime dispatch chain: AVX2 → SSSE3 → scalar
- [ ] Benchmark on a pre-AVX2 machine and document the speedup

### 24b — AVX-512 + GFNI path ✅

Intel Ice Lake (2019+) and later server CPUs expose `AVX-512BW` and GFNI
(`gf2p8affine_epi64_epi8`). GFNI performs GF(2^8) affine transforms in a
single instruction; with a two-step decomposition GF(2^16) multiplication
becomes two `vgf2p8affineqb` on 512-bit vectors — roughly 2× the AVX2
shuffle throughput (32 words per iteration vs 16).

- [x] Decompose GF(2^16) multiply-by-coeff into four 8×8 GF(2) matrices
      (M_ll, M_lh, M_hl, M_hh), computed once per flush batch
- [x] Implement `flush_avx512_gfni()` using `_mm512_gf2p8affine_epi64_epi8`:
      de-interleave lo/hi bytes with `vpshufb`, apply paired matrices with two
      GFNI calls, fold with `vpbsrldq`+`vpxor`, re-interleave with `vpunpcklbw`
- [x] Runtime dispatch: GFNI+AVX512F+AVX512BW → AVX2 → SSSE3 → NEON → scalar
- [x] Requires Rust ≥ 1.89 (AVX-512 GFNI intrinsics stabilised in 1.89);
      MSRV bumped accordingly
- [x] Benchmark infrastructure: `bench-internals` Cargo feature exposes
      `BenchPath` enum and `with_forced_path()` builder so each SIMD path can
      be measured independently on the same machine (`cargo bench --features bench-internals`)
- [x] Baseline measured on AMD/Intel AVX2 machine (12 Rayon threads,
      slice 768 000 B, 256 MiB @ 10 % redundancy):
      - scalar:  317 MiB/s in |  10.9 GiB/s GF madd
      - SSSE3:   597 MiB/s in |  20.4 GiB/s GF madd  (1.88× vs scalar)
      - AVX2:    813 MiB/s in |  27.8 GiB/s GF madd  (2.56× vs scalar)
      - GFNI+AVX512: measured on Ice Lake / Sapphire Rapids hardware (pending)

### 24c — ARM NEON path ✅

The encoder is gated on `#[cfg(target_arch = "x86_64")]`. Apple Silicon M-series
and ARM servers (AWS Graviton, Ampere Altra) fall back to scalar. The AArch64
NEON instruction `vqtbl1q_u8` is the direct equivalent of `_mm_shuffle_epi8`
and enables the same 4-nibble shuffle algorithm.

- [x] Implement `flush_neon()` using `vqtbl1q_u8` for `target_arch = "aarch64"`
- [x] Mirror the 32 KiB cache-blocking strategy from the x86 paths
- [x] Add `#[cfg(target_arch = "aarch64")]` dispatch alongside the x86 chain
- [x] Verify correctness with the existing Reed-Solomon unit tests on an ARM target

### 24d — XOR bit-dependency method (x86, advanced)

For a fixed GF(16) coefficient it is possible to precompute which input bits
XOR into each output bit (a 16×16 GF(2) matrix) and then apply that as a
sequence of `vpand` + `vpxor` with operands in registers — no `vpshufb`, no
table loads after setup. For large recovery sets the working set stays entirely
in registers, eliminating all L1 miss pressure in the inner loop.

This is the most complex item: the coefficient-specific XOR program must be
generated at runtime once per (recovery-block, coefficient) pair.

- [ ] Implement a code-generator that, given a `u16` coefficient, emits a
      sequence of `(mask, shift, xor_into_reg)` operations computable with
      `vpand`/`vpsrl`/`vpxor`
- [ ] Integrate as `flush_avx2_xor()` and benchmark against the current shuffle path
- [ ] Ship only if the benchmark shows ≥ 20 % real-world improvement on a
      representative recovery set (≥ 50 recovery blocks, 1 MiB slice size)

### 24e — Optimal slice size selection ✅

The current implementation derives a fixed slice size from `--par2-block-size`
or a simple ratio of total file size. For mixed-size sets (e.g. one 10 GiB file
+ twenty 1 MiB sidecar files) a naive size either wastes blocks on tiny files or
hits the PAR2 spec limit of 32 768 input blocks. A binary search over candidate
sizes finds the optimal value automatically.

- [x] Implement `optimal_par2_slice_size(per_file_articles, article_size, redundancy_pct)`
      that binary-searches the smallest `articles_per_slice` such that:
      total input blocks ≤ 32 768 **and** total recovery blocks ≤ 65 535
- [x] Wire into `poster.rs` replacing the previous linear correction loop
- [x] Unit tests: empty input, single article, single file, no redundancy,
      200% redundancy (recovery limit binding), mixed sizes, pathological
      case (more files than spec limit → best-effort)

### 24f — Memory-bounded multi-pass recovery computation ✅

When `recovery_count × slice_size` exceeds a configurable threshold (default:
1 GiB), the encoder splits recovery blocks into groups and makes multiple passes
over the input data rather than holding all recovery buffers in RAM simultaneously.
The tradeoff is extra I/O reads in exchange for bounded memory.

- [x] Split recovery blocks into groups of `floor(memory_limit / slice_size)` blocks
- [x] For each group: iterate over input files, feed all slices, call `finish()`,
      write the PAR2 volumes for that group, then free the buffers
- [x] Expose `[posting] par2_memory_limit` config key (default `"1 GiB"`,
      parseable with the same `parse_upload_rate`-style helper, e.g. `"512 MiB"`)
- [x] Emit a `ProgressEvent::Status` when multi-pass is triggered so the user
      understands the extra I/O passes

### 24g — RS encoder throughput: close the gap with parpar

Profiling against parpar (AVX2, i5-10400, 12 threads) shows pesto running at
~229 MB/s vs parpar's ~400 MB/s on a 5 GiB file. All four improvements below
attack different root causes of the ~1.75× gap.

#### 24g-1 — Parallelize `finish()` buffer conversion

`finish()` converts `Vec<Vec<u16>>` → `Vec<u8>` for each recovery buffer
sequentially. For 200 recovery blocks of ~2.6 MiB each (5 GiB file at 10%)
this is ~500 MiB of serial work after the RS compute has already finished.

- [ ] Replace the sequential iterator in `finish()` with `par_iter()` using
      rayon to convert all buffers in parallel
- [x] Measure wall-clock improvement on the 5 GiB benchmark (expected ~5–10%)
      — measured: negligible (<0.5%); conversion was never the bottleneck

#### 24g-2 — Pre-compute coefficient tables outside rayon closure

In `flush_avx2_work` (and SSSE3/GFNI equivalents), each of the 200 rayon tasks
independently builds a full `tables` vec of per-(recovery, input-slice)
coefficients, including 64 `gf.mul()` calls per entry to derive the 8 shuffle
vectors. This causes 200 × 128 × 64 = 1.6 M redundant GF multiplications per
flush (16 flushes for a 5 GiB file), with all threads contending on the same
128 KB log/antilog arrays.

- [x] Pre-compute a flat `all_tables` array `[recovery_count × queued_len]` of
      full SIMD table structs in a parallel pre-pass before the chunk loop
- [x] Apply the same refactor to `flush_avx512_gfni_work` (pending)
- [ ] Apply to `flush_ssse3_work`

#### 24g-3 — Input-major flush: read each slice once

Current loop order: outer = recovery buffer (rayon), middle = 32 KiB chunk,
inner = input slice. Each input slice is read `recovery_count` times from L3/RAM
because different rayon threads need different slices at different times.

Parpar's `Input pass(es): 1` confirms it reads the input exactly once. The fix
is to invert the outer two loops inside each rayon task so the input slice is
the outer dimension and the recovery block chunk is inner — or, better, to
restructure the flush so threads partition over input-slice batches and each
thread iterates over all its recovery blocks for that batch.

- [x] Move the chunk loop outside `par_iter_mut` so all threads rendezvous at
      each chunk boundary; input window (128 slices × 32 KiB = 4 MiB) stays L3-resident
- [x] Measured: 1G 3.6s → 2.9s (−20%), 5G 22.3s → 16.6s (−25%); gap vs parpar
      reduced from 1.75× to 1.31×
- [ ] Apply to `flush_ssse3_work` and `flush_avx512_gfni_work`

#### 24g-4 — Unroll 2–4 recovery buffers per inner loop iteration

Once input-major order is in place, each inner iteration loads one 32-byte (AVX2)
or 64-byte (GFNI) chunk of input data. Amortize that load over 2 or 4 recovery
buffers simultaneously: different coefficients, same input vector, 2–4 store
instructions — halves or quarters the load/store ratio.

- [x] Implement a 2× unrolled variant via `par_chunks_mut(2)`: each rayon task
      handles a pair of recovery blocks sharing one input load + nibble decomposition
- [x] Measured: 1G −1.8%, 5G −2.9%; modest gain due to register pressure (16 table
      vectors against 16 available YMM registers); gap vs parpar now 1.27×
- [ ] Apply to `flush_ssse3_work`; skip GFNI (already 2× wider than AVX2)

---

## Phase 20 — Future Ideas & Brainstorming (To Be Evaluated)

*A collection of concepts to improve resilience, extreme-environment performance, pipelining, visual feedback, and open-source composability. Kept here for future selection.*

### A. Extreme Environments & Resource Management
1. **Auto-Detect RAM limits:** Automatically cap buffer pools based on total system RAM to prevent OOM errors on low-memory machines.
2. **Dynamic Connection Scaling:** Reduce the number of active NNTP connections on-the-fly if memory pressure is high or TCP buffers fill up.
3. **CPU Topology Awareness:** Adjust the `rayon` thread pool dynamically based on available physical cores versus total logical CPUs.
4. **Disk Space Pre-flight:** Check if the temp directory has enough free space *before* starting heavy compression or PAR2 generation.
5. **In-Memory Mode:** For files smaller than available RAM, avoid writing temporary archives to disk completely (bypassing I/O bottlenecks).
6. **Direct I/O (`O_DIRECT`):** On Linux, bypass the OS page cache for huge files to prevent thrashing system memory.
7. **Memory Mapping (`mmap`):** Alternative fast-path for reading massive files using `madvise(MADV_SEQUENTIAL)`.
8. **Adaptive Buffering:** Grow or shrink `Shared::acquire_buffer` pools based on the delta between network upload speed and disk read speed.
9. **Disk Read Throttling:** Intentionally stall disk reads if the NNTP upload queue becomes saturated, saving memory.
10. **Single-Core Fallback:** Auto-detect environments like Raspberry Pi 1/Zero and switch to a fully sequential, low-overhead pipeline.

### B. Pipelined Processing & Archiving (Streaming)
11. **Pipelined Volume Streaming (The "RAR Volumes" Idea):** Stream archive volumes (`.part01.rar`) from the compressor directly into the NNTP upload queue as soon as each volume is flushed to disk, instead of waiting for the entire archive to finish.
12. **Native Streaming Compression:** Use pure Rust crates (`zip` or `sevenz-rust`) to compress on-the-fly directly in memory, feeding the NNTP workers without temporary files.
13. **On-the-fly TAR Bundling:** Bundle directories into a tar stream dynamically during the read pass, eliminating the need for a temporary archive step.
14. **Stdin Pipelining:** ✅ Implemented in Phase 23a. `-` is accepted as a file argument; data is buffered to a named temp file.
15. **Eager PAR2 Processing in Watch Mode:** In `--watch` mode, start hashing and computing PAR2 blocks as soon as a file is detected, before the upload queue is ready.
16. **Async Backpressure:** Ensure that the compression/PAR2 stages block properly if the network layer stalls, preventing buffer bloat.
17. **Chunked/Live Uploading:** Support infinite data streams (like live video), producing a continuous sequence of NZBs or a dynamically updating NZB.
18. **Progressive NZB Flushing:** Write the `.nzb` XML progressively to disk to save memory when uploading sets with millions of articles.
19. **Incremental State Saving:** Flush `.pesto-state` periodically during long uploads so that a crash loses absolutely minimal progress.
20. **Zero-Copy yEnc:** Optimize buffer handling to zero-copy levels using advanced scatter-gather I/O.

### C. Visual Feedback & Terminal UX
21. **Interactive TUI Mode:** A `ratatui`-based dashboard showing real-time graphs of upload speed, memory usage, and thread activity.
22. **Sparkline Metrics:** Add mini Unicode sparklines (e.g., ` ▂▃▅▆▇`) to the CLI output to show network throughput over the last 10 seconds.
23. **Buffer Pool Visualizer:** Display a small visual indicator of free vs. in-use memory buffers to show the health of the internal pipeline.
24. **Adaptive Refresh Rate:** Lower the terminal redraw rate dynamically when the CPU is bogged down, keeping resources focused on the upload.
25. **Color-Coded Status Matrix:** Show a grid representing NNTP worker states (🟢 Uploading, 🟡 Authenticating, 🔴 Retrying, ⚪ Idle).
26. **Confidence-Based ETA:** Display ETA as a range (e.g., `12-15 min`) or add a stability indicator if throughput is fluctuating heavily.
27. **Directory Tree Preview:** Print a clean `tree`-style breakdown of the payload during the pre-flight summary before uploading.
28. **Quiet / Minimal Mode:** A mode showing *only* a single spinning character and ETA, minimizing terminal pollution for tmux/screen users.
29. **Audible / ANSI Bell Notifications:** Optionally trigger a terminal bell (`\a`) on completion for users without desktop notification integrations.
30. **Smooth Progress Transitions:** Use sub-character block rendering (e.g., `▏▎▍▌▋▊▉█`) for ultra-smooth progress bars.

### D. Performance & Concurrency
31. **SIMD yEnc Acceleration:** Implement AVX2/NEON intrinsics for the yEnc encoding loop, pushing encoding speeds to memory-bandwidth limits.
32. **TCP `SO_RCVBUF`/`SO_SNDBUF` Tuning:** Auto-tune socket buffers for Long Fat Networks (LFNs) to maximize throughput over high-latency connections.
33. **Hardware-Accelerated CRC32:** Use `CRC32c` or ARM CRC instructions if supported by the CPU, falling back to software.
34. **GPU-Accelerated PAR2:** Experimental CUDA/Vulkan backend for computing PAR2 recovery data on massive files almost instantly.
35. **Connection Reuse Across Jobs:** In `--each` mode, keep the NNTP connection pool alive between files to skip TLS handshake overhead.
36. **NNTP Command Pipelining:** Send multiple `POST` commands back-to-back without waiting for the server's response, if the server supports it.
37. **Dynamic Worker Scaling:** Automatically spawn more NNTP connections mid-flight if throughput is under the network cap.
38. **Multi-Path TCP (MPTCP):** Bond multiple network interfaces (e.g., Wi-Fi + Ethernet) to aggregate upload bandwidth.
39. **NUMA-Aware Threading:** Pin Rayon threads to specific CPU cores on high-end servers to avoid cross-socket memory latency.
40. **TLS Session Resumption:** Utilize TLS session tickets across multiple connections to speed up the initial swarm connection phase.

### E. Resilience, Error Handling & Open-Source Best Practices
41. **Auto-Relocate Temp Storage:** If `/tmp` gets full, dynamically switch to `$HOME` or the output directory without failing the upload.
42. **Intelligent Network Backoff:** Implement fully jittered exponential backoff for NNTP server drops to avoid thundering-herd reconnects.
43. **Auto-Ban Failing Servers:** Temporarily ban an NNTP server from the pool during the run if it drops connections more than 5 times.
44. **Pre-flight NZB Validation:** Hash-check the generated NZB file against original files right before finishing to guarantee data integrity.
45. **Corrupt State Recovery:** Detect corrupted `.pesto-state` JSON files and automatically repair or fallback gracefully.
46. **OOM Graceful Exit:** Catch allocation failures (where supported) and write a clean crash-log instead of a hard abort.
47. **C-Compatible FFI:** Export a C-API so `pesto` can be linked directly into Python/Go/C++ applications without subprocess overhead.
48. **WebAssembly (WASM) Core:** Compile the yEnc/PAR2/NZB generation logic to WASM, allowing browser-based offline NZB generation.
49. **Pluggable Storage Backends:** Abstract `std::fs` to allow reading directly from AWS S3, MinIO, or HTTP streams.
50. **gRPC / Webhook Interceptor:** A granular hook system allowing external tools to modify metadata (like renaming the subject) *during* the run via RPC.
