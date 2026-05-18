# Contributing to pesto

Thank you for your interest in contributing. This document covers everything
you need to get started: setting up the environment, running tests, and the
conventions we follow.

---

## Development environment

### Requirements

- **Rust 1.75+** — install or update via <https://rustup.rs>
- `cargo fmt` and `cargo clippy` must pass before a PR is merged (the CI
  enforces both)

### Optional tools (for specific features)

| Tool | Required for |
|------|-------------|
| `p7zip` | `--compress` tests |
| `rar` | `--compress=rar` tests |
| `mediainfo` | `--nfo` tests |
| `par2cmdline` | PAR2 round-trip integration tests |

None of these are needed to build `pesto` or run the unit tests.

### Build

```bash
cargo build            # debug build
cargo build --release  # optimised binary at target/release/pesto
```

---

## Running tests

```bash
cargo test             # unit tests + integration tests (no network, no external tools)
cargo test --all       # same, including all crates
```

Tests that require a live NNTP server are gated with `#[ignore]` and never run
in CI:

```bash
cargo test -- --ignored   # run only the ignored (network) tests
```

### Test structure

| Location | Purpose |
|----------|---------|
| `#[cfg(test)]` modules inside each `src/*.rs` | Unit tests for pure logic (no I/O) |
| `tests/integration.rs` | End-to-end post with a mock NNTP server |
| `tests/par2_*.rs` | PAR2 round-trips verified with `par2cmdline` |
| `tests/obfuscated_directory.rs` | Obfuscated multi-folder upload round-trips |

The mock NNTP harness in `tests/integration.rs` listens on a local TCP socket
and records every `POST` command received. Use it as a base for new
network-touching integration tests.

---

## Code style

```bash
cargo fmt              # auto-format (required before commit)
cargo clippy -- -D warnings   # no warnings allowed
```

- Follow the conventions in `CLAUDE.md` (design principles, module layout).
- Comments only when the *why* is non-obvious — never describe what the code
  does, only surprising constraints or spec references.
- When changing yEnc or NNTP behaviour, cite the relevant spec section in a
  comment instead of "tweaking until it works".

---

## Commit messages

Short, imperative, present tense. Group related changes into one commit.

```
add --watch-interval flag
fix PAR2 slice count off-by-one on empty files
refactor: extract rate_limiter into its own module
```

Reference the relevant roadmap phase when applicable (`Phase 7c`, `Phase 14b`).

---

## Submitting a pull request

1. Fork the repo and create a branch from `main`.
2. Make your changes; ensure `cargo fmt` and `cargo clippy -- -D warnings` pass.
3. Run `cargo test --all` and confirm no regressions.
4. Open a PR against `main`. Describe *what* changed and *why*.

If you are picking up an item from [`ROADMAP.md`](ROADMAP.md), mention the
phase in the PR title (e.g. `Phase 22e: cargo install instructions`).

---

## Questions

Open a [GitHub issue](https://github.com/franzopl/pesto/issues) for bugs,
feature requests, or design questions.
