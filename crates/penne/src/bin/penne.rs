//! `penne` CLI: reads a `.nzb`, downloads it, assembles the result,
//! verifies/repairs it with PAR2 if recovery data was part of the release,
//! and extracts any archives (`.rar`/`.7z`/`.zip`) it finds. `--mode`
//! ([`ProcessingMode`]) picks how far down that pipeline a run goes,
//! mirroring `sabnzbd`'s per-category Download/+Repair/+Unpack/+Delete
//! processing levels.
//!
//! `info` and `download` are both functional end-to-end: fetch (Phase 2,
//! with per-segment retry/backoff, resume via [`penne::cache`], and
//! N-parallel-connections-per-server concurrency — Phases 8/9), yEnc decode
//! (Phase 3), file assembly (Phase 4), PAR2 verify/repair (Phase 6),
//! archive extraction (Phase 7), and post-extraction cleanup
//! ([`penne::cleanup`]).

use anyhow::Result;
use clap::Parser;

#[path = "penne/check.rs"]
mod check;
#[path = "penne/cli.rs"]
mod cli;
#[path = "penne/command.rs"]
mod command;
#[path = "penne/download.rs"]
mod download;
#[path = "penne/info.rs"]
mod info;

use cli::Cli;

#[tokio::main]
async fn main() -> Result<()> {
    command::run(Cli::parse()).await
}
