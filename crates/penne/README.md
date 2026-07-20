# Penne

**Fast NZB downloader for Usenet, written in Rust.**

Companion to [`pesto`](../pesto) (which posts) and
[`parmesan`](../parmesan) (which handles PAR2). Reads a `.nzb`, fetches its
articles over parallel NNTP connections, reassembles the original files,
verifies/repairs them with PAR2, extracts any `.rar`/`.7z`/`.zip` it finds,
and — if asked — cleans up the compressed volumes and PAR2 recovery data
afterward. `--mode` picks how far down that pipeline a run goes; see
[Processing modes](#processing-modes) below.

> **Status:** the core pipeline is complete and tested end-to-end — fetch,
> yEnc decode, assembly, PAR2 verify/repair, archive extraction, resume, and
> retry/backoff, all with real N-connection concurrency per server. See
> [`ROADMAP.md`](ROADMAP.md) for what's still open (mainly packaging/release
> and a couple of documented performance follow-ups) and for the full
> phase-by-phase history.

## Quick start

```bash
# Create the config interactively (see Configuration below).
cargo run --bin penne -- --config

# Download, assemble, PAR2-verify/repair, and extract.
cargo run --bin penne -- download path/to/release.nzb
```

`penne download` fetches every file the `.nzb` lists, assembles them,
verifies/repairs with PAR2 if recovery data was included, and extracts any
archive it finds — printing a per-file status line for each step. It exits
non-zero if anything is still incomplete or damaged once PAR2 has had its
chance to fix it.

## Configuration

Server credentials live in a TOML file. `penne --config` (no value) launches
a guided wizard that writes one for you at the default location; skip
straight to a manual example below if you'd rather write it by hand.

### Default location

When `--config <FILE>` isn't given, `penne` loads (and the wizard writes to)
the OS-standard path:

| OS | Path |
|----|------|
| Linux/macOS | `$XDG_CONFIG_HOME/penne/config.toml`, falling back to `~/.config/penne/config.toml` |
| Windows | `%APPDATA%\penne\config.toml` |

If nothing exists there, `penne download` fails with a clear message telling
you to run `penne --config` or pass `--config <FILE>` explicitly — it never
silently proceeds with no servers.

### `--config` forms

```bash
penne --config              # interactive wizard; writes to the default path above
penne download FILE.nzb     # loads the default path automatically
penne download FILE.nzb --config custom.toml   # loads a specific file instead
```

`--config` is a global flag — it works before or after the subcommand.

### File format

```toml
# Where completed, assembled files are written. Overridden by --out-dir.
download_dir = "/downloads"

# Optional: default `connections` for any [[servers]] entry below that
# doesn't set its own. Falls back to 8 if omitted entirely.
connections = 8

# Optional: retry attempts per segment against one server before moving on
# to the next configured server. Default: 3.
retries = 3

# Optional: default processing mode for `penne download` when --mode isn't
# given on the command line (--mode always overrides this per run). One of:
#   "download" - fetch and assemble only; no PAR2 verify/repair, no extraction
#   "repair"   - download, plus PAR2 verify/repair when recovery data is present
#   "unpack"   - repair, plus extracting any .rar/.7z/.zip found (built-in default)
#   "delete"   - unpack, plus deleting the compressed volumes and PAR2 recovery
#                data once extraction succeeds, leaving only the release's other files
# See Processing modes below for the full picture. Default: "unpack".
mode = "unpack"

[[servers]]
host = "news.example.com"
# port = 563          # 563 = TLS, 119 = plaintext. Default: 563 if ssl, else 119.
ssl = true
username = "user"
password = "pass"
connections = 8        # Parallel NNTP connections to this server.
retry_delay = 1         # Seconds between retry attempts. Default: 1.

# A second [[servers]] entry is a backup provider: only asked about
# segments the first one didn't have, never raced against it.
[[servers]]
host = "backup.example.com"
ssl = true
username = "user2"
password = "pass2"
connections = 4
```

At least one `[[servers]]` entry is required. Servers are tried strictly in
the order they're listed — the first is primary, the rest are backup
providers consulted only for segments the primary didn't have. This is the
same `[[servers]]` shape `pesto` uses (see the root
[`config.example.toml`](../../config.example.toml)) minus posting-only
fields, so a combined config file can share the block between the two tools
if you use both.

