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
- `pesto::walk::{InputFile, expand_inputs, expand_inputs_with_options}` for input discovery;
- `pesto::progress` for progress events and receivers; and
- `pesto::poster::PostOutcome` and its related outcome types.

`expand_inputs` applies built-in OS/FUSE metadata exclusions to discovered
entries. `expand_inputs_with_options(paths, &config.exclude, config.no_exclude)`
adds custom globs or disables all exclusions. Explicit files bypass exclusions.
For split uploads, use `expand_inputs_from_root(paths, root, &config.exclude,
config.no_exclude)` to retain the original directory root for path globs.
`Exclusions::with_root(root)` applies the same root during batch/watch entry
selection. Discovered entries remain subject to exclusions; explicitly named
file arguments use the ordinary expansion API and bypass them.

Extension filtering is a separate step:
`pesto::walk::apply_ext_filter(&mut inputs, &config.ext, entry_label)`.
It applies to explicit files as well as discovered files and rejects an empty
filtered list. `pesto::upload::run_upload` applies both configured filters;
callers constructing inputs for `post` must apply their chosen filters before
posting. See the [input filter documentation](../crates/pesto/README.md#directory-exclusions)
for matching rules and examples.

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

Consumers should import items from these facade paths. Legacy implementation
paths such as `config::types`, `config::parse`, `yenc::decode`, and the
architecture-specific yEnc backends remain publicly reachable for source
compatibility, but new code should use the items re-exported directly from
`config` and `yenc`.

## Operational compatibility surface

The modules `article`, `cancel`, `memory`, `notify`, `resume`, `spool`, and
`update` remain publicly reachable because the package's own executable and
integration tests are separate crate roots. They are implementation support,
not general embedding APIs. Changes should preserve their existing paths while
the executable or tests use them, but new sibling-crate code must not depend on
them without first promoting the required behavior into the supported surface
above.

`memory::alloc` is a required public child module: a binary must be able to
name `CountingAlloc` in its `#[global_allocator]` declaration. The other
memory child modules remain public compatibility paths for existing consumers;
new code should prefer the types re-exported through the `memory` facade where
available.

## Compatibility rules

1. Add embedding behavior to an existing domain facade when possible.
2. Re-export a moved supported item from its existing facade path.
3. Do not expose a child module only to make an internal cross-module import
   compile; use `pub(crate)` instead.
4. Treat changes to the supported paths above as API changes and update all
   three embedding applications in the same change.
5. Public operational helpers do not become supported embedding API merely
   because Rust visibility makes them reachable.
