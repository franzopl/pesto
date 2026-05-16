# Roadmap — `pesto`

Fast, lean Usenet poster in Rust. Inspired by `nyuu`, with only the essentials.
Each phase must leave the program in a working, testable state.

## Phase 0 — Foundation ✅

- [x] `cargo init` with `main.rs` + `lib.rs`
- [x] Module skeleton (`config`, `yenc`, `nntp`, `article`, `nzb`)
- [x] CLI parsing with `clap`
- [x] TOML config loading + merge with flags
- [x] Basic CI (`fmt`, `clippy`, `test`)

## Phase 1 — yEnc encoding ✅

- [x] yEnc encoder following the specification (escaping of `=`, NUL, CR, LF)
- [x] `=ybegin` / `=yend` lines with CRC32
- [x] File segmentation into parts (`=ypart` for multi-part)
- [x] Tests against known yEnc vectors

## Phase 2 — NNTP client

- [ ] TCP + TLS connection (`rustls`) on port 563
- [ ] Handshake and `AUTHINFO USER/PASS` authentication
- [ ] `POST` command (article upload, handling of 240/441 responses)
- [ ] Article assembly: headers (`Subject`, `From`, `Newsgroups`,
      `Message-ID`) + yEnc body
- [ ] Unique `Message-ID` generation per segment

## Phase 3 — Parallel posting

- [ ] Pool of N concurrent TLS connections (`tokio`)
- [ ] Work queue: segments distributed across connections
- [ ] Retry of failed segments
- [ ] Progress bar / throughput in the terminal
- [ ] Flags `--connections`, `--ssl`, `--groups`

## Phase 4 — `.nzb` generation

- [ ] Collect `Message-ID`s, sizes and CRCs of posted segments
- [ ] Write a valid `.nzb` XML file (nzb DTD)
- [ ] Flag `--out` for the `.nzb` path

## Phase 5 — MVP polish

- [ ] Actionable error messages (network, auth, I/O)
- [ ] Ctrl-C handling / clean shutdown
- [ ] `README` with usage examples
- [ ] End-to-end integration test (mock NNTP)

## Phase 6 — `upapasta` integration

- [ ] Stabilize the public API of `lib.rs`
- [ ] Document integration points
- [ ] Adapt the `upapasta` posting flow to use `pesto`

## Post-MVP (future ideas)

- Compression / RAR creation before posting
- PAR2 file generation
- Subject/file name obfuscation
- Resume of interrupted posts
- Rate limiting
- Multiple servers / failover
