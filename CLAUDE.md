# CLAUDE.md

Guide for agents working in this repository.

> The official language of this project is **English**. All code, comments,
> documentation, commit messages and identifiers must be written in English.

## What `pesto` is

`pesto` is a Usenet poster written in Rust. It takes a list of files, encodes
them with yEnc, posts the resulting articles to Usenet groups over NNTP, and
generates an `.nzb` file describing what was posted.

It is inspired by [`nyuu`](https://github.com/animetosho/Nyuu), but with a
deliberately smaller scope: **just the basics, executed extremely fast**. No
superfluous flags. Performance and simplicity over feature coverage.

End goal: `pesto` will be integrated into the posting flow of the `upapasta`
program. Design decisions must keep the tool usable both as a standalone CLI
binary and as an embeddable library (`lib.rs` + `main.rs`).

## Design principles

- **Speed first.** The hot path is: read file → yEnc → send over the NNTP
  connection. Avoid needless allocations, buffer copies and lock contention on
  that path. Prefer streaming I/O over loading whole files into memory.
- **Connection-pool concurrency.** Throughput comes from N parallel NNTP
  connections posting articles simultaneously. That is the core architecture,
  not an add-on.
- **Minimal scope.** Before adding a flag or option, ask whether it is
  fundamental to posting files. If not, it waits until after the MVP.
- **Fail clearly.** Network, authentication and I/O errors must produce
  actionable messages, not panics.

## Intended architecture

```
src/
  main.rs        # CLI parsing, loads config, kicks off the post
  lib.rs         # public API for use by upapasta
  config.rs      # TOML loading + merge with flags
  yenc.rs        # yEnc encoder (hot path — keep it lean)
  nntp/
    mod.rs       # NNTP client, POST command
    pool.rs      # pool of concurrent TLS connections
  article.rs     # article assembly (headers + yEnc body, segmentation)
  nzb.rs         # .nzb file generation (XML)
```

This tree is a target, not the current state — see `ROADMAP.md` for what has
already been delivered.

## Stack and suggested dependencies

- Async runtime: `tokio`
- TLS: `rustls` + `tokio-rustls` (avoids depending on the system OpenSSL)
- CLI: `clap` (derive)
- Config: `serde` + `toml`
- `.nzb` XML: manual string generation or `quick-xml`
- Errors: `anyhow` in the binary, dedicated error types in the library

Keep the dependency tree small. Every new crate must justify itself.

## Configuration

Server and credentials come from a TOML file (see `config.example.toml`). Any
field can be overridden by a CLI flag. Credentials must never be logged.

## Commands

```bash
cargo build --release      # optimized binary at target/release/pesto
cargo run -- <args>        # debug run
cargo test                 # tests
cargo clippy -- -D warnings
cargo fmt
```

> Note: the Rust toolchain (`cargo`/`rustc`) is not yet installed in this
> environment. Install it via <https://rustup.rs> before building.

## Conventions

- Formatting: `cargo fmt` (defaults).
- Lints: code must pass `cargo clippy -D warnings`.
- Commits: short imperative messages. Group by roadmap phase.
- yEnc and NNTP have specifications; when changing them, cite the relevant
  part of the spec in a comment instead of "tweaking until it works".
