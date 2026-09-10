# Docker — `pesto --watch` daemon

Container image for the pesto CLI running as a long-lived `--watch` daemon.
Scope is the **pesto CLI only** (not upapasta, penne, or sugo). See issue
[#185](https://github.com/franzopl/pesto/issues/185).

## Pull vs build

```bash
# Pre-built (published on each pesto-v* tag)
docker pull ghcr.io/franzopl/pesto:latest
# or a specific version, e.g. ghcr.io/franzopl/pesto:0.10.3

docker run --rm ghcr.io/franzopl/pesto:latest --version
```

The first GHCR package for this repo may be private until a maintainer sets
the package visibility to **public**. Until then:

```bash
echo "$GITHUB_TOKEN" | docker login ghcr.io -u USER --password-stdin
```

The runtime is **Debian 12 (bookworm, glibc 2.36)**. GHCR and
`pesto-linux-x86_64` are the same glibc binary, built on Ubuntu 22.04
(glibc 2.35) so they load here. A host binary from Ubuntu 24.04
(glibc 2.39) will not (`GLIBC_2.39 not found`).

Build from source (linux/amd64, default Dockerfile target):

```bash
docker build -t pesto .
docker run --rm pesto --version
```

`--compress=rar` is **not** in the image (`rar` is not redistributable).
`--compress` / `--compress=7z` / `--compress=zip` use `p7zip-full`.
`mediainfo` is also absent; `--nfo` falls back without it.

## Compose

From the repository root:

```bash
mkdir -p docker/config/pesto docker/incoming docker/nzb docker/archive
cp docker/config.toml.example docker/config/pesto/config.toml
# edit credentials in docker/config/pesto/config.toml — never commit it

docker compose -f docker/compose.yaml up -d
```

| Volume | Container path | Use |
|---|---|---|
| `docker/config` | `/config` | `config.toml` at `/config/pesto/config.toml`, history, hooks |
| `docker/incoming` | `/data/incoming` | `--watch` |
| `docker/nzb` | `/data/nzb` | `--nzb-dir` (directory, **not** `--out`) |
| `docker/archive` | `/data/archive` | `--cleanup-to` after a successful upload |
| tmpfs | `/tmp` | PAR2 / compress scratch (`TMPDIR`) |

The service runs as uid/gid **1000**. If the host user is not 1000, set
`user: "${UID}:${GID}"` in `compose.yaml` (and chmod the bind mounts).

Optional `mem_limit` is commented in the compose file; pesto's PAR2 planner
already reads cgroup `memory.max` when a limit is present.

## Signals

`docker compose stop` / `restart` send SIGTERM.

- **First SIGTERM:** `--watch` stops polling and waits for in-progress
  uploads to unwind (the CLI help text's "finish in-progress upload").
- **10 seconds later:** pesto aborts in-flight NNTP I/O
  (`GRACEFUL_SHUTDOWN_DEADLINE` in `crates/pesto/src/cancel.rs`) and
  persists resume state. It does **not** keep posting a multi-gigabyte
  release to completion after that deadline.
- **`stop_grace_period: 30m`:** stops Docker's default 10s SIGKILL from
  racing that abort/persist path. It is not a promise that the current
  file will finish uploading.

A second SIGTERM/SIGINT aborts immediately, same as on the host.

## Existing files on startup

`--watch` ignores entries already in the watched directory when the
process starts (`run_watch` pre-populates `done` from `top_level_entries`).
A container restart therefore skips files that arrived while it was down.
`--cleanup-to` keeps *completed* work out of `incoming` but does not close
that gap.

Drain leftovers with the compose `drain` profile (one-shot `--each`):

```bash
docker compose -f docker/compose.yaml --profile drain run --rm pesto-drain
```

Or:

```bash
docker run --rm \
  -v "$PWD/docker/config:/config" \
  -v "$PWD/docker/incoming:/data/incoming" \
  -v "$PWD/docker/nzb:/data/nzb" \
  -v "$PWD/docker/archive:/data/archive" \
  ghcr.io/franzopl/pesto:latest \
  --each /data/incoming --nzb-dir /data/nzb --cleanup-to /data/archive
```

There is no `--watch-include-existing` flag yet; that is a follow-up.

## Hooks

The image is Linux. Shell hooks work if the script lives on the config
volume (`/config/pesto/hooks/`) and is executable. PowerShell hooks do not.
The runtime has `/bin/sh`.

## Credentials

Username/password stay in the mounted TOML. pesto does not read
`PESTO_PASSWORD` or other credential environment variables.
