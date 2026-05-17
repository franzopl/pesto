# pesto

**Fast, lean Usenet poster written in Rust.**

yEnc-encodes files, posts them over parallel NNTP connections, generates a `.nzb`,
and stays out of your way. Inspired by [`nyuu`](https://github.com/animetosho/Nyuu),
with a deliberately minimal scope: just the essentials, executed extremely fast.

---

## Contents

- [Build](#build)
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

## Build

Requires the Rust toolchain — install once from <https://rustup.rs>.

```bash
cargo build --release
```

The binary is written to `target/release/pesto`. Copy it anywhere on your `PATH`.

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

```bash
# Explicit path
pesto --out /nzbs/movie.nzb movie.mkv

# Save in a directory; filename derived from the upload name
pesto --nzb-dir /nzbs/ movie.mkv
```

---

## All flags

| Flag | Config key | Default | Description |
|------|-----------|---------|-------------|
| `-c`, `--config [PATH]` | — | auto | Load a TOML config; with no value, run the setup wizard |
| `--host <HOST>` | `server.host` | — | NNTP server hostname |
| `--port <PORT>` | `server.port` | `563` | NNTP server port |
| `--no-ssl` | `server.ssl` | TLS on | Disable TLS (plaintext) |
| `--connections <N>` | `server.connections` | `4` | Parallel NNTP connections |
| `--retry-delay <SECS>` | `server.retry_delay` | `1` | Seconds between retries |
| `--username <USER>` | `auth.username` | — | NNTP username |
| `--auth-password <PASS>` | `auth.password` | — | NNTP password |
| `--from <ADDRESS>` | `posting.from` | random | `From` header (omit = random per run) |
| `--groups <G,...>` | `posting.groups` | — | Newsgroups, comma-separated |
| `--article-size <BYTES>` | `posting.article_size` | `768000` | Target segment size in bytes |
| `--line-length <CHARS>` | `posting.line_length` | `128` | yEnc encoded line length |
| `--retries <N>` | `posting.retries` | `3` | Post attempts per segment |
| `--obfuscate[=MODE]` | `posting.obfuscate` | `none` | `none`, `subject`, or `full`; bare flag = `full` |
| `--par2 <PERCENT>` | `posting.par2` | `10` | PAR2 recovery percentage (0 = off) |
| `--par2-only` | — | off | Write PAR2 files only; do not post |
| `--dry-run` | — | off | Encode only; never touch the network |
| `--no-resume` | — | off | Ignore existing state; start fresh |
| `--verify` | `posting.verify` | off | Confirm each article with STAT |
| `--rate <RATE>` | `posting.upload_rate` | unlimited | Max upload rate (e.g. `"50 MiB/s"`) |
| `--compress [FORMAT]` | `compression.format` | off | Bundle into an archive (`7z`, `zip`, `rar`) |
| `--password [PASSWORD]` | — | — | Archive password; bare flag = random |
| `-o`, `--out <PATH>` | `output.nzb` | derived | Explicit `.nzb` output path |
| `--nzb-dir <DIR>` | `output.nzb_dir` | — | Directory where `.nzb` files are saved |
| `--nzb-name <NAME>` | `output.nzb_name` | — | `<meta type="name">` in the `.nzb` |
| `--nzb-password <PASS>` | `output.nzb_password` | — | `<meta type="password">` in the `.nzb` |
| `--nzb-category <CAT>` | `output.nzb_category` | — | `<meta type="category">` in the `.nzb` |
| `--no-upload` | — | off | Skip automatic indexer upload this run |
| `--each` | — | off | Post each top-level entry as its own release |
| `--season` | — | off | Like `--each`, plus a consolidated season `.nzb` |
| `--jobs <N>` | — | `1` | Parallel uploads for `--each`/`--season` (0 = CPU count) |
| `--watch <DIR>` | — | — | Watch a directory and post new entries automatically |
| `--watch-done <DIR>` | — | delete | Move completed watch entries here instead of deleting |
| `--watch-interval <SECS>` | `watch.poll_interval` | `30` | Poll interval for `--watch` |
| `--output-format <FORMAT>` | — | `terminal` | `terminal` or `json` |

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

Sample event stream:

```json
{"type":"started","files":1,"total_bytes":4294967296}
{"type":"progress","posted_bytes":134217728,"total_bytes":4294967296,"rate_bps":52428800}
{"type":"progress","posted_bytes":268435456,"total_bytes":4294967296,"rate_bps":53477376}
{"type":"done","segments":5590,"failures":0}
{"type":"nzb_written","path":"movie.nzb"}
```

All diagnostic messages continue to go to stderr so stdout is clean for parsing.

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
