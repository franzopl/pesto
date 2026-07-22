# Releasing `pesto`

How to cut a new release of `pesto-poster`: crates.io, docs.rs, and a
GitHub release with prebuilt binaries.

`pesto` is versioned, published, and released **independently of the rest
of the workspace** — own cadence, own tags, own crates.io package. Don't
confuse this with `parmesan`'s or `penne`'s release processes
(`.github/workflows/release-parmesan.yml` / `release-penne.yml`, tags
`parmesan-v*` / `penne-v*`): those build and release their own binaries and
have nothing to do with `pesto`.

## Prerequisites

- Push access to `main` (or an approved PR).
- A crates.io API token for the `pesto-poster` crate, logged in locally
  (`cargo login`) or already present in `~/.cargo/credentials.toml`.
- The full pre-commit checklist passing — see the root `CLAUDE.md` — before
  you start: `cargo fmt --check`, `cargo clippy --all-targets -- -D
  warnings`, `cargo test`, all from the workspace root.

## Steps

### 1. Decide the version bump

`pesto-poster` is pre-1.0, so Cargo's semver rules for `0.x` apply: the
**second** number is the effective "major" — bump it for anything that
could break a consumer (new required behavior, changed output format,
removed/renamed public items) or that ships a substantial new feature
surface. Bump the **third** number for backwards-compatible fixes and
additions.

### 2. Update `crates/pesto/Cargo.toml`

Bump `version`. If the change is significant enough, consider whether
`description` still accurately describes the crate (it does not
auto-update).

### 3. Update every workspace crate that pins a version on `pesto-poster`

Not just the `path` — Cargo enforces the version *requirement* string too,
even for path dependencies within a workspace. Check:

```bash
grep -rn 'pesto-poster' --include=Cargo.toml crates/
```

As of this writing, `crates/penne/Cargo.toml`, `crates/sugo/Cargo.toml` and
`crates/upapasta/Cargo.toml` all depend on it path-only (no version
requirement), so none of them need updating when `pesto`'s version bumps.

### 4. Update `crates/pesto/CHANGELOG.md`

Rename the `## [Unreleased]` header to `## [<version>] — <YYYY-MM-DD>`, and
add a fresh empty `## [Unreleased]` above it for whatever comes next.

### 5. Run the full checklist and commit

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo doc --no-deps -p pesto-poster   # should produce zero warnings
```

Commit the version bump (`Cargo.toml`, `Cargo.lock`, `CHANGELOG.md`) and
push to `main` (directly, or via PR — whichever the change warrants).

### 6. Publish to crates.io

Dry-run first — it catches packaging problems without doing anything
irreversible:

```bash
cd crates/pesto
cargo publish --dry-run
cargo publish
```

`cargo publish` waits for the new version to become available on the
registry before it returns, so a successful exit means it's live.

**This step cannot be undone.** A published version can be `cargo yank`ed
(hidden from new dependents) but never deleted or overwritten.

### 7. Confirm docs.rs picked it up

docs.rs builds automatically after a crates.io publish; it usually takes a
few minutes.

```bash
curl -s "https://docs.rs/crate/pesto-poster/<version>/status.json"
# {"doc_status":true,"version":"<version>"} once it's built
```

If `doc_status` is `false` after a reasonable wait, check the build log
linked from `https://docs.rs/crate/pesto-poster/<version>/builds`.

### 8. Tag and push to trigger the GitHub release

```bash
git tag -a pesto-v<version> -m "pesto-poster v<version>: <one-line summary>"
git push origin pesto-v<version>
```

Pushing a `pesto-v*` tag triggers `.github/workflows/release-pesto.yml`,
which builds the `pesto` binary for `x86_64-unknown-linux-gnu`,
`x86_64-unknown-linux-musl`, and `x86_64-pc-windows-msvc`, then creates a
GitHub Release at that tag with all three attached. It does **not** use
GitHub's auto-generated release notes — those pick the "previous tag" by
creation time across the *whole repo*, which would pull in unrelated
`parmesan-v*`/`penne-v*` tags interleaved with `pesto-v*` ones. The release
body just links to `CHANGELOG.md`, crates.io, and docs.rs instead.

Watch it with:

```bash
gh run list --workflow="Release pesto" --limit 1
gh run watch <run-id>
```

### 9. Verify

```bash
gh release view pesto-v<version> --json url,assets -q '{url, assets: [.assets[].name]}'
```

Should list all three binaries and a working URL.

## Checklist summary

- [ ] Decide version bump (semver 0.x rules)
- [ ] Bump `crates/pesto/Cargo.toml` version (and `description` if stale)
- [ ] Bump the version requirement in any workspace `Cargo.toml` that pins one (none as of this writing — all path-only)
- [ ] `CHANGELOG.md`: `[Unreleased]` → `[<version>] — <date>`, fresh empty `[Unreleased]` above
- [ ] `cargo fmt --check` / `cargo clippy --workspace --all-targets -- -D warnings` / `cargo test --workspace` / `cargo doc --no-deps -p pesto-poster` all clean
- [ ] Commit + push to `main`
- [ ] `cargo publish --dry-run` then `cargo publish`
- [ ] Confirm docs.rs built (`status.json`)
- [ ] `git tag pesto-v<version>` + push
- [ ] Confirm the GitHub Actions run succeeded and the release has all three binaries
