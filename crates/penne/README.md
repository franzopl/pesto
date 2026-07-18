# Penne

**Fast NZB downloader for Usenet, written in Rust.**

Companion to [`pesto`](../pesto) (which posts) and
[`parmesan`](../parmesan) (which handles PAR2). Reads a `.nzb`, fetches its
articles over parallel NNTP connections, reassembles the original files,
verifies/repairs them with PAR2, and extracts any `.rar`/`.7z`/`.zip` it
finds.

> **Status:** the core pipeline is complete and tested end-to-end — fetch,
> yEnc decode, assembly, PAR2 verify/repair, archive extraction, resume, and
> retry/backoff, all with real N-connection concurrency per server. See
> [`ROADMAP.md`](ROADMAP.md) for what's still open (mainly packaging/release
> and a couple of documented performance follow-ups) and for the full
> phase-by-phase history.

## Quick start

1. Write a config file with at least one server (see
   [Configuration](#configuration) below) — call it `penne.toml`.
2. Run:

   ```bash
   cargo run --bin penne -- download path/to/release.nzb \
       --config penne.toml \
       --out-dir ./downloads
   ```

`penne download` fetches every file the `.nzb` lists, assembles them,
verifies/repairs with PAR2 if recovery data was included, and extracts any
archive it finds — printing a per-file status line for each step. It exits
non-zero if anything is still incomplete or damaged once PAR2 has had its
chance to fix it.

## Configuration

`--config` is **required** for `download` (there's nothing useful to do
without server credentials) and points at a TOML file:

```toml
# Where completed, assembled files are written. Overridden by --out-dir.
download_dir = "/downloads"

# Optional: default `connections` for any [[servers]] entry below that
# doesn't set its own. Falls back to 8 if omitted entirely.
connections = 8

# Optional: retry attempts per segment against one server before moving on
# to the next configured server. Default: 3.
retries = 3

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

## Usage

```bash
# Parse a .nzb and print file/segment/size counts — no network I/O.
cargo run --bin penne -- info path/to/release.nzb

# Download, assemble, PAR2-verify/repair, and extract.
cargo run --bin penne -- download path/to/release.nzb \
    --config penne.toml \
    --out-dir ./downloads   # optional; defaults to the config's download_dir
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
4. **PAR2 verify/repair**, if any `.par2` file is present among the
   downloaded files: files left unwritten in step 3 can be recreated
   *whole* from recovery data; files with a bad checksum are patched at
   just the damaged parts.
5. **Extract** any `.rar`/`.7z`/`.zip` found (including multi-volume sets),
   using the `.nzb`'s embedded password if it has one.

If anything is still incomplete or damaged after step 4, `penne download`
exits non-zero and reports which files.

### Resume

An interrupted `penne download` run doesn't start over: every successfully
fetched article body is cached under `<out-dir>/.penne-cache/`, keyed by
Message-ID. Re-running the same command against the same `.nzb`/`--out-dir`
skips the network entirely for anything already cached. The cache is deleted
automatically once a run completes fully.

## Roadmap

See [`ROADMAP.md`](ROADMAP.md). A web UI (à la SABnzbd) is planned as a
separate crate built on top of `penne` once the CLI/engine reach feature
parity with a real downloader — not before.
