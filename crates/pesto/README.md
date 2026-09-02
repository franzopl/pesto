
# Pesto

**Fast, lean Usenet poster written in Rust.**

[![CI](https://github.com/franzopl/pesto/actions/workflows/ci.yml/badge.svg)](https://github.com/franzopl/pesto/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/pesto-poster.svg)](https://crates.io/crates/pesto-poster)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Rust 1.87+](https://img.shields.io/badge/rust-1.87%2B-orange.svg)](https://www.rust-lang.org)

<img width="102" height="153" alt="5jPd0-removebg-preview (1)" src="https://github.com/user-attachments/assets/e61a0276-efc4-4fbd-8868-386021940618" />


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
- [SOCKS5 proxy](#socks5-proxy)
  - [Post a single file](#post-a-single-file)
  - [Post a directory](#post-a-directory)
  - [Multiple files](#multiple-files)
- [Obfuscation](#obfuscation)
- [Compression and passwords](#compression-and-passwords)
- [PAR2 recovery data](#par2-recovery-data)
- [Batch and watch modes](#batch-and-watch-modes)
- [Reliability](#reliability)
- [NZB metadata](#nzb-metadata)
- [All flags](#all-flags)
- [Exit codes](#exit-codes)
- [JSON output mode](#json-output-mode)
- [Performance](#performance)

---

## Installing

### Pre-built binary (recommended)

Download the latest binary for your platform from the
[GitHub Releases](https://github.com/franzopl/pesto/releases) page:

| Platform | File |
|----------|------|
| Linux x86-64 (glibc) | `pesto-linux-x86_64` |
| Linux x86-64 (musl / Alpine) | `pesto-linux-x86_64-musl` |
| Windows x86-64 | `pesto-windows-x86_64.exe` |

These are plain binaries, not archives — download and copy the file to a
directory on your `PATH` (e.g. `/usr/local/bin` on Linux, `C:\Windows\System32`
on Windows), renaming it to `pesto`/`pesto.exe` if you want the short name.
[`scripts/install.sh`](../../scripts/install.sh)/[`install.ps1`](../../scripts/install.ps1)
do this for you (`curl ... | bash` / `irm ... | iex` — see each script's header
for the one-liner).

### Via cargo

```bash
cargo install pesto-poster
```

The installed binary is named `pesto`.

### Updating

If you installed a prebuilt binary (not via `cargo install`), run:

```bash
pesto --update
```

This downloads the latest `pesto-v*` release for your platform, verifies its
SHA256 against the `SHA256SUMS` file published alongside it, and replaces the
running binary in place. It touches nothing else — no config, no NZBs.
`cargo install pesto-poster` installs are unaffected; update those the same
way you installed them (`cargo install pesto-poster` again).

### Build from source

---

## Build from source

Requires Rust **1.87 or newer** — install or update via <https://rustup.rs>.

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
proxy       = "socks5://127.0.0.1:1080" # optional; socks5h:// and bare host:port also work

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


### SOCKS5 proxy
Route every NNTP connection through a SOCKS5 proxy with `--proxy` / `-p`, or
set `proxy` at the top level or in `[server]`. Proxy credentials are supported,
and Pesto sends the NNTP hostname to SOCKS5 for remote DNS resolution.

```bash
# Local dynamic proxy via SSH
ssh -D 1080 user@jump-host
pesto -p 127.0.0.1:1080 movie.mkv

# Authenticated commercial proxy
pesto -p 'socks5://user:password@proxy.example:1080' movie.mkv
```

Before posting, Pesto verifies the SOCKS5 connection, proxy authentication, and
NNTP authentication. The live terminal panel keeps a dedicated `proxy` box
visible throughout the upload; it never prints proxy credentials. Add
`--proxy-check-ip` to show the public exit IP (it contacts `api.ipify.org`).

With more than one entry, `groups` is a pool of alternatives: one is picked
at random each run to spread posts across the pool over time — it does not
cross-post. To post the same article to several groups at once instead, join
their names with `+` in a single entry, e.g. `groups = ["alt.a+alt.b"]`. The
two can be mixed, e.g. `groups = ["alt.a+alt.b", "alt.c"]` picks between
"cross-post to a and b" or "post to c alone" each run. (`,` is accepted as a
deprecated alias for `+` within a `groups` array entry, with a warning —
prefer `+` in new configs.)

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

`--obfuscate` controls metadata visible on the wire; it does not encrypt
content or promise anonymity. See the workspace's authoritative
[`docs/obfuscation.md`](../../docs/obfuscation.md) for exact artifacts,
compatibility gates and non-goals.

| Mode | Subject | yEnc `name=` | `From` header | Real path in `.nzb` |
|------|---------|--------------|---------------|----------------------|
| `none` (default) | real name | real name | config value | yes |
| `full` | random, unique per article | opaque random, stable per physical file | random per article | yes |
| `full-shared` | shared prefix + real extension, same across the release | shared prefix + random suffix, per file | random, shared across the release | yes |
| `light` | shared prefix, same across the release | identical to Subject | random, shared across the release | yes |

`full` randomises Subject and From independently for every article, while a
physical file retains one opaque variable-length alphanumeric yEnc name for
client-safe multipart assembly. The real file names are only in the `.nzb` you
keep, or recoverable through the PAR2 set.

A bare `--obfuscate` (no value) means `full`.

```bash
# Private default — names do not identify files, but content is not encrypted
pesto --obfuscate movie.mkv
# same as:
pesto --obfuscate=full movie.mkv

# Add archive encryption to protect content too
pesto --obfuscate --password movie.mkv
```

### Shared-prefix mode

Under plain `full`, Subjects and senders are independently random per article,
and yEnc names only connect segments of one physical file. The archive and
its PAR2 volumes therefore have no release-wide wire identity. Some public
indexers rely on a shared base name to recognise that a PAR2 set belongs to a
given release, so under `full` those posts often show up unindexed or split
apart (see [issue #58]).

`--obfuscate=full-shared` fixes that by generating one random *prefix* per run
and reusing it — with the real extension (or archive volume suffix) kept — as
the **Subject** of every file: the archive (or loose input files) and every
PAR2 index/volume. The yEnc body `name=` carries that same prefix too, each
with its own random suffix (`{prefix}-{random}`) rather than repeating the
subject verbatim — an indexer that can only see the yEnc body (not the
Subject) still recognises every article as part of the release, while the
random suffix keeps the Subject and yEnc name from ever matching exactly
(the fingerprint plain `full` avoids by keeping them fully independent — see
[issue #106]). The real names still never touch the wire; only the
*grouping* changes.

```bash
pesto --obfuscate=full-shared movie.mkv
```

This is a distinct, explicit choice rather than the default for `full`: reusing
one name across the whole release is exactly the kind of correlation that the
legacy strict `article` mode (below) is designed to prevent, so pick `full-shared` only when
indexer compatibility matters more than resistance to wire-metadata
correlation.

[issue #58]: https://github.com/franzopl/pesto/issues/58
[issue #106]: https://github.com/franzopl/pesto/issues/106

### Legacy header-fragmented alias

`--obfuscate=header-fragmented` is accepted as a compatibility alias for
`--obfuscate=full`. It emits the same client-safe contract: each article has
an independent Subject and From while every physical file retains one opaque
yEnc `name=`. A body-aware observer can therefore group a physical file's
articles by its yEnc name.

```bash
pesto --obfuscate=header-fragmented movie.mkv
```

> **Legacy note:** `article` is hidden and experimental. It, and its
> `paranoid` alias, retain the old strict behavior where Subject, yEnc name and
> From all change per article. It is not compatible with conventional
> multipart PAR2 repair and cleanup.

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

### Volume-split archive

```bash
# Split into 1 GB volumes: stem.part01.rar, stem.part02.rar, ...
pesto --compress=rar --compress-volume-size=1g movie.mkv

# 7z volumes: stem.7z.001, stem.7z.002, ...
pesto --compress=7z --compress-volume-size=1g movie.mkv
```

`--compress-volume-size` takes a number with an optional unit (`b`/`k`/`m`/`g`/`t`),
e.g. `500m` or `4g`. Supported with `--compress=rar` and `--compress=7z`; rejected
with `--compress=zip` (7z's zip backend has no volume support).

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
# Full obfuscation and a random archive password
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

### Memory budget

Two flags, two scopes:

- **`--memory-limit <SIZE|PCT|auto>`** bounds the whole process — PAR2,
  uploads and the check queue together, as shares of one ceiling (PAR2 gets
  60%). Accepts an absolute size (`"8 GiB"`), a percentage of host RAM
  (`"70%"`), or `auto` (default): derive it from host RAM, any cgroup memory
  limit, and this process's own address-space ceiling (`RLIMIT_AS` /
  `ulimit -v`) — shared hosting and seedbox accounts commonly cap the latter
  well below host RAM, invisible to the first two.
- **`--par2-memory-limit <SIZE>`** bounds the PAR2 recovery-encoding pass
  specifically, on top of (not instead of) the share above — whichever is
  tighter wins. Most invocations only need `--memory-limit`; reach for this
  one when PAR2 specifically needs a different number than its default
  share, e.g. to deliberately force multiple passes on a very
  memory-constrained host.

A manually-set limit that doesn't fit safely inside the effective ceiling is
rejected up front with an actionable error, instead of the process aborting
partway through the upload with no explanation.

The startup banner reports the numbers behind the decision:

```
memory: address-space limit 9.5 GiB | reserved for overhead (connections+threads+runtime) 4.0 GiB | PAR2 budget 2.8 GiB/pass
```

When `--memory-limit` is set explicitly, the banner also names the global
ceiling it resolved to, so the two-flag split is never silent even though
migrating from the old single-flag behavior is strictly safer by
construction (a `--memory-limit` that used to mean "PAR2 may use this much"
now means "the whole process may use this much" — PAR2's actual share is
smaller, never larger).

When the recovery data needed exceeds this budget, PAR2 generation splits
into multiple passes — each one re-reads the source files from scratch. By
default those extra passes run concurrently with posting (data files go out
while later passes are still catching up), so on a memory-constrained host a
large release's PAR2 index/volumes can end up posted a while after its data
files. See `--par2-before-upload` below if that gap matters for how your
target indexer groups a release's files together.

### Generating PAR2 before posting

By default pesto computes PAR2 recovery data concurrently with the upload —
posting starts as soon as the first data article is ready, and PAR2 volumes
get posted as their recovery data finishes, interleaved with the rest of the
release. `--par2-before-upload` switches to a two-phase workflow instead,
closer to tools like ParPar+nyuu: generate every PAR2 file first (index and
volumes, nothing posted yet), then post the data files followed by the
already-generated PAR2 files, back to back with no gap between them.

```bash
pesto --par2-before-upload movie.mkv
```

This trades a longer wait before the first article goes out for a release
whose articles all land within a tight time window on the wire — useful if
you've observed an indexer failing to group a large release's PAR2 volumes
with its data files, which can happen when generation needs multiple passes
(see the memory budget note above) and the resulting gap outlasts however
long that indexer waits before considering a release's file set complete.

### SIMD acceleration

pesto selects the fastest available Reed-Solomon path at startup via runtime
CPU feature detection:

| Path | Requirement | Notes |
|------|------------|-------|
| GFNI + AVX-512 | AVX-512F + AVX-512BW + GFNI | Verified on Intel Ice Lake Xeon; enabled by default |
| GFNI + AVX2 | AVX2 + GFNI (Ice Lake+, Zen 4+) | Default fast path on modern x86-64 |
| AVX2 | AVX2 (Haswell+) | Fallback for CPUs without GFNI |
| SSSE3 | SSSE3 (Sandy Bridge+) | Covers nearly all x86-64 CPUs since 2007 |
| NEON | AArch64 | Apple Silicon, AWS Graviton, Ampere Altra |
| Scalar | any | Universal fallback |

The dispatch happens in `RecoveryEncoder::flush()` (`src/par2/encoder.rs`).
Measured throughput on an i5-14400 at 10 % redundancy, 256 MiB workload:

| Path | PAR2 encode speed |
|------|----------------:|
| Scalar | 317 MiB/s |
| SSSE3 | 597 MiB/s |
| AVX2 | 813 MiB/s |
| GFNI + AVX2 | ~1 991–2 348 MiB/s (internal bench) |

### yEnc encoding performance

pesto features a world-class yEnc encoder utilizing SIMD expansion tables
(`PSHUFB`) and direct pointer manipulation. It is designed to saturate the
memory bandwidth of modern CPUs.

Measured throughput on an Intel i5-10400 (line length 128):

| Tool | yEnc throughput |
|------|----------------:|
| **pesto** (v0.2.23) | **2 204 MB/s** |
| `nyuu` / `node-yencode` | 2 165 MB/s |

**Benchmarking vs node-yencode**:

```bash
cargo build --release --example yenc-bench
./bench_pesto_yenc_vs_node.sh
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

### `--merge-season` — combine per-episode NZBs offline

If a folder was posted with `--each` and you need a combined season NZB after
the fact, use `--merge-season`. No server connection is required.

```bash
# Read all .nzb files in the directory, group by season, write one combined NZB per group
pesto --merge-season ./nzb/uploaded/

# Override the display name in the NZB <head>
pesto --merge-season ./nzb/uploaded/ --nzb-title "Batwheels Season 2"
```

Files are grouped by their season identifier (`S01`, `S02`, …). Each group
produces one output NZB named after the group key (e.g. `Batwheels.S02.nzb`)
written beside the source files. The terminal prints each included episode with
its file and segment counts.

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

### `--ext` — restrict uploads to specific extensions

```bash
# Only post .mkv files: a subtitle sitting loose next to the video no longer
# becomes its own release under --each/--watch, and a nested .srt is dropped
# from an episode's upload instead of being bundled in
pesto --each --ext mkv ./Season01/
pesto --watch ./incoming/ --each --ext mkv

# Comma-separate to allow more than one extension
pesto --each --ext mkv,mp4 ./Season01/
```

`--ext` is a no-op by default (every file is included). It's most useful with
`--each`/`--season`/`--watch`, where a downloaded release folder often mixes
the video with subtitles, samples, or other extras you don't want posted as
their own release or bundled into one.

---

## Reliability

### Interrupting an upload

Press Ctrl-C (or send SIGTERM) once to stop queueing new articles and finish
the articles already in flight. Press it again to abort immediately: pesto
drops active NNTP connections, writes the partial NZB and `.pesto-state`, and
exits 130. If graceful shutdown is still waiting after about 10 seconds, it
escalates automatically. In both cases, run again with `--resume` to skip
confirmed segments.

### Upload resume

If a posting run is interrupted (Ctrl-C, network failure, articles still
missing after every automatic retry, etc.), `pesto` can pick up where it left
off instead of re-posting everything from scratch.

Progress is tracked automatically for every run. If a run ends incomplete, that
progress is saved to a `.pesto-state` sidecar file next to the `.nzb`; if it
completes successfully, any state file is deleted — there is nothing left to
resume from a finished upload. `--resume` controls the other half: whether a
*prior* run's saved state is actually loaded and its already-posted segments
skipped. Without it, `pesto` always starts fresh, even if a `.pesto-state` file
is sitting right there.

```bash
pesto --resume movie.mkv
```

Or enable it permanently in config.toml:
```toml
[output]
resume = true
```

When posting finishes but only a handful of articles fail the post-check,
`pesto` already retries them automatically in the same run before giving up
(see `--check-post-retries` and `--check-recover-max` below) — `--resume` is
for what that can't cover: a run interrupted outright, or one where too many
articles failed to justify an automatic retry. When a run does end that way,
the printed error includes a ready-to-run retry command with the original
`--article-size`/`--obfuscate`/`--par2`/`--compress` values already filled in.

**Safety.** A saved state is only trusted if it was recorded under the same
posting parameters (`--article-size`, `--obfuscate`, `--compress`, `--par2`)
and, per file, the same size and modification time as what `--resume` sees
now. Any mismatch — different parameters, or a file that changed since the
state was recorded — is discarded rather than partially trusted, so a
mismatched retry never corrupts the `.nzb`; it just re-posts as if `--resume`
had not found anything.

**Compressed uploads (`--compress`).** The archive is rebuilt from scratch on
every run, so it always looks "changed" to the per-file check above and its
segments can't be skipped on `--resume` — the archive's *content* is not
resumable today. What does carry over: the obfuscated name/identity used to
build it (reused instead of regenerated, so at least the file doesn't change
identity every retry), and any article that was sent but never got a
confirmed response (see below). The same applies to PAR2 recovery volumes for
segments an interrupted run never reached — they're regenerated, not resumed.
A plain, uncompressed upload gets full data-level resume; a compressed one
mainly gets a safe, fast "no" instead of a slow re-post pretending to be a
skip.

**Sent but unconfirmed.** A segment whose article was sent but whose server
acknowledgement never arrived (e.g. the connection dropped between `POST` and
reading `240`) is cached in a `.pesto-spool` sidecar directory as soon as it's
encoded, before it goes over the wire. On `--resume`, that exact article is
replayed under its original `Message-ID` instead of being re-encoded and
posted under a new one — avoiding a duplicate article if the original `POST`
had, in fact, gone through.

### Post-verification via STAT

```bash
# After posting each article, confirm with STAT that the server registered it
pesto --verify movie.mkv
```

Failed STAT checks trigger automatic reposts. Off by default because it adds
one round-trip per article.

### Post-upload check and repost

```bash
# After the whole upload finishes, STAT every article and repost any that are missing
pesto --check movie.mkv

# Give a flaky provider more chances: 3 repost rounds, each followed by a fresh STAT pass
pesto --check --check-post-retries 3 movie.mkv
```

`--check` waits `--check-delay` seconds (default `30`) for propagation, then
STATs every posted article. Anything missing is reposted under its original
`Message-ID` and re-verified; `--check-post-retries` controls how many
repost-then-verify rounds to try (default `1`) before giving up — some
providers only make an article STAT-findable after receiving it more than
once.

If articles are still confirmed missing after every round, `pesto` refuses to
write the `.nzb` and skips post-upload hooks — it never ships a release it
couldn't confirm is fully retrievable. Pass `--allow-incomplete-nzb` to
publish anyway (e.g. when PAR2 recovery is expected to cover the gap); the
process still exits non-zero so scripts and hooks can tell the upload wasn't
fully clean.

Once every `--check-post-retries` round is exhausted, `pesto` makes one more
automatic repost-and-verify attempt for whatever is still missing, as long as
that's cheap — at most `--check-recover-max` articles (default `50`) and
within `--check-recover-percent` of the release's total segments (default
`15`), whichever cap is smaller. This resolves the common case of "posting
finished, the check failed for a handful of articles" without requiring a
separate `--resume` invocation. Set `--check-recover-max 0` to disable it and
fall back to `--allow-incomplete-nzb`/`--resume` only.

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

## NZB metadata

### Custom NZB metadata

```bash
# Set the display name shown in NZBGet / SABnzbd
pesto --nzb-title "My Movie (2024)" movie.mkv

# Set a category and extraction password
pesto --nzb-category "Movies" --nzb-password "archive_pass" movie.mkv

# Add multiple tags (repeat --nzb-tag for each one)
pesto --nzb-tag hd --nzb-tag 2024 --nzb-tag dts movie.mkv
```

These values are written as `<meta>` elements in the `.nzb`:

```xml
<meta type="title">My Movie (2024)</meta>
<meta type="category">Movies</meta>
<meta type="password">archive_pass</meta>
<meta type="tag">hd</meta>
<meta type="tag">2024</meta>
<meta type="tag">dts</meta>
```

`--nzb-title` maps to `<meta type="title">` — SABnzbd's documented meta type for a
human-readable NZB name; plain `<meta type="name">` isn't part of the NZB 1.1 spec.
`--nzb-name` is a deprecated alias, still accepted with a warning.

`--nzb-tag` can be repeated; each occurrence produces one `<meta type="tag">`.
If `--nzb-tag` is used on the command line, it replaces any `nzb_tags` set in
`config.toml`. When `--obfuscate` is active, pesto also adds its own
`<meta type="tag">obfuscated:<mode></meta>` (e.g. `obfuscated:full`) automatically,
so an indexer can tell an obfuscated release apart from a plain one without
inspecting article headers.

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

## Hooks

pesto supports two hook points: **pre-upload** (runs before anything is posted,
can abort the upload) and **post-upload** (runs after a successful upload).

`--no-hooks` disables only the executable scripts found in `~/.config/pesto/hooks/`;
explicit `--pre-hook` and `--post-hook` commands are unaffected. This lets you
run a single explicit hook without triggering every directory script.

To make this the permanent default instead of passing `--no-hooks` on every
run, set it once in `config.toml`:

```toml
[output]
no_hooks = true
```

### Pre-upload hook

A pre-upload hook runs **before compression, PAR2 generation, and NNTP
connection**. If the command exits with a non-zero code the upload is aborted
immediately — nothing is posted and no state is written.

**Use case:** query NZBHydra2 or Prowlarr to check for duplicates before
uploading.

There are two ways to register a pre-upload hook:

- **`config.toml`** — runs for every upload:
  ```toml
  [output]
  pre_hook = "~/.config/pesto/hooks/check-duplicate.sh"
  ```
- **`~/.config/pesto/pre-hooks/` directory** — every executable in this
  directory is run in alphabetical order before the upload:
  ```bash
  chmod +x ~/.config/pesto/pre-hooks/check-duplicate.sh
  ```
- **`--pre-hook <CMD>`** — one-off command for a single run:
  ```bash
  pesto --pre-hook '~/.config/pesto/hooks/check-duplicate.sh' movie.mkv
  ```

`--no-hooks` suppresses the `pre-hooks/` directory scripts. The `--pre-hook`
flag and `output.pre_hook` config value are **not** affected — they always run.
Pre-hooks are never run during `--dry-run`.

Environment variables available to the pre-hook:

| Variable | Description |
|----------|-------------|
| `PESTO_NAME` | Release name / entry label |
| `PESTO_BYTES` | Total size in bytes of all input files (decimal string) |
| `PESTO_INPUT_PATHS` | Colon-separated list of input file/directory paths |
| `PESTO_SERVER` | NNTP server hostname |
| `PESTO_GROUP` | First configured newsgroup |
| `PESTO_GROUPS` | Colon-separated list of all configured newsgroups |
| `PESTO_CATEGORY` | Value of `--nzb-category` (empty when not set) |
| `PESTO_NZB_TITLE` | Value of `--nzb-title` (empty when not set) |
| `PESTO_NZB_NAME` | Deprecated alias of `PESTO_NZB_TITLE`, same value |
| `PESTO_OBFUSCATE` | Obfuscation mode in use: `none`, `light`, `full-shared`, `full`, or legacy `article` |
| `PESTO_PAR2` | PAR2 redundancy percentage (e.g. `10`) |
| `PESTO_TAGS` | Space-separated list of NZB tags (empty when none) |
| `PESTO_TMDB_ID` | Value of `--tmdb` / `--tmdb-id` (empty when not set) |
| `PESTO_IMDB_ID` | Value of `--imdb-id` / `--imdb` (empty when not set) |
| `PESTO_TVDB_ID` | Value of `--tvdb-id` / `--tvdb` (empty when not set) |
| `PESTO_MAL_ID` | Value of `--mal-id` / `--mal` (empty when not set) |

> `PESTO_NZB`, `PESTO_NFO`, and `PESTO_PASSWORD` are **not** available in the
> pre-hook — the NZB and NFO don't exist yet, and the archive password is only
> resolved after compression.

### Post-upload hooks

Any executable script placed in `~/.config/pesto/hooks/` is run automatically
after each successful upload, in alphabetical order. Each script receives the
following environment variables:

| Variable | Description |
|----------|-------------|
| `PESTO_NZB` | Absolute path to the generated `.nzb` file |
| `PESTO_NFO` | Absolute path to the `.nfo` file (empty when `--nfo` was not used) |
| `PESTO_NAME` | Release name / entry label |
| `PESTO_BYTES` | Total bytes posted (decimal string) |
| `PESTO_INPUT_PATHS` | Colon-separated list of input file/directory paths |
| `PESTO_SERVER` | NNTP server hostname |
| `PESTO_GROUP` | First Usenet newsgroup |
| `PESTO_GROUPS` | Colon-separated list of all configured newsgroups |
| `PESTO_PASSWORD` | Archive password (empty when none) |
| `PESTO_CATEGORY` | Value of `--nzb-category` (empty when not set) |
| `PESTO_NZB_TITLE` | Value of `--nzb-title` (empty when not set) |
| `PESTO_NZB_NAME` | Deprecated alias of `PESTO_NZB_TITLE`, same value |
| `PESTO_OBFUSCATE` | Obfuscation mode in use: `none`, `light`, `full-shared`, `full`, or legacy `article` |
| `PESTO_PAR2` | PAR2 redundancy percentage (e.g. `10`) |
| `PESTO_TAGS` | Space-separated list of NZB tags (empty when none) |
| `PESTO_TMDB_ID` | Value of `--tmdb` / `--tmdb-id` (empty when not set) |
| `PESTO_IMDB_ID` | Value of `--imdb-id` / `--imdb` (empty when not set) |
| `PESTO_TVDB_ID` | Value of `--tvdb-id` / `--tvdb` (empty when not set) |
| `PESTO_MAL_ID` | Value of `--mal-id` / `--mal` (empty when not set) |
| `PESTO_WIRE_SUBJECT` | The actual `Subject:` header sent to the NNTP server for the first posted file — differs from the real filename under `--obfuscate` (empty when nothing was posted) |

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

NFO generation is a local operation — it works with `--dry-run` just as it
does in a full upload run.

```bash
pesto --nfo movie.mkv
pesto --dry-run --nfo movie.mkv   # generate NFO without touching the network
```

### Bundled examples

The [`examples/hooks/`](examples/hooks/) directory contains ready-to-use hook
scripts:

| Script | Type | Platform | Description |
|--------|------|----------|-------------|
| [`print-vars.sh`](examples/hooks/print-vars.sh) | Post-upload | Unix | Prints all `PESTO_*` variables — useful as a starting point or for debugging |
| [`generic-indexer.sh`](examples/hooks/generic-indexer.sh) | Post-upload | Unix | Sends the NZB (and optional NFO) to any Newznab-compatible indexer via its REST API |
| [`generic-indexer.bat`](examples/hooks/generic-indexer.bat) | Post-upload | Windows | Same as above — `.bat` version for `cmd.exe` |
| [`generic-indexer.ps1`](examples/hooks/generic-indexer.ps1) | Post-upload | Windows | Same as above — PowerShell version with native JSON parsing (recommended on Windows) |
| [`different-indexer.sh`](examples/hooks/different-indexer.sh) | Post-upload | Unix | Sends the NZB (and optional NFO) to an indexer that takes the API key as a query parameter and replies with a JSON `guid` |
| [`different-indexer.ps1`](examples/hooks/different-indexer.ps1) | Post-upload | Windows | Same as above — PowerShell version |
| [`newznab-dedup.sh`](examples/hooks/newznab-dedup.sh) | Pre-upload | Unix | Aborts the upload if a release with the same name already exists on a Newznab indexer (fail-open on network errors) |
| [`newznab-dedup.ps1`](examples/hooks/newznab-dedup.ps1) | Pre-upload | Windows | Same as above — PowerShell version |

To install a **post-upload** hook on Unix:

```bash
cp examples/hooks/generic-indexer.sh ~/.config/pesto/hooks/
chmod +x ~/.config/pesto/hooks/generic-indexer.sh
# edit API_KEY inside the file
```

`newznab-dedup.sh`/`.ps1` are **pre-upload** hooks instead — install them to
`~/.config/pesto/pre-hooks/` (Unix) or `%APPDATA%\pesto\pre-hooks\` (Windows),
not the `hooks/` directory above. See [Pre-upload hook](#pre-upload-hook) for
details.

To install a hook on Windows, copy the `.bat` or `.ps1` file to `%APPDATA%\pesto\hooks\` and edit the variables at the top of the file. For the PowerShell version, set `post_hook` in `config.toml`:

```toml
post_hook = "powershell -ExecutionPolicy Bypass -File \"%APPDATA%\\pesto\\hooks\\generic-indexer.ps1\""
```

`.ps1` scripts run via `pwsh` (PowerShell 7+) when it is on `PATH`, falling back
to the built-in `powershell` (Windows PowerShell 5.1) otherwise. If you write
your own `.ps1` hooks using syntax that only exists in PowerShell 6+, install
[PowerShell 7](https://github.com/PowerShell/PowerShell/releases) to have it
picked up automatically — no config change needed.

---

## All flags

| Flag | Config key | Default | Description |
|------|-----------|---------|-------------|
| `-c`, `--config [PATH]` | — | auto | Load a TOML config; with no value, run the setup wizard |
| `--update` | — | — | Download and install the latest release binary for this platform, then exit |
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
| `--groups <G,...>` | `posting.groups` | — | Newsgroups; a pool to pick one from at random per run, or join with `+` in one entry to cross-post to all of them |
| `--article-size <BYTES>` | `posting.article_size` | `768000` | Target segment size in bytes |
| `--line-length <CHARS>` | `posting.line_length` | `128` | yEnc encoded line length |
| `--retries <N>` | `posting.retries` | `3` | Post attempts per segment |
| `--obfuscate[=MODE]` | `posting.obfuscate` | `none` | `none`, `light`, `full-shared`, `full`; `header-fragmented` is a compatibility alias; bare flag = `full` (`article` hidden/experimental) |
| `--date <VALUE>` | `posting.date` | server-supplied | `now`, deprecated `random` (last 2 h), or an RFC 2822 timestamp |
| `--no-archive` | `posting.no_archive` | off | Add `X-No-Archive: yes` to every article |
| `--message-id-domain <D>` | `posting.message_id_domain` | random | Fixed domain for `Message-ID` headers |
| `--pipeline-depth <N>` | `posting.pipeline_depth` | `0` | Articles to pipeline per connection (`0` = adaptive) |
| `--stdin-name <NAME>` | — | — | Filename for stdin (`-`) input |
| **Reliability** | | | |
| `--par2 <PERCENT>` | `posting.par2` | `10` | PAR2 recovery percentage (0 = off) |
| `--par2-only` | — | off | Write PAR2 files only; do not post |
| `--par2-before-upload` | `posting.par2_before_upload` | off | Generate all PAR2 recovery data before posting anything, instead of concurrently with the upload; posts data files then the PAR2 index/volumes back to back |
| `--dry-run` | — | off | Encode only; never touch the network |
| `--resume` | `output.resume` | off | Load a prior run's `.pesto-state` file and skip already-posted segments |
| `--slice-size <SIZE>` | — | auto | Manual PAR2 slice size (e.g. `"1 MiB"`) |
| `--slice-count <N>` | — | auto | Target number of PAR2 input slices |
| `--recovery-count <N>` | — | auto | Exact number of PAR2 recovery blocks |
| `--memory-limit <SIZE\|PCT\|auto>` | `posting.memory_limit` | `auto` | Global memory budget for the whole process (PAR2/upload/check share it) |
| `--par2-memory-limit <SIZE>` | `posting.par2_memory_limit` | `"1 GiB"` | Max RAM for PAR2 recovery buffers specifically |
| `--threads <N>` | — | auto | Threads for PAR2 compute (`0` = physical cores) |
| `--simd <MODE>` | — | auto | Force SIMD: `auto`, `avx2-gfni`, `avx2`, `ssse3`, `scalar` |
| `--verify` | `posting.verify` | off | Confirm each article with STAT |
| `--check` | `posting.check` | off | Run a STAT pass over all articles after upload |
| `--check-delay <SECS>` | `posting.check_delay` | `30` | Seconds to wait before STAT pass; implies `--check` |
| `--check-retries <N>` | `posting.check_retries` | `3` | STAT attempts per article during check pass |
| `--check-connections <N>` | `posting.check_connections` | same as upload | Parallel connections for STAT pass |
| `--check-post-retries <N>` | `posting.check_post_retries` | `1` | Repost-then-verify rounds for articles still missing after `--check` |
| `--allow-incomplete-nzb` | `posting.allow_incomplete_nzb` | off | Write the `.nzb` and run hooks even if some articles are still confirmed missing after `--check-post-retries` |
| `--check-recover-percent <N>` | `posting.check_recover_percent` | `15` | Skip the automatic final recovery pass below if still-missing articles exceed this percent of the release |
| `--check-recover-max <N>` | `posting.check_recover_max` | `50` | After `--check-post-retries` is exhausted, automatically retry once more if at most this many articles (and within `--check-recover-percent`) are still missing; `0` disables |
| `--rate <RATE>` | `posting.upload_rate` | unlimited | Max upload rate (e.g. `"50 MiB/s"`) |
| **Compression** | | | |
| `--compress [FORMAT]` | `compression.format` | off | Bundle into an archive (`7z`, `zip`, `rar`) |
| `--compress-temp-dir <DIR>` | `compression.temp_dir` | OS temp dir | Where the `--compress` archive is staged before posting |
| `--compress-volume-size <SIZE>` | `compression.volume_size` | off | Split the archive into volumes (e.g. `500m`, `4g`); `rar`/`7z` only, rejected with `--compress=zip` |
| `--password [PASSWORD]` | — | — | Archive password; bare flag = random |
| **Output** | | | |
| `-o`, `--out <PATH>` | `output.nzb` | derived | Explicit `.nzb` output path |
| `--nzb-dir <DIR>` | `output.nzb_dir` | — | Directory where `.nzb` files are saved |
| `--nzb-title <NAME>` | `output.nzb_title` | — | `<meta type="title">` in the `.nzb` |
| `--nzb-name <NAME>` (deprecated) | `output.nzb_name` | — | Alias of `--nzb-title`; prints a deprecation warning |
| `--nzb-password <PASS>` | `output.nzb_password` | — | `<meta type="password">` in the `.nzb` |
| `--nzb-category <CAT>` | `output.nzb_category` | — | `<meta type="category">` in the `.nzb` |
| `--nzb-tag <TAG>` | `output.nzb_tags` | — | `<meta type="tag">` in the `.nzb`; repeatable. Replaces config `nzb_tags` when used. |
| `--nzb-conflict <MODE>` | `output.nzb_conflict` | overwrite | `overwrite`, `rename`, or `fail` on existing NZB |
| `--no-overwrite` | — | — | Alias for `--nzb-conflict=rename` |
| `-v`, `--verbose` | — | off | Increase log verbosity (`-v`=INFO, `-vv`=DEBUG, `-vvv`=TRACE) |
| `--log-file <FILE>` | — | — | Redirect verbose logs to file (requires `-v`) |
| `--nfo` / `--no-nfo` | `output.nfo` | off | Generate a `.nfo` file alongside the `.nzb` |
| `--pre-hook <CMD>` | `output.pre_hook` | — | Shell command run before upload; non-zero exit aborts |
| `--post-hook <CMD>` | `output.post_hook` | — | Shell command run after each successful upload |
| `--history` / `--no-history` | `output.history` | on | Write a record to the upload history log |
| `--notify` / `--no-notify` | — | on | Send completion notification (webhook / ntfy) |
| `-q`, `--quiet` | `output.quiet` | off | Single-line minimal output (no panel) |
| `--bell` | `output.bell` | off | Write ASCII BEL to stderr on completion |
| `--output-format <FORMAT>` | — | `terminal` | `terminal` or `json` |
| **Batch / watch** | | | |
| `--each` | — | off | Post each top-level entry as its own release |
| `--season` | — | off | Like `--each`, plus a consolidated season `.nzb` |
| `--merge-season <DIR>` | — | — | Merge per-episode NZBs in DIR into season NZBs (offline) |
| `--jobs <N>` | — | `1` | Parallel uploads for `--each`/`--season` (0 = CPU count) |
| `--watch <DIR>` | — | — | Watch a directory and post new entries automatically |
| `--watch-done <DIR>` | — | delete | Move completed watch entries here instead of deleting |
| `--watch-interval <SECS>` | — | `30` | Poll interval for `--watch` |
| `--ext <EXT[,EXT...]>` | — | off | Only post files with these extensions (case-insensitive); drops non-matching top-level entries and files nested inside a directory |

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
follows once in-flight segments complete. A second signal (or the ten-second
graceful-shutdown deadline) emits `aborted` before `finished`.

```json
{"type":"interrupted"}
```

#### `aborted`

Emitted when pesto drops in-flight NNTP I/O during an escalated interrupt. The
partial resume state is saved before the following `finished` event.

```json
{"type":"aborted"}
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

## Performance

Release-validation medians on an i5-10400 (6 physical cores, AVX2), governor
`performance`, three repetitions and eight mock-NNTP connections:

| workload | scenario | pesto | competitor | result |
|---|---|---:|---:|---:|
| movie-1080p | post-only, 0 ms | **2226.1 MiB/s** | Nyuu 1339.4 MiB/s | **1.66x** |
| movie-1080p | post-only, 30 ms | **92.2 MiB/s** | Nyuu 92.0 MiB/s | **1.00x** |
| movie-1080p | full two-phase, 0 ms | 265.5 MiB/s | ParPar+Nyuu 283.4 MiB/s | 6.3% gap |
| movie-1080p | full streaming, 30 ms | **82.8 MiB/s** | ParPar+Nyuu two-phase 68.2 MiB/s | **1.21x** |
| many-small | post-only, 0 ms | **1760.6 MiB/s** | Nyuu 505.6 MiB/s | **3.48x** |

Nyuu was 0.4.2 and ParPar 0.4.5. At 30 ms the pure-posting row is
latency-limited; Pesto's default streaming pipeline overlaps PAR2 creation
with upload, while `--par2-before-upload` provides the like-for-like
two-phase comparison.

On c7i.2xlarge (4 physical cores, AVX-512+GFNI), five measured repetitions
after one excluded warmup, Parmesan created the 6 GiB movie recovery set at
**553.2 MiB/s** against ParPar 0.4.6 at 577.8 MiB/s (4.3% gap), while
`many-small` reached **464.3 vs 234.9 MiB/s (1.98x)**. All nine official
cross-tool correctness checks passed.

### Reproduce on your machine

```bash
cargo build --release
./bench/run.sh --list
./bench/run.sh par2 e2e correctness \
  --workload many-small --workload movie-1080p \
  --scale 1.0 --reps 3 --latencies 0,30 --yes
```

See [`bench/README.md`](../../bench/README.md) for the complete methodology,
hardware fingerprints, competitor versions, raw data, and limitations.

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
