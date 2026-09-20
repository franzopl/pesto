# Pesto embedding API

This document defines the public Rust surface that workspace applications may
embed. It separates supported integration points from symbols that are public
only because Cargo builds the `pesto` executable and integration tests as
separate crates.

The policy is intentionally path-based. Moving an implementation file does not
change its supported public path: domain modules re-export their public types
and functions from the domain facade.

## Supported surface

The preferred high-level entry points are:

- `pesto::{post, post_cancelable, post_pausable}` for a posting run;
- `pesto::config` for configuration types, parsing and validation;
- `pesto::walk::{InputFile, expand_inputs}` for input discovery;
- `pesto::progress` for progress events and receivers; and
- `pesto::poster::PostOutcome` and its related outcome types.

Workspace applications also rely on the following domain APIs:

| Domain | Supported purpose |
|---|---|
| `compress` | archive creation and existing-volume discovery |
| `history` | upload history records |
| `hooks` | application-managed post-upload hooks |
| `logging` | CLI and TUI logging setup |
| `nfo` | NFO detection and generation |
| `nntp` | NNTP connections and the shared connection broker |
| `nzb` | NZB model, parsing and generation |
| `par2` | PAR2 creation, verification and repair |
| `poster` | advanced posting entry points and season PAR2 generation |
| `ui` | shared render primitives and terminal progress rendering |
| `upload` | the application-oriented upload lifecycle |
| `yenc` | yEnc encode/decode primitives |

Consumers should import items from these facade paths. Implementation modules
such as `config::types`, `config::parse`, `yenc::decode`, and architecture-
specific yEnc backends are private. Their supported items remain available as
direct children of `config` and `yenc`.

## Operational compatibility surface

The modules `article`, `cancel`, `memory`, `notify`, `resume`, `spool`, and
`update` remain publicly reachable because the package's own executable and
integration tests are separate crate roots. They are implementation support,
not general embedding APIs. Changes should preserve their existing paths while
the executable or tests use them, but new sibling-crate code must not depend on
them without first promoting the required behavior into the supported surface
above.

`memory::alloc` is the one deliberately public child module in this group: a
binary must be able to name `CountingAlloc` in its `#[global_allocator]`
declaration. Memory budgeting, ceiling discovery internals and pressure
tracking implementations are crate-private or exposed only through the
`memory` facade.

## Compatibility rules

1. Add embedding behavior to an existing domain facade when possible.
2. Re-export a moved supported item from its existing facade path.
3. Do not expose a child module only to make an internal cross-module import
   compile; use `pub(crate)` instead.
4. Treat changes to the supported paths above as API changes and update all
   three embedding applications in the same change.
5. Public operational helpers do not become supported embedding API merely
   because Rust visibility makes them reachable.
