# Architecture map

Use this document to choose the right crate and boundary before changing code.
It describes the current design; active work belongs in the relevant roadmap.

## The two product flows

```text
Upload
──────
upapasta ─┐
          ├──> pesto ──> yEnc + NNTP POST ──> Usenet
pesto CLI ┘       │
                  └──> parmesan ──> PAR2 files

Download
────────
sugo ──> penne ──> pesto::nntp + pesto::nzb ──> Usenet
             │
             ├──> assembly / de-obfuscation / extraction
             └──> pesto::par2 (parmesan) ──> verify / repair
```

`pesto` owns the upload path and never downloads content. `penne` owns the
download path and reuses `pesto`'s protocol, NZB and PAR2 primitives instead of
reimplementing them. `upapasta` and `sugo` are clients of library APIs, never
of sibling CLIs.

## Ownership and entry points

| Crate | Responsibility | Start here |
|---|---|---|
| `parmesan` | PAR2 format, Reed-Solomon, encoding, verification and repair | `src/lib.rs`, then `ops.rs`, `verify.rs` or `repair.rs` |
| `pesto` | Posting, configuration, yEnc, NNTP, NZB generation and upload progress | `src/lib.rs`, then `poster/`, `nntp/`, `yenc/` or `upload.rs` |
| `upapasta` | Upload TUI, catalog, watch mode and passive indexer integration | `src/app/` for state; `src/events.rs` for actions; `src/ui/` for rendering |
| `penne` | NZB retrieval, assembly, checks, repair and extraction | `src/download.rs` for the pipeline; neighboring stage modules for behavior |
| `sugo` | HTTP/SSE/API presentation and one-job-at-a-time orchestration | `src/job/` for behavior; `src/api/` or `src/web/` for transport/presentation |

## Dependency rules

- `parmesan` is a computation and file-format library: no NNTP, UI or web
  concerns belong there.
- `pesto` is the shared Usenet primitive layer and posting engine. Keep
  download policy, catalog policy and UI state outside it.
- `upapasta` may orchestrate uploads and maintain user-facing state, but must
  call `pesto` as a library rather than reproducing posting behavior.
- `penne` may add download-specific policy around retrieval and repair, but
  should reuse `pesto::nntp`, `pesto::nzb`, `pesto::yenc` and `pesto::par2`
  whenever their semantics fit.
- `sugo` owns HTTP routes, templates, SSE and persistent job state. Its job
  pipeline calls `penne` directly; it must not grow a second downloader.

External processes are acceptable only at explicit integration boundaries, such
as archive tools, media inspection or user-configured hooks. They are not an
inter-crate integration mechanism.

## Route a change

| If the change concerns… | Begin with… | Keep out of… |
|---|---|---|
| POST throughput, retries, article scheduling or posting PAR2 | `pesto/src/poster/` | UIs and `penne` |
| NNTP/TLS/authentication or POST/STAT/BODY semantics | `pesto/src/nntp/` | application-specific policy |
| yEnc wire encoding or decoding | `pesto/src/yenc/` | UI and NNTP connection management |
| NZB XML, subjects or Message-IDs | `pesto/src/nzb/` | downloader assembly |
| PAR2 geometry, packets, verification or repair | `parmesan/src/` | `pesto` CLI and UI code |
| Download scheduling, failover or file assembly | `penne/src/download.rs` or `assemble.rs` | `sugo` handlers |
| Download post-processing | the matching `penne` stage (`repair`, `deobfuscate`, `extract`, `cleanup`) | route handlers |
| UpaPasta key behavior | `upapasta/src/events.rs` and `app/` | ratatui widgets |
| UpaPasta layout | `upapasta/src/ui/` after the required state exists | persistence and network tasks |
| SABnzbd API or browser behavior | `sugo/src/api/` or `web/` | `penne` pipeline internals |
| Sugo job lifecycle/progress | `sugo/src/job/` | templates and API serialization |

For a cross-crate change, begin at the consumer-facing public API in the
provider's `lib.rs`. Change the provider contract first, then adapt callers and
their tests. Do not make callers reach into private implementation details.

## UI and background work

The TUI and web layers render state; they do not perform blocking work.

- In `upapasta`, an input event changes `App` state or starts a background task;
  results return through `AppEvent` and update state before the next render.
- In `sugo`, a route validates input and delegates lifecycle work to `job/`;
  SSE and templates consume the resulting job state.
- Long-running posting, download, filesystem and HTTP operations must expose
  progress through the existing channels/events rather than blocking a render
  or request handler.

## Test boundaries

- Unit tests cover pure parsing, state and protocol behavior near their module.
- Integration tests use local mock NNTP servers and fixtures.
- Default tests must not contact providers/indexers, run hooks or invoke other
  user-side effects. Optional external-tool checks remain explicitly gated.

## Documentation hierarchy

- [Workspace roadmap](../ROADMAP.md): unfinished cross-workspace work.
- Crate roadmaps: unfinished crate-specific work.
- Crate changelogs: released behavior.
- [Roadmap history](roadmap-history/): superseded plans and decisions.
- [AGENTS.md](../AGENTS.md): repository workflow, conventions and validation.
