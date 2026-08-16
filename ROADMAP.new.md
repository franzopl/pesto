# Roadmap

Development plan for the `pesto` workspace: **pesto** (poster + core library),
**parmesan** (PAR2 engine), **penne** (NZB downloader), **sugo** (SABnzbd-compatible
web UI) and **upapasta** (TUI).

This file supersedes the phase-by-phase historical roadmap. Completed work is
recorded in each crate's `CHANGELOG.md`; what follows is only what is *left*.

Principles (from `CLAUDE.md`, unchanged): `pesto` stays lean and fast on the hot
path; complex UX lives in `upapasta`; crates integrate as libraries, never as
subprocesses; shared types live in `pesto` and are reused, not redefined.

---

## Current status

**Solid.** The posting pipeline is mature and heavily exercised. yEnc encoding has
scalar/SSSE3/AVX2 backends with byte-for-byte differential tests on x86-64; the
NNTP layer has pipelining, keepalive, multi-server failover, adaptive pipeline
depth and bounded connect/handshake timeouts; PAR2 generation is memory-budgeted
with multi-pass splitting and verified against real `par2cmdline`. Release file
ordering, obfuscation modes, the streaming STAT check with repost/recovery, and
season-mode global PAR2 with proper File Description packets are all done and
tested. `cargo clippy --all-targets` is clean across the workspace.

**Main remaining risk.** None — see Phase 1 below, closed via #121.

Phase 1 closed the correctness and safety gaps. Everything after it is feature
and polish work.

---

## Phase 1 — Correctness, safety and input hardening ✅ Done (#121)

**The gate for a 1.0-shaped release.** Every item had a filed issue with a
location and a failure mode; all landed together in #121 (merged into `main`
as 936842d), which also validated the season-path PAR2 spec-limit check that
routing season geometry through the shared `par2_geometry` helper (1c) had
silently dropped, and greened the new aarch64 CI job (1b) that the same PR
introduced.

### 1a — Untrusted input boundaries *(highest priority)*

- [x] **#109 — Sanitize PAR2-supplied file names before they become paths.**
      Do it once in `RecoverySet::load` so `parmesan::verify`, `parmesan::repair`
      and `penne::deobfuscate` all inherit it. Reject absolute paths, drive/UNC
      prefixes and `..` components; decide explicitly whether legitimate PAR2
      subdirectories are kept (scoped under the base) or flattened. Add a
      post-join containment assert as belt and braces, plus a regression test per
      call site.
- [x] **#115 — Validate published file names at the `walk::InputFile` chokepoint.**
      Reject or replace CR, LF, NUL and C0 controls before a name can reach a
      header, a `=ybegin` line or the NZB. Independently harden both sinks:
      `Article::build_headers` must refuse line terminators in any header value,
      and `nzb::escape` must handle XML-illegal control characters. Handle `"` in
      `default_subject` while there.
- [x] **#116 — Reject inconsistent PAR2 metadata at load time.** Validate that a
      `FileEntry`'s IFSC slice count matches `length.div_ceil(slice_size)`, and
      make `repair::slice_write_len` saturating, so a crafted `.par2` cannot abort
      the process.

### 1b — Cross-architecture verification

- [x] **#111 — Test the NEON yEnc encoder.** Mirror the existing x86 differential
      macros for aarch64, covering the same vectors (all byte values, critical
      bytes, positional bytes at short `line_len`s, single byte, empty, large
      payload). Pin `encoded_size_neon` to `encode_neon` output length. Fix the
      `# Safety` doc on `encoded_size_neon` (it underflows on empty input).
- [x] **Add an aarch64 CI job** (GitHub ARM runners) so 1b actually runs. Landed
      as `test-aarch64` on `ubuntu-24.04-arm`; it also caught two unrelated
      dead-code lint failures on that target (x86-only PAR2 SIMD buffer variants
      not gated for `-D warnings` elsewhere), fixed in the same branch.
