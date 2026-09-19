# Codebase simplification roadmap

This is the persistent execution plan for reducing the maintenance and context
cost of the Pesto workspace. It is intentionally more detailed than the active
product roadmaps: a later session should be able to resume the refactor from
this file without reconstructing the original analysis.

The refactor is developed on `refactor/codebase-simplification`. Work may be
merged in smaller pull requests, but every completed step must be reflected in
the progress log at the end of this document.

## Why this work exists

The workspace currently contains about 92,600 lines of Rust. The total is not
itself the problem. The maintenance cost comes from a small number of files
that mix several responsibilities and therefore force maintainers and coding
agents to load unrelated code into context.

Baseline measured on 2026-09-19:

| Crate | Rust lines | Files at least 1,000 lines |
|---|---:|---:|
| `pesto` | 47,585 | 8 |
| `parmesan` | 17,346 | 5 |
| `upapasta` | 12,299 | 3 |
| `penne` | 12,244 | 2 |
| `sugo` | 3,130 | 0 |

The most expensive general-purpose files are:

| File | Baseline lines | Main mixed responsibilities |
|---|---:|---|
| `crates/pesto/src/poster/mod.rs` | 5,947 | API, run lifecycle, producer, workers, PAR2, resume and season parity |
| `crates/pesto/src/bin/pesto.rs` | 4,993 | CLI, dispatch, upload, batch, watch, hooks and output |
| `crates/upapasta/src/app.rs` | 3,588 | State and behavior for almost every TUI feature |
| `crates/pesto/src/ui/terminal.rs` | 3,227 | State reduction, metrics and three renderers |
| `crates/upapasta/src/ui/mod.rs` | 2,885 | Rendering for every screen and overlay |
| `crates/upapasta/src/main.rs` | 2,817 | Event loop and all background operations |
| `crates/pesto/src/nfo.rs` | 2,174 | Detection, metadata, formatting and tests |

The five largest files contain roughly 842 KB of source. Splitting them along
behavioral boundaries reduces the context needed for a change even when it
does not immediately reduce the total line count.

## Outcomes and constraints

The desired outcome is not the largest possible number of small files. A good
boundary lets a maintainer understand and change one behavior without reading
unrelated subsystems.

Completion targets:

- a routine change normally requires reading no more than two to five related
  implementation files;
- general-purpose production files stay below 800 lines, with 400 to 600 lines
  preferred when a natural boundary exists;
- `main.rs`, `lib.rs` and `mod.rs` files act as entry points or facades and
  normally stay below 250 lines;
- new functions normally stay below 80 to 100 lines, and orchestration
  functions do not contain entire multi-stage workflows;
- public APIs remain stable while implementation files move;
- posting throughput and memory behavior remain within benchmark variance;
- tests never invoke real providers, hooks or other external side effects.

These are review signals, not reasons to damage cohesion. SIMD kernels,
constant tables, generated code and focused compatibility tests may remain
large when splitting would make the code harder to understand. Such exceptions
must be explicit in `scripts/source-size-baseline.txt`.

## Rules for every phase

1. Do not combine structural movement with behavior changes.
2. Preserve public paths with facade re-exports before considering API cleanup.
3. Move tests with the behavior they protect.
4. Prefer domain names over generic `common`, `helpers` or `utils` modules.
5. Confirm that duplicated helpers have identical semantics before sharing
   them.
6. Keep performance-sensitive allocation, buffering and concurrency decisions
   unchanged unless a separately measured task explicitly changes them.
7. Run the narrow crate checks during a step and the complete repository gates
   before committing a completed phase.
8. Lower or remove a source-size baseline entry whenever its file shrinks.
9. Update the progress log and the `Next action` field before ending a session.

Required complete gates:

```bash
bash scripts/check-source-size.sh
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all
```

Changes to the posting hot path additionally require the relevant benchmarks
or a documented reason that the change cannot affect generated code or runtime
behavior.

## Session resume procedure

At the beginning of every continuation session:

1. Read this document and `AGENTS.md`.
2. Check out `refactor/codebase-simplification` or the phase branch named in
   the progress log.
3. Run `git status --short` and preserve unrelated user changes.
4. Read the latest progress-log entry and the `Next action` field.
5. Inspect the target module and its neighboring modules before editing.
6. Run the narrowest useful baseline test before changing a hot path.
7. Complete one unchecked step at a time.
8. Update this roadmap with files moved, validation performed, decisions made
   and the exact next action.

