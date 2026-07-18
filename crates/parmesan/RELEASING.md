# Releasing `parmesan`

How to cut a new release of `parmesan-par2`: crates.io, docs.rs, and a
GitHub release with prebuilt binaries. Written from the v0.2.0 release,
which exercised every step here.

`parmesan` is versioned, published, and released **independently of
`pesto`** — different cadence, different tags, different crates.io package.
Don't confuse this with `pesto`'s own release process
(`.github/workflows/release.yml`, tags `v*`): that one builds and releases
the `pesto` binary and has nothing to do with `parmesan`.

## Prerequisites

- Push access to `main` (or an approved PR).
- A crates.io API token for the `parmesan-par2` crate, logged in locally
  (`cargo login`) or already present in `~/.cargo/credentials.toml`.
- The full pre-commit checklist passing — see the root `CLAUDE.md` — before
  you start: `cargo fmt --check`, `cargo clippy --all-targets -- -D
  warnings`, `cargo test`, all from the workspace root.

## Steps

### 1. Decide the version bump

`parmesan-par2` is pre-1.0, so Cargo's semver rules for `0.x` apply: the
**second** number is the effective "major" — bump it for anything that
could break a consumer (new required behavior, changed output format,
removed/renamed public items) or that ships a substantial new feature
surface. Bump the **third** number for backwards-compatible fixes only.

(0.1.0 → 0.2.0 was a minor-as-major bump: new public API surface — `verify`,
`repair`, `recovery_set`, `decoder`, `matrix`, `gf16_mac`, `packet_reader` —
plus a breaking change to recovery data for multi-file sets, see
`CHANGELOG.md`.)

### 2. Update `crates/parmesan/Cargo.toml`

Bump `version`. If the change is significant enough, consider whether
`description` still accurately describes the crate (it does not
auto-update).

### 3. Update every workspace crate that pins a version on `parmesan-par2`

Not just the `path` — Cargo enforces the version *requirement* string too,
even for path dependencies within a workspace. Check:

```bash
grep -rn 'parmesan-par2' --include=Cargo.toml crates/
```

As of v0.2.0, `crates/pesto/Cargo.toml` has:

```toml
parmesan = { package = "parmesan-par2", version = "0.2.0", path = "../parmesan", default-features = false }
```

If you bump `parmesan`'s version and forget this, `cargo check --workspace`
fails with a version-resolution error — the path is right there on disk,
but Cargo still won't use it if the version requirement doesn't match.
`crates/upapasta/Cargo.toml` has no version requirement (path-only), so it
never needs this.

### 4. Update `crates/parmesan/CHANGELOG.md`

Rename the `## [Unreleased]` header to `## [<version>] — <YYYY-MM-DD>`, and
add a fresh empty `## [Unreleased]` above it for whatever comes next.

### 5. Run the full checklist and commit

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo doc --no-deps -p parmesan-par2   # should produce zero warnings
```

Commit the version bump (`Cargo.toml`, `Cargo.lock`, the dependent crate's
`Cargo.toml`, `CHANGELOG.md`) and push to `main` (directly, or via PR —
whichever the change warrants; a pure version/changelog bump with no code
changes is low-risk enough for a direct commit).

### 6. Publish to crates.io

Dry-run first — it catches packaging problems (e.g. a `[[bench]]` or
`[[test]]` target whose file falls under `exclude`) without doing anything
irreversible:

```bash
cd crates/parmesan
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
curl -s "https://docs.rs/crate/parmesan-par2/<version>/status.json"
# {"doc_status":true,"version":"<version>"} once it's built
```

If `doc_status` is `false` after a reasonable wait, check the build log
linked from `https://docs.rs/crate/parmesan-par2/<version>/builds`.

### 8. Tag and push to trigger the GitHub release

```bash
git tag -a parmesan-v<version> -m "parmesan-par2 v<version>: <one-line summary>"
git push origin parmesan-v<version>
```

Pushing a `parmesan-v*` tag triggers
`.github/workflows/release-parmesan.yml`, which builds the `parmesan`
binary for `x86_64-unknown-linux-gnu`, `x86_64-unknown-linux-musl`, and
`x86_64-pc-windows-msvc`, then creates a GitHub Release at that tag with all
three attached. It does **not** use GitHub's auto-generated release notes —
those pick the "previous tag" by creation time across the *whole repo*,
which would pull in unrelated `pesto` (`v*`) tags interleaved with
`parmesan-v*` ones. The release body just links to `CHANGELOG.md`,
crates.io, and docs.rs instead.

Watch it with:

```bash
gh run list --workflow="Release parmesan" --limit 1
gh run watch <run-id>
```

### 9. Verify

```bash
gh release view parmesan-v<version> --json url,assets -q '{url, assets: [.assets[].name]}'
```

Should list all three binaries and a working URL.

## Checklist summary

- [ ] Decide version bump (semver 0.x rules)
- [ ] Bump `crates/parmesan/Cargo.toml` version (and `description` if stale)
- [ ] Bump the version requirement in every workspace `Cargo.toml` that pins one (`crates/pesto/Cargo.toml` today)
- [ ] `CHANGELOG.md`: `[Unreleased]` → `[<version>] — <date>`, fresh empty `[Unreleased]` above
- [ ] `cargo fmt --check` / `cargo clippy --workspace --all-targets -- -D warnings` / `cargo test --workspace` / `cargo doc --no-deps -p parmesan-par2` all clean
- [ ] Commit + push to `main`
- [ ] `cargo publish --dry-run` then `cargo publish`
- [ ] Confirm docs.rs built (`status.json`)
- [ ] `git tag parmesan-v<version>` + push
- [ ] Confirm the GitHub Actions run succeeded and the release has all three binaries