- [x] **#114 — Assert the no-leading-dot invariant where it is relied on.**
      `debug_assert!` in `post_parts_inner`/`enqueue_post`, a comment explaining
      why the body is deliberately not dot-stuffed, and one test that a crafted
      payload never yields a body line starting with `.`. Keeping the body
      unstuffed is the right call for the hot path — this is about making the
      dependency explicit.

### 1c — PAR2 memory and geometry consistency

- [x] **#110 — Give season PAR2 the same memory budget as `producer`.** Compute a
      budget from `Ceiling::discover` + `budget::share_of` + `address_space_budget`
      and split into multiple read passes using the `exponent_start` parameter
      `try_new_smart` already takes. Pass `0` for the connection reserve during
      generation, as `--par2-before-upload` already does.
- [x] **Factor the budget and pass-splitting logic out of `producer`** into a
      helper both paths call. Landed as `par2_memory_plan`, shared by `producer`
      and `generate_season_par2`.
- [x] **#117 — Route season geometry through `par2_geometry`** so
      `--par2-slice-count` and `--par2-recovery-count` behave identically in both
      paths. Fix the append-mode volume writes (`truncate` on first touch, reuse
      the handle), sort or assert recovery-slice exponent order, and correct the
      volume-naming example in the doc comment. Sharing the geometry helper had
      dropped the old silent `.min(65535)` season clamp without replacing it with
      `producer`'s spec-limit `bail!`; added on top of #121 before merge.
- [x] **#118 — Split `RecoverySet::load` into metadata-only and block-loading
      paths.** `verify`, `health` and `deobfuscate` provably need no recovery
      blocks. For `repair`, index blocks by `(path, offset, len)` and stream them,
      or at minimum load only `total_bad_slices()` of them — Reed-Solomon is MDS,
      so any `m` blocks reconstruct `m` missing inputs.

### 1d — Resume robustness

- [x] **#112 — Derive `RunFingerprint` from `Config`.** Add `par2_slice_size`,
      `par2_slice_count`, `par2_recovery_count`, `compress_volume_size`,
      `compress_password` (hashed — the state file is plaintext) and `line_length`.
      Build it in one `RunFingerprint::from_config` used by both construction
      sites, generate `resume_flags_string` from the same source, and add a test
      asserting each field changes the fingerprint.
- [x] **#113 — Make resume state robust.** `load` returns an empty state on a parse
      error (as its doc already promises), keeping hard errors only for genuine
      I/O failures; `save` writes to a temp file and renames. This is also the
      migration path for the new fingerprint fields.
- [x] **#119 — Decide `--resume` + `--compress` explicitly.** Preferred: keep the
      generated archive keyed by `archive_stem` and reuse it when its recorded
      `{size, mtime}` still match, which makes the existing `archive_stem`
      persistence pay off. Otherwise document the incompatibility, warn once up
      front rather than mid-run, and drop the dead machinery.

### 1e — Documentation truth

- [x] Refresh `parmesan/src/recovery_set.rs` and `repair.rs` module docs — both
      still describe the pre-Phase-48 ordering situation and point at roadmap
      phases that have moved on.
- [x] Audit `docs/memory-management.md` against what `--memory-limit` actually
      bounds once #110 lands.

---

## Phase 2 — `pesto` engine work

- [x] **Connection pool reuse across `--each` episodes.** Added
      `nntp::pool::ConnectionBroker`: a long-lived set of `ConnectionSlot`s,
      checked out per episode and checked back in (instead of `QUIT`) when a
      worker is done, with a background task keeping idle-between-episodes
      connections alive via the same `MODE READER` keepalive workers already
      use within a run. `run_batch` builds one broker per `--each`/`--season`
      batch, sized to `total_connections()` and shared via the broker's
      internal semaphore across concurrent episodes under `--jobs N` — which
      also fixes `--jobs N` independently over-opening up to `N ×
      total_connections()` real sockets, capping it at the configured budget
      instead. `poster::post_files_inner` takes the broker as an optional
      parameter; the public `post`/`post_cancelable`/
      `post_files_with_progress_and_cancel` are unaffected (`broker: None`,
      exact prior behavior) so `upapasta` and other embedders are untouched.
      `--watch` and the single-file path still pass `None` — same win applies
      there, left as a follow-up. Out of scope for this pass: the streaming
      check coordinator's and `repost_failed_tasks`' own ephemeral
      connections.