When a planned boundary proves incorrect, update the plan before implementing
the replacement. The document should describe the current intended design,
not preserve an obsolete proposal for historical reasons.

## Phase 0 — Baseline and guardrails

Goal: make the simplification measurable and prevent the workspace from
gaining new oversized files while existing debt is removed incrementally.

Steps:

- [x] Record the workspace and hotspot baseline in this document.
- [x] Add `scripts/source-size-baseline.txt` for existing files over 800 lines.
- [x] Add `scripts/check-source-size.sh` to reject unrecorded oversized files
  and growth beyond the recorded baseline.
- [x] Run the size check from the repository pre-commit script.
- [x] Run the size check in the primary CI job.
- [x] Link this roadmap from the workspace roadmap.
- [x] Run all repository quality gates.
- [x] Commit Phase 0 independently.

Acceptance criteria:

- the current tree passes the guardrail;
- a new Rust file above 800 lines fails it;
- increasing a baselined file beyond its recorded size fails it;
- the guardrail does not require Rust or network access.

## Phase 1 — Decompose the Pesto CLI

Goal: make the binary entry point a small parser and dispatcher. This phase is
first because it offers high context reduction without changing the posting
engine's concurrency-sensitive internals.

Target layout (adjust names if inspection reveals a clearer domain boundary):

```text
crates/pesto/src/bin/pesto/
├── main.rs          # runtime construction, parsing and top-level dispatch
├── cli.rs           # Clap model and conversion to config overrides
├── command.rs       # command/mode selection
├── upload.rs        # one upload lifecycle
├── batch.rs         # --each and --season orchestration
├── watch.rs         # watch mode
├── stdin.rs         # stdin materialization
├── hooks.rs         # CLI-specific hook integration
├── output.rs        # NZB destination and conflict policy
├── summary.rs       # session reporting
└── tests/           # tests grouped with the owning behavior
```

Steps:

- [x] Inventory every top-level type and function in `bin/pesto.rs` and assign
  one owner in the target layout.
- [x] Extract CLI declarations and `Cli::overrides` without changing flags,
  help text or defaults.
- [x] Extract cleanup mode and its tests.
- [x] Extract NZB destination, archive-path and path-expansion behavior.
- [x] Extract hook environment construction and execution. Reuse
  `pesto::hooks` only where semantics are already identical.
- [x] Extract batch/season behavior.
  - [x] Extract input filtering, season NZB destination and release labels with
    their focused tests.
  - [x] Extract season PAR2 generation/upload with its progress regression
    test.
  - [x] Extract batch orchestration and remaining batch tests.
- [x] Extract watch behavior.
- [x] Convert `run_single_upload` into named stages with a small context/result
  type instead of a monolithic function.
- [ ] Reduce `run` to validation and mode dispatch.
- [ ] Keep the binary entry file below 250 lines.
- [ ] Run Pesto tests after every extraction and all gates at phase completion.

Do not redesign flags or hook behavior in this phase. Potential shared helpers
identified during analysis include `expand_tilde`, recursive size calculation,
session summaries and hook execution; they must only be consolidated after
their edge cases are compared.

### Phase 1 inventory

The line ranges below describe the Phase 0 version of `bin/pesto.rs`; they are
an ownership map, not stable source references.

| Original region | Responsibility | Intended owner |
|---|---|---|
| 34–805 | help text, `Cli`, and override resolution | `cli.rs` |
| 806–1,021 | cleanup policy and tests | `cleanup.rs` |
| 1,022–2,101 | upload context, result, timings and single-upload lifecycle | `upload.rs` |
| 2,102–2,453 | input filters, season PAR2 helpers and release labels | `batch.rs` |
| 2,454–2,840 | batch orchestration | `batch.rs` |
| 2,841–3,167 | watch state, retry and cleanup orchestration | `watch.rs` |
| 3,168–3,365 | merge-season command and season-name parsing | `merge.rs` |
| 3,366–3,419 | session report writing | `summary.rs` |
| 3,420–3,789 | runtime startup, validation and mode dispatch | `main.rs` / `command.rs` |
| 3,790–3,955 | compression roots, resume identity and upload summaries | `upload.rs` |
| 3,956–3,986 | welcome/header output | `command.rs` |
| 3,987–4,266 | hook environment and hook execution | `hooks.rs` |
| 4,267–4,328 | NZB destinations and path expansion | `output.rs` |
| 4,329–end | mixed unit tests | move beside each owning module |