**Pooling equal-priority servers with `group`:** two *adjacent*
`[[servers]]` entries sharing the same `group` value are drained together
as one combined worker pool instead of one strictly finishing before the
next starts — for two equal-priority accounts (e.g. two blocks of
connections on the same provider, or two mirror providers) that should
share load rather than act as primary/backup:

```toml
[[servers]]
host = "account-a.example.com"
group = 1
connections = 10

[[servers]]
host = "account-b.example.com"
group = 1
connections = 10

# Not in group 1, and not adjacent to it either way: its own tier, tried
# only once both pooled servers above have been asked.
[[servers]]
host = "backup.example.com"
```

Omit `group` (the default) to keep a server as its own solitary priority
tier — unaffected, and how every `[[servers]]` entry behaves without this
field. Servers sharing a `group` value that *aren't* adjacent in the file
each get their own tier instead of being pooled — list group members next
to each other.

**Naming a server for `--server`:** give any `[[servers]]` entry a `name` to
pick it out for a single run instead of drawing on every configured server:

```toml
[[servers]]
name = "blocknews"
host = "usnews.blocknews.net"
ssl = true
username = "user"
password = "pass"

[[servers]]
name = "newshosting"
host = "news.newshosting.com"
ssl = true
username = "user2"
password = "pass2"
```

```bash
# Use only the "blocknews" entry for this run.
cargo run --bin penne -- download path/to/release.nzb --stat --server blocknews

# Repeat --server to pick more than one; they keep their relative order
# from the config file (so failover/group semantics are unaffected).
cargo run --bin penne -- download path/to/release.nzb --server blocknews --server newshosting
```

Omitting `--server` uses every configured server, exactly as before this
flag existed. Requesting a name that no entry has errors out immediately,
listing the names that do exist.

**Keeping an account out of the automatic mix with `explicit_only`:** an
entry with `explicit_only = true` is skipped whenever `--server` is
omitted, and only ever used when named directly. For a block/quota account
that must never be drawn on as a silent fallback:

```toml
[[servers]]
name = "blocknews"
host = "usnews.blocknews.net"
ssl = true
username = "user"
password = "pass"
explicit_only = true
```

```bash
# Plain `penne download`/`--stat` never touches "blocknews" — only "main"
# (or whatever other non-explicit_only servers are configured) is used.
cargo run --bin penne -- download path/to/release.nzb --stat

# Only this run uses it, because it's named explicitly.
cargo run --bin penne -- download path/to/release.nzb --stat --server blocknews
```

`explicit_only` requires `name` — otherwise there'd be no way to ever
select the entry, and `penne` refuses to load the config.

## Usage

```bash
# Parse a .nzb and print file/segment/size counts — no network I/O.
cargo run --bin penne -- info path/to/release.nzb

# Download, assemble, deobfuscate, PAR2-verify/repair, and extract.
# --out-dir defaults to the config's download_dir; --password overrides
# the .nzb's own embedded password. Both optional.
cargo run --bin penne -- download path/to/release.nzb \
    --out-dir ./downloads \
    --password hunter2

# Just check whether every segment is still on the server — no download,
# no disk writes. Exits non-zero if anything is missing.
cargo run --bin penne -- download path/to/release.nzb --stat
```

### What `download` does, in order

1. **Fetch** every segment the `.nzb` lists, with up to `connections`
   parallel connections per server. A segment already cached from a
   previous, interrupted run is never re-fetched (see Resume below).
2. **Decode** each fetched article body (yEnc) and cache the raw bytes for
   resume.
3. **Assemble** each file from its decoded segments. A file missing any
   segment is left unwritten entirely — a partial file that looks complete
   is worse than none.