- [x] **Real pause/resume in the poster.** Added `Shared.paused: Arc<AtomicBool>`,
      mirrored from a new `external_pause` parameter by `post_files_inner`'s
      existing cancel-polling task (now a continuous loop instead of one-shot,
      so it can toggle back and forth), and checked by `worker()` at the same
      segment-batch boundary as `cancelled`. While paused, a worker sits in a
      short-poll wait loop (`PAUSE_POLL`, 100ms — deliberately *not* the 2s
      `IDLE_POLL` used for idle-but-unpaused keepalive fan-out, or cancelling
      while paused would be sluggish) sending the same `MODE READER` keepalive
      already used for idle time within a run, so the connection survives
      without the broker or `ConnectionSlot` needing any changes. Workers never
      draining the queue while paused means a producer racing ahead naturally
      blocks on the bounded channel — pause propagates to the encode side for
      free. New public `post_pausable` in `lib.rs` (alongside `post`/
      `post_cancelable`, purely additive — `post_cancelable` has no in-repo
      callers today, so nothing else changed) and new `ProgressEvent::Paused`/
      `Resumed`. `upapasta`'s pause UI (the `p` key, `upload_paused` state,
      `PauseUpload`/`ResumeUpload` events) was removed in its Phase 40c-5
      specifically because this capability didn't exist yet — a "paused" state
      that froze the stats display while the upload kept running underneath was
      judged dishonest, so the UI was pulled rather than kept lying (see
      `crates/upapasta/ROADMAP.md` 40c-5 and Phase 46). Re-adding that UI on top
      of this mechanism is Phase 3's already-tracked item, left as a follow-up.
- [ ] **Server-side queue limit detection** (`441 Too many articles`) to cap
      pipeline depth adaptively. Previously deferred; revisit if reports appear.
- [ ] **Season PAR2 from spooled slices** *(deferred, low priority)*. Avoids one
      full re-read pass over the season (~5–10% on large seasons). Requires
      `FileHasher` state serialization alongside slices. Only worth doing if
      real-world feedback says the re-read hurts; Phase 1c's pass-splitting work
      may make this cheaper to land.
