# pesto

**Fast, lean Usenet poster written in Rust.**

[![CI](https://github.com/franzopl/pesto/actions/workflows/ci.yml/badge.svg)](https://github.com/franzopl/pesto/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Rust 1.75+](https://img.shields.io/badge/rust-1.75%2B-orange.svg)](https://www.rust-lang.org)

yEnc-encodes files, posts them over parallel NNTP connections, generates a `.nzb`,
and stays out of your way. Inspired by [`nyuu`](https://github.com/animetosho/Nyuu),
with a deliberately minimal scope: just the essentials, executed extremely fast.

---

## Contents

- [Installing](#installing)
- [Build from source](#build-from-source)
- [Quick start](#quick-start)
- [Configuration](#configuration)
- [Basic usage](#basic-usage)
  - [Post a single file](#post-a-single-file)
  - [Post a directory](#post-a-directory)
  - [Multiple files](#multiple-files)
- [Obfuscation](#obfuscation)
- [Compression and passwords](#compression-and-passwords)
- [PAR2 recovery data](#par2-recovery-data)
- [Batch and watch modes](#batch-and-watch-modes)
- [Reliability](#reliability)
- [NZB metadata and indexer upload](#nzb-metadata-and-indexer-upload)
- [All flags](#all-flags)
- [Exit codes](#exit-codes)
- [JSON output mode](#json-output-mode)

---

## Installing

### Pre-built binary (recommended)

Download the latest binary for your platform from the
[GitHub Releases](https://github.com/franzopl/pesto/releases) page:

| Platform | File |
|----------|------|
| Linux x86-64 (glibc) | `pesto-x86_64-unknown-linux-gnu.tar.gz` |
| Linux x86-64 (musl / Alpine) | `pesto-x86_64-unknown-linux-musl.tar.gz` |
| Windows x86-64 | `pesto-x86_64-pc-windows-msvc.zip` |

Extract the archive and copy the binary to a directory on your `PATH`
(e.g. `/usr/local/bin` on Linux/macOS, `C:\Windows\System32` on Windows).

### Build from source

---

## Build from source

Requires Rust **1.75 or newer** — install or update via <https://rustup.rs>.

```bash
cargo build --release
```

The binary is written to `target/release/pesto`. Copy it anywhere on your `PATH`.

---

## Prerequisites

`pesto` itself has no mandatory runtime dependencies — the Rust binary is
self-contained. Some features require external tools:

| Feature | Tool required | Install |
|---------|--------------|---------|
| `--compress` (7z / zip) | `p7zip` | `apt install p7zip-full` · `brew install p7zip` · [7-zip.org](https://www.7-zip.org) |
| `--compress=rar` | `rar` | [rarlab.com/download.htm](https://www.rarlab.com/download.htm) (not redistributable) |
| `--nfo` (video metadata) | `mediainfo` | `apt install mediainfo` · `brew install media-info` · [mediaarea.net](https://mediaarea.net/en/MediaInfo) |

`pesto` will print a clear error if a required tool is missing. `mediainfo` is
optional and its absence degrades gracefully — `--nfo` falls back to a
directory listing instead.

---

## Quick start

```bash
# 1. Create the config file (runs a short interactive wizard)
pesto --config

# 2. Post a file — that's it
pesto movie.mkv
```

The wizard writes `~/.config/pesto/config.toml` (or `$XDG_CONFIG_HOME/pesto/config.toml`).
`pesto` loads it automatically on every subsequent run, so you only need to configure
the server once. See [`config.example.toml`](config.example.toml) for all available options.

---

## Configuration

### Config file

```toml
[server]
host        = "news.example.com"
port        = 563          # default; 119 for plaintext
ssl         = true         # default
connections = 10           # parallel NNTP connections

[auth]
username = "your_username"
password = "your_password"

[posting]
groups  = ["alt.binaries.test"]
par2    = 10               # % of PAR2 recovery data (0 = disabled)
# from omitted → random identity per run

[output]
nzb_dir = "/home/user/nzbs"   # where .nzb files are saved
```

Any config field can be overridden by a CLI flag for a single run.

### Multiple servers with automatic failover

```toml
[[servers]]
host        = "news.primary.com"
port        = 563
ssl         = true
connections = 20
username    = "user1"
password    = "pass1"

[[servers]]
host        = "news.fallback.com"
port        = 563
ssl         = true
connections = 10
username    = "user2"
password    = "pass2"
```

When `[[servers]]` is present, `[server]` and `[auth]` are ignored. Connections
that fail automatically retry on the next server in the list.

---

## Basic usage

### Post a single file

```bash
pesto movie.mkv
```

`pesto` loads the default config, opens 10 parallel TLS connections (or however
many you configured), and streams the file as yEnc-encoded articles. When done
it prints a summary and writes `movie.nzb` next to the binary (or in
`output.nzb_dir` if set in the config).

### Post a directory

```bash
pesto ./MyShow.S01/
```

The directory is walked recursively. Every file is posted as part of one logical
upload, with the folder structure preserved in the `.nzb` and PAR2 metadata so
a downloader can reconstruct the original layout. Files starting with `.` are
included; symbolic links are skipped. The `.nzb` is named after the root folder
(`MyShow.S01.nzb`).

### Multiple files

```bash
pesto --out upload.nzb file1.mkv file2.mkv extras/bonus.mkv
```

All files are grouped into a single `.nzb`. The `--out` flag sets an explicit
output path; without it the name is derived from the first argument.

### Without a config file

All settings can be passed as flags:

```bash
pesto \
  --host news.example.com \
  --username alice --auth-password secret \
  --groups alt.binaries.test \
  --connections 20 \
  --out upload.nzb \
  movie.mkv
```

---

## Obfuscation

`--obfuscate` controls what appears on the wire. Nothing prevents Usenet indexers
from cataloguing plain posts; obfuscation hides the content from them.

| Mode | Subject header | yEnc `name=` field | Real path in `.nzb` |
|------|---------------|---------------------|----------------------|
| `none` (default) | real name | real name | yes |
| `subject` | random UUID | real name | yes |
| `full` | random UUID | random UUID | yes |

`full` hides everything on the wire. The real file names are only in the `.nzb`
you keep, or recoverable through the PAR2 set.

A bare `--obfuscate` (no value) means `full`.

```bash
# Hide the subject only — indexers cannot catalogue it; download clients still
# see the real file name from the yEnc header
pesto --obfuscate=subject movie.mkv

# Full obfuscation — nothing on the wire reveals the content
pesto --obfuscate movie.mkv
# same as:
pesto --obfuscate=full movie.mkv

# Combine with compression for maximum privacy
pesto --obfuscate --password movie.mkv
```

---

## Compression and passwords

`--compress` bundles all input files into a single archive before encoding and
uploading. The archive is created in a temporary directory and deleted after posting.

### Supported formats

| Format | Flag | Notes |
|--------|------|-------|
| 7z (default) | `--compress` or `--compress=7z` | Store mode (no recompression); with password: encrypts headers too |
| ZIP | `--compress=zip` | Standard ZIP; password does not encrypt file names |
| RAR | `--compress=rar` | Requires `rar` binary in `PATH`; with password: header encryption |

### Open archive (no password)

```bash
# Default format (7z, store mode)
pesto --compress movie.mkv

# Explicit format
pesto --compress=zip movie.mkv
pesto --compress=rar movie.mkv
```

### Password-protected archive

```bash
# Random 24-character password — printed to stdout and embedded in the .nzb
pesto --password movie.mkv

# Explicit password
pesto --password=MySecret42 movie.mkv

# RAR with password (requires rar in PATH)
pesto --compress=rar --password=MySecret42 movie.mkv
```

When `--password` is used, the password is stored in `<meta type="password">`
inside the `.nzb` so that NZBGet and SABnzbd can extract automatically.

### Combined: obfuscation + password

```bash
# Full obfuscation and a random archive password (maximum privacy)
pesto --obfuscate --password movie.mkv

# Same, but explicit password and a directory input
pesto --obfuscate=full --password=MySecret42 ./MyShow.S01/
```

---

## PAR2 recovery data

pesto generates PAR2 parity files using its own pure-Rust implementation.
Parity is computed in the same single read pass as posting, so it adds minimal
overhead. The PAR2 files are uploaded alongside the data and referenced in the `.nzb`.

```bash
# 10% recovery data (default when par2 is set in config)
pesto movie.mkv

# Explicit percentage
pesto --par2 15 movie.mkv

# Disable PAR2 for this run
pesto --par2 0 movie.mkv

# Generate PAR2 files next to the source without posting
pesto --par2-only movie.mkv
pesto --par2-only ./MyShow.S01/
```

---

## Batch and watch modes

### `--each` — post each entry as a separate upload

```bash
# Post each top-level item in a directory as its own release with its own .nzb
pesto --each ./Season01/

# Run up to 4 uploads in parallel
pesto --each --jobs 4 ./Season01/
```

### `--season` — batch with a combined season NZB

```bash
# Post each episode independently AND produce one consolidated Season01.nzb
pesto --season ./Season01/

# Parallel posting, 2 jobs at a time
pesto --season --jobs 2 ./Season01/
```

### `--watch` — daemon mode

```bash
# Watch a folder and post every new entry automatically (Ctrl-C / SIGTERM to stop)
pesto --watch ./incoming/

# Move completed entries to a done folder instead of deleting them
pesto --watch ./incoming/ --watch-done ./done/

# Post up to 3 entries in parallel with a 60-second poll interval
pesto --watch ./incoming/ --jobs 3 --watch-interval 60
```

Entries already present in the watched directory when `pesto` starts are ignored;
only new arrivals are posted. Completed entries are moved to `--watch-done` or
deleted if `--watch-done` is not set.

---

## Reliability

### Upload resume

If a posting run is interrupted (Ctrl-C, network failure, etc.), `pesto` saves
state to a `.pesto-state` sidecar file next to the `.nzb`. On the next run with
the same output path, already-posted articles are skipped and their `Message-ID`s
are reused, so the final `.nzb` is complete and correct.

```bash
# Disable resume — ignore any existing state and start from scratch
pesto --no-resume movie.mkv
```

### Post-verification via STAT

```bash
# After posting each article, confirm with STAT that the server registered it
pesto --verify movie.mkv
```

Failed STAT checks trigger automatic reposts. Off by default because it adds
one round-trip per article.

### Rate limiting

```bash
# Limit total upload speed to 50 MiB/s across all connections
pesto --rate "50 MiB/s" movie.mkv

# Accepted units: B, KB/KiB, MB/MiB, GB/GiB (all case-insensitive)
pesto --rate "10 MB/s" movie.mkv
```

### Dry run

```bash
# Encode everything and measure performance — never touch the network
pesto --dry-run movie.mkv
pesto --dry-run --par2 15 ./MyShow.S01/
```

---

## NZB metadata and indexer upload

### Custom NZB metadata

```bash
# Set the display name shown in NZBGet / SABnzbd
pesto --nzb-name "My Movie (2024)" movie.mkv

# Set a category and extraction password
pesto --nzb-category "Movies" --nzb-password "archive_pass" movie.mkv
```

These values are written as `<meta>` elements in the `.nzb`:

```xml
<meta type="name">My Movie (2024)</meta>
<meta type="category">Movies</meta>
<meta type="password">archive_pass</meta>
```

### Automatic upload to a Newznab indexer

Add this to your config:

```toml
[output.indexer]
url      = "https://my.indexer.com"
api_key  = "abc123"
category = "5000"   # optional Newznab category ID
```

`pesto` will POST the `.nzb` to the indexer after every successful upload.
Skip it for a single run with:

```bash
pesto --no-upload movie.mkv
```

### NZB output path

By default the `.nzb` (and `.nfo` when `--nfo` is enabled) are saved in the
current working directory, named after the uploaded file or folder.

Use `--nzb-dir` or `output.nzb_dir` to redirect all output files to a fixed
directory. `~` is expanded to the home directory.

```bash
# Explicit path for a single run
pesto --out /nzbs/movie.nzb movie.mkv

# Fixed output directory via flag
pesto --nzb-dir ~/nzb/pesto movie.mkv

# Fixed output directory via config (recommended)
# ~/.config/pesto/config.toml
# [output]
# nzb_dir = "~/nzb/pesto"
# nfo     = true
```

With the config above, `pesto arquivo.mkv` saves `~/nzb/pesto/arquivo.nzb`
and `~/nzb/pesto/arquivo.nfo` on every run without any extra flags.

---

## Post-upload hooks

Any executable script placed in `~/.config/pesto/hooks/` is run automatically
after each successful upload, in alphabetical order. Each script receives the
following environment variables:

| Variable | Description |
|----------|-------------|
| `PESTO_NZB` | Absolute path to the generated `.nzb` file |
| `PESTO_NFO` | Absolute path to the `.nfo` file (empty when `--nfo` was not used) |
| `PESTO_NAME` | Release name / entry label |
| `PESTO_BYTES` | Total bytes posted (decimal string) |
| `PESTO_GROUP` | First Usenet newsgroup |
| `PESTO_PASSWORD` | Archive password (empty when none) |
| `PESTO_SERVER` | NNTP server hostname |

Scripts must have the executable bit set on Unix (`chmod +x`). On Windows,
files with `.exe`, `.cmd`, `.bat`, `.ps1`, or `.py` extensions are recognised
automatically.

A hook that exits non-zero is logged and skipped; the remaining hooks still
run. Hooks are suppressed for `--par2-only`, `--dry-run`, and failed uploads.

You can also run a one-off command for a single invocation with `--post-hook`:

```bash
pesto --post-hook 'notify-send "pesto" "Upload done: $PESTO_NAME"' movie.mkv
```

### NFO generation

Pass `--nfo` to generate a `.nfo` text file alongside the `.nzb`. pesto runs
`mediainfo` on the first video file it finds; for generic folders it falls back
to a recursive directory listing. The path is exposed as `PESTO_NFO` to every
hook script.

```bash
pesto --nfo movie.mkv
```

### Bundled examples

The [`examples/hooks/`](examples/hooks/) directory contains ready-to-use hook
scripts:

| Script | Description |
|--------|-------------|
| [`print-vars.sh`](examples/hooks/print-vars.sh) | Prints all `PESTO_*` variables — useful as a starting point or for debugging |
| [`curupira.sh`](examples/hooks/curupira.sh) | Uploads the `.nzb` (and optional `.nfo`) to [Curupira.cc](https://curupira.cc) via its REST API |

To install a hook:

```bash
cp examples/hooks/curupira.sh ~/.config/pesto/hooks/
chmod +x ~/.config/pesto/hooks/curupira.sh
# edit API_KEY inside the file
```

---

## All flags

| Flag | Config key | Default | Description |
|------|-----------|---------|-------------|
| `-c`, `--config [PATH]` | — | auto | Load a TOML config; with no value, run the setup wizard |
| **Connection** | | | |
| `--host <HOST>` | `server.host` | — | NNTP server hostname |
| `--port <PORT>` | `server.port` | `563` | NNTP server port |
| `--no-ssl` | `server.ssl` | TLS on | Disable TLS (plaintext) |
| `--connections <N>` | `server.connections` | `4` | Parallel NNTP connections |
| `--retry-delay <SECS>` | `server.retry_delay` | `1` | Seconds between retries |
| `--username <USER>` | `auth.username` | — | NNTP username |
| `--auth-password <PASS>` | `auth.password` | — | NNTP password |
| **Posting** | | | |
| `--from <ADDRESS>` | `posting.from` | random | `From` header (omit = random per run) |
| `--groups <G,...>` | `posting.groups` | — | Newsgroups, comma-separated |
| `--article-size <BYTES>` | `posting.article_size` | `768000` | Target segment size in bytes |
| `--line-length <CHARS>` | `posting.line_length` | `128` | yEnc encoded line length |
| `--retries <N>` | `posting.retries` | `3` | Post attempts per segment |
| `--obfuscate[=MODE]` | `posting.obfuscate` | `none` | `none`, `subject`, or `full`; bare flag = `full` |
| `--date <VALUE>` | `posting.date` | server-supplied | `now`, `random`, or an RFC 2822 timestamp |
| `--no-archive` | `posting.no_archive` | off | Add `X-No-Archive: yes` to every article |
| `--message-id-domain <D>` | `posting.message_id_domain` | random | Fixed domain for `Message-ID` headers |
| **Reliability** | | | |
| `--par2 <PERCENT>` | `posting.par2` | `10` | PAR2 recovery percentage (0 = off) |
| `--par2-only` | — | off | Write PAR2 files only; do not post |
| `--dry-run` | — | off | Encode only; never touch the network |
| `--no-resume` | — | off | Ignore existing state; start fresh |
| `--verify` | `posting.verify` | off | Confirm each article with STAT |
| `--rate <RATE>` | `posting.upload_rate` | unlimited | Max upload rate (e.g. `"50 MiB/s"`) |
| **Compression** | | | |
| `--compress [FORMAT]` | `compression.format` | off | Bundle into an archive (`7z`, `zip`, `rar`) |
| `--password [PASSWORD]` | — | — | Archive password; bare flag = random |
| **Output** | | | |
| `-o`, `--out <PATH>` | `output.nzb` | derived | Explicit `.nzb` output path |
| `--nzb-dir <DIR>` | `output.nzb_dir` | — | Directory where `.nzb` files are saved |
| `--nzb-name <NAME>` | `output.nzb_name` | — | `<meta type="name">` in the `.nzb` |
| `--nzb-password <PASS>` | `output.nzb_password` | — | `<meta type="password">` in the `.nzb` |
| `--nzb-category <CAT>` | `output.nzb_category` | — | `<meta type="category">` in the `.nzb` |
| `--nfo` / `--no-nfo` | `output.nfo` | off | Generate a `.nfo` file alongside the `.nzb` |
| `--post-hook <CMD>` | `output.post_hook` | — | Shell command run after each successful upload |
| `--history` / `--no-history` | `output.history` | on | Write a record to the upload history log |
| `--no-upload` | — | off | Skip automatic indexer upload this run |
| `--notify` / `--no-notify` | — | on | Send completion notification (webhook / ntfy) |
| `-q`, `--quiet` | `output.quiet` | off | Single-line minimal output (no panel) |
| `--bell` | `output.bell` | off | Write ASCII BEL to stderr on completion |
| `--output-format <FORMAT>` | — | `terminal` | `terminal` or `json` |
| **Batch / watch** | | | |
| `--each` | — | off | Post each top-level entry as its own release |
| `--season` | — | off | Like `--each`, plus a consolidated season `.nzb` |
| `--jobs <N>` | — | `1` | Parallel uploads for `--each`/`--season` (0 = CPU count) |
| `--watch <DIR>` | — | — | Watch a directory and post new entries automatically |
| `--watch-done <DIR>` | — | delete | Move completed watch entries here instead of deleting |
| `--watch-interval <SECS>` | `watch.poll_interval` | `30` | Poll interval for `--watch` |

---

## Exit codes

| Code | Meaning |
|------|---------|
| `0` | All segments posted successfully |
| `1` | One or more segments failed |
| `130` | Interrupted by Ctrl-C |

On Ctrl-C, `pesto` stops queuing new segments, lets in-flight ones finish, and
still writes a `.nzb` for everything that was posted.

---

## JSON output mode

`--output-format json` switches from the interactive terminal panel to
newline-delimited JSON events on stdout. Intended for scripting and integration
with tools like `upapasta`.

```bash
pesto --output-format json movie.mkv
```

All diagnostic messages go to stderr; stdout carries only the event stream, so
it is safe to pipe or redirect without filtering.

### Event reference

Every event is a JSON object on a single line. The `type` field identifies it.

#### `started`

Emitted once at the beginning of the run.

```json
{"type":"started","total_files":2,"total_bytes":4294967296,"total_segments":5590,"connections":10,"target":"news.example.com:563"}
```

| Field | Type | Description |
|-------|------|-------------|
| `total_files` | integer | Number of input files (including PAR2 estimate) |
| `total_bytes` | integer | Sum of raw input bytes |
| `total_segments` | integer | Total number of yEnc segments to post |
| `connections` | integer | Number of NNTP worker connections |
| `target` | string \| null | `host:port` of the NNTP server; `null` for `--par2-only` |

#### `segment_done`

Emitted after each segment is posted (or skipped via resume).

```json
{"type":"segment_done","file":"movie.mkv","bytes":768000,"ok":true,"done_segments":1,"total_segments":5590,"done_bytes":768000,"total_bytes":4294967296,"progress_pct":0.0}
```

| Field | Type | Description |
|-------|------|-------------|
| `file` | string | Relative path of the file this segment belongs to |
| `bytes` | integer | Raw payload size of this segment in bytes |
| `ok` | boolean | `false` if the segment failed every retry |
| `done_segments` | integer | Running total of completed segments |
| `total_segments` | integer | Total segments in the run |
| `done_bytes` | integer | Running total of completed bytes |
| `total_bytes` | integer | Total bytes in the run |
| `progress_pct` | float | Overall completion percentage (0–100) |

#### `queue_extended`

Emitted when PAR2 files are appended to the work queue (after the data pass
computes parity). Updates `total_segments` and `total_bytes` upwards.

```json
{"type":"queue_extended","file":"movie.mkv.vol0+1.par2","segments":12,"bytes":9216000,"total_segments":5602,"total_bytes":4303183296}
```

| Field | Type | Description |
|-------|------|-------------|
| `file` | string | PAR2 file being added |
| `segments` | integer | Segments added for this file |
| `bytes` | integer | Bytes added for this file |
| `total_segments` | integer | Updated total segments |
| `total_bytes` | integer | Updated total bytes |

#### `status`

A short human-readable note from the poster (e.g. "computing PAR2"). An empty
string clears the current status.

```json
{"type":"status","text":"computing PAR2 recovery data"}
```

#### `failed`

A segment failed permanently after exhausting all retries.

```json
{"type":"failed","description":"segment 42 of movie.mkv: 441 Posting not allowed"}
```

#### `interrupted`

Emitted when Ctrl-C is received. The run is winding down; a `finished` event
follows once in-flight segments complete.

```json
{"type":"interrupted"}
```

#### `compress_started`

Archive creation has begun.

```json
{"type":"compress_started","total_bytes":4294967296}
```

#### `compress_progress`

Archive file on disk has grown (polled approximately every 200 ms).

```json
{"type":"compress_progress","bytes_written":134217728}
```

#### `compress_done`

Archive is complete and ready for posting.

```json
{"type":"compress_done"}
```

#### `par2_write_started`

PAR2 recovery volume writing has started.

```json
{"type":"par2_write_started","total":64}
```

`total` is the number of PAR2 recovery slices that will be written.

#### `par2_slice_written`

One PAR2 recovery slice has been written to disk. Emitted `total` times after
`par2_write_started`.

```json
{"type":"par2_slice_written"}
```

#### `finished`

Always the last event. The run is complete.

```json
{"type":"finished","segments":5590,"failures":0,"progress_pct":100.0,"ok":true}
```

| Field | Type | Description |
|-------|------|-------------|
| `segments` | integer | Total segments processed |
| `failures` | integer | Segments that failed permanently |
| `progress_pct` | float | Final completion percentage |
| `ok` | boolean | `true` if all segments succeeded |

#### `nzb_written`

Printed by `pesto` after `finished`, once the `.nzb` file has been written to
disk. Not part of the internal event stream — always the very last line.

```json
{"type":"nzb_written","path":"/home/user/nzbs/movie.nzb"}
```

---

## Development

```bash
cargo test                  # unit + integration tests
cargo clippy -- -D warnings
cargo fmt
```

See [`ROADMAP.md`](ROADMAP.md) for the full feature history and what comes next.

---

## License

MIT
