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
| 4 | File assembly — `penne::assemble::assemble`/`assemble_all`, whole-file CRC-32, temp-file-then-rename; `penne download` CLI now performs a real end-to-end download |
| 6 | PAR2 verify & repair — `penne::repair::verify_and_repair`, wired into `penne download`; recreates fully-missing files and patches damaged ones via `pesto::par2` |
| 7 | Archive extraction — `penne::extract::extract_all` (`.rar`/`.7z`/`.zip`, multi-volume, password), wired into `penne download` after PAR2 |
| 8 | Resilience — `penne::cache` (segment-level resume), configurable retry/backoff in `download_queue` |
| 9 | Performance — N-parallel-connections-per-server concurrency in `download_queue`, closing Phase 2's long-standing open item |

---

## Phase 1 — NZB loading & queue model

- [x] `penne::nzb::load` — read a `.nzb` file from disk via `pesto::nzb::parse`.
- [x] `penne::nzb::summarize` — file/segment/byte counts (`penne info`).
- [x] `penne::queue::build` — group parsed segments into `QueuedFile`/`QueuedSegment`
      (pure data, no I/O; drives Phase 2 onward).
- [ ] Handle multi-`.nzb` batch input (a queue of queues) once single-file
      download works end-to-end.

## Phase 2 — NNTP article retrieval ✅

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
- [x] True N-parallel-connections-per-server concurrency: closed in
      Phase 9 (`drain_one_server`/`worker_loop`) rather than here, once
      Phase 4 gave the decoded bytes somewhere to go and throughput work had
      something real to measure against, exactly as anticipated below.
- [x] Wire `penne download` (the CLI) to actually call `download_queue`:
      done in Phase 4, once assembly existed for decoded segments to land in.

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

## Phase 4 — File assembly ✅

- [x] `penne::assemble::assemble` — writes each decoded segment at its own
      byte offset (`DecodedPart::begin`) via `seek`+`write_all`, rather than
      appending in fetch order — assembles correctly regardless of arrival
      order, which matters once downloading is parallelized (Phase 2's
      still-open N-connection item). A segment missing from the `decoded`
      map (not fetched/not decodable) makes the whole file `Incomplete`
      *without writing anything* — a partial file that looks complete is
      worse than no file. `assemble_all` runs this over every file in a
      `DownloadQueue` and reports one `AssembleOutcome` each.