- [x] **Replace hand-written yEnc SIMD differential vectors with a property
      test.** *(Carried over from Phase 1b — not part of #121.)* Added
      `proptest` as a `pesto` dev-dependency and replaced the per-backend
      hand-written vector lists (SSSE3/AVX2/NEON × all-256-bytes, critical
      bytes, positional boundaries, short line lengths, large payload) with
      `all_backends_match_scalar`: one proptest over arbitrary `(data,
      line_len)` that checks every backend compiled for the target
      architecture — via `all_encoder_outputs`, so a new backend needs no new
      hand-written list — against `encode_scalar`, plus `encoded_size` and
      round-trip decoding. A deterministic 750 KB payload test remains for
      the widest SIMD chunk strides.

---

## Phase 3 — `upapasta` to feature parity

The remaining gap against the legacy Python version.

- [ ] **Watch mode** with smart rules and move-to-done logic — the single largest
      missing feature.
- [ ] **Metadata enrichment**: TMDb lookup, improved NFO generation.
- [ ] **First-run setup wizard.**
- [ ] Directory-level queuing (Space on a directory marks its files recursively).
- [ ] Auto-switch to Dashboard when an upload starts.
- [ ] Real pause (blocked on Phase 2's pesto API).
- [ ] Theme support (dark/light, user-configurable colors).
- [ ] TUI performance tuning during long uploads.
- [ ] Comprehensive error handling and user feedback.
- [ ] Migration path from the Python version, then retire/archive it.

---

## Phase 4 — `penne` and `sugo`

`penne` first — `sugo` builds on it.

- [ ] Multi-`.nzb` batch input (a queue of queues).
- [ ] Exit codes distinguishing "fully complete", "complete after repair" and
      "incomplete"; `--verbose`/`--quiet` matching `pesto`'s conventions.
- [ ] On-demand extra-volume fetching: today every `.par2` volume in the NZB is
      downloaded whether or not repair needs it.
- [ ] Parallel `verify()` — currently single-threaded and sequential; pairs
      naturally with #118's streaming rework.
- [ ] Double-buffered writer / buffer pool on the assembly path.
- [ ] Incremental extraction (`DirectUnpack`-style).
- [ ] Benchmark against a real indexer/provider pair.
- [ ] Add `penne` to the release workflow once its CLI surface is stable.

---

## Phase 5 — API stability and release engineering

Prerequisite for anything resembling 1.0.

- [ ] **Freeze the `pesto` public API surface.** `lib.rs` currently re-exports
      nearly every module. Decide what is genuinely public (`post`,
      `post_cancelable`, `upload::run_upload`, `config`, `progress`, `walk`) versus
      what is public only because `upapasta`/`penne` needed it, and mark the rest
      `#[doc(hidden)]` or move it behind a `internals` feature. Season PAR2
      (`generate_and_write_season_par2`) is public API today with a
      caller-supplied output directory and no contract about it — see #117.
- [ ] **`#![deny(missing_docs)]` on `parmesan`**, with runnable `# Examples` in
      `lib.rs`; `cargo doc --no-deps` clean.
- [ ] **Test strategy.** Property tests for the encoders (Phase 1b), a fixture
      corpus of real third-party `.par2` files (varying slice sizes, Unicode
      names, packet orders), an optional non-blocking CI job running the real
      `par2cmdline`, and a `cargo-fuzz` harness for `packet_reader.rs`.
- [ ] **Packaging.** Portable binaries for Linux (glibc + musl), Windows and
      macOS (including aarch64, which Phase 1b makes safe to ship); man pages via
      `clap_mangen`; confirm the release workflow publishes every artifact.
- [ ] **Documentation.** Complete flag tables and documented exit codes per crate;
      an `INTERNALS.md` for parmesan covering GF(2¹⁶) Reed-Solomon, the PAR2
      packet subset implemented, and the volume layout algorithm.

---

## Deferred / intentionally not implemented

Recorded so they are not re-litigated:

- **Posting order stays File-ID order** when `--par2 > 0` with multiple files. The
  PAR2 spec numbers input blocks by File-ID order and the producer streams slices
  in `metas` order; decoupling them would cost a second full read pass for no
  user-visible gain. Indexers group and sort by Subject, which Phase 48 already
  fixed independently. See `--file-counter`.
- **`--file-counter` stays off by default under `--obfuscate=full`/`paranoid`.** A
  stable release-wide `[N/M]` is exactly the correlation vector those modes exist
  to deny. On by default for `none`/`full-shared`, which already accept that
  correlation.
- **`DEFAULT_LINE_LENGTH` stays 128.** Raising to 256 was benchmarked and rejected.
- **The AVX2 yEnc encoder is not preferred over SSSE3** by the dispatcher —
  investigated and closed; SSSE3 wins on the measured hardware.
- **yEnc SIMD escaping / multi-line encoding** (PSHUFB in-place escape insertion,
  AVX2/AVX-512 multi-line) remain unscheduled ideas, not commitments.
- **Issue #68's indexer grouping** is closed as indexer-side. NZBIndex/Binsearch
  key "complete set" grouping off the `.volNNN+MMM.par2` filename pattern, not the
  subject counter. The subject-shape fix (always emit `(part/total)`) and
  `--par2-before-upload` both landed and are the actionable parts.

---

## References

- yEnc draft v1.3: <http://www.yenc.org/yenc-draft.1.3.txt>
  ([mirror](https://github.com/caronc/newsreap/blob/master/docs/yenc-draft.1.3.txt))
- RFC 3977 (NNTP), RFC 4643 (NNTP AUTHINFO)
- PAR2 specification: <https://parchive.sourceforge.net/docs/specifications/parity-volume-spec/article-spec.html>
