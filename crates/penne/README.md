# Penne

**Fast NZB downloader for Usenet, written in Rust.**

Companion to [`pesto`](../pesto) (which posts) and
[`parmesan`](../parmesan) (which handles PAR2). Reads a `.nzb`, fetches its
articles over parallel NNTP connections, reassembles the original files, and
verifies/repairs them with PAR2.

> **Status: early skeleton.** Only `.nzb` parsing and the `penne info`
> command are functional today. Article retrieval, yEnc decoding, file
> assembly, PAR2 repair and archive extraction are not implemented yet — see
> [`ROADMAP.md`](ROADMAP.md) for the phased plan.

## Usage (current state)

```bash
# Parse a .nzb and print file/segment/size counts.
cargo run --bin penne -- info path/to/release.nzb

# Parse a .nzb and report what would be downloaded (no network I/O yet).
cargo run --bin penne -- download path/to/release.nzb --out-dir ./downloads
```

## Configuration

`penne` reads the same `[[servers]]` shape as `pesto`
(see [`config.example.toml`](../../config.example.toml)), plus a
download-specific `download_dir`:

```toml
download_dir = "/downloads"

[[servers]]
host = "news.example.com"
ssl = true
username = "user"
password = "pass"
connections = 8
```

## Roadmap

See [`ROADMAP.md`](ROADMAP.md). A web UI (à la SABnzbd) is planned as a
separate crate built on top of `penne` once the CLI/engine reach feature
parity with a real downloader — not before.