- [x] Whole-file CRC-32 check: accumulated incrementally with
      `pesto::yenc::Crc32` while writing, in ascending part order (guaranteed
      by `queue::build`'s sort) — not by re-reading the file back, so cost is
      independent of file size. Compared against any segment's
      `file_crc32` (from `=yend crc32=`) when one was sent. Per-part
      `crc_matches()` failures are tracked too and surfaced separately
      (`AssembleOutcome::ChecksumMismatch { bad_parts, .. }`) — a
      structurally valid decode whose content doesn't match its own claimed
      checksum is corruption-in-transit, not something to hide by only
      checking the whole-file sum. Either signal lands the file in
      `ChecksumMismatch`, kept on disk regardless (a PAR2 repair candidate,
      Phase 6, not something to discard).
- [x] Temp-file-then-rename: writes go to a `<name>.penne-part` sibling of
      the final path, renamed into place only after every segment has
      landed, so a killed download never leaves behind a file that looks
      complete but isn't.
- [x] `penne download` (the CLI) now performs a real download: parses the
      `.nzb`, requires `--config` (server credentials — no longer optional,
      since there is nothing meaningful to do without one), calls
      `download_queue` then `assemble_all`, and reports per-file status
      (`ok`/`ok (unverified)`/`DAMAGED`/`INCOMPLETE`). Exits non-zero if
      anything didn't fully assemble. Verified against a local, in-process
      fake NNTP server through the actual compiled binary (not just the
      library) in `tests/cli_download_end_to_end.rs`, using the
      synchronous mock-server pattern from
      `crates/pesto/tests/server_substituted_message_id.rs` (a blocking
      `std::process::Command` inside an async `tokio` test would otherwise
      risk starving the mock server's own task).

## Phase 5 — Progress & CLI UX

- [ ] Wire `penne::progress::ProgressEvent` into `penne download`: live
      per-file progress, not just the current "would download" summary.
- [ ] Exit codes distinguishing "fully complete", "complete after repair",
      and "incomplete/missing data" — a downloader's most important signal.
- [ ] `--verbose`/`--quiet`, matching `pesto`'s conventions.

## Phase 6 — PAR2 verify & repair ✅ (core done; on-demand extra-volume fetch still open)

- [x] `penne::repair::find_par2_index` — any `.par2` file directly under the
      download directory is a valid starting point for
      `pesto::par2::recovery_set::RecoverySet::load` (index and every
      recovery volume carry the same Main/File-Description/IFSC packets per
      the PAR2 spec; only recovery blocks differ), which itself scans the
      directory for the rest of the set.
- [x] `penne::repair::verify_and_repair` — loads the recovery set, runs
      `pesto::par2::verify::verify`, and calls
      `pesto::par2::repair::repair` when repairable. Runs on
      `tokio::task::spawn_blocking` (PAR2 is synchronous, CPU/IO-bound
      Reed-Solomon work), mirroring the pattern `pesto`'s own poster already
      uses for PAR2 (`crates/pesto/src/upload.rs`). Returns a `RepairOutcome`
      distinguishing `NoRecoveryData` / `Ok` / `Repaired` / `NotRepairable`
      instead of collapsing them into one boolean.
- [x] **The actual payoff, proven by test:** an
      `AssembleOutcome::Incomplete` file (Phase 4 wrote nothing at all for
      it, since segments were missing) is exactly `parmesan`'s
      `FileStatus::Missing` — `pesto::par2::repair::repair` recreates it
      *whole* from recovery blocks, no reassembly needed. An
      `AssembleOutcome::ChecksumMismatch` file is `FileStatus::Damaged` —
      patched in place at only the bad slices.
- [x] Wired into `penne download` (the CLI): runs `verify_and_repair` after
      every `assemble_all`, unconditionally (not only when assembly reported
      trouble — matching how a real downloader always PAR2-checks when data
      is present). Prints per-file repair results; exits non-zero only on
      `NotRepairable`, or on `NoRecoveryData` when something still needed
      fixing.
- [x] Test fixtures use *real* on-disk PAR2 bytes, not hand-built structs:
      `tests/support/mod.rs` drives the actual encoder/packet-writer API
      (`pesto::par2::{encoder, packet}`, fully public), adapted from
      `crates/parmesan/src/test_support.rs` (`pub(crate)` there, unreachable
      from another crate). `tests/repair.rs` covers intact / fully-missing /
      damaged / not-repairable / no-`.par2`-present. A new
      `tests/cli_download_end_to_end.rs` case
      (`download_recovers_a_fully_missing_segment_via_par2`) drives the
      *actual compiled binary* through a fake NNTP server that never serves
      one segment of a two-part file, alongside its PAR2 index and recovery
      volume (also fetched over NNTP, like a real `.nzb` would list them) —
      and confirms `penne download` still produces the exact original file.
- [ ] **Still open:** on-demand extra-volume fetching. Today, every `.par2`
      volume listed in the `.nzb` is downloaded unconditionally along with
      the data files (simple, correct, but wasteful for releases that ship
      much more redundancy than any single run needs). Skipping volumes
      up front and fetching more only if `verify` finds insufficient local
      recovery blocks is a worthwhile optimization, not a correctness gap —
      deferred until there's a queue/download API for fetching a delta
      after the fact.

## Phase 7 — Archive extraction ✅

- [x] `penne::extract::extract_all` for `.rar`/`.7z`/`.zip`. `pesto::compress`
      only creates archives (no extraction path to build on), so this is new
      code — mirrors its conventions (`pesto::compress::find_binary` reused
      directly; a local `run_command` with the same password-redaction-in-
      debug-logs behavior) rather than reimplementing archive parsing.
      Shells out to `7z x` for `.7z`/`.zip` and `unrar x` for `.rar` — well-
      tested external tools, same posture as PAR2 (Phase 6) and `pesto`'s own
      compression (Phase 9 there).
      Runs on `tokio::task::spawn_blocking`, and only *after* PAR2
      verify/repair ([`crate::repair`]) in `penne download` — extracting a
      `.rar` before confirming (or repairing) its integrity is pointless.
- [x] Password support: `penne download` passes `ParsedNzb::meta.password`
      (already parsed from the `.nzb`'s `<meta type="password">` today)
      through to `extract_all`.
- [x] Multi-volume RAR (both old-style `.rar`+`.r00`+`.r01`+… and new-style
      `.partN.rar`) and 7z (`.7z.001`, `.7z.002`, …) sets: `find_extractable`
      groups a release's volume files by `(kind, base_name)` and picks the
      correct entry point per group — the bare `.rar`/`.7z` file if one
      exists, else the lowest-numbered volume — since `7z`/`unrar` discover
      sibling volumes themselves once pointed at the right one. Verified
      against *real* archives built with the actual `7z`/`rar` CLIs (not
      hand-crafted archive bytes) in `tests/extract.rs`: plain and password-
      protected `.7z`, a wrong-password failure, a genuine multi-volume
      `.rar` set (uncovered a `rar` quirk along the way — see below), and a
      no-archives-present no-op. Tests skip gracefully if `7z`/`rar`/`unrar`
      aren't installed, matching `pesto::compress`'s own stance that these
      are optional system dependencies (`rar` itself isn't distributed with
      `pesto`/`penne` "due to licensing").
- **Fixture bug found while writing the RAR test, not a `penne` bug:**
  `rar a` given an *absolute* input path embeds the full path inside the
  archive (`tmp/xyz/big.bin`) unless `-ep1` is passed — exactly the flag
  `pesto::compress::compress_with_rar` already uses for real releases. The
  test fixture was missing it; `penne::extract`'s own logic was correct
  throughout.

## Phase 8 — Resilience ✅

- [x] **Resume**, at the segment level (`penne::cache`): before any network
      request, `download_queue` checks a small on-disk cache
      (`<dest_dir>/.penne-cache/`, one file per Message-ID, keyed by a
      percent-encoded — not hashed, to rule out collisions entirely — form
      of the ID) for a body already fetched in a previous, interrupted run.
      A hit skips the network request outright. Every freshly fetched body
      is cached the same way, so an interrupted `penne download` re-run on
      the same `.nzb`/`--out-dir` picks up exactly where it left off instead
      of re-downloading everything. `penne download` clears the cache once
      a run completes fully (assembled, PAR2-clean or repaired, extracted)
      — its only purpose is resuming *that* download.
      This is deliberately **not** the "resumable unit is 'segments not yet
      fetched', not 'files not yet posted'" design taken further into a
      full fetch-and-write-incrementally pipeline merge with
      `crate::assemble` (which would also solve holding a whole file's
      decoded bytes in memory before writing — a real scalability concern
      for multi-GB releases, tracked under Phase 9). The cache achieves the
      same resumability outcome without that larger refactor's risk.
- [x] **Retry/backoff per segment, configurable:** a connection or fetch
      error against one server (not a definitive `430` — that is retried by
      trying the *next server*, never the same one again) is retried up to
      `retries` times, sleeping that server's own `retry_delay` between
      attempts, reconnecting each time since an error likely means the
      connection is dead. `retries` now comes from `penne`'s own config
      (`RawConfig::retries`, defaulting to `pesto::config::DEFAULT_RETRIES`);
      `RawServer::retry_delay` is newly configurable per server too — it was
      silently hardcoded to `1` before this phase, ignoring whatever the
      TOML said.
- [x] **Multi-server priority** (primary + backup providers): already
      implemented in Phase 2's per-segment failover — nothing new needed
      here, just confirming the roadmap's forward-reference is satisfied.
- [x] Verified in `tests/resilience.rs`: a segment already present in the
      cache is served without any network I/O (proven against a server
      that would report the article missing if actually queried); a
      freshly fetched segment lands in the cache for next time; a
      transient connection failure (a fake server that drops its first *N*
      connections outright) is recovered from once `retries` covers it; and
      exhausting `retries` reports the segment `missing` rather than
      hanging or failing the whole run.

## Phase 9 — Performance ✅ (core done; buffer pool and real-provider benchmarks still open)

- [x] **True N-parallel-connections-per-server concurrency** — the
      centerpiece of this phase, and the item Phase 2 flagged as its own
      biggest remaining gap. `download_queue` no longer drains one
      connection per server sequentially: for each server, in priority
      order, up to `server.connections` worker tasks (`tokio::task::JoinSet`)
      pull from a shared, mutex-guarded work queue and fetch/decode/cache
      concurrently. Each worker keeps one connection open for its whole
      pass rather than reconnecting per segment.
      Priority-ordered *servers* stay sequential (all of server 1's workers
      finish before server 2's start) — that part is deliberately unlike
      `pesto::nntp::pool`'s rotate-on-error model, because "missing from
      this server" is an expected, per-segment condition for a downloader
      (an article genuinely not being on a given provider), not a failure
      to route around; a backup provider should only ever be asked about
      the segments the primary didn't have, not raced against it.
      `DownloadOutcome`'s shape and `download_queue`'s public signature are
      unchanged, so nothing downstream (`assemble`, `repair`, `extract`,
      the CLI) needed to change.
      Verified in `tests/concurrency.rs` two ways against a fake server that
      deliberately holds each `BODY` request open for 80 ms: peak observed
      concurrent in-flight requests actually exceeds 1 (impossible for a
      sequential drain), and wall-clock time for 8 segments over 4
      connections lands far under the ~640 ms a sequential drain would take
      (consistently ~250 ms across repeated runs).
- [ ] **Still open:** double-buffered writer / buffer pool on the assembly
      path, mirroring `pesto`'s posting-side buffer pool (Phase 12 there).
      Judged lower priority than connection concurrency: a downloader's
      bottleneck is overwhelmingly NNTP round-trip latency and connection
      count, not the cost of a `seek`+`write_all` per already-in-memory
      segment — `assemble` doing that today is unlikely to be where time
      actually goes. Worth revisiting with real profiling data, not
      speculatively.
- [ ] **Still open:** benchmark against a real indexer/provider pair; add a
      `bench/` entry alongside the existing `pesto`/`parmesan` ones. Blocked
      on infrastructure this environment doesn't have (a real Usenet
      provider account and indexer) — `tests/concurrency.rs`'s synthetic
      timing check is the closest available substitute for now.

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