The first extraction is `output.rs`: it is a leaf policy module with only
filesystem/config dependencies and validates the child-module visibility and
layout before moving stateful upload code.

## Phase 2 — Separate terminal state from rendering

Goal: turn terminal progress into a one-way data flow that can be understood
and tested without loading all renderers.

Target layout:

```text
crates/pesto/src/ui/
├── mod.rs
├── state.rs         # RenderState and persistent display state
├── reducer.rs       # ProgressEvent -> RenderState
├── metrics.rs       # rate, ETA and aggregate calculations
├── format.rs        # text, sizes and reusable bars
├── terminal/
│   ├── mod.rs       # channel loop and renderer selection
│   ├── panel.rs
│   ├── plain.rs
│   └── quiet.rs
└── tests/
```

Steps:

- [ ] Extract terminal unit tests from the implementation file and group them
  by state, metrics and renderer behavior.
- [ ] Extract pure formatting functions.
- [ ] Extract rate and ETA calculations.
- [ ] Move `RenderState` and construction into `state.rs`.
- [ ] Move `ProgressEvent` application into `reducer.rs`.
- [ ] Split quiet, panel and plain renderers.
- [ ] Leave the async render loop and renderer selection in `terminal/mod.rs`.
- [ ] Confirm snapshots/assertions cover equivalent output.
- [ ] Run Pesto tests and all gates.

The intended flow is:

```text
ProgressEvent -> pure reducer -> RenderState -> selected renderer
```

## Phase 3 — Decompose the posting engine

Goal: isolate the upload lifecycle, producer, workers and PAR2 policy while
preserving the exact concurrency and memory behavior of the mature hot path.

Target layout:

```text
crates/pesto/src/poster/
├── mod.rs             # facade and stable re-exports
├── api.rs             # public entry points
├── options.rs         # internal RunOptions/RunContext
├── outcome.rs         # public and internal result types
├── orchestrator.rs    # run lifecycle
├── shared.rs          # shared state and event emission
├── task.rs            # dispatcher, PostTask and ReadyArticle
├── producer.rs        # file reading and task production
├── worker.rs          # encode/post workers
├── result.rs          # commits, failures and final ordering
├── identity.rs        # names, Message-IDs and persisted identity
├── connections.rs     # connection split, checkout and return
├── par2/
│   ├── mod.rs
│   ├── geometry.rs
│   ├── memory.rs
│   ├── ingest.rs
│   └── season.rs
├── check.rs
└── tests/
```

Safe extraction order:

- [ ] Move tests out of `poster/mod.rs` without changing coverage.
- [ ] Extract public outcome types and pure decisions.
- [ ] Extract connection accounting and slot lifecycle.
- [ ] Extract identity and naming helpers.
- [ ] Extract PAR2 geometry and memory planning.
- [ ] Extract season PAR2 as an independent submodule.
- [ ] Extract task and shared-state types.
- [ ] Extract producer behavior.
- [ ] Extract worker and ready-article behavior.
- [ ] Express the main run as named stages in `orchestrator.rs`.
- [ ] Replace the long internal argument list with an internal `RunOptions`;
  keep existing public functions as compatibility facades.
- [ ] Verify resume, check/repost, pause/cancel and connection-reuse tests.
- [ ] Compare posting benchmarks and memory metrics with the baseline.
- [ ] Run all gates.

The orchestration should read approximately as:

```text
validate
  -> prepare run
  -> prepare inputs
  -> start pipeline
  -> await workers
  -> recover or repost
  -> persist resume state
  -> build outcome
```

No retry, queue-depth, allocation, buffering, PAR2 or connection policy change
belongs in this phase.

## Phase 4 — Organize UpaPasta by feature

Goal: stop requiring the three largest UpaPasta files for every TUI change.
State, effects and rendering remain separate, but each feature becomes easy to
locate by name.

Target layout:

```text
crates/upapasta/src/
├── main.rs
├── runtime.rs
├── app/
│   ├── mod.rs
│   ├── navigation.rs
│   ├── queue.rs
│   ├── upload.rs
│   ├── history.rs
│   ├── vault.rs
│   ├── watch.rs
│   ├── config.rs
│   └── prowlarr.rs
├── tasks/
│   ├── upload.rs
│   ├── watch.rs
│   ├── hooks.rs
│   └── prowlarr.rs
└── ui/
    ├── mod.rs
    ├── dashboard.rs
    ├── browser.rs
    ├── queue.rs
    ├── history.rs
    ├── vault.rs
    ├── watch.rs
    ├── config.rs
    └── overlays/
```

