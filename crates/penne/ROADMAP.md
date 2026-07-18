# Roadmap — `penne`

Fast NZB downloader for Usenet, CLI-first. Companion to
[`pesto`](../../ROADMAP.md) (which posts) and [`parmesan`](../parmesan/ROADMAP.md)
(which handles PAR2). Each phase must leave the program in a working,
testable state.

> **Scope:** this document governs the `penne` crate only. A web UI
> (SABnzbd-like) built on top of the download engine developed here is
> planned but explicitly **out of scope** until the CLI reaches feature
> parity with a real downloader — see "Later — Web UI" at the end of this
> file.

---

## Design decisions

- **Reuse `pesto`, don't fork it.** `.nzb` parsing ([`pesto::nzb::parse`]),
  the NNTP TCP/TLS/`AUTHINFO` handshake ([`pesto::nntp::Connection`]), and
  PAR2 verify/repair ([`pesto::par2`], i.e. `parmesan`) already exist and are
  reused as libraries. `penne` only adds what posting never needed: article
  *retrieval*, yEnc *decoding*, and file *reassembly*.
- **`pesto` stays upload-only.** Per `CLAUDE.md`, `pesto`'s hot path is
  "yEnc → article → NNTP" for posting. Download-specific protocol commands
  (`GROUP`, `ARTICLE`, `BODY`) and the yEnc *decoder* are new code; where they
  land (inside `pesto::nntp`/`pesto::yenc` as shared plumbing, vs. local to
  `penne`) is decided per-phase below, favoring the shared location whenever
  the logic is truly protocol-level and not download-specific policy.
- **Engine first, UI later.** `penne` is a CLI now and a library underneath a
  future web UI later (the user explicitly deferred the web UI). Business
  logic must not assume a terminal — see `src/lib.rs`, which is written to be
  driven by any frontend via `mpsc` progress channels, mirroring
  `pesto::post()`.

---

## Completed ✅

| Phase | Topic |
|-------|-------|
| 0 | Foundation — workspace crate, CLI skeleton (`info`/`download`), config, `ROADMAP.md` |
| 1 | NZB loading & queue model — `penne::nzb::load`/`summarize`, `penne::queue::build` |
| 2 | NNTP article retrieval — `BODY` in `pesto::nntp`, `DownloadClient::body`, per-segment server failover, missing-segment tracking |
| 3 | yEnc decoding — `pesto::yenc::decode_part`, wired into `download_queue` with per-segment corrupt-copy failover |

---

## Phase 1 — NZB loading & queue model

- [x] `penne::nzb::load` — read a `.nzb` file from disk via `pesto::nzb::parse`.
- [x] `penne::nzb::summarize` — file/segment/byte counts (`penne info`).
- [x] `penne::queue::build` — group parsed segments into `QueuedFile`/`QueuedSegment`
      (pure data, no I/O; drives Phase 2 onward).
- [ ] Handle multi-`.nzb` batch input (a queue of queues) once single-file
      download works end-to-end.

## Phase 2 — NNTP article retrieval ✅ (core done; N-way concurrency still open)

The first real gap versus `pesto`: posting never needed to *read* an article
back.

- [x] Add `BODY` to `pesto::nntp::Connection` (RFC 3977 §6.2.3), undoing dot-
      stuffing over raw bytes (not `String`/`read_line`, since yEnc bodies are
      8-bit data and not guaranteed valid UTF-8). Unit-tested with the same
      `mock_conn` duplex-stream pattern the existing `POST`/`STAT` tests use,
      including a non-UTF-8 byte round-trip.
      `GROUP` and `ARTICLE`-by-number were deliberately **not** added:
      `.nzb` files address every segment by Message-ID, so `BODY <message-id>`
      alone is sufficient and a selected group is never needed.
- [x] `penne::client::DownloadClient::body` — fetch one article's raw bytes
      by Message-ID (replaces the old `bail!` stub); returns `Ok(None)` on a
      `430` so the caller can fail over instead of erroring out.
- [x] `penne::download::download_queue` — drains a `DownloadQueue` against a
      list of servers, trying each **per segment** in priority order so one
      provider missing a handful of articles doesn't sink a file a backup
      server has intact. Connections are opened lazily (only servers actually
      needed) and reused for the rest of the run.
      Verified end-to-end against a local, in-process fake NNTP server over
      real TCP (loopback only — see `tests/download_with_failover.rs`,
      following the same pattern as `crates/pesto/tests/server_substituted_message_id.rs`):
      single-server fetch, primary-missing-falls-back-to-backup, and
      no-server-has-it.
- [x] Missing-article handling: `DownloadOutcome::missing` records every
      segment no configured server had, alongside `DownloadOutcome::segments`
      for what *was* fetched and decoded — nothing is silently dropped.
- [ ] **Still open:** true N-parallel-connections-per-server concurrency,
      mirroring `pesto::nntp::pool`'s `ConnectionSlot`/`ConnectionPool`.
      Today's `download_queue` is one connection per server, drained
      sequentially — correct, but not yet fast. This is the natural next
      increment once Phase 4 gives the decoded bytes somewhere to go
      (assembly), so throughput work has something real to measure against.
- [ ] Wire `penne download` (the CLI) to actually call `download_queue`
      instead of only printing a summary — reasonable to defer until
      assembly (Phase 4) exists, since decoded segments still need to land
      on disk to become a file.

## Phase 3 — yEnc decoding ✅

