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

---

## Phase 1 — NZB loading & queue model

- [x] `penne::nzb::load` — read a `.nzb` file from disk via `pesto::nzb::parse`.
- [x] `penne::nzb::summarize` — file/segment/byte counts (`penne info`).
- [x] `penne::queue::build` — group parsed segments into `QueuedFile`/`QueuedSegment`
      (pure data, no I/O; drives Phase 2 onward).
- [ ] Handle multi-`.nzb` batch input (a queue of queues) once single-file
      download works end-to-end.

## Phase 2 — NNTP article retrieval

The first real gap versus `pesto`: posting never needed to *read* an article
back.

- [ ] Add `GROUP`, `ARTICLE`/`BODY`, `STAT` (already partially present) to
      `pesto::nntp::Connection`, or a `penne`-local equivalent if the
      semantics diverge enough (e.g. download wants raw body bytes, not a
      parsed response) to not belong in `pesto`.
- [ ] `penne::client::DownloadClient::body` — fetch one article's raw bytes
      by Message-ID (replaces today's `bail!` stub).
- [ ] Connection pool for downloading: N parallel connections pulling from
      the queue built in Phase 1. Reuse `pesto::nntp::pool` patterns
      (`ConnectionSlot`/`ConnectionPool`) if they transfer cleanly; downloading
      differs from posting in one important way — a missing article should
      retry against the *next configured server* (backup provider), not just
      reconnect to the same one.
- [ ] Missing-article handling: record which segments could not be fetched
      from any server; surface them instead of silently producing a
      truncated file.

## Phase 3 — yEnc decoding

- [ ] `pesto::yenc` currently only encodes. Add a decoder: parse `=ybegin`/
      `=ypart`/`=yend` control lines, undo the yEnc byte escaping, verify the
      segment CRC32 the article carries.
- [ ] Decide placement: this is generic yEnc, not download-specific policy,
      so it belongs in `pesto::yenc` (shared with any future consumer),
      analogous to how `pesto::nzb::parse` already sits next to `generate`.
- [ ] Property-test round-trip: `decode(encode(x)) == x` for arbitrary bytes,
      including the escape-byte edge cases the yEnc spec calls out.

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
