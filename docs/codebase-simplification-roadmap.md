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
- [x] Reduce `run` to validation and mode dispatch.
- [x] Keep the binary entry file below 250 lines.
- [x] Run Pesto tests after every extraction and all gates at phase completion.

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
│   ├── quiet.rs
│   ├── summary.rs
│   ├── tests.rs
│   └── tests/        # renderer, state and outcome regressions
```

Steps:

- [x] Extract terminal unit tests from the implementation file and group them
  by state, metrics and renderer behavior.
- [x] Extract pure formatting functions.
- [x] Extract rate and ETA calculations.
- [x] Move `RenderState` and construction into `state.rs`.
- [x] Move `ProgressEvent` application into `reducer.rs`.
- [x] Split quiet, panel and plain renderers.
- [x] Leave the async render loop and renderer selection in `terminal/mod.rs`.
- [x] Confirm snapshots/assertions cover equivalent output.
- [x] Run Pesto tests and all gates.

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

- [x] Move tests out of `poster/mod.rs` without changing coverage.
- [x] Extract public outcome types and pure decisions.
- [x] Extract connection accounting and slot lifecycle.
- [x] Extract identity and naming helpers.
- [x] Extract PAR2 geometry and memory planning.
- [x] Extract season PAR2 as an independent submodule.
- [x] Extract task and shared-state types.
- [x] Extract producer behavior.
- [x] Extract worker and ready-article behavior.
- [x] Move the run entry point into `poster/orchestrator.rs`.
- [x] Extract run preparation, resume persistence and outcome as named stages.
- [x] Extract the cancel watcher, pipeline startup and worker join as named
  stages.
- [x] Extract the check/recovery block as a named stage inside
  `orchestrator.rs`.
- [x] Replace the long internal argument list with an internal `RunOptions`;
  keep existing public functions as compatibility facades.
- [x] Verify resume, check/repost, pause/cancel and connection-reuse tests.
- [x] Compare posting benchmarks and memory metrics with the baseline.
- [x] Run all gates.

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
│   ├── confirm.rs
│   └── prowlarr.rs
├── tasks/
│   ├── upload.rs
│   ├── progress.rs
│   ├── season.rs
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

- [x] Extract one screen renderer per change, starting with low-coupling
  history and configuration screens.
  - [x] Extract the history screen renderer.
  - [x] Extract the configuration screen renderer.
  - [x] Extract the watch screen renderer.
  - [x] Extract the NZB Vault screen renderer.
  - [x] Extract the queue screen renderer.
  - [x] Extract the Browser screen renderer.
  - [x] Extract the Dashboard screen renderer.
- [x] Extract overlays after their owning screens.
  - [x] Extract the History NZB viewer overlay.
  - [x] Extract the Vault viewer overlay.
  - [x] Extract the Prowlarr overlays.
  - [x] Extract the hook picker overlay.
- [x] Keep `ui/mod.rs` as screen dispatch only.
- [x] Split feature-specific state and methods out of `app.rs` while retaining
  `App` as the root state.
  - [x] Move the feature state types into an `app/` module directory, leaving
    `App` in `app/mod.rs` and re-exporting the same public paths.
  - [x] Move each feature's `impl App` methods next to its state module.
- [x] Move filesystem, upload, hook and indexer operations into `tasks/`.
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
- Added `command.rs` for CLI validation, configuration resolution, logging
  initialization and top-level mode dispatch. The command workflow now uses
  named stdin materialization, config loading, cleanup-policy and upload-mode
  dispatch stages instead of one monolithic function.
- Completed Phase 1 with the entrypoint reduced from 4,993 to 172 lines and
  removed it from the
  source-size debt baseline. The extracted
  `cli.rs`, `command.rs`, `batch.rs`, `batch/tests.rs`, `watch.rs`, `hooks.rs`,
  `season.rs`, `upload.rs`, `upload/compression.rs`, `upload/artifacts.rs`,
  `upload/completion.rs`, `upload/lifecycle.rs`, `merge.rs`, `summary.rs`,
  `output.rs` and `cleanup.rs` modules contain 776, 399, 588, 224, 286, 280,
  247, 584, 172, 157, 179, 511, 209, 45, 67 and 230 lines respectively.

Validation completed:

- `bash scripts/check-source-size.sh`: passed.
- `cargo check -p pesto-poster --all-targets`: passed.
- `cargo test -p pesto-poster --bin pesto`: 52 passed.
- `cargo fmt --all -- --check`: passed at Phase 1 completion.
- `cargo clippy --all-targets -- -D warnings`: passed at Phase 1 completion.
- `cargo test --all`: passed at Phase 1 completion, with only the repository's
  explicitly ignored tests skipped.
- `pesto --help` before and after the CLI extraction: byte-identical.

Next action: completed in the following progress entry.

### 2026-09-19 — Phase 2 completed

- Moved all 32 terminal regression tests out of the 3,227-line production
  module into `ui/terminal/tests.rs` and `ui/terminal/tests/outcomes.rs`.
  The first group covers layout and progress rendering; the second covers
  verification, recovery and final outcomes.
- Reduced `ui/terminal.rs` to 2,308 lines and lowered its source-size debt
  baseline accordingly. No state, rendering or async-loop behavior changed.
- Added `ui/format.rs` for pure panel sizing, dual-band bars, wrapped notes,
  plain-mode ANSI removal, status labels and IEC byte formatting. Moved the
  focused bar-width regression test with the policy and reduced
  `ui/terminal.rs` further to 2,144 lines.
- Added `ui/metrics.rs` for progress fractions, rates, chronological speed
  samples, confidence-aware ETA ranges, phase estimates and overall ETA
  selection. `RenderState` retains sample collection while delegating pure
  calculations, reducing `ui/terminal.rs` to 2,097 lines.
- Added `ui/state.rs` for `RenderState`, connection state and construction
  defaults. Fields remain visible only inside `ui`; the production terminal
  module was reduced to 1,819 lines at this step.
- Added `ui/reducer.rs` for all `ProgressEvent` to `RenderState` transitions.
  The existing crate-private `RenderState::apply` call sites remain unchanged,
  while `ui/terminal.rs` is reduced further to 1,451 lines.
- Split terminal output into `terminal/panel.rs`, `plain.rs`, `quiet.rs` and
  `summary.rs`. The 125-line `terminal/mod.rs` now owns only terminal setup,
  channel processing, renderer selection and adaptive refresh timing.
- Moved derived state projections beside `RenderState`. Production files now
  contain 657 lines in `panel.rs`, 471 in `state.rs`, 373 in `reducer.rs`, 216
  in `plain.rs`, 172 in `summary.rs` and 127 in `quiet.rs`; no terminal UI
  production file remains on the source-size debt baseline.
- Grouped terminal regressions into renderer, state and outcome modules, with
  pure metric tests remaining beside `metrics.rs`. The focused UI suite now
  contains 48 passing tests, including direct state-transition assertions.

Validation completed:

- `cargo check -p pesto-poster --all-targets`: passed.
- `cargo clippy -p pesto-poster --all-targets -- -D warnings`: passed.
- `cargo test -p pesto-poster --lib ui::terminal::tests`: 32 passed.
- `cargo test -p pesto-poster --lib ui::`: 46 passed after the state
  extraction.
- `cargo test -p pesto-poster --lib ui::`: 46 passed after the reducer
  extraction.
- `bash scripts/check-source-size.sh`: passed.
- `cargo fmt --all -- --check`: passed after the state extraction.
- `cargo clippy --all-targets -- -D warnings`: passed after the state
  extraction.
- `cargo test --all`: passed after the state extraction, with only the
  repository's explicitly ignored tests skipped.
- `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D warnings`
  and `cargo test --all`: passed after the reducer extraction.
- `cargo test -p pesto-poster --lib ui::`: 48 passed after the renderer and
  test-suite split.
- `bash scripts/check-source-size.sh`, `cargo fmt --all -- --check`,
  `cargo clippy --all-targets -- -D warnings` and `cargo test --all`: passed at
  Phase 2 completion.

Next action: completed in the following progress entry.

### 2026-09-19 — Phase 3 started

- Inventoried all 76 unit tests embedded in `poster/mod.rs` and moved them to
  `poster/tests/`, grouped as `memory.rs` (7), `paths.rs` (14), `par2.rs` (11),
  `policy.rs` (25), `dry_run.rs` (6) and `internals.rs` (13). Shared fixtures
  live in the 39-line `tests/mod.rs` facade.
- Reduced the production `poster/mod.rs` from 5,947 to 4,856 lines without
  changing implementation code or test coverage, and lowered its source-size
  debt baseline accordingly.
- Added `poster/outcome.rs` for `PostOutcome`, `PostedSegment`, `FailedTask`
  and the pure NZB publication decisions. The facade preserves every existing
  `pesto::poster::*` path through re-exports, while the six policy tests now
  live beside their owner.
- Reduced `poster/mod.rs` further to 4,631 lines and lowered its source-size
  debt baseline again.
- Added `poster/connections.rs` for the upload/check budget split, broker
  checkout and whole-set release lifecycle. Its seven boundary tests now live
  beside the policy, and the existing broker-reuse and posting-server
  integration tests remain unchanged.
- Reduced `poster/mod.rs` further to 4,570 lines and lowered its source-size
  debt baseline again.
- Added `poster/identity.rs` for persisted wire identity, posting-group
  selection, client-path normalization, PAR2/yEnc naming and article-date
  resolution. The public `pick_post_group` path remains stable through the
  facade, and 24 focused tests now live beside these rules.
- Reduced `poster/mod.rs` further to 4,408 lines and lowered its source-size
  debt baseline again.

Validation completed:

- `cargo check -p pesto-poster --all-targets`: passed.
- `cargo clippy -p pesto-poster --all-targets -- -D warnings`: passed.
- `cargo test -p pesto-poster --lib poster::tests::`: 76 passed.
- `bash scripts/check-source-size.sh`: passed.
- `cargo fmt --all -- --check`: passed.
- `cargo clippy --all-targets -- -D warnings`: passed.
- `cargo test --all`: passed, with only the repository's explicitly ignored
  tests skipped.
- `cargo check -p pesto-poster --all-targets`: passed after the outcome
  extraction.
- `cargo clippy -p pesto-poster --all-targets -- -D warnings`: passed after
  the outcome extraction.
- `cargo test -p pesto-poster --lib poster::outcome::tests`: 6 passed.
- `bash scripts/check-source-size.sh`, `cargo fmt --all -- --check`,
  `cargo clippy --all-targets -- -D warnings` and `cargo test --all`: passed
  after the outcome extraction.
- `cargo test -p pesto-poster --lib poster::connections::tests`: 7 passed.
- `cargo test -p pesto-poster --test each_reuses_connections_across_episodes`:
  3 passed.
- `cargo test -p pesto-poster --test check_targets_posting_server`: 1 passed.
- `cargo clippy -p pesto-poster --all-targets -- -D warnings`: passed after
  the connection extraction.
- `bash scripts/check-source-size.sh`, `cargo fmt --all -- --check`,
  `cargo clippy --all-targets -- -D warnings` and `cargo test --all`: passed
  after the connection extraction.
- `cargo test -p pesto-poster --lib poster::identity::tests`: 24 passed.
- `cargo test -p pesto-poster --test full_shared_obfuscation`: 7 passed.
- `cargo test -p pesto-poster --test independent_obfuscation_tokens`: 3
  passed.
- `cargo test -p pesto-poster --test resume_confirm`: 7 passed.
- `cargo clippy -p pesto-poster --all-targets -- -D warnings`: passed after
  the identity extraction.
- `bash scripts/check-source-size.sh`, `cargo fmt --all -- --check`,
  `cargo clippy --all-targets -- -D warnings` and `cargo test --all`: passed
  after the identity extraction.

Next action: completed in the following progress entry.

### 2026-09-20 — Phase 3 completed: named recovery and benchmark comparison

- Added `recover_or_repost` and its compact `RecoveryOutcome` to
  `poster/orchestrator.rs`. The run entry point now delegates the blind retry,
  streaming STAT drain, bounded tail recovery, slot release and stale resume
  record cleanup as one named stage before resume persistence and outcome
  construction.
- The extraction preserves the existing retry decisions, event order,
  connection-slot ownership and resume mutations. `run` is now 347 lines and
  reads as the lifecycle documented above; `orchestrator.rs` remains below the
  source-size limit at 640 lines.
- Compared the changed release binary with an independently built `HEAD`
  control using `bench/run.sh stages --workload mixed-folder --scale 1.0
  --reps 3 --yes` on the same medialab host. The 2.0 GiB posting-only median
  was 2,269.3 versus 2,266.8 MiB/s (+0.1%), with 70.6 versus 80.3 MiB peak
  RSS. The `post+check` median was 283.5 versus 280.5 MiB/s (+1.1%), with
  717.0 versus 725.8 MiB peak RSS. Both throughput deltas are within measured
  noise and memory did not regress. The CPU governor was `powersave`, so these
  results are treated as a same-session regression check rather than a new
  publishable performance baseline.

Validation completed:

- `cargo clippy -p pesto-poster --all-targets -- -D warnings`: passed.
- Focused check/recovery, pause/cancel and connection-reuse integration tests:
  12 passed.
- `bash scripts/check-source-size.sh`: passed with 23 baselined files.
- `cargo fmt --all -- --check`: passed.
- `cargo clippy --all-targets -- -D warnings`: passed.
- `XDG_CONFIG_HOME=/tmp/pesto-test-empty-config cargo test --all`: passed,
  with only explicitly ignored tests skipped. The isolated config is required
  because `batch_order` otherwise discovers the developer's real Pesto config
  and attempts proxy validation instead of remaining self-contained.

Next action: completed in the following progress entry.

### 2026-09-20 — Phase 4 started: history renderer

- Added `crates/upapasta/src/ui/history.rs` as the owner of the History screen
  layout, search bar, upload list, selected-record detail and catalog stats.
- Kept the existing root `App` state and shared rendering policies; the new
  module imports the existing byte formatting, category color and Unicode-safe
  truncation helpers rather than duplicating them.
- `ui/mod.rs` now dispatches the History state to `history::draw` and shrank
  from 2,885 to 2,644 lines. Lowered its source-size debt baseline to match;
  the focused `history.rs` is 245 lines.
- The NZB viewer overlay remains in `ui/mod.rs` for now and will move with the
  overlay pass, after its owning History screen boundary is established.

Validation completed:

- `cargo check -p upapasta`: passed.
- `cargo clippy -p upapasta --all-targets -- -D warnings`: passed.
- `cargo test -p upapasta`: 38 passed.
- `bash scripts/check-source-size.sh`: passed with 23 baselined files.
- `cargo fmt --all -- --check` and `git diff --check`: passed.

Next action: completed in the following progress entry.

### 2026-09-20 — Phase 4 continued: configuration renderer

- Added `crates/upapasta/src/ui/config.rs` for the configuration field model,
  effective server and Prowlarr status panel, override indicators, editing
  presentation and selection state.
- `ui/mod.rs` now delegates `AppState::Config` to `config::draw` and shrank
  from 2,644 to 2,329 lines. Lowered its source-size debt baseline again; the
  extracted configuration module is 327 lines.
- Preserved the existing `App`/`ConfigState` ownership and every field order,
  default, mask, hint, color and override count. No configuration persistence
  or input handling moved in this renderer-only step.

Validation completed:

- `cargo check -p upapasta`: passed.
- `cargo clippy -p upapasta --all-targets -- -D warnings`: passed.
- `cargo test -p upapasta`: 38 passed.
- `bash scripts/check-source-size.sh`: passed with 23 baselined files.
- `cargo fmt --all -- --check` and `git diff --check`: passed.

Next action: completed in the following progress entry.

### 2026-09-20 — Phase 4 continued: Watch renderer

- Added `crates/upapasta/src/ui/watch.rs` for Watch field projection, editing
  presentation and the live uploading/stabilizing/queued status panels.
- `ui/mod.rs` now delegates `AppState::Watch` to `watch::draw` and shrank from
  2,329 to 2,145 lines. Lowered its source-size debt baseline again; the new
  renderer is 196 lines.
- Filesystem scanning, stability tracking, queueing and event handling remain
  outside the renderer. Field order, hints, selection, colors and five-entry
  status limits are unchanged.

Validation completed:

- `cargo check -p upapasta`: passed.
- `cargo clippy -p upapasta --all-targets -- -D warnings`: passed.
- `cargo test -p upapasta`: 38 passed.
- `bash scripts/check-source-size.sh`: passed with 23 baselined files.
- `cargo fmt --all -- --check` and `git diff --check`: passed.

Next action: completed in the following progress entry.

### 2026-09-20 — Phase 4 continued: NZB Vault renderer

- Added `crates/upapasta/src/ui/vault.rs` for the Vault file list, origin and
  catalog markers, sorting label, selection state and parsed NZB detail panel.
- `ui/mod.rs` now delegates `AppState::NzbVault` to `vault::draw` and shrank
  from 2,145 to 1,945 lines. Lowered its source-size debt baseline again; the
  new renderer is 206 lines.
- Kept Vault loading, parsing, deletion and input handling outside the
  renderer. The Vault viewer remains in `ui/mod.rs` for the later overlay pass.

Validation completed:

- `cargo check -p upapasta`: passed.
- `cargo clippy -p upapasta --all-targets -- -D warnings`: passed.
- `cargo test -p upapasta`: 38 passed.
- `cargo fmt --all -- --check` and `git diff --check`: passed.

Next action: completed in the following progress entry.

### 2026-09-20 — Phase 4 continued: Queue renderer

- Added `crates/upapasta/src/ui/queue.rs` for the full-height Queue screen and
  its reusable upload configuration panel.
- `ui/mod.rs` now delegates `AppState::Queue` to `queue::draw`; the Browser
  reuses `queue::draw_upload_config_panel`. The main UI module shrank from
  1,945 to 1,685 lines and its source-size debt baseline was lowered again;
  the new Queue renderer is 274 lines.
- Queue mutations, upload startup and all input handling remain outside the
  renderer. Empty-state text, totals, status glyphs, field order and hints are
  unchanged.

Validation completed:

- `cargo check -p upapasta`: passed.
- `cargo clippy -p upapasta --all-targets -- -D warnings`: passed.
- `cargo test -p upapasta`: 38 passed.
- `cargo fmt --all -- --check` and `git diff --check`: passed.

Next action: completed in the following progress entry.

### 2026-09-20 — Phase 4 continued: Browser renderer

- Added `crates/upapasta/src/ui/browser.rs` for the file-tree layout, NZB
  status/detail panel and compact queue summary.
- `ui/mod.rs` now delegates `AppState::Browser` to `browser::draw` and shrank
  from 1,685 to 1,307 lines. Lowered its source-size debt baseline again; the
  new Browser renderer is 392 lines.
- Continued to reuse `queue::draw_upload_config_panel` when confirmation is
  open. Navigation, background sizing, catalog lookups, queue mutations and
  input handling remain outside the renderer.

Validation completed:

- `cargo check -p upapasta`: passed.
- `cargo clippy -p upapasta --all-targets -- -D warnings`: passed.
- `cargo test -p upapasta`: 38 passed.
- `cargo fmt --all -- --check` and `git diff --check`: passed.

Next action: completed in the following progress entry.

### 2026-09-20 — Phase 4 continued: Dashboard renderer

- Added `crates/upapasta/src/ui/dashboard.rs` for the idle Dashboard, effective
  upload settings, stage gauges, speed history and per-file progress view.
- `ui/mod.rs` now delegates `AppState::Dashboard` to `dashboard::draw` and
  shrank from 1,307 to 812 lines. Lowered its source-size debt baseline again;
  the new Dashboard renderer is 509 lines.
- All screen renderers in the target Phase 4 layout now have dedicated
  modules. Upload orchestration, progress updates, pause/cancel behavior and
  input handling remain outside the renderer.

Validation completed:

- `cargo check -p upapasta`: passed.
- `cargo clippy -p upapasta --all-targets -- -D warnings`: passed.
- `cargo test -p upapasta`: 38 passed.
- `cargo fmt --all -- --check` and `git diff --check`: passed.

Next action: start the overlay pass by extracting the History NZB viewer into
`ui/overlays/nzb_viewer.rs`; this will bring `ui/mod.rs` below the 800-line
source-size limit and remove its debt baseline.

### 2026-09-20 — Phase 4 continued: overlay renderers

- Added `crates/upapasta/src/ui/overlays/` as the floating-overlay module:
  `nzb_viewer.rs` (History NZB archive viewer), `vault_viewer.rs` (NZB Vault
  file viewer), `prowlarr.rs` (queue batch-search and search/detail overlays)
  and `hook_picker.rs` (per-release hook picker).
- `ui/mod.rs` now dispatches every overlay to `overlays::*` from `draw` and
  `draw_main`, and shrank from 812 to 323 lines. It dropped below the 800-line
  general-purpose limit, so its source-size debt baseline entry was removed;
  the workspace baseline fell from 23 to 22 entries.
- Only `centered_rect`, `format_bytes`, `category_color` and `truncate_str`
  remain in `ui/mod.rs` as shared rendering helpers. Overlay content, layout
  strings, colors, scroll math and list highlighting are byte-for-byte
  unchanged; no state or input handling moved.

Validation completed:

- `cargo check -p upapasta`: passed.
- `cargo clippy -p upapasta --all-targets -- -D warnings`: passed.
- `cargo test -p upapasta`: 38 passed.
- `cargo fmt --all -- --check`, `git diff --check` and
  `bash scripts/check-source-size.sh`: passed.

Next action: keep `ui/mod.rs` as screen dispatch only by relocating the shared
rendering helpers into a dedicated `ui/` helper module, then begin moving
feature state out of `app.rs`.

### 2026-09-20 — Phase 4 continued: rendering helpers and `app/` state modules

- Added `crates/upapasta/src/ui/helpers.rs` for the shared `truncate_str`,
  `format_bytes`, `centered_rect` and `category_color` helpers and their three
  truncation tests. Every screen and overlay now imports them from
  `ui::helpers`; `ui/mod.rs` is 240 lines and holds only screen/overlay
  dispatch, the top bar, the status bar and the small-terminal fallback.
- Converted `crates/upapasta/src/app.rs` into `app/mod.rs` with feature state
  modules: `queue.rs`, `vault.rs`, `prowlarr.rs`, `history.rs`, `config.rs`,
  `watch.rs` and `hook_picker.rs`. `App` stays the root state in `mod.rs` and
  re-exports the same public paths (`app::VaultState`, `app::queue_entry_info`,
  `app::dir_stats`, …) so no caller changed.
- Moved the types verbatim; only `WatchSettings` became `pub(super)` and
  `dir_stats` is re-exported `pub(crate)`. `app/mod.rs` shrank from 3,588 to
  3,057 lines and its source-size debt baseline was lowered accordingly.
- The feature `impl App` methods still live in `app/mod.rs` and are the next
  extraction target.

Validation completed:

- `cargo check -p upapasta`, `cargo clippy -p upapasta --all-targets -- -D
  warnings` and `cargo test -p upapasta` (38 passed): passed.
- `cargo fmt --all -- --check`, `git diff --check` and
  `bash scripts/check-source-size.sh`: passed.

Next action: move the feature-specific `impl App` methods (queue, vault,
prowlarr, history, config, watch, hooks) alongside their state modules in
`app/`, then start extracting filesystem/upload/hook/indexer work into
`tasks/`.

### 2026-09-20 — Phase 4 continued: Watch methods moved

- Moved the Watch-mode `impl App` methods into `app/watch.rs`, beside
  `WatchState`: field navigation/editing, enable toggling, scan folding,
  watch-triggered upload start/finish, the done-dir move, and settings
  load/save. `move_watch_item_to_done` stays private to the module; the public
  entry points keep their signatures, so the event loop is unchanged.
- `app/mod.rs` shrank from 3,057 to 2,768 lines and its source-size debt
  baseline was lowered again. `WATCH_FIELD_COUNT` is no longer re-exported
  because only `watch.rs` uses it.
- The Watch group was chosen first because its private helper is used only
  inside the group; the remaining feature methods still share helpers with the
  root `App` and need the same check before each move.

Validation completed:

- `cargo check -p upapasta`, `cargo clippy -p upapasta --all-targets -- -D
  warnings` and `cargo test -p upapasta` (38 passed): passed.
- `cargo fmt --all -- --check`, `git diff --check` and
  `bash scripts/check-source-size.sh`: passed.

Next action: move the Vault methods (`load_vault`, `vault_parse_selected`,
`vault_open_viewer`) into `app/vault.rs`, then continue with history, config,
queue and hooks.

### 2026-09-20 — Phase 4 continued: Vault, History and Queue methods moved

- Moved the Vault methods (`load_vault`, `vault_parse_selected`,
  `vault_open_viewer`) into `app/vault.rs`.
- Moved the History methods (`refresh_history`, `refresh_stats`,
  `history_select_next/prev`, `open_nzb_viewer`, `close_nzb_viewer`,
  `nzb_viewer_scroll_*`) into `app/history.rs`.
- Moved the Queue methods (`toggle_queue_at_cursor`, `sync_queue_badges`,
  `queue_info`, `take_pending_meta`, `apply_queue_meta`, `remove_queue_selected`,
  `clear_queue`) into `app/queue.rs`, beside the queue metadata helpers.
- Each group keeps its public method signatures; only private helpers that are
  used solely within a group moved with it. `app/mod.rs` shrank from 2,768 to
  2,486 lines and its source-size debt baseline was lowered again.

Validation completed:

- `cargo check -p upapasta`, `cargo clippy -p upapasta --all-targets -- -D
  warnings` and `cargo test -p upapasta` (38 passed): passed.
- `cargo fmt --all -- --check`, `git diff --check` and
  `bash scripts/check-source-size.sh`: passed.

Next action: move the Config-screen and confirm-panel methods into
`app/config.rs`, then extract upload/hook/indexer work into `tasks/`.

### 2026-09-20 — Phase 4 continued: navigation module

- Moved `AppState` and the `next_tab`/`prev_tab` methods into
  `app/navigation.rs`, matching the target `app/` layout. `AppState` is
  re-exported at the same path, so no caller changed.
- `app/mod.rs` shrank from 2,486 to 2,441 lines and its source-size debt
  baseline was lowered again.
- Remaining `impl App` groups are the Config/confirm panel (largest, shares
  effective-value helpers with upload) and the upload/hook/indexer methods,
  which are the next targets.

Validation completed:

- `cargo check -p upapasta`, `cargo clippy -p upapasta --all-targets -- -D
  warnings` and `cargo test -p upapasta` (38 passed): passed.
- `cargo fmt --all -- --check`, `git diff --check` and
  `bash scripts/check-source-size.sh`: passed.

Next action: move the Config-screen and confirm-panel methods into
`app/config.rs`, promoting the shared effective-value helpers to
`pub(super)` where upload also needs them.

### 2026-09-20 — Phase 4 continued: Config, confirm and upload methods

- Moved the Config-screen methods into `app/config.rs` (field navigation,
  editing, reset, effective config/folder mode, and the prefs persistence).
- Split the upload confirmation panel into `app/confirm.rs`: field order,
  effective values, cycling, increment/decrement, edit commit/cancel and the
  field views.
- Moved the live upload methods into `app/upload.rs`: `trigger_upload`,
  per-item status tracking, NZB-disk/hook index refresh, catalog recording,
  upload start/finish, progress handling, pause/cancel and
  `effective_upload_settings`.
- Queue persistence (`save_queue`/`load_queue`) went to `app/queue.rs`.
- `app/mod.rs` shrank from 2,441 to 1,139 lines and its source-size debt
  baseline was lowered again. Every `app/` file is now under the 800-line
  limit. What remains in `mod.rs` is `App`'s fields, `App::new`, the
  `UploadProgress` type, the shared free functions and the tests.
- The target `app/` layout gained a `confirm.rs` entry for the upload
  confirmation panel, which is a distinct screen concern from the Config
  screen.

Validation completed:

- `cargo check -p upapasta`, `cargo clippy -p upapasta --all-targets -- -D
  warnings` and `cargo test -p upapasta` (38 passed): passed.
- `cargo fmt --all -- --check`, `git diff --check` and
  `bash scripts/check-source-size.sh`: passed.

Next action: extract filesystem, upload, hook and indexer operations from
`app/mod.rs` and `main.rs` into `tasks/`, beginning with the watch scan and
queue sizing jobs.

### 2026-09-20 — Phase 4 continued: background tasks extracted

- Added `crates/upapasta/src/tasks/` and moved the background operations out of
  `main.rs`:
  - `tasks/watch.rs`: directory scanning, entry sizing and watch upload
    dispatch (with the watch scan tests).
  - `tasks/hooks.rs`: hook picker resolution and single-hook execution.
  - `tasks/prowlarr.rs`: connection check, search, queue search and download.
  - `tasks/upload.rs`: upload trigger, dry-run config, season hooks and the
    real upload pipeline (with the season gate tests kept in `season.rs`).
  - `tasks/progress.rs`: progress event formatting and the session summary.
  - `tasks/season.rs`: the shared season-pack write/skip gate.
- `main.rs` now contains only `main` and the `run_app` event loop; it shrank
  from 2,817 to 912 lines and its source-size debt baseline was lowered. Every
  `tasks/` file is under the 800-line guardrail.
- Functions are `pub(crate)` only where the event loop calls them; the moved
  bodies are unchanged.

Validation completed:

- `cargo check -p upapasta`, `cargo clippy -p upapasta --all-targets -- -D
  warnings` and `cargo test -p upapasta` (38 passed): passed.
- `cargo fmt --all -- --check`, `git diff --check` and
  `bash scripts/check-source-size.sh`: passed.

Next action: reduce `run_app` to dispatch by moving its per-event handling into
focused functions or a `runtime` module, then review `AppEvent`.

### 2026-09-19 — Phase 3 continued: named pipeline join

- Added `poster/pipeline.rs` (247 lines) for the posting pipeline machinery:
  `spawn_cancel_watcher`, the `Pipeline` struct and `start_pipeline`, plus
  `run_pipeline` which runs the producer or pre-generated-release poster, joins
  the encode and POST workers and returns the force-abort/failure state and
  recovered POST slots.
- `orchestrator.rs` shrank from 797 to 597 lines; `pipeline.rs` is well under
  the 800-line limit and the source-size baseline stays at 23 entries. All
  `run` body state, cancellation semantics and join ordering are unchanged.
- The check/recovery block (blind retry, streaming STAT drain and automatic
  tail recovery) is the last inline stage and remains the documented next step.

Validation completed:

- `cargo check -p pesto-poster --all-targets`: passed.
- `cargo clippy -p pesto-poster --all-targets -- -D warnings`: passed.
- `cargo test -p pesto-poster`: all 34 test binaries passed.
- `cargo fmt --all -- --check`: passed.
- `bash scripts/check-source-size.sh`: passed with 23 baselined files.

Next action: extract the check/recovery block from `run` into a named stage,
then run the posting benchmarks and memory measurements against the Phase 0
baseline.

### 2026-09-19 — Phase 3 continued: named pipeline startup

- Added `spawn_cancel_watcher` for the external cancel/pause flag forwarding
  and `start_pipeline` plus its `Pipeline` struct for spawning the POST worker
  and yEnc encode pools. Both were lifted verbatim from `run`; channel depths,
  round-robin dispatch, buffer-pool wiring and task handles are unchanged.
- `run` now names: cancel watcher, `--par2-before-upload` generation, slot
  checkout, check coordinator, pipeline startup, producer/join, retry and
  recovery, persist, outcome. The worker-join and check/recovery blocks remain
  inline and are the last staging step.
- `poster/orchestrator.rs` is 797 lines, still below the general-purpose
  800-line limit, and the source-size baseline remains at 23 entries.

Validation completed:

- `cargo check -p pesto-poster --all-targets`: passed.
- `cargo clippy -p pesto-poster --all-targets -- -D warnings`: passed.
- `cargo test -p pesto-poster`: all 34 test binaries passed.
- `cargo fmt --all -- --check`: passed.
- `bash scripts/check-source-size.sh`: passed with 23 baselined files.

Next action: extract the worker join and check/recovery blocks from `run` into
named stages, then run the posting benchmarks and memory measurements against
the Phase 0 baseline.

### 2026-09-19 — Phase 3 continued: named run preparation and finish stages

- Added `poster/prepare.rs` (495 lines) for the preparation stages:
  `prepare_resume` (resume/spool validation and the shared release identity),
  `prepare_inputs` (per-file metadata, resume fingerprints, published names,
  File-ID ordering and `--file-counter` numbering) and `prepare_resources`
  (proxy validation, connection split, worker sizing, buffer pre-fill and PAR2
  geometry) as `RunResources`.
- Named the finishing stages in `poster/result.rs`: `persist_resume_state`
  (the single incomplete-run persistence decision) and `build_outcome` (final
  event, natural segment ordering and `PostOutcome`).
- `run` now reads as: validate -> `prepare_resume` -> `prepare_inputs` ->
  `prepare_resources` -> build `Shared` -> announce plan -> start pipeline ->
  await workers -> recover or repost -> `persist_resume_state` ->
  `build_outcome`. The preparation and finish stages are named functions; the
  pipeline start/join/recovery blocks are still inline and remain the next
  extraction. Order, logging, event emissions and every value are unchanged.
- `poster/orchestrator.rs` is now 762 lines and was removed from the debt
  baseline. No `poster/` production file remains above 800 lines; the
  workspace baseline dropped from 24 to 23 entries.

Validation completed:

- `cargo check -p pesto-poster --all-targets`: passed.
- `cargo clippy -p pesto-poster --all-targets -- -D warnings`: passed.
- `cargo test -p pesto-poster`: all 34 test binaries passed (589 library
  tests, 52 binary tests, every integration suite).
- `cargo fmt --all -- --check`: passed.
- `bash scripts/check-source-size.sh`: passed with 23 baselined files.

Next action: extract the pipeline startup, worker join and check/recovery
blocks from `run` into named stages, then run the posting benchmarks and memory
measurements against the Phase 0 baseline.

### 2026-09-19 — Phase 3 continued: orchestrator and RunOptions

- Moved `post_files_inner_with_release_prefix` — the 1,127-line run entry
  point — into `poster/orchestrator.rs`. Its body is byte-for-byte unchanged;
  it is now the internal `run(options)` function.
- Added `poster/options.rs` with the internal borrowed `RunOptions`. The
  public function keeps its historical nine-argument signature and only
  assembles the struct, so every `pesto::poster::*` path is unchanged and
  external callers are unaffected.
- Pruned the imports the move left unused in `mod.rs` and `orchestrator.rs`.
- `poster/mod.rs` is now 611 lines and is no longer on the debt baseline.
  `poster/orchestrator.rs` (1,177 lines) is the remaining Phase 3 hotspot and
  replaced `mod.rs` on the baseline until its stages are named.

Validation completed:

- `cargo check -p pesto-poster --all-targets`: passed.
- `cargo clippy -p pesto-poster --all-targets -- -D warnings`: passed.
- `cargo test -p pesto-poster`: all targets passed (589 library tests, 52
  binary tests, and every integration test including resume, check/repost,
  pause/cancel, connection reuse and season PAR2).
- `cargo fmt --all -- --check`: passed.
- `bash scripts/check-source-size.sh`: passed after swapping the baseline
  entry from `mod.rs` to `orchestrator.rs`.

Next action: split the `run` body in `poster/orchestrator.rs` into named
stages (prepare run, prepare inputs, start pipeline, await workers, recover or
repost, persist resume state, build outcome) without changing the order, then
run the posting benchmarks.

### 2026-09-19 — Phase 3 continued: result policy extraction and main sync

- Rebased `refactor/phase3-par2-engine` onto `origin/main` after PR #191
  merged `refactor/codebase-simplification`. The pre-rebase tree was identical
  to the merge result, so the rebase replayed cleanly.
- Added `poster/result.rs` (381 lines) for `commit_result`, `jittered`,
  `is_cheap_to_recover`, `target_label`, `record_failure` and the public
  `repost_failed_tasks`. Resume recording, failure description formatting,
  retry backoff and the end-of-run repost loop are unchanged.
- Callers now reach the moved policy through `poster::result` (`worker.rs`,
  `check.rs`) or the explicit test imports in `tests/{policy,paths,internals}.rs`.
- Reduced `poster/mod.rs` from 2,088 to 1,733 lines and lowered its source-size
  debt baseline accordingly.

Validation completed:

- `cargo check -p pesto-poster --all-targets`: passed.
- `cargo clippy -p pesto-poster --all-targets -- -D warnings`: passed.
- `cargo test -p pesto-poster --lib poster::`: 82 passed.
- `cargo test -p pesto-poster --test integration`: 8 passed.
- `cargo test -p pesto-poster --test check_post_retries`: 4 passed.
- `cargo test -p pesto-poster --test check_recover_pass`: 3 passed.
- `cargo test -p pesto-poster --test resume_confirm`: 7 passed.
- `cargo test -p pesto-poster --test pause_resume`: 2 passed.

Next action: reduce the run entry point to named stages in
`poster/orchestrator.rs` and introduce an internal `RunOptions` equivalent,
keeping the existing public functions as compatibility facades.

### 2026-09-19 — Phase 3 continued: worker extraction

- Added `poster/worker.rs` (671 lines) for `RateLimiter`, `encode_worker`,
  `prepare_ready` and `worker`. The yEnc/resume/spool path, per-connection
  message pump, idle keepalive and STAT/repost arms are unchanged.
- Moved the two `RateLimiter` regression tests beside the implementation, per
  the phase rule that tests travel with the behavior they protect.
- `encode_worker` and `worker` are the only exports back to the poster scope;
  `prepare_ready` and `RateLimiter` stay private to the module.
- Reduced `poster/mod.rs` from 2,716 to 2,088 lines and lowered its source-size
  debt baseline accordingly.

Validation completed:

- `cargo check -p pesto-poster --all-targets`: passed.
- `cargo clippy -p pesto-poster --all-targets -- -D warnings`: passed.
- `cargo test -p pesto-poster --lib poster::`: 82 passed.
- `cargo test -p pesto-poster --test integration`: 8 passed.
- `cargo test -p pesto-poster --test pause_resume`: 2 passed.
- `cargo test -p pesto-poster --test check_recover_pass`: 3 passed.
- `cargo test -p pesto-poster --test check_repost_preserves_obfuscation`:
  3 passed.
- `cargo test -p pesto-poster --test each_reuses_connections_across_episodes`:
  3 passed.
- `cargo test -p pesto-poster --test streaming_check_overlaps_upload`: 1 passed.
- `cargo test -p pesto-poster --test paranoid_per_article_subject`: 2 passed.

Next action: extract commit, failure and final-ordering policy into
`poster/result.rs`, then reduce the run entry point to named stages in
`poster/orchestrator.rs`.

### 2026-09-19 — Phase 3 continued: producer extraction

- Added `poster/producer.rs` (761 lines) for `producer`, `feed_par2_slice` and
  `par2_only_ingest`. The producer still reads sequentially, feeds PAR2 slices
  through the zero-copy fast path, and dispatches the same `PostTask`s through
  the same `TaskDispatcher`; no allocation, buffering or pass logic changed.
- `producer` remains the only entry point re-exported to the poster scope;
  `feed_par2_slice` and `par2_only_ingest` stay private to the module.
- `file_md5_16k` and `par2_output_dir` remain in the facade because the
  orchestrator and season paths also use them.
- Reduced `poster/mod.rs` from 3,451 to 2,716 lines and lowered its source-size
  debt baseline accordingly.

Validation completed:

- `cargo check -p pesto-poster --all-targets`: passed.
- `cargo clippy -p pesto-poster --all-targets -- -D warnings`: passed.
- `cargo test -p pesto-poster --lib poster::`: 82 passed.
- `cargo test -p pesto-poster --test integration`: 8 passed.
- `cargo test -p pesto-poster --test par2_before_upload`: 6 passed.
- `cargo test -p pesto-poster --test par2_directory`: 2 passed.
- `cargo test -p pesto-poster --test file_counter`: 3 passed.
- `cargo test -p pesto-poster --test full_shared_obfuscation`: 7 passed.

Next action: extract the worker and ready-article path into
`poster/worker.rs`, keeping the message pump, retry decisions and buffer
recycling byte-for-byte equivalent.

### 2026-09-19 — Phase 3 continued: task and shared state

- Added `poster/task.rs` for `TaskDispatcher`, `PostTask` and `ReadyArticle`.
  Round-robin fan-out, backpressure and the `SendError` contract are unchanged.
- Added `poster/shared.rs` for `Shared`, its buffer pools and `emit`. Merged the
  two former `impl Shared` blocks into one; field visibility is `pub(super)` so
  the poster orchestrator and its tests keep constructing and reading the same
  fields as before.
- Moved the shared-state field doc comments with the struct. The `internals`
  regression tests (buffer reuse, oversized-drop, failure recording) stay in
  `poster/tests/internals.rs` because they share the `minimal_shared` fixture;
  they still protect the same behavior.
- No allocation, buffering, dispatcher or progress-emission behavior changed.
- Reduced `poster/mod.rs` from 3,658 to 3,451 lines and lowered its source-size
  debt baseline accordingly.

Validation completed:

- `cargo check -p pesto-poster --all-targets`: passed.
- `cargo clippy -p pesto-poster --all-targets -- -D warnings`: passed.
- `cargo test -p pesto-poster --lib poster::`: 82 passed.
- `cargo test -p pesto-poster --test each_reuses_connections_across_episodes`:
  3 passed.
- `cargo test -p pesto-poster --test integration`: 8 passed.
- `cargo test -p pesto-poster --test pause_resume`: 2 passed.
- `cargo test -p pesto-poster --test streaming_check_overlaps_upload`: 1 passed.

Next action: extract producer behavior into `poster/producer.rs`, then the
worker and ready-article path into `poster/worker.rs`, keeping the message
pump and retry decisions byte-for-byte equivalent.

### 2026-09-19 — Phase 3 continued: season PAR2 submodule

- Added `poster/par2/season.rs` for the season-wide recovery set: episode
  ordering by File ID, per-episode File Description/IFSC packet assembly, the
  append-as-we-go volume writer, the per-pass ingestion loop and
  `generate_season_par2`.
- Keep `generate_and_write_season_par2` and its progress variant public through
  the `par2` facade so `pesto::poster::*` and the CLI season path are
  unchanged.
- Left `file_md5_16k` in the poster facade because the per-file path uses it
  too; the season module imports it from its ancestor rather than duplicating
  the hash logic.
- The recovery set's byte layout, pass split, memory plan and progress events
  are unchanged. `poster/par2/season.rs` is 518 lines.
- Reduced `poster/mod.rs` from 4,158 to 3,658 lines and lowered its
  source-size debt baseline accordingly.

Validation completed:

- `cargo check -p pesto-poster --all-targets`: passed.
- `cargo clippy -p pesto-poster --all-targets -- -D warnings`: passed.
- `cargo test -p pesto-poster --lib poster::`: 82 passed.
- `cargo test -p pesto-poster --test season_par2_file_desc`: 3 passed.
- `cargo test -p pesto-poster --test season_par2_matches_compressed_archive`:
  1 passed.
- `cargo test -p pesto-poster --test par2_directory`: 2 passed.
- `bash scripts/check-source-size.sh`: passed.
- `cargo fmt --all -- --check`: passed.

Next action: extract the task, shared-state and ready-article types into
`poster/task.rs` and `poster/shared.rs` without changing dispatcher or
buffer-pool behavior.

### 2026-09-19 — Phase 3 continued: PAR2 geometry and memory planning

- Added `poster/par2/mod.rs` as the PAR2 planning facade, re-exporting the
  geometry and memory helpers at the poster scope so existing call sites stay
  unchanged.
- Added `poster/par2/geometry.rs` for `par2_geometry` and
  `par2_geometry_from_sizes`, together with the eight slice-geometry
  regression tests. No formula changed.
- Added `poster/par2/memory.rs` for the address-space ceiling wrapper, the
  connection/thread overhead reserve, the ceiling/retention constants and the
  shared `par2_memory_plan`. The seven memory-model tests moved beside the
  constants they pin, including the local `budget_for` reproduction.
- Moved the message-ID randomness test into `tests/internals.rs` and retired
  the now-empty `tests/memory.rs`; `tests/par2.rs` now covers only
  `par2_output_dir`.
- Reduced `poster/mod.rs` from 4,408 to 4,158 lines and lowered its
  source-size debt baseline accordingly. No behavior, allocation or
  concurrency policy changed.

Validation completed:

- `cargo check -p pesto-poster --all-targets`: passed.
- `cargo clippy -p pesto-poster --all-targets -- -D warnings`: passed.
- `cargo test -p pesto-poster --lib poster::`: 82 passed.
- `bash scripts/check-source-size.sh`: passed with 24 baselined files.
- `cargo fmt --all -- --check`: passed.
- `cargo clippy --all-targets -- -D warnings`: passed.
- `cargo test --all`: passed, with only the repository's explicitly ignored
  tests skipped.

Next action: extract season PAR2 (packet assembly, per-episode ingestion and
volume writing) into `poster/par2/season.rs` without changing the recovery
set's byte layout.