Steps:

- [ ] Extract one screen renderer per change, starting with low-coupling
  history and configuration screens.
- [ ] Extract overlays after their owning screens.
- [ ] Keep `ui/mod.rs` as screen dispatch only.
- [ ] Split feature-specific state and methods out of `app.rs` while retaining
  `App` as the root state.
- [ ] Move filesystem, upload, hook and indexer operations into `tasks/`.
- [ ] Keep the event loop responsible for dispatch rather than business logic.
- [ ] Review `AppEvent` after boundaries exist; group events only when doing so
  improves navigation and exhaustiveness.
- [ ] Run `cargo check -p upapasta`, Clippy and tests after each feature move.
- [ ] Run all gates.

The TUI must remain responsive: render modules never perform blocking work and
background results continue to arrive through events/channels.

## Phase 5 — Split medium-sized Pesto domains

Goal: complete the context reduction in protocol and formatting modules after
the highest-value hotspots are under control.

- [ ] Split `nfo.rs` into metadata model, detection/parsing and rendering.
- [ ] Split `nntp/mod.rs` into protocol/response parsing, authentication and
  client behavior while preserving `pool.rs`.
- [ ] Split `nzb.rs` into shared model, reader and writer.
- [ ] Group `config/types.rs` by configuration section.
- [ ] Separate public progress events from terminal-specific presentation.
- [ ] Review `compress.rs`, `resume.rs` and `upload.rs`; extract only natural
  boundaries rather than chasing the line limit.
- [ ] Run all gates.

After movement is complete, perform a separate public API audit:

- [ ] inventory current external users in UpaPasta, Penne and Sugo;
- [ ] replace broad `pub mod` exposure with deliberate re-exports where this
  can be done compatibly;
- [ ] use `pub(crate)` for implementation details;
- [ ] document and freeze the supported embedding surface.

## Phase 6 — Apply the policy to Penne and Parmesan

Goal: address remaining general-purpose hotspots without fragmenting focused
numeric code.

Penne:

- [ ] Split `bin/penne.rs` into CLI, dispatch and command modules.
- [ ] Split `check.rs` into planning, execution and reporting.
- [ ] Review `download.rs` and `assemble.rs` for existing pipeline-stage
  boundaries.
- [ ] Preserve failover, concurrency and end-to-end mock tests.

Parmesan:

- [ ] Split `create.rs` into planning, ingestion and packet writing if the
  resulting dependencies remain one-directional.
- [ ] Organize encoder tests by behavior/backend.
- [ ] Keep cohesive SIMD kernels and lookup tables on the explicit exception
  list.
- [ ] Split the CLI entry point from command behavior.

Run crate-specific checks during each step and all gates at phase completion.

## Phase 7 — Tighten and institutionalize the boundaries

Goal: convert the temporary no-growth baseline into a durable architecture
guardrail.

- [ ] Remove baseline entries as files fall below 800 lines.
- [ ] Classify remaining exceptions as SIMD, tables, generated or focused
  compatibility tests and document why each should stay large.
- [ ] Consider a lower warning threshold after the first six phases; do not
  fail historical 400–800-line modules without an identified boundary.
- [ ] Add concise module maps to complex subsystem facades.
- [ ] Verify entry points are navigational and dependency direction remains
  consistent with `docs/architecture.md`.
- [ ] Re-measure workspace files, hotspot concentration and common-task context
  size.
- [ ] Remove completed work from this active roadmap or archive it according to
  the repository roadmap policy.

## Progress log

### 2026-09-19 — Phase 0 started

- Branch: `refactor/codebase-simplification` from `main` at `44a0a6e`.
- Recorded the initial source and hotspot metrics.
- Added the persistent phase plan and session resume procedure.
- Added the no-growth source-size baseline and checker.
- Wired the checker into local pre-commit and CI.
- No production Rust behavior changed.

Validation completed:

- `bash scripts/check-source-size.sh`: passed with 26 baselined files.
- `cargo fmt --all -- --check`: passed.
- `cargo clippy --all-targets -- -D warnings`: passed.
- `cargo test --all`: passed, with only the repository's explicitly ignored
  tests skipped.

Decisions:

- The first guardrail uses 800 lines because it catches the current context
  hotspots without forcing arbitrary fragmentation of cohesive 400–800-line
  modules.
- Existing large files are debt baselines, not permanent exemptions.
- SIMD and tables are eligible for permanent exceptions only after the general
  refactors reach Phase 7.

