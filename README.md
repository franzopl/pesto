# pesto

Fast, lean Usenet poster, written in Rust.

It takes a list of files, encodes them with yEnc, posts the articles to Usenet
groups over NNTP, and generates an `.nzb` file. Inspired by
[`nyuu`](https://github.com/animetosho/Nyuu), but with a minimal scope: just
the basics, executed extremely fast.

It will be integrated into the posting flow of the `upapasta` program.

## Status

The MVP is complete: yEnc encoding, parallel TLS posting and `.nzb` generation
all work end to end. See [`ROADMAP.md`](ROADMAP.md) for what comes next.

## Build

Requires the Rust toolchain (install via <https://rustup.rs>).

```bash
cargo build --release
```

The optimized binary is written to `target/release/pesto`.

## Usage

```bash
pesto --config config.toml --out upload.nzb file1 file2 ...
```

Server and credentials come from a TOML config file; any field can be
overridden on the command line. See [`config.example.toml`](config.example.toml).

### Without a config file

Everything can be passed as flags instead:

```bash
pesto \
  --host news.example.com --port 563 \
  --username alice --password secret \
  --from 'alice <alice@example.com>' \
  --groups alt.binaries.test \
  --connections 10 \
  --out upload.nzb \
  movie.mkv
```

### Flags

| Flag | Description |
|------|-------------|
| `-c`, `--config <PATH>` | TOML config file |
| `--host <HOST>` | NNTP server hostname |
| `--port <PORT>` | NNTP server port (default 563) |
| `--no-ssl` | Disable TLS |
| `--connections <N>` | Number of parallel connections (default 4) |
| `--username <USER>` | Authentication username |
| `--password <PASS>` | Authentication password |
| `--from <FROM>` | `From` header for posted articles |
| `--groups <G,...>` | Newsgroups to post to (comma-separated) |
| `-o`, `--out <PATH>` | Path of the `.nzb` file to write |
| `--obfuscate` | Post under random subjects and yEnc file names |

### Exit codes

| Code | Meaning |
|------|---------|
| `0` | All segments posted |
| `1` | One or more segments failed |
| `130` | Interrupted with Ctrl-C |

On Ctrl-C, `pesto` stops taking new segments, lets in-flight ones finish, and
still writes an `.nzb` for whatever was posted.

### Obfuscation

With `--obfuscate`, each file is posted under a random subject and a random
yEnc file name, so nothing on the wire reveals the real file name. The real
name is preserved only in the generated `.nzb`, in the `name` attribute of the
`<file>` element — keep the `.nzb` to restore it. Without the `.nzb` an
obfuscated post cannot be reassembled or named.

## Development

```bash
cargo test                  # unit + integration tests
cargo clippy -- -D warnings
cargo fmt
```

## License

MIT