- [x] `pesto::yenc::decode::decode_part` — parses `=ybegin`/`=ypart`/`=yend`
      control lines and decodes the data lines back to raw bytes. Operates on
      raw bytes throughout (never `String`/UTF-8), since yEnc data is 8-bit
      and a `name=` field or the decoded content itself is not guaranteed
      valid UTF-8 (`name=` is captured via `String::from_utf8_lossy`, used
      only for display — assembly should prefer the `.nzb`'s `file_name`,
      never obfuscated, over this field).
      Decodes one line at a time: confirmed safe because the encoder
      (`scalar::encode_scalar`) never splits an escape pair across a line
      boundary — both escape bytes are always written before the line-wrap
      check runs.
      Returns `DecodedPart` with `part_crc32`/`file_crc32` plus a
      `crc_matches()` helper; a checksum mismatch is **not** a decode error —
      it's a data-integrity signal for Phase 4/6 to act on, not a reason to
      fail parsing that otherwise succeeded.
- [x] Decided placement: lives in `pesto::yenc` (new `decode` submodule,
      re-exported as `pesto::yenc::{decode_part, DecodedPart}`), not in
      `penne` — generic yEnc, not download-specific policy, mirroring how
      `pesto::nzb::parse` already sits next to `generate`.
- [x] Round-trip tests: single-part, multi-part with file CRC, empty part,
      names containing spaces, and a CRC-mismatch case that decodes fine but
      reports `crc_matches() == false`. `round_trips_single_part` cycles all
      256 byte values across multiple 128-byte lines, exercising every
      critical/positional escape (NUL/LF/CR/`=`, boundary TAB/space, and
      line-start `.`) the encoder can produce.
- [x] Wired into `penne::download::download_queue`: each fetched body is
      decoded immediately; a decode failure is **not** treated the same as a
      missing article — the next configured server is tried before giving up
      on the segment (a truncated/corrupted transfer from one provider
      doesn't have to sink a file a backup server serves intact). Segments
      that no server could produce a decodable copy of land in the new
      `DownloadOutcome::corrupt`, distinct from `missing` (article exists
      somewhere; no copy retrieved was structurally valid yEnc) — surfaced
      via a new `ProgressEvent::SegmentCorrupt`, not silently dropped.
      Verified end-to-end in `tests/download_with_failover.rs` using real
      `encode_part` output as the served bodies, plus a corrupt-primary /
      good-backup failover case and a no-server-decodable case.

## Phase 4 — File assembly

- [ ] `penne::assemble::assemble` — write decoded segments to their offset in
      the destination file; segments may complete out of order across
      connections, so this cannot assume sequential arrival.
- [ ] Whole-file CRC32 check once every part has landed (`pesto::yenc::Crc32`
      already supports incremental updates).
- [ ] Temp-file-then-rename so a killed download never leaves a file that
      looks complete but isn't.

## Phase 5 — Progress & CLI UX

- [ ] Wire `penne::progress::ProgressEvent` into `penne download`: live
      per-file progress, not just the current "would download" summary.
- [ ] Exit codes distinguishing "fully complete", "complete after repair",
      and "incomplete/missing data" — a downloader's most important signal.
- [ ] `--verbose`/`--quiet`, matching `pesto`'s conventions.

## Phase 6 — PAR2 verify & repair

- [ ] `penne::repair::verify_and_repair` — call `pesto::par2` (i.e.
      `parmesan`) verify on the assembled set; if damaged and `.par2` volumes
      with enough recovery data were part of the `.nzb`, repair.
- [ ] If verify finds damage but not enough *local* recovery blocks, and more
      `.par2` volumes are listed in the `.nzb` but weren't downloaded yet
      (common: clients skip par2 volumes unless needed), fetch the
      additional volumes on demand before giving up.

## Phase 7 — Archive extraction

- [ ] `penne::extract::extract_all` for `.rar`/`.7z`/`.zip` (`pesto::compress`
      only creates archives; this is new code, most likely shelling out to
      `7z`/`unrar` like `pesto::compress::find_binary` already does for
      creation).
- [ ] Password support (`.nzb` `<meta type="password">`, already parsed into
      `ParsedNzb::meta` today).
- [ ] Multi-volume RAR (`.r00`, `.r01`, …) and 7z (`.7z.001`, …) sets.

## Phase 8 — Resilience

- [ ] Resume: persist queue state so an interrupted download continues
      instead of restarting (mirrors `pesto::resume` conceptually, but the
      resumable unit is "segments not yet fetched", not "files not yet
      posted").
- [ ] Retry/backoff per segment, configurable, matching `pesto`'s conventions
      (`retry_delay` already exists on `ServerEntry`, reused from `pesto`).
- [ ] Multi-server priority: primary + backup providers, already representable
      via `Config::servers`; wire actual failover logic in Phase 2.

## Phase 9 — Performance

- [ ] Double-buffered writer / buffer pool on the assembly path, mirroring
      `pesto`'s posting-side buffer pool (Phase 12 there).
- [ ] Benchmark against a real indexer/provider pair once Phases 2–4 exist;
      add a `bench/` entry alongside the existing `pesto`/`parmesan` ones.

## Phase 10 — Packaging & release

- [ ] `README.md` usage docs (mirrors `pesto`'s and `parmesan`'s structure).
- [ ] Add `penne` to the release workflow once it has a stable CLI surface
      (see `.github/workflows/release.yml` / `release-parmesan.yml` for the
      pattern to follow).

---

## Later — Web UI

Explicitly deferred until the phases above reach feature parity with a real
NZB downloader. When it starts, it should be a **separate crate** consuming
`penne` as a library (same relationship `upapasta` has with `pesto`), not
code embedded in this crate. No design work on it belongs in this file yet.