Next action: completed in the following progress entry.

### 2026-09-19 — Phase 1 started

- Inventoried all top-level CLI types, functions and tests and assigned their
  intended modules in the Phase 1 inventory table.
- Added `crates/pesto/src/bin/pesto/output.rs` for NZB conflict resolution,
  archive destinations and tilde expansion.
- Added `crates/pesto/src/bin/pesto/cleanup.rs` for cleanup policy, watch-mode
  cleanup coordination and the ten focused regression tests.
- Moved the binary entry point to `crates/pesto/src/bin/pesto/main.rs`, its
  final module directory, and removed the temporary explicit module paths.
- Added `crates/pesto/src/bin/pesto/cli.rs` for all Clap declarations and
  conversion to configuration overrides. Only fields consumed by the parent
  dispatcher are visible outside the module.
- Added `crates/pesto/src/bin/pesto/hooks.rs` for the CLI's pre/post-hook
  environment, failure policy and platform command selection. The shared
  `pesto::hooks` module remains distinct because its reusable captured-output
  API does not expose pre-hook abort semantics or all CLI metadata variables.
- Added `crates/pesto/src/bin/pesto/batch.rs` for pure input filtering, season
  NZB destination policy and release labels, together with twelve focused
  tests.
- Added `crates/pesto/src/bin/pesto/season.rs` for season-wide PAR2 generation
  and its internal upload lifecycle, together with the progress regression
  test. The module remains separate from `batch.rs` so pure entry-selection
  policy does not depend on upload machinery.
- Moved the batch job scheduler, shared connection broker, consolidated season
  NZB/NFO/hooks flow and compression-temp cleanup guard into `batch.rs`.
  Focused batch tests now live in `batch/tests.rs`, keeping the production
  module below the 800-line guardrail without weakening cohesion.
- Added `crates/pesto/src/bin/pesto/watch.rs` for stability polling, retry
  state, concurrent dispatch and watch-specific cleanup coordination.
- Started `crates/pesto/src/bin/pesto/upload.rs` with the stable per-entry
  context/result types, phase timings, password policy and the first named
  lifecycle stage: behavior-neutral NZB/resume path planning. Password tests
  moved with their implementation.
- Moved compression-root selection, shared upload-root detection, resumable
  archive identity and resume-command fingerprint formatting into `upload.rs`,
  together with their fourteen focused tests.
- Added `upload/compression.rs` as a named lifecycle stage. It owns format
  resolution, resume archive reuse, temporary paths, progress polling,
  published archive names and compression timing while returning an explicit
  outcome to the remaining upload orchestration.
- Added `upload/artifacts.rs` for canonical NZB persistence, conflict-aware
  user destinations, hardlink/copy fallback, reported-path selection and
  history recording. Its request type makes the stage inputs explicit.
- Added `upload/completion.rs` for completion notifications, asynchronous NFO
  generation with heartbeat output, and post-upload hook environments built
  from the original pre-compression inputs.
- Added `upload/lifecycle.rs` as the orchestration boundary for one complete
  upload. `upload.rs` now acts as the facade and owns only shared upload types
  and planning policy; batch and watch callers retain their existing call
  signatures.
- Added `merge.rs` for the offline season-NZB merge command and moved its
  season-key tests beside the implementation. Added `summary.rs` for the
  final structured session-log record.
- Reduced the entrypoint from 4,993 to 567 lines and removed it from the
  source-size debt baseline. The extracted
  `cli.rs`, `batch.rs`, `batch/tests.rs`, `watch.rs`, `hooks.rs`, `season.rs`,
  `upload.rs`, `upload/compression.rs`, `upload/artifacts.rs`,
  `upload/completion.rs`, `upload/lifecycle.rs`, `merge.rs`, `summary.rs`,
  `output.rs` and `cleanup.rs` modules contain 776, 588, 224, 286, 280, 247,
  584, 172, 157, 179, 511, 209, 45, 67 and 230 lines respectively.

Validation completed:

- `bash scripts/check-source-size.sh`: passed.
- `cargo check -p pesto-poster --all-targets`: passed.
- `cargo test -p pesto-poster --bin pesto`: 52 passed.
- `pesto --help` before and after the CLI extraction: byte-identical.

Next action: split the remaining `run` workflow into validation/configuration
and mode dispatch in `command.rs`. Keep runtime construction and the final
top-level call in `main.rs`, preserve initialization order, and target an
entrypoint below 250 lines.
