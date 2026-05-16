# pesto

Fast, lean Usenet poster, written in Rust.

It takes a list of files, encodes them with yEnc, posts the articles to Usenet
groups over NNTP, and generates an `.nzb` file. Inspired by
[`nyuu`](https://github.com/animetosho/Nyuu), but with a minimal scope: just
the basics, executed extremely fast.

It will be integrated into the posting flow of the `upapasta` program.

## Status

Early development. See [`ROADMAP.md`](ROADMAP.md).

## Build

Requires the Rust toolchain (install via <https://rustup.rs>).

```bash
cargo build --release
```

## Usage

```bash
pesto --config config.toml file1 file2 ...
```

See [`config.example.toml`](config.example.toml) for the configuration.

## License

MIT