4. **De-obfuscate**: obfuscated releases (common for scene/P2P posts) hide
   real filenames behind random hashes, in both the `.nzb` subject and the
   downloaded file names. `penne` content-sniffs for PAR2 packets regardless
   of extension, tags them `.par2`, and matches every other file against the
   PAR2 recovery set's real names by size + hash. Whatever PAR2 doesn't
   cover (or when there's no PAR2 at all) gets a best-effort guess from
   archive magic bytes (`.rar`/`.7z`/`.zip`) plus `.nzb` file order — clearly
   reported as a guess, distinct from a PAR2-confirmed recovery.
5. **PAR2 verify/repair** (`--mode repair` or higher), if any `.par2` file
   is present among the downloaded files (including ones just tagged in
   step 4): files left unwritten in step 3 can be recreated *whole* from
   recovery data; files with a bad checksum are patched at just the
   damaged parts.
6. **Extract** (`--mode unpack` or higher, the default) any
   `.rar`/`.7z`/`.zip` found (including multi-volume sets), using
   `--password` if given, else the `.nzb`'s own embedded password.
7. **Clean up** (`--mode delete` only): once extraction succeeds, delete
   every compressed volume and `.par2` file, leaving only the release's
   other files (the extracted media, subtitles, `.nfo`, etc.).

At `--mode repair` or higher, anything still incomplete or damaged after
step 5 makes `penne download` exit non-zero and report which files. Below
that (`--mode download`), a missing/damaged file only prints a warning —
the run still succeeds, and the resume cache (see below) is kept instead
of cleared, so a later `--mode repair` run can pick up without refetching.

### Processing modes

`--mode` picks how far down the pipeline above a run goes, mirroring
`sabnzbd`'s per-category Download/+Repair/+Unpack/+Delete processing
levels — each mode does everything the previous one does, plus one more
step:

| `--mode`   | Fetch/assemble | PAR2 verify/repair | Extract | Delete archives + PAR2 |
|------------|:--:|:--:|:--:|:--:|
| `download` | ✓ |    |    |    |
| `repair`   | ✓ | ✓  |    |    |
| `unpack` (default) | ✓ | ✓ | ✓ |    |
| `delete`   | ✓ | ✓ | ✓ | ✓ |

```bash
# Just fetch and assemble — no PAR2, no extraction.
cargo run --bin penne -- download path/to/release.nzb --mode download

# Fetch, verify/repair, and once everything's intact, drop the archives
# and PAR2 recovery data, keeping only the release's actual content.
cargo run --bin penne -- download path/to/release.nzb --mode delete
```

Precedence: `--mode` on the command line wins when given; otherwise the
config file's `mode` (see File format above) is used; if neither is set,
`penne` falls back to `unpack`, unchanged from before this config field
existed. Set `mode` in the config file once to change your everyday
default (e.g. to `download` if you routinely handle PAR2/extraction with
other tools) without typing `--mode` on every run.

### `--stat`: check availability without downloading

`penne download <nzb> --stat` runs only a completeness check: it `STAT`s
(RFC 3977 §6.2.4) every segment against the configured server(s) — a small
existence check, not an article transfer — and reports which files are
complete and which are missing segments, without fetching, decoding,
writing, or extracting anything. Exits non-zero if anything is missing, so
it's useful to script ahead of a real download (e.g. skip a release that's
already expired off the indexer's server). `STAT` commands are pipelined
(several queued and sent per round trip, not one-request-one-response) on
top of `connections`' usual concurrency, so a check's wall time scales with
round-trip latency far less than a naive implementation would. A live
progress bar (segments checked, not bytes/speed — nothing is ever fetched)
tracks the check on an interactive terminal, same as `download`'s own
panel. A concise summary
closes the run, leading with the percentage of articles actually present —
the number that matters most at a glance — plus how many bytes the check
itself used (KiB/MiB, not the release's size, proof of just how cheap
`STAT` is next to a real download):

```
checking 6968 segment(s) across 24 file(s)...
  complete: movie.mkv (200/200 segments)
  ...

summary
  articles present: 6968/6968 (100.0%)
  files complete:   24/24
  data used:        218.7 KiB (STAT only — no article data downloaded)
```

### Progress

While fetching, `penne download` draws a live panel on stderr — an overall
progress bar, download speed, ETA, and one bar per file currently
downloading (capped so a release with many volumes doesn't flood the
terminal) — instead of sitting silent until the whole queue is done, which
would otherwise look like a hang on a large release. Redirected output
(not a terminal) falls back to one plain status line per whole percentage
point instead.

### Resume

An interrupted `penne download` run doesn't start over: every successfully
fetched article body is cached under `<out-dir>/.penne-cache/`, keyed by
Message-ID. Re-running the same command against the same `.nzb`/`--out-dir`
skips the network entirely for anything already cached. The cache is deleted
automatically once a run completes with nothing left incomplete or
damaged — at `--mode download`, that only happens if the fetch itself was
already fully clean; otherwise it's kept so a later `--mode repair` (or
higher) run can still use it.

## Roadmap

See [`ROADMAP.md`](ROADMAP.md). A web UI (à la SABnzbd) is planned as a
separate crate built on top of `penne` once the CLI/engine reach feature
parity with a real downloader — not before.
